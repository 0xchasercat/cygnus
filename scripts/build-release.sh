#!/usr/bin/env bash
# Build a self-contained, local Cygnus release bundle.
#
# The bundle contains the Linux release binaries, the Bun engine used to run
# Tenant Zero, and a rooted archive of the built console.  No network bootstrap
# scripts are used; cargo and bun perform their normal dependency resolution.
set -Eeuo pipefail
IFS=$'\n\t'
shopt -s nullglob dotglob

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

output_dir=${CYGNUS_RELEASE_OUTPUT_DIR:-$REPO_ROOT/release}
target=${CYGNUS_RELEASE_TARGET:-x86_64-unknown-linux-gnu}
release_version=${CYGNUS_RELEASE_VERSION:-0.1.0}
console_dir=${CYGNUS_CONSOLE_DIR:-$REPO_ROOT/console}
cargo_bin=${CYGNUS_CARGO_BIN:-${CARGO:-cargo}}
bun_bin=${CYGNUS_BUN_BIN:-${BUN_BIN:-${BUN:-}}}
tar_bin=${CYGNUS_TAR_BIN:-${TAR:-tar}}
bun_version=${CYGNUS_BUN_VERSION:-}
source_date_epoch=${SOURCE_DATE_EPOCH:-0}
cargo_target_dir=${CARGO_TARGET_DIR:-$REPO_ROOT/target}

usage() {
  cat <<'EOF'
Usage: scripts/build-release.sh [options]

Build a Linux release bundle in OUTPUT_DIR. Paths may be absolute or relative
(the output path is relative to the caller's working directory; source paths are
relative to the repository when supplied relative to this script).

Options:
  --output-dir DIR       Bundle output directory (default: ./release)
  --target TRIPLE        Rust target triple (default: x86_64-unknown-linux-gnu)
  --version VERSION      Release version label (default: 0.1.0)
  --console-dir DIR      Console source directory (default: ./console)
  --cargo-bin PATH       cargo executable (default: cargo)
  --bun-bin PATH         Bun executable (default: bun on PATH)
  --tar-bin PATH         tar executable (default: tar)
  --bun-version VERSION  Require this Bun engine version (default: detected)
  --source-date-epoch N  Archive timestamp where tar supports --mtime (default: 0)
  --help                 Show this help

Environment aliases are accepted for automation: CYGNUS_RELEASE_OUTPUT_DIR,
CYGNUS_RELEASE_TARGET, CYGNUS_RELEASE_VERSION, CYGNUS_CONSOLE_DIR,
CYGNUS_CARGO_BIN/CARGO, CYGNUS_BUN_BIN/BUN_BIN/BUN, CYGNUS_TAR_BIN/TAR,
CYGNUS_BUN_VERSION, CARGO_TARGET_DIR, and SOURCE_DATE_EPOCH.
EOF
}

fail() {
  printf 'build-release: error: %s\n' "$*" >&2
  exit 1
}

resolve_path() {
  local path=$1 base=$2
  case $path in
    /*) printf '%s\n' "$path" ;;
    *) printf '%s/%s\n' "$base" "$path" ;;
  esac
}

resolve_command() {
  local value=$1 resolved
  if [[ $value == */* ]]; then
    [[ -x $value && ! -d $value ]] || fail "required executable is not executable: $value"
    printf '%s\n' "$value"
    return
  fi
  resolved=$(command -v -- "$value" 2>/dev/null || true)
  [[ -n $resolved && -x $resolved ]] || fail "required tool is missing: $value"
  printf '%s\n' "$resolved"
}

