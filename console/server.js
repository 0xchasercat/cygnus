import {
  createHash,
  createHmac,
  randomBytes,
  timingSafeEqual,
} from "node:crypto";
import { readFileSync } from "node:fs";
import { AdminProtocolError, adminRequest } from "./admin-client.js";

const indexPath = `${import.meta.dir}/dist/index.html`;
const indexFile = Bun.file(indexPath);
const socketPath = process.env.CYGNUS_SOCKET?.trim();
const adminSocketPath = process.env.CYGNUS_ADMIN_SOCKET?.trim();
const requestedPort = process.env.PORT?.trim() || "3000";
const port = Number(requestedPort);

export const ACTOR_SUBJECT = "local:operator";
export const SESSION_COOKIE = "__Host-cygnus_session";
// Used when the console is reached over plain HTTP (IP:3000, local LAN). Browsers
// refuse Secure/__Host- cookies outside a secure context — localhost is special-
// cased by browsers, which is why macOS local worked while remote Linux HTTP did not.
export const SESSION_COOKIE_INSECURE = "cygnus_session";
export const SESSION_TTL_SECONDS = 12 * 60 * 60;
export const MAX_JSON_BODY_BYTES = 32 * 1024;
export const MAX_DEPLOY_CHUNK_JSON_BODY_BYTES = 2 * 1024 * 1024;
export const MAX_DEPLOY_TOTAL_BYTES = 64 * 1024 * 1024;
export const MAX_DEPLOY_CHUNK_BYTES = 1024 * 1024;
export const MAX_DEPLOY_ADMIN_CHUNK_BYTES = 32 * 1024;
export const MAX_IDENTIFIER_LENGTH = 128;
export const MAX_WEBHOOK_BODY_BYTES = 25 * 1024 * 1024;
export const MAX_WEBHOOK_CHUNK_BYTES = 32 * 1024;
export const MANIFEST_STATE_TTL_MS = 60 * 60 * 1000;
export const MAX_MANIFEST_STATES = 1024;
export const GITHUB_WEBHOOK_PATH = "/github/webhook";
export const GITHUB_MANIFEST_CALLBACK_PATH = "/github/app/manifest/callback";
export const GITHUB_SETUP_PATH = "/github/app/setup";
export const GITHUB_INSTALL_CALLBACK_PATH = "/github/app/install/callback";
const MAX_LOGIN_ATTEMPTS = 5;
const LOGIN_BLOCK_MS = 60_000;
const MAX_LOGIN_TRACKED_IPS = 1024;
const loginAttempts = new Map();
const manifestStates = new Map();

if (socketPath && !socketPath.startsWith("/")) {
  throw new Error(`CYGNUS_SOCKET must be an absolute path (received ${socketPath})`);
}
if (adminSocketPath && !adminSocketPath.startsWith("/")) {
  throw new Error(`CYGNUS_ADMIN_SOCKET must be an absolute path (received ${adminSocketPath})`);
}
if (!socketPath && (!Number.isInteger(port) || port < 0 || port > 65535)) {
  throw new Error(`PORT must be an integer between 0 and 65535 (received ${requestedPort})`);
}

let server;
if (import.meta.main) {
  if (!(await indexFile.exists())) {
    throw new Error(`Built console not found at ${indexPath}; run bun run build first`);
  }
  // The console is one self-contained document. Serve it from memory: file
  // streaming fast paths differ per platform (macOS sendfile over unix
  // sockets truncates large bodies) and a buffered body behaves identically
  // everywhere.
  const indexBytes = new Uint8Array(await indexFile.arrayBuffer());
  server = Bun.serve({
    ...(socketPath ? { unix: socketPath } : { port }),
    async fetch(request) {
      const url = new URL(request.url);
      if (
        request.method !== "GET" &&
        request.method !== "HEAD" &&
        url.pathname !== GITHUB_WEBHOOK_PATH &&
        !sameOrigin(request, url)
      ) {
        return apiError(403, "csrf", "request origin is not allowed");
      }
      if ([GITHUB_WEBHOOK_PATH, GITHUB_MANIFEST_CALLBACK_PATH, GITHUB_SETUP_PATH, GITHUB_INSTALL_CALLBACK_PATH].includes(url.pathname)) {
        return handleApi(request, url);
      }
      if (url.pathname === "/healthz") {
        return healthResponse(request);
      }

      if (url.pathname.startsWith("/api/v1/")) {
        return handleApi(request, url);
      }

      if (request.method !== "GET" && request.method !== "HEAD") {
        return methodNotAllowed("GET, HEAD");
      }
      return new Response(request.method === "HEAD" ? null : indexBytes, {
        headers: {
          "cache-control": "no-store",
          "content-type": "text/html; charset=utf-8",
        },
      });
    },
  });
}

export async function healthResponse(request, requestAdmin = adminRequest, socket = adminSocketPath) {
  if (request.method !== "GET" && request.method !== "HEAD") return methodNotAllowed("GET, HEAD");
  const auth = authStatus();
  const value = {
    ok: true,
    service: "cygnus-console",
    tenant: "tenant-0",
    mode: socket ? "live" : "preview",
    dataSource: socket ? "daemon" : "unavailable",
    daemonBridge: socket ? "configured" : "offline",
    auth,
    locked: Boolean(socket && !auth.configured),
    setupRequired: false,
  };
  if (socket) {
    try {
      const { data } = await requestAdmin(socket, { type: "account_status" });
      if (typeof data?.configured === "boolean") value.setupRequired = !data.configured;
    } catch {
      // Health remains available when the daemon bridge cannot answer.
    }
  }
  return jsonResponse(value, request.method === "HEAD");
}

export async function handleApi(request, url, requestAdmin = adminRequest, socket = adminSocketPath) {
  const path = url.pathname;
  if (path === "/api/v1/session") {
    if (request.method === "GET" || request.method === "HEAD") {
      return sessionResponse(request, request.method === "HEAD", requestAdmin, socket);
    }
    if (request.method === "POST") {
      if (!sameOrigin(request, url)) return apiError(403, "csrf", "request origin is not allowed");
      return login(request, requestAdmin, socket);
    }
    if (request.method === "DELETE") {
      if (!sameOrigin(request, url)) return apiError(403, "csrf", "request origin is not allowed");
      return logout(request);
    }
    return methodNotAllowed("GET, HEAD, POST, DELETE");
  }
  if (path === "/api/v1/setup") {
    if (request.method !== "POST") return methodNotAllowed("POST");
    if (!sameOrigin(request, url)) return apiError(403, "csrf", "request origin is not allowed");
    return setup(request, requestAdmin, socket);
  }
  if (path === "/api/v1/logout" || path === "/api/v1/session/logout") {
    if (request.method !== "POST") return methodNotAllowed("POST");
    if (!sameOrigin(request, url)) return apiError(403, "csrf", "request origin is not allowed");
    return logout(request);
  }

  if (path === GITHUB_MANIFEST_CALLBACK_PATH) return manifestCallback(request, url, requestAdmin, socket);
  if (path === GITHUB_SETUP_PATH || path === GITHUB_INSTALL_CALLBACK_PATH) return githubSetupCallback(request, url);
  const deployUploadRoute = [
    "/api/v1/deploy/begin",
    "/api/v1/deploy/chunk",
    "/api/v1/deploy/finish",
  ].includes(path);
  const dashboardDomainRoute = path === "/api/v1/settings/dashboard-domain";
  const dashboardTlsRoute = path === "/api/v1/settings/dashboard-tls";
  const passwordRoute = path === "/api/v1/settings/password";
  const appDomainsRoute = /^\/api\/v1\/apps\/[^/]+\/domains$/u.test(path);
  const appDomainRoute = /^\/api\/v1\/apps\/[^/]+\/domains\/[^/]+$/u.test(path);
  const appDomainTlsRoute = /^\/api\/v1\/apps\/[^/]+\/domains\/[^/]+\/tls$/u.test(path);
  const appDomainPrimaryRoute = /^\/api\/v1\/apps\/[^/]+\/domains\/[^/]+\/primary$/u.test(path);
  const appDomainRetryAcmeRoute = /^\/api\/v1\/apps\/[^/]+\/domains\/[^/]+\/retry-acme$/u.test(path);
  const appEnvRoute = /^\/api\/v1\/apps\/[^/]+\/env$/u.test(path);
  const appEnvKeyRoute = /^\/api\/v1\/apps\/[^/]+\/env\/[^/]+$/u.test(path);
  const githubReposRoute = path === "/api/v1/github/repositories";
  const mutationRoute = deployUploadRoute || dashboardDomainRoute || dashboardTlsRoute || passwordRoute || [
    "/api/v1/map-domain",
    "/api/v1/rollback",
    "/api/v1/github/manifest",
  ].includes(path) || /^\/api\/v1\/github\/jobs\/[^/]+\/retry$/u.test(path);
  const readRoute = /^(?:\/api\/v1\/(?:status|apps|deployments)|\/api\/v1\/github\/(?:status|repositories|installations\/[^/]+\/repositories|discoverable-repositories|jobs))(?:\/[^/]+)?$/u.test(path)
    || /^\/api\/v1\/(?:metrics|requests|events)$/u.test(path)
    || /^\/api\/v1\/(?:apps|deployments)\/[^/]+\/logs$/u.test(path);
  if (mutationRoute && request.method !== "POST") return methodNotAllowed("POST");
  if (appDomainsRoute && !["GET", "HEAD", "POST"].includes(request.method)) return methodNotAllowed("GET, HEAD, POST");
  if (appDomainRoute && request.method !== "DELETE") return methodNotAllowed("DELETE");
  if (appDomainTlsRoute && request.method !== "POST") return methodNotAllowed("POST");
  if (appDomainPrimaryRoute && request.method !== "POST") return methodNotAllowed("POST");
  if (appDomainRetryAcmeRoute && request.method !== "POST") return methodNotAllowed("POST");
  if (appEnvRoute && !["GET", "HEAD", "POST"].includes(request.method)) return methodNotAllowed("GET, HEAD, POST");
  if (appEnvKeyRoute && request.method !== "DELETE") return methodNotAllowed("DELETE");
  if (githubReposRoute && !["GET", "HEAD", "POST"].includes(request.method)) return methodNotAllowed("GET, HEAD, POST");
  if (readRoute && !appDomainsRoute && !appEnvRoute && !githubReposRoute && request.method !== "GET" && request.method !== "HEAD") return methodNotAllowed("GET, HEAD");

  if (request.method !== "GET" && request.method !== "HEAD") {
    if (!sameOrigin(request, url)) return apiError(403, "csrf", "request origin is not allowed");
  }
  if (path === "/api/v1/deploy") return apiError(404, "not_found", "API route not found");
  if (deployUploadRoute) return deployUploadIngress(request, url, requestAdmin, socket);

  const auth = authStatus();
  if (!auth.sessionConfigured) {
    return apiError(503, "misconfigured", "console authentication is misconfigured");
  }
  const sessionCookie = request.headers.get("cookie");
  const session = verifySessionCookie(sessionCookie);
  if (!session) {
    return apiError(401, "unauthorized", "authentication required");
  }
  if (path === "/api/v1/github/manifest") return manifestStart(request, url, sessionCookie);
  if (!socket) {
    return apiError(503, "unavailable", "daemon admin bridge unavailable");
  }

  let command;
  try {
    command = await commandForRequest(request, url);
  } catch (error) {
    const status = error instanceof HttpInputError ? error.status : 422;
    const code = error instanceof HttpInputError ? error.code : "validation";
    return apiError(status, code, error instanceof Error ? error.message : "invalid request");
  }
  if (!command) return apiError(404, "not_found", "API route not found");

  try {
    const { data, requestId } = await requestAdmin(socket, command, session.sub);
    const publicData = path.startsWith("/api/v1/github/") ? sanitizeGithubData(data) : data;
    return jsonResponse({ ok: true, data: publicData, requestId }, request.method === "HEAD");
  } catch (error) {
    const code = error instanceof AdminProtocolError ? error.code : "internal";
    const status = statusForDaemonCode(code);
    const detail = error instanceof AdminProtocolError ? error.message : undefined;
    return apiError(status, publicDaemonCode(code), safeErrorMessage(code, detail));
  }
}

