# Security Policy

## Supported versions

Cygnus is pre-1.0. Only the latest release receives security fixes. There is
no long-term support branch at this stage.

## Reporting a vulnerability

Report vulnerabilities **privately** via GitHub Security Advisories:

https://github.com/0xchasercat/cygnus/security/advisories/new

Do not open a public issue for security matters. Include a description of
the issue, reproduction steps, and the version you tested against.

Expected acknowledgment: within 72 hours of receipt. We will coordinate
a fix and a disclosure timeline with you. There is no bug bounty program.

## Isolation model

On Linux, each app runs in a kernel-sandboxed cage: Linux namespaces
(user, mount, pid, ipc, uts, net), cgroups v2 (memory, CPU, pids), and a
seccomp-bpf allowlist of approximately 80 syscalls. The daemon is the only
privileged process; cages hold no TLS certificates, no admin capability,
and no host filesystem access. Egress defaults to the public internet only —
no RFC1918 ranges, no cloud metadata service, no cage-to-cage traffic.

This is a shared-kernel design. It provides strong defense-in-depth against
buggy or compromised apps, but it is not equivalent to a microVM boundary.
The primary operational security control on a shared-kernel platform is
kernel patch cadence.

Full technical detail is in [docs/spec.md](docs/spec.md).

On macOS, cages run as plain processes with no namespace, cgroup, or
seccomp enforcement. macOS is supported for local development only.
