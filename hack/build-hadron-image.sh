#!/usr/bin/env bash
# Build the Baryon-enabled Hadron image with the pinned versions from
# packaging/versions.env.
#
# Usage: hack/build-hadron-image.sh [image-ref] [extra docker build args...]
#   KUBERNETES_DISTRO=k3s (default) | k0s | "" (core image without Kubernetes)
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
# shellcheck source=../packaging/versions.env
. packaging/versions.env

ref="${1:-baryon/hadron:dev}"
shift || true
distro="${KUBERNETES_DISTRO-k3s}"
version="${KUBERNETES_VERSION:-$K3S_VERSION}"
baryon_version="v$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
commit="$(git rev-parse --short=12 HEAD 2>/dev/null || echo unknown)"

args=()
for v in HADRON_IMAGE KAIROS_INIT_IMAGE \
         CLOUD_HYPERVISOR_VERSION CLOUD_HYPERVISOR_URL CLOUD_HYPERVISOR_SHA256 \
         CLOUDHV_FIRMWARE_RELEASE CLOUDHV_FIRMWARE_URL CLOUDHV_FIRMWARE_SHA256 \
         VIRTIOFSD_VERSION VIRTIOFSD_ZIP_URL VIRTIOFSD_ZIP_SHA256 VIRTIOFSD_SHA256; do
  args+=(--build-arg "$v=${!v}")
done

DOCKER_BUILDKIT=1 docker build \
  -f packaging/hadron/Dockerfile \
  --target image \
  "${args[@]}" \
  --build-arg "KUBERNETES_DISTRO=$distro" \
  --build-arg "KUBERNETES_VERSION=$version" \
  --build-arg "BARYON_VERSION=$baryon_version" \
  --build-arg "BARYON_GIT_COMMIT=$commit" \
  -t "$ref" \
  "$@" \
  .
echo "built $ref"