async function login(request, requestAdmin = adminRequest, socket = adminSocketPath) {
  const ip = requestIp(request);
  const now = Date.now();
  const throttle = loginThrottle(ip, now);
  if (throttle.blocked) {
    return apiError(429, "rate_limited", "too many login attempts", {
      "retry-after": String(Math.max(1, Math.ceil((throttle.retryAt - now) / 1000))),
    });
  }

  let body;
  try {
    body = await readJsonBody(request);
  } catch (error) {
    constantTimeTokenMatch("");
    return inputErrorResponse(error);
  }

  const auth = authStatus();
  if (!auth.sessionConfigured) {
    return apiError(503, "misconfigured", "console authentication is misconfigured");
  }

  let subject;
  try {
    const keys = body && typeof body === "object" && !Array.isArray(body) ? Object.keys(body) : [];
    if (keys.length === 1 && keys[0] === "token") {
      assertExactKeys(body, ["token"]);
      if (typeof body.token !== "string" || body.token.length > 1024) {
        throw new HttpInputError(422, "validation", "token is invalid");
      }
      if (!constantTimeTokenMatch(body.token)) return invalidCredentials(ip, now);
      subject = ACTOR_SUBJECT;
    } else {
      assertExactKeys(body, ["email", "password"]);
      const email = safeEmail(body.email);
      const password = safePassword(body.password);
      if (!socket) return apiError(503, "unavailable", "daemon admin bridge unavailable");
      let verified;
      try {
        verified = await requestAdmin(socket, { type: "verify_credentials", email, password });
      } catch (error) {
        const code = error instanceof AdminProtocolError ? error.code : "internal";
        return apiError(statusForDaemonCode(code), publicDaemonCode(code), safeErrorMessage(code, error instanceof AdminProtocolError ? error.message : undefined));
      }
      if (verified?.data?.ok !== true || !safeAccountSubject(verified?.data?.subject)) {
        return invalidCredentials(ip, now);
      }
      subject = verified.data.subject;
    }
  } catch (error) {
    constantTimeTokenMatch("");
    return inputErrorResponse(error);
  }

  loginAttempts.delete(ip);
  const cookie = signSession({ sub: subject });
  return jsonResponse(
    { ok: true, data: { authenticated: true, actor: subject } },
    false,
    200,
    { "set-cookie": sessionSetCookie(cookie, request) },
  );
}

function invalidCredentials(ip, now) {
  const state = recordLoginFailure(ip, now);
  const headers = state.blocked
    ? { "retry-after": String(Math.max(1, Math.ceil((state.retryAt - now) / 1000))) }
    : {};
  return apiError(401, "unauthorized", "invalid credentials", headers);
}

function logout(request) {
  return jsonResponse(
    { ok: true, data: { authenticated: false } },
    false,
    200,
    { "set-cookie": sessionClearCookies(request) },
  );
}

export async function sessionResponse(request, head = false, requestAdmin = adminRequest, socket = adminSocketPath) {
  const auth = authStatus();
  const session = verifySessionCookie(request.headers.get("cookie"));
  const data = {
    authenticated: Boolean(session),
    actor: session?.sub,
    configured: auth.sessionConfigured,
    locked: !auth.sessionConfigured,
    setupRequired: false,
  };
  if (socket) {
    try {
      const status = await requestAdmin(socket, { type: "account_status" });
      if (typeof status?.data?.configured === "boolean") data.setupRequired = !status.data.configured;
    } catch {
      // Session authentication state remains useful while the daemon is offline.
    }
  }
  return jsonResponse({ ok: true, data }, head);
}

export async function setup(request, requestAdmin = adminRequest, socket = adminSocketPath) {
  if (!authStatus().sessionConfigured) return apiError(503, "misconfigured", "console authentication is misconfigured");
  if (!socket) return apiError(503, "unavailable", "daemon admin bridge unavailable");
  let body;
  try {
    body = await readJsonBody(request);
    assertExactKeys(body, ["email", "password", "dashboard_domain", "apex_domain", "ssl"]);
    safeEmail(body.email);
    safePassword(body.password);
    safeDomain(body.dashboard_domain);
    safeDomain(body.apex_domain);
    if (typeof body.ssl !== "boolean") throw new HttpInputError(422, "validation", "ssl must be a boolean");
  } catch (error) {
    return inputErrorResponse(error);
  }

  try {
    const status = await requestAdmin(socket, { type: "account_status" });
    if (status?.data?.configured !== false) return apiError(409, "conflict", "initial account is already configured");
    const created = await requestAdmin(socket, {
      type: "create_initial_account",
      email: body.email,
      password: body.password,
    });
    const subject = created?.data?.subject;
    if (!safeAccountSubject(subject)) throw new Error("daemon returned an invalid account subject");
    await requestAdmin(socket, {
      type: "set_dashboard_domain",
      domain: body.dashboard_domain,
      apex: body.apex_domain,
    }, subject);
    await requestAdmin(socket, {
      type: "set_dashboard_tls",
      mode: body.ssl ? "acme" : "self_signed",
      ...(body.ssl ? { email: body.email } : {}),
    }, subject);
    const cookie = signSession({ sub: subject });
    return jsonResponse(
      { ok: true, data: { apex_domain: body.apex_domain, dashboard_domain: body.dashboard_domain } },
      false,
      200,
      { "set-cookie": sessionSetCookie(cookie, request) },
    );
  } catch (error) {
    const code = error instanceof AdminProtocolError ? error.code : "internal";
    return apiError(statusForDaemonCode(code), publicDaemonCode(code), safeErrorMessage(code, error instanceof AdminProtocolError ? error.message : undefined));
  }
}

