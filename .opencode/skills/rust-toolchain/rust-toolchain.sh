#!/usr/bin/env bash
# Deploy a pinned, sha256-verified Rust toolchain to a prefix and expose it via PATH,
# so unmodified scripts (including Claude-provided test scripts from drops) can use it.
#
# Usage:
#   rust-toolchain.sh [--prefix DIR] [--version VER] deploy
#   rust-toolchain.sh [--prefix DIR] [--version VER] env        # print export lines for eval
#   rust-toolchain.sh [--prefix DIR] [--version VER] run CMD... # exec CMD with the toolchain on PATH
#
# Env vars: RBE_RUST_PREFIX (default /tmp/opencode/rust-home), RBE_RUST_VERSION (default 1.91.1)
# Writes only under the prefix; downloads are verified against rust-lang.org's .sha256 sidecar.

set -euo pipefail

PREFIX="${RBE_RUST_PREFIX:-/tmp/opencode/rust-home}"
VERSION="${RBE_RUST_VERSION:-1.91.1}"
CMD=""
TMP="" STAGE=""

cleanup() {
  [ -z "$TMP" ] || rm -rf "$TMP"
  [ -z "$STAGE" ] || rm -rf "$STAGE"
  return 0
}
trap cleanup EXIT

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX="$2"; shift 2 ;;
    --version) VERSION="$2"; shift 2 ;;
    deploy|env|run) CMD="$1"; shift; break ;;
    *) echo "usage: rust-toolchain.sh [--prefix DIR] [--version VER] {deploy|env|run CMD...}" >&2; exit 2 ;;
  esac
done

case "$(uname -m)" in
  x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
  aarch64) TARGET="aarch64-unknown-linux-gnu" ;;
  *) echo "unsupported arch: $(uname -m)" >&2; exit 1 ;;
esac

TARBALL="rust-${VERSION}-${TARGET}.tar.gz"
URL="https://static.rust-lang.org/dist/${TARBALL}"
BIN_DIR="${PREFIX}/bin"

deploy() {
  if [ -x "${BIN_DIR}/rustc" ] && [ "$("${BIN_DIR}/rustc" --version | awk '{print $2}')" = "${VERSION}" ]; then
    echo "already deployed: rust ${VERSION} at ${BIN_DIR}" >&2
    return 0
  fi
  if [ -e "${BIN_DIR}" ] && [ ! -L "${BIN_DIR}" ]; then
    echo "refusing to overwrite non-symlink directory: ${BIN_DIR}" >&2
    exit 1
  fi
  mkdir -p "${PREFIX}"
  TMP="$(mktemp -d "${PREFIX}/.dl.XXXXXX")"
  STAGE="$(mktemp -d "${PREFIX}/.stage.XXXXXX")"
  echo "downloading ${URL}" >&2
  curl -fsSL --retry 3 -o "${TMP}/${TARBALL}" "${URL}"
  curl -fsSL --retry 3 -o "${TMP}/${TARBALL}.sha256" "${URL}.sha256"
  ( cd "$TMP" && sha256sum -c "${TARBALL}.sha256" )
  tar xzf "${TMP}/${TARBALL}" -C "$STAGE"
  rm -rf "$TMP"; TMP=""
  SRC="${STAGE}/${TARBALL%.tar.gz}"
  [ -f "${SRC}/install.sh" ] || { echo "no install.sh in ${SRC} — unexpected tarball layout" >&2; exit 1; }
  DEST="${PREFIX}/rust-${VERSION}-${TARGET}"
  if [ -e "$DEST" ]; then mv "$DEST" "${DEST}.old"; fi
  ( cd "$SRC" && ./install.sh --destdir="$DEST" )
  rm -rf "$STAGE"; STAGE=""
  ln -sfn "rust-${VERSION}-${TARGET}/usr/local/bin" "${BIN_DIR}"
  rm -rf "${DEST}.old"
  echo "deployed: rust ${VERSION} at ${BIN_DIR}" >&2
}

case "$CMD" in
  deploy) deploy ;;
  env) deploy; echo "export PATH=\"${BIN_DIR}:\$PATH\"" ;;
  run)
    [ $# -ge 1 ] || { echo "run: missing command" >&2; exit 2; }
    deploy
    PATH="${BIN_DIR}:${PATH}" exec "$@"
    ;;
  *) echo "usage: rust-toolchain.sh [--prefix DIR] [--version VER] {deploy|env|run CMD...}" >&2; exit 2 ;;
esac
