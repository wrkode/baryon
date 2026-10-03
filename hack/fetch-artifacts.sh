#!/usr/bin/env bash
# Download the pinned host components into _output/artifacts and verify
# their SHA-256 checksums.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
# shellcheck source=../packaging/versions.env
. packaging/versions.env
out=_output/artifacts
mkdir -p "$out"

fetch() {
  local url="$1" dest="$2" sum="$3"
  if [ ! -f "$dest" ] || ! echo "$sum  $dest" | sha256sum -c --quiet - 2>/dev/null; then
    echo "fetching $url"
    curl -fsSL -o "$dest.tmp" "$url"
    mv "$dest.tmp" "$dest"
  fi
  echo "$sum  $dest" | sha256sum -c --quiet -
}

fetch "$CLOUD_HYPERVISOR_URL" "$out/cloud-hypervisor" "$CLOUD_HYPERVISOR_SHA256"
chmod 0755 "$out/cloud-hypervisor"
fetch "$CLOUDHV_FIRMWARE_URL" "$out/CLOUDHV.fd" "$CLOUDHV_FIRMWARE_SHA256"
fetch "$VIRTIOFSD_ZIP_URL" "$out/virtiofsd.zip" "$VIRTIOFSD_ZIP_SHA256"
if [ ! -f "$out/virtiofsd" ] || ! echo "$VIRTIOFSD_SHA256  $out/virtiofsd" | sha256sum -c --quiet - 2>/dev/null; then
  unzip -o -q -j "$out/virtiofsd.zip" '*/x86_64-unknown-linux-musl/release/virtiofsd' -d "$out"
fi
echo "$VIRTIOFSD_SHA256  $out/virtiofsd" | sha256sum -c --quiet -
chmod 0755 "$out/virtiofsd"
echo "artifacts verified in $out"