export function authStatus() {
  const bootstrap = credential("CYGNUS_CONSOLE_BOOTSTRAP_TOKEN", "CYGNUS_CONSOLE_BOOTSTRAP_TOKEN_FILE");
  const sessionKey = credential("CYGNUS_CONSOLE_SESSION_KEY", "CYGNUS_CONSOLE_SESSION_KEY_FILE");
  return {
    configured: sessionKey.length > 0,
    bootstrapConfigured: bootstrap.length > 0,
    sessionConfigured: sessionKey.length > 0,
  };
}

export function constantTimeTokenMatch(candidate) {
  const configured = credential("CYGNUS_CONSOLE_BOOTSTRAP_TOKEN", "CYGNUS_CONSOLE_BOOTSTRAP_TOKEN_FILE");
  const left = createHash("sha256").update(String(candidate ?? ""), "utf8").digest();
  const right = createHash("sha256").update(configured, "utf8").digest();
  return timingSafeEqual(left, right) && configured.length > 0;
}

export function signSession(input = {}, now = Date.now()) {
  if (typeof input === "number") {
    now = input;
    input = {};
  }
  const issuedAt = Number.isInteger(input.iat) ? input.iat : Math.floor(now / 1000);
  const expiresAt = Number.isInteger(input.exp) ? input.exp : issuedAt + SESSION_TTL_SECONDS;
  const payload = base64UrlEncode(
    JSON.stringify({ sub: input.sub ?? ACTOR_SUBJECT, iat: issuedAt, exp: expiresAt }),
  );
  const signature = sessionSignature(payload);
  return signature ? `v1.${payload}.${signature}` : "";
}

export function verifySessionCookie(cookie, now = Date.now()) {
  if (typeof cookie !== "string") return null;
  // Accept either cookie name so sessions survive HTTP↔HTTPS transitions.
  let value = null;
  if (cookie.includes("=")) {
    value =
      parseCookie(cookie, SESSION_COOKIE) ??
      parseCookie(cookie, SESSION_COOKIE_INSECURE);
  } else {
    value = cookie;
  }
  if (!value) return null;
  const pieces = value.split(".");
  if (pieces.length !== 3 || pieces[0] !== "v1") return null;
  const [, encoded, signature] = pieces;
  const expected = sessionSignature(encoded);
  if (!expected || !constantTimeStringEqual(signature, expected)) return null;
  let payload;
  try {
    payload = JSON.parse(base64UrlDecode(encoded));
  } catch {
    return null;
  }
  if (
    !payload ||
    (payload.sub !== ACTOR_SUBJECT && !safeAccountSubject(payload.sub)) ||
    !Number.isSafeInteger(payload.iat) ||
    !Number.isSafeInteger(payload.exp) ||
    payload.exp <= payload.iat ||
    Math.floor(now / 1000) >= payload.exp
  ) {
    return null;
  }
  return payload;
}

function sessionSignature(payload) {
  const key = credential("CYGNUS_CONSOLE_SESSION_KEY", "CYGNUS_CONSOLE_SESSION_KEY_FILE");
  if (!key) return "";
  return createHmac("sha256", key).update(payload, "utf8").digest("base64url");
}

function credential(valueName, fileName) {
  const direct = process.env[valueName] ?? "";
  if (direct) return direct;
  const path = process.env[fileName]?.trim() ?? "";
  if (!path.startsWith("/")) return "";
  try {
    const raw = readFileSync(path);
    return raw.length === 32 ? raw.toString("hex") : "";
  } catch {
    return "";
  }
}

/** True when the browser will treat this request as a secure context for cookies. */
export function requestIsSecure(request) {
  if (!request) return false;
  const forwarded = String(request.headers?.get?.("x-forwarded-proto") ?? "")
    .split(",")[0]
    .trim()
    .toLowerCase();
  if (forwarded === "https") return true;
  if (forwarded === "http") return false;
  try {
    const url = new URL(request.url);
    if (url.protocol === "https:") return true;
    // Browsers treat loopback HTTP as a secure context for cookies.
    const host = url.hostname.replace(/^\[|\]$/g, "").toLowerCase();
    if (host === "localhost" || host === "127.0.0.1" || host === "::1") return true;
  } catch {
    /* ignore */
  }
  return false;
}

function sessionCookieName(secure) {
  return secure ? SESSION_COOKIE : SESSION_COOKIE_INSECURE;
}

function sessionSetCookie(value, request) {
  const secure = requestIsSecure(request);
  const name = sessionCookieName(secure);
  // Secure + __Host- only on HTTPS/loopback. SameSite=Lax so top-level
  // navigations (GitHub OAuth return) still carry the session.
  const flags = secure
    ? `Path=/; Max-Age=${SESSION_TTL_SECONDS}; HttpOnly; Secure; SameSite=Lax`
    : `Path=/; Max-Age=${SESSION_TTL_SECONDS}; HttpOnly; SameSite=Lax`;
  return `${name}=${value}; ${flags}`;
}

function sessionClearCookies(request) {
  // Always clear both names so a later HTTPS upgrade doesn't leave a stale
  // insecure cookie, and vice versa.
  const secure = requestIsSecure(request);
  const secureFlags = "Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax";
  const plainFlags = "Path=/; Max-Age=0; HttpOnly; SameSite=Lax";
  // Bun/jsonResponse only accepts one set-cookie header string here; emit both
  // names on sequential responses is ideal, but a single combined clear of the
  // active name plus the alternate is enough for browsers that accept multi
  // Set-Cookie via array — fall back to the active name first.
  const primary = secure
    ? `${SESSION_COOKIE}=; ${secureFlags}`
    : `${SESSION_COOKIE_INSECURE}=; ${plainFlags}`;
  const secondary = secure
    ? `${SESSION_COOKIE_INSECURE}=; ${plainFlags}`
    : `${SESSION_COOKIE}=; ${secureFlags}`;
  // Prefer clearing the cookie that matches the current scheme; secondary is
  // attached via array when the response helper supports it.
  return [primary, secondary];
}

function parseCookie(header, name) {
  for (const part of String(header ?? "").split(";")) {
    const index = part.indexOf("=");
    if (index < 0) continue;
    if (part.slice(0, index).trim() === name) return part.slice(index + 1).trim();
  }
  return null;
}

function constantTimeStringEqual(left, right) {
  const a = createHash("sha256").update(String(left), "utf8").digest();
  const b = createHash("sha256").update(String(right), "utf8").digest();
  return timingSafeEqual(a, b);
}

export async function commandForRequest(request, url) {
  const parts = url.pathname.split("/").filter(Boolean);
  if (request.method === "GET" || request.method === "HEAD") {
    return commandForRead(url, parts);
  }
  if (request.method === "DELETE" && parts.length === 6 && parts[2] === "apps" && parts[4] === "domains") {
    assertQueryKeys(url, []);
    await assertEmptyBody(request);
    return removeAppDomainCommand(
      decodeSegment(parts[3], "app"),
      decodeDomainSegment(parts[5], "host"),
    );
  }
  if (request.method === "DELETE" && parts.length === 6 && parts[2] === "apps" && parts[4] === "env") {
    assertQueryKeys(url, []);
    await assertEmptyBody(request);
    return removeEnvVarCommand(decodeSegment(parts[3], "app"), decodeSegment(parts[5], "key"));
  }
  if (request.method !== "POST") return null;
  if (parts.length === 3 && parts[2] === "map-domain") {
    return mapDomainCommand(await readJsonBody(request));
  }
  if (parts.length === 3 && parts[2] === "rollback") {
    return rollbackCommand(await readJsonBody(request));
  }
  if (parts.length === 4 && parts[2] === "settings" && parts[3] === "dashboard-domain") {
    assertQueryKeys(url, []);
    return dashboardDomainCommand(await readJsonBody(request));
  }
  if (parts.length === 4 && parts[2] === "settings" && parts[3] === "dashboard-tls") {
    assertQueryKeys(url, []);
    return dashboardTlsCommand(await readJsonBody(request));
  }
  if (parts.length === 4 && parts[2] === "settings" && parts[3] === "password") {
    assertQueryKeys(url, []);
    return changePasswordCommand(await readJsonBody(request));
  }
  if (parts.length === 5 && parts[2] === "apps" && parts[4] === "domains") {
    assertQueryKeys(url, []);
    return addAppDomainCommand(decodeSegment(parts[3], "app"), await readJsonBody(request));
  }
  if (parts.length === 7 && parts[2] === "apps" && parts[4] === "domains" && parts[6] === "tls") {
    assertQueryKeys(url, []);
    return appDomainTlsCommand(
      decodeSegment(parts[3], "app"),
      decodeDomainSegment(parts[5], "host"),
      await readJsonBody(request),
    );
  }
  if (parts.length === 7 && parts[2] === "apps" && parts[4] === "domains" && parts[6] === "primary") {
    assertQueryKeys(url, []);
    await assertEmptyBody(request);
    return setPrimaryDomainCommand(decodeSegment(parts[3], "app"), decodeDomainSegment(parts[5], "host"));
  }
  if (parts.length === 7 && parts[2] === "apps" && parts[4] === "domains" && parts[6] === "retry-acme") {
    assertQueryKeys(url, []);
    await assertEmptyBody(request);
    return retryDomainAcmeCommand(decodeSegment(parts[3], "app"), decodeDomainSegment(parts[5], "host"));
  }
  if (parts.length === 5 && parts[2] === "apps" && parts[4] === "env") {
    assertQueryKeys(url, []);
    return setEnvVarCommand(decodeSegment(parts[3], "app"), await readJsonBody(request));
  }
  if (parts.length === 4 && parts[2] === "github" && parts[3] === "repositories") {
    return configureRepositoryCommand(await readJsonBody(request));
  }
  if (parts.length === 6 && parts[2] === "github" && parts[3] === "jobs" && parts[5] === "retry") {
    await assertEmptyBody(request);
    return retryDeployJobCommand(decodeSegment(parts[4], "job id"));
  }
  return null;
}

