import { expect, test } from "bun:test";
import {
  access,
  chmod,
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  stat,
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
  expect(parseRunnerArgs(["--auto"])).toEqual({
    install: false,
    frozen: false,
    auto: true,
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
  await writeFile(directory + "/index.html", "<h1>fixture</h1>");
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
    expect(result.stderr).toContain("no output directory with index.html exists");
  } finally {
    await rm(fixture.root, { recursive: true, force: true });
  }
});

test("auto mode serves static output when the start script is a dev server", async () => {
  // CRA-style repository: `start` launches a webpack dev server (never a
  // production contract) while the build script emits build/index.html.
  // The runner must serve the static output instead of packaging the dev
  // server as a Bun runtime app that dies at boot.
  const fixture = await staticFixture("build.ts");
  try {
    await writeFile(
      join(fixture.workspace, "package.json"),
      JSON.stringify({ scripts: { start: "craco start", build: "craco build" } }),
    );
    await writeFile(
      join(fixture.workspace, "build.ts"),
      'import { mkdirSync, writeFileSync } from "node:fs";\n' +
        'mkdirSync("build", { recursive: true });\n' +
        'writeFileSync("build/index.html", "<h1>static ok</h1>");\n',
    );
    const result = await run(["--auto"], fixture.env);
    expect(result.status).toBe(0);
    expect(result.stderr).toContain("start script launches a dev server (craco start)");
    expect(result.stderr).toContain("serving static output build instead");
    expect(await exists(join(fixture.output, "public", "index.html"))).toBe(true);
  } finally {
    await rm(fixture.root, { recursive: true, force: true });
  }
});

test("auto mode serves the assets directory from wrangler config", async () => {
  // Cloudflare Workers project: assets live in a non-conventional directory
  // named only in wrangler.jsonc, alongside a worker script Cygnus cannot
  // run. The assets are the servable site; the log must say what happens to
  // the worker.
  const fixture = await staticFixture("build.ts");
  try {
    await writeFile(
      join(fixture.workspace, "wrangler.jsonc"),
      '{\n  // deployed on the edge\n  "name": "site",\n  "main": "src/worker.ts",\n  "assets": { "directory": "./site-assets" }\n}\n',
    );
    await writeFile(
      join(fixture.workspace, "build.ts"),
      'import { mkdirSync, writeFileSync } from "node:fs";\n' +
        'mkdirSync("site-assets", { recursive: true });\n' +
        'writeFileSync("site-assets/index.html", "<h1>edge assets ok</h1>");\n',
    );
    const result = await run(["--auto"], fixture.env);
    expect(result.status).toBe(0);
    expect(result.stderr).toContain("Cloudflare Workers config found (wrangler.jsonc)");
    expect(result.stderr).toContain("worker script (src/worker.ts) does not run on Cygnus");
    expect(await exists(join(fixture.output, "public", "index.html"))).toBe(true);
  } finally {
    await rm(fixture.root, { recursive: true, force: true });
  }
});

test("auto mode serves wrangler assets when the start script is wrangler dev", async () => {
  // The Greptile-flagged ordering case: start is "wrangler dev" (a dev
  // server) and the assets live in a directory named ONLY in wrangler
  // config — the dev-server branch must consult wrangler config instead of
  // failing before the Workers rescue step is reached.
  const fixture = await staticFixture("build.ts");
  try {
    await writeFile(
      join(fixture.workspace, "package.json"),
      JSON.stringify({ scripts: { start: "wrangler dev" } }),
    );
    await writeFile(
      join(fixture.workspace, "wrangler.toml"),
      'name = "edge-site"\ncompatibility_date = "2026-01-01"\n\n[assets]\ndirectory = "./edge-assets"\n',
    );
    await writeFile(
      join(fixture.workspace, "build.ts"),
      'import { mkdirSync, writeFileSync } from "node:fs";\n' +
        'mkdirSync("edge-assets", { recursive: true });\n' +
        'writeFileSync("edge-assets/index.html", "<h1>edge start ok</h1>");\n',
    );
    const result = await run(["--auto"], fixture.env);
    expect(result.status).toBe(0);
    expect(result.stderr).toContain("start script launches a dev server (wrangler dev)");
    expect(result.stderr).toContain("serving Cloudflare Workers assets from edge-assets");
    expect(await exists(join(fixture.output, "public", "index.html"))).toBe(true);
  } finally {
    await rm(fixture.root, { recursive: true, force: true });
  }
});

