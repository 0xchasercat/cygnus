#!/usr/bin/env bash
# Focused, rootless installer tests. Every destination lives below a temp root.
set -Eeuo pipefail

ROOT=$(CDPATH= cd -- "$(mktemp -d "${TMPDIR:-/tmp}/cygnus-install-test.XXXXXX")" && pwd)
trap 'rm -rf -- "$ROOT"' EXIT
INSTALLER=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)/install.sh
BUNDLE=$ROOT/release
FAKEBIN=$ROOT/fake-bin
CONSOLE_BUILD=$ROOT/console-build
mkdir -p "$BUNDLE" "$FAKEBIN" "$CONSOLE_BUILD/opt/cygnus-console/dist"
printf '%s\n' 'export default {}' >"$CONSOLE_BUILD/opt/cygnus-console/server.js"
printf '%s\n' 'export function adminRequest() {}' >"$CONSOLE_BUILD/opt/cygnus-console/admin-client.js"
printf '%s\n' '<!doctype html><title>Cygnus Console</title>' >"$CONSOLE_BUILD/opt/cygnus-console/dist/index.html"

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum -- "$1" | cut -d' ' -f1; else shasum -a 256 -- "$1" | cut -d' ' -f1; fi
}
write_checksums() {
  local name
  : >"$BUNDLE/SHA256SUMS"
  for name in cygnus-daemon cygnusctl cygnus-init bun cygnus-console.tar; do
    printf '%s  %s\n' "$(hash_file "$BUNDLE/$name")" "$name" >>"$BUNDLE/SHA256SUMS"
  done
}
make_bundle() {
  local name
  for name in cygnus-daemon cygnusctl cygnus-init bun; do
    cat >"$BUNDLE/$name" <<'EOF'
#!/usr/bin/env sh
case "$(basename "$0")" in
  cygnusctl)
    printf '%s\n' "$*" >> "${CYGNUS_TEST_CTL_LOG:?}"
    exit 0
    ;;
  *) exit 0 ;;
esac
EOF
    chmod 0755 "$BUNDLE/$name"
  done
  tar -cf "$BUNDLE/cygnus-console.tar" -C "$CONSOLE_BUILD" opt/cygnus-console
  write_checksums
}
make_bundle

cat >"$FAKEBIN/systemctl" <<'EOF'
#!/usr/bin/env sh
printf '%s\n' "$*" >> "${CYGNUS_TEST_SYSTEMCTL_LOG:?}"
if [ "$1" = restart ] && [ "${CYGNUS_TEST_NO_READY:-0}" != 1 ]; then
  mkdir -p "$(dirname "$CYGNUS_TEST_READY_FILE")" "$(dirname "$CYGNUS_TEST_TENANT_READY_FILE")"
  : >"$CYGNUS_TEST_READY_FILE"
  : >"$CYGNUS_TEST_TENANT_READY_FILE"
fi
exit "${CYGNUS_TEST_SYSTEMCTL_STATUS:-0}"
EOF
chmod 0755 "$FAKEBIN/systemctl"

export CYGNUS_INSTALL_TEST_MODE=1
export CYGNUS_INSTALL_TEST_ROOT="$ROOT"
export CYGNUS_TEST_CTL_LOG="$ROOT/ctl.log"
export CYGNUS_TEST_SYSTEMCTL_LOG="$ROOT/systemctl.log"
export CYGNUS_TEST_READY_FILE="$ROOT/run/cygnus/admin.sock"
export CYGNUS_TEST_TENANT_READY_FILE="$ROOT/run/cygnus/tenant-0/admin.sock"
export PATH="$FAKEBIN:$PATH"

run_install() {
  bash "$INSTALLER" --noninteractive --bundle-dir "$BUNDLE" \
    --prefix "$ROOT/usr/local/bin" --config-dir "$ROOT/etc/cygnus" \
    --state-dir "$ROOT/var/lib/cygnus" --runtime-dir "$ROOT/run/cygnus" \
    --listen 127.0.0.1:3300 --https-listen 127.0.0.1:3443 --apps-domain apps.test \
    --acme-email ops@apps.test --dns-provider cloudflare --bun-version 1.3.14 "$@"
}
expect_fail() {
  if "$@" >/dev/null 2>&1; then echo "expected command to fail: $*" >&2; exit 1; fi
}
assert_destinations_absent() {
  [[ ! -e $ROOT/usr/local/bin && ! -e $ROOT/etc/cygnus && ! -e $ROOT/var/lib/cygnus ]] || {
    echo 'failed installer wrote destinations before source validation' >&2; exit 1;
  }
}