export async function deployUploadIngress(request, url, requestAdmin = adminRequest, socket = adminSocketPath) {
  if (request.method !== "POST") return methodNotAllowed("POST");
  if (!sameOrigin(request, url)) return apiError(403, "csrf", "request origin is not allowed");
  if (!authStatus().sessionConfigured) return apiError(503, "misconfigured", "console authentication is misconfigured");
  const session = verifySessionCookie(request.headers.get("cookie"));
  if (!session) return apiError(401, "unauthorized", "authentication required");
  if (!socket) return apiError(503, "unavailable", "daemon admin bridge unavailable");

  try {
    if (url.pathname === "/api/v1/deploy/begin") {
      const command = deployUploadBeginCommand(await readJsonBody(request));
      const result = await requestAdmin(socket, command, session.sub);
      return jsonResponse({ ok: true, data: result?.data, requestId: result?.requestId });
    }
    if (url.pathname === "/api/v1/deploy/finish") {
      const command = deployUploadFinishCommand(await readJsonBody(request));
      const result = await requestAdmin(socket, command, session.sub);
      return jsonResponse({ ok: true, data: result?.data, requestId: result?.requestId });
    }
    if (url.pathname === "/api/v1/deploy/chunk") {
      const body = await readJsonBody(request, MAX_DEPLOY_CHUNK_JSON_BODY_BYTES);
      const { uploadId, bytes } = deployUploadChunk(body);
      let last;
      for (let offset = 0; offset < bytes.length; offset += MAX_DEPLOY_ADMIN_CHUNK_BYTES) {
        const chunk = bytes.subarray(offset, offset + MAX_DEPLOY_ADMIN_CHUNK_BYTES);
        last = await requestAdmin(socket, {
          type: "deploy_upload_chunk",
          upload_id: uploadId,
          chunk_base64: chunk.toString("base64"),
        }, session.sub);
      }
      return jsonResponse({
        ok: true,
        data: { received_bytes: last?.data?.received_bytes },
        requestId: last?.requestId,
      });
    }
    return apiError(404, "not_found", "API route not found");
  } catch (error) {
    if (error instanceof HttpInputError) return apiError(error.status, error.code, error.message);
    const code = error instanceof AdminProtocolError ? error.code : "internal";
    return apiError(statusForDaemonCode(code), publicDaemonCode(code), safeErrorMessage(code, error instanceof AdminProtocolError ? error.message : undefined));
  }
}

export function deployUploadBeginCommand(body) {
  assertObjectKeys(body, ["app", "total_bytes"], ["domain", "engine_version", "entry", "env", "preview"]);
  const command = {
    type: "deploy_upload_begin",
    app: safeApp(body.app),
    total_bytes: safeInteger(body.total_bytes, "total_bytes", 1, MAX_DEPLOY_TOTAL_BYTES),
  };
  if (body.domain !== undefined) command.domain = safeDomain(body.domain);
  if (body.engine_version !== undefined) command.engine_version = safeVersion(body.engine_version);
  if (body.entry !== undefined) command.entry = safeEntry(body.entry);
  if (body.env !== undefined) command.env = safeEnvMap(body.env);
  if (body.preview !== undefined) command.preview = safePreviewSlug(body.preview);
  return command;
}

export function deployUploadFinishCommand(body) {
  assertExactKeys(body, ["upload_id"]);
  return { type: "deploy_upload_finish", upload_id: safeDeployment(body.upload_id) };
}

export function deployUploadChunk(body) {
  assertExactKeys(body, ["upload_id", "chunk_base64"]);
  const uploadId = safeDeployment(body.upload_id);
  if (typeof body.chunk_base64 !== "string" || body.chunk_base64.length === 0) {
    throw new HttpInputError(422, "validation", "chunk_base64 must be canonical base64");
  }
  const bytes = Buffer.from(body.chunk_base64, "base64");
  if (bytes.length === 0 || bytes.length > MAX_DEPLOY_CHUNK_BYTES || bytes.toString("base64") !== body.chunk_base64) {
    throw new HttpInputError(422, "validation", "chunk_base64 must be canonical base64 of at most 1 MiB");
  }
  return { uploadId, bytes };
}

export function commandFor(url) {
  const parts = url.pathname.split("/").filter(Boolean);
  return commandForRead(url, parts);
}

function commandForRead(url, parts = url.pathname.split("/").filter(Boolean)) {
  if (parts.length === 3 && parts[2] === "status") {
    assertQueryKeys(url, []);
    return { type: "status" };
  }
  if (parts.length === 3 && parts[2] === "metrics") {
    assertQueryKeys(url, []);
    return { type: "get_metrics" };
  }
  if (parts.length === 3 && parts[2] === "requests") {
    assertQueryKeys(url, ["limit"]);
    return { type: "list_requests", limit: observabilityLimit(url) };
  }
  if (parts.length === 3 && parts[2] === "events") {
    assertQueryKeys(url, ["limit"]);
    return { type: "list_events", limit: observabilityLimit(url) };
  }
  if (parts.length === 3 && parts[2] === "apps") {
    assertQueryKeys(url, ["cursor", "limit"]);
    return { type: "list_apps", cursor: optionalQuery(url, "cursor"), limit: listLimit(url) };
  }
  if (parts.length === 4 && parts[2] === "apps") {
    assertQueryKeys(url, []);
    return { type: "get_app", app: safeApp(decodeSegment(parts[3], "app")) };
  }
  if (parts.length === 5 && parts[2] === "apps" && parts[4] === "logs") {
    assertQueryKeys(url, ["stream", "offset", "limit"]);
    return {
      type: "read_app_log",
      app: safeApp(decodeSegment(parts[3], "app")),
      stream: logStream(url),
      offset: logOffset(url),
      limit: logLimit(url),
    };
  }
  if (parts.length === 5 && parts[2] === "apps" && parts[4] === "domains") {
    assertQueryKeys(url, []);
    return { type: "list_app_domains", app: safeApp(decodeSegment(parts[3], "app")) };
  }
  if (parts.length === 5 && parts[2] === "apps" && parts[4] === "env") {
    assertQueryKeys(url, []);
    return listEnvVarsCommand(decodeSegment(parts[3], "app"));
  }
  if (parts.length === 3 && parts[2] === "deployments") {
    assertQueryKeys(url, ["app", "cursor", "limit"]);
    const app = optionalQuery(url, "app");
    return {
      type: "list_deployments",
      app: app === undefined ? undefined : safeApp(app),
      cursor: optionalQuery(url, "cursor"),
      limit: listLimit(url),
    };
  }
  if (parts.length === 4 && parts[2] === "deployments") {
    assertQueryKeys(url, []);
    return { type: "get_deployment", deployment: safeDeployment(decodeSegment(parts[3], "deployment")) };
  }
  if (parts.length === 5 && parts[2] === "deployments" && parts[4] === "logs") {
    assertQueryKeys(url, ["stream", "offset", "limit"]);
    return {
      type: "read_log",
      deployment: safeDeployment(decodeSegment(parts[3], "deployment")),
      stream: logStream(url),
      offset: logOffset(url),
      limit: logLimit(url),
    };
  }
  if (parts.length === 4 && parts[2] === "github" && parts[3] === "status") {
    assertQueryKeys(url, []);
    return githubStatusCommand();
  }
  if (parts.length === 4 && parts[2] === "github" && parts[3] === "repositories") {
    assertQueryKeys(url, ["limit"]);
    return listRepositoriesCommand(listLimit(url));
  }
  if (parts.length === 6 && parts[2] === "github" && parts[3] === "installations" && parts[5] === "repositories") {
    assertQueryKeys(url, []);
    return listInstallationRepositoriesCommand(safePositiveId(decodeSegment(parts[4], "installation id")));
  }
  if (parts.length === 4 && parts[2] === "github" && parts[3] === "discoverable-repositories") {
    assertQueryKeys(url, []);
    return { type: "list_discoverable_repositories" };
  }
  if (parts.length === 4 && parts[2] === "github" && parts[3] === "jobs") {
    assertQueryKeys(url, ["cursor", "limit"]);
    return listDeployJobsCommand(optionalQuery(url, "cursor"), listLimit(url));
  }
  return null;
}

