#!/usr/bin/env bash
# Cygnus release installer.
set -Eeuo pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
VERSION_DEFAULT=${CYGNUS_BUN_VERSION:-bundled}
TEST_MODE=0
if [[ ${CYGNUS_INSTALL_TEST_MODE:-0} == 1 || ${CYGNUS_INSTALL_TEST:-0} == 1 || ${CYGNUS_TEST_MODE:-0} == 1 ]]; then
  TEST_MODE=1
fi
OS=$(uname -s)
if (( TEST_MODE )) && [[ -n ${CYGNUS_INSTALL_TEST_UNAME:-} ]]; then OS=$CYGNUS_INSTALL_TEST_UNAME; fi
host_arch() {
  if (( TEST_MODE )) && [[ -n ${CYGNUS_INSTALL_TEST_ARCH:-} ]]; then
    printf '%s\n' "$CYGNUS_INSTALL_TEST_ARCH"
  else
    uname -m
  fi
}
case $OS in
  Linux|Darwin) ;;
  *) printf 'cygnus installer: ERROR: Unsupported OS: %s\n' "$OS" >&2; exit 1 ;;
esac
TEST_ROOT=${CYGNUS_INSTALL_TEST_ROOT:-${CYGNUS_TEST_ROOT:-}}
if (( TEST_MODE )) && [[ -z $TEST_ROOT ]]; then
  TEST_ROOT=${TMPDIR:-/tmp}/cygnus-installer-test-root
fi

bundle_dir=""
prefix="/usr/local/bin"
config_dir="/etc/cygnus"
state_dir="/var/lib/cygnus"
runtime_dir="/run/cygnus"
systemd_dir="/etc/systemd/system"
listen=""
https_listen=""
apps_domain=""
acme_email=""
dns_provider=""
bun_version="$VERSION_DEFAULT"
noninteractive=0
reconfigure=0
rotate_secrets=0
uninstall=0
assume_yes=0
prefix_set=0
config_set=0
state_set=0
runtime_set=0
systemd_set=0
listen_set=0
https_set=0
domain_set=0
email_set=0
dns_set=0
version_set=0

usage() {
  cat <<'EOF'
Usage: install.sh [options]

Install Cygnus. By default, this downloads the latest release from GitHub for your
architecture. The listen address, HTTPS listener, application domain, ACME email,
and DNS provider are not prompted at install time; configure them after install
through the Cygnus dashboard (or pass the matching flags to override here).

Options:
  --bundle-dir DIR       Install from a local bundle instead of downloading
  --prefix DIR           Binary destination (Linux: /usr/local/bin; macOS: ~/.cygnus/bin)
  --config-dir DIR       Configuration/secrets destination (Linux: /etc/cygnus; macOS: ~/.cygnus/etc)
  --state-dir DIR        Durable state/artifacts destination (Linux: /var/lib/cygnus; macOS: ~/.cygnus/state)
  --runtime-dir DIR      Runtime sockets destination (Linux: /run/cygnus; macOS: ~/.cygnus/run)
  --listen ADDR          Management/dashboard HTTP listener (default: 0.0.0.0:3000 Linux, 127.0.0.1:3000 macOS). Application ingress always binds :80.
  --https-listen ADDR    Optional HTTPS listener (default: disabled)
  --apps-domain DOMAIN   Default application domain (default: apps.localhost)
  --acme-email EMAIL     ACME account email (optional unless HTTPS is enabled)
  --dns-provider NAME    DNS provider (default: none)
  --bun-version VERSION  Registered Bun engine version (default: bundled)
  --noninteractive       Reserved for compatibility; the installer never prompts
                         for network/domain values (they are dashboard-managed).
  --reconfigure          Permit replacing changed config/service files
  --rotate-secrets       Generate and atomically install new console secrets
  --uninstall            Remove Cygnus: stop the service, delete binaries,
                         config, state, and runtime sockets. Prompts for
                         confirmation unless --yes is also given.
  --yes                  Skip the --uninstall confirmation prompt
  -h, --help             Show this help

For focused tests only, set CYGNUS_INSTALL_TEST_MODE=1 (and optionally
CYGNUS_INSTALL_TEST_ROOT).  This bypasses host/root checks and must not be used
for a production install.
EOF
}

fail() {
  echo "cygnus installer: ERROR: $*" >&2
  exit 1
}

while (($#)); do
  case $1 in
    --bundle-dir|--bundle) [[ $# -ge 2 ]] || fail "--bundle-dir needs a value"; bundle_dir=$2; shift 2 ;;
    --prefix|--bin-dir) [[ $# -ge 2 ]] || fail "--prefix needs a value"; prefix=$2; prefix_set=1; shift 2 ;;
    --config-dir|--config|--config-path) [[ $# -ge 2 ]] || fail "--config-dir needs a value"; config_dir=$2; config_set=1; shift 2 ;;
    --state-dir|--state|--state-path) [[ $# -ge 2 ]] || fail "--state-dir needs a value"; state_dir=$2; state_set=1; shift 2 ;;
    --runtime-dir|--runtime|--runtime-path) [[ $# -ge 2 ]] || fail "--runtime-dir needs a value"; runtime_dir=$2; runtime_set=1; shift 2 ;;
    --systemd-dir) [[ $# -ge 2 ]] || fail "--systemd-dir needs a value"; systemd_dir=$2; systemd_set=1; shift 2 ;;
    --listen|--http-listen) [[ $# -ge 2 ]] || fail "--listen needs a value"; listen=$2; listen_set=1; shift 2 ;;
    --https-listen|--https-address) [[ $# -ge 2 ]] || fail "--https-listen needs a value"; https_listen=$2; https_set=1; shift 2 ;;
    --apps-domain|--app-domain) [[ $# -ge 2 ]] || fail "--apps-domain needs a value"; apps_domain=$2; domain_set=1; shift 2 ;;
    --acme-email) [[ $# -ge 2 ]] || fail "--acme-email needs a value"; acme_email=$2; email_set=1; shift 2 ;;
    --dns-provider) [[ $# -ge 2 ]] || fail "--dns-provider needs a value"; dns_provider=$2; dns_set=1; shift 2 ;;
    --bun-version) [[ $# -ge 2 ]] || fail "--bun-version needs a value"; bun_version=$2; version_set=1; shift 2 ;;
    --noninteractive|--no-interactive) noninteractive=1; shift ;;
    --reconfigure|--force) reconfigure=1; shift ;;
    --rotate-secrets|--rotate-secret) rotate_secrets=1; shift ;;
    --uninstall) uninstall=1; shift ;;
    --yes|-y) assume_yes=1; shift ;;
    -h|--help) usage; exit 0 ;;
    --) shift; break ;;
    -*) fail "unknown option: $1 (use --help)" ;;
    *) fail "unexpected argument: $1 (use --help)" ;;
  esac
done

if [[ $OS == Darwin ]]; then
  (( prefix_set )) || prefix=$HOME/.cygnus/bin
  (( config_set )) || config_dir=$HOME/.cygnus/etc
  (( state_set )) || state_dir=$HOME/.cygnus/state
  (( runtime_set )) || runtime_dir=$HOME/.cygnus/run
fi

echo "cygnus installer" >&2
if [[ $OS == Darwin ]]; then
  echo "macOS runs cages as plain processes: no namespaces, no cgroups, no seccomp." >&2
  # macOS installs are per-user: everything lives under ~/.cygnus and the
  # daemon runs as a launchd user agent, which root cannot bootstrap into a
  # user session. Refuse root before touching the filesystem so a mistaken
  # sudo leaves nothing behind.
  if (( ! TEST_MODE )) && [[ $EUID -eq 0 ]]; then
    fail "macOS installs run as your user, not root. Rerun without sudo:
  curl -fsSL https://raw.githubusercontent.com/0xchasercat/cygnus/main/install.sh | bash"
  fi
  # A previous run under sudo leaves two kinds of residue a user install
  # cannot fix itself: root-owned files under ~/.cygnus, and a cygnus service
  # half-registered in launchd's system domain that keeps respawning as root
  # (it survives deleting the plist until bootout or reboot). Detect both and
  # say exactly how to recover.
  if (( ! TEST_MODE )); then
    if command -v launchctl >/dev/null 2>&1 && launchctl print system/com.cygnus.daemon >/dev/null 2>&1; then
      fail "a cygnus service from a previous sudo run is still registered as root. Recover with:
  sudo launchctl bootout system/com.cygnus.daemon
  sudo rm -rf \"$HOME/.cygnus\" \"$HOME/Library/LaunchAgents/com.cygnus.daemon.plist\"
then rerun this installer without sudo."
    fi
    foreign_owned=""
    [[ -e $HOME/.cygnus ]] && foreign_owned=$(find "$HOME/.cygnus" ! -user "$(id -un)" -print -quit 2>/dev/null)
    if [[ -z $foreign_owned && -e $HOME/Library/LaunchAgents/com.cygnus.daemon.plist && ! -w $HOME/Library/LaunchAgents/com.cygnus.daemon.plist ]]; then
      foreign_owned=$HOME/Library/LaunchAgents/com.cygnus.daemon.plist
    fi
    if [[ -n $foreign_owned ]]; then
      fail "$foreign_owned is not owned by $(id -un) (a previous sudo run?). Recover with:
  sudo launchctl bootout system/com.cygnus.daemon 2>/dev/null
  sudo rm -rf \"$HOME/.cygnus\" \"$HOME/Library/LaunchAgents/com.cygnus.daemon.plist\"
then rerun this installer without sudo."
    fi
  fi
fi

downloaded_bundle=""
if (( ! uninstall )) && [[ -z $bundle_dir ]]; then
  ARCH=$(host_arch)
  case $OS in
    Linux) OS_LOWER="unknown-linux-gnu" ;;
    Darwin) OS_LOWER="apple-darwin" ;;
    *) fail "Unsupported OS: $OS" ;;
  esac
  case $ARCH in
    x86_64|amd64) ARCH_LOWER="x86_64" ;;
    aarch64|arm64) ARCH_LOWER="aarch64" ;;
    *) fail "Unsupported architecture: $ARCH" ;;
  esac
  TARGET="${ARCH_LOWER}-${OS_LOWER}"

  if (( TEST_MODE )); then
    # In tests, fallback to local build to avoid hitting the network
    bundle_dir="$SCRIPT_DIR/release"
  else
    echo "Download release for $TARGET" >&2
    downloaded_bundle=$(mktemp -d "${TMPDIR:-/tmp}/cygnus-download.XXXXXX")
    TAR_URL="https://github.com/0xchasercat/cygnus/releases/latest/download/cygnus-${TARGET}.tar.gz"

    if command -v curl >/dev/null 2>&1; then
      curl -fL "$TAR_URL" -o "$downloaded_bundle/cygnus.tar.gz" || fail "Failed to download $TAR_URL"
    elif command -v wget >/dev/null 2>&1; then
      wget -qO "$downloaded_bundle/cygnus.tar.gz" "$TAR_URL" || fail "Failed to download $TAR_URL"
    else
      fail "curl or wget is required to download the release"
    fi

    tar -xzf "$downloaded_bundle/cygnus.tar.gz" -C "$downloaded_bundle" || fail "Failed to extract bundle"
    bundle_dir="$downloaded_bundle"
  fi
