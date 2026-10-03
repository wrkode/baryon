#!/usr/bin/env bash
# Fail if deploy/base/crds/ is out of date with the Rust API types.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
generated="$(mktemp)"
trap 'rm -f "$generated"' EXIT
cargo run --quiet --locked -p baryon-api --bin baryon-crdgen > "$generated"
if ! diff -u deploy/base/crds/virtualization.baryon.io.yaml "$generated"; then
  echo "deploy/base/crds is stale; run: cargo run -p baryon-api --bin baryon-crdgen > deploy/base/crds/virtualization.baryon.io.yaml" >&2
  exit 1
fi
