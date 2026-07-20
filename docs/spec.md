# Cygnus — Technical Specification

## 0. Positioning, Trust Model & Non-Goals

**One-liner:** A single-binary, self-hosted serverless platform that runs unmodified Bun/Node apps in kernel-sandboxed, scale-to-zero cages — full runtime compatibility with sub-100ms revival, on hardware you own.

**The thesis:** V8-isolate platforms bought cold starts by sacrificing Node compatibility; container platforms bought compatibility by sacrificing cold starts and density. Kernel primitives (namespaces, seccomp, cgroups v2) plus a page-cache-shared runtime plus bytecode artifacts recover most of both — at an isolation tier that is *stronger than shared-process isolates in blast radius, weaker than microVMs in kernel attack surface*. Cygnus says this out loud instead of hiding it.

**Primary ICP:** self-hosters running many apps on one box — a Coolify/Dokploy alternative that adds scale-to-zero, per-app isolation, and 100× lighter deploys (no images, no registry, no Docker daemon). **Secondary ICP:** platform-builders hosting high-cardinality, mostly-idle tenants: AI-generated apps, preview-deploys-per-PR, per-customer plugins/functions. This second market is where the density economics are actually differentiating.

**Trust model (Class A cages):** designed for *operator-adjacent* code — your apps, your team's, your AI agents', your contracted customers'. The sandbox is real defense-in-depth against buggy and compromised apps (supply-chain payloads, prompt-injected agent code). It is **not** marketed for anonymous hostile free-tier multitenancy; that requires the microVM cage class (§14 roadmap). The #1 operational security control on a shared-kernel platform is kernel patch cadence — the spec treats this as an operations requirement, not a footnote.

**License:** AGPL-3.0 with a CLA.

## 1. Architecture Overview

```text
[ CLIENT: HTTP/1.1 · HTTP/2 · HTTP/3 (QUIC) ]
              |
              v
[ Cygnus DAEMON — single Rust binary, root ]
  ├─ TLS termination (rustls) + ACME manager
  ├─ H1/H2/H3 front, normalized to H1 upstream
  ├─ Routing: SNI/Host → ArcSwap<HashMap> (lock-free reads)
  ├─ io_uring proxy loop (splice UDS↔TCP; ~zero-copy in kernel)
  ├─ Request logs · metering · per-request timeouts · rate limits
  ├─ Cage supervisor (spawn, health, drain, reap, crash backoff)
  └─ Admin API (root-only UDS) + Tenant-0 bridge (typed commands)
              |
              |  HTTP/1.1 over per-app UDS (pooled, keep-alive)
              v
========== CAGE — per app, warm, reused ==========
|  userns · mntns · pidns · ipcns · utsns · netns  |
|  cgroup v2 (mem/cpu/pids) · seccomp allowlist    |
|                                                   |
|  bun (text shared via page cache, per version)    |
|   ├─ preload shim: listen()/Bun.serve → UDS      |
|   └─ artifact: bundle.js + bundle.jsc (RO mount) |
|                                                   |
|  egress: veth ─ nftables policy ─ host NAT       |
|  DNS: host-side forwarder at gateway IP           |
====================================================
```

**Key properties:**
- The daemon is the only privileged process. Cages hold no certs, no admin capability, no host mounts.
- Ingress is filesystem-scoped (UDS mounted into the cage); egress is network-scoped (veth + policy). The two planes are independent — an app with `network: none` still serves traffic.
- A cage is a *server*, not a function instance: it handles concurrent requests, holds WebSocket/SSE connections, and keeps in-memory state between requests (with documented loss-on-reap semantics). This is the single biggest DX/economics difference vs Lambda-model platforms and should be the marquee claim.
- Everything is one binary: embedded default Bun engine (unpacked to tmpfs/`memfd_create` at boot), embedded Tenant 0 artifact, SQLite state. `scp` + run = a node.