# 1. Checksum failures happen before any destination is created.
printf 'tampered\n' >>"$BUNDLE/cygnus-console.tar"
expect_fail run_install
assert_destinations_absent
make_bundle

# 2. Traversal entries are rejected before extraction or destination writes.
command -v python3 >/dev/null 2>&1 || { echo 'python3 is required for archive safety fixture' >&2; exit 1; }
python3 - "$BUNDLE/cygnus-console.tar" "$CONSOLE_BUILD" <<'PY'
import io, pathlib, sys, tarfile
archive, root = sys.argv[1:]
with tarfile.open(archive, "w") as output:
    output.add(pathlib.Path(root) / "opt", arcname="opt")
    payload = b"escape"
    entry = tarfile.TarInfo("../escape")
    entry.size = len(payload)
    output.addfile(entry, io.BytesIO(payload))
PY
write_checksums
expect_fail run_install
assert_destinations_absent
make_bundle

# 3. First install creates the rooted console, pinned Tenant Zero config, and
# least-privilege files/directories.
run_install >"$ROOT/install-output" 2>&1
[[ -x $ROOT/usr/local/bin/cygnus-daemon && -x $ROOT/usr/local/bin/cygnusctl && -x $ROOT/usr/local/bin/cygnus-init ]] || { echo 'binaries missing' >&2; exit 1; }
[[ -x $ROOT/var/lib/cygnus/engines/bun-1.3.14/usr/local/bin/bun && -x $ROOT/var/lib/cygnus/engines/bun-1.3.14/usr/local/bin/cygnus-init ]] || { echo 'engine lowerdir missing executable/init' >&2; exit 1; }
[[ -f $ROOT/var/lib/cygnus/artifacts/tenant-0/opt/cygnus-console/server.js ]] || { echo 'console server missing from lowerdir' >&2; exit 1; }
[[ -f $ROOT/var/lib/cygnus/artifacts/tenant-0/opt/cygnus-console/admin-client.js ]] || { echo 'console admin client missing from lowerdir' >&2; exit 1; }
[[ -f $ROOT/var/lib/cygnus/artifacts/tenant-0/opt/cygnus-console/dist/index.html ]] || { echo 'console dist missing from lowerdir' >&2; exit 1; }
[[ -f $ROOT/etc/cygnus/node.json && -f $ROOT/etc/cygnus/secrets.env && -f $ROOT/var/lib/cygnus/artifacts/tenant-0-secrets/cygnus/secrets/bootstrap.token && -f $ROOT/var/lib/cygnus/artifacts/tenant-0-secrets/cygnus/secrets/session.key ]] || { echo 'configuration/credentials missing' >&2; exit 1; }
[[ ! -e $ROOT/etc/cygnus/console-bootstrap.token && ! -e $ROOT/etc/cygnus/console-session.key ]] || { echo 'duplicate config-dir credentials exist' >&2; exit 1; }
for credential in "$ROOT/var/lib/cygnus/artifacts/tenant-0-secrets/cygnus/secrets/bootstrap.token" "$ROOT/var/lib/cygnus/artifacts/tenant-0-secrets/cygnus/secrets/session.key"; do
  [[ $(stat -c '%a' "$credential" 2>/dev/null || stat -f '%Lp' "$credential") == 600 ]] || { echo "credential is not 0600: $credential" >&2; exit 1; }
  [[ $(wc -c <"$credential" | tr -d ' ') == 32 ]] || { echo "credential is not 32 bytes: $credential" >&2; exit 1; }
