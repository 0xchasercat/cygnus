#!/usr/bin/env bash
# Focused fixture test for scripts/build-release.sh.  Fake cargo and Bun avoid
# compiling the Rust workspace while exercising the complete bundle assembly.
set -Eeuo pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
BUILDER=$SCRIPT_DIR/build-release.sh
ROOT=$(mktemp -d "${TMPDIR:-/tmp}/cygnus-release-test.XXXXXX")
trap 'rm -rf -- "$ROOT"' EXIT

FAKEBIN=$ROOT/bin
CONSOLE=$ROOT/console
OUTPUT=$ROOT/release
CARGO_TARGET=$ROOT/cargo-target
mkdir -p "$FAKEBIN" "$CONSOLE" "$CONSOLE/dist"

cat >"$FAKEBIN/cargo" <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail
log=${FAKE_CARGO_LOG:?}
printf '%s\n' "$*" >>"$log"
target=''
bins=()
while (($#)); do
  case $1 in
    --target) target=$2; shift 2 ;;
    --bin) bins+=("$2"); shift 2 ;;
    *) shift ;;
  esac
done
[[ -n $target ]] || { echo 'fake cargo: --target missing' >&2; exit 1; }
[[ ${#bins[@]} -gt 0 ]] || { echo 'fake cargo: --bin missing' >&2; exit 1; }
out=${CARGO_TARGET_DIR:?}/$target/release
mkdir -p "$out"
for bin in "${bins[@]}"; do
  printf '#!/usr/bin/env sh\nexit 0\n' >"$out/$bin"
  chmod 0755 "$out/$bin"
done
EOF
chmod 0755 "$FAKEBIN/cargo"

cat >"$FAKEBIN/bun" <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail
case ${1:-} in
  --version)
    printf '%s\n' '1.2.3-fixture'
    ;;
  install)
    [[ ${2:-} == --frozen-lockfile ]] || { echo 'fake bun: install was not frozen' >&2; exit 1; }
    : >.frozen-install
    ;;
  run)
    [[ ${2:-} == build ]] || { echo 'fake bun: build command missing' >&2; exit 1; }
    mkdir -p dist/assets
    printf '%s\n' '<!doctype html><title>fixture</title>' >dist/index.html
    printf '%s\n' 'fixture asset' >dist/assets/app.css
    ;;
  *)
    echo "fake bun: unexpected command: $*" >&2
    exit 1
    ;;
esac
EOF
chmod 0755 "$FAKEBIN/bun"

cat >"$FAKEBIN/file" <<'EOF'
#!/usr/bin/env sh
printf '%s\n' "${FAKE_FILE_FORMAT:-ELF 64-bit LSB executable, x86-64}"
EOF
chmod 0755 "$FAKEBIN/file"
export PATH="$FAKEBIN:$PATH"

printf '%s\n' '{"name":"fixture-console"}' >"$CONSOLE/package.json"
printf '%s\n' '# fixture lock' >"$CONSOLE/bun.lock"
printf '%s\n' 'export default {};' >"$CONSOLE/server.js"
printf '%s\n' 'export function adminRequest() {}' >"$CONSOLE/admin-client.js"

export FAKE_CARGO_LOG=$ROOT/cargo.log
export CARGO_TARGET_DIR=$CARGO_TARGET
bash "$BUILDER" \
  --output-dir "$OUTPUT" \
  --target x86_64-unknown-linux-gnu-fixture \
  --version 9.9.9-fixture \
  --console-dir "$CONSOLE" \
  --cargo-bin "$FAKEBIN/cargo" \
  --bun-bin "$FAKEBIN/bun"

[[ -f $OUTPUT/SHA256SUMS ]] || { echo 'SHA256SUMS missing' >&2; exit 1; }
for name in cygnus-daemon cygnus cygnus-init bun; do
  [[ -f $OUTPUT/$name && ! -L $OUTPUT/$name ]] || { echo "missing binary: $name" >&2; exit 1; }
  [[ -x $OUTPUT/$name ]] || { echo "binary is not executable: $name" >&2; exit 1; }
  if mode=$(stat -c '%a' "$OUTPUT/$name" 2>/dev/null); then :; else mode=$(stat -f '%Lp' "$OUTPUT/$name"); fi
  [[ $mode == 755 ]] || { echo "binary mode is not 0755: $name ($mode)" >&2; exit 1; }
done
[[ -f $OUTPUT/cygnus-console.tar && ! -L $OUTPUT/cygnus-console.tar ]] || { echo 'console archive missing' >&2; exit 1; }

