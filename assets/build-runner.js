import { mkdir } from "node:fs/promises";
// Daemon-owned dependency build controller.
//
// This file is staged outside the tenant workspace and is launched with the
// daemon-owned bunfig. Its argument surface is intentionally tiny: the daemon
// may request one workspace-relative entrypoint and, when preflight says so,
// the fixed install phase.

// Paths come from the daemon, which knows whether this build runs inside a
// rooted cage (Linux: the fixed /cygnus layout) or as a plain process
// (macOS: host staging paths). The cage layout stays as the fallback.
const TRUSTED_CONFIG = process.env.CYGNUS_BUILD_CONFIG ?? "/cygnus/build.bunfig.toml";
const WORKSPACE = process.env.CYGNUS_BUILD_WORKSPACE ?? "/workspace";
const OUTPUT = process.env.CYGNUS_BUILD_OUTPUT ?? "/cygnus/output/app";
const CACHE = process.env.BUN_INSTALL_CACHE_DIR ?? "/workspace/.cygnus-cache";
const REGISTRY = "https://registry.npmjs.org";
const HOME = process.env.HOME ?? "/cygnus/home";
const TMPDIR = process.env.TMPDIR ?? "/cygnus/tmp";
const PATH = process.env.PATH ?? "/usr/local/bin:/usr/bin:/bin";

const CONTROL_ENV = Object.freeze({
  HOME,
  TMPDIR,
  PATH,
  BUN_INSTALL_CACHE_DIR: CACHE,
  NPM_CONFIG_REGISTRY: REGISTRY,
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
  if (!Array.isArray(argv) || (argv.length !== 1 && argv.length !== 2)) {
    fail("runner accepts [entry], [--install, entry], or [--install-latest, entry]");
  }
  const install = argv.length === 2;
  if (install && argv[0] !== "--install" && argv[0] !== "--install-latest") {
    fail(`unknown runner argument ${JSON.stringify(argv[0])}`);
  }
  const frozen = install && argv[0] === "--install";
  const entry = argv[install ? 1 : 0];
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

async function buildBundle(entry) {
  phaseLog("build", "starting deterministic Bun bundle");
  const result = await Bun.build({
    entrypoints: [`${WORKSPACE}/${entry}`],
    root: WORKSPACE,
    outdir: OUTPUT,
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
  if (!result.success) {
    for (const log of result.logs) console.error("[build]", log);
    phaseLog("build", "failed");
    return 1;
  }
  phaseLog("build", "completed");
  return 0;
}

export async function runRunner(argv) {
  const { install, frozen, entry } = parseRunnerArgs(argv);
  await ensureDirectories();
  if (install) {
    const status = await installDependencies(frozen);
    if (status !== 0) return status;
  }
  return buildBundle(entry);
}

if (import.meta.main) {
  runRunner(process.argv.slice(2))
    .then((status) => process.exit(status))
    .catch((error) => {
      phaseLog("build", error instanceof Error ? error.message : String(error));
      process.exit(1);
    });
}