while (($#)); do
  case $1 in
    --output-dir|--output)
      (($# >= 2)) || fail "$1 needs a value"
      output_dir=$2
      shift 2
      ;;
    --target)
      (($# >= 2)) || fail "--target needs a value"
      target=$2
      shift 2
      ;;
    --version)
      (($# >= 2)) || fail "--version needs a value"
      release_version=$2
      shift 2
      ;;
    --console-dir)
      (($# >= 2)) || fail "--console-dir needs a value"
      console_dir=$2
      shift 2
      ;;
    --cargo-bin)
      (($# >= 2)) || fail "--cargo-bin needs a value"
      cargo_bin=$2
      shift 2
      ;;
    --bun-bin)
      (($# >= 2)) || fail "--bun-bin needs a value"
      bun_bin=$2
      shift 2
      ;;
    --tar-bin)
      (($# >= 2)) || fail "--tar-bin needs a value"
      tar_bin=$2
      shift 2
      ;;
    --bun-version)
      (($# >= 2)) || fail "--bun-version needs a value"
      bun_version=$2
      shift 2
      ;;
    --source-date-epoch)
      (($# >= 2)) || fail "--source-date-epoch needs a value"
      source_date_epoch=$2
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      fail "unknown option: $1 (use --help for usage)"
      ;;
  esac
done

[[ $target =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || fail "invalid Rust target: $target"
[[ $release_version =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]] || fail "invalid release version: $release_version"
[[ $source_date_epoch =~ ^[0-9]+$ ]] || fail "source date epoch must be a non-negative integer: $source_date_epoch"

output_dir=$(resolve_path "$output_dir" "$PWD")
console_dir=$(resolve_path "$console_dir" "$REPO_ROOT")
cargo_target_dir=$(resolve_path "$cargo_target_dir" "$REPO_ROOT")

[[ -d $console_dir && ! -L $console_dir ]] || fail "console directory is not a real directory: $console_dir"
[[ -f $console_dir/server.js && ! -L $console_dir/server.js ]] || fail "console runtime file is missing: $console_dir/server.js"
[[ -f $console_dir/admin-client.js && ! -L $console_dir/admin-client.js ]] || fail "console runtime file is missing: $console_dir/admin-client.js"
[[ -f $console_dir/bun.lock || -f $console_dir/bun.lockb ]] || fail "console lockfile is missing in $console_dir"

cargo_bin=$(resolve_command "$cargo_bin")
if [[ -z $bun_bin ]]; then
  bun_bin=$(command -v bun 2>/dev/null || true)
fi
[[ -n $bun_bin ]] || fail "Bun is missing; pass --bun-bin PATH or set BUN_BIN"
bun_bin=$(resolve_command "$bun_bin")
tar_bin=$(resolve_command "$tar_bin")
file_bin=$(command -v file 2>/dev/null || true)
[[ -n $file_bin ]] || fail "file is required to validate the Bun target"
bun_format=$("$file_bin" -Lb -- "$bun_bin") || fail "could not inspect Bun executable: $bun_bin"
case $target in
  x86_64-*-linux-*)
    [[ $bun_format == *ELF* && $bun_format == *x86-64* ]] || fail "Bun executable does not target Linux x86-64: $bun_format"
    ;;
  aarch64-*-linux-*|arm64-*-linux-*)
    [[ $bun_format == *ELF* && ( $bun_format == *aarch64* || $bun_format == *ARM\ aarch64* ) ]] || fail "Bun executable does not target Linux aarch64: $bun_format"
    ;;
  *-linux-*)
    [[ $bun_format == *ELF* ]] || fail "Bun executable does not target Linux: $bun_format"
    ;;
  aarch64-apple-darwin|arm64-apple-darwin)
    [[ $bun_format == *Mach-O* && ( $bun_format == *arm64* || $bun_format == *aarch64* ) ]] || fail "Bun executable does not target macOS aarch64: $bun_format"
    ;;
  *) fail "release target is not supported: $target" ;;
esac
if command -v sha256sum >/dev/null 2>&1; then
  hash_tool=$(command -v sha256sum)
else
  hash_tool=$(command -v shasum 2>/dev/null || true)
  [[ -n $hash_tool ]] || fail "sha256sum or shasum is required to write SHA256SUMS"
fi

reported_bun_version=$("$bun_bin" --version 2>/dev/null) || fail "could not execute Bun for --version: $bun_bin"
[[ $reported_bun_version =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]] || fail "Bun returned an invalid version: $reported_bun_version"
if [[ -z $bun_version ]]; then
  bun_version=$reported_bun_version
else
  [[ $bun_version =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]] || fail "invalid Bun version: $bun_version"
  [[ $bun_version == "$reported_bun_version" ]] || fail "Bun version mismatch: requested $bun_version, executable reports $reported_bun_version"
fi

if [[ -e $output_dir || -L $output_dir ]]; then
  [[ -d $output_dir && ! -L $output_dir ]] || fail "output path is not a real directory: $output_dir"
else
  mkdir -p -- "$output_dir"
fi

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/cygnus-release.XXXXXX")
cleanup() { rm -rf -- "$work_dir"; }
trap cleanup EXIT
stage=$work_dir/stage
bundle_out=$work_dir/bundle
mkdir -p -- "$stage/opt/cygnus-console" "$bundle_out"

printf 'build-release: version=%s target=%s bun=%s output=%s\n' \
  "$release_version" "$target" "$bun_version" "$output_dir" >&2

printf 'build-release: installing frozen console dependencies and building dist/\n' >&2
(
  cd -- "$console_dir"
  "$bun_bin" install --frozen-lockfile
  "$bun_bin" run build
)

# Keep cargo's target directory explicit, so output lookup never depends on the
# caller's current directory or an implicit cargo configuration.
CARGO_TARGET_DIR="$cargo_target_dir" "$cargo_bin" build --release --locked --target "$target" \
  --manifest-path "$REPO_ROOT/Cargo.toml" -p cygnus-daemon --bin cygnus-daemon --bin cygnus

if [[ $target == *-linux-* ]]; then
  CARGO_TARGET_DIR="$cargo_target_dir" "$cargo_bin" build --release --locked --target "$target" \
    --manifest-path "$REPO_ROOT/Cargo.toml" -p cygnus-init --bin cygnus-init
fi

copy_binary() {
  local name=$1 source=$2 destination=$bundle_out/$1
  [[ -f $source && ! -L $source ]] || fail "release artifact is missing or not regular: $source"
  [[ -x $source ]] || fail "release artifact is not executable: $source"
  cp -p -- "$source" "$destination"
  chmod 0755 "$destination"
}

release_bin_dir=$cargo_target_dir/$target/release
copy_binary cygnus-daemon "$release_bin_dir/cygnus-daemon"
copy_binary cygnus "$release_bin_dir/cygnus"
if [[ $target == *-linux-* ]]; then
  copy_binary cygnus-init "$release_bin_dir/cygnus-init"
fi
copy_binary bun "$bun_bin"

copy_tree() {
  local source=$1 destination=$2 entry name
  [[ -d $source && ! -L $source ]] || fail "console tree is missing or not a real directory: $source"
  mkdir -p -- "$destination"
  for entry in "$source"/* "$source"/.[!.]* "$source"/..?*; do
    [[ -e $entry || -L $entry ]] || continue
    name=${entry##*/}
    if [[ -L $entry ]]; then
      fail "console archive cannot contain symlinks: $entry"
    elif [[ -d $entry ]]; then
      copy_tree "$entry" "$destination/$name"
    elif [[ -f $entry ]]; then
      cp -p -- "$entry" "$destination/$name"
    else
      fail "console archive contains unsupported file: $entry"
    fi
  done
}

cp -p -- "$console_dir/server.js" "$stage/opt/cygnus-console/server.js"
cp -p -- "$console_dir/admin-client.js" "$stage/opt/cygnus-console/admin-client.js"
copy_tree "$console_dir/dist" "$stage/opt/cygnus-console/dist"
[[ -f "$stage/opt/cygnus-console/dist/index.html" ]] || fail "console build did not produce dist/index.html"

archive_tmp=$work_dir/cygnus-console.tar
tar_help=$("$tar_bin" --help 2>&1 || true)
tar_options=(-cf "$archive_tmp" -C "$stage")
# GNU tar and newer bsdtar expose some of these knobs.  Use each only when the
# selected implementation advertises it, retaining portability on macOS.
[[ $tar_help == *--sort* ]] && tar_options+=(--sort=name)
[[ $tar_help == *--mtime* ]] && tar_options+=(--mtime="@$source_date_epoch")
[[ $tar_help == *--owner* ]] && tar_options+=(--owner=0)
[[ $tar_help == *--group* ]] && tar_options+=(--group=0)
[[ $tar_help == *numeric-owner* ]] && tar_options+=(--numeric-owner)
TZ=UTC "$tar_bin" "${tar_options[@]}" opt/cygnus-console
[[ -s $archive_tmp ]] || fail "tar did not produce cygnus-console.tar"
cp -p -- "$archive_tmp" "$bundle_out/cygnus-console.tar"

hash_file() {
  local file=$1 line
  if [[ ${hash_tool##*/} == sha256sum ]]; then
    line=$("$hash_tool" -- "$file")
  else
    line=$("$hash_tool" -a 256 -- "$file")
  fi
  printf '%s\n' "${line%% *}"
}

sums_tmp=$bundle_out/SHA256SUMS
: >"$sums_tmp"

bundle_files=(cygnus-daemon cygnus bun cygnus-console.tar)
if [[ $target == *-linux-* ]]; then
  bundle_files+=(cygnus-init)
fi

for name in "${bundle_files[@]}"; do
  [[ -f $bundle_out/$name && ! -L $bundle_out/$name ]] || fail "bundle artifact is missing: $name"
  sum=$(hash_file "$bundle_out/$name")
  [[ $sum =~ ^[[:xdigit:]]{64}$ ]] || fail "checksum tool returned an invalid SHA-256 for $name"
  printf '%s  %s\n' "${sum,,}" "$name" >>"$sums_tmp"
done

for name in "${bundle_files[@]}" SHA256SUMS; do
  destination=$output_dir/$name
  if [[ -L $destination || ( -e $destination && ! -f $destination ) ]]; then
    fail "output artifact destination is not a regular file: $destination"
  fi
  mv -f -- "$bundle_out/$name" "$destination"
done

printf 'build-release: wrote %s (Bun %s, Rust target %s)\n' "$output_dir" "$bun_version" "$target" >&2
