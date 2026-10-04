#!/usr/bin/env bash
# Build static musl binaries of the Baryon components in a rust:alpine
# container, so no musl C toolchain is needed on the host. Hadron is
# musl-only, so host components must be static.
#
# Usage: hack/build-static.sh [package...]
# Output: target/static/x86_64-unknown-linux-musl/release/<binary>
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

toolchain="$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml)"
image="${BARYON_BUILD_IMAGE:-rust:${toolchain}-alpine}"
packages=("$@")
if [ "${#packages[@]}" -eq 0 ]; then
  packages=(baryon-virtd baryon-anchor baryon-controller baryon-node-agent)
fi
pkg_args=()
for p in "${packages[@]}"; do
  pkg_args+=(-p "$p")
done

commit="$(git rev-parse --short=12 HEAD 2>/dev/null || echo unknown)"
# Mark builds from a tree with uncommitted changes to tracked files, so they
# are not mistaken for builds of the commit itself.
if [ "$commit" != unknown ] && ! git diff --quiet HEAD -- 2>/dev/null; then
  commit="$commit-dirty"
fi
cache="${BARYON_BUILD_CACHE:-${HOME}/.cache/baryon-build}"
mkdir -p "$cache/registry" "$cache/git"

docker run --rm \
  -u "$(id -u):$(id -g)" \
  -e CARGO_HOME=/cargo \
  -e CARGO_TARGET_DIR=/src/target/static \
  -e BARYON_GIT_COMMIT="$commit" \
  -e RUSTUP_TOOLCHAIN="$toolchain" \
  -v "$cache/registry:/cargo/registry" \
  -v "$cache/git:/cargo/git" \
  -v "$PWD:/src" \
  -w /src \
  "$image" \
  cargo build --locked --release --target x86_64-unknown-linux-musl "${pkg_args[@]}"
