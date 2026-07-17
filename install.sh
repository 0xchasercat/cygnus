#!/usr/bin/env bash
# Cygnus local-bundle installer.  This script deliberately does not fetch code.
set -Eeuo pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
VERSION_DEFAULT=${CYGNUS_BUN_VERSION:-bundled}
TEST_MODE=0
if [[ ${CYGNUS_INSTALL_TEST_MODE:-0} == 1 || ${CYGNUS_INSTALL_TEST:-0} == 1 || ${CYGNUS_TEST_MODE:-0} == 1 ]]; then
  TEST_MODE=1
fi
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

Install Cygnus from a local release bundle (never downloads or executes a remote
script).  Interactive installs prompt for values not supplied on the command
line; --noninteractive requires --bundle-dir.

Options:
  --bundle-dir DIR       Local release bundle containing required binaries and cygnus-console.tar
  --prefix DIR           Binary destination (default: /usr/local/bin)
  --config-dir DIR       Configuration/secrets destination (default: /etc/cygnus)
  --state-dir DIR        Durable state/artifacts destination (default: /var/lib/cygnus)
  --runtime-dir DIR      Runtime sockets destination (default: /run/cygnus)
  --listen ADDR          HTTP listener (default: 127.0.0.1:3000)
  --https-listen ADDR    Optional HTTPS listener (default: disabled)
  --apps-domain DOMAIN   Default application domain (default: apps.localhost)
  --acme-email EMAIL     ACME account email (optional unless HTTPS is enabled)
  --dns-provider NAME    DNS provider (default: none)
  --bun-version VERSION  Registered Bun engine version (default: bundled)
  --noninteractive       Never prompt; fail when required input is absent
  --reconfigure          Permit replacing changed existing files/configuration
  --rotate-secrets       Generate and atomically install a new console secret
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
    -h|--help) usage; exit 0 ;;
    --) shift; break ;;
    -*) fail "unknown option: $1 (use --help)" ;;
    *) fail "unexpected argument: $1 (use --help)" ;;
  esac
done

if [[ -z $bundle_dir && $noninteractive -eq 0 && -t 0 && -t 1 ]]; then
  default_bundle="$SCRIPT_DIR/release"
  printf 'Local release bundle directory [%s]: ' "$default_bundle" >&2
  IFS= read -r answer || true
  bundle_dir=${answer:-$default_bundle}
fi
if [[ -z $listen && $noninteractive -eq 0 && -t 0 && -t 1 ]]; then
  printf 'HTTP listen address [127.0.0.1:3000]: ' >&2; IFS= read -r answer || true; listen=${answer:-127.0.0.1:3000}
fi
if [[ -z $https_listen && $https_set -eq 0 && $noninteractive -eq 0 && -t 0 && -t 1 ]]; then
  printf 'HTTPS listen address [disabled]: ' >&2; IFS= read -r answer || true; https_listen=$answer
fi
if [[ -z $apps_domain && $noninteractive -eq 0 && -t 0 && -t 1 ]]; then
  printf 'Applications domain [apps.localhost]: ' >&2; IFS= read -r answer || true; apps_domain=${answer:-apps.localhost}
fi
if [[ -z $acme_email && $noninteractive -eq 0 && -t 0 && -t 1 ]]; then
  printf 'ACME email [optional]: ' >&2; IFS= read -r answer || true; acme_email=$answer
fi
if [[ -z $dns_provider && $noninteractive -eq 0 && -t 0 && -t 1 ]]; then
  printf 'DNS provider [none]: ' >&2; IFS= read -r answer || true; dns_provider=${answer:-none}
fi

