import { afterEach, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  ACTOR_SUBJECT,
  MAX_DEPLOY_ADMIN_CHUNK_BYTES,
  MAX_DEPLOY_CHUNK_BYTES,
  MAX_DEPLOY_CHUNK_JSON_BODY_BYTES,
  MAX_DEPLOY_TOTAL_BYTES,
  MAX_JSON_BODY_BYTES,
  MAX_WEBHOOK_BODY_BYTES,
  MAX_WEBHOOK_CHUNK_BYTES,
  SESSION_COOKIE,
  SESSION_COOKIE_INSECURE,
  addAppDomainCommand,
  appDomainTlsCommand,
  buildGithubManifest,
  changePasswordCommand,
  clearManifestStates,
  commandForRequest,
  configureRepositoryCommand,
  constantTimeTokenMatch,
  consumeManifestState,
  dashboardDomainCommand,
  dashboardTlsCommand,
  deployUploadBeginCommand,
  deployUploadChunk,
  deployUploadFinishCommand,
  deployUploadIngress,
  handleApi,
  healthResponse,
  listEnvVarsCommand,
  mapDomainCommand,
  removeAppDomainCommand,
  removeEnvVarCommand,
  requestIsSecure,
  retryDomainAcmeCommand,
  rollbackCommand,
  sessionResponse,
  setEnvVarCommand,
  setPrimaryDomainCommand,
  setup,
  signSession,
  statusForDaemonCode,
  verifySessionCookie,
  webhookIngress,
} from "./server.js";
import { AdminProtocolError } from "./admin-client.js";

const previousBootstrap = process.env.CYGNUS_CONSOLE_BOOTSTRAP_TOKEN;
const previousSessionKey = process.env.CYGNUS_CONSOLE_SESSION_KEY;
const previousBootstrapFile = process.env.CYGNUS_CONSOLE_BOOTSTRAP_TOKEN_FILE;
const previousSessionFile = process.env.CYGNUS_CONSOLE_SESSION_KEY_FILE;

afterEach(() => {
  if (previousBootstrap === undefined) delete process.env.CYGNUS_CONSOLE_BOOTSTRAP_TOKEN;
  else process.env.CYGNUS_CONSOLE_BOOTSTRAP_TOKEN = previousBootstrap;
  if (previousSessionKey === undefined) delete process.env.CYGNUS_CONSOLE_SESSION_KEY;
  else process.env.CYGNUS_CONSOLE_SESSION_KEY = previousSessionKey;
  if (previousBootstrapFile === undefined) delete process.env.CYGNUS_CONSOLE_BOOTSTRAP_TOKEN_FILE;
  else process.env.CYGNUS_CONSOLE_BOOTSTRAP_TOKEN_FILE = previousBootstrapFile;
  if (previousSessionFile === undefined) delete process.env.CYGNUS_CONSOLE_SESSION_KEY_FILE;
  else process.env.CYGNUS_CONSOLE_SESSION_KEY_FILE = previousSessionFile;
});