# Check that SHA256SUMS is strict: exactly the five contract entries, with no
# duplicate/extra paths, and every digest matches the corresponding artifact.
declare -A checksums=()
line_count=0
while IFS= read -r line || [[ -n $line ]]; do
  [[ -n $line ]] || { echo 'blank checksum line' >&2; exit 1; }
  read -r sum name extra <<<"$line"
  [[ -n ${sum:-} && -n ${name:-} && -z ${extra:-} ]] || { echo "malformed checksum line: $line" >&2; exit 1; }
  [[ $sum =~ ^[[:xdigit:]]{64}$ ]] || { echo "invalid checksum: $name" >&2; exit 1; }
  case $name in
    cygnus-daemon|cygnus|cygnus-init|bun|cygnus-console.tar) ;;
    *) echo "unexpected checksum path: $name" >&2; exit 1 ;;
  esac
  [[ -z ${checksums[$name]+present} ]] || { echo "duplicate checksum: $name" >&2; exit 1; }
  checksums[$name]=${sum,,}
  line_count=$((line_count + 1))
done <"$OUTPUT/SHA256SUMS"
[[ $line_count == 5 ]] || { echo "expected five checksum lines, got $line_count" >&2; exit 1; }
for name in cygnus-daemon cygnus cygnus-init bun cygnus-console.tar; do
  [[ -n ${checksums[$name]+present} ]] || { echo "checksum missing: $name" >&2; exit 1; }
  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum -- "$OUTPUT/$name")
  else
    actual=$(shasum -a 256 -- "$OUTPUT/$name")
  fi
  actual=${actual%% *}
  [[ ${checksums[$name]} == "${actual,,}" ]] || { echo "checksum mismatch: $name" >&2; exit 1; }
done

# Validate every archive member's root/type, then extract it and verify the
# runtime files and built dist tree are present.
entry_count=0
while IFS= read -r entry || [[ -n $entry ]]; do
  [[ -n $entry ]] || { echo 'empty tar member' >&2; exit 1; }
  normalized=${entry%/}
  case $normalized in
    /*|..|../*|*/../*|*/./*) echo "unsafe tar member: $entry" >&2; exit 1 ;;
    opt|opt/cygnus-console|opt/cygnus-console/*) ;;
    *) echo "unrooted tar member: $entry" >&2; exit 1 ;;
  esac
  entry_count=$((entry_count + 1))
done < <(tar -tf "$OUTPUT/cygnus-console.tar")
[[ $entry_count -ge 5 ]] || { echo "archive is unexpectedly small: $entry_count members" >&2; exit 1; }
while IFS= read -r listing || [[ -n $listing ]]; do
  [[ -n $listing ]] || continue
  type=${listing:0:1}
  [[ $type == - || $type == d ]] || { echo "archive member is not a regular file/directory: $listing" >&2; exit 1; }
done < <(tar -tvf "$OUTPUT/cygnus-console.tar")

EXTRACT=$ROOT/extracted
mkdir -p "$EXTRACT"
tar -xf "$OUTPUT/cygnus-console.tar" -C "$EXTRACT"
[[ -f $EXTRACT/opt/cygnus-console/server.js ]] || { echo 'server.js missing from archive' >&2; exit 1; }
[[ -f $EXTRACT/opt/cygnus-console/admin-client.js ]] || { echo 'admin-client.js missing from archive' >&2; exit 1; }
[[ -f $EXTRACT/opt/cygnus-console/dist/index.html ]] || { echo 'dist/index.html missing from archive' >&2; exit 1; }
[[ -f $EXTRACT/opt/cygnus-console/dist/assets/app.css ]] || { echo 'dist asset missing from archive' >&2; exit 1; }
[[ -f "$CONSOLE/.frozen-install" ]] || { echo 'Bun frozen install was not exercised' >&2; exit 1; }

[[ $(wc -l <"$FAKE_CARGO_LOG") -eq 2 ]] || { echo 'expected two focused cargo package builds' >&2; exit 1; }

# A Linux bundle must never silently contain a host-platform Bun binary.
if env FAKE_FILE_FORMAT='Mach-O 64-bit executable arm64' bash "$BUILDER" \
  --output-dir "$ROOT/wrong-platform" \
  --target x86_64-unknown-linux-gnu-fixture \
  --console-dir "$CONSOLE" \
  --cargo-bin "$FAKEBIN/cargo" \
  --bun-bin "$FAKEBIN/bun" >/dev/null 2>&1; then
  echo 'builder accepted a non-Linux Bun executable' >&2
  exit 1
fi
# Daemon/CLI stay on the requested glibc triple; cygnus-init is rebuilt for
# the matching musl triple so the cage PID 1 is self-contained.
gnu_seen=0
musl_seen=0
while IFS= read -r cargo_command || [[ -n $cargo_command ]]; do
  if [[ $cargo_command == *'--release --locked --target x86_64-unknown-linux-gnu-fixture'* ]]; then
    gnu_seen=$((gnu_seen + 1))
  fi
  if [[ $cargo_command == *'--release --locked --target x86_64-unknown-linux-musl-fixture'* ]]; then
    musl_seen=$((musl_seen + 1))
  fi
done <"$FAKE_CARGO_LOG"
[[ $gnu_seen == 1 ]] || { echo "expected one glibc cargo build, got $gnu_seen" >&2; exit 1; }
[[ $musl_seen == 1 ]] || { echo "expected one musl cargo build for cygnus-init, got $musl_seen" >&2; exit 1; }
printf '%s\n' 'release bundle fixture test passed'