if [[ -n $bundle_dir && $bundle_dir != /* ]]; then
  [[ $bundle_dir != .. && $bundle_dir != ../* && $bundle_dir != */../* && $bundle_dir != */.. ]] || fail "bundle path traversal is not allowed"
  bundle_dir=$PWD/${bundle_dir#./}
fi
# A test root maps only default destinations. Explicit paths are never rewritten.
map_default_path() {
  local value=$1 was_set=$2
  if (( TEST_MODE )) && [[ -n $TEST_ROOT ]] && (( ! was_set )) && [[ $value == /* ]]; then
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
for path_name in bundle_dir prefix config_dir state_dir runtime_dir systemd_dir; do
  [[ -n ${!path_name} ]] || fail "$path_name is required"
  is_abs_safe "${!path_name}" || fail "$path_name must be an absolute path without whitespace or path traversal: ${!path_name}"
done
[[ -n $bundle_dir ]] || fail "--bundle-dir is required (use --noninteractive for automation)"
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

if (( ! TEST_MODE )); then
  [[ $(id -u) -eq 0 ]] || fail "root is required (or set CYGNUS_INSTALL_TEST_MODE=1 only for tests)"
  [[ $(uname -s) == Linux ]] || fail "Cygnus installation requires Linux"
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
if (( ! TEST_MODE )); then check_host; fi

# All source checks happen before any destination mkdir/write.  The staging
# directory and diagnostics are outside installation destinations.
stage=$(mktemp -d "${TMPDIR:-/tmp}/cygnus-install.XXXXXX")
diag_file=${TMPDIR:-/tmp}/cygnus-install-$BASHPID.log
: >"$diag_file"
chmod 0600 "$diag_file"
exec 3>>"$diag_file"
cleanup() {
  local status=$?
  exec 3>&- || true
  rm -rf -- "$stage"
  if (( status == 0 )); then rm -f -- "$diag_file"; else printf 'cygnus installer: diagnostics retained at %s\n' "$diag_file" >&2; fi
  return "$status"
}
trap cleanup EXIT
log() { printf '%s\n' "$*" | tee -a /dev/fd/3 >&2; }

[[ -d $bundle_dir && ! -L $bundle_dir ]] || fail "bundle directory is not a real directory: $bundle_dir"
sums_file=$bundle_dir/SHA256SUMS
[[ -f $sums_file && ! -L $sums_file ]] || fail "bundle SHA256SUMS is missing or not regular"
command -v sha256sum >/dev/null 2>&1 && hash_tool=sha256sum || hash_tool=shasum
command -v "$hash_tool" >/dev/null 2>&1 || fail "sha256 checksum tool is missing"
required=(cygnus-daemon cygnusctl cygnus-init bun cygnus-console.tar)
declare -A expected=()
while IFS= read -r sum_line || [[ -n $sum_line ]]; do
  [[ -z $sum_line ]] && continue
  # Checksums are intentionally strict: only a hash and one bundle basename.
  read -r sum name extra <<<"$sum_line"
  [[ -n ${sum:-} && -n ${name:-} && -z ${extra:-} ]] || fail "malformed checksum line"
  if [[ $name == \** ]]; then name=${name#\*}; fi
  [[ $sum =~ ^[[:xdigit:]]{64}$ ]] || fail "invalid checksum in SHA256SUMS"
  case $name in cygnus-daemon|cygnusctl|cygnus-init|bun|cygnus-console.tar) ;; *) fail "unexpected or unsafe checksum path: $name" ;; esac
  [[ -z ${expected[$name]+present} ]] || fail "duplicate checksum entry: $name"
  expected[$name]=${sum,,}
done < "$sums_file"
checksum_file() {
  local file=$1 result
  if [[ $hash_tool == sha256sum ]]; then result=$(sha256sum -- "$file"); else result=$(shasum -a 256 -- "$file"); fi
  printf '%s' "${result%% *}"
}
for name in "${required[@]}"; do
  src=$bundle_dir/$name
  [[ -n ${expected[$name]+present} ]] || fail "SHA256SUMS has no entry for required binary: $name"
  [[ -f $src && ! -L $src ]] || fail "bundle input is not a regular file: $name"
  if [[ $name != cygnus-console.tar ]]; then [[ -x $src ]] || fail "bundle input is not executable: $name"; fi
  actual=$(checksum_file "$src")
  [[ ${expected[$name]} == "${actual,,}" ]] || fail "checksum verification failed for $name"
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
service_file=$systemd_dir/cygnus.service
admin_socket=$runtime_dir/admin.sock
tenant_admin_socket=$runtime_dir/tenant-0/admin.sock
engine_root=$state_dir/engines/bun-$bun_version
console_root=$state_dir/artifacts/tenant-0
secret_root=$state_dir/artifacts/tenant-0-secrets
secret_bootstrap_file=$secret_root/cygnus/secrets/bootstrap.token
secret_session_file=$secret_root/cygnus/secrets/session.key
console_socket=$runtime_dir/tenant-0/console.sock
secret_bootstrap_path=/cygnus/secrets/bootstrap.token
secret_session_path=/cygnus/secrets/session.key

json_safe_string() {
  [[ $1 != *'"'* && $1 != *'\\'* && $1 != *$'\n'* && $1 != *$'\r'* ]] || fail "value cannot be represented safely in generated JSON"
  printf '%s' "$1"
}
json_listen=$(json_safe_string "$listen")
# Preserve each credential independently unless rotation is explicit. Raw files
# are exactly 32 bytes; the rooted cage lowerdir carries them read-only.
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
    [[ -e $secret_bootstrap_file && -e $secret_session_file ]] || fail "secret lowerdir credentials are incomplete; use --rotate-secrets"
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
mkdir -p "$secret_stage/cygnus/secrets"
cp -- "$stage/bootstrap.token" "$secret_stage/cygnus/secrets/bootstrap.token"
cp -- "$stage/session.key" "$secret_stage/cygnus/secrets/session.key"
chmod 0700 "$secret_stage" "$secret_stage/cygnus" "$secret_stage/cygnus/secrets"
chmod 0600 "$secret_stage/cygnus/secrets/bootstrap.token" "$secret_stage/cygnus/secrets/session.key"

json_listen=$(json_safe_string "$listen")
json_https='null'
[[ -z $https_listen ]] || json_https="\"$(json_safe_string "$https_listen")\""
json_domain=$(json_safe_string "$apps_domain")
json_console_domain=$(json_safe_string "cygnus.$apps_domain")
json_engine_root=$(json_safe_string "$engine_root")
json_console_root=$(json_safe_string "$console_root")
json_secret_root=$(json_safe_string "$secret_root")
json_console_upstream=$(json_safe_string "$console_socket")
json_email=$(json_safe_string "$acme_email")
json_dns='null'
[[ -z $acme_email || $dns_provider == none ]] || json_dns="\"$(json_safe_string "$dns_provider")\""
json_acme='null'
if [[ -n $acme_email ]]; then
  json_acme="{\"email\":\"$json_email\",\"directory_url\":\"https://acme-v02.api.letsencrypt.org/directory\",\"dns_provider\":$json_dns}"
fi

cat >"$stage/node.json" <<EOF
{"listen":"$json_listen","edge":{"https_listen":$json_https,"apps_domain":"$json_domain","acme":$json_acme},"apps":[{"name":"tenant-0","domains":["$json_console_domain"],"tenant_admin":true,"upstream":"$json_console_upstream","command":"/usr/local/bin/bun","args":["/opt/cygnus-console/server.js"],"init":"/usr/local/bin/cygnus-init","env":{"CYGNUS_SOCKET":"/cygnus/io/console.sock","CYGNUS_CONSOLE_BOOTSTRAP_TOKEN_FILE":"$secret_bootstrap_path","CYGNUS_CONSOLE_SESSION_KEY_FILE":"$secret_session_path"},"rootfs":{"lowerdirs":["$json_engine_root","$json_console_root","$json_secret_root"]},"lifecycle":{"min_instances":1}}]}
EOF
cat >"$stage/secrets.env" <<EOF
# Cygnus console credentials; keep this file mode 0600.
CYGNUS_APPS_DOMAIN=$apps_domain
CYGNUS_HTTPS_LISTEN=$https_listen
CYGNUS_ACME_EMAIL=$acme_email
CYGNUS_DNS_PROVIDER=$dns_provider
EOF
cat >"$stage/cygnus.service" <<EOF
[Unit]
Description=Cygnus request plane
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
EnvironmentFile=-$secrets_env
ExecStart=$prefix/cygnus-daemon --state $state_dir/state.db --admin-socket $admin_socket --tenant-admin-socket $tenant_admin_socket serve --initial-config $config_file
Restart=on-failure
RestartSec=2s
UMask=0077
PrivateTmp=true
ProtectSystem=full
ProtectHome=read-only
ReadWritePaths=$state_dir $runtime_dir $config_dir

[Install]
WantedBy=multi-user.target
EOF

# Existing paths are never replaced without --reconfigure (or secret rotation).
existing_diff() { [[ -e $1 ]] && { [[ ! -L $1 && -f $1 ]] || fail "existing path is not a regular file: $1"; } && ! cmp -s "$2" "$1"; }
check_change_allowed() {
  local dest=$1 src=$2
  if existing_diff "$dest" "$src" && (( ! reconfigure )); then
    fail "existing $dest differs; re-run with --reconfigure (secrets require --rotate-secrets)"
  fi
}
check_change_allowed "$config_file" "$stage/node.json"
check_change_allowed "$service_file" "$stage/cygnus.service"
if [[ -e $secrets_env ]]; then
  [[ ! -L $secrets_env && -f $secrets_env ]] || fail "existing secrets env is not a regular file"
  if (( ! rotate_secrets )); then
    old_nonsecret=$(cat "$secrets_env")
    new_nonsecret=$(cat "$stage/secrets.env")
    if [[ $old_nonsecret != "$new_nonsecret" && $reconfigure -eq 0 ]]; then
      fail "existing $secrets_env differs; re-run with --reconfigure"
    fi
  fi
fi
if [[ -L $console_root ]]; then
  fail "existing console root is not a real directory: $console_root"
elif [[ -e $console_root ]]; then
  [[ -d $console_root ]] || fail "existing console root is not a directory: $console_root"
  if ! diff -qr "$console_stage" "$console_root" >/dev/null 2>&1 && (( ! reconfigure )); then
    fail "existing $console_root differs; re-run with --reconfigure"
  fi
fi
if [[ -L $secret_root ]]; then
  fail "existing secret root is not a real directory: $secret_root"
elif [[ -e $secret_root ]]; then
  [[ -d $secret_root ]] || fail "existing secret root is not a directory: $secret_root"
  if ! diff -qr "$secret_stage" "$secret_root" >/dev/null 2>&1 && (( ! reconfigure && ! rotate_secrets )); then
    fail "existing $secret_root differs; re-run with --rotate-secrets"
  fi
fi

binaries=(cygnus-daemon cygnusctl cygnus-init bun)
for name in "${binaries[@]}"; do
  src=$bundle_dir/$name
  cp -- "$src" "$stage/$name"
  chmod 0755 "$stage/$name"
  if [[ -e $prefix/$name ]] && { [[ -L $prefix/$name || ! -f $prefix/$name ]] || ! cmp -s "$src" "$prefix/$name"; }; then
    (( reconfigure )) || fail "existing $prefix/$name differs; re-run with --reconfigure"
  fi
done
mkdir -p "$stage/engine/usr/local/bin"
cp -- "$bundle_dir/bun" "$stage/engine/usr/local/bin/bun"
cp -- "$bundle_dir/cygnus-init" "$stage/engine/usr/local/bin/cygnus-init"
chmod 0755 "$stage/engine/usr/local/bin/bun" "$stage/engine/usr/local/bin/cygnus-init"
if [[ -e $engine_root ]]; then
  [[ -d $engine_root && ! -L $engine_root ]] || fail "existing engine root is not a directory"
  [[ -f $engine_root/usr/local/bin/bun && ! -L $engine_root/usr/local/bin/bun ]] || fail "existing engine executable is invalid"
  [[ -f $engine_root/usr/local/bin/cygnus-init && ! -L $engine_root/usr/local/bin/cygnus-init ]] || fail "existing cage init executable is invalid"
  if ! cmp -s "$bundle_dir/bun" "$engine_root/usr/local/bin/bun" && (( ! reconfigure )); then
    fail "existing engine differs; re-run with --reconfigure"
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
    (( reconfigure )) && allow_replace=1
    [[ $kind == secrets && $rotate_secrets -eq 1 ]] && allow_replace=1
    (( allow_replace )) || fail "existing $dest differs; re-run with $([[ $kind == secrets ]] && printf '%s' --rotate-secrets || printf '%s' --reconfigure)"
  fi
  tmp=$parent/.$(basename "$dest").staging-$BASHPID
  old=$parent/.$(basename "$dest").previous-$BASHPID
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

log "Installing verified Cygnus release from $bundle_dir"
ensure_dir "$prefix" 0755
ensure_dir "$config_dir" 0700
ensure_dir "$state_dir" 0700
ensure_dir "$runtime_dir" 0700
ensure_dir "$runtime_dir/tenant-0" 0700
ensure_dir "$systemd_dir" 0755
ensure_dir "$state_dir/artifacts" 0700
ensure_dir "$state_dir/logs" 0700
atomic_install_dir "$console_stage" "$console_root"
atomic_install_dir "$secret_stage" "$secret_root" secrets
# Enforce least-privilege modes on the secret lowerdir regardless of how cp
# handled them (GNU cp without -p, umask, container filesystems, etc.).
find "$secret_root" -type d -exec chmod 0700 {} +
find "$secret_root" -type f -exec chmod 0600 {} +
ensure_dir "$state_dir/engines" 0700
for name in "${binaries[@]}"; do atomic_copy "$stage/$name" "$prefix/$name" 0755; done

if [[ ! -e $engine_root || $reconfigure -eq 1 ]]; then
  # Build the replacement completely before moving the current engine away.
  engine_tmp="$state_dir/engines/.bun-$bun_version.staging-$$"
  rm -rf -- "$engine_tmp"
  mkdir -p "$engine_tmp/usr/local/bin"
  chmod 0755 "$engine_tmp" "$engine_tmp/usr" "$engine_tmp/usr/local" "$engine_tmp/usr/local/bin"
  cp -- "$stage/engine/usr/local/bin/bun" "$engine_tmp/usr/local/bin/bun"
  cp -- "$stage/engine/usr/local/bin/cygnus-init" "$engine_tmp/usr/local/bin/cygnus-init"
  chmod 0755 "$engine_tmp/usr/local/bin/bun" "$engine_tmp/usr/local/bin/cygnus-init"
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
atomic_copy "$stage/node.json" "$config_file" 0600
atomic_copy "$stage/secrets.env" "$secrets_env" 0600
atomic_copy "$stage/cygnus.service" "$service_file" 0644

systemctl_bin=$(command -v systemctl || true)
[[ -n $systemctl_bin ]] || fail "systemctl is required"
"$systemctl_bin" daemon-reload >>"$diag_file" 2>&1 || fail "systemd daemon-reload failed; diagnostics: $diag_file"
"$systemctl_bin" enable cygnus.service >>"$diag_file" 2>&1 || fail "could not enable cygnus.service; diagnostics: $diag_file"
if ! "$systemctl_bin" restart cygnus.service >>"$diag_file" 2>&1; then
  fail "could not start cygnus.service; diagnostics: $diag_file"
fi

ready=0
for ((attempt=1; attempt<=50; attempt++)); do
  if (( TEST_MODE )); then
    [[ -e $admin_socket && -e $tenant_admin_socket ]] && { ready=1; break; }
  else
    [[ -S $admin_socket && -S $tenant_admin_socket ]] && { ready=1; break; }
  fi
  sleep 0.1
done
(( ready )) || fail "daemon admin sockets did not become ready at $admin_socket and $tenant_admin_socket; diagnostics: $diag_file"

"$prefix/cygnusctl" --admin-socket "$admin_socket" engine register --version "$bun_version" --host-root "$engine_root" --cage-executable /usr/local/bin/bun >>"$diag_file" 2>&1 || fail "engine registration failed; diagnostics: $diag_file"
"$prefix/cygnusctl" --admin-socket "$admin_socket" apply "$config_file" >>"$diag_file" 2>&1 || fail "node configuration apply failed; diagnostics: $diag_file"

console_scheme=http
console_listener=$listen
[[ -n $https_listen ]] && { console_scheme=https; console_listener=$https_listen; }
console_port_suffix=""
if [[ $console_listener =~ :([0-9]+)$ ]]; then
  console_port=${BASH_REMATCH[1]}
  [[ $console_port == 80 || $console_port == 443 ]] || console_port_suffix=":$console_port"
fi
log "Cygnus is installed and configured. Console URL: ${console_scheme}://cygnus.${apps_domain}${console_port_suffix}"
log "Bootstrap token file: $secret_bootstrap_file"
log "Next action: log in to the host and deploy with cygnusctl --admin-socket $admin_socket."
if [[ -n $https_listen ]]; then log "HTTPS is configured at $https_listen; ACME/DNS provider settings are in $secrets_env."; fi