describe("console session primitives", () => {
  test("signs, rejects tampering, and expires cookies", () => {
    process.env.CYGNUS_CONSOLE_SESSION_KEY = "unit-test-session-key";
    const value = signSession({ iat: 100, exp: 200 }, 100_000);
    expect(verifySessionCookie(value, 100_000)?.sub).toBe(ACTOR_SUBJECT);
    expect(verifySessionCookie(`${value.slice(0, -1)}x`, 100_000)).toBeNull();
    expect(verifySessionCookie(value, 200_000)).toBeNull();
  });

  test("compares bootstrap credentials through a fixed-size digest", () => {
    process.env.CYGNUS_CONSOLE_BOOTSTRAP_TOKEN = "correct horse";
    expect(constantTimeTokenMatch("correct horse")).toBe(true);
    expect(constantTimeTokenMatch("wrong")).toBe(false);
    expect(constantTimeTokenMatch("")).toBe(false);
  });

  test("loads independent raw credentials from rooted cage files", () => {
    const directory = mkdtempSync(join(tmpdir(), "cygnus-console-credentials-"));
    const bootstrap = Buffer.alloc(32, 0xab);
    const session = Buffer.alloc(32, 0xcd);
    const bootstrapPath = join(directory, "bootstrap.token");
    const sessionPath = join(directory, "session.key");
    writeFileSync(bootstrapPath, bootstrap);
    writeFileSync(sessionPath, session);
    delete process.env.CYGNUS_CONSOLE_BOOTSTRAP_TOKEN;
    delete process.env.CYGNUS_CONSOLE_SESSION_KEY;
    process.env.CYGNUS_CONSOLE_BOOTSTRAP_TOKEN_FILE = bootstrapPath;
    process.env.CYGNUS_CONSOLE_SESSION_KEY_FILE = sessionPath;

    expect(constantTimeTokenMatch(bootstrap.toString("hex"))).toBe(true);
    expect(constantTimeTokenMatch(session.toString("hex"))).toBe(false);
    const cookie = signSession({ iat: 100, exp: 200 }, 100_000);
    expect(verifySessionCookie(cookie, 100_000)?.sub).toBe(ACTOR_SUBJECT);
    rmSync(directory, { recursive: true, force: true });
  });
  test("authenticates account credentials through the daemon and preserves bootstrap recovery", async () => {
    process.env.CYGNUS_CONSOLE_BOOTSTRAP_TOKEN = "bootstrap";
    process.env.CYGNUS_CONSOLE_SESSION_KEY = "session-key";
    const url = new URL("https://console.example/api/v1/session");
    const commands = [];
    const requestAdmin = async (socket, command, actor) => {
      commands.push({ socket, command, actor });
      if (command.type === "verify_credentials" && command.password === "correct horse battery") {
        return { data: { ok: true, subject: "account:7" } };
      }
      return { data: { ok: false, subject: null } };
    };
    const login = (body) => handleApi(new Request(url, {
      method: "POST",
      headers: { origin: url.origin, "content-type": "application/json" },
      body: JSON.stringify(body),
    }), url, requestAdmin, "/admin.sock");

    const account = await login({ email: "admin@example.com", password: "correct horse battery" });
    expect(account.status).toBe(200);
    expect(verifySessionCookie(account.headers.get("set-cookie"))?.sub).toBe("account:7");
    expect(commands).toEqual([{ socket: "/admin.sock", command: { type: "verify_credentials", email: "admin@example.com", password: "correct horse battery" }, actor: undefined }]);

    expect((await login({ email: "admin@example.com", password: "wrong password value" })).status).toBe(401);
    const recovery = await login({ token: "bootstrap" });
    expect(recovery.status).toBe(200);
    expect(verifySessionCookie(recovery.headers.get("set-cookie"))?.sub).toBe(ACTOR_SUBJECT);
  });

  test("uses Secure __Host cookie on HTTPS and plain cookie on remote HTTP", async () => {
    process.env.CYGNUS_CONSOLE_BOOTSTRAP_TOKEN = "bootstrap";
    process.env.CYGNUS_CONSOLE_SESSION_KEY = "session-key";

    const httpsUrl = new URL("https://console.example/api/v1/session");
    const httpsLogin = await handleApi(new Request(httpsUrl, {
      method: "POST",
      headers: { origin: httpsUrl.origin, "content-type": "application/json" },
      body: JSON.stringify({ token: "bootstrap" }),
    }), httpsUrl);
    expect(httpsLogin.status).toBe(200);
    const httpsCookie = httpsLogin.headers.getSetCookie?.() ?? [httpsLogin.headers.get("set-cookie")];
    expect(httpsCookie.some((c) => c?.startsWith(`${SESSION_COOKIE}=`))).toBe(true);
    expect(httpsCookie.some((c) => c?.includes("Secure"))).toBe(true);
    expect(requestIsSecure(new Request(httpsUrl))).toBe(true);

    const httpUrl = new URL("http://207.148.74.199:3000/api/v1/session");
    const httpLogin = await handleApi(new Request(httpUrl, {
      method: "POST",
      headers: { origin: httpUrl.origin, "content-type": "application/json" },
      body: JSON.stringify({ token: "bootstrap" }),
    }), httpUrl);
    expect(httpLogin.status).toBe(200);
    const httpCookie = httpLogin.headers.getSetCookie?.() ?? [httpLogin.headers.get("set-cookie")];
    expect(httpCookie.some((c) => c?.startsWith(`${SESSION_COOKIE_INSECURE}=`))).toBe(true);
    const active = httpCookie.find((c) => c?.startsWith(`${SESSION_COOKIE_INSECURE}=`));
    expect(active).toBeTruthy();
    expect(active.includes("Secure")).toBe(false);
    expect(requestIsSecure(new Request(httpUrl))).toBe(false);

    // Loopback HTTP is treated as secure (browser special-case).
    const loopback = new URL("http://127.0.0.1:3000/api/v1/session");
    expect(requestIsSecure(new Request(loopback))).toBe(true);

    // verifySessionCookie accepts either cookie name prefix in the header.
    const signed = signSession({ sub: "account:9" });
    expect(verifySessionCookie(`${SESSION_COOKIE_INSECURE}=${signed}`)?.sub).toBe("account:9");
    expect(verifySessionCookie(`${SESSION_COOKIE}=${signed}`)?.sub).toBe("account:9");
  });

  test("dashboardTlsCommand forwards ACME contact email", () => {
    expect(dashboardTlsCommand({ mode: "acme", email: "ops@example.com" })).toEqual({
      type: "set_dashboard_tls",
      mode: "acme",
      email: "ops@example.com",
    });
    expect(dashboardTlsCommand({ mode: "self_signed" })).toEqual({
      type: "set_dashboard_tls",
      mode: "self_signed",
    });
  });

  test("sets a signed cookie and bounds repeated failures", async () => {
    process.env.CYGNUS_CONSOLE_BOOTSTRAP_TOKEN = "bootstrap";
    process.env.CYGNUS_CONSOLE_SESSION_KEY = "session-key";
    const url = new URL("http://localhost/api/v1/session");
    const request = (value) => new Request(url, {
      method: "POST",
      headers: { origin: url.origin, "content-type": "application/json" },
      body: JSON.stringify({ token: value }),
    });
    const login = await handleApi(request("bootstrap"), url);
    expect(login.status).toBe(200);
    expect(verifySessionCookie(login.headers.get("set-cookie"))).not.toBeNull();
    for (let attempt = 0; attempt < 5; attempt += 1) {
      expect((await handleApi(request("wrong"), url)).status).toBe(401);
    }
    expect((await handleApi(request("wrong"), url)).status).toBe(429);
  });

});

