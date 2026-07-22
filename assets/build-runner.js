import {
  copyFile,
  lstat,
  mkdir,
  readdir,
  rm,
} from "node:fs/promises";
import { basename, dirname, join } from "node:path";
// Daemon-owned dependency build controller.
//
// This file is staged outside the tenant workspace and is launched with the
// daemon-owned bunfig. Its argument surface is intentionally tiny: the daemon
// may request one workspace-relative entrypoint or the fixed static mode and,
// when preflight says so, the fixed install phase.

// Paths come from the daemon, which knows whether this build runs inside a
// rooted cage (Linux: the fixed /cygnus layout) or as a plain process
// (macOS: host staging paths). The cage layout stays as the fallback.
const TRUSTED_CONFIG = process.env.CYGNUS_BUILD_CONFIG ?? "/cygnus/build.bunfig.toml";
const WORKSPACE = process.env.CYGNUS_BUILD_WORKSPACE ?? "/workspace";
const OUTPUT = process.env.CYGNUS_BUILD_OUTPUT ?? "/cygnus/output/app";
const CACHE = process.env.BUN_INSTALL_CACHE_DIR ?? "/workspace/.cygnus-cache";
const STATIC_BUILD_SCRIPT = process.env.CYGNUS_STATIC_BUILD_SCRIPT ?? "";
const BUILD_DETECTION = process.env.CYGNUS_BUILD_DETECTION ?? "";
const STATIC_SERVER_SOURCE =
  process.env.CYGNUS_STATIC_SERVER_SOURCE ?? "/cygnus/cygnus-static-server.ts";
const REGISTRY = "https://registry.npmjs.org";
const HOME = process.env.HOME ?? "/cygnus/home";
const TMPDIR = process.env.TMPDIR ?? "/cygnus/tmp";
// Framework build scripts commonly shell out to `bun`/`bunx` by name (e.g.
// "bun x vite build"). Put the running engine's own directory on PATH so those
// resolve to the same Bun that drives the build — on rooted Linux this is
// already /usr/local/bin, on rootless macOS it is the host engine dir that
// would otherwise be absent from PATH.
const ENGINE_DIR = dirname(process.execPath);
const BASE_PATH = process.env.PATH ?? "/usr/local/bin:/usr/bin:/bin";
const PATH = BASE_PATH.split(":").includes(ENGINE_DIR)
  ? BASE_PATH
  : `${ENGINE_DIR}:${BASE_PATH}`;
const STATIC_OUTPUT_DIRECTORIES = Object.freeze([
  "dist",
  "build",
  "out",
  ".output/public",
  "public",
]);
const ROOT_COPY_EXCLUSIONS = Object.freeze(new Set([
  "node_modules",
  ".git",
  ".cygnus-cache",
]));

const CONTROL_ENV = Object.freeze({
  HOME,
  TMPDIR,
  PATH,
  BUN_INSTALL_CACHE_DIR: CACHE,
  NPM_CONFIG_REGISTRY: REGISTRY,
  // Forward cage-staged CA paths when the daemon sets them (Linux). On
  // macOS host builds leave these unset so Bun uses the system trust store.
  ...(process.env.SSL_CERT_FILE
    ? { SSL_CERT_FILE: process.env.SSL_CERT_FILE }
    : {}),
  ...(process.env.SSL_CERT_DIR ? { SSL_CERT_DIR: process.env.SSL_CERT_DIR } : {}),
  ...(process.env.NODE_EXTRA_CA_CERTS
    ? { NODE_EXTRA_CA_CERTS: process.env.NODE_EXTRA_CA_CERTS }
    : {}),
});

function fail(message) {
  throw new Error(message);
}

function isSafeEntry(entry) {
  if (typeof entry !== "string" || entry.length === 0 || entry.length > 4096) {
    return false;
  }
  if (entry.includes("\u0000") || entry.includes("\\") || entry.startsWith("/")) {
    return false;
  }
  const parts = entry.split("/");
  return parts.every((part) => part.length > 0 && part !== "." && part !== "..");
}

export function parseRunnerArgs(argv) {
  if (!Array.isArray(argv) || argv.length === 0 || argv.length > 2) {
    fail(
      "runner accepts [entry], [--install, entry], [--install-latest, entry], [--static], [--auto], [--install, --static], [--install-latest, --static], [--install, --auto], or [--install-latest, --auto]",
    );
  }
  const install = argv.length === 2;
  if (install && argv[0] !== "--install" && argv[0] !== "--install-latest") {
    fail(`unknown runner argument ${JSON.stringify(argv[0])}`);
  }
  const frozen = install && argv[0] === "--install";
  const entry = argv[install ? 1 : 0];
  if (entry === "--static") {
    return { install, frozen, static: true };
  }
  if (entry === "--auto") {
    return { install, frozen, auto: true };
  }
  if (typeof entry === "string" && entry.startsWith("--")) {
    fail(`unknown runner argument ${JSON.stringify(entry)}`);
  }
  if (!isSafeEntry(entry)) {
    fail("runner entry must be a safe workspace-relative path");
  }
  return { install, frozen, entry };
}