export function dashboardDomainCommand(body) {
  assertExactKeys(body, ["domain", "apex"]);
  return {
    type: "set_dashboard_domain",
    domain: nullableDomain(body.domain, "domain"),
    apex: nullableDomain(body.apex, "apex"),
  };
}

export function dashboardTlsCommand(body) {
  assertObjectKeys(body, ["mode"], ["email"]);
  const command = { type: "set_dashboard_tls", mode: safeTlsMode(body.mode) };
  if (body.email !== undefined && body.email !== null && String(body.email).trim() !== "") {
    command.email = safeEmail(body.email);
  }
  return command;
}

export function changePasswordCommand(body) {
  assertExactKeys(body, ["email", "current_password", "new_password"]);
  return {
    type: "change_password",
    email: safeEmail(body.email),
    current_password: safePassword(body.current_password),
    new_password: safePassword(body.new_password),
  };
}

export function addAppDomainCommand(app, body) {
  assertExactKeys(body, ["host"]);
  return { type: "add_app_domain", app: safeApp(app), host: safeDomain(body.host) };
}

export function removeAppDomainCommand(app, host) {
  return { type: "remove_app_domain", app: safeApp(app), host: safeDomain(host) };
}

export function appDomainTlsCommand(app, host, body) {
  assertExactKeys(body, ["mode"]);
  return {
    type: "set_app_domain_tls",
    app: safeApp(app),
    host: safeDomain(host),
    mode: safeTlsMode(body.mode),
  };
}

export function setPrimaryDomainCommand(app, host) {
  return { type: "set_primary_domain", app: safeApp(app), host: safeDomain(host) };
}

export function retryDomainAcmeCommand(app, host) {
  return { type: "retry_domain_acme", app: safeApp(app), host: safeDomain(host) };
}

export function mapDomainCommand(body) {
  assertExactKeys(body, ["app", "domain"]);
  return { type: "map_domain", app: safeApp(body.app), domain: safeDomain(body.domain) };
}

export function rollbackCommand(body) {
  assertExactKeys(body, ["app", "deployment", "expected_active_artifact"]);
  return {
    type: "rollback",
    app: safeApp(body.app),
    deployment: safeDeployment(body.deployment),
    expected_active_artifact: safeArtifact(body.expected_active_artifact),
  };
}

export function listEnvVarsCommand(app) {
  return { type: "list_env_vars", app: safeApp(app) };
}

export function setEnvVarCommand(app, body) {
  assertExactKeys(body, ["key", "value"]);
  return {
    type: "set_env_var",
    app: safeApp(app),
    key: safeEnvKey(body.key),
    value: safeEnvValue(body.value),
  };
}

export function removeEnvVarCommand(app, key) {
  return { type: "remove_env_var", app: safeApp(app), key: safeEnvKey(key) };
}

export function convertManifestCommand(body) {
  if (!body || typeof body !== "object" || Array.isArray(body) || Object.keys(body).some((key) => key !== "code" && key !== "owner")) {
    throw new HttpInputError(422, "validation", "request contains unsupported fields");
  }
  const code = body.code;
  const owner = body.owner;
  if (typeof code !== "string" || code.length === 0 || code.length > 512 || /[\u0000-\u001f\u007f]/u.test(code)) {
    throw new HttpInputError(422, "validation", "manifest conversion code is invalid");
  }
  if (owner !== undefined) safeGithubOwner(owner);
  const command = { type: "convert_manifest", code };
  if (owner !== undefined) command.owner = owner;
  return command;
}

export function githubStatusCommand() {
  return { type: "github_status" };
}

export function listInstallationRepositoriesCommand(installationId) {
  return { type: "list_installation_repositories", installation_id: safePositiveId(installationId) };
}

export function listRepositoriesCommand(limit = 50) {
  return { type: "list_repositories", limit: safeListLimit(limit) };
}

export function configureRepositoryCommand(body) {
  assertObjectKeys(
    body,
    ["installation_id", "repository_id", "owner", "name", "branch", "app", "domain", "engine_version"],
    ["entry"],
  );
  const repository = {
    installation_id: safePositiveId(body.installation_id),
    repository_id: safePositiveId(body.repository_id),
    owner: safeGithubOwner(body.owner),
    name: safeGithubName(body.name),
    branch: safeGithubBranch(body.branch),
    app: safeApp(body.app),
    domain: safeDomain(body.domain),
    engine_version: safeVersion(body.engine_version),
  };
  // Empty/omitted entry → daemon auto-detects static vs server.
  if (body.entry !== undefined && body.entry !== null && String(body.entry).trim() !== "") {
    repository.entry = safeEntry(body.entry);
  }
  return {
    type: "configure_repository",
    repository,
  };
}

export function listDeployJobsCommand(cursor, limit = 50) {
  if (cursor !== undefined) safeCursor(cursor);
  return { type: "list_deploy_jobs", ...(cursor === undefined ? {} : { cursor }), limit: safeListLimit(limit) };
}

export function retryDeployJobCommand(jobId) {
  return { type: "retry_deploy_job", job_id: safeDeployment(jobId) };
}

function safeListLimit(value) {
  if (!Number.isInteger(value) || value < 1 || value > 50) throw new HttpInputError(422, "validation", "limit must be an integer between 1 and 50");
  return value;
}

function safePositiveId(value) {
  const number = typeof value === "number" ? value : Number(value);
  if (!Number.isSafeInteger(number) || number <= 0) throw new HttpInputError(422, "validation", "identifier must be a positive integer");
  return number;
}

function safeCursor(value) {
  if (typeof value !== "string" || value.length === 0 || value.length > MAX_IDENTIFIER_LENGTH || /[\u0000-\u001f\u007f/\\]/u.test(value)) {
    throw new HttpInputError(422, "validation", "cursor is invalid");
  }
  return value;
}

function safeGithubOwner(value) {
  return safeIdentifier(value, "owner", /^[A-Za-z0-9][A-Za-z0-9-]{0,38}$/u);
}

function safeGithubName(value) {
  return safeIdentifier(value, "repository name", /^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$/u);
}

function safeGithubBranch(value) {
  if (typeof value !== "string" || value.length === 0 || value.length > 255 || value.startsWith("/") || value.includes("\\") || /[\u0000-\u001f\u007f]/u.test(value) || value.split("/").some((part) => !part || part === "." || part === "..")) {
    throw new HttpInputError(422, "validation", "branch is invalid");
  }
  return value;
}

function assertExactKeys(value, expected) {
  assertObjectKeys(value, expected, []);
}

function assertObjectKeys(value, required, optional) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new HttpInputError(422, "validation", "request body must be a JSON object");
  }
  const actual = Object.keys(value);
  const allowed = new Set([...required, ...optional]);
  if (actual.some((key) => !allowed.has(key)) || required.some((key) => !Object.hasOwn(value, key))) {
    throw new HttpInputError(422, "validation", "request contains unsupported fields");
  }
}

function safeInteger(value, name, minimum, maximum) {
  if (!Number.isSafeInteger(value) || value < minimum || value > maximum) {
    throw new HttpInputError(422, "validation", `${name} must be an integer between ${minimum} and ${maximum}`);
  }
  return value;
}

