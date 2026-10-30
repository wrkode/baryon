#!/usr/bin/env bash
# Build the Baryon-enabled Hadron image with the pinned versions from
# packaging/versions.env.
#
# Usage: hack/build-hadron-image.sh [image-ref] [extra docker build args...]
#   KUBERNETES_DISTRO=k0s (default, the tested distribution) | k3s (not
#   supported) | "" (core image without Kubernetes)
#   KUBERNETES_VERSION defaults to K0S_VERSION or K3S_VERSION from
#   packaging/versions.env.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
# shellcheck source=../packaging/versions.env
. packaging/versions.env

ref="${1:-baryon/hadron:dev}"
shift || true
distro="${KUBERNETES_DISTRO-k0s}"
case "$distro" in
  k0s) default_version="$K0S_VERSION" ;;
  k3s) default_version="$K3S_VERSION" ;;
  *) default_version="" ;;
esac
version="${KUBERNETES_VERSION:-$default_version}"
baryon_version="v$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
commit="$(git rev-parse --short=12 HEAD 2>/dev/null || echo unknown)"
# Mark builds from a tree with uncommitted changes to tracked files, so they
# are not mistaken for builds of the commit itself.
if [ "$commit" != unknown ] && ! git diff --quiet HEAD -- 2>/dev/null; then
  commit="$commit-dirty"
fi

# The image labels name the full commit (with -dirty for an uncommitted
# tree) and the commit time, which keeps them the same for every build of
# a commit.
revision="$(git rev-parse HEAD 2>/dev/null || echo unknown)"
case "$commit" in *-dirty) revision="$revision-dirty" ;; esac
created="$(git log -1 --format=%cI 2>/dev/null || echo unknown)"

args=()
for v in HADRON_IMAGE KAIROS_INIT_IMAGE RUST_BUILDER_IMAGE ALPINE_IMAGE CARGO_AUDITABLE_VERSION \
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
  --build-arg "BARYON_REVISION=$revision" \
  --build-arg "BARYON_CREATED=$created" \
  -t "$ref" \
  "$@" \
  .
echo "built $ref"
