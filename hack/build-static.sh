#!/usr/bin/env bash
# Build static musl binaries of the Baryon components in a rust:alpine
# container, so no musl C toolchain is needed on the host. Hadron is
# musl-only, so host components must be static. The build goes through
# cargo-auditable (CARGO_AUDITABLE_VERSION in packaging/versions.env), which
# embeds the crate dependency list in each binary.
#
# Usage: hack/build-static.sh [package...]
# Output: target/static/x86_64-unknown-linux-musl/release/<binary>
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

toolchain="$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml)"
# shellcheck source=../packaging/versions.env
. packaging/versions.env
image="${BARYON_BUILD_IMAGE:-$RUST_BUILDER_IMAGE}"
case "$image" in
  *"rust:${toolchain}-alpine"*) ;;
  *) [ -n "${BARYON_BUILD_IMAGE:-}" ] || { echo "RUST_BUILDER_IMAGE does not match rust-toolchain.toml ($toolchain)" >&2; exit 1; } ;;
esac
packages=("$@")
if [ "${#packages[@]}" -eq 0 ]; then
  packages=(baryon-virtd baryon-anchor baryon-importer baryon-controller baryon-node-agent)
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
mkdir -p "$cache/registry" "$cache/git" "$cache/tools"

docker run --rm \
  -u "$(id -u):$(id -g)" \
  -e CARGO_HOME=/cargo \
  -e CARGO_TARGET_DIR=/src/target/static \
  -e BARYON_GIT_COMMIT="$commit" \
  -e RUSTUP_TOOLCHAIN="$toolchain" \
  -v "$cache/registry:/cargo/registry" \
  -v "$cache/git:/cargo/git" \
  -v "$cache/tools:/tools" \
  -v "$PWD:/src" \
  -w /src \
  "$image" \
  sh -euc '
    # cargo-auditable records the dependency tree in each binary.
    if ! /tools/bin/cargo-auditable --version 2>/dev/null | grep -qF " $1"; then
      cargo install --locked --root /tools "cargo-auditable@$1"
    fi
    shift
    PATH="/tools/bin:$PATH" cargo auditable build --locked --release --target x86_64-unknown-linux-musl "$@"
  ' sh "$CARGO_AUDITABLE_VERSION" "${pkg_args[@]}"