function safeApp(value) {
  return safeIdentifier(value, "app", /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/u);
}
function safeDeployment(value) {
  return safeIdentifier(value, "deployment", /^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/u);
}
function safeArtifact(value) {
  return safeIdentifier(value, "expected_active_artifact", /^[A-Za-z0-9][A-Za-z0-9._:-]{0,255}$/u);
}
function safeVersion(value) {
  return safeIdentifier(value, "engine_version", /^[A-Za-z0-9][A-Za-z0-9._:+-]{0,127}$/u);
}
function safeIdentifier(value, name, pattern) {
  if (typeof value !== "string" || value.length === 0 || value.length > MAX_IDENTIFIER_LENGTH || !pattern.test(value)) {
    throw new HttpInputError(422, "validation", `${name} is invalid`);
  }
  return value;
}
function safeDomain(value) {
  if (typeof value !== "string" || value.length === 0 || value.length > 253 || /[\u0000-\u0020/\\:@#[\]]/u.test(value)) {
    throw new HttpInputError(422, "validation", "domain is invalid");
  }
  const host = value.startsWith("*.") ? value.slice(2) : value;
  if (!host || host.split(".").some((label) => !/^[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?$/u.test(label))) {
    throw new HttpInputError(422, "validation", "domain is invalid");
  }
  return value;
}
function nullableDomain(value, name) {
  if (value === null) return null;
  try {
    return safeDomain(value);
  } catch {
    throw new HttpInputError(422, "validation", `${name} is invalid`);
  }
}
function safeTlsMode(value) {
  if (value !== "acme" && value !== "self_signed") {
    throw new HttpInputError(422, "validation", "mode must be acme or self_signed");
  }
  return value;
}
function safeEnvKey(value) {
  if (typeof value !== "string" || value.length === 0 || value.length > MAX_IDENTIFIER_LENGTH || !/^[A-Za-z_][A-Za-z0-9_]*$/u.test(value)) {
    throw new HttpInputError(422, "validation", "env var key is invalid");
  }
  if (["CYGNUS_SOCKET", "PATH", "HOME"].includes(value)) {
    throw new HttpInputError(422, "validation", `${value} is reserved by the daemon`);
  }
  return value;
}
function safeEnvValue(value) {
  if (typeof value !== "string" || Buffer.byteLength(value) > 32 * 1024 || /\u0000/u.test(value)) {
    throw new HttpInputError(422, "validation", "env var value is invalid");
  }
  return value;
}
function safeEnvMap(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new HttpInputError(422, "validation", "env must be an object");
  }
  const entries = Object.entries(value);
  if (entries.length > 100) {
    throw new HttpInputError(422, "validation", "env vars exceed 100 entries per request");
  }
  const env = {};
  for (const [key, entryValue] of entries) {
    env[safeEnvKey(key)] = safeEnvValue(entryValue);
  }
  return env;
}
function safePreviewSlug(value) {
  if (typeof value !== "string" || value.length === 0 || value.length > MAX_IDENTIFIER_LENGTH || !/^[A-Za-z0-9][A-Za-z0-9._-]*$/u.test(value)) {
    throw new HttpInputError(422, "validation", "preview slug is invalid");
  }
  return value;
}
function safeEmail(value) {
  if (typeof value !== "string" || Buffer.byteLength(value) < 1 || Buffer.byteLength(value) > 254 || /[\u0000-\u001f\u007f]/u.test(value)) {
    throw new HttpInputError(422, "validation", "email is invalid");
  }
  return value;
}
function safePassword(value) {
  const bytes = typeof value === "string" ? Buffer.byteLength(value) : 0;
  if (typeof value !== "string" || bytes < 12 || bytes > 1024 || /[\u0000-\u001f\u007f]/u.test(value)) {
    throw new HttpInputError(422, "validation", "password is invalid");
  }
  return value;
}
function safeAccountSubject(value) {
  return typeof value === "string" && value.length <= MAX_IDENTIFIER_LENGTH && /^account:[1-9]\d*$/u.test(value);
}
function safeEntry(value) {
  if (typeof value !== "string" || value.length === 0 || value.length > 4096 || value.startsWith("/") || value.includes("\\") || /[\u0000-\u001f\u007f]/u.test(value)) {
    throw new HttpInputError(422, "validation", "entry must be a workspace-relative path");
  }
  if (value.split("/").some((part) => !part || part === "." || part === "..")) {
    throw new HttpInputError(422, "validation", "entry must be a workspace-relative path");
  }
  return value;
}

function assertQueryKeys(url, allowed) {
  const keys = [...new Set(url.searchParams.keys())];
  if (keys.some((key) => !allowed.includes(key))) throw new HttpInputError(422, "validation", "query contains unsupported fields");
}
function listLimit(url) {
  const raw = url.searchParams.get("limit") ?? "50";
  const limit = Number(raw);
  if (!Number.isInteger(limit) || limit < 1 || limit > 50) throw new HttpInputError(422, "validation", "limit must be an integer between 1 and 50");
  return limit;
}
function observabilityLimit(url) {
  return integerQuery(url, "limit", 100, 1, 500);
}
function logStream(url) {
  const values = url.searchParams.getAll("stream");
  if (values.length !== 1 || (values[0] !== "stdout" && values[0] !== "stderr")) {
    throw new HttpInputError(422, "validation", "stream must be stdout or stderr");
  }
  return values[0];
}
function logOffset(url) {
  return integerQuery(url, "offset", 0, 0, Number.MAX_SAFE_INTEGER);
}
function logLimit(url) {
  return integerQuery(url, "limit", 16_384, 1, 49_152);
}
function integerQuery(url, key, defaultValue, minimum, maximum) {
  const values = url.searchParams.getAll(key);
  if (values.length === 0) return defaultValue;
  const raw = values[0];
  if (values.length !== 1 || !/^(?:0|[1-9]\d*)$/u.test(raw)) {
    throw new HttpInputError(422, "validation", `${key} must be an integer between ${minimum} and ${maximum}`);
  }
  const value = Number(raw);
  if (!Number.isSafeInteger(value) || value < minimum || value > maximum) {
    throw new HttpInputError(422, "validation", `${key} must be an integer between ${minimum} and ${maximum}`);
  }
  return value;
}
function optionalQuery(url, key) {
  const value = url.searchParams.get(key);
  if (value === null || value === "") return undefined;
  if (value.length > MAX_IDENTIFIER_LENGTH || /[\u0000-\u001f\u007f/\\]/u.test(value)) throw new HttpInputError(422, "validation", `${key} is invalid`);
  return value;
}
function decodeSegment(value, name = "path identifier") {
  let decoded;
  try {
    decoded = decodeURIComponent(value);
  } catch {
    throw new HttpInputError(422, "validation", `${name} is invalid`);
  }
  if (!decoded || decoded.length > MAX_IDENTIFIER_LENGTH || /[\u0000-\u001f\u007f/\\]/u.test(decoded)) throw new HttpInputError(422, "validation", `${name} is invalid`);
  return decoded;
}
function decodeDomainSegment(value, name = "domain") {
  let decoded;
  try {
    decoded = decodeURIComponent(value);
  } catch {
    throw new HttpInputError(422, "validation", `${name} is invalid`);
  }
  try {
    return safeDomain(decoded);
  } catch {
    throw new HttpInputError(422, "validation", `${name} is invalid`);
  }
}
function inputErrorResponse(error) {
  return apiError(
    error instanceof HttpInputError ? error.status : 422,
    error instanceof HttpInputError ? error.code : "validation",
    error instanceof Error ? error.message : "invalid request",
  );
}

async function readJsonBody(request, maxBytes = MAX_JSON_BODY_BYTES) {
  const rawLength = request.headers.get("content-length");
  if (rawLength !== null && (!/^\d+$/u.test(rawLength) || Number(rawLength) > maxBytes)) {
    throw new HttpInputError(413, "body_too_large", "request body is too large");
  }
  const reader = request.body?.getReader();
  if (!reader) throw new HttpInputError(422, "validation", "request body is required");
  const chunks = [];
  let total = 0;
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > maxBytes) {
      await reader.cancel().catch(() => {});
      throw new HttpInputError(413, "body_too_large", "request body is too large");
    }
    chunks.push(Buffer.from(value));
  }
  if (total === 0) throw new HttpInputError(422, "validation", "request body is required");
  try {
    return JSON.parse(Buffer.concat(chunks).toString("utf8"));
  } catch {
    throw new HttpInputError(422, "validation", "request body must be valid JSON");
  }
}
async function assertEmptyBody(request) {
  const rawLength = request.headers.get("content-length");
  if (rawLength !== null && (!/^\d+$/u.test(rawLength) || Number(rawLength) !== 0)) throw new HttpInputError(422, "validation", "request body must be empty");
  const reader = request.body?.getReader();
  if (!reader) return;
  const { done, value } = await reader.read();
  if (!done && value?.byteLength) {
    await reader.cancel().catch(() => {});
    throw new HttpInputError(422, "validation", "request body must be empty");
  }
}

export function buildGithubManifest(origin) {
  if (typeof origin !== "string" || !/^https?:\/\/[^/]+$/u.test(origin)) {
    throw new HttpInputError(422, "validation", "console origin is invalid");
  }
  return {
    name: "Cygnus Tenant Zero",
    url: origin,
    redirect_url: `${origin}${GITHUB_MANIFEST_CALLBACK_PATH}`,
    setup_url: `${origin}${GITHUB_SETUP_PATH}`,
    callback_urls: [`${origin}${GITHUB_INSTALL_CALLBACK_PATH}`],
    public: false,
    hook_attributes: { url: `${origin}${GITHUB_WEBHOOK_PATH}`, active: true },
    default_permissions: {
      contents: "read",
      pull_requests: "read",
      checks: "write",
      deployments: "write",
    },
    default_events: ["push", "pull_request"],
  };
}

export function manifestStateSize(now = Date.now()) {
  pruneManifestStates(now);
  return manifestStates.size;
}

export function clearManifestStates() {
  manifestStates.clear();
}

