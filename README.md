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

Pre-alpha. Nothing here is usable yet.

Current work is the proof-of-concept gates from the
[technical spec](docs/spec.md) — cold-start latency, proxy overhead, idle
density, and seccomp conformance — which decide whether the architecture
premise holds before any platform code gets built on top of it.

## Layout

```
crates/cygnus-cage    isolation stack: namespaces, cgroups v2, mounts, seccomp
crates/cygnus-proxy   data path: io_uring proxy loop, UDS upstream, splice
console/              dashboard concept (the future Tenant 0 app)
docs/spec.md          the technical specification, ground truth for design
```

## Requirements

- Production nodes: Linux 5.15+ (cgroups v2, io_uring, core scheduling).
  Cage isolation is built from Linux kernel primitives and exists nowhere
  else.
- Development: the same workspace builds, tests, and runs on macOS. Cages
  degrade to plain processes -- no namespaces, no cgroups, no seccomp -- and
  the tooling labels the mode plainly. Do not serve production traffic from
  a non-Linux host.
- Rust (stable) to build.

## License

[AGPL-3.0](LICENSE).
