//! The cage: Cygnus's per-app isolation stack.
//!
//! A cage is a warm, reusable sandbox built from kernel primitives: user,
//! mount, pid, ipc, uts, and net namespaces; a cgroup v2 slice; an overlayfs
//! root; and a seccomp allowlist. One cage per app, booted on demand,
//! reaped after an idle TTL. See `docs/spec.md` §4–5.