## 2. Build Pipeline & Artifacts

**Builds run server-side, always.** `Cygnus deploy` and the GitOps webhook upload *source* (tarball or git ref); the node builds in a build cage. Locally built `.jsc` is never accepted — Bun's sidecar bytecode loader validates integrity against corruption, not against adversaries, so tenant-supplied bytecode is an attack surface on the JSC decoder. (Local `--bytecode` stays fine for `Cygnus dev`.)

**Artifact layout** (content-addressed, RO-mounted into the cage):
```text
/var/lib/cygnus/apps/{app}/{artifact-hash}/
  bundle.js        # bundled source, @bytecode pragma (CJS format — bytecode requires it)
  bundle.jsc       # JSC bytecode cache
  meta.json        # { bunVersion, sourceHash, builtAt, config }
  assets/          # static files, native addons (.node), if any
```

**Both files ship.** Bun loads `bundle.js`, validates the `.jsc` hash, and uses bytecode when valid. On any mismatch it silently falls back to parsing source — **fail-open**: never downtime, just a slower cold start. Bytecode skips the parse phase; its value scales with bundle size (marginal for 50KB, significant for a 5MB SvelteKit server build).

**Engine versioning** — the part v1 ignored:
- Bytecode is **arch-independent but version-locked** (per Bun's own docs: build on ARM64, run on x64 is supported; a Bun upgrade invalidates every `.jsc` on the node).
- Each artifact pins its `bunVersion`. Engines live at `/var/lib/cygnus/engines/{version}/bun`; the daemon embeds the current default and can fetch others (sha256-pinned, signed manifest).
- Engine upgrades are **rolling rebuilds**: new deploys use the new engine; existing apps are rebuilt lazily (next deploy) or by an explicit `Cygnus rebuild --all`. Old engines are GC'd when unreferenced. Multiple resident engine versions cost one page-cache text copy each (~100MB) — the incentive to converge is economic, not forced.
- Pin glibc builds of Bun; cross-libc (glibc↔musl) bytecode has known decoder crashes upstream.

**Entrypoint: preloaded shim.** No hidden entrypoint file; instead the cage runs `bun --preload /cygnus/shim.js bundle.js`. The shim (JS-level, no binary modification):

1. If the bundle default-exports a `{ fetch, websocket? }` object (the Bun/CF/Deno convention), the shim calls `Bun.serve({ ...app, unix: '/cygnus/io/app.sock' })`.
2. Otherwise it monkey-patches `Bun.serve` and `node:http`/`node:net` `Server.prototype.listen` to redirect any TCP port bind to the UDS. This catches legacy Express/Fastify/`app.listen(3000)` apps unmodified — a compat reach V8-isolate platforms structurally cannot make.
3. Direct native-binding socket binds that bypass JS entirely don't get the UDS — they bind inside the cage's netns and simply never receive ingress traffic (harmless), because ingress only arrives via the UDS.

## 4. The Cage: Isolation Stack

Layered, root-supervisor model.

**Namespaces:** `user` (cage root → unprivileged host UID range per app), `mnt`, `pid` (cage PID 1 is a small init that reaps zombies and forwards signals; glibc-linked, shares the staged hostlib with Bun), `ipc`, `uts`, `net` (veth, §7), `cgroup`.

**Filesystem (mntns):** overlayfs — lowerdir = minimal RO base (CA certs, tzdata, resolv.conf) + engine dir + artifact dir; upperdir = per-cage tmpfs (`size=` capped, `noexec,nosuid,nodev`). Policy: **writable mounts are noexec; RO artifact mount is exec-allowed** — this preserves native addons (`.node` dlopen needs file-backed PROT_EXEC) while ensuring nothing *written at runtime* is executable. `/proc` masked, no `/sys` write, no host paths.

**cgroup v2 defaults (per app, configurable):** `memory.max=256M` / `memory.high=224M` (pressure signal before OOM), `cpu.max=1.0` vCPU, `pids.max=128`, io throttling optional. OOM kills are per-cage, surfaced as events.

**seccomp-bpf:** an allowlist of ~80 syscalls organized by family — file I/O (`openat`/`read`/`write`/`close`/stat family), memory (`mmap`/`mprotect`/`munmap`/`mremap`/`madvise`/`brk`), threads & sync (`clone` arg-filtered to thread-flavor flag sets, `futex`, `membarrier` — JSC requires it for cross-thread JIT invalidation, `rseq`, `sched_yield`), events (`epoll_*`, `eventfd2`, `timerfd_*`, `pipe2`), sockets (`socket` arg-filtered to `AF_UNIX`/`AF_INET`/`AF_INET6`, `connect`, `bind`, `sendmsg`, ...), signals, time, identity, `getrandom`, exit. Arg filters: `mmap`/`mprotect` restricted to `MAP_PRIVATE|MAP_ANONYMOUS` for writable+exec mappings; file-backed `PROT_EXEC` allowed read-only (addons); `ioctl` restricted to a small FIONBIO-class set.

**Explicit deny list (where the security payload is):** `io_uring_*`, `ptrace`, `process_vm_*`, `bpf`, `perf_event_open`, `userfaultfd`, `keyctl`/`add_key`, `mount` family, `pivot_root`, `setns`, `unshare`, `kexec`, `reboot`, `init_module`, `execveat`. `execve` is permitted at boot (the launcher execs bun) but useless afterward: nothing executable exists in the cage but the engine itself, writable mounts are noexec, and `pids.max` bounds spawn loops. `child_process`/`Bun.spawn` are documented-unsupported in v1 (roadmap: opt-in toolbox mount).

**Filter derivation is empirical, not aspirational:** a CI harness runs the Bun conformance suite + a corpus of real apps under the filter with `SECCOMP_RET_KILL` and audit logging; the shipped list is generated from that, reviewed by hand, and re-validated per engine upgrade.

**JIT honesty:** JSC's JIT pool on Linux is effectively RWX anonymous memory — stock JSC does not give you Apple-style W^X here, and the spec does not pretend otherwise. The seccomp arg filters prevent *file-backed* writable-exec and foreign mappings; the RWX JIT region is accepted as the price of native-speed JS, mitigated by everything else in this section. **Per-app paranoia knob:** `jit: false` sets `JSC_useJIT=0` — interpreter-only at ~2–5× CPU cost, eliminating the JIT-spray surface for apps that don't need speed.

**Side channels:** core scheduling (`PR_SCHED_CORE`, per-app cookies) so tenants never share a hyperthread; on dedicated nodes, disabling SMT is the simpler equivalent. Standard kernel mitigations (IBPB/STIBP/KPTI) stay on. Process-per-tenant means no shared-address-space Spectre problem — the platform inherits the same posture as any multi-tenant Linux box, which is materially *stronger* than thousands of tenants in one process.

**Kernel floor:** 5.15+ LTS (io_uring maturity, core scheduling, cgroup v2 everywhere). Patch cadence is an operational requirement: unattended-upgrades for the kernel + live-patch or scheduled reboot windows. The docs must say this plainly.

## 5. Cage Lifecycle & Scheduling

**A cage is a warm, per-app server.** One cage per app in v1 (vertical concurrency; `instances: N` horizontal scaling is roadmap). Bun handles concurrent requests natively — no per-request instance tax, no Lambda-style concurrency billing model.

**States:** `cold` (no process; artifact on disk) → `booting` (namespaces + exec in flight) → `ready` (UDS accepting) → `draining` (deploy/reap in progress) → `cold`.

**Scale-to-zero:** idle TTL default 10 minutes (configurable per app; `min_instances: 1` pins an app always-warm — the right default for a self-hoster's main site, while 200 preview deploys scale to zero behind it).

**Cold boot sequence** (all under the supervisor, budget in §12): create/acquire namespaces + cgroup → mounts → apply seccomp → exec engine with preload shim → shim binds UDS → supervisor observes socket ready → proxy begins. **Request coalescing:** concurrent requests to a cold app park on the same boot future — one boot, not a thundering herd of spawns.

**Warm-up reality (why this lifecycle exists):** JSC tiers (LLInt → Baseline → DFG → FTL) need thousands of invocations to reach native speed. Warm reuse is what makes "native JIT" a real feature instead of marketing. Consequence to document honestly: the *first* requests after a cold boot run interpreter-speed; steady-state is where Cygnus beats isolate platforms on compute-heavy work.

**Deploys are blue-green within the node:** build artifact → spawn new cage → readiness check (UDS accepting; optional `/_health`) → atomic route swap (ArcSwap) → drain old cage (in-flight requests finish, WS connections get configurable grace, then SIGTERM → SIGKILL). Rollback is the same mechanism pointed at the previous artifact — instant, because old artifacts are retained (N=5 default).

**Crash handling:** supervisor restarts with exponential backoff; crash-looping apps get a 503 + status page and an event in the dashboard rather than a spawn storm. OOM kills recorded per app.

**High-density mode (optional, off by default):** namespace pre-warm pools. Namespace churn — especially netns create/teardown, which serializes on global kernel locks — is irrelevant at Coolify scale (dozens of apps) but is the known bottleneck at thousands of boots/minute; SOCK measured exactly this class of cost and pooling is the established fix. The design accommodates it; v1 doesn't build it.

## 6. Data Path: Ingress, TLS, Proxying

**The router stays in the data path.** This is the load-bearing v2 reversal, for three reasons: (1) stock Bun cannot adopt a passed connection FD — `process.binding('pipe_wrap')` is unimplemented and the `Bun.serve({fd})` PR was closed unmerged; (2) HTTP/3 is UDP/QUIC — there is no per-connection FD to pass, ever; (3) an out-of-path router cannot log, meter, rate-limit, or time out requests, and cage self-reporting is untrusted. The v1 design silently traded away the entire platform plane for a benchmark number.

**TLS terminates in the router (rustls).** Private keys and ACME material never enter a cage — in v1, a cage doing direct socket I/O either couldn't do TLS or held the `*.apps-domain` wildcard key, meaning any tenant could impersonate every tenant. Routing key is SNI (TLS) / Host (H1) / `:authority` (H2/H3), looked up in an `ArcSwap<HashMap>` — lock-free on the hot path, atomically swapped on deploys.

**Protocol normalization:** clients speak H1/H2/H3 to the router; cages speak HTTP/1.1 over a per-app UDS with a persistent keep-alive connection pool. WebSocket upgrades proxy through transparently (uWS under `Bun.serve` handles WS-over-UDS fine). SSE and streaming bodies flow unbuffered.

**Proxy mechanics:** io_uring event loop; header phase is parsed (routing, logging, limits — headers are touched anyway), body phase uses `splice()` through pipes between TCP and UDS — payload bytes move kernel-side without crossing into userspace. Target overhead: <0.5ms added p50 latency, single-digit % CPU vs direct at 10Gbps-class throughput. This is "zero-copy where it counts" without the v1 contradictions.

**Limits & timeouts (router-enforced, per app, configurable):** request timeout 60s default; WS/SSE exempt from request timeout, governed by idle timeout (5min default); max body size; max concurrent connections per cage; per-IP rate limits optional. Because the router owns the connection, a hung or crashed cage can't leak client connections — the supervisor kills the cage; the router closes its side and returns 502/504. The v1 `SCM_RIGHTS` cleanup trap (§7.2 of v1) disappears structurally.

**v2 flex (roadmap, explicitly gated):** upstream an FD-adoption API into Bun (`Bun.serve({ acceptFd })` — uWS already has `adoptSocket` internally), then: router completes TLS handshake → pushes session keys into kernel kTLS → passes the FD → cage does plaintext I/O while the kernel encrypts; keys still never enter the cage. TCP-only, keep-alive-per-tenant only, H3 stays router-owned. Do this only if profiling shows the splice proxy is actually a bottleneck — it probably isn't.

## 7. Egress Networking

**Mechanism:** each cage's netns gets a veth pair onto a host bridge (per-node CGNAT range, e.g. `100.64.0.0/16`; deterministic IP per app), NAT/masquerade to the host's egress interface, connection tracking for return traffic. Native C/C++ DB drivers, raw TCP, TLS to origin — all just work, because it's real networking.

**Default policy (`egress: public`), enforced by nftables per cage:**
- **Deny:** RFC1918, link-local (`169.254.0.0/16` — cloud metadata), the host's own addresses, the bridge subnet (no cage↔cage traffic), multicast/broadcast.
- **Allow:** public internet, TCP+UDP, plus the host DNS forwarder at the gateway IP.
- This is SSRF containment by default — a prompt-injected AI app or a compromised dependency cannot reach the metadata service, the daemon, or its neighbors.

**Modes per app:** `none` (no veth — pure-compute; ingress still works since it's UDS-based) · `restricted` (CIDR/port allowlist; domain-level allowlists via the DNS-forwarder-populates-nftables-set pattern — resolved IPs of allowlisted names get time-bounded set entries, the dnsmasq/ipset trick) · `public` (default) · `open` (private ranges too — for apps that talk to LAN services; explicit opt-in).

**DNS:** host-side forwarder bound to the bridge gateway; cages get `resolv.conf` pointing at it. Centralizes caching, enables domain policy and per-app query logging.

**Cage-to-cage communication** is denied by default and offered properly instead: apps address each other by name through the router (`http://api.internal.{domain}` or a shim-provided `Cygnus.fetchService('api')`), which keeps auth, logging, and policy in one place.

**Bandwidth:** optional per-veth `tc` shaping; per-app byte counting via nftables counters feeds metering (§9).

**Why not the elegant alternatives:** a userspace stack (slirp-style) taxes every DB query with latency; an `SCM_RIGHTS` socket-broker (router hands connected FDs to cages on request — capability-style egress, genuinely nice) is blocked on the same Bun FD-adoption gap as ingress and can't serve native drivers that call `connect()` directly. veth+nftables is boring, complete, and preserves the compat promise. The broker idea is retained in §14 as a v2+ option for `restricted` mode.

## 8. Memory, Density & Node Sizing

The page cache shares one copy of the engine *text* (~90–100MB per resident Bun version) across all cages.

**Honest per-cage numbers (to be validated in the PoC, §12):**
- Idle hello-world cage: **~35–60MB RSS** (`--smol` trims JSC heap for small apps — exposed as `memory_profile: small`).
- Typical API/SSR app warm: **80–200MB**.
- Default cap 256MB (`memory.high` 224MB); configurable to whatever the box allows. Per the design brief: 200–300MB per meaningful app is acceptable — the platform optimizes *aggregate* economics via scale-to-zero, not per-cage anorexia.

**Density comes from two honest sources:**
1. **Warm density:** no per-container OS image, no Docker daemon overhead, shared engine text, one shared kernel. A warm Cygnus cage ≈ a bare OS process + private heap.
2. **Scale-to-zero (the real multiplier):** cold tenants cost disk only (~1–20MB artifact). Revival is a page-cache exec, not an image pull. Density is bounded by *concurrent-active* tenants, not registered tenants.

**Sizing guidance (to publish with real benchmarks):**

| Node RAM | Always-warm apps (@ ~100MB avg) | + scale-to-zero tenants |
|---|---|---|
| 8 GB | ~40–60 | hundreds |
| 32 GB | ~200–250 | thousands |
| 128 GB | ~800–1,000 | tens of thousands |

**Rejected: KSM/memory dedup.** Cross-tenant page dedup is a documented COW-timing side channel and costs CPU to scan. A platform with a core-scheduling section does not ship KSM.

**Page-cache hygiene:** engines and hot artifacts are `mlock`'d or periodically touched so cold-start latency doesn't regress when the page cache is pressured by tenant workloads; artifact reads use `posix_fadvise(WILLNEED)` on route registration.

## 9. Observability & Metering

**Request plane:** structured JSON request logs (route, tenant, status, latency, bytes in/out, coldstart flag) to a per-app ring buffer, optional file/shipping sink. Prometheus `/metrics` on the admin interface: per-app RPS, p50/p95/p99, error rates, cold-start counts and durations, proxy overhead.

**Cage plane:** stdout/stderr piped to host-side ring buffers (crash-safe: last N MB survive the cage), streamed live to the dashboard over WebSocket. cgroup sampling: CPU time, memory current/peak, OOM events, pids. nftables counters: egress bytes/connections per app.

**Metering (the billing substrate, even if v1 never bills):** per-app CPU-seconds (cgroup, kernel-accounted — not self-reported), GB-seconds of residency, request counts and bytes from the router, egress bytes from nftables. All trustworthy because none of it is measured inside the tenant's own process.

**Events:** deploys, scale-to-zero/revival, OOM kills, crash loops, cert issuance/renewal, seccomp violations (a violation is a high-signal event — either an attack or a filter gap; both page the operator in dashboard and logs).

## 10. Control Plane: Tenant 0, GitOps, CLI

**Tenant 0 stays** — the dashboard is a full-stack Bun app in a standard Class A cage, dogfooding the platform. v2 hardens the parts v1 hand-waved:

**The privilege bridge is a protocol, not a pipe.** Tenant 0 gets one extra mount: `admin.sock`. Over it: a versioned, **typed command enum** (`DeployApp`, `MapDomain`, `SetEnv`, `Rollback`, `ReadLogs{app, range}`, `GetStats` — closed set, no generic filesystem/exec/`ReadPath` operations), length-prefixed and schema-validated by the daemon, per-command authorization, append-only audit log with actor attribution (which GitHub user clicked what). Threat model: **Tenant 0 is semi-trusted** — it is a JS app with an npm dependency tree, i.e. the platform's most privileged supply-chain surface. A fully compromised Tenant 0 can do what the command set allows (deploy apps, map domains) and nothing else: no cert keys (router holds them), no host filesystem, no arbitrary exec, and full audit visibility. High-risk commands (e.g. `DeleteApp`, engine changes) can require CLI confirmation.

**Break-glass:** `cygnusctl` on the host talks to the daemon over a root-only UDS, bypassing Tenant 0 entirely — deploy, rollback, route, logs, cert ops. When a bad dashboard deploy bricks the dashboard, the operator fixes it over SSH. (Kubernetes taught everyone what happens when the control plane's recovery path runs on the control plane.)

**GitOps flow** (unchanged shape, hardened build): GitHub App via manifest flow → webhook (signature-verified by Tenant 0) → daemon spawns a **build cage**: clone + `bun install` + `bun build --bytecode`. Build cages are the weakest sandbox on the node — they need network and execve — so: egress locked to an allowlist (git host + configured registries, via the DNS→nftables-set mechanism), **lifecycle scripts disabled by default** (Bun's `trustedDependencies` model — postinstall is the #1 supply-chain payload vehicle), ephemeral overlay, tight cgroup, no access to other apps' artifacts or env. MicroVM build class is the first roadmap hardening (§14) — builds are latency-insensitive, so the heavy option is cheap there.

**CLI:** `cygnus dev` (local emulator: same shim, same UDS wiring, `app.localhost` routing) · `cygnus link` · `cygnus deploy` (uploads **source**, streams server-side build logs, prints live URL) · `cygnus logs/status/rollback` (thin wrappers over the same API Tenant 0 uses).

**Domains & TLS:** wildcard `*.{APPS_DOMAIN}` via DNS-01 where the DNS provider is configured, else per-hostname HTTP-01 issued on first route. Custom domains: HTTP-01 with rate-limit-aware retry/backoff. Certs on disk `0700` under the daemon's state dir; hot-loaded into rustls with zero reloads. **State:** SQLite (WAL) at `/var/lib/cygnus/state.db` — apps, artifacts, domains, env (secrets encrypted at rest, XChaCha20-Poly1305, node key in `0600` file; TPM/KMS later), audit log. The routing HashMap is a projection of it, rebuilt on boot.

## 11. Request Lifecycle (End to End)

1. **Accept:** client connects; router terminates TLS (or QUIC). SNI/Host → route lookup in `ArcSwap<HashMap>` (~100ns).
2. **Warm path (common case):** grab a pooled UDS connection to the app's cage → forward request line + headers → `splice()` bodies both directions → log, meter, enforce timeout. Added latency target: <0.5ms p50.
3. **Cold path:** route exists, cage is cold → request parks on the app's boot future (coalesced with any concurrent arrivals) → supervisor boots the cage (§5) → UDS ready → parked requests flush. User-visible cost: one cold-start budget (§12), once per idle→active transition, not per request.
4. **Streaming/WS:** upgrade or stream flows through the same splice path, exempt from request timeout, governed by idle timeout.
5. **Failure:** cage crash mid-request → router returns 502, supervisor restarts with backoff; request timeout → 504 + cage health check; app OOM → per-cage kill, event logged, restart.
6. **Teardown:** idle TTL expiry → drain (finish in-flight, WS grace) → SIGTERM → SIGKILL → namespace/cgroup teardown (or return-to-pool in high-density mode). Cage state is gone; the artifact and route remain; next request re-runs step 3.

## 12. Performance Budgets & PoC Gates

**Cold start decomposition (hello-world, warm page cache, target hardware = modern server cores):**

| Phase | Expected | Notes |
|---|---|---|
| clone + namespaces + cgroup | 1–3 ms | netns is the slow one; ~0.2ms with pooling (off in v1) |
| mounts (overlay + tmpfs) + seccomp | ~1 ms | |
| exec bun + runtime init | 10–35 ms | the dominant term; page-cache resident text |
| bytecode load vs parse | saves 1–50+ ms | scales with bundle size — the bigger the app, the more `.jsc` matters |
| shim binds UDS, ready signal | ~1 ms | |
| **Total, request-arrival → first user-code byte** | **~15–45 ms typical** | |

**Published targets:** p50 ≤ 50ms, p99 ≤ 150ms scale-from-zero for a 1MB bundle. (v1's 15ms becomes the *good case*, not the promise.)

**PoC go/no-go gates — build these four things before building anything else:**
- **G1 — Cold start:** harness that spawns the full cage stack around stock Bun. *Gate: p99 ≤ 150ms. If p99 > 250ms on decent hardware, the architecture premise fails — stop and rethink.*
- **G2 — Proxy overhead:** io_uring+splice UDS↔TCP loop vs direct connection. *Gate: ≤ 0.5ms added p50; ≥ 5 Gbps single-node body throughput; CPU overhead < 10% vs direct.*
- **G3 — Density:** 200 idle hello-world cages on a 16GB box. *Gate: < 60% memory used, cold-revival p99 still within G1 under that load.*
- **G4 — seccomp conformance:** Bun's test suite + 10 real-world apps (Express, SvelteKit SSR, a Postgres client, a native-addon app) run green under the production filter. *Gate: zero filter violations across the corpus.*

## 13. Competitive Positioning (Corrected)

| | CF Workers | AWS Lambda | Fly.io | Unikraft Cloud | Coolify / Dokploy | **Cygnus** |
|---|---|---|---|---|---|---|
| Isolation | V8 isolate + process mitigations | Firecracker microVM | Firecracker microVM | Unikernel microVM | Docker (shared kernel) | **Kernel cage: ns + seccomp + cgroups (shared kernel)** |
| JS engine | V8, **JIT enabled** (v1's "no JIT" claim was wrong — they MPK-tag JIT memory) | Full Node | Full Node | Compat layer | Full Node | **Bun/JSC, full JIT, per-app jitless knob** |
| Node compat | Partial: no native addons, no fs, frozen timers, workerd APIs | Full | Full | Partial | Full | **Full, incl. native addons & legacy `listen()` apps** |
| Long-lived connections (WS/SSE) | Constrained | Poor fit (per-request model) | Yes | Yes | Yes | **Yes — cages are servers** |
| Scale-to-zero | Yes | Yes | Suspend/resume | Yes | **No — always-on containers** | **Yes** |
| Cold start | ~0–10 ms | ~100–500ms (SnapStart helps) | ~300ms–1s | ~ms–20ms | n/a | **target ≤50ms p50 / ≤150ms p99** |
| Self-host the platform | Not really (workerd ≠ platform) | No | No | No | **Yes** | **Yes — one binary** |
| Deploy artifact | Script bundle | Zip/image | Image | Image | Image + compose | **js+jsc pair, ~MBs, no registry** |

**The two attacks to have answers for:**
1. *"Stronger isolation exists at comparable latency"* (Unikraft, SnapStart). Answer: true, and Cygnus doesn't claim otherwise — the trade is **zero code changes, the entire Node ecosystem including native addons, and single-binary self-hosting**, none of which those platforms offer. The microVM cage class (§14) is the convergence path for tenants who need it.
2. *"Coolify already owns self-host"* — for 5 always-on apps, it genuinely does; density and cold start barely matter there. Cygnus's wedge inside that market is per-app isolation Coolify lacks + preview-envs/AI-generated apps by the hundred, where always-on containers are economically absurd.

## 14. Roadmap (Post-v1)

**v1 ships:** everything in §§2–12, single node, Class A cages, Tenant 0 + CLI + GitOps, ACME, scale-to-zero.

**v1.x — hardening & density:**
- MicroVM **build** cages (Cloud Hypervisor or Firecracker, optional dependency — builds are the weakest sandbox and latency-insensitive, so harden them first).
- Namespace pre-warm pools + boot-storm scheduling (the high-density mode, §5).
- Domain-level egress allowlists productized; per-app egress dashboards.
- `instances: N` horizontal scaling within the node; per-app CPU pinning.

**v2 — the performance flex, each gated on evidence of need:**
- Upstream `Bun.serve({ acceptFd })` (uWS `adoptSocket` exists internally) → kTLS handoff data path: handshake in router, kernel-encrypted direct I/O in cage, keys never in cage. TCP-only; H3 stays router-owned.
- `SCM_RIGHTS` egress socket broker as the `restricted`-mode backend (capability-style outbound; depends on the same FD-adoption work).
- Zygote-style pre-initialized engine template processes if cold-start profiling shows runtime init dominating (fork of a multithreaded JSC runtime is genuinely hairy — only with upstream Bun cooperation).

**v2.x — trust tier expansion:**
- MicroVM **tenant** class (`isolation: vm` per app): same artifacts, same routing, Firecracker boundary for hostile-tenant hosting. This is the moment anonymous free-tier multitenancy becomes claimable.

**v3 — multi-node (only if the product earns it):** deliberately boring — stateless routers sharing the SQLite-replicated control DB (litestream/raft), content-addressed artifact sync, DNS or anycast steering. No "global edge" language until this exists.

**Explicitly never:** KSM dedup (side channel), per-request cage lifecycle (kills JIT + keep-alive), trusting tenant-built bytecode, faking browser-only APIs.
