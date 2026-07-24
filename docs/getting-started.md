# Getting started

Cygnus is a single-binary, self-hosted serverless platform for Bun and Node
apps. This guide takes you from an empty host to a deployed app.

## 1. Install

### Linux (production)

Requirements: kernel 5.15+ with cgroups v2, systemd, `nft`, and root.

```sh
curl -fsSL https://raw.githubusercontent.com/0xchasercat/cygnus/main/install.sh | sudo bash
```

The installer downloads the latest release, verifies checksums, starts the
daemon under systemd, and prints:

- your **console URL** (`https://cygnus.<apps-domain>`)
- your **recovery token** — save it now, it's shown only this once. You
  won't need it for first login (the setup wizard creates the admin
  account); it's your way back in if you ever lose that password.

Non-interactive installs: pass `--noninteractive` plus flags like
`--apps-domain apps.example.com --https-listen 0.0.0.0:443 --acme-email you@example.com`.

To remove Cygnus entirely — service, binaries, config, state, and runtime
sockets — run `install.sh --uninstall`. Re-running the installer afterward
is a clean reinstall; nothing needs manual deletion first.

### macOS (development)

The same installer works on macOS and sets everything up under `~/.cygnus`
without root. Cages run as plain processes on macOS: no namespaces, no
cgroups, no seccomp. Your machine, your call.

```sh
curl -fsSL https://raw.githubusercontent.com/0xchasercat/cygnus/main/install.sh | bash
```

## 2. DNS

Apps get subdomains of your apps domain. Point a wildcard record at the host:

```
*.apps.example.com  A  <host-ip>
```

For local use the default `apps.localhost` works out of the box — browsers
resolve `*.localhost` to loopback.

## 3. Open the console

Visit the console URL. On first visit the setup wizard walks you through
creating the admin account (email + password) — that's your login going
forward. The recovery token from install isn't needed here; keep it for if
you ever lose the password, and rotate it any time with
`install.sh --rotate-secrets`.

## 4. Ship an app

Any Bun or Node HTTP app works unmodified — `Bun.serve`, Express, Fastify,
`app.listen(3000)`, native addons, WebSockets. Builds run server-side; if the
project has dependencies it needs a committed `bun.lock`.

**From the dashboard:** press Ship → *Upload a folder*, pick your project,
and watch the build stream live. The app is served at `<name>.<apps-domain>`
the moment it goes active.

**From a Git repository:** press Ship → *Connect Git*. The console creates a
private GitHub App for your account (one click), you install it on your
repositories, and map each repository to an app and branch. Pushes deploy
automatically; pull requests get preview deployments.

**From the CLI:** run `cygnus deploy` from inside the project directory and
it infers the app name from the folder, or pass `--app`/`--domain` to
override:

```sh
cygnus deploy --source-dir . --app my-app
```

The build output streams to your terminal and the live URL prints at the end.

### Listener modes and resources

The default `integrated` mode remains the simplest setup: Cygnus owns HTTP
and TLS ingress and routes domains to apps. Advanced installations can select
`tcp` (one stable, persisted port per app) or `uds` (one stable daemon-owned
socket per app for an external reverse proxy). The dashboard keeps its own
TCP listener in every mode.

The same settings are accepted in `node.json`:

```json
{
  "listen": "0.0.0.0:3000",
  "listener": {
    "mode": "integrated",
    "http_listen": "0.0.0.0:80"
  },
  "resources": {
    "node_memory_budget_bytes": 4294967296,
    "app_memory_default_bytes": 536870912
  },
  "edge": {
    "https_listen": "0.0.0.0:443"
  },
  "apps": []
}
```

TCP mode uses `host`, `port_start`, `port_end`, and (when binding a wildcard
address) `advertise_host`. UDS mode uses `socket_dir`, optional
`socket_group`, and an octal `socket_mode` such as `"0660"`. Existing
configuration files with no `listener` or `resources` fields retain the
integrated behavior and current app limits.

Useful node-only CLI updates—none replace the app list—include:

```sh
cygnus listener --mode tcp --advertise-host node.example.com
cygnus dashboard-listen --listen 0.0.0.0:3000
cygnus node-resources --node-memory-budget-bytes 4294967296 \
  --app-memory-default-bytes 536870912
cygnus app-resources my-app --memory-max-bytes 1073741824
cygnus env set my-app API_URL https://api.example.com
```

Environment and per-app resource changes are durable and reported as pending
until the app is redeployed; the dashboard and API expose both desired and
applied configuration revisions.

## 5. Operate

- **Dashboard** — latency charts, cold-start anatomy, live request stream,
  events, build and runtime logs, domains, rollbacks.
- **CLI** — `cygnus status`, `cygnus apps`, `cygnus logs <deployment>`,
  `cygnus rollback` (compare-and-swap on the active artifact). The CLI talks
  to the daemon's root-only admin socket, so it keeps working even if you
  break the dashboard with a bad deploy of the dashboard itself.

## Scale-to-zero, in practice

Idle apps are reaped after their idle TTL (default 10 minutes) and cost disk
only. The next request boots the cage again — typically tens of milliseconds.
Pin an app always-warm with `min_instances: 1` (the dashboard's own app,
`tenant-0`, ships pinned).

## Where things live

| Path | What |
|---|---|
| `/var/lib/cygnus/state.db` | all platform state (SQLite) |
| `/var/lib/cygnus/artifacts` | content-addressed build artifacts |
| `/var/lib/cygnus/logs` | build and app logs |
| `/run/cygnus/admin.sock` | root-only admin socket (break-glass) |
| `/etc/cygnus` | node config and non-secret env |

macOS uses `~/.cygnus/{state,run,etc}` for the same roles.

## Troubleshooting

- **Console unreachable** — `systemctl status cygnus`, then
  `journalctl -u cygnus -n 100`. The daemon logs every request and every
  boot failure with the reason.
- **App 502/503** — the daemon's log line says why the cage failed to boot;
  `cygnus logs <deployment>` shows the build output.
- **Locked out of the console** — sign in with the recovery token from
  install, or regenerate it with `install.sh --rotate-secrets` if you lost
  it too.
- **macOS: sockets never become ready after an accidental `sudo` install** —
  a root copy of the service may still be registered with launchd (it
  survives deleting the plist). Remove it and start clean:
  `sudo launchctl bootout system/com.cygnus.daemon`, then
  `sudo rm -rf ~/.cygnus ~/Library/LaunchAgents/com.cygnus.daemon.plist`,
  then rerun the installer without sudo.