function phaseLog(phase, message) {
  console.error(`[${phase}] ${message}`);
}

async function ensureDirectories() {
  await mkdir(HOME, { recursive: true });
  await mkdir(TMPDIR, { recursive: true });
  await mkdir(CACHE, { recursive: true });
  await mkdir(OUTPUT, { recursive: true });
}

async function installDependencies(frozen) {
  phaseLog(
    "install",
    frozen
      ? "starting frozen dependency install"
      : "no lockfile committed; resolving dependency versions fresh",
  );
  const args = [
    process.execPath,
    "--no-env-file",
    `--config=${TRUSTED_CONFIG}`,
    "install",
    "--ignore-scripts",
    `--registry=${REGISTRY}`,
    `--cache-dir=${CACHE}`,
  ];
  if (frozen) {
    args.splice(4, 0, "--frozen-lockfile");
  }
  const child = Bun.spawn(
    args,
    {
      cwd: WORKSPACE,
      env: CONTROL_ENV,
      stdio: ["ignore", "inherit", "inherit"],
    },
  );
  const status = await child.exited;
  if (status !== 0) {
    phaseLog("install", `failed with status ${status}`);
    return status;
  }
  phaseLog("install", "completed");
  return 0;
}

const DETERMINISTIC_BUILD_OPTIONS = Object.freeze({
  target: "bun",
  format: "cjs",
  bytecode: true,
  minify: true,
  sourcemap: "none",
  packages: "bundle",
  splitting: false,
  define: { "process.env.NODE_ENV": JSON.stringify("production") },
  env: "disable",
});

async function buildBundle(entry) {
  phaseLog("build", "starting deterministic Bun bundle");
  const result = await Bun.build({
    entrypoints: [`${WORKSPACE}/${entry}`],
    root: WORKSPACE,
    outdir: OUTPUT,
    ...DETERMINISTIC_BUILD_OPTIONS,
  });
  if (!result.success) {
    for (const log of result.logs) console.error("[build]", log);
    phaseLog("build", "failed");
    return 1;
  }
  phaseLog("build", "completed");
  return 0;
}

async function runStaticBuildScript() {
  phaseLog("detect", `static build script configured: ${STATIC_BUILD_SCRIPT}`);
  phaseLog("build", `starting bun run ${STATIC_BUILD_SCRIPT}`);
  const child = Bun.spawn(
    [
      process.execPath,
      "--no-env-file",
      `--config=${TRUSTED_CONFIG}`,
      "run",
      STATIC_BUILD_SCRIPT,
    ],
    {
      cwd: WORKSPACE,
      env: CONTROL_ENV,
      stdio: ["ignore", "inherit", "inherit"],
    },
  );
  const status = await child.exited;
  if (status !== 0) {
    phaseLog("build", `static build script failed with status ${status}`);
    return status;
  }
  phaseLog("build", "static build script completed");
  return 0;
}

async function firstStaticOutputDirectory() {
  for (const relativePath of STATIC_OUTPUT_DIRECTORIES) {
    const candidate = join(WORKSPACE, relativePath);
    try {
      const metadata = await lstat(candidate);
      if (metadata.isSymbolicLink()) {
        fail(`static output ${relativePath} must not be a symlink`);
      }
      if (metadata.isDirectory()) return { path: candidate, relativePath };
    } catch (error) {
      if (error?.code !== "ENOENT") throw error;
    }
  }
  fail(
    `static build completed but no output directory exists (${STATIC_OUTPUT_DIRECTORIES.join(", ")})`,
  );
}

async function copyStaticTree(source, destination, excludeControls) {
  const metadata = await lstat(source);
  if (metadata.isSymbolicLink()) {
    fail(`static output contains a symlink: ${source}`);
  }
  if (metadata.isFile()) {
    await copyFile(source, destination);
    return;
  }
  if (!metadata.isDirectory()) {
    fail(`static output contains a special file: ${source}`);
  }

  await mkdir(destination, { recursive: true });
  const entries = await readdir(source, { withFileTypes: true });
  entries.sort((left, right) => left.name.localeCompare(right.name));
  for (const entry of entries) {
    if (excludeControls && ROOT_COPY_EXCLUSIONS.has(entry.name)) continue;
    await copyStaticTree(
      join(source, entry.name),
      join(destination, entry.name),
      excludeControls,
    );
  }
}