test("auto mode explains adapter options for worker-only wrangler projects", async () => {
  const fixture = await staticFixture("noop.ts");
  try {
    await writeFile(
      join(fixture.workspace, "wrangler.toml"),
      'name = "api"\nmain = "src/worker.ts"\ncompatibility_date = "2026-01-01"\n',
    );
    await writeFile(join(fixture.workspace, "noop.ts"), 'console.error("worker build");\n');
    const result = await run(["--auto"], fixture.env);
    expect(result.status).not.toBe(0);
    expect(result.stderr).toContain("targets the Cloudflare Workers runtime (wrangler.toml, main: src/worker.ts)");
    expect(result.stderr).toContain("@sveltejs/adapter-node");
  } finally {
    await rm(fixture.root, { recursive: true, force: true });
  }
});

test("auto mode finds Angular's dist/<project>/browser output", async () => {
  const fixture = await staticFixture("build.ts");
  try {
    await writeFile(
      join(fixture.workspace, "build.ts"),
      'import { mkdirSync, writeFileSync } from "node:fs";\n' +
        'mkdirSync("dist/myapp/browser", { recursive: true });\n' +
        'writeFileSync("dist/myapp/browser/index.html", "<h1>ng ok</h1>");\n',
    );
    const result = await run(["--auto"], fixture.env);
    expect(result.status).toBe(0);
    expect(result.stderr).toContain("static output found: dist/myapp/browser");
    expect(await exists(join(fixture.output, "public", "index.html"))).toBe(true);
  } finally {
    await rm(fixture.root, { recursive: true, force: true });
  }
});

test("auto mode serves prerendered SvelteKit cloudflare output", async () => {
  const fixture = await staticFixture("build.ts");
  try {
    await writeFile(
      join(fixture.workspace, "build.ts"),
      'import { mkdirSync, writeFileSync } from "node:fs";\n' +
        'mkdirSync(".svelte-kit/cloudflare", { recursive: true });\n' +
        'writeFileSync(".svelte-kit/cloudflare/index.html", "<h1>prerendered ok</h1>");\n' +
        'writeFileSync(".svelte-kit/cloudflare/_worker.js", "export default {};");\n',
    );
    const result = await run(["--auto"], fixture.env);
    expect(result.status).toBe(0);
    expect(result.stderr).toContain("static output found: .svelte-kit/cloudflare");
    expect(await exists(join(fixture.output, "public", "index.html"))).toBe(true);
  } finally {
    await rm(fixture.root, { recursive: true, force: true });
  }
});

test("auto mode publishes an SPA fallback document as index.html", async () => {
  // SvelteKit adapter-static's SPA recipe (fallback: "200.html") produces a
  // build/ with assets and 200.html but no index.html. The site is fully
  // servable — the fallback must be promoted, not rejected.
  const fixture = await staticFixture("build.ts");
  try {
    await writeFile(
      join(fixture.workspace, "build.ts"),
      'import { mkdirSync, writeFileSync } from "node:fs";\n' +
        'mkdirSync("build/_app", { recursive: true });\n' +
        'writeFileSync("build/200.html", "<h1>spa shell</h1>");\n' +
        'writeFileSync("build/_app/app.js", "console.log(1);");\n',
    );
    const result = await run(["--auto"], fixture.env);
    expect(result.status).toBe(0);
    expect(result.stderr).toContain("published SPA fallback 200.html as index.html");
    expect(await readFile(join(fixture.output, "public", "index.html"), "utf8")).toBe(
      "<h1>spa shell</h1>",
    );
    expect(await exists(join(fixture.output, "public", "200.html"))).toBe(true);
  } finally {
    await rm(fixture.root, { recursive: true, force: true });
  }
});

test("auto mode failure inventories the workspace and names SvelteKit misconfigs", async () => {
  // adapter-static with prerendering disabled and no fallback writes only
  // assets (_app/, fonts, robots.txt) — unservable anywhere. The failure
  // must show what the build produced and name the fix.
  const fixture = await staticFixture("build.ts");
  try {
    await writeFile(
      join(fixture.workspace, "build.ts"),
      'import { mkdirSync, writeFileSync } from "node:fs";\n' +
        'mkdirSync("build/_app/immutable", { recursive: true });\n' +
        'mkdirSync(".svelte-kit/output", { recursive: true });\n' +
        'writeFileSync("build/robots.txt", "User-agent: *");\n' +
        'writeFileSync("build/_app/version.json", "{}");\n',
    );
    const result = await run(["--auto"], fixture.env);
    expect(result.status).not.toBe(0);
    expect(result.stderr).toContain("workspace after build:");
    expect(result.stderr).toContain("build/ contains: _app/, robots.txt");
    expect(result.stderr).toContain("SvelteKit assets but no HTML pages");
    expect(result.stderr).toContain("export const prerender = true");
  } finally {
    await rm(fixture.root, { recursive: true, force: true });
  }
});