async function manifestStart(request, url, sessionCookie) {
  const scheme = forwardedScheme(request, url);
  if (scheme !== "https:" && !["localhost", "127.0.0.1", "[::1]"].includes(url.hostname)) {
    return apiError(422, "github_origin", "GitHub App setup requires an HTTPS console origin");
  }
  let body;
  try {
    body = await readJsonBody(request);
  } catch (error) {
    return apiError(error.status ?? 422, error.code ?? "validation", error.message ?? "invalid request");
  }
  if (!body || typeof body !== "object" || Array.isArray(body)) return apiError(422, "validation", "request body must be a JSON object");
  const keys = Object.keys(body);
  let owner;
  try {
    owner = body.owner === undefined || body.owner === null || body.owner === "" ? undefined : safeGithubOwner(body.owner);
  } catch (error) {
    return apiError(error.status ?? 422, error.code ?? "validation", error.message ?? "owner is invalid");
  }
  const verified = verifySessionCookie(sessionCookie);
  if (!verified) return apiError(401, "unauthorized", "authentication required");
  const now = Date.now();
  pruneManifestStates(now);
  while (manifestStates.size >= MAX_MANIFEST_STATES) manifestStates.delete(manifestStates.keys().next().value);
  const state = randomBytes(32).toString("base64url");
  manifestStates.set(createHash("sha256").update(state, "utf8").digest("hex"), {
    owner,
    sessionHash: createHash("sha256").update(String(sessionCookie), "utf8").digest("hex"),
    expiresAt: now + MANIFEST_STATE_TTL_MS,
  });
  const manifest = buildGithubManifest(`${scheme}//${url.host}`);
  const action = owner
    ? `https://github.com/organizations/${encodeURIComponent(owner)}/settings/apps/new?state=${encodeURIComponent(state)}`
    : `https://github.com/settings/apps/new?state=${encodeURIComponent(state)}`;
  return jsonResponse({ ok: true, data: { action, manifest } });
}

function pruneManifestStates(now = Date.now()) {
  for (const [hash, entry] of manifestStates) if (entry.expiresAt <= now) manifestStates.delete(hash);
}

export function consumeManifestState(state, sessionCookie, now = Date.now()) {
  if (typeof state !== "string" || state.length < 22 || state.length > 128 || !/^[A-Za-z0-9_-]+$/u.test(state)) return null;
  pruneManifestStates(now);
  const hash = createHash("sha256").update(state, "utf8").digest("hex");
  const entry = manifestStates.get(hash);
  if (!entry) return null;
  const sessionHash = createHash("sha256").update(String(sessionCookie ?? ""), "utf8").digest("hex");
  if (!constantTimeStringEqual(sessionHash, entry.sessionHash)) return null;
  manifestStates.delete(hash);
  return { ...entry };
}
// GitHub's manifest flow sends the temporary conversion `code` to the
// `redirect_url` declared in the manifest (NOT `setup_url` — that's the
// post-install setup URL, only used if the app needs extra configuration).
// The docs example is literally: `https://example.com/redirect?code=…&state=…`
// This handler does the conversion (POST /app-manifests/{code}/conversions)
// and bounces the user back to the console. The conversion must complete
// within one hour, and requires a session cookie so the state token we
// minted in manifestStart can be validated.
async function manifestCallback(request, url, requestAdmin = adminRequest, socket = adminSocketPath) {
  if (request.method !== "GET") return methodNotAllowed("GET");
  const code = url.searchParams.get("code");
  const state = url.searchParams.get("state");
  if (!code || !state) return apiError(400, "github_callback", "GitHub callback is missing code or state");
  try {
    convertManifestCommand({ code });
  } catch (error) {
    return apiError(error.status ?? 400, error.code ?? "github_callback", "GitHub callback code is invalid");
  }
  const auth = authStatus();
  if (!auth.configured) return apiError(503, "misconfigured", "console authentication is misconfigured");
  const cookie = request.headers.get("cookie");
  if (!verifySessionCookie(cookie)) return apiError(401, "unauthorized", "authentication required");
  const stateEntry = consumeManifestState(state, cookie);
  if (!stateEntry) return apiError(400, "github_state", "GitHub callback state is invalid or expired");
  if (!socket) return apiError(503, "unavailable", "daemon admin bridge unavailable");
  try {
    await requestAdmin(
      socket,
      convertManifestCommand({ code, ...(stateEntry.owner === undefined ? {} : { owner: stateEntry.owner }) }),
      ACTOR_SUBJECT,
    );
    return new Response(null, {
      status: 303,
      headers: { location: "/?github=configured", "cache-control": "no-store" },
    });
  } catch (error) {
    const protocolCode = error instanceof AdminProtocolError ? error.code : "internal";
    return apiError(statusForDaemonCode(protocolCode), publicDaemonCode(protocolCode), safeErrorMessage(protocolCode));
  }
}

// `setup_url` is hit after the user installs the app (if `setup_on_update`
// is set, or the app asked for additional setup). GitHub decides which
// params to include — we just bounce the user back to the console. The
// conversion already happened at `redirect_url`.
function githubSetupCallback(request, url) {
  if (request.method !== "GET") return methodNotAllowed("GET");
  const installationId = url.searchParams.get("installation_id");
  const hasInstallationId = installationId !== null
    && /^\d+$/u.test(installationId)
    && Number(installationId) > 0;
  const location = hasInstallationId
    ? `/?github=setup&installation_id=${encodeURIComponent(installationId)}`
    : "/?github=configured";
  return new Response(null, {
    status: 303,
    headers: { location, "cache-control": "no-store" },
  });
}

export async function webhookIngress(request, requestAdmin = adminRequest, socket = adminSocketPath) {
  if (request.method !== "POST") return methodNotAllowed("POST");
  const rawLength = request.headers.get("content-length");
  if (rawLength === null || !/^\d+$/u.test(rawLength)) return apiError(411, "length_required", "webhook Content-Length is required");
  const totalBytes = Number(rawLength);
  if (!Number.isSafeInteger(totalBytes) || totalBytes < 1 || totalBytes > MAX_WEBHOOK_BODY_BYTES) return apiError(413, "body_too_large", "webhook body is too large");
  const deliveryId = request.headers.get("x-github-delivery") ?? "";
  const event = request.headers.get("x-github-event") ?? "";
  const signature = request.headers.get("x-hub-signature-256") ?? "";
  if (!deliveryId || deliveryId.length > MAX_IDENTIFIER_LENGTH || /[\u0000-\u001f\u007f]/u.test(deliveryId)) return apiError(400, "webhook_delivery", "webhook delivery header is invalid");
  if (!event || event.length > MAX_IDENTIFIER_LENGTH || /[^A-Za-z0-9_.-]/u.test(event)) return apiError(400, "webhook_event", "webhook event header is invalid");
  if (!/^sha256=[0-9a-f]{64}$/u.test(signature)) return apiError(400, "webhook_signature", "webhook signature header is invalid");
  if (!socket) return apiError(503, "unavailable", "daemon admin bridge unavailable");
  const reader = request.body?.getReader();
  if (!reader) return apiError(400, "body_required", "webhook body is required");
  let begun;
  try {
    begun = await requestAdmin(socket, { type: "webhook_begin", delivery_id: deliveryId, event, signature, total_bytes: totalBytes }, "github:webhook");
  } catch (error) {
    const code = error instanceof AdminProtocolError ? error.code : "internal";
    return apiError(statusForDaemonCode(code), publicDaemonCode(code), safeErrorMessage(code, error instanceof AdminProtocolError ? error.message : undefined));
  }
  const duplicate = begun?.data?.duplicate === true;
  if (duplicate) {
    await reader.cancel().catch(() => {});
    return jsonResponse({ ok: true, data: { delivery_id: deliveryId, duplicate: true } }, false, 202);
  }
  let received = 0;
  let carry = Buffer.alloc(0);
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      const bytes = Buffer.from(value);
      received += bytes.length;
      if (received > totalBytes || received > MAX_WEBHOOK_BODY_BYTES) {
        await reader.cancel().catch(() => {});
        await requestAdmin(socket, { type: "webhook_finish", delivery_id: deliveryId }, "github:webhook").catch(() => {});
        return apiError(413, "body_too_large", "webhook body is too large");
      }
      carry = carry.length ? Buffer.concat([carry, bytes]) : bytes;
      while (carry.length >= MAX_WEBHOOK_CHUNK_BYTES) {
        const chunk = carry.subarray(0, MAX_WEBHOOK_CHUNK_BYTES);
        carry = carry.subarray(MAX_WEBHOOK_CHUNK_BYTES);
        await requestAdmin(socket, { type: "webhook_chunk", delivery_id: deliveryId, chunk_base64: chunk.toString("base64") }, "github:webhook");
      }
    }
    if (received !== totalBytes) {
      await requestAdmin(socket, { type: "webhook_finish", delivery_id: deliveryId }, "github:webhook").catch(() => {});
      return apiError(400, "length_mismatch", "webhook body length does not match Content-Length");
    }
    if (carry.length) await requestAdmin(socket, { type: "webhook_chunk", delivery_id: deliveryId, chunk_base64: carry.toString("base64") }, "github:webhook");
    const finished = await requestAdmin(socket, { type: "webhook_finish", delivery_id: deliveryId }, "github:webhook");
    return jsonResponse({ ok: true, data: finished?.data ?? { delivery_id: deliveryId, duplicate: false, jobs: 0 } }, false, 202);
  } catch (error) {
    await requestAdmin(socket, { type: "webhook_finish", delivery_id: deliveryId }, "github:webhook").catch(() => {});
    const code = error instanceof AdminProtocolError ? error.code : "internal";
    return apiError(statusForDaemonCode(code), publicDaemonCode(code), safeErrorMessage(code, error instanceof AdminProtocolError ? error.message : undefined));
  }
}