describe("console first-run setup", () => {
  test("creates the account, applies dashboard settings, and mints an account session", async () => {
    process.env.CYGNUS_CONSOLE_SESSION_KEY = "setup-session-key";
    delete process.env.CYGNUS_CONSOLE_BOOTSTRAP_TOKEN;
    const url = new URL("https://console.example/api/v1/setup");
    const calls = [];
    const requestAdmin = async (socket, command, actor) => {
      calls.push({ socket, command, actor });
      if (command.type === "account_status") return { data: { configured: false } };
      if (command.type === "create_initial_account") return { data: { subject: "account:1" } };
      return { data: { ok: true } };
    };
    const response = await setup(new Request(url, {
      method: "POST",
      headers: { origin: url.origin, "content-type": "application/json" },
      body: JSON.stringify({
        email: "admin@example.com",
        password: "correct horse battery staple",
        dashboard_domain: "dashboard.cygnus.run",
        apex_domain: "cygnus.run",
        ssl: true,
      }),
    }), requestAdmin, "/admin.sock");

    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ ok: true, data: { apex_domain: "cygnus.run", dashboard_domain: "dashboard.cygnus.run" } });
    expect(verifySessionCookie(response.headers.get("set-cookie"))?.sub).toBe("account:1");
    expect(calls).toEqual([
      { socket: "/admin.sock", command: { type: "account_status" }, actor: undefined },
      { socket: "/admin.sock", command: { type: "create_initial_account", email: "admin@example.com", password: "correct horse battery staple" }, actor: undefined },
      { socket: "/admin.sock", command: { type: "set_dashboard_domain", domain: "dashboard.cygnus.run", apex: "cygnus.run" }, actor: "account:1" },
      { socket: "/admin.sock", command: { type: "set_dashboard_tls", mode: "acme", email: "admin@example.com" }, actor: "account:1" },
    ]);
  });

  test("returns conflict when setup already exists or loses the first-run race", async () => {
    process.env.CYGNUS_CONSOLE_SESSION_KEY = "setup-session-key";
    const url = new URL("https://console.example/api/v1/setup");
    const request = () => new Request(url, {
      method: "POST",
      headers: { origin: url.origin, "content-type": "application/json" },
      body: JSON.stringify({ email: "admin@example.com", password: "correct horse battery staple", dashboard_domain: "dashboard.cygnus.run", apex_domain: "cygnus.run", ssl: false }),
    });
    const configured = await setup(request(), async () => ({ data: { configured: true } }), "/admin.sock");
    expect(configured.status).toBe(409);

    let calls = 0;
    const raced = await setup(request(), async (_socket, command) => {
      calls += 1;
      if (command.type === "account_status") return { data: { configured: false } };
      throw new AdminProtocolError("initial account setup has already been completed", "conflict");
    }, "/admin.sock");
    expect(calls).toBe(2);
    expect(raced.status).toBe(409);
  });

  test("reports setupRequired on health and session, including HEAD", async () => {
    process.env.CYGNUS_CONSOLE_SESSION_KEY = "setup-session-key";
    const requestAdmin = async () => ({ data: { configured: false } });
    const health = await healthResponse(new Request("https://console.example/healthz"), requestAdmin, "/admin.sock");
    expect((await health.json()).setupRequired).toBe(true);
    const session = await sessionResponse(new Request("https://console.example/api/v1/session"), false, requestAdmin, "/admin.sock");
    expect((await session.json()).data.setupRequired).toBe(true);
    const head = await sessionResponse(new Request("https://console.example/api/v1/session", { method: "HEAD" }), true, async () => ({ data: { configured: true } }), "/admin.sock");
    expect(head.status).toBe(200);
    expect(await head.text()).toBe("");
  });
});

