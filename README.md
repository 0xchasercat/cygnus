# Cygnus

A single-binary, self-hosted serverless platform. Cygnus runs unmodified
Bun/Node apps in kernel-sandboxed, scale-to-zero *cages* — full runtime
compatibility with sub-100ms revival, on hardware you own.

V8-isolate platforms bought fast cold starts by giving up Node compatibility.
Container platforms kept compatibility but gave up cold starts and density.
Cygnus uses kernel primitives (namespaces, seccomp, cgroups v2), a
page-cache-shared runtime, and bytecode artifacts to recover most of both.

A cage is a warm, per-app **server**, not a function instance: it handles
concurrent requests, holds WebSocket/SSE connections, and keeps in-memory
state between requests. Idle apps scale to zero and cost disk only; revival
is a page-cache exec, not an image pull.

## Status

Pre-alpha. The single-node source path now runs end to end: the daemon copies a
source directory into owned staging, builds Bun bytecode in a finite offline
cage, seals a read-only content-addressed artifact, atomically activates its
route in SQLite, and cold-boots the rooted app on the first request. Build
publication is quota-backed; logs, manifests, engine hashes, crash recovery,
and scale-to-zero state are daemon-owned. TLS, replacement/rollback, the typed
Tenant 0 bridge, and the public setup/deploy experience are still under
construction; the commands below are control-plane primitives, not the final UX.

## Layout

```
crates/cygnus-cage        isolation stack: namespaces, cgroups, mounts, network, seccomp
crates/cygnus-init        static PID 1 for signal forwarding and orphan reaping
crates/cygnus-supervisor  cold boot, exit reconciliation, backoff, and scale-to-zero
crates/cygnus-router      lock-free Host-to-app routing table
crates/cygnus-daemon      source builds, artifact/SQLite state, runnable front, and UDS relay
crates/cygnus-proxy       io_uring/splice data-path benchmark and primitives
console/                  self-contained Tenant 0 Bun app (offline/read-only bridge mode)
docs/spec.md              the technical specification, ground truth for design
```

## Run the source path

Register a prepared, read-only Bun engine root, then deploy a source directory.
`--cage-executable` is the absolute path *inside* that root; the daemon hashes
the corresponding host file before trusting it. Projects with `dependencies`,
`devDependencies`, or `optionalDependencies` must include a text `bun.lock`.
The build cage runs a frozen, script-free install with egress restricted to the
npm registry, then bundles dependencies into the sealed runtime artifact.

```sh
cargo run -p cygnus-daemon -- --state ./state.db engine register \
  --version 1.3.14 \
  --host-root /opt/cygnus/engines/bun-1.3.14 \
  --cage-executable /usr/local/bin/bun

cargo run -p cygnus-daemon -- --state ./state.db deploy \
  --source-dir ./my-app \
  --app my-app \
  --domain my-app.localhost \
  --engine-version 1.3.14 \
  --artifact-root /var/lib/cygnus/artifacts \
  --upstream /run/cygnus/my-app.sock

cargo run -p cygnus-daemon -- --state ./state.db serve
curl -H 'Host: my-app.localhost' http://127.0.0.1:3000/
```

The lower-level `apply` command remains available for manually provisioned
apps and request-path development. Its command must bind and accept HTTP on
the absolute `upstream` Unix socket; rooted apps receive the socket parent at
`/cygnus/io`.

```json
{
  "listen": "127.0.0.1:3000",
  "apps": [{
    "name": "api",
    "domains": ["api.localhost"],
    "upstream": "/tmp/cygnus/api.sock",
    "command": "/absolute/path/to/server",
    "env": { "CYGNUS_SOCKET": "/tmp/cygnus/api.sock" }
  }]
}
```

```sh
cargo run -p cygnus-daemon -- --state ./state.db apply ./node.json
cargo run -p cygnus-daemon -- --state ./state.db serve
curl -H 'Host: api.localhost' http://127.0.0.1:3000/
```

## Requirements

- Cage isolation is built from Linux kernel primitives; the full sandbox
  needs Linux 5.15+ (cgroups v2, io_uring, core scheduling).
- The same workspace builds, tests, and runs on macOS, with cages as plain
  processes: no namespaces, no cgroups, no seccomp. Your machine, your call.
- Rust (stable) to build.

## License

[AGPL-3.0](LICENSE).