test("auto mode fails clearly for a dev-server start with no static output", async () => {
  const fixture = await staticFixture("noop.ts");
  try {
    await writeFile(
      join(fixture.workspace, "package.json"),
      JSON.stringify({ scripts: { start: "react-scripts start" } }),
    );
    await writeFile(join(fixture.workspace, "noop.ts"), 'console.error("built nothing");\n');
    const result = await run(["--auto"], fixture.env);
    expect(result.status).not.toBe(0);
    expect(result.stderr).toContain("launches a development server (react-scripts start)");
    expect(result.stderr).toContain("add a build script");
  } finally {
    await rm(fixture.root, { recursive: true, force: true });
  }
});

test("auto mode packages and runs a start-only Bun application", async () => {
  const fixture = await staticFixture();
  try {
    await writeFile(
      join(fixture.workspace, "package.json"),
      JSON.stringify({ scripts: { start: "fixture-dependency && bun server.ts" } }),
    );
    await writeFile(
      join(fixture.workspace, "server.ts"),
      `Bun.serve({ port: 3000, fetch() { return new Response("runtime ok"); } });\n`,
    );
    await mkdir(join(fixture.workspace, "node_modules", "fixture-dependency"), { recursive: true });
    await writeFile(
      join(fixture.workspace, "node_modules", "fixture-dependency", "index.js"),
      "#!/usr/bin/env bun\nconsole.log('dependency bin ok');\n",
    );
    await chmod(
      join(fixture.workspace, "node_modules", "fixture-dependency", "index.js"),
      0o755,
    );
    await mkdir(join(fixture.workspace, "node_modules", ".bin"), { recursive: true });
    await symlink(
      "../fixture-dependency/index.js",
      join(fixture.workspace, "node_modules", ".bin", "fixture-dependency"),
    );

    const result = await run(["--auto"], fixture.env);
    expect(result.status).toBe(0);
    expect(result.stderr).toContain("package start script found");
    expect(result.stderr).toContain("runtime application packaged successfully");
    expect(
      await exists(join(fixture.output, "workspace", "node_modules", "fixture-dependency", "index.js")),
    ).toBe(true);
    expect(
      await readFile(join(fixture.output, "workspace", "node_modules", ".bin", "fixture-dependency"), "utf8"),
    ).toContain("exec bun");
    expect(
      (await stat(
        join(fixture.output, "workspace", "node_modules", ".bin", "fixture-dependency"),
      )).mode & 0o111,
    ).not.toBe(0);

    const runtimeShim = join(fixture.output, "cygnus", "shim.js");
    await mkdir(join(fixture.output, "cygnus"), { recursive: true });
    await copyFile(SHIM, runtimeShim);
    const generatedServer = join(fixture.output, "cygnus-static-server.js");
    expect(await exists(generatedServer)).toBe(true);
    expect(await exists(join(fixture.output, "cygnus-static-server.js.jsc"))).toBe(true);

    const socket = join(fixture.root, "runtime.sock");
    const server = Bun.spawn([BUN, "--no-env-file", "--preload", runtimeShim, generatedServer], {
      env: {
        ...process.env,
        CYGNUS_SOCKET: socket,
        CYGNUS_RUNTIME_ARTIFACT_ROOT: fixture.output,
        CYGNUS_RUNTIME_SHIM: runtimeShim,
      },
      stdout: "pipe",
      stderr: "pipe",
    });
    const runtimeStderr = new Response(server.stderr).text();
    try {
      const response = await unixRequestEventually(socket, "/");
      expect(response.status).toBe(200);
      expect(response.body).toBe("runtime ok");
    } catch (error) {
      server.kill();
      await server.exited;
      throw new Error(`${error}\nruntime stderr:\n${await runtimeStderr}`);
    } finally {
      server.kill();
      await server.exited;
    }
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