describe("console request validation", () => {
  test("rejects unknown fields and host paths cannot enter GitHub config", async () => {
    expect(() => mapDomainCommand({ app: "demo", domain: "demo.test", actor: "host" })).toThrow("unsupported fields");
    expect(() => rollbackCommand({ app: "../demo", deployment: "dpl_1", expected_active_artifact: "abc" })).toThrow("app is invalid");
    expect(() => configureRepositoryCommand({
      installation_id: 1,
      repository_id: 2,
      owner: "acme",
      name: "demo",
      branch: "main",
      app: "demo",
      domain: "demo.test",
      engine_version: "bun-1",
      entry: "index.ts",
      artifact_root: "/tmp/artifacts",
    })).toThrow("unsupported fields");
  });

  test("emits strict ConfigureRepository payload without daemon paths", async () => {
    const request = new Request("http://localhost/api/v1/github/repositories", {
      method: "POST",
      body: JSON.stringify({ installation_id: 1, repository_id: 2, owner: "acme", name: "demo", branch: "main", app: "demo", domain: "demo.test", engine_version: "bun-1.2", entry: "src/index.ts" }),
    });
    const command = await commandForRequest(request, new URL(request.url));
    expect(command).toEqual({ type: "configure_repository", repository: { installation_id: 1, repository_id: 2, owner: "acme", name: "demo", branch: "main", app: "demo", domain: "demo.test", engine_version: "bun-1.2", entry: "src/index.ts" } });
    expect(JSON.stringify(command)).not.toContain("artifact_root");
    expect(JSON.stringify(command)).not.toContain("upstream");
  });

  test("routes observability reads with exact payloads and defaults", async () => {
    const command = async (path) => {
      const request = new Request(`http://localhost${path}`);
      return commandForRequest(request, new URL(request.url));
    };

    expect(await command("/api/v1/metrics")).toEqual({ type: "get_metrics" });
    expect(await command("/api/v1/requests")).toEqual({ type: "list_requests", limit: 100 });
    expect(await command("/api/v1/events")).toEqual({ type: "list_events", limit: 100 });
    expect(await command("/api/v1/apps/demo/logs?stream=stdout")).toEqual({
      type: "read_app_log",
      app: "demo",
      stream: "stdout",
      offset: 0,
      limit: 16_384,
    });
    expect(await command("/api/v1/deployments/dpl_1/logs?stream=stderr")).toEqual({
      type: "read_log",
      deployment: "dpl_1",
      stream: "stderr",
      offset: 0,
      limit: 16_384,
    });
  });

  test("accepts observability maximum bounds", async () => {
    const command = async (path) => {
      const request = new Request(`http://localhost${path}`);
      return commandForRequest(request, new URL(request.url));
    };

    expect(await command("/api/v1/requests?limit=500")).toEqual({ type: "list_requests", limit: 500 });
    expect(await command("/api/v1/events?limit=500")).toEqual({ type: "list_events", limit: 500 });
    expect(await command("/api/v1/apps/demo/logs?stream=stderr&offset=9007199254740991&limit=49152")).toEqual({
      type: "read_app_log",
      app: "demo",
      stream: "stderr",
      offset: Number.MAX_SAFE_INTEGER,
      limit: 49_152,
    });
    expect(await command("/api/v1/deployments/dpl_1/logs?stream=stdout&offset=12&limit=49152")).toEqual({
      type: "read_log",
      deployment: "dpl_1",
      stream: "stdout",
      offset: 12,
      limit: 49_152,
    });
  });

  test("rejects invalid observability bounds and streams", async () => {
    const command = (path) => {
      const request = new Request(`http://localhost${path}`);
      return commandForRequest(request, new URL(request.url));
    };

    await expect(command("/api/v1/requests?limit=501")).rejects.toThrow("limit must be an integer between 1 and 500");
    await expect(command("/api/v1/events?limit=0")).rejects.toThrow("limit must be an integer between 1 and 500");
    await expect(command("/api/v1/apps/demo/logs?stream=combined")).rejects.toThrow("stream must be stdout or stderr");
    await expect(command("/api/v1/deployments/dpl_1/logs?stream=stdout&offset=-1")).rejects.toThrow("offset must be an integer");
    await expect(command("/api/v1/deployments/dpl_1/logs?stream=stdout&limit=49153")).rejects.toThrow("limit must be an integer between 1 and 49152");
  });

  test("rejects unsafe observability paths and unknown queries", async () => {
    const command = (path) => {
      const request = new Request(`http://localhost${path}`);
      return commandForRequest(request, new URL(request.url));
    };

    await expect(command("/api/v1/apps/demo%2Fescape/logs?stream=stdout")).rejects.toThrow("app is invalid");
    await expect(command("/api/v1/deployments/dpl_1%5Cescape/logs?stream=stdout")).rejects.toThrow("deployment is invalid");
    await expect(command("/api/v1/metrics?limit=1")).rejects.toThrow("query contains unsupported fields");
    await expect(command("/api/v1/requests?cursor=next")).rejects.toThrow("query contains unsupported fields");
    await expect(command("/api/v1/apps/demo/logs?stream=stdout&cursor=next")).rejects.toThrow("query contains unsupported fields");
  });

  test("maps dashboard and app-domain routes to exact admin commands", async () => {
    const command = async (path, method = "GET", body) => {
      const url = new URL(`https://console.example${path}`);
      const request = new Request(url, {
        method,
        ...(body === undefined ? {} : { headers: { "content-type": "application/json" }, body: JSON.stringify(body) }),
      });
      return commandForRequest(request, url);
    };

    expect(await command("/api/v1/settings/dashboard-domain", "POST", { domain: "dashboard.cygnus.run", apex: "cygnus.run" })).toEqual({ type: "set_dashboard_domain", domain: "dashboard.cygnus.run", apex: "cygnus.run" });
    expect(await command("/api/v1/settings/dashboard-tls", "POST", { mode: "acme" })).toEqual({ type: "set_dashboard_tls", mode: "acme" });
    expect(await command("/api/v1/apps/demo/domains")).toEqual({ type: "list_app_domains", app: "demo" });
    expect(await command("/api/v1/apps/demo/domains", "POST", { host: "www.example.com" })).toEqual({ type: "add_app_domain", app: "demo", host: "www.example.com" });
    expect(await command("/api/v1/apps/demo/domains/www.example.com", "DELETE")).toEqual({ type: "remove_app_domain", app: "demo", host: "www.example.com" });
    expect(await command("/api/v1/apps/demo/domains/www.example.com/tls", "POST", { mode: "self_signed" })).toEqual({ type: "set_app_domain_tls", app: "demo", host: "www.example.com", mode: "self_signed" });
    expect(await command("/api/v1/apps/demo/domains/www%2Eexample%2Ecom/tls", "POST", { mode: "acme" })).toEqual({ type: "set_app_domain_tls", app: "demo", host: "www.example.com", mode: "acme" });
  });

  test("strictly validates domain lifecycle inputs and empty deletes", async () => {
    expect(() => dashboardDomainCommand({ domain: "dashboard.example.com", apex: "example.com", extra: true })).toThrow("unsupported fields");
    expect(() => dashboardTlsCommand({ mode: "auto" })).toThrow("acme or self_signed");
    expect(() => addAppDomainCommand("../demo", { host: "www.example.com" })).toThrow("app is invalid");
    expect(() => removeAppDomainCommand("demo", "https://example.com")).toThrow("domain is invalid");
    expect(() => appDomainTlsCommand("demo", "www.example.com", { mode: "off" })).toThrow("acme or self_signed");

    const url = new URL("https://console.example/api/v1/apps/demo/domains/www.example.com");
    await expect(commandForRequest(new Request(url, { method: "DELETE", body: "x" }), url)).rejects.toThrow("body must be empty");
    const encodedSlash = new URL("https://console.example/api/v1/apps/demo/domains/www%2Fexample.com");
    await expect(commandForRequest(new Request(encodedSlash, { method: "DELETE" }), encodedSlash)).rejects.toThrow("host is invalid");
  });

  test("maps primary-domain, retry-acme, and env var routes to exact admin commands", async () => {
    const command = async (path, method = "GET", body) => {
      const url = new URL(`https://console.example${path}`);
      const request = new Request(url, {
        method,
        ...(body === undefined ? {} : { headers: { "content-type": "application/json" }, body: JSON.stringify(body) }),
      });
      return commandForRequest(request, url);
    };

    expect(await command("/api/v1/apps/demo/domains/www.example.com/primary", "POST")).toEqual({
      type: "set_primary_domain",
      app: "demo",
      host: "www.example.com",
    });
    expect(await command("/api/v1/apps/demo/domains/www.example.com/retry-acme", "POST")).toEqual({
      type: "retry_domain_acme",
      app: "demo",
      host: "www.example.com",
    });
    expect(await command("/api/v1/apps/demo/env")).toEqual({ type: "list_env_vars", app: "demo" });
    expect(await command("/api/v1/apps/demo/env", "POST", { key: "API_KEY", value: "secret" })).toEqual({
      type: "set_env_var",
      app: "demo",
      key: "API_KEY",
      value: "secret",
    });
    expect(await command("/api/v1/apps/demo/env/API_KEY", "DELETE")).toEqual({
      type: "remove_env_var",
      app: "demo",
      key: "API_KEY",
    });
  });

  test("rejects invalid primary-domain, retry-acme, and env var inputs", async () => {
    expect(() => setPrimaryDomainCommand("../demo", "www.example.com")).toThrow("app is invalid");
    expect(() => retryDomainAcmeCommand("demo", "https://example.com")).toThrow("domain is invalid");
    expect(() => setEnvVarCommand("demo", { key: "not valid", value: "x" })).toThrow("env var key is invalid");
    expect(() => setEnvVarCommand("demo", { key: "PATH", value: "x" })).toThrow("reserved by the daemon");
    expect(() => setEnvVarCommand("demo", { key: "OK", value: "x", extra: true })).toThrow("unsupported fields");
    expect(() => removeEnvVarCommand("demo", "1bad")).toThrow("env var key is invalid");
    expect(() => listEnvVarsCommand("../demo")).toThrow("app is invalid");
  });

  test("validates and shapes the password change command", async () => {
    expect(changePasswordCommand({
      email: "admin@example.com",
      current_password: "correct horse battery staple",
      new_password: "another strong password here",
    })).toEqual({
      type: "change_password",
      email: "admin@example.com",
      current_password: "correct horse battery staple",
      new_password: "another strong password here",
    });
    expect(() => changePasswordCommand({
      email: "admin@example.com",
      current_password: "correct horse battery staple",
      new_password: "short",
    })).toThrow("password is invalid");
    expect(() => changePasswordCommand({
      email: "admin@example.com",
      current_password: "correct horse battery staple",
      new_password: "another strong password here",
      extra: true,
    })).toThrow("unsupported fields");

    const url = new URL("https://console.example/api/v1/settings/password");
    const request = new Request(url, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        email: "admin@example.com",
        current_password: "correct horse battery staple",
        new_password: "another strong password here",
      }),
    });
    expect(await commandForRequest(request, url)).toEqual({
      type: "change_password",
      email: "admin@example.com",
      current_password: "correct horse battery staple",
      new_password: "another strong password here",
    });
  });

  test("enforces methods and same origin for authenticated domain mutations", async () => {
    process.env.CYGNUS_CONSOLE_SESSION_KEY = "session-key";
    const getSettings = new URL("https://console.example/api/v1/settings/dashboard-domain");
    const wrongMethod = await handleApi(new Request(getSettings), getSettings, async () => { throw new Error("not called"); }, "/admin.sock");
    expect(wrongMethod.status).toBe(405);
    expect(wrongMethod.headers.get("allow")).toBe("POST");

    const add = new URL("https://console.example/api/v1/apps/demo/domains");
    const crossOrigin = await handleApi(new Request(add, {
      method: "POST",
      headers: { origin: "https://evil.example", cookie: signSession(), "content-type": "application/json" },
      body: JSON.stringify({ host: "www.example.com" }),
    }), add, async () => { throw new Error("not called"); }, "/admin.sock");
    expect(crossOrigin.status).toBe(403);
  });

  test("builds exact public manifest and consumes session-bound state once", async () => {
    process.env.CYGNUS_CONSOLE_BOOTSTRAP_TOKEN = "manifest-bootstrap";
    process.env.CYGNUS_CONSOLE_SESSION_KEY = "manifest-session";
    clearManifestStates();
    const cookie = signSession();
    const url = new URL("http://localhost/api/v1/github/manifest");
    const response = await handleApi(new Request(url, { method: "POST", headers: { origin: url.origin, cookie, "content-type": "application/json" }, body: JSON.stringify({ owner: "acme" }) }), url);
    expect(response.status).toBe(200);
    const result = await response.json();
    expect(result.data.manifest).toEqual(buildGithubManifest("http://localhost"));
    expect(result.data.manifest).toEqual({ name: "Cygnus Tenant Zero", url: "http://localhost", redirect_url: "http://localhost/github/app/manifest/callback", setup_url: "http://localhost/github/app/setup", callback_urls: ["http://localhost/github/app/install/callback"], public: false, hook_attributes: { url: "http://localhost/github/webhook", active: true }, default_permissions: { contents: "read", pull_requests: "read", checks: "write", deployments: "write" }, default_events: ["push", "pull_request"] });
    const state = new URL(result.data.action).searchParams.get("state");
    expect(state).toBeTruthy();
    expect(consumeManifestState(state, cookie)?.owner).toBe("acme");
    expect(consumeManifestState(state, cookie)).toBeNull();
  });

  test("streams an exact 25 MiB webhook through frame-safe chunks", async () => {
    const body = Buffer.alloc(MAX_WEBHOOK_BODY_BYTES, 0x5a);
    const expected = createHash("sha256").update(body).digest("hex");
    const observed = createHash("sha256");
    let chunks = 0;
    let bytes = 0;
    const commands = [];
    const requestAdmin = async (_socket, command, actor) => {
      commands.push(command.type);
      expect(actor).toBe("github:webhook");
      if (command.type === "webhook_begin") {
        expect(command.total_bytes).toBe(body.length);
        return { data: { duplicate: false } };
      }
      if (command.type === "webhook_chunk") {
        const chunk = Buffer.from(command.chunk_base64, "base64");
        expect(chunk.length).toBeLessThanOrEqual(MAX_WEBHOOK_CHUNK_BYTES);
        observed.update(chunk);
        bytes += chunk.length;
        chunks += 1;
        return { data: { received_bytes: bytes } };
      }
      return { data: { delivery_id: "delivery-1", duplicate: false, jobs: 1 } };
    };
    const request = new Request("https://cygnus.apps.test/github/webhook", {
      method: "POST",
      headers: {
        "content-length": String(body.length),
        "x-github-delivery": "delivery-1",
        "x-github-event": "push",
        "x-hub-signature-256": `sha256=${"a".repeat(64)}`,
      },
      body,
    });

    const response = await webhookIngress(request, requestAdmin, "/cygnus/admin/admin.sock");
    expect(response.status).toBe(202);
    expect(bytes).toBe(body.length);
    expect(observed.digest("hex")).toBe(expected);
    expect(chunks).toBe(Math.ceil(body.length / MAX_WEBHOOK_CHUNK_BYTES));
    expect(commands[0]).toBe("webhook_begin");
    expect(commands.at(-1)).toBe("webhook_finish");
  });

  test("rejects malformed webhooks before admin work and short-circuits duplicates", async () => {
    let calls = 0;
    const send = async () => { calls += 1; return { data: { duplicate: true } }; };
    const missingLength = new Request("https://cygnus.apps.test/github/webhook", {
      method: "POST",
      headers: {
        "x-github-delivery": "delivery-2",
        "x-github-event": "push",
        "x-hub-signature-256": `sha256=${"b".repeat(64)}`,
      },
      body: "{}",
    });
    missingLength.headers.delete("content-length");
    expect((await webhookIngress(missingLength, send, "/admin.sock")).status).toBe(411);
    expect(calls).toBe(0);

    const duplicate = new Request("https://cygnus.apps.test/github/webhook", {
      method: "POST",
      headers: {
        "content-length": "2",
        "x-github-delivery": "delivery-2",
        "x-github-event": "push",
        "x-hub-signature-256": `sha256=${"b".repeat(64)}`,
      },
      body: "{}",
    });
    expect((await webhookIngress(duplicate, send, "/admin.sock")).status).toBe(202);
    expect(calls).toBe(1);
  });

  test("rejects non-HTTPS manifest origins and removes the host deploy route", async () => {
    process.env.CYGNUS_CONSOLE_BOOTSTRAP_TOKEN = "bootstrap";
    process.env.CYGNUS_CONSOLE_SESSION_KEY = "session";
    const cookie = signSession();
    const manifestUrl = new URL("http://console.example/api/v1/github/manifest");
    const manifest = await handleApi(new Request(manifestUrl, {
      method: "POST",
      headers: { origin: manifestUrl.origin, cookie, "content-type": "application/json" },
      body: "{}",
    }), manifestUrl);
    expect(manifest.status).toBe(422);

    const deployUrl = new URL("https://cygnus.apps.test/api/v1/deploy");
    const deploy = await handleApi(new Request(deployUrl, {
      method: "POST",
      headers: { origin: deployUrl.origin, cookie, "content-type": "application/json" },
      body: "{}",
    }), deployUrl);
    expect(deploy.status).toBe(404);
  });
  test("maps daemon error codes to safe HTTP statuses", () => {
    expect(statusForDaemonCode("unauthorized")).toBe(401);
    expect(statusForDaemonCode("forbidden")).toBe(403);
    expect(statusForDaemonCode("not_found")).toBe(404);
    expect(statusForDaemonCode("conflict")).toBe(409);
    expect(statusForDaemonCode("validation")).toBe(422);
    expect(statusForDaemonCode("internal")).toBe(503);
  });

  test("rejects cross-origin state changes and unauthenticated reads", async () => {
    const csrf = await handleApi(
      new Request("http://localhost/api/v1/deploy", {
        method: "POST",
        headers: { origin: "https://evil.test", "content-type": "application/json" },
        body: JSON.stringify({}),
      }),
      new URL("http://localhost/api/v1/deploy"),
    );
    expect(csrf.status).toBe(403);

    const read = await handleApi(
      new Request("http://localhost/api/v1/status"),
      new URL("http://localhost/api/v1/status"),
    );
    expect([401, 503]).toContain(read.status);
  });

  test("keeps JSON body limit bounded", () => {
    expect(MAX_JSON_BODY_BYTES).toBeLessThanOrEqual(32 * 1024);
  });
});

