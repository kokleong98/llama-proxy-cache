#!/usr/bin/env bash
#
# build-releases.sh — automated release builds + packaging for the
# mainstream Rust target platforms.
#
# For every target in the matrix the script:
#   1. checks the toolchain needed to build it (native build, a cross C
#      compiler, osxcross, or `cross`/docker) and skips the target with a
#      clear install hint when the toolchain is missing
#   2. runs `cargo build --release --target <t>`
#   3. packages the binary as dist/lpcache-<version>-<t>.tar.gz
#      (.zip for Windows targets) containing the binary, README.md,
#      LICENSE (if present) and a VERSION file
#   4. appends the archive checksum to dist/SHA256SUMS.txt
#
# Usage:
#   ./build-releases.sh [options]
#
# Options:
#   --list             print the supported target matrix and exit
#   --only <t1,t2,..>  build only these targets (overrides $TARGETS)
#   --out <dir>        output directory (default: ./dist)
#   --strict           exit non-zero if any target had to be skipped
#   -h, --help         show this help
#
# Environment:
#   TARGETS    comma-separated target list (default: full matrix below)
#   USE_CROSS  1 = always use `cross` (docker) for non-host targets
#              0 = never (default: auto — `cross` is used only when the
#              native toolchain for a target is missing)
#
# Toolchains needed per target (install separately; missing => skipped):
#   x86_64-unknown-linux-gnu     native build, no extra tools
#   aarch64-unknown-linux-gnu    aarch64-linux-gnu-gcc
#   x86_64-unknown-linux-musl    x86_64-linux-musl-gcc
#   aarch64-unknown-linux-musl   aarch64-linux-musl-gcc
#                                (musl-tools: apt install musl-tools, or the
#                                static cross compilers from musl.cc)
#   x86_64-pc-windows-gnu        x86_64-w64-mingw32-gcc   (mingw-w64)
#   x86_64-apple-darwin          native on a macOS host; on Linux:
#   aarch64-apple-darwin         osxcross with SDKROOT set
#   x86_64-pc-windows-msvc       native on a Windows host; elsewhere:
#   aarch64-pc-windows-msvc      `cross` + docker
#
# The project uses `rustls` (no OpenSSL), so each target only needs the
# `rust-std` component (added via `rustup target add`) and a C compiler.
#
# No-root toolchain setup (used on this machine, everything in ~/tools):
#   ~/tools/bin/<name> are the tool names this script looks for on PATH.
#   aarch64-linux-gnu-gcc  -> bash wrapper: zig cc -target aarch64-linux-gnu
#                             (zig cc drops the rust triple in --target,
#                             which zig cannot parse)
#   x86_64/aarch64-linux-musl-gcc -> symlinks to the musl.cc static cross
#                             toolchains (musl.cc/x86_64-linux-musl-cross.tgz
#                             targets x86_64; musl.cc/aarch64-linux-musl-cross.tgz
#                             targets aarch64)
#   x86_64-w64-mingw32-gcc -> shim around the self-contained llvm-mingw gcc
#                             (github.com/mstorsjo/llvm-mingw); at link time
#                             it adds -L paths for the mingw runtime and for
#                             libgcc.a/libgcc_eh.a extracted from Ubuntu's
#                             gcc-mingw-w64-x86-64-posix deb (apt-get
#                             download + dpkg -x, no root needed). rustc's
#                             windows-gnu link line references those libs.
#   zig: ziglang.org x86_64-linux tarball; llvm-mingw: ucrt release tarball
#   darwin targets need a macOS host (or osxcross + Xcode SDK); the msvc
#   targets need a Windows host or `cross` + docker.
#
# Exit codes:
#   0  at least one target built (skips allowed)
#   1  a build failed, nothing built, or --strict with skips
#
set -euo pipefail

# ---------------------------------------------------------------- constants