done
[[ $(stat -c '%a' "$ROOT/etc/cygnus/secrets.env" 2>/dev/null || stat -f '%Lp' "$ROOT/etc/cygnus/secrets.env") == 600 ]] || { echo 'secrets.env is not 0600' >&2; exit 1; }
! grep -q '^CYGNUS_CONSOLE_BOOTSTRAP_TOKEN=' "$ROOT/etc/cygnus/secrets.env" || { echo 'secrets.env persisted bootstrap secret' >&2; exit 1; }
! grep -q '^CYGNUS_CONSOLE_SESSION_KEY=' "$ROOT/etc/cygnus/secrets.env" || { echo 'secrets.env persisted session secret' >&2; exit 1; }
python3 - "$ROOT/etc/cygnus/node.json" "$ROOT" <<'PY'
import json, pathlib, sys
node = json.load(open(sys.argv[1]))
root = pathlib.Path(sys.argv[2])
assert node["edge"]["https_listen"] == "127.0.0.1:3443"
assert node["edge"]["apps_domain"] == "apps.test"
assert node["edge"]["acme"]["email"] == "ops@apps.test"
assert node["edge"]["acme"]["dns_provider"] == "cloudflare"
assert len(node["apps"]) == 1
app = node["apps"][0]
assert app["name"] == "tenant-0"
assert app["domains"] == ["cygnus.apps.test"]
assert app["tenant_admin"] is True
assert app["upstream"] == str(root / "run/cygnus/tenant-0/console.sock")
assert app["command"] == "/usr/local/bin/bun"
assert app["args"] == ["/opt/cygnus-console/server.js"]
assert app["init"] == "/usr/local/bin/cygnus-init"
assert app["env"]["CYGNUS_SOCKET"] == "/cygnus/io/console.sock"
assert app["env"]["CYGNUS_CONSOLE_BOOTSTRAP_TOKEN_FILE"] == "/cygnus/secrets/bootstrap.token"
assert app["env"]["CYGNUS_CONSOLE_SESSION_KEY_FILE"] == "/cygnus/secrets/session.key"
assert "CYGNUS_CONSOLE_BOOTSTRAP_TOKEN" not in app["env"]
assert "CYGNUS_CONSOLE_SESSION_KEY" not in app["env"]
assert app["rootfs"]["lowerdirs"] == [str(root / "var/lib/cygnus/engines/bun-1.3.14"), str(root / "var/lib/cygnus/artifacts/tenant-0"), str(root / "var/lib/cygnus/artifacts/tenant-0-secrets")]
secret_root = root / "var/lib/cygnus/artifacts/tenant-0-secrets/cygnus/secrets"
assert secret_root.joinpath("bootstrap.token").stat().st_mode & 0o777 == 0o600
assert secret_root.joinpath("session.key").stat().st_mode & 0o777 == 0o600
assert len(secret_root.joinpath("bootstrap.token").read_bytes()) == 32
assert len(secret_root.joinpath("session.key").read_bytes()) == 32
assert app["lifecycle"]["min_instances"] == 1
PY
grep -q "ExecStart=$ROOT/usr/local/bin/cygnus-daemon.*--admin-socket $ROOT/run/cygnus/admin.sock.*--tenant-admin-socket $ROOT/run/cygnus/tenant-0/admin.sock.*serve --initial-config $ROOT/etc/cygnus/node.json" "$ROOT/etc/systemd/system/cygnus.service" || { echo 'unit initial config/admin sockets missing' >&2; exit 1; }
grep -q 'daemon-reload' "$ROOT/systemctl.log" || { echo 'daemon reload missing' >&2; exit 1; }
grep -q 'restart cygnus.service' "$ROOT/systemctl.log" || { echo 'daemon restart missing' >&2; exit 1; }
grep -q 'engine register' "$ROOT/ctl.log" || { echo 'engine admin call missing' >&2; exit 1; }
grep -q 'apply ' "$ROOT/ctl.log" || { echo 'apply admin call missing' >&2; exit 1; }
grep -q 'Console URL: https://cygnus.apps.test:3443' "$ROOT/install-output" || { echo 'console URL missing from output' >&2; exit 1; }
grep -q "Bootstrap token file: $ROOT/var/lib/cygnus/artifacts/tenant-0-secrets/cygnus/secrets/bootstrap.token" "$ROOT/install-output" || { echo 'bootstrap token path missing from output' >&2; exit 1; }
bootstrap_before=$(hash_file "$ROOT/var/lib/cygnus/artifacts/tenant-0-secrets/cygnus/secrets/bootstrap.token")
session_before=$(hash_file "$ROOT/var/lib/cygnus/artifacts/tenant-0-secrets/cygnus/secrets/session.key")
config_before=$(cat "$ROOT/etc/cygnus/node.json")
unit_before=$(cat "$ROOT/etc/systemd/system/cygnus.service")
console_before=$(hash_file "$ROOT/var/lib/cygnus/artifacts/tenant-0/opt/cygnus-console/server.js")

# 4. Idempotent rerun preserves generated values/content and the console root.
run_install
[[ $(hash_file "$ROOT/var/lib/cygnus/artifacts/tenant-0-secrets/cygnus/secrets/bootstrap.token") == "$bootstrap_before" ]] || { echo 'idempotent rerun rotated bootstrap token' >&2; exit 1; }
[[ $(hash_file "$ROOT/var/lib/cygnus/artifacts/tenant-0-secrets/cygnus/secrets/session.key") == "$session_before" ]] || { echo 'idempotent rerun rotated session key' >&2; exit 1; }
[[ $(cat "$ROOT/etc/cygnus/node.json") == "$config_before" && $(cat "$ROOT/etc/systemd/system/cygnus.service") == "$unit_before" ]] || { echo 'idempotent rerun changed content' >&2; exit 1; }
[[ $(hash_file "$ROOT/var/lib/cygnus/artifacts/tenant-0/opt/cygnus-console/server.js") == "$console_before" ]] || { echo 'idempotent rerun changed console root' >&2; exit 1; }

