# Changelog

All notable changes will be documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

Initial public release candidate. Core platform is functional on Linux
(kernel 5.15+, systemd) and macOS (development mode).

### What is included

- Deploy Bun and Node apps via CLI upload, dashboard upload, or GitHub push
- Integrated ingress with automatic HTTPS (ACME/Let's Encrypt, Cloudflare
  DNS-01 for wildcard certs) or TCP/UDS listener modes for external proxies
- Kernel-sandboxed cages: Linux namespaces, cgroups v2, seccomp-bpf
- Scale-to-zero: idle apps reap after a configurable TTL and revive in tens
  of milliseconds on the next request
- Per-app and per-node memory policy
- Blue-green redeploy and instant rollback against retained sealed artifacts
- Web console (`tenant-0`) for build logs, metrics, domains, environment
  variables, and rollback management
- CLI (`cygnus`) for all operations over the root-only admin socket
- SQLite-backed state; backup is a directory copy
