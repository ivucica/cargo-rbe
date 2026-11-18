#!/usr/bin/env bash
# Expose a suitable Rust toolchain on PATH for unmodified scripts (including Claude-provided
# test scripts from drops). If the system rustc already satisfies the required version, nothing
# is installed and the system toolchain is used as-is; otherwise deploy a pinned, sha256-verified
# toolchain to a prefix.
#
# Usage:
#   rust-toolchain.sh [--prefix DIR] [--version VER] deploy
#   rust-toolchain.sh [--prefix DIR] [--version VER] env        # print export lines for eval
#   rust-toolchain.sh [--prefix DIR] [--version VER] run CMD... # exec CMD with the toolchain on PATH
#
# Prefix resolution: --prefix > RBE_RUST_PREFIX > XDG cache basedir/.rust-toolchain-cache
# (basedir resolved via python3-xdg/pyxdg when installed, else $XDG_CACHE_HOME or ~/.cache).
# Hash verification: built-in pin for the pinned version below; other versions verify against
# static.rust-lang.org's .sha256 sidecar unless RBE_RUST_SHA256 is set.
# Download/staging scratch: $RBE_RUST_SCRATCH, else $TMPDIR, else /tmp (never under the prefix).
# Components: everything in the dist tarball except $RBE_RUST_WITHOUT (default rust-docs — ~780 MB of HTML).
# Free disk space is checked against expected sizes before downloading; fails early with numbers.
# An existing install dir is only deleted when --replace is given (never automatically).
# Tarball download cache: $RBE_RUST_TARBALL_DIR, else <scratch>/rust-toolchain-tarballs — reused
# across runs (always re-verified); point it at a NAS path for persistence/sharing.

set -euo pipefail

PREFIX=""
VERSION="${RBE_RUST_VERSION:-1.85.1}"   # project minimum (Cargo.toml rust-version); set higher, e.g. 1.91.1, to force a specific version
WITHOUT="${RBE_RUST_WITHOUT:-rust-docs}"   # components to skip; rust-docs is ~780 MB of HTML, useless for building
REPLACE="${RBE_RUST_REPLACE:-0}"           # 1 = allowed to delete an existing install dir first (never automatic)
CMD=""
STAGE=""

cleanup() {
  [ -z "$STAGE" ] || rm -rf "$STAGE"
  return 0
}
trap cleanup EXIT

# XDG cache basedir: pyxdg (python3-xdg) when installed, else the spec fallback.
# Python is used ONLY for this resolution; everything else in this script is bash.
default_prefix() {
  python3 - <<'PYEOF'
import os
try:
    from xdg.BaseDirectory import xdg_cache_home
    print(xdg_cache_home())
except Exception:
    print(os.environ.get("XDG_CACHE_HOME") or os.path.join(os.environ.get("HOME", "/root"), ".cache"))
PYEOF
}

# System toolchain check: if the rustc on PATH already satisfies VERSION, no install is needed.
system_rustc_version() {
  command -v rustc >/dev/null 2>&1 || return 1
  rustc --version 2>/dev/null | awk '{print $2}'
}

use_system_toolchain() { # true (and prints why) when the system rustc is >= VERSION
  local sys
  sys="$(system_rustc_version)" || return 1
  if [ "$(printf '%s\n%s\n' "$sys" "$VERSION" | sort -V | tail -n1)" = "$sys" ]; then
    echo "system toolchain $(rustc --version) satisfies >= ${VERSION} — no install needed (set RBE_RUST_VERSION higher to force a specific version)" >&2
    return 0
  fi
  return 1
}

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX="$2"; shift 2 ;;
    --version) VERSION="$2"; shift 2 ;;
    --replace) REPLACE=1; shift ;;
    deploy|env|run) CMD="$1"; shift; break ;;
    *) echo "usage: rust-toolchain.sh [--prefix DIR] [--version VER] [--replace] {deploy|env|run CMD...}" >&2; exit 2 ;;
  esac
done

[ -n "$PREFIX" ] || PREFIX="${RBE_RUST_PREFIX:-$(default_prefix)/.rust-toolchain-cache}"

