import { afterEach, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  ACTOR_SUBJECT,
  MAX_JSON_BODY_BYTES,
  MAX_WEBHOOK_BODY_BYTES,
  MAX_WEBHOOK_CHUNK_BYTES,
  buildGithubManifest,
  clearManifestStates,
  commandForRequest,
  configureRepositoryCommand,
  constantTimeTokenMatch,
  consumeManifestState,
  handleApi,
  mapDomainCommand,
  rollbackCommand,
  signSession,
  statusForDaemonCode,
  verifySessionCookie,
  webhookIngress,
} from "./server.js";

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