DEFAULT_TARGETS=(
  x86_64-unknown-linux-gnu
  x86_64-unknown-linux-musl
  aarch64-unknown-linux-gnu
  aarch64-unknown-linux-musl
  x86_64-apple-darwin
  aarch64-apple-darwin
  x86_64-pc-windows-gnu
  x86_64-pc-windows-msvc
  aarch64-pc-windows-msvc
)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

VERSION="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)"/\1/p' Cargo.toml | head -n1)"
[[ -n "$VERSION" ]] || { echo "error: could not read version from Cargo.toml" >&2; exit 1; }

HOST="$(rustc -vV | awk '/^host:/{print $2}')"

OUT_DIR="dist"
STRICT=0
TARGETS_OVERRIDE=""
USE_CROSS="${USE_CROSS:-auto}"

BUILT=()    # entries: "target<TAB>archive"
SKIPPED=()  # entries: "target<TAB>reason"
FAILED=()   # entries: "target<TAB>detail"

# ------------------------------------------------------------------- utils

log()  { printf '\033[1;34m[releases]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[releases]\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[releases]\033[0m %s\n' "$*" >&2; exit 1; }

have() { command -v "$1" >/dev/null 2>&1; }

# uppercase triple with - => _ (for CARGO_TARGET_* env vars)
up_triple() { echo "$1" | tr '[:lower:]' '[:upper:]' | tr '-' '_'; }
# lowercase triple with - => _ (for CC_* env vars)
lo_triple() { echo "$1" | tr '[:upper:]' '[:lower:]' | tr '-' '_'; }

sha256_of() {
  if have sha256sum; then sha256sum "$1" | awk '{print $1}'
  elif have shasum; then shasum -a 256 "$1" | awk '{print $1}'
  else die "no sha256sum/shasum available"; fi
}

usage() { sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'; }

list_targets() {
  echo "Supported targets (default matrix):"
  local t
  for t in "${DEFAULT_TARGETS[@]}"; do
    local mark=" "
    [[ "$t" == "$HOST" ]] && mark="*"
    printf '  %s %-28s %s\n' "$mark" "$t" "$([[ "$t" == "$HOST" ]] && echo '(host — native build)')"
  done
  echo
  echo "host: $HOST"
  echo "override with: ./build-releases.sh --only <t1,t2,...>   (or TARGETS=...)"
}

# ------------------------------------------------------------ argument parse

while [[ $# -gt 0 ]]; do
  case "$1" in
    --list)   list_targets; exit 0 ;;
    --only)   [[ $# -ge 2 ]] || die "--only needs a value"; TARGETS_OVERRIDE="$2"; shift ;;
    --out)    [[ $# -ge 2 ]] || die "--out needs a value"; OUT_DIR="$2"; shift ;;
    --strict) STRICT=1 ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1 (see --help)" ;;
  esac
  shift
done

TARGETS=()
if [[ -n "$TARGETS_OVERRIDE" ]]; then
  IFS=',' read -ra TARGETS <<< "$TARGETS_OVERRIDE"
elif [[ -n "${TARGETS:-}" ]]; then
  IFS=',' read -ra TARGETS <<< "$TARGETS"
else
  TARGETS=("${DEFAULT_TARGETS[@]}")
fi

mkdir -p "$OUT_DIR"
: > "$OUT_DIR/SHA256SUMS.txt"

log "lpcache $VERSION — host: $HOST — targets: ${TARGETS[*]}"

# ---------------------------------------------------------- target readiness

# resolve_toolchain <target>
#
# Sets the globals LINKER / CC_BIN (or USE_CROSS_THIS=1 when the `cross`
# fallback is chosen) and returns 0, or sets SKIP_REASON and returns 1.
resolve_toolchain() {
  local target="$1"
  local ut lt arch
  ut="$(up_triple "$target")"
  lt="$(lo_triple "$target")"
  arch="${target%%-*}"

  USE_CROSS_THIS=0
  LINKER=""
  CC_BIN=""

  local missing=""
  case "$target" in
    "$HOST")
      # native build
      ;;
    *-unknown-linux-gnu)
      missing="${arch}-linux-gnu-gcc"
      LINKER="$missing"; CC_BIN="$missing"
      ;;
    *-unknown-linux-musl)
      missing="${arch}-linux-musl-gcc"
      LINKER="$missing"; CC_BIN="$missing"
      ;;
    *-pc-windows-gnu)
      missing="${arch}-w64-mingw32-gcc"
      LINKER="$missing"; CC_BIN="$missing"
      ;;
    *-apple-darwin)
      if [[ "$HOST" == *-apple-darwin ]]; then
        # macOS host: rustc uses the system SDK for both darwin archs
        :
      else
        # osxcross: any <arch>-apple-darwin*<ver>-clang on PATH + SDKROOT
        local clangbin
        clangbin="$( { compgen -c || true; } | grep -E "^${arch}-apple-darwin([0-9]+)?-clang$" | head -n1 || true )"
        if [[ -z "$clangbin" || -z "${SDKROOT:-}" ]]; then
          missing="osxcross (${arch}-apple-darwin*clang + SDKROOT)"
        else
          LINKER="$clangbin"; CC_BIN="$clangbin"
        fi
      fi
      ;;
    *-pc-windows-msvc)
      if [[ "$HOST" == *-pc-windows-msvc ]]; then
        : # native build on a Windows (MSVC) host
      else
        missing="cross + docker (MSVC targets)"
      fi
      ;;
    *)
      missing="no known toolchain mapping"
      ;;
  esac

  if [[ -n "$missing" ]] && ! have "$missing"; then
    # `cross` fallback for anything buildable in docker
    local cross_fb=0
    if have docker; then
      if [[ "$USE_CROSS" == "1" ]]; then
        cross_fb=1
      elif [[ "$USE_CROSS" == "auto" ]] && have cross; then
        cross_fb=1
      fi
    fi
    if [[ $cross_fb -eq 1 ]]; then
      USE_CROSS_THIS=1
      return 0
    fi
    SKIP_REASON="$missing"
    return 1
  fi
  return 0
}

