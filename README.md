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

Pre-alpha. The single-node request path now runs end to end: SQLite state is
projected into the router and supervisor, the first request cold-boots its cage,
and the front relays HTTP/1.1 over the app's Unix socket. Overlay-rooted apps
receive their daemon-owned socket directory at `/cygnus/io`. TLS, the
deploy/admin control plane, and live crash monitoring are still under construction.

## Layout

```
crates/cygnus-cage        isolation stack: namespaces, cgroups, mounts, network, seccomp
crates/cygnus-init        static PID 1 for signal forwarding and orphan reaping
crates/cygnus-supervisor  cold boot coalescing, backoff, and scale-to-zero policy
crates/cygnus-router      lock-free Host-to-app routing table
crates/cygnus-daemon      SQLite state, runnable front, and UDS relay
crates/cygnus-proxy       io_uring/splice data-path benchmark and primitives
console/                  dashboard concept (the future Tenant 0 app)
docs/spec.md              the technical specification, ground truth for design
```

## Run the request path

The daemon imports one complete JSON node configuration into its SQLite state.
The configured command must bind and accept HTTP on the absolute `upstream`
Unix socket; its environment is explicit rather than inherited.
For an app with `rootfs` configured, the daemon mounts the host `upstream`
parent at `/cygnus/io`; the app binds `/cygnus/io/<upstream filename>` while
readiness and routing continue to use the host path.

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