async function buildStaticServer() {
  if (basename(STATIC_SERVER_SOURCE) !== "cygnus-static-server.ts") {
    fail("static server source must use reserved entry cygnus-static-server.ts");
  }
  phaseLog("build", "starting deterministic static server bundle");
  const result = await Bun.build({
    entrypoints: [STATIC_SERVER_SOURCE],
    root: dirname(STATIC_SERVER_SOURCE),
    outdir: OUTPUT,
    ...DETERMINISTIC_BUILD_OPTIONS,
  });
  if (!result.success) {
    for (const log of result.logs) console.error("[build]", log);
    phaseLog("build", "static server bundle failed");
    return 1;
  }
  phaseLog("build", "static server bundle completed");
  return 0;
}

async function buildStatic() {
  const ranBuildScript = STATIC_BUILD_SCRIPT.length > 0;
  if (ranBuildScript) {
    const status = await runStaticBuildScript();
    if (status !== 0) return status;
  } else {
    phaseLog("detect", "no static build script configured; publishing workspace root");
  }

  let source = WORKSPACE;
  let selected = "workspace root";
  if (ranBuildScript) {
    const output = await firstStaticOutputDirectory();
    source = output.path;
    selected = output.relativePath;
  }
  phaseLog("detect", `selected static output: ${selected}`);

  const publicOutput = join(OUTPUT, "public");
  phaseLog("build", `copying static output from ${selected}`);
  await rm(publicOutput, { recursive: true, force: true });
  await copyStaticTree(source, publicOutput, !ranBuildScript);
  phaseLog("build", "static output copy completed");

  return buildStaticServer();
}

async function buildAuto() {
  // Run the build script first, then decide based on output.
  phaseLog("detect", "running build script, then inspecting output");
  const status = await runStaticBuildScript();
  if (status !== 0) return status;

  // 1. Check if static output was produced (Vite, Gatsby, plain HTML, etc.)
  try {
    const output = await firstStaticOutputDirectory();
    phaseLog("detect", `static output found: ${output.relativePath} → static mode`);
    const publicOutput = join(OUTPUT, "public");
    await rm(publicOutput, { recursive: true, force: true });
    await copyStaticTree(output.path, publicOutput, true);
    phaseLog("build", "static output copy completed");
    return buildStaticServer();
  } catch {
    // No standard static output dir — continue to server checks.
  }

  // 2. Check for known framework standalone entrypoints.
  const STANDALONE_ENTRIES = [
    ".next/standalone/server.js",          // Next.js standalone
    ".output/server/index.mjs",            // Nuxt / Nitro
    ".output/server/index.js",             // Nuxt / Nitro (CJS fallback)
    "dist/server/index.mjs",               // SolidStart
    "build/server/index.mjs",              // SvelteKit
    "build/index.js",                      // Remix
  ];
  for (const candidate of STANDALONE_ENTRIES) {
    const fullPath = join(WORKSPACE, candidate);
    try {
      const meta = await lstat(fullPath);
      if (meta.isFile()) {
        phaseLog("detect", `server entry found: ${candidate} → server mode`);
        // Bundle the standalone entry so the daemon can run it with the shim.
        const result = await Bun.build({
          entrypoints: [fullPath],
          root: WORKSPACE,
          outdir: OUTPUT,
          ...DETERMINISTIC_BUILD_OPTIONS,
        });
        if (result.success) {
          phaseLog("build", "standalone entry bundled successfully");
          return 0;
        }
        phaseLog("build", "standalone bundle failed, copying as-is");
        // Fallback: copy the entry file directly.
        const dest = join(OUTPUT, "entry.js");
        await copyFile(fullPath, dest);
        return 0;
      }
    } catch {
      // Not found, try next.
    }
  }

  // 3. Nothing worked — fail with guidance.
  phaseLog(
    "detect",
    "build completed but no static output or server entry found; pass --entry <path>",
  );
  return 1;
}

export async function runRunner(argv) {
  const { install, frozen, static: staticMode, auto: autoMode, entry } = parseRunnerArgs(argv);
  if (BUILD_DETECTION) phaseLog("detect", BUILD_DETECTION);
  await ensureDirectories();
  if (install) {
    const status = await installDependencies(frozen);
    if (status !== 0) return status;
  }
  if (autoMode) return buildAuto();
  return staticMode ? buildStatic() : buildBundle(entry);
}

if (import.meta.main) {
  runRunner(process.argv.slice(2))
    .then((status) => process.exit(status))
    .catch((error) => {
      phaseLog("build", error instanceof Error ? error.message : String(error));
      process.exit(1);
    });
}