fi
# A server binds to every interface so the console is reachable over the
# network; a developer's Mac stays on loopback. The console is auth-gated
# either way, and the daemon routes any unmatched host to it, so reaching the
# box by IP lands on the login screen.
if [[ $OS == Linux ]]; then default_listen=0.0.0.0:3000; else default_listen=127.0.0.1:3000; fi

# Network and domain defaults are baked in here; the operator can override any
# of them via the matching flag. Configuration is dashboard-driven post-install,
# so the installer never prompts for these values, even in interactive mode.
[[ -n $listen ]] || listen=$default_listen
[[ -n $apps_domain ]] || apps_domain=apps.localhost
[[ -n $dns_provider ]] || dns_provider=none
# https_listen and acme_email are intentionally left empty when not set on the
# command line; the dashboard owns them after install.

if [[ -n $bundle_dir && $bundle_dir != /* ]]; then
  [[ $bundle_dir != .. && $bundle_dir != ../* && $bundle_dir != */../* && $bundle_dir != */.. ]] || fail "bundle path traversal is not allowed"
  bundle_dir=$PWD/${bundle_dir#./}
fi
# A test root maps only default destinations. Explicit paths are never rewritten.
map_default_path() {
  local value=$1 was_set=$2
  if (( TEST_MODE )) && [[ $OS == Linux && -n $TEST_ROOT ]] && (( ! was_set )) && [[ $value == /* ]]; then
    printf '%s%s' "${TEST_ROOT%/}" "$value"
  else
    printf '%s' "$value"
  fi
}
prefix=$(map_default_path "$prefix" "$prefix_set")
config_dir=$(map_default_path "$config_dir" "$config_set")
state_dir=$(map_default_path "$state_dir" "$state_set")
runtime_dir=$(map_default_path "$runtime_dir" "$runtime_set")
systemd_dir=$(map_default_path "$systemd_dir" "$systemd_set")

is_abs_safe() {
  local p=$1
  [[ $p == /* && $p != *$'\n'* && $p != *$'\r'* && $p != *$'\t'* && $p != *' '* ]]
  [[ $p != */../* && $p != */.. && $p != ../* && $p != .. ]]
}
for path_name in prefix config_dir state_dir runtime_dir systemd_dir; do
  [[ -n ${!path_name} ]] || fail "$path_name is required"
  is_abs_safe "${!path_name}" || fail "$path_name must be an absolute path without whitespace or path traversal: ${!path_name}"
done

if (( uninstall )); then
  if (( ! TEST_MODE )) && [[ $OS == Linux ]]; then
    [[ $(id -u) -eq 0 ]] || fail "root is required to uninstall (or set CYGNUS_INSTALL_TEST_MODE=1 only for tests)"
  fi

  if [[ $OS == Darwin ]]; then
    service_file=$HOME/Library/LaunchAgents/com.cygnus.daemon.plist
    console_root=$HOME/.cygnus/console
    log_dir=$HOME/.cygnus/log
  else
    service_file=$systemd_dir/cygnus.service
  fi

  echo "This removes Cygnus entirely: stops the service, deletes binaries," >&2
  echo "config, state (including the SQLite database and all app artifacts)," >&2
  echo "and runtime sockets. Deployed apps and their data are not recoverable" >&2
  echo "afterward." >&2
  echo >&2
  echo "  binaries   $prefix/cygnus-daemon, cygnus, cygnusctl, bun$([[ $OS == Linux ]] && printf '%s' ', cygnus-init')" >&2
  echo "  service    $service_file" >&2
  echo "  config     $config_dir" >&2
  echo "  state      $state_dir" >&2
  echo "  runtime    $runtime_dir" >&2
  [[ $OS == Darwin ]] && echo "  console    $console_root" >&2
  [[ $OS == Darwin ]] && echo "  logs       $log_dir" >&2
  echo >&2

  if (( ! assume_yes )) && [[ -t 0 ]] && (( ! TEST_MODE )); then
    read -r -p "Type 'yes' to remove Cygnus: " confirmation
    [[ $confirmation == yes ]] || fail "uninstall cancelled"
  elif (( ! assume_yes )) && (( ! TEST_MODE )); then
    fail "uninstall requires --yes when not run from an interactive terminal"
  fi

  echo "Stop Cygnus" >&2
  if [[ $OS == Darwin ]]; then
    launchctl_bin=$(command -v launchctl || true)
    if [[ -n $launchctl_bin ]]; then
      "$launchctl_bin" bootout "gui/$(id -u)/com.cygnus.daemon" >/dev/null 2>&1 || true
    fi
    if (( ! TEST_MODE )); then
      pkill -U "$(id -u)" -f "$prefix/cygnus-daemon" 2>/dev/null || true
      pkill -U "$(id -u)" -f "cygnus-console/server.js" 2>/dev/null || true
    fi
  else
    systemctl_bin=$(command -v systemctl || true)
    if [[ -n $systemctl_bin ]]; then
      "$systemctl_bin" stop cygnus.service >/dev/null 2>&1 || true
      "$systemctl_bin" disable cygnus.service >/dev/null 2>&1 || true
    fi
  fi

  echo "Remove Cygnus files" >&2
  # Shared system/user locations (prefix, systemd/launchd dirs) only lose the
  # specific Cygnus entries; installer-exclusive roots (config/state/runtime,
  # and on macOS the console/log roots) are removed entirely.
  rm -f -- "$prefix/cygnus-daemon" "$prefix/cygnus" "$prefix/cygnusctl" "$prefix/cygnus-init" "$prefix/bun" "$service_file"
  rm -rf -- "$config_dir" "$state_dir" "$runtime_dir"
  if [[ $OS == Darwin ]]; then
    rm -rf -- "$console_root" "$log_dir"
    # The default macOS prefix (~/.cygnus/bin) is installer-exclusive; a
    # custom --prefix may be a shared system directory and must not be
    # removed wholesale.
    if [[ $prefix == "$HOME/.cygnus/bin" ]]; then
      rmdir -- "$prefix" 2>/dev/null || true
      rmdir -- "$HOME/.cygnus" 2>/dev/null || true
    fi
  fi

  echo >&2
  echo "Cygnus is uninstalled." >&2
  exit 0
fi

[[ -n $bundle_dir ]] || fail "--bundle-dir is required"
is_abs_safe "$bundle_dir" || fail "bundle_dir must be an absolute path without whitespace or path traversal: $bundle_dir"
[[ -n $listen ]] || listen=127.0.0.1:3000
[[ -n $apps_domain ]] || apps_domain=apps.localhost
[[ -n $dns_provider ]] || dns_provider=none
[[ $listen =~ ^[A-Za-z0-9.:_-]+$ ]] || fail "invalid --listen address"
[[ -z $https_listen || $https_listen =~ ^[A-Za-z0-9.:_-]+$ ]] || fail "invalid --https-listen address"
[[ $apps_domain =~ ^[A-Za-z0-9.-]+$ ]] || fail "invalid --apps-domain"
[[ $dns_provider =~ ^[A-Za-z0-9._-]+$ ]] || fail "invalid --dns-provider"
[[ $bun_version =~ ^[A-Za-z0-9._+-]+$ ]] || fail "invalid --bun-version"
if [[ -n $acme_email && ! $acme_email =~ ^[^[:space:]@]+@[^[:space:]@]+\.[^[:space:]@]+$ ]]; then
  fail "invalid --acme-email"
fi
[[ -z $acme_email || -n $https_listen ]] || fail "--acme-email requires --https-listen"

if (( ! TEST_MODE )) && [[ $OS == Linux ]]; then
  [[ $(id -u) -eq 0 ]] || fail "root is required (or set CYGNUS_INSTALL_TEST_MODE=1 only for tests)"
fi

check_host() {
  local cmd
  for cmd in nft nsenter ip; do command -v "$cmd" >/dev/null 2>&1 || fail "required host command missing: $cmd"; done
  [[ -r /proc/self/mountinfo ]] || fail "cannot inspect /proc/self/mountinfo"
  grep -q ' - cgroup2 ' /proc/self/mountinfo || fail "cgroup v2 is not mounted"
  [[ -r /sys/fs/cgroup/cgroup.controllers ]] || fail "cgroup v2 controllers are unavailable"
  local controllers
  controllers=$(cat /sys/fs/cgroup/cgroup.controllers)
  for cmd in cpu memory pids; do [[ " $controllers " == *" $cmd "* ]] || fail "cgroup v2 controller missing: $cmd"; done
  [[ -e /proc/sys/net/ipv4/ip_forward ]] || fail "kernel IP forwarding facility is unavailable"
  if [[ -r /proc/sys/user/max_user_namespaces ]]; then
    [[ $(cat /proc/sys/user/max_user_namespaces) =~ ^[1-9][0-9]*$ ]] || fail "user namespaces are disabled"
  fi
  local release major minor
  release=$(uname -r); major=${release%%.*}; minor=${release#*.}; minor=${minor%%.*}
  [[ $major =~ ^[0-9]+$ && $minor =~ ^[0-9]+$ ]] || fail "cannot determine Linux kernel version"
  (( major > 5 || (major == 5 && minor >= 15) )) || fail "Linux 5.15 or newer is required"
}
if (( ! TEST_MODE )) && [[ $OS == Linux ]]; then check_host; fi

# All source checks happen before any destination mkdir/write.  The staging
# directory and diagnostics are outside installation destinations.
stage=$(mktemp -d "${TMPDIR:-/tmp}/cygnus-install.XXXXXX")
diag_file=${TMPDIR:-/tmp}/cygnus-install-$$.log
: >"$diag_file"
chmod 0600 "$diag_file"
exec 3>>"$diag_file"
cleanup() {
  local status=$?
  exec 3>&- || true
  rm -rf -- "$stage"
  [[ -n ${downloaded_bundle:-} ]] && rm -rf -- "$downloaded_bundle"
  if (( status == 0 )); then rm -f -- "$diag_file"; else printf 'cygnus installer: diagnostics retained at %s\n' "$diag_file" >&2; fi
  return "$status"
}
trap cleanup EXIT
log() { printf '%s\n' "$*" | tee -a /dev/fd/3 >&2; }

log "Verify release bundle"
[[ -d $bundle_dir && ! -L $bundle_dir ]] || fail "bundle directory is not a real directory: $bundle_dir"
sums_file=$bundle_dir/SHA256SUMS
[[ -f $sums_file && ! -L $sums_file ]] || fail "bundle SHA256SUMS is missing or not regular"
command -v sha256sum >/dev/null 2>&1 && hash_tool=sha256sum || hash_tool=shasum
command -v "$hash_tool" >/dev/null 2>&1 || fail "sha256 checksum tool is missing"
if [[ $OS == Darwin ]]; then
  required=(cygnus-daemon cygnus bun cygnus-console.tar)
  allowed_bundle_member='cygnus-daemon|cygnus|bun|cygnus-console.tar'
  [[ ! -e $bundle_dir/cygnus-init ]] || fail "unexpected Darwin bundle member: cygnus-init"
else
  required=(cygnus-daemon cygnus cygnus-init bun cygnus-console.tar)
  allowed_bundle_member='cygnus-daemon|cygnus|cygnus-init|bun|cygnus-console.tar'
fi
expected_file=$stage/expected-checksums
: >"$expected_file"
while IFS= read -r sum_line || [[ -n $sum_line ]]; do
  [[ -z $sum_line ]] && continue
  # Checksums are intentionally strict: only a hash and one bundle basename.
  read -r sum name extra <<<"$sum_line"
  [[ -n ${sum:-} && -n ${name:-} && -z ${extra:-} ]] || fail "malformed checksum line"
  if [[ $name == \** ]]; then name=${name#\*}; fi
  [[ $sum =~ ^[[:xdigit:]]{64}$ ]] || fail "invalid checksum in SHA256SUMS"
  [[ $name =~ ^($allowed_bundle_member)$ ]] || fail "unexpected or unsafe checksum path: $name"
  duplicate=0
  while IFS=$'\t' read -r _ existing_name; do
    if [[ $existing_name == "$name" ]]; then duplicate=1; break; fi
  done <"$expected_file"
  (( ! duplicate )) || fail "duplicate checksum entry: $name"
  sum=$(printf '%s' "$sum" | tr '[:upper:]' '[:lower:]')
  printf '%s\t%s\n' "$sum" "$name" >>"$expected_file"
done < "$sums_file"
checksum_file() {
  local file=$1 result
  if [[ $hash_tool == sha256sum ]]; then result=$(sha256sum -- "$file"); else result=$(shasum -a 256 -- "$file"); fi
  printf '%s' "${result%% *}"
}
expected_checksum() {
  local wanted=$1 stored_sum stored_name
  while IFS=$'\t' read -r stored_sum stored_name; do
    if [[ $stored_name == "$wanted" ]]; then printf '%s' "$stored_sum"; return 0; fi
  done <"$expected_file"
  return 1
}
for name in "${required[@]}"; do
  src=$bundle_dir/$name
  expected_sum=$(expected_checksum "$name") || fail "SHA256SUMS has no entry for required binary: $name"
  [[ -f $src && ! -L $src ]] || fail "bundle input is not a regular file: $name"
  if [[ $name != cygnus-console.tar ]]; then [[ -x $src ]] || fail "bundle input is not executable: $name"; fi
  actual=$(checksum_file "$src" | tr '[:upper:]' '[:lower:]')
  [[ $expected_sum == "$actual" ]] || fail "checksum verification failed for $name"
done

console_archive=$bundle_dir/cygnus-console.tar
tar_bin=$(command -v tar || true)
[[ -n $tar_bin ]] || fail "tar is required to validate cygnus-console.tar"

# Validate every member before extraction. Only regular files/directories rooted
# at opt/cygnus-console are supported; links and special files are not allowed.
archive_names=$stage/archive-names
archive_listing=$stage/archive-listing
"$tar_bin" -tf "$console_archive" >"$archive_names" || fail "unable to list cygnus-console.tar"
"$tar_bin" -tvf "$console_archive" >"$archive_listing" || fail "unable to inspect cygnus-console.tar"
while IFS= read -r archive_name || [[ -n $archive_name ]]; do
  [[ -n $archive_name ]] || continue
  [[ $archive_name != /* && $archive_name != *$'\n'* && $archive_name != *$'\r'* && $archive_name != *$'\t'* && $archive_name != *' '* ]] || fail "unsafe console archive path: $archive_name"
  [[ $archive_name != ../* && $archive_name != */../* && $archive_name != */.. && $archive_name != .. && $archive_name != ./* && $archive_name != */./* && $archive_name != */. ]] || fail "console archive path traversal: $archive_name"
  case $archive_name in
    opt|opt/|opt/cygnus-console|opt/cygnus-console/|opt/cygnus-console/*) ;;
    *) fail "unsupported console archive path: $archive_name" ;;
  esac
done <"$archive_names"
while IFS= read -r archive_line || [[ -n $archive_line ]]; do
  [[ -n $archive_line ]] || continue
  case ${archive_line:0:1} in
    -|d) ;;
    *) fail "unsupported console archive entry type" ;;
  esac
done <"$archive_listing"
grep -Fxq 'opt/cygnus-console/server.js' "$archive_names" || fail "console archive is missing opt/cygnus-console/server.js"
grep -Fxq 'opt/cygnus-console/admin-client.js' "$archive_names" || fail "console archive is missing opt/cygnus-console/admin-client.js"
grep -Eq '^opt/cygnus-console/dist(/|$)' "$archive_names" || fail "console archive is missing opt/cygnus-console/dist"

console_stage=$stage/console
mkdir -p "$console_stage"
"$tar_bin" -xf "$console_archive" -C "$console_stage" || fail "unable to extract cygnus-console.tar"
[[ -d $console_stage/opt/cygnus-console && ! -L $console_stage/opt/cygnus-console ]] || fail "console archive did not produce opt/cygnus-console"
[[ -f $console_stage/opt/cygnus-console/server.js && ! -L $console_stage/opt/cygnus-console/server.js ]] || fail "staged console server.js is invalid"
[[ -f $console_stage/opt/cygnus-console/admin-client.js && ! -L $console_stage/opt/cygnus-console/admin-client.js ]] || fail "staged console admin-client.js is invalid"
[[ -d $console_stage/opt/cygnus-console/dist && ! -L $console_stage/opt/cygnus-console/dist ]] || fail "staged console dist is invalid"
while IFS= read -r -d '' extracted_path; do
  [[ -L $extracted_path ]] && fail "staged console contains an unsupported link"
  [[ -d $extracted_path || -f $extracted_path ]] || fail "staged console contains an unsupported entry"
done < <(find "$console_stage" -print0)

config_file=$config_dir/node.json
secrets_env=$config_dir/secrets.env
admin_socket=$runtime_dir/admin.sock
tenant_admin_socket=$runtime_dir/tenant-0/admin.sock
engine_root=$state_dir/engines/bun-$bun_version
console_socket=$runtime_dir/tenant-0/console.sock
if [[ $OS == Darwin ]]; then
  launchd_dir=$HOME/Library/LaunchAgents
  log_dir=$HOME/.cygnus/log
  service_file=$launchd_dir/com.cygnus.daemon.plist
  console_root=$HOME/.cygnus/console
  secret_root=$config_dir/secrets
  secret_bootstrap_file=$secret_root/bootstrap.token
  secret_session_file=$secret_root/session.key
  secret_bootstrap_path=$secret_bootstrap_file
  secret_session_path=$secret_session_file
else
  service_file=$systemd_dir/cygnus.service
  console_root=$state_dir/artifacts/tenant-0
  secret_root=$state_dir/artifacts/tenant-0-secrets
  secret_bootstrap_file=$secret_root/cygnus/secrets/bootstrap.token
  secret_session_file=$secret_root/cygnus/secrets/session.key
  secret_bootstrap_path=/cygnus/secrets/bootstrap.token
  secret_session_path=/cygnus/secrets/session.key
  # The cage overlay rootfs only carries engine + console + secrets, so the
  # dynamic linker and glibc that both `bun` and `cygnus-init` need must be
  # staged as a dedicated lowerdir. Both binaries are glibc-linked against the
  # same loader path (/lib64/ld-linux-*.so.*); hostlib is their shared ABI root.
  hostlib_root=$state_dir/hostlib
  case $(host_arch) in
    x86_64) hostlib_loader=/lib64/ld-linux-x86-64.so.2; hostlib_lib_dir=/lib/x86_64-linux-gnu ;;
    aarch64) hostlib_loader=/lib64/ld-linux-aarch64.so.1; hostlib_lib_dir=/lib/aarch64-linux-gnu ;;
    *) fail "unsupported architecture for host lib staging: $(host_arch)" ;;
  esac
fi

json_safe_string() {
  [[ $1 != *'"'* && $1 != *'\\'* && $1 != *$'\n'* && $1 != *$'\r'* ]] || fail "value cannot be represented safely in generated JSON"
  printf '%s' "$1"
}
json_listen=$(json_safe_string "$listen")
# Preserve each credential independently unless rotation is explicit. Raw files
# are exactly 32 bytes.
for credential_file in "$secret_bootstrap_file" "$secret_session_file"; do
  if [[ -e $credential_file ]]; then
    [[ ! -L $credential_file && -f $credential_file ]] || fail "existing credential is not a regular file: $credential_file"
    if [[ $(wc -c <"$credential_file" | tr -d ' ') != 32 && $rotate_secrets -eq 0 ]]; then
      fail "existing credential is not 32 bytes; use --rotate-secrets: $credential_file"
    fi
  fi
done
if [[ -e $secret_root ]]; then
  [[ -d $secret_root && ! -L $secret_root ]] || fail "existing secret root is not a real directory: $secret_root"
fi
if (( ! rotate_secrets )); then
  if [[ -e $secret_bootstrap_file || -e $secret_session_file || -e $secret_root ]]; then
    [[ -e $secret_bootstrap_file && -e $secret_session_file ]] || fail "console credentials are incomplete; use --rotate-secrets"
    cp -- "$secret_bootstrap_file" "$stage/bootstrap.token"
    cp -- "$secret_session_file" "$stage/session.key"
  else
    dd if=/dev/urandom of="$stage/bootstrap.token" bs=32 count=1 2>/dev/null || fail "unable to generate bootstrap token"
    dd if=/dev/urandom of="$stage/session.key" bs=32 count=1 2>/dev/null || fail "unable to generate session key"
  fi
else
  dd if=/dev/urandom of="$stage/bootstrap.token" bs=32 count=1 2>/dev/null || fail "unable to generate bootstrap token"
  dd if=/dev/urandom of="$stage/session.key" bs=32 count=1 2>/dev/null || fail "unable to generate session key"
fi
bootstrap_hex=$(od -An -N32 -tx1 "$stage/bootstrap.token" | tr -d ' \n')
session_hex=$(od -An -N32 -tx1 "$stage/session.key" | tr -d ' \n')
[[ $bootstrap_hex =~ ^[[:xdigit:]]{64}$ && $session_hex =~ ^[[:xdigit:]]{64}$ ]] || fail "unable to generate 32-byte console credentials"
secret_stage=$stage/secrets-root
if [[ $OS == Darwin ]]; then
  mkdir -p "$secret_stage"
  cp -- "$stage/bootstrap.token" "$secret_stage/bootstrap.token"
  cp -- "$stage/session.key" "$secret_stage/session.key"
  chmod 0700 "$secret_stage"
  chmod 0600 "$secret_stage/bootstrap.token" "$secret_stage/session.key"
else
  mkdir -p "$secret_stage/cygnus/secrets"
  cp -- "$stage/bootstrap.token" "$secret_stage/cygnus/secrets/bootstrap.token"
  cp -- "$stage/session.key" "$secret_stage/cygnus/secrets/session.key"
  chmod 0700 "$secret_stage" "$secret_stage/cygnus" "$secret_stage/cygnus/secrets"
  chmod 0600 "$secret_stage/cygnus/secrets/bootstrap.token" "$secret_stage/cygnus/secrets/session.key"
fi

json_listen=$(json_safe_string "$listen")
json_https='null'
[[ -z $https_listen ]] || json_https="\"$(json_safe_string "$https_listen")\""
json_domain=$(json_safe_string "$apps_domain")
json_engine_root=$(json_safe_string "$engine_root")
json_console_root=$(json_safe_string "$console_root")
json_secret_root=$(json_safe_string "$secret_root")
# hostlib_root is only populated on Linux (the cage lowerdir that supplies the
# dynamic loader and glibc). On Darwin the cage shares the host filesystem,
# so this variable is unset and the JSON field is omitted.
json_hostlib_root=$(json_safe_string "${hostlib_root:-}")
json_console_upstream=$(json_safe_string "$console_socket")
json_secret_bootstrap_path=$(json_safe_string "$secret_bootstrap_path")
json_secret_session_path=$(json_safe_string "$secret_session_path")
json_email=$(json_safe_string "$acme_email")
json_dns='null'
[[ -z $acme_email || $dns_provider == none ]] || json_dns="\"$(json_safe_string "$dns_provider")\""
json_acme='null'
if [[ -n $acme_email ]]; then
  json_acme="{\"email\":\"$json_email\",\"directory_url\":\"https://acme-v02.api.letsencrypt.org/directory\",\"dns_provider\":$json_dns}"
fi

log "Configure Cygnus"
if [[ $OS == Darwin ]]; then
  json_command=$(json_safe_string "$prefix/bun")
  json_console_script=$(json_safe_string "$console_root/opt/cygnus-console/server.js")
  # tenant-0 has no product hostname. Operators set dashboard_domain in the
  # console; the management listener + Host default route reach it until then.
  printf '{"listen":"%s","edge":{"https_listen":%s,"apps_domain":"%s","acme":%s},"apps":[{"name":"tenant-0","domains":[],"tenant_admin":true,"upstream":"%s","command":"%s","args":["%s"],"env":{"CYGNUS_SOCKET":"%s","CYGNUS_CONSOLE_BOOTSTRAP_TOKEN_FILE":"%s","CYGNUS_CONSOLE_SESSION_KEY_FILE":"%s"},"lifecycle":{"min_instances":1}}]}\n' \
    "$json_listen" "$json_https" "$json_domain" "$json_acme" "$json_console_upstream" "$json_command" "$json_console_script" "$json_console_upstream" "$json_secret_bootstrap_path" "$json_secret_session_path" >"$stage/node.json"
else
  printf '{"listen":"%s","edge":{"https_listen":%s,"apps_domain":"%s","acme":%s},"apps":[{"name":"tenant-0","domains":[],"tenant_admin":true,"upstream":"%s","command":"/usr/local/bin/bun","args":["/opt/cygnus-console/server.js"],"init":"/usr/local/bin/cygnus-init","env":{"CYGNUS_SOCKET":"/cygnus/io/console.sock","CYGNUS_CONSOLE_BOOTSTRAP_TOKEN_FILE":"%s","CYGNUS_CONSOLE_SESSION_KEY_FILE":"%s"},"rootfs":{"lowerdirs":["%s","%s","%s","%s"]},"lifecycle":{"min_instances":1}}]}\n' \
    "$json_listen" "$json_https" "$json_domain" "$json_acme" "$json_console_upstream" "$json_secret_bootstrap_path" "$json_secret_session_path" "$json_hostlib_root" "$json_engine_root" "$json_console_root" "$json_secret_root" >"$stage/node.json"
fi
printf '%s\n' \
  '# Cygnus console credentials; keep this file mode 0600.' \
  "CYGNUS_APPS_DOMAIN=$apps_domain" \
  "CYGNUS_HTTPS_LISTEN=$https_listen" \
  "CYGNUS_ACME_EMAIL=$acme_email" \
  "CYGNUS_DNS_PROVIDER=$dns_provider" >"$stage/secrets.env"
if [[ $OS == Darwin ]]; then
  xml_escape() {
    printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'
  }
  plist_daemon=$(xml_escape "$prefix/cygnus-daemon")
  plist_state=$(xml_escape "$state_dir/state.db")
  plist_admin=$(xml_escape "$admin_socket")
  plist_tenant_admin=$(xml_escape "$tenant_admin_socket")
  plist_config=$(xml_escape "$config_file")
  plist_stdout=$(xml_escape "$log_dir/daemon.log")
  plist_stderr=$(xml_escape "$log_dir/daemon.error.log")
  printf '%s\n' \
    '<?xml version="1.0" encoding="UTF-8"?>' \
    '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' \
    '<plist version="1.0">' \
    '<dict>' \
    '  <key>Label</key>' \
    '  <string>com.cygnus.daemon</string>' \
    '  <key>ProgramArguments</key>' \
    '  <array>' \
    "    <string>$plist_daemon</string>" \
    '    <string>--state</string>' \
    "    <string>$plist_state</string>" \
    '    <string>--admin-socket</string>' \
    "    <string>$plist_admin</string>" \
    '    <string>--tenant-admin-socket</string>' \
    "    <string>$plist_tenant_admin</string>" \
    '    <string>serve</string>' \
    '    <string>--initial-config</string>' \
    "    <string>$plist_config</string>" \
    '  </array>' \
    '  <key>RunAtLoad</key>' \
    '  <true/>' \
    '  <key>KeepAlive</key>' \
    '  <true/>' \
    '  <key>StandardOutPath</key>' \
    "  <string>$plist_stdout</string>" \
    '  <key>StandardErrorPath</key>' \
    "  <string>$plist_stderr</string>" \
    '</dict>' \
    '</plist>' >"$stage/com.cygnus.daemon.plist"
else
  printf '%s\n' \
    '[Unit]' \
    'Description=Cygnus request plane' \
    'Wants=network-online.target' \
    'After=network-online.target' \
    '' \
    '[Service]' \
    'Type=simple' \
    "EnvironmentFile=-$secrets_env" \
    "ExecStart=$prefix/cygnus-daemon --state $state_dir/state.db --admin-socket $admin_socket --tenant-admin-socket $tenant_admin_socket serve --initial-config $config_file" \
    'Restart=on-failure' \
    'RestartSec=2s' \
    'UMask=0077' \
    'PrivateTmp=true' \
    'ProtectSystem=full' \
    'ProtectHome=read-only' \
    "ReadWritePaths=$state_dir $runtime_dir $config_dir" \
    '' \
    '[Install]' \
    'WantedBy=multi-user.target' >"$stage/cygnus.service"
fi
service_stage=$stage/cygnus.service
[[ $OS == Darwin ]] && service_stage=$stage/com.cygnus.daemon.plist

# Plain reruns are upgrades: preserve operator/dashboard configuration while
# replacing package content. `--reconfigure` is the explicit request to use
# newly supplied installer values. Previously the generated defaults differed
# from any configured ACME install, so a normal rerun failed before upgrading.
preserve_existing_file() {
  local dest=$1 staged=$2 label=$3
  if [[ ! -e $dest ]]; then
    return
  fi
  [[ ! -L $dest && -f $dest ]] || fail "existing $label is not a regular file: $dest"
  if (( ! reconfigure )) && ! cmp -s "$staged" "$dest"; then
    cp -- "$dest" "$staged"
    log "Preserve existing $label (use --reconfigure to replace it)"
  fi
}
preserve_existing_file "$config_file" "$stage/node.json" "node configuration"
preserve_existing_file "$service_file" "$service_stage" "service configuration"
if [[ -e $secrets_env ]]; then
  [[ ! -L $secrets_env && -f $secrets_env ]] || fail "existing secrets env is not a regular file"
  if (( ! rotate_secrets )); then
    # Secret material and its associated environment remain stable unless the
    # operator explicitly rotates it. Reconfiguration changes node.json, not
    # recovery/session credentials.
    cp -- "$secrets_env" "$stage/secrets.env"
  fi
fi
if [[ -L $console_root ]]; then
  fail "existing console root is not a real directory: $console_root"
elif [[ -e $console_root ]]; then
  [[ -d $console_root ]] || fail "existing console root is not a directory: $console_root"
fi
if [[ -L $secret_root ]]; then
  fail "existing secret root is not a real directory: $secret_root"
elif [[ -e $secret_root ]]; then
  [[ -d $secret_root ]] || fail "existing secret root is not a directory: $secret_root"
  if ! diff -qr "$secret_stage" "$secret_root" >/dev/null 2>&1 && (( ! rotate_secrets )); then
    fail "existing $secret_root differs; re-run with --rotate-secrets"
  fi
fi

if [[ $OS == Darwin ]]; then
  binaries=(cygnus-daemon cygnus bun)
else
  binaries=(cygnus-daemon cygnus cygnus-init bun)
fi
for name in "${binaries[@]}"; do
  src=$bundle_dir/$name
  cp -- "$src" "$stage/$name"
  chmod 0755 "$stage/$name"
  if [[ -e $prefix/$name ]]; then
    [[ ! -L $prefix/$name && -f $prefix/$name ]] || fail "existing $prefix/$name is not a regular file"
  fi
done
# Break-glass compatibility: the developer-facing binary is `cygnus`, but keep a
# `cygnusctl` symlink next to it so existing operator muscle memory and docs
# keep working. The symlink points at the real binary in the same directory.
if [[ -e $prefix/cygnusctl || -L $prefix/cygnusctl ]]; then
  # Non-symlink leftovers are unexpected; the install step rewrites the link.
  [[ -L $prefix/cygnusctl || ! -e $prefix/cygnusctl ]] || fail "existing $prefix/cygnusctl is not a symlink"
fi
mkdir -p "$stage/engine/usr/local/bin"
cp -- "$bundle_dir/bun" "$stage/engine/usr/local/bin/bun"
chmod 0755 "$stage/engine/usr/local/bin/bun"
if [[ $OS == Linux ]]; then
  cp -- "$bundle_dir/cygnus-init" "$stage/engine/usr/local/bin/cygnus-init"
  chmod 0755 "$stage/engine/usr/local/bin/cygnus-init"
  # Cage hostlib only stages the glibc loader. A musl-dynamic init fails at
  # execve with ENOENT looking for /lib/ld-musl-*.so.1 — catch it at install.
  if command -v file >/dev/null 2>&1; then
    init_file=$(file -b -- "$stage/engine/usr/local/bin/cygnus-init" 2>/dev/null || true)
    case $init_file in
      *ld-musl-*)
        fail "cygnus-init is musl-dynamic ($init_file); the cage hostlib only provides the glibc loader. Rebuild the release against the glibc target."
        ;;
    esac
  fi
fi
if [[ -e $engine_root ]]; then
  [[ -d $engine_root && ! -L $engine_root ]] || fail "existing engine root is not a directory"
  [[ -f $engine_root/usr/local/bin/bun && ! -L $engine_root/usr/local/bin/bun ]] || fail "existing engine executable is invalid"
  if [[ $OS == Linux ]]; then
    [[ -f $engine_root/usr/local/bin/cygnus-init && ! -L $engine_root/usr/local/bin/cygnus-init ]] || fail "existing cage init executable is invalid"
  fi
fi

ensure_dir() {
  local d=$1 mode=$2
  if [[ -e $d ]]; then [[ -d $d && ! -L $d ]] || fail "destination is not a real directory: $d"; else mkdir -p -- "$d"; fi
  chmod "$mode" "$d"
}
atomic_copy() {
  local src=$1 dest=$2 mode=$3 parent tmp
  parent=${dest%/*}
  if [[ -e $parent ]]; then
    [[ -d $parent && ! -L $parent ]] || fail "destination parent is not a real directory: $parent"
  else
    ensure_dir "$parent" 0700
  fi
  if [[ -e $dest ]]; then [[ -f $dest && ! -L $dest ]] || fail "destination is not a regular file: $dest"; fi
  if [[ -e $dest ]] && cmp -s "$src" "$dest"; then chmod "$mode" "$dest"; return; fi
  tmp=$(mktemp "$parent/.cygnus-install.XXXXXX")
  cp -- "$src" "$tmp"; chmod "$mode" "$tmp"; mv -f -- "$tmp" "$dest"
}
atomic_install_dir() {
  local src=$1 dest=$2 kind=${3:-console} parent tmp old allow_replace=0 mode=0755
  [[ $kind == secrets ]] && mode=0700
  parent=${dest%/*}
  [[ -d $parent && ! -L $parent ]] || fail "${kind} destination parent is not a real directory: $parent"
  if [[ -e $dest ]]; then
    [[ -d $dest && ! -L $dest ]] || fail "${kind} destination is not a real directory: $dest"
    if diff -qr "$src" "$dest" >/dev/null 2>&1; then chmod "$mode" "$dest"; return; fi
    if [[ $kind == secrets ]]; then
      (( rotate_secrets )) || fail "existing $dest differs; re-run with --rotate-secrets"
    fi
    # Package roots (console, engine) always track the release being installed.
    allow_replace=1
  fi
  tmp=$parent/.$(basename "$dest").staging-$$
  old=$parent/.$(basename "$dest").previous-$$
  rm -rf -- "$tmp" "$old"
  mkdir -p -- "$tmp"
  chmod "$mode" "$tmp"
  cp -Rp -- "$src/." "$tmp/"
  [[ -d $tmp && ! -L $tmp ]] || fail "staged ${kind} root is invalid"
  if [[ -e $dest ]]; then
    mv -- "$dest" "$old" || fail "unable to stage existing ${kind} root for replacement"
  fi
  if ! mv -- "$tmp" "$dest"; then
    [[ -e $old ]] && mv -- "$old" "$dest" || true
    fail "unable to install ${kind} root atomically"
  fi
  [[ ! -e $old ]] || rm -rf -- "$old"
}

# Tear down any previous install before replacing binaries or rebinding sockets.
# Reinstalls must not fight a live daemon holding the old binary/sockets.
stop_existing_service() {
  if [[ $OS == Darwin ]]; then
    local launchctl_bin
    launchctl_bin=$(command -v launchctl || true)
    if [[ -n $launchctl_bin ]]; then
      "$launchctl_bin" bootout "gui/$(id -u)/com.cygnus.daemon" >>"$diag_file" 2>&1 || true
    fi
    if (( ! TEST_MODE )); then
      # Cover both launchd-managed and direct nohup fallbacks from earlier runs.
      pkill -U "$(id -u)" -f "$prefix/cygnus-daemon" 2>/dev/null || true
      pkill -U "$(id -u)" -f "cygnus-daemon --state $state_dir/state.db" 2>/dev/null || true
      pkill -U "$(id -u)" -f "cygnus-console/server.js" 2>/dev/null || true
      local i
      for ((i=1; i<=50; i++)); do
        if ! pgrep -U "$(id -u)" -f "cygnus-daemon --state $state_dir/state.db" >/dev/null 2>&1 \
          && ! pgrep -U "$(id -u)" -f "$prefix/cygnus-daemon" >/dev/null 2>&1; then
          break
        fi
        sleep 0.1
      done
      if pgrep -U "$(id -u)" -f "cygnus-daemon --state $state_dir/state.db" >/dev/null 2>&1 \
        || pgrep -U "$(id -u)" -f "$prefix/cygnus-daemon" >/dev/null 2>&1; then
        pkill -9 -U "$(id -u)" -f "$prefix/cygnus-daemon" 2>/dev/null || true
        pkill -9 -U "$(id -u)" -f "cygnus-daemon --state $state_dir/state.db" 2>/dev/null || true
        pkill -9 -U "$(id -u)" -f "cygnus-console/server.js" 2>/dev/null || true
        sleep 0.2
      fi
      # Stale runtime sockets block the next bind if a previous process died hard.
      rm -f -- "$admin_socket" "$tenant_admin_socket" "$console_socket" 2>/dev/null || true
    fi
  else
    local systemctl_bin
    systemctl_bin=$(command -v systemctl || true)
    if [[ -n $systemctl_bin ]]; then
      "$systemctl_bin" stop cygnus.service >>"$diag_file" 2>&1 || true
    fi
  fi
}

log "Stop existing Cygnus"
stop_existing_service

log "Install Cygnus"
ensure_dir "$prefix" 0755
ensure_dir "$config_dir" 0700
ensure_dir "$state_dir" 0700
ensure_dir "$runtime_dir" 0700
ensure_dir "$runtime_dir/tenant-0" 0700
if [[ $OS == Darwin ]]; then
  ensure_dir "$launchd_dir" 0755
  ensure_dir "$log_dir" 0700
else
  ensure_dir "$systemd_dir" 0755
  ensure_dir "$state_dir/artifacts" 0700
  ensure_dir "$state_dir/logs" 0700
fi
atomic_install_dir "$console_stage" "$console_root"
atomic_install_dir "$secret_stage" "$secret_root" secrets
find "$secret_root" -type d -exec chmod 0700 {} +
find "$secret_root" -type f -exec chmod 0600 {} +
ensure_dir "$state_dir/engines" 0700
for name in "${binaries[@]}"; do atomic_copy "$stage/$name" "$prefix/$name" 0755; done
# Install the break-glass `cygnusctl` symlink after the real binary lands. A
# relative target keeps it valid regardless of where $prefix is mounted.
if [[ -L $prefix/cygnusctl ]]; then
  [[ $(readlink "$prefix/cygnusctl") == cygnus ]] || ln -sfn -- cygnus "$prefix/cygnusctl"
else
  ln -sf -- cygnus "$prefix/cygnusctl"
fi

engine_needs_install=0
if [[ ! -e $engine_root ]]; then
  engine_needs_install=1
elif ! cmp -s "$stage/engine/usr/local/bin/bun" "$engine_root/usr/local/bin/bun"; then
  engine_needs_install=1
elif [[ $OS == Linux ]] && ! cmp -s "$stage/engine/usr/local/bin/cygnus-init" "$engine_root/usr/local/bin/cygnus-init"; then
  engine_needs_install=1
fi
if (( engine_needs_install )); then
  # Build the replacement completely before moving the current engine away.
  engine_tmp="$state_dir/engines/.bun-$bun_version.staging-$$"
  rm -rf -- "$engine_tmp"
  mkdir -p "$engine_tmp/usr/local/bin"
  chmod 0755 "$engine_tmp" "$engine_tmp/usr" "$engine_tmp/usr/local" "$engine_tmp/usr/local/bin"
  cp -- "$stage/engine/usr/local/bin/bun" "$engine_tmp/usr/local/bin/bun"
  chmod 0755 "$engine_tmp/usr/local/bin/bun"
  # Framework tools (vite, etc.) often use `#!/usr/bin/env node`. Bun is a
  # drop-in for that surface; hardlink so cages don't need a separate Node.
  ln -- "$engine_tmp/usr/local/bin/bun" "$engine_tmp/usr/local/bin/node"
  chmod 0755 "$engine_tmp/usr/local/bin/node"
  if [[ $OS == Linux ]]; then
    cp -- "$stage/engine/usr/local/bin/cygnus-init" "$engine_tmp/usr/local/bin/cygnus-init"
    chmod 0755 "$engine_tmp/usr/local/bin/cygnus-init"
  fi
  old_engine=""
  if [[ -e $engine_root ]]; then
    [[ -d $engine_root && ! -L $engine_root ]] || fail "engine root is not a real directory"
    old_engine="$state_dir/engines/.bun-$bun_version.previous-$$"
    rm -rf -- "$old_engine"
    mv -- "$engine_root" "$old_engine"
  fi
  if ! mv -- "$engine_tmp" "$engine_root"; then
    [[ -n $old_engine && -e $old_engine ]] && mv -- "$old_engine" "$engine_root" || true
    fail "unable to install engine root atomically"
  fi
  [[ -n $old_engine ]] && rm -rf -- "$old_engine"
fi

# Stage a curated snapshot of the host glibc so the cage overlay's loader and
# libraries are deterministic and don't change when the host's package manager
# upgrades. This is the only lowerdir that is allowed to vary across hosts;
# the engine and console layers are reproducible from the bundle.
stage_hostlib() {
  local stage=$1
  # Loader may be a symlink on Ubuntu/Debian merged-usr layouts; cp -L below
  # resolves it to the real file before copying.
  [[ -e $hostlib_loader ]] || fail "host loader is missing: $hostlib_loader"
  [[ -d $hostlib_lib_dir && ! -L $hostlib_lib_dir ]] || fail "host lib dir is not a real directory: $hostlib_lib_dir"
  mkdir -p "$stage/lib64" "$stage/$hostlib_lib_dir"
  chmod 0755 "$stage" "$stage/lib64" "$stage/$hostlib_lib_dir"
  cp -L -- "$hostlib_loader" "$stage/lib64/$(basename -- "$hostlib_loader")"
  chmod 0755 "$stage/lib64/$(basename -- "$hostlib_loader")"
  local libs=(
    libc.so.6
    libpthread.so.0
    libdl.so.2
    libm.so.6
    libgcc_s.so.1
    libstdc++.so.6
    # Framework native addons (rollup, etc.) load these at runtime.
    librt.so.1
    libresolv.so.2
  )
  local lib src
  for lib in "${libs[@]}"; do
    src=$hostlib_lib_dir/$lib
    [[ -e $src ]] || fail "required host library is missing: $src"
    # Most libs are direct files; some distros ship libstdc++.so.6 as a
    # versioned symlink. cp -L resolves to the real file in either case.
    cp -L -- "$src" "$stage/$hostlib_lib_dir/$lib"
    chmod 0755 "$stage/$hostlib_lib_dir/$lib"
  done
  # Framework build scripts (`bun run build`, vite) need a POSIX shell.
  # Stage the host's real /bin/sh (dash on Debian/Ubuntu) as a regular file
  # at /bin/sh so cages do not depend on a /bin symlink farm.
  local host_sh
  host_sh=$(readlink -f /bin/sh 2>/dev/null || true)
  [[ -n $host_sh && -x $host_sh ]] || host_sh=/usr/bin/dash
  [[ -x $host_sh ]] || host_sh=/bin/bash
  [[ -x $host_sh ]] || fail "no usable host shell found for cage hostlib (/bin/sh)"
  mkdir -p "$stage/bin"
  chmod 0755 "$stage/bin"
  cp -L -- "$host_sh" "$stage/bin/sh"
  chmod 0755 "$stage/bin/sh"
  # Also expose as /usr/bin/env for scripts that use `#!/usr/bin/env`.
  if [[ -x /usr/bin/env ]]; then
    mkdir -p "$stage/usr/bin"
    chmod 0755 "$stage/usr" "$stage/usr/bin"
    cp -L -- /usr/bin/env "$stage/usr/bin/env"
    chmod 0755 "$stage/usr/bin/env"
  fi
}
if [[ $OS == Linux ]]; then
  hostlib_stage=$stage/hostlib
  rm -rf -- "$hostlib_stage"
  stage_hostlib "$hostlib_stage"
  # Always replace atomically: the hostlib is tiny (~5 MB) and we want it to
  # track any host-side loader changes that could break a future cage.
  hostlib_tmp="$state_dir/.hostlib.staging-$$"
  rm -rf -- "$hostlib_tmp"
  cp -Rp -- "$hostlib_stage/." "$hostlib_tmp/" || fail "unable to stage hostlib"
  chmod 0755 "$hostlib_tmp"
  [[ -d $hostlib_tmp && ! -L $hostlib_tmp ]] || fail "staged hostlib is invalid"
  if [[ -e $hostlib_root ]]; then
    [[ -d $hostlib_root && ! -L $hostlib_root ]] || fail "existing hostlib is not a real directory: $hostlib_root"
    old_hostlib="$state_dir/.hostlib.previous-$$"
    rm -rf -- "$old_hostlib"
    mv -- "$hostlib_root" "$old_hostlib" || fail "unable to stage existing hostlib for replacement"
    if ! mv -- "$hostlib_tmp" "$hostlib_root"; then
      [[ -e $old_hostlib ]] && mv -- "$old_hostlib" "$hostlib_root" || true
      fail "unable to install hostlib atomically"
    fi
    rm -rf -- "$old_hostlib"
  else
    if ! mv -- "$hostlib_tmp" "$hostlib_root"; then
      fail "unable to install hostlib atomically"
    fi
  fi
fi
atomic_copy "$stage/node.json" "$config_file" 0600
atomic_copy "$stage/secrets.env" "$secrets_env" 0600
atomic_copy "$service_stage" "$service_file" 0644


start_service() {
service_started=1
if [[ $OS == Darwin ]]; then
  launchctl_bin=$(command -v launchctl || true)
  service_started=0
  if [[ -n $launchctl_bin ]]; then
    # Reinstalls and crashed runs leave the label registered; bootout is the
    # idempotent way to clear it, and repeated failures can leave the label
    # disabled, which makes bootstrap fail with an opaque I/O error — enable
    # clears that override. Both are no-ops on a clean host.
    if command -v plutil >/dev/null 2>&1 && ! plutil -lint "$service_file" >>"$diag_file" 2>&1; then
      fail "generated launchd plist failed validation: $service_file; diagnostics: $diag_file"
    fi
    "$launchctl_bin" bootout "gui/$(id -u)/com.cygnus.daemon" >>"$diag_file" 2>&1 || true
    "$launchctl_bin" enable "gui/$(id -u)/com.cygnus.daemon" >>"$diag_file" 2>&1 || true
    if "$launchctl_bin" bootstrap "gui/$(id -u)" "$service_file" >>"$diag_file" 2>&1; then
      service_started=1
    elif "$launchctl_bin" load -w "$service_file" >>"$diag_file" 2>&1; then
      service_started=1
    else
      # Record what launchd thinks of the label for the diagnostics file.
      "$launchctl_bin" print "gui/$(id -u)/com.cygnus.daemon" >>"$diag_file" 2>&1 || true
      "$launchctl_bin" print-disabled "gui/$(id -u)" >>"$diag_file" 2>&1 || true
    fi
  fi
  if (( ! service_started )) && (( ! TEST_MODE )); then
    # launchd refused the job. Do not strand the user: run the daemon
    # directly so this install still finishes; it will not restart at login
    # until launchd accepts the service (rerun the installer to retry).
    echo "launchd did not accept the service; starting the daemon directly (no restart at login). Diagnostics: $diag_file" >&2
    nohup "$prefix/cygnus-daemon" --state "$state_dir/state.db" \
      --admin-socket "$admin_socket" --tenant-admin-socket "$tenant_admin_socket" \
      serve --initial-config "$config_file" \
      >>"$log_dir/daemon.log" 2>>"$log_dir/daemon.error.log" </dev/null &
    disown %% 2>/dev/null || true
    service_started=1
  fi
  if (( ! service_started )); then
    printf 'Launch Cygnus with: %q --state %q --admin-socket %q --tenant-admin-socket %q serve --initial-config %q\n' \
      "$prefix/cygnus-daemon" "$state_dir/state.db" "$admin_socket" "$tenant_admin_socket" "$config_file" >&2
  fi
else
  systemctl_bin=$(command -v systemctl || true)
  [[ -n $systemctl_bin ]] || fail "systemctl is required"
  "$systemctl_bin" daemon-reload >>"$diag_file" 2>&1 || fail "systemd daemon-reload failed; diagnostics: $diag_file"
  "$systemctl_bin" enable cygnus.service >>"$diag_file" 2>&1 || fail "could not enable cygnus.service; diagnostics: $diag_file"
  if ! "$systemctl_bin" restart cygnus.service >>"$diag_file" 2>&1; then
    fail "could not start cygnus.service; diagnostics: $diag_file"
  fi
fi
}

log "Start Cygnus"
start_service

socket_present() {
  if (( TEST_MODE )); then [[ -e $1 ]]; else [[ -S $1 ]]; fi
}
wait_for_socket() {
  local path=$1 attempts=$2 i
  for ((i=1; i<=attempts; i++)); do
    socket_present "$path" && return 0
    sleep 0.1
  done
  return 1
}

if ! wait_for_socket "$admin_socket" 50; then
  if [[ $OS == Darwin && $service_started -eq 0 ]]; then
    log "Cygnus is installed; start it with the foreground command above to finish configuration."
    exit 0
  fi
  fail "daemon admin socket did not become ready at $admin_socket; diagnostics: $diag_file$([[ $OS == Darwin ]] && printf '%s' '. If a previous sudo run is fighting this install, check: sudo launchctl print system/com.cygnus.daemon (remove with sudo launchctl bootout system/com.cygnus.daemon), then rerun')"
fi

"$prefix/cygnus" --admin-socket "$admin_socket" engine register --version "$bun_version" --host-root "$engine_root" --cage-executable /usr/local/bin/bun --default >>"$diag_file" 2>&1 || fail "engine registration failed; diagnostics: $diag_file"
"$prefix/cygnus" --admin-socket "$admin_socket" apply "$config_file" >>"$diag_file" 2>&1 || fail "node configuration apply failed; diagnostics: $diag_file"

# Listener and ACME account changes are persisted by apply, then become active
# after this controlled restart. Keeping the restart in the installer makes
# `--reconfigure` a complete operation instead of returning a conflict that
# forces users to manually delete state.
if (( reconfigure )); then
  log "Restart Cygnus with reconfigured node settings"
  start_service
  wait_for_socket "$admin_socket" 50 || fail "daemon did not come back after reconfiguration; diagnostics: $diag_file"
fi

# The Tenant Zero bridge socket binds at daemon startup from stored state. A
# daemon that booted before the configuration was stored (an interrupted
# earlier install) is configured now but not listening for the console — one
# restart picks the bridge up.
if ! wait_for_socket "$tenant_admin_socket" 20; then
  log "Restart Cygnus to bind the Tenant Zero bridge"
  start_service
  wait_for_socket "$admin_socket" 50 || fail "daemon did not come back after restart; diagnostics: $diag_file"
  wait_for_socket "$tenant_admin_socket" 50 || fail "Tenant Zero bridge socket did not become ready at $tenant_admin_socket; diagnostics: $diag_file"
fi

console_scheme=http
console_listener=$listen
[[ -n $https_listen ]] && { console_scheme=https; console_listener=$https_listen; }
console_port_suffix=""
if [[ $console_listener =~ :([0-9]+)$ ]]; then
  console_port=${BASH_REMATCH[1]}
  [[ $console_port == 80 || $console_port == 443 ]] || console_port_suffix=":$console_port"
fi

# Console URL for the operator. Prefer the management listener address —
# dashboard_domain is operator-owned and may not exist yet. Never invent
# cygnus.<apex> as a second competing hostname.
listen_host=${console_listener%:*}
console_host=""
access_note=""
if [[ $listen_host == 0.0.0.0 || $listen_host == "::" || $listen_host == "[::]" ]]; then
  primary_ip=""
  if [[ $OS == Linux ]]; then
    local_ips=$(hostname -I 2>/dev/null || true)
    for candidate in $local_ips; do
      [[ $candidate == *:* ]] && continue
      primary_ip=$candidate
      break
    done
  else
    primary_ip=$(ipconfig getifaddr en0 2>/dev/null || true)
  fi
  if [[ -n $primary_ip ]]; then
    console_host=$primary_ip
    access_note="set dashboard domain in the console for a stable HTTPS URL"
  else
    console_host=localhost
    access_note="loopback only; re-run with --listen 0.0.0.0:PORT to expose it"
  fi
else
  # Strip brackets from IPv6 literals for display.
  console_host=${listen_host#[}
  console_host=${console_host%]}
fi

if [[ $OS == Darwin && :$PATH: != *:$HOME/.cygnus/bin:* ]]; then
  log 'Add Cygnus to PATH: export PATH="$HOME/.cygnus/bin:$PATH"'
fi
log ""
log "Cygnus is running."
log ""
log "  console   ${console_scheme}://${console_host}${console_port_suffix}"
[[ -n $access_note ]] && log "            ($access_note)"
log "  cli       cygnus status"
if [[ $console_scheme == http && $apps_domain != apps.localhost ]]; then
  log ""
  log "  Enable HTTPS + push-to-deploy: re-run with"
  log "    --https-listen 0.0.0.0:443 --acme-email you@example.com"
fi
log ""
log "IMPORTANT: save your recovery token now — it is shown only this once."
log "It signs you into the console if you ever lose your password, and it is"
log "the only way in until you create the admin account."
log ""
log "  $bootstrap_hex"
log ""
log "Lost it later? Rotate with: install.sh --rotate-secrets"
