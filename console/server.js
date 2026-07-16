import {
  createHash,
  createHmac,
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
export const SESSION_TTL_SECONDS = 12 * 60 * 60;
export const MAX_JSON_BODY_BYTES = 32 * 1024;
export const MAX_IDENTIFIER_LENGTH = 128;
const MAX_LOGIN_ATTEMPTS = 5;
const LOGIN_BLOCK_MS = 60_000;
const MAX_LOGIN_TRACKED_IPS = 1024;
const loginAttempts = new Map();

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
  server = Bun.serve({
    ...(socketPath ? { unix: socketPath } : { port }),
    async fetch(request) {
      const url = new URL(request.url);
      if (request.method !== "GET" && request.method !== "HEAD" && !sameOrigin(request, url)) {
        return apiError(403, "csrf", "request origin is not allowed");
      }
      if (url.pathname === "/healthz") {
        if (request.method !== "GET" && request.method !== "HEAD") return methodNotAllowed("GET, HEAD");
        return jsonResponse(
          {
            ok: true,
            service: "cygnus-console",
            tenant: "tenant-0",
            mode: adminSocketPath ? "live" : "preview",
            dataSource: adminSocketPath ? "daemon" : "unavailable",
            daemonBridge: adminSocketPath ? "configured" : "offline",
            auth: authStatus(),
            locked: Boolean(adminSocketPath && !authStatus().configured),
          },
          request.method === "HEAD",
        );
      }

      if (url.pathname.startsWith("/api/v1/")) {
        return handleApi(request, url);
      }

      if (request.method !== "GET" && request.method !== "HEAD") {
        return methodNotAllowed("GET, HEAD");
      }
      return new Response(request.method === "HEAD" ? null : indexFile, {
        headers: {
          "cache-control": "no-store",
          "content-type": "text/html; charset=utf-8",
        },
      });
    },
  });
}

export async function handleApi(request, url) {
  const path = url.pathname;
  if (path === "/api/v1/session") {
    if (request.method === "GET" || request.method === "HEAD") {
      return jsonResponse(sessionStatus(request), request.method === "HEAD");
    }
    if (request.method === "POST") {
      if (!sameOrigin(request, url)) return apiError(403, "csrf", "request origin is not allowed");
      return login(request);
    }
    if (request.method === "DELETE") {
      if (!sameOrigin(request, url)) return apiError(403, "csrf", "request origin is not allowed");
      return logout(request);
    }
    return methodNotAllowed("GET, HEAD, POST, DELETE");
  }
  if (path === "/api/v1/logout" || path === "/api/v1/session/logout") {
    if (request.method !== "POST") return methodNotAllowed("POST");
    if (!sameOrigin(request, url)) return apiError(403, "csrf", "request origin is not allowed");
    return logout(request);
  }

  const mutationRoute = ["/api/v1/map-domain", "/api/v1/rollback", "/api/v1/deploy"].includes(path);
  const readRoute = /^(?:\/api\/v1\/(?:status|apps|deployments))(?:\/[^/]+)?$/u.test(path);
  if (mutationRoute && request.method !== "POST") return methodNotAllowed("POST");
  if (readRoute && request.method !== "GET" && request.method !== "HEAD") return methodNotAllowed("GET, HEAD");

  if (request.method !== "GET" && request.method !== "HEAD") {
    if (!sameOrigin(request, url)) return apiError(403, "csrf", "request origin is not allowed");
  }

  const auth = authStatus();
  if (!auth.configured) {
    return apiError(503, "misconfigured", "console authentication is misconfigured");
  }
  if (!verifySessionCookie(request.headers.get("cookie"))) {
    return apiError(401, "unauthorized", "authentication required");
  }
  if (!adminSocketPath) {
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
    const { data, requestId } = await adminRequest(adminSocketPath, command, ACTOR_SUBJECT);
    return jsonResponse({ ok: true, data, requestId }, request.method === "HEAD");
  } catch (error) {
    const code = error instanceof AdminProtocolError ? error.code : "internal";
    const status = statusForDaemonCode(code);
    return apiError(status, publicDaemonCode(code), safeErrorMessage(code));
  }
}

async function login(request) {
  const ip = requestIp(request);
  const now = Date.now();
  const throttle = loginThrottle(ip, now);
  if (throttle.blocked) {
    return apiError(429, "rate_limited", "too many login attempts", {
      "retry-after": String(Math.max(1, Math.ceil((throttle.retryAt - now) / 1000))),
    });
  }

  let token = "";
  try {
    const body = await readJsonBody(request);
    assertExactKeys(body, ["token"]);
    if (typeof body.token !== "string" || body.token.length > 1024) {
      throw new HttpInputError(422, "validation", "token is invalid");
    }
    token = body.token;
  } catch (error) {
    // Hash an empty value too, keeping malformed and incorrect credentials on the
    // same comparison path without ever reflecting the submitted secret.
    constantTimeTokenMatch(token);
    return apiError(
      error instanceof HttpInputError ? error.status : 422,
      error instanceof HttpInputError ? error.code : "validation",
      error instanceof Error ? error.message : "invalid request",
    );
  }

  const validToken = constantTimeTokenMatch(token);
  const auth = authStatus();
  if (!auth.configured) {
    return apiError(503, "misconfigured", "console authentication is misconfigured");
  }
  if (!validToken) {
    const state = recordLoginFailure(ip, now);
    const headers = state.blocked
      ? { "retry-after": String(Math.max(1, Math.ceil((state.retryAt - now) / 1000))) }
      : {};
    return apiError(401, "unauthorized", "invalid credentials", headers);
  }

  loginAttempts.delete(ip);
  const cookie = signSession();
  return jsonResponse(
    { ok: true, data: { authenticated: true, actor: ACTOR_SUBJECT } },
    false,
    200,
    { "set-cookie": sessionSetCookie(cookie) },
  );
}

