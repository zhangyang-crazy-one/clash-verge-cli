#!/usr/bin/env bash
# Package release binaries into dist/: one tarball per target, .deb and .rpm
# packages for the static (musl) targets, and SHA256SUMS.
#
# Usage: scripts/package-release.sh VERSION TARGET...
#
# Each target/<TARGET>/release/clash-verge-cli must already be built.
# Completions and man pages come from the binary itself, so one target must
# be runnable on this machine (the first one that runs is used).
# .deb/.rpm need cargo-deb and cargo-generate-rpm.
set -euo pipefail

if [[ $# -lt 2 ]]; then
  echo "usage: $0 VERSION TARGET..." >&2
  exit 2
fi
version="$1"
shift
targets=("$@")

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "${root}"
dist="${root}/dist"
assets="${root}/build/release-assets"
rm -rf "${assets}"
mkdir -p "${dist}" "${assets}/completions"

# Completions and man pages are the same for every target.
generator=""
for target in "${targets[@]}"; do
  candidate="target/${target}/release/clash-verge-cli"
  if [[ -x "${candidate}" ]] && "${candidate}" --version >/dev/null 2>&1; then
    generator="${candidate}"
    break
  fi
done
if [[ -z "${generator}" ]]; then
  echo "none of the targets runs on this machine; cannot generate completions" >&2
  exit 1
fi
"${generator}" completions bash > "${assets}/completions/clash-verge-cli.bash"
"${generator}" completions zsh > "${assets}/completions/_clash-verge-cli"
"${generator}" completions fish > "${assets}/completions/clash-verge-cli.fish"
"${generator}" man --dir "${assets}/man"

for target in "${targets[@]}"; do
  binary="target/${target}/release/clash-verge-cli"
  stage="$(mktemp -d)"
  install -Dm755 "${binary}" "${stage}/clash-verge-cli"
  cp -r "${assets}/completions" "${assets}/man" "${stage}/"
  install -m644 README.md LICENSE "${stage}/"
  tar -czf "${dist}/clash-verge-cli-${version}-${target}.tar.gz" -C "${stage}" .
  rm -rf "${stage}"

  if [[ "${target}" == *-musl* ]]; then
    cargo deb -p clash-verge-cli --no-build --no-strip --target "${target}" --output "${dist}/"
    rpm_arch=()
    # Fedora/RHEL name 32-bit hard-float ARM "armv7hl".
    [[ "${target}" == armv7-* ]] && rpm_arch=(--arch armv7hl)
    cargo generate-rpm -p src-tui --target "${target}" "${rpm_arch[@]}" --output "${dist}/"
  fi
done

cd "${dist}"
rm -f SHA256SUMS
shopt -s nullglob
sha256sum -- *.tar.gz *.deb *.rpm > SHA256SUMS
cat SHA256SUMS