case "$(uname -m)" in
  x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
  aarch64) TARGET="aarch64-unknown-linux-gnu" ;;
  *) echo "unsupported arch: $(uname -m)" >&2; exit 1 ;;
esac

# Pinned downloads (see the hash table in SKILL.md). The single dist tarball contains both
# rustc and cargo — there is no separate cargo download.
pinned_sha256() {
  case "${VERSION}-${TARGET}" in
    "1.85.1-x86_64-unknown-linux-gnu") echo "b7202563a52b47f575b284a5a4794fafd688e39bfe8fd855b5e80129e671cb7f" ;;
    "1.91.1-x86_64-unknown-linux-gnu") echo "1c955c040dd087e4751d15588ddec288b4208bea16f8ec5046c164877e55fff7" ;;
    *) return 1 ;;
  esac
}

TARBALL="rust-${VERSION}-${TARGET}.tar.gz"
URL="https://static.rust-lang.org/dist/${TARBALL}"
BIN_DIR="${PREFIX}/bin"

deploy() {
  if use_system_toolchain; then return 0; fi
  if [ -x "${BIN_DIR}/rustc" ] && [ "$("${BIN_DIR}/rustc" --version | awk '{print $2}')" = "${VERSION}" ]; then
    echo "already deployed: rust ${VERSION} at ${BIN_DIR}" >&2
    return 0
  fi
  if [ -e "${BIN_DIR}" ] && [ ! -L "${BIN_DIR}" ]; then
    echo "refusing to overwrite non-symlink directory: ${BIN_DIR}" >&2
    exit 1
  fi
  # Download/staging scratch lives on /tmp (or $RBE_RUST_SCRATCH), NOT under the prefix:
  # staging next to the install would double peak usage on whatever partition holds it.
  SCRATCH="${RBE_RUST_SCRATCH:-${TMPDIR:-/tmp}}"
  [ -d "$SCRATCH" ] || { echo "scratch dir $SCRATCH does not exist (set RBE_RUST_SCRATCH)" >&2; exit 1; }
  # An existing install (or partial one) is NEVER deleted automatically — even under .cache a user
  # may have modifications they care about. Refuse and point at --replace; freeing it before the
  # space check is what makes re-deploys possible on tight partitions.
  # AGENT RULE: when this refusal fires, you MUST ask the user via the question tool before
  # re-running with --replace. Deletion of an existing install dir is never implied consent —
  # not for a partial/failed-looking one, not under .cache. Never assume deletion is free.
  DEST="${PREFIX}/rust-${VERSION}-${TARGET}"
  if [ -e "$DEST" ]; then
    if [ "${REPLACE:-0}" != "1" ]; then
      echo "existing install at ${DEST} ($(du -sh "$DEST" 2>/dev/null | cut -f1)) — re-run with --replace to delete it, or move/delete it yourself (AGENT: ask the user via the question tool first; no implied consent for deletion)" >&2
      exit 1
    fi
    rm -rf "$DEST"
  fi
  # Fail early on tight disks (measured 2026-11-18: full install 1.6 GB incl. 782 MB rust-docs;
  # without docs ~850 MB; scratch needs tarball ~354 MB + extracted tree ~1.6 GB).
  avail_mb() { df -Pk "$1" | awk 'NR==2 {print int($4/1024)}'; }
  NEED_PREFIX_MB=900
  case ",$WITHOUT," in *",rust-docs,"*) ;; *) NEED_PREFIX_MB=$((NEED_PREFIX_MB + 800)) ;; esac
  HAVE_PREFIX_MB="$(avail_mb "$(dirname "$PREFIX")")"
  if [ "$HAVE_PREFIX_MB" -lt "$NEED_PREFIX_MB" ]; then
    echo "not enough space at ${PREFIX}: need ~${NEED_PREFIX_MB} MB, have ${HAVE_PREFIX_MB} MB (set RBE_RUST_PREFIX to a roomier location)" >&2
    exit 1
  fi
  HAVE_SCRATCH_MB="$(avail_mb "$SCRATCH")"
  if [ "$HAVE_SCRATCH_MB" -lt 2048 ]; then
    echo "not enough scratch space at ${SCRATCH}: need ~2048 MB, have ${HAVE_SCRATCH_MB} MB (set RBE_RUST_SCRATCH)" >&2
    exit 1
  fi
  mkdir -p "${PREFIX}"
  STAGE="$(mktemp -d "${SCRATCH}/rust-toolchain.stage.XXXXXX")"
  # Tarball cache: reuse an already-downloaded tarball across runs; always re-verified.
  TARBALL_CACHE_DIR="${RBE_RUST_TARBALL_DIR:-${SCRATCH}/rust-toolchain-tarballs}"
  CACHED="${TARBALL_CACHE_DIR}/${TARBALL}"
  mkdir -p "${TARBALL_CACHE_DIR}"
  if [ ! -f "$CACHED" ]; then
    echo "downloading ${URL} (cached at ${CACHED})" >&2
    curl -fsSL --retry 3 -o "${CACHED}.part" "${URL}" && mv "${CACHED}.part" "$CACHED"
  else
    echo "reusing cached tarball: ${CACHED}" >&2
  fi
  # A failed check refuses and points at the bad entry — it never deletes a cache file on its own.
  BAD_CACHE="cached tarball failed sha256 check — remove or replace ${CACHED} yourself"
  if [ -n "${RBE_RUST_SHA256:-}" ]; then
    HASH_SOURCE="env RBE_RUST_SHA256"
    ( cd "$(dirname "$CACHED")" && echo "${RBE_RUST_SHA256}  ${TARBALL}" | sha256sum -c - ) || { echo "$BAD_CACHE" >&2; exit 1; }
  else
    PIN="$(pinned_sha256 || true)"
    if [ -n "$PIN" ]; then
      HASH_SOURCE="built-in pin (see SKILL.md hash table)"
      ( cd "$(dirname "$CACHED")" && echo "${PIN}  ${TARBALL}" | sha256sum -c - ) || { echo "$BAD_CACHE" >&2; exit 1; }
    else
      HASH_SOURCE="static.rust-lang.org .sha256 sidecar"
      curl -fsSL --retry 3 -o "${TARBALL_CACHE_DIR}/${TARBALL}.sha256" "${URL}.sha256"
      ( cd "${TARBALL_CACHE_DIR}" && sha256sum -c "${TARBALL}.sha256" ) || { echo "$BAD_CACHE" >&2; exit 1; }
    fi
  fi
  echo "sha256 verified (${HASH_SOURCE})" >&2
  tar xzf "$CACHED" -C "$STAGE"
  SRC="${STAGE}/${TARBALL%.tar.gz}"
  [ -f "${SRC}/install.sh" ] || { echo "no install.sh in ${SRC} — unexpected tarball layout" >&2; exit 1; }
  INSTALL_ARGS=(--destdir="$DEST")
  if [ -n "$WITHOUT" ]; then INSTALL_ARGS+=(--without="$WITHOUT"); fi
  ( cd "$SRC" && ./install.sh "${INSTALL_ARGS[@]}" )
  rm -rf "$STAGE"; STAGE=""
  ln -sfn "rust-${VERSION}-${TARGET}/usr/local/bin" "${BIN_DIR}"
  echo "deployed: rust ${VERSION} at ${BIN_DIR}" >&2
}

case "$CMD" in
  deploy) deploy ;;
  env)
    if use_system_toolchain; then exit 0; fi
    deploy
    echo "export PATH=\"${BIN_DIR}:\$PATH\""
    ;;
  run)
    [ $# -ge 1 ] || { echo "run: missing command" >&2; exit 2; }
    if use_system_toolchain; then exec "$@"; fi
    deploy
    PATH="${BIN_DIR}:${PATH}" exec "$@"
    ;;
  *) echo "usage: rust-toolchain.sh [--prefix DIR] [--version VER] [--replace] {deploy|env|run CMD...}" >&2; exit 2 ;;
esac
