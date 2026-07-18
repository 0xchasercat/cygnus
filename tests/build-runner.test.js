import { expect, test } from "bun:test";
import {
  access,
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import http from "node:http";
import { parseRunnerArgs } from "../assets/build-runner.js";

const RUNNER = join(import.meta.dir, "..", "assets", "build-runner.js");
const STATIC_SERVER = join(import.meta.dir, "..", "assets", "cygnus-static-server.ts");
const SHIM = join(import.meta.dir, "..", "assets", "shim.js");
const BUN = process.execPath;

async function run(args, env = {}) {
  const child = Bun.spawn([BUN, "--no-env-file", RUNNER, ...args], {
    env: { ...process.env, ...env },
    stdout: "pipe",
    stderr: "pipe",
    stdin: "ignore",
  });
  return {
    status: await child.exited,
    stderr: await new Response(child.stderr).text(),
  };
}

async function exists(path) {
  try {
    await access(path);
    return true;
  } catch {
    return false;
  }
}

async function unixRequestEventually(socketPath, path) {
  const deadline = Date.now() + 4_000;
  let lastError;
  while (Date.now() < deadline) {
    try {
      return await new Promise((resolve, reject) => {
        const request = http.get({ path, socketPath, timeout: 250 }, (response) => {
          let body = "";
          response.setEncoding("utf8");
          response.on("data", (chunk) => (body += chunk));
          response.on("end", () => resolve({ status: response.statusCode, body }));
        });
        request.on("timeout", () => request.destroy(new Error("request timed out")));
        request.on("error", reject);
      });
    } catch (error) {
      lastError = error;
      await Bun.sleep(25);
    }
  }
  throw new Error(`timed out waiting for ${socketPath}: ${lastError}`);
}

async function staticFixture(buildScript = "") {
  const root = await mkdtemp(join(tmpdir(), "cygnus-build-runner-"));
  const workspace = join(root, "workspace");
  const output = join(root, "output");
  const controls = join(root, "controls");
  const home = join(root, "home");
  const temporary = join(root, "tmp");
  const cache = join(workspace, ".cygnus-cache");
  await Promise.all([
    mkdir(workspace, { recursive: true }),
    mkdir(output, { recursive: true }),
    mkdir(controls, { recursive: true }),
    mkdir(home, { recursive: true }),
    mkdir(temporary, { recursive: true }),
    mkdir(cache, { recursive: true }),
  ]);
  const config = join(controls, "build.bunfig.toml");
  const server = join(controls, "cygnus-static-server.ts");
  await writeFile(config, "");
  await copyFile(STATIC_SERVER, server);
  return {
    root,
    workspace,
    output,
    env: {
      CYGNUS_BUILD_CONFIG: config,
      CYGNUS_BUILD_WORKSPACE: workspace,
      CYGNUS_BUILD_OUTPUT: output,
      CYGNUS_STATIC_BUILD_SCRIPT: buildScript,
      CYGNUS_STATIC_SERVER_SOURCE: server,
      BUN_INSTALL_CACHE_DIR: cache,
      HOME: home,
      TMPDIR: temporary,
    },
  };
}

test("runner parses bundle and static argument forms", () => {
  expect(parseRunnerArgs(["index.ts"])).toEqual({
    install: false,
    frozen: false,
    entry: "index.ts",
  });
  expect(parseRunnerArgs(["--install", "index.ts"])).toEqual({
    install: true,
    frozen: true,
    entry: "index.ts",
  });
  expect(parseRunnerArgs(["--install-latest", "index.ts"])).toEqual({
    install: true,
    frozen: false,
    entry: "index.ts",
  });
  expect(parseRunnerArgs(["--static"])).toEqual({
    install: false,
    frozen: false,
    static: true,
  });
  expect(parseRunnerArgs(["--install", "--static"])).toEqual({
    install: true,
    frozen: true,
    static: true,
  });
  expect(parseRunnerArgs(["--install-latest", "--static"])).toEqual({
    install: true,
    frozen: false,
    static: true,
  });
});

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

test("static mode rejects tenant traversal and unknown arguments", async () => {
  const traversal = await run(["--static", "../outside"]);
  expect(traversal.status).not.toBe(0);
  expect(traversal.stderr).toContain("unknown runner argument");

  const unknown = await run(["--install", "--unknown"]);
  expect(unknown.status).not.toBe(0);
  expect(unknown.stderr).toContain("unknown runner argument");
});

test("plain static mode copies the workspace root and emits server bytecode", async () => {
  const fixture = await staticFixture();
  try {
    await writeFile(join(fixture.workspace, "index.html"), "root index");
    await mkdir(join(fixture.workspace, "assets"));
    await writeFile(join(fixture.workspace, "assets", "app.js"), "app");
    await mkdir(join(fixture.workspace, "public"));
    await writeFile(join(fixture.workspace, "public", "nested.txt"), "nested public");
    await mkdir(join(fixture.workspace, "node_modules"));
    await writeFile(join(fixture.workspace, "node_modules", "secret"), "excluded");
    await mkdir(join(fixture.workspace, ".git"));
    await writeFile(join(fixture.workspace, ".git", "config"), "excluded");
    await writeFile(join(fixture.workspace, ".cygnus-cache", "cached"), "excluded");

    const result = await run(["--static"], fixture.env);
    expect(result.status).toBe(0);
    expect(result.stderr).toContain("[detect] no static build script configured");
    expect(result.stderr).toContain("[detect] selected static output: workspace root");
    expect(result.stderr).toContain("[build] static output copy completed");
    expect(await readFile(join(fixture.output, "public", "index.html"), "utf8")).toBe(
      "root index",
    );
    expect(await readFile(join(fixture.output, "public", "public", "nested.txt"), "utf8")).toBe(
      "nested public",
    );
    expect(await exists(join(fixture.output, "public", "node_modules"))).toBe(false);
    expect(await exists(join(fixture.output, "public", ".git"))).toBe(false);
    expect(await exists(join(fixture.output, "public", ".cygnus-cache"))).toBe(false);
    const generatedServer = join(fixture.output, "cygnus-static-server.js");
    expect(await exists(generatedServer)).toBe(true);
    expect(await exists(join(fixture.output, "cygnus-static-server.js.jsc"))).toBe(true);

    const socket = join(fixture.root, "app.sock");
    const server = Bun.spawn([BUN, "--no-env-file", "--preload", SHIM, generatedServer], {
      env: { ...process.env, CYGNUS_SOCKET: socket },
      stdout: "pipe",
      stderr: "pipe",
    });
    try {
      const response = await unixRequestEventually(socket, "/client/route");
      expect(response.status).toBe(200);
      expect(response.body).toBe("root index");
    } finally {
      server.kill();
      await server.exited;
    }
  } finally {
    await rm(fixture.root, { recursive: true, force: true });
  }
});

test("static build selects the first conventional output directory", async () => {
  const fixture = await staticFixture("make-output.ts");
  try {
    await writeFile(
      join(fixture.workspace, "make-output.ts"),
      `import { mkdir, writeFile } from "node:fs/promises";
for (const directory of ["build", "public", "dist", "out", ".output/public"]) {
  await mkdir(directory, { recursive: true });
  await writeFile(directory + "/selected.txt", directory);
}
console.error("fixture build output");
`,
    );

    const result = await run(["--static"], fixture.env);
    expect(result.status).toBe(0);
    expect(result.stderr).toContain("fixture build output");
    expect(result.stderr).toContain("[detect] selected static output: dist");
    expect(await readFile(join(fixture.output, "public", "selected.txt"), "utf8")).toBe("dist");
    expect(await exists(join(fixture.output, "public", "build"))).toBe(false);
  } finally {
    await rm(fixture.root, { recursive: true, force: true });
  }
});

test("plain static mode rejects symlink traversal", async () => {
  const fixture = await staticFixture();
  try {
    const outside = join(fixture.root, "outside.txt");
    await writeFile(outside, "outside");
    await symlink(outside, join(fixture.workspace, "escape.txt"));
    const result = await run(["--static"], fixture.env);
    expect(result.status).not.toBe(0);
    expect(result.stderr).toContain("static output contains a symlink");
    expect(await exists(join(fixture.output, "public", "escape.txt"))).toBe(false);
  } finally {
    await rm(fixture.root, { recursive: true, force: true });
  }
});

test("static build fails when no conventional output directory exists", async () => {
  const fixture = await staticFixture("noop.ts");
  try {
    await writeFile(join(fixture.workspace, "noop.ts"), 'console.error("no output");\n');
    const result = await run(["--static"], fixture.env);
    expect(result.status).not.toBe(0);
    expect(result.stderr).toContain("[build] static build script completed");
    expect(result.stderr).toContain("no output directory exists");
  } finally {
    await rm(fixture.root, { recursive: true, force: true });
  }
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