describe("console deploy upload bridge", () => {
  test("builds exact begin and finish commands while omitting optional defaults", () => {
    expect(deployUploadBeginCommand({ app: "demo.api_v1", total_bytes: 1 })).toEqual({
      type: "deploy_upload_begin",
      app: "demo.api_v1",
      total_bytes: 1,
    });
    expect(deployUploadBeginCommand({
      app: "demo",
      domain: "demo.test",
      engine_version: "bun-1.2",
      entry: "src/index.ts",
      total_bytes: MAX_DEPLOY_TOTAL_BYTES,
    })).toEqual({
      type: "deploy_upload_begin",
      app: "demo",
      total_bytes: MAX_DEPLOY_TOTAL_BYTES,
      domain: "demo.test",
      engine_version: "bun-1.2",
      entry: "src/index.ts",
    });
    expect(deployUploadFinishCommand({ upload_id: "upl_1" })).toEqual({
      type: "deploy_upload_finish",
      upload_id: "upl_1",
    });
    expect(() => deployUploadBeginCommand({ app: "demo", total_bytes: 1, extra: true })).toThrow("unsupported fields");
    expect(deployUploadBeginCommand({
      app: "demo",
      total_bytes: 1,
      env: { API_KEY: "secret", DEBUG: "true" },
      preview: "pr-42",
    })).toEqual({
      type: "deploy_upload_begin",
      app: "demo",
      total_bytes: 1,
      env: { API_KEY: "secret", DEBUG: "true" },
      preview: "pr-42",
    });
    expect(() => deployUploadBeginCommand({ app: "demo", total_bytes: 1, env: { PATH: "x" } })).toThrow("reserved by the daemon");
    expect(() => deployUploadBeginCommand({ app: "demo", total_bytes: 1, preview: "../escape" })).toThrow("preview slug is invalid");
    expect(() => deployUploadFinishCommand({ upload_id: "upl_1", app: "demo" })).toThrow("unsupported fields");
  });

  test("enforces deploy byte bounds and canonical base64", () => {
    expect(deployUploadBeginCommand({ app: "demo", total_bytes: MAX_DEPLOY_TOTAL_BYTES }).total_bytes).toBe(MAX_DEPLOY_TOTAL_BYTES);
    expect(() => deployUploadBeginCommand({ app: "demo", total_bytes: 0 })).toThrow("total_bytes");
    expect(() => deployUploadBeginCommand({ app: "demo", total_bytes: MAX_DEPLOY_TOTAL_BYTES + 1 })).toThrow("total_bytes");
    expect(() => deployUploadBeginCommand({ app: "demo", total_bytes: 1.5 })).toThrow("total_bytes");

    const maximum = Buffer.alloc(MAX_DEPLOY_CHUNK_BYTES, 0xa5).toString("base64");
    expect(deployUploadChunk({ upload_id: "upl_1", chunk_base64: maximum }).bytes.length).toBe(MAX_DEPLOY_CHUNK_BYTES);
    const oversized = Buffer.alloc(MAX_DEPLOY_CHUNK_BYTES + 1, 0xa5).toString("base64");
    expect(() => deployUploadChunk({ upload_id: "upl_1", chunk_base64: oversized })).toThrow("at most 1 MiB");
    for (const chunk_base64 of ["", "YQ", "YR==", "YQ===", "YQ==\n", "not base64"]) {
      expect(() => deployUploadChunk({ upload_id: "upl_1", chunk_base64 })).toThrow("canonical base64");
    }
  });

  test("accepts a 1 MiB request through the dedicated cap and reframes it to 32 KiB admin commands", async () => {
    process.env.CYGNUS_CONSOLE_BOOTSTRAP_TOKEN = "bootstrap";
    process.env.CYGNUS_CONSOLE_SESSION_KEY = "session";
    const url = new URL("https://cygnus.apps.test/api/v1/deploy/chunk");
    const source = Buffer.alloc(MAX_DEPLOY_CHUNK_BYTES, 0x5a);
    const body = JSON.stringify({ upload_id: "upl_1", chunk_base64: source.toString("base64") });
    expect(Buffer.byteLength(body)).toBeGreaterThan(MAX_JSON_BODY_BYTES);
    expect(Buffer.byteLength(body)).toBeLessThan(MAX_DEPLOY_CHUNK_JSON_BODY_BYTES);
    const observed = [];
    let received = 0;
    const requestAdmin = async (socket, command, actor) => {
      expect(socket).toBe("/admin.sock");
      expect(actor).toBe(ACTOR_SUBJECT);
      expect(command.type).toBe("deploy_upload_chunk");
      expect(command.upload_id).toBe("upl_1");
      const bytes = Buffer.from(command.chunk_base64, "base64");
      expect(bytes.length).toBeLessThanOrEqual(MAX_DEPLOY_ADMIN_CHUNK_BYTES);
      observed.push(bytes);
      received += bytes.length;
      return { data: { received_bytes: received }, requestId: `req-${observed.length}` };
    };
    const response = await deployUploadIngress(new Request(url, {
      method: "POST",
      headers: {
        origin: url.origin,
        cookie: signSession(),
        "content-type": "application/json",
      },
      body,
    }), url, requestAdmin, "/admin.sock");
    expect(response.status).toBe(200);
    const result = await response.json();
    expect(result.data).toEqual({ received_bytes: source.length });
    expect(result.requestId).toBe(`req-${source.length / MAX_DEPLOY_ADMIN_CHUNK_BYTES}`);
    expect(observed).toHaveLength(source.length / MAX_DEPLOY_ADMIN_CHUNK_BYTES);
    expect(Buffer.concat(observed)).toEqual(source);
  });

  test("rejects deploy chunk JSON above its dedicated body cap", async () => {
    process.env.CYGNUS_CONSOLE_BOOTSTRAP_TOKEN = "bootstrap";
    process.env.CYGNUS_CONSOLE_SESSION_KEY = "session";
    const url = new URL("https://cygnus.apps.test/api/v1/deploy/chunk");
    const response = await deployUploadIngress(new Request(url, {
      method: "POST",
      headers: {
        origin: url.origin,
        cookie: signSession(),
        "content-type": "application/json",
        "content-length": String(MAX_DEPLOY_CHUNK_JSON_BODY_BYTES + 1),
      },
      body: "{}",
    }), url, async () => { throw new Error("admin must not be called"); }, "/admin.sock");
    expect(response.status).toBe(413);
  });

  test("requires POST, same origin, and authentication without changing the deploy tombstone", async () => {
    process.env.CYGNUS_CONSOLE_BOOTSTRAP_TOKEN = "bootstrap";
    process.env.CYGNUS_CONSOLE_SESSION_KEY = "session";
    const url = new URL("https://cygnus.apps.test/api/v1/deploy/begin");
    const send = async (request) => deployUploadIngress(request, url, async () => ({ data: { upload_id: "upl_1" }, requestId: "req-1" }), "/admin.sock");

    const wrongMethod = await send(new Request(url, { method: "GET" }));
    expect(wrongMethod.status).toBe(405);
    expect(wrongMethod.headers.get("allow")).toBe("POST");
    const crossOrigin = await send(new Request(url, {
      method: "POST",
      headers: { origin: "https://evil.test", cookie: signSession(), "content-type": "application/json" },
      body: JSON.stringify({ app: "demo", total_bytes: 1 }),
    }));
    expect(crossOrigin.status).toBe(403);
    const unauthenticated = await send(new Request(url, {
      method: "POST",
      headers: { origin: url.origin, "content-type": "application/json" },
      body: JSON.stringify({ app: "demo", total_bytes: 1 }),
    }));
    expect(unauthenticated.status).toBe(401);
    const authenticated = await send(new Request(url, {
      method: "POST",
      headers: { origin: url.origin, cookie: signSession(), "content-type": "application/json" },
      body: JSON.stringify({ app: "demo", total_bytes: 1 }),
    }));
    expect(authenticated.status).toBe(200);

    const tombstoneUrl = new URL("https://cygnus.apps.test/api/v1/deploy");
    const tombstone = await handleApi(new Request(tombstoneUrl, {
      method: "POST",
      headers: { origin: tombstoneUrl.origin, cookie: signSession(), "content-type": "application/json" },
      body: "{}",
    }), tombstoneUrl);
    expect(tombstone.status).toBe(404);
    expect(await tombstone.json()).toEqual({ ok: false, error: { code: "not_found", message: "API route not found" } });
  });
});
