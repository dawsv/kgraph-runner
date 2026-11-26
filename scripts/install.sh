#!/usr/bin/env sh
# Install KGraph Runner (kgraph & kgraph-town) on macOS or Linux.
# Prefers prebuilt GitHub Release binaries when available; falls back to `cargo install`.
set -eu

REPO="dawsv/kgraph-runner"
INSTALL_DIR="${KGRAPH_INSTALL_DIR:-$HOME/.cargo/bin}"
mkdir -p "$INSTALL_DIR"

OS="$(uname -s)"
ARCH="$(uname -m)"
TARGET=""
case "$OS:$ARCH" in
  Darwin:arm64|Darwin:aarch64) TARGET="aarch64-apple-darwin" ;;
  Darwin:x86_64)               TARGET="x86_64-apple-darwin" ;;
  Linux:x86_64)                TARGET="x86_64-unknown-linux-gnu" ;;
  *)                           TARGET="" ;;
esac

if [ -n "$TARGET" ] && command -v curl >/dev/null 2>&1; then
  TMPDIR_INST="$(mktemp -d)"
  trap 'rm -rf "$TMPDIR_INST"' EXIT
  ARCHIVE="kgraph-${TARGET}.tar.gz"
  BASE_URL="https://github.com/${REPO}/releases/latest/download"
  if curl -fsSL "${BASE_URL}/${ARCHIVE}" -o "${TMPDIR_INST}/${ARCHIVE}" 2>/dev/null; then
    if curl -fsSL "${BASE_URL}/SHA256SUMS.txt" -o "${TMPDIR_INST}/SHA256SUMS.txt" 2>/dev/null; then
      EXPECTED="$(grep " ${ARCHIVE}\$" "${TMPDIR_INST}/SHA256SUMS.txt" | awk '{print $1}' || true)"
      if [ -n "$EXPECTED" ]; then
        if command -v shasum >/dev/null 2>&1; then
          ACTUAL="$(shasum -a 256 "${TMPDIR_INST}/${ARCHIVE}" | awk '{print $1}')"
        elif command -v sha256sum >/dev/null 2>&1; then
          ACTUAL="$(sha256sum "${TMPDIR_INST}/${ARCHIVE}" | awk '{print $1}')"
        else
          ACTUAL="$EXPECTED"
        fi
        if [ "$ACTUAL" != "$EXPECTED" ]; then
          echo "Checksum mismatch for ${ARCHIVE}" >&2
          exit 1
        fi
      fi
    fi
    tar -xzf "${TMPDIR_INST}/${ARCHIVE}" -C "$INSTALL_DIR"
    chmod +x "${INSTALL_DIR}/kgraph" "${INSTALL_DIR}/kgraph-town"
    echo "Installed prebuilt kgraph and kgraph-town to ${INSTALL_DIR}"
    exit 0
  fi
fi

echo "No prebuilt archive available for ${OS}/${ARCH}; installing from source with Cargo..."
if ! command -v cargo >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
  export PATH="$HOME/.cargo/bin:$PATH"
fi
cargo install --git "https://github.com/${REPO}.git" --locked --force kgraph-cli
echo "Installed kgraph and kgraph-town via Cargo into ${INSTALL_DIR}"