# 5. Changed console content is gated, then atomically replaced with --reconfigure.
printf '%s\n' 'changed console' >"$CONSOLE_BUILD/opt/cygnus-console/server.js"
make_bundle
expect_fail run_install
[[ $(hash_file "$ROOT/var/lib/cygnus/artifacts/tenant-0/opt/cygnus-console/server.js") == "$console_before" ]] || { echo 'gated reconfigure changed console root' >&2; exit 1; }
run_install --reconfigure
[[ $(cat "$ROOT/var/lib/cygnus/artifacts/tenant-0/opt/cygnus-console/server.js") == 'changed console' ]] || { echo 'reconfigure did not replace console root' >&2; exit 1; }

# 6. Secret rotation is explicit, changes both credentials, and updates the
# rooted app env without requiring a separate reconfigure flag.
run_install --rotate-secrets
bootstrap_after=$(hash_file "$ROOT/var/lib/cygnus/artifacts/tenant-0-secrets/cygnus/secrets/bootstrap.token")
session_after=$(hash_file "$ROOT/var/lib/cygnus/artifacts/tenant-0-secrets/cygnus/secrets/session.key")
[[ $bootstrap_after != "$bootstrap_before" && $session_after != "$session_before" ]] || { echo 'credential rotation did not change both values' >&2; exit 1; }
python3 - "$ROOT/etc/cygnus/node.json" "$ROOT/var/lib/cygnus/artifacts/tenant-0-secrets/cygnus/secrets/bootstrap.token" "$ROOT/var/lib/cygnus/artifacts/tenant-0-secrets/cygnus/secrets/session.key" <<'PY'
import json, pathlib, sys
node_bytes = pathlib.Path(sys.argv[1]).read_bytes()
node = json.loads(node_bytes)
app = node["apps"][0]
assert pathlib.Path(sys.argv[2]).read_bytes().hex().encode() not in node_bytes
assert pathlib.Path(sys.argv[3]).read_bytes().hex().encode() not in node_bytes
assert app["env"]["CYGNUS_CONSOLE_BOOTSTRAP_TOKEN_FILE"] == "/cygnus/secrets/bootstrap.token"
assert app["env"]["CYGNUS_CONSOLE_SESSION_KEY_FILE"] == "/cygnus/secrets/session.key"
PY

# 7. Noninteractive mode fails when required bundle input is absent.
expect_fail env CYGNUS_INSTALL_TEST_MODE=1 CYGNUS_INSTALL_TEST_ROOT="$ROOT/missing" PATH="$PATH" bash "$INSTALLER" --noninteractive --prefix "$ROOT/missing/bin"

# 8. Daemon readiness failure stops before either admin mutation.
ROOT_FAIL=$ROOT/failure
BUNDLE_FAIL=$ROOT_FAIL/release
mkdir -p "$BUNDLE_FAIL"
cp "$BUNDLE"/cygnus-* "$BUNDLE_FAIL/"
cp "$BUNDLE/bun" "$BUNDLE_FAIL/bun"
cp "$BUNDLE/SHA256SUMS" "$BUNDLE_FAIL/SHA256SUMS"
export CYGNUS_TEST_NO_READY=1
export CYGNUS_TEST_CTL_LOG="$ROOT_FAIL/ctl.log"
export CYGNUS_TEST_SYSTEMCTL_LOG="$ROOT_FAIL/systemctl.log"
export CYGNUS_TEST_READY_FILE="$ROOT_FAIL/run/cygnus/admin.sock"
export CYGNUS_TEST_TENANT_READY_FILE="$ROOT_FAIL/run/cygnus/tenant-0/admin.sock"
expect_fail bash "$INSTALLER" --noninteractive --bundle-dir "$BUNDLE_FAIL" \
  --prefix "$ROOT_FAIL/usr/local/bin" --config-dir "$ROOT_FAIL/etc/cygnus" \
  --state-dir "$ROOT_FAIL/var/lib/cygnus" --runtime-dir "$ROOT_FAIL/run/cygnus" \
  --listen 127.0.0.1:3300 --apps-domain apps.test
[[ ! -e ${CYGNUS_TEST_CTL_LOG:-} ]] || { echo 'admin mutation ran before readiness' >&2; exit 1; }

printf '%s\n' 'installer tests passed'
