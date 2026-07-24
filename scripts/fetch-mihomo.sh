#!/usr/bin/env bash
# Optional offline/manual helper. Normal use auto-downloads via the CLI on start.
# Target layout matches src-tui/src/mihomo_manager/binary.rs::mihomo_binary_path().
set -euo pipefail

VERSION="${MIHOMO_VERSION:-v1.19.29}"
DEST_DIR="${XDG_DATA_HOME:-${HOME}/.local/share}/clash-verge-cli"
DEST="${DEST_DIR}/mihomo"

ARCH="$(uname -m)"
case "${ARCH}" in
  x86_64|amd64) ASSET="mihomo-linux-amd64-v2" ;;
  aarch64|arm64) ASSET="mihomo-linux-arm64" ;;
  armv7l|armhf) ASSET="mihomo-linux-armv7" ;;
  riscv64) ASSET="mihomo-linux-riscv64" ;;
  *)
    echo "unsupported architecture for auto-fetch: ${ARCH}" >&2
    echo "install a system verge-mihomo or place a binary at ${DEST}" >&2
    exit 1
    ;;
esac

URL="https://github.com/MetaCubeX/mihomo/releases/download/${VERSION}/${ASSET}-${VERSION}.gz"
TMP="$(mktemp -d)"
cleanup() { rm -rf "${TMP}"; }
trap cleanup EXIT

echo "Fetching ${URL}"
curl -fsSL "${URL}" -o "${TMP}/mihomo.gz"
mkdir -p "${DEST_DIR}"
gzip -dc "${TMP}/mihomo.gz" > "${DEST}"
chmod +x "${DEST}"

echo "Installed ${VERSION} -> ${DEST}"
"${DEST}" -v || true
