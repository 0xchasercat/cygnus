import { afterEach, describe, expect, test } from "bun:test";
import {
  ACTOR_SUBJECT,
  MAX_JSON_BODY_BYTES,
  commandForRequest,
  constantTimeTokenMatch,
  deployCommand,
  handleApi,
  mapDomainCommand,
  rollbackCommand,
  signSession,
  statusForDaemonCode,
  verifySessionCookie,
} from "./server.js";

const previousBootstrap = process.env.CYGNUS_CONSOLE_BOOTSTRAP_TOKEN;
const previousSessionKey = process.env.CYGNUS_CONSOLE_SESSION_KEY;

afterEach(() => {
  if (previousBootstrap === undefined) delete process.env.CYGNUS_CONSOLE_BOOTSTRAP_TOKEN;
  else process.env.CYGNUS_CONSOLE_BOOTSTRAP_TOKEN = previousBootstrap;
  if (previousSessionKey === undefined) delete process.env.CYGNUS_CONSOLE_SESSION_KEY;
  else process.env.CYGNUS_CONSOLE_SESSION_KEY = previousSessionKey;
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
  test("rejects unknown mutation fields and unsafe paths", () => {
    expect(() => mapDomainCommand({ app: "demo", domain: "demo.test", actor: "host" })).toThrow("unsupported fields");
    expect(() => rollbackCommand({ app: "../demo", deployment: "dpl_1", expected_active_artifact: "abc" })).toThrow("app is invalid");
    expect(() => deployCommand({ request: {
      source_dir: "relative", app: "demo", domain: "demo.test", engine_version: "bun-1",
      entry: "index.ts", artifact_root: "/tmp/artifacts", upstream: "/tmp/demo.sock",
    } })).toThrow("absolute host path");
  });

  test("emits documented typed deploy payload without an actor field", async () => {
    const request = new Request("http://localhost/api/v1/deploy", {
      method: "POST",
      body: JSON.stringify({ request: {
        source_dir: "/tmp/src", app: "demo", domain: "demo.test", engine_version: "bun-1.2",
        entry: "src/index.ts", artifact_root: "/var/lib/cygnus/artifacts", upstream: "/run/cygnus/demo.sock",
      } }),
    });
    const command = await commandForRequest(request, new URL(request.url));
    expect(command).toEqual({ type: "deploy", request: {
      source_dir: "/tmp/src", app: "demo", domain: "demo.test", engine_version: "bun-1.2",
      entry: "src/index.ts", artifact_root: "/var/lib/cygnus/artifacts", upstream: "/run/cygnus/demo.sock",
    } });
    expect(command.actor).toBeUndefined();
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