# ---------------------------------------------------------------- build one

build_target() {
  local target="$1"
  local ut lt
  ut="$(up_triple "$target")"
  lt="$(lo_triple "$target")"

  # make sure the rust-std component is installed (no-op when present)
  if have rustup; then
    rustup target add "$target" >/dev/null 2>&1 || true
    if ! rustup target list --installed 2>/dev/null | grep -qx "$target"; then
      SKIPPED+=("$target"$'\t'"rust-std component missing (run: rustup target add $target)")
      warn "SKIP  $target — rust-std component not available"
      return 0
    fi
  fi

  local skip_reason=""
  if ! resolve_toolchain "$target"; then
    skip_reason="$SKIP_REASON"
    SKIPPED+=("$target"$'\t'"missing toolchain: $skip_reason")
    warn "SKIP  $target — missing toolchain: $skip_reason"
    return 0
  fi

  log "BUILD $target"

  local -a cmd=()
  if [[ "${USE_CROSS_THIS:-0}" == "1" ]]; then
    log "      (via cross/docker)"
    cmd=(cross build --release --target "$target")
  else
    # per-target linker / C compiler for the cc crate
    [[ -n "${LINKER}" ]] && export "CARGO_TARGET_${ut}_LINKER=${LINKER}"
    [[ -n "${CC_BIN}" ]] && export "CC_${lt}=${CC_BIN}"
    cmd=(cargo build --release --target "$target")
  fi

  if ! "${cmd[@]}" 2>&1 | tail -n 25; then
    FAILED+=("$target"$'\t'"build failed")
    warn "FAIL  $target — build failed"
  else
    log "BUILT $target (cargo)"
  fi
  unset "CARGO_TARGET_${ut}_LINKER" 2>/dev/null || true
  [[ -n "${CC_BIN:-}" ]] && unset "CC_${lt}" 2>/dev/null || true
  return 0
}


# --------------------------------------------------------------- package one