// Bun's request.url (and url.origin/url.protocol derived from it) always uses
// the scheme of the raw HTTP request Bun itself terminated — http:, even when
// this console sits behind the daemon's TLS-terminating reverse proxy on a
// plain UNIX socket. Trust X-Forwarded-Proto (set by the daemon relay) for
// the scheme the browser actually saw; fall back to the connection's own
// protocol when unset (direct/local access, or tests hitting the handler
// with a literal https: URL). Deliberately NOT requestIsSecure()'s
// loopback-is-secure allowance — that heuristic is for cookie flags, and
// would make an `http://localhost` Origin/manifest fail to match an inferred
// https scheme here.
function forwardedScheme(request, url) {
  const forwarded = request.headers?.get?.("x-forwarded-proto")?.split(",")[0]?.trim()?.toLowerCase();
  return forwarded === "https" ? "https:" : forwarded === "http" ? "http:" : url.protocol;
}

function sameOrigin(request, url) {
  const origin = request.headers.get("origin");
  if (typeof origin !== "string" || request.headers.get("sec-fetch-site") === "cross-site") {
    return false;
  }
  const scheme = forwardedScheme(request, url);
  const expected = `${scheme}//${url.host}`;
  return origin === expected;
}

function requestIp(request) {
  const value = server?.requestIP?.(request);
  return value?.address || "unknown";
}
function loginThrottle(ip, now) {
  const current = loginAttempts.get(ip);
  if (current?.blockedUntil > now) return { blocked: true, retryAt: current.blockedUntil };
  if (current && current.blockedUntil > 0 && current.blockedUntil <= now) loginAttempts.delete(ip);
  return { blocked: false };
}
function recordLoginFailure(ip, now) {
  if (loginAttempts.size >= MAX_LOGIN_TRACKED_IPS && !loginAttempts.has(ip)) {
    loginAttempts.delete(loginAttempts.keys().next().value);
  }
  const current = loginAttempts.get(ip) ?? { failures: 0, blockedUntil: 0 };
  current.failures += 1;
  if (current.failures >= MAX_LOGIN_ATTEMPTS) current.blockedUntil = now + LOGIN_BLOCK_MS;
  loginAttempts.set(ip, current);
  return { blocked: current.blockedUntil > now, retryAt: current.blockedUntil };
}

export function statusForDaemonCode(code) {
  if (code === "unauthorized") return 401;
  if (code === "forbidden") return 403;
  if (code === "not_found") return 404;
  if (code === "conflict") return 409;
  if (code === "validation" || code === "invalid_request" || code === "unsupported_version") return 422;
  return 503;
}
function publicDaemonCode(code) {
  if (["unauthorized", "forbidden", "not_found", "conflict", "validation"].includes(code)) return code;
  if (code === "invalid_request" || code === "unsupported_version") return "validation";
  return "unavailable";
}
function sanitizeGithubData(data) {
  if (!data || typeof data !== "object") return { kind: "unknown" };
  const kind = data.kind;
  if (kind === "repositories" || kind === "installation_repositories" || kind === "discoverable_repositories") {
    const repositories = Array.isArray(data.repositories)
      ? data.repositories.map((repo) => kind === "repositories" ? sanitizeConfiguredRepository(repo) : sanitizeInstallationRepository(repo))
      : [];
    const out = { kind, repositories, ...(data.next_cursor ? { next_cursor: data.next_cursor } : {}) };
    if (kind === "discoverable_repositories") {
      out.installations = Array.isArray(data.installations)
        ? data.installations.map((item) => ({
            installation_id: item?.installation_id,
            account_login: item?.account_login,
            account_type: item?.account_type,
          }))
        : [];
    }
    return out;
  }
  if (kind === "repository_configured") return { kind, repository: sanitizeConfiguredRepository(data.repository) };
  if (kind === "github_status" || kind === "git_hub_status" || kind === "manifest_converted") {
    return { kind: "github_status", ...(typeof data.configured === "boolean" ? { configured: data.configured } : {}), ...(data.app ? { app: sanitizeGithubApp(data.app) } : {}) };
  }
  if (kind === "deploy_jobs") {
    const jobs = Array.isArray(data.jobs) ? data.jobs.map(sanitizeGithubJob) : [];
    return { kind, jobs, ...(data.next_cursor ? { next_cursor: data.next_cursor } : {}) };
  }
  if (kind === "deploy_job_retried") return { kind, job: sanitizeGithubJob(data.job) };
  if (kind === "webhook_begun") return { kind, delivery_id: data.delivery_id, duplicate: data.duplicate === true };
  if (kind === "webhook_chunked") return { kind, delivery_id: data.delivery_id, received_bytes: data.received_bytes };
  if (kind === "webhook_accepted") return { kind, delivery_id: data.delivery_id, duplicate: data.duplicate === true, jobs: data.jobs };
  return { kind: typeof kind === "string" ? kind : "unknown" };
}

function sanitizeGithubApp(app) {
  return {
    app_id: app?.app_id,
    client_id: app?.client_id,
    name: app?.name,
    html_url: app?.html_url,
    owner: app?.owner,
    configured_at: app?.configured_at,
  };
}

function sanitizeInstallationRepository(repo) {
  return {
    installation_id: repo?.installation_id,
    repository_id: repo?.repository_id,
    owner: repo?.owner,
    name: repo?.name,
    full_name: repo?.full_name,
    default_branch: repo?.default_branch,
    private: repo?.private === true,
  };
}

function sanitizeConfiguredRepository(repo) {
  return {
    installation_id: repo?.installation_id,
    repository_id: repo?.repository_id,
    owner: repo?.owner,
    name: repo?.name,
    branch: repo?.branch,
    app: repo?.app,
    domain: repo?.domain,
    engine_version: repo?.engine_version,
    entry: repo?.entry,
  };
}

function sanitizeGithubJob(job) {
  return {
    id: job?.id,
    key: job?.key,
    source: job?.source,
    source_ref: job?.source_ref,
    app: job?.app,
    installation_id: job?.installation_id,
    repository_id: job?.repository_id,
    owner: job?.owner,
    name: job?.name,
    environment: job?.environment,
    kind: job?.kind,
    pull_request: job?.pull_request,
    sha: job?.sha,
    status: job?.status,
    attempts: job?.attempts,
    next_attempt_at: job?.next_attempt_at,
    error: job?.error,
    check_run_id: job?.check_run_id,
    github_deployment_id: job?.github_deployment_id,
    deployment_id: job?.deployment_id,
    created_at: job?.created_at,
    updated_at: job?.updated_at,
  };
}

function safeErrorMessage(code, message) {
  if (code === "unauthorized") return "authentication required";
  if (code === "forbidden") return "permission denied";
  if (code === "not_found") return message || "requested object was not found";
  if (code === "conflict") return message || "state changed; refresh and try again";
  if (code === "validation" || code === "invalid_request" || code === "unsupported_version") {
    // Daemon validation messages are crafted for the operator; hiding them
    // turns a precise explanation into a dead end. Pass them through.
    return message || "request was rejected";
  }
  // Transport/timeout failures already carry the OS error code (e.g. ENOENT,
  // ECONNREFUSED) from the admin client — surface it so a broken bridge is
  // diagnosable instead of an opaque "unavailable".
  if (code === "transport" || code === "timeout") {
    return message || "daemon admin bridge unavailable";
  }
  return "daemon admin bridge unavailable";
}

class HttpInputError extends Error {
  constructor(status, code, message) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

function apiError(status, code, message, headers = {}) {
  return jsonResponse({ ok: false, error: { code, message } }, false, status, headers);
}
function methodNotAllowed(allow) {
  return apiError(405, "method_not_allowed", "method not allowed", { allow });
}
function jsonResponse(value, head = false, status = 200, headers = {}) {
  const responseHeaders = new Headers({
    "cache-control": "no-store",
    "content-type": "application/json; charset=utf-8",
  });
  for (const [key, raw] of Object.entries(headers ?? {})) {
    if (raw == null) continue;
    // Multiple Set-Cookie values must be appended, not comma-joined.
    if (key.toLowerCase() === "set-cookie" && Array.isArray(raw)) {
      for (const cookie of raw) responseHeaders.append("set-cookie", cookie);
      continue;
    }
    responseHeaders.set(key, String(raw));
  }
  return new Response(head ? null : JSON.stringify(value), {
    status,
    headers: responseHeaders,
  });
}
function base64UrlEncode(value) {
  return Buffer.from(value, "utf8").toString("base64url");
}
function base64UrlDecode(value) {
  return Buffer.from(value, "base64url").toString("utf8");
}

if (import.meta.main) {
  console.log(
    `cygnus-console · tenant-0 · daemon bridge ${adminSocketPath ? "configured" : "offline"} · listening on ${
      socketPath ? `unix:${socketPath}` : server.url.href
    }`,
  );
}
