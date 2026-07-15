import { expect, test } from "bun:test";
import { join } from "node:path";

const RUNNER = join(import.meta.dir, "..", "assets", "build-runner.js");
const BUN = process.execPath;

async function run(args) {
  const child = Bun.spawn([BUN, "--no-env-file", RUNNER, ...args], {
    stdout: "pipe",
    stderr: "pipe",
    stdin: "ignore",
  });
  return {
    status: await child.exited,
    stderr: await new Response(child.stderr).text(),
  };
}

test("runner rejects unknown arguments before any phase", async () => {
  const result = await run(["--unknown", "index.ts"]);
  expect(result.status).not.toBe(0);
  expect(result.stderr).toContain("unknown runner argument");
});

test("runner rejects traversal entrypoints", async () => {
  const result = await run(["--install", "../outside.ts"]);
  expect(result.status).not.toBe(0);
  expect(result.stderr).toContain("safe workspace-relative path");
});

test("runner rejects absolute entrypoints", async () => {
  const result = await run(["/tmp/tenant.ts"]);
  expect(result.status).not.toBe(0);
  expect(result.stderr).toContain("safe workspace-relative path");
});

// The integration requires the daemon's Linux overlay paths and network policy.
// CI can opt in when those paths are mounted; ordinary macOS test runs remain
// deterministic and offline.
test.skipIf(process.env.CYGNUS_RUNNER_INTEGRATION !== "1")(
  "runner installs a frozen dependency and emits bytecode",
  async () => {
    const result = await run(["--install", "index.ts"]);
    expect(result.status).toBe(0);
  },
);