package_target() {
  local target="$1"
  local bin="target/$target/release/lpcache"
  local exe=""
  case "$target" in
    *-pc-windows-*) exe=".exe" ;;
  esac
  if [[ ! -f "$bin$exe" ]]; then
    FAILED+=("$target"$'\t'"binary not found: $bin$exe")
    warn "FAIL  $target — binary not found: $bin$exe"
    return 0
  fi

  local pkg="$OUT_DIR/lpcache-$VERSION-$target"
  rm -rf "$pkg"
  mkdir -p "$pkg"
  cp "$bin$exe" "$pkg/lpcache$exe"
  cp README.md "$pkg/" 2>/dev/null || true
  [[ -f LICENSE ]] && cp LICENSE "$pkg/"
  printf '%s\n' "$VERSION" > "$pkg/VERSION"

  local archive
  case "$target" in
    *-pc-windows-*)
      if have zip; then
        ( cd "$OUT_DIR" && zip -qr "lpcache-$VERSION-$target.zip" "lpcache-$VERSION-$target" )
        archive="lpcache-$VERSION-$target.zip"
      else
        ( cd "$OUT_DIR" && tar -czf "lpcache-$VERSION-$target.tar.gz" "lpcache-$VERSION-$target" )
        archive="lpcache-$VERSION-$target.tar.gz"
      fi
      ;;
    *)
      ( cd "$OUT_DIR" && tar -czf "lpcache-$VERSION-$target.tar.gz" "lpcache-$VERSION-$target" )
      archive="lpcache-$VERSION-$target.tar.gz"
      ;;
  esac

  local sum
  sum="$(sha256_of "$OUT_DIR/$archive")"
  printf '%s  %s\n' "$sum" "$archive" >> "$OUT_DIR/SHA256SUMS.txt"
  rm -rf "$pkg"

  BUILT+=("$target"$'\t'"$archive")
  log "OK    $target -> $OUT_DIR/$archive"
}

# --------------------------------------------------------------------- main

t=""
for t in "${TARGETS[@]}"; do
  build_target "$t"
done

# package every target that was neither skipped nor failed
for t in "${TARGETS[@]}"; do
  skip=0; fail=0
  for s in ${SKIPPED[@]+"${SKIPPED[@]}"}; do
    [[ "${s%%$'\t'*}" == "$t" ]] && skip=1
  done
  for f in ${FAILED[@]+"${FAILED[@]}"}; do
    [[ "${f%%$'\t'*}" == "$t" ]] && fail=1
  done
  if [[ $skip -eq 0 && $fail -eq 0 ]]; then
    package_target "$t"
  fi
done

# ------------------------------------------------------------------ summary

echo
log "================ release summary ================"
printf '%-28s  %s\n' "TARGET" "RESULT"
for t in "${TARGETS[@]}"; do
  status="BUILT"; archive=""
  for b in ${BUILT[@]+"${BUILT[@]}"}; do
    [[ "${b%%$'\t'*}" == "$t" ]] && archive="${b#*$'\t'}"
  done
  for s in ${SKIPPED[@]+"${SKIPPED[@]}"}; do
    [[ "${s%%$'\t'*}" == "$t" ]] && status="SKIPPED — ${s#*$'\t'}"
  done
  for f in ${FAILED[@]+"${FAILED[@]}"}; do
    [[ "${f%%$'\t'*}" == "$t" ]] && status="FAILED — ${f#*$'\t'}"
  done
  [[ -n "$archive" ]] && status="$status -> $OUT_DIR/$archive"
  printf '%-28s  %s\n' "$t" "$status"
done
log "artifacts in: $OUT_DIR  (checksums: $OUT_DIR/SHA256SUMS.txt)"

if [[ ${#FAILED[@]} -gt 0 || ${#BUILT[@]} -eq 0 ]]; then
  die "${#FAILED[@]} target(s) failed, ${#BUILT[@]} built"
fi
if [[ $STRICT -eq 1 && ${#SKIPPED[@]} -gt 0 ]]; then
  die "--strict: ${#SKIPPED[@]} target(s) skipped"
fi
log "done: ${#BUILT[@]} built, ${#SKIPPED[@]} skipped"