function logout() {
  return jsonResponse(
    { ok: true, data: { authenticated: false } },
    false,
    200,
    { "set-cookie": sessionClearCookie() },
  );
}

function sessionStatus(request) {
  const auth = authStatus();
  const session = verifySessionCookie(request.headers.get("cookie"));
  return {
    ok: true,
    data: {
      authenticated: Boolean(session),
      actor: session ? ACTOR_SUBJECT : undefined,
      configured: auth.configured,
      locked: !auth.configured,
    },
  };
}

export function authStatus() {
  const bootstrap = credential("CYGNUS_CONSOLE_BOOTSTRAP_TOKEN", "CYGNUS_CONSOLE_BOOTSTRAP_TOKEN_FILE");
  const sessionKey = credential("CYGNUS_CONSOLE_SESSION_KEY", "CYGNUS_CONSOLE_SESSION_KEY_FILE");
  return {
    configured: bootstrap.length > 0 && sessionKey.length > 0,
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
  const value = cookie.includes("=") ? parseCookie(cookie, SESSION_COOKIE) : cookie;
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
    payload.sub !== ACTOR_SUBJECT ||
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

function sessionSetCookie(value) {
  return `${SESSION_COOKIE}=${value}; Path=/; Max-Age=${SESSION_TTL_SECONDS}; HttpOnly; Secure; SameSite=Strict`;
}

function sessionClearCookie() {
  return `${SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Strict`;
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
  if (request.method !== "POST") return null;
  if (parts.length === 3 && parts[2] === "map-domain") {
    return mapDomainCommand(await readJsonBody(request));
  }
  if (parts.length === 3 && parts[2] === "rollback") {
    return rollbackCommand(await readJsonBody(request));
  }
  if (parts.length === 3 && parts[2] === "deploy") {
    return deployCommand(await readJsonBody(request));
  }
  return null;
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
  if (parts.length === 3 && parts[2] === "apps") {
    assertQueryKeys(url, ["cursor", "limit"]);
    return { type: "list_apps", cursor: optionalQuery(url, "cursor"), limit: listLimit(url) };
  }
  if (parts.length === 4 && parts[2] === "apps") {
    assertQueryKeys(url, []);
    return { type: "get_app", app: safeApp(decodeSegment(parts[3], "app")) };
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
  return null;
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

export function deployCommand(body) {
  assertExactKeys(body, ["request"]);
  assertExactKeys(body.request, [
    "source_dir",
    "app",
    "domain",
    "engine_version",
    "entry",
    "artifact_root",
    "upstream",
  ]);
  const request = body.request;
  return {
    type: "deploy",
    request: {
      source_dir: absoluteHostPath(request.source_dir, "source_dir"),
      app: safeApp(request.app),
      domain: safeDomain(request.domain),
      engine_version: safeVersion(request.engine_version),
      entry: safeEntry(request.entry),
      artifact_root: absoluteHostPath(request.artifact_root, "artifact_root"),
      upstream: absoluteHostPath(request.upstream, "upstream"),
    },
  };
}

function assertExactKeys(value, expected) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new HttpInputError(422, "validation", "request body must be a JSON object");
  }
  const actual = Object.keys(value).sort();
  const wanted = [...expected].sort();
  if (actual.length !== wanted.length || actual.some((key, i) => key !== wanted[i])) {
    throw new HttpInputError(422, "validation", "request contains unsupported fields");
  }
}

function safeApp(value) {
  return safeIdentifier(value, "app", /^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$/u);
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
function absoluteHostPath(value, name) {
  if (typeof value !== "string" || value.length === 0 || value.length > 4096 || !value.startsWith("/") || /[\u0000-\u001f\u007f]/u.test(value)) {
    throw new HttpInputError(422, "validation", `${name} must be an absolute host path`);
  }
  if (value.split("/").some((part) => part === "..")) {
    throw new HttpInputError(422, "validation", `${name} contains an unsafe path segment`);
  }
  return value;
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

async function readJsonBody(request) {
  const rawLength = request.headers.get("content-length");
  if (rawLength !== null && (!/^\d+$/u.test(rawLength) || Number(rawLength) > MAX_JSON_BODY_BYTES)) {
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
    if (total > MAX_JSON_BODY_BYTES) {
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

function sameOrigin(request, url) {
  const origin = request.headers.get("origin");
  return typeof origin === "string" && origin === url.origin && request.headers.get("sec-fetch-site") !== "cross-site";
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
function safeErrorMessage(code) {
  if (code === "unauthorized") return "authentication required";
  if (code === "forbidden") return "permission denied";
  if (code === "not_found") return "requested object was not found";
  if (code === "conflict") return "state changed; refresh and try again";
  if (code === "validation" || code === "invalid_request" || code === "unsupported_version") return "request was rejected";
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
  return new Response(head ? null : JSON.stringify(value), {
    status,
    headers: {
      "cache-control": "no-store",
      ...headers,
      "content-type": "application/json; charset=utf-8",
    },
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
