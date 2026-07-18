# Cygnus

**Self-hosted serverless for Bun and Node apps.** One binary. Scale-to-zero.
Sub-100ms revival. No containers, no registry, no YAML.

Cygnus runs unmodified Bun/Node apps in kernel-sandboxed, scale-to-zero
*cages* on hardware you own. V8-isolate platforms bought fast cold starts by
giving up Node compatibility; container platforms kept compatibility but gave
up cold starts and density. Cygnus uses kernel primitives — namespaces,
seccomp, cgroups v2 — plus a page-cache-shared runtime and bytecode artifacts
to recover most of both.

A cage is a warm, per-app **server**, not a function instance: it handles
concurrent requests, holds WebSocket/SSE connections, and keeps in-memory
state between requests. Idle apps scale to zero and cost disk only; revival is
a page-cache exec, not an image pull.

## Install

One command on a Linux host (kernel 5.15+, systemd):

```sh
curl -fsSL https://raw.githubusercontent.com/0xchasercat/cygnus/main/install.sh | sudo bash
```

The installer downloads the latest release, walks you through listeners and
domains, starts the daemon, and prints your console URL and bootstrap token.
Open the console, paste the token, and ship your first app from the dashboard
— upload a folder or connect a GitHub repository for push-to-deploy.

macOS runs the same platform for development, with cages as plain processes:
no namespaces, no cgroups, no seccomp. Your machine, your call. The installer
sets everything up under `~/.cygnus` — no root required.

## Deploy

Three ways in:

- **Dashboard** — upload a folder or connect a Git repository; watch the
  build stream live and the app go active.
- **Git push** — the console sets up a GitHub App for your account; pushes to
  a configured branch build and deploy automatically, PRs get preview
  deployments.
- **CLI** — `cygnus deploy --source-dir . --app my-app` streams the server
  side build to your terminal and prints the live URL.

Builds always run server-side in a locked-down build cage: frozen installs,
lifecycle scripts disabled, egress limited to the package registry. The build
produces a content-addressed artifact — bundled source plus JSC bytecode —
that boots straight from the page cache.

## How it works

```text
[ client HTTP/HTTPS ]
        |
[ cygnus daemon — one Rust binary ]
  ├─ TLS termination (rustls) + ACME
  ├─ Host routing (lock-free)
  ├─ request logs · metrics · limits
  ├─ cage supervisor (boot, drain, reap, backoff)
  └─ admin API (root UDS) + Tenant 0 bridge
        |  HTTP/1.1 over per-app unix sockets
[ cage: userns · mntns · pidns · netns · cgroups · seccomp ]
  └─ bun --preload shim.js bundle.js   (bytecode, RO artifact)
```

- The daemon is the only privileged process; cages hold no certs, no admin
  capability, no host mounts.
- Apps need zero changes: the preload shim redirects `Bun.serve`, `node:http`
  and `app.listen(3000)` onto the cage's unix socket.
- Egress is real networking (veth + nftables) with SSRF containment by
  default: no metadata service, no cage-to-cage traffic, no RFC1918.
- Deploys are blue-green with instant rollback; the dashboard (Tenant 0) is
  itself a caged Cygnus app.
- Everything lives in one SQLite state file; `cygnus` on the host is the
  break-glass path when you break your own dashboard.

## The console

The dashboard streams build logs live, charts request latency and cold-start
anatomy from the daemon's in-memory telemetry, and manages domains, GitHub
repositories, and rollbacks. It is served by the platform itself as app
`tenant-0`.

## CLI

```
cygnus status                 node, engines, certificates
cygnus apps                   registered apps and their cages
cygnus deploy --source-dir .  server-side build, streamed
cygnus logs <deployment>      build output
cygnus rollback <app> <dep>   instant blue-green rollback
```

## Building from source

```sh
cargo build --release            # daemon, CLI, init
cd console && bun install && bun run build   # dashboard
cargo test --workspace           # unit + integration (cage tests need Linux)
```

The workspace builds and tests on macOS; the full isolation stack needs
Linux 5.15+ with cgroups v2.

## Documentation

- [Getting started](docs/getting-started.md)
- [Technical specification](docs/spec.md) — architecture ground truth

## License

[AGPL-3.0](LICENSE)
