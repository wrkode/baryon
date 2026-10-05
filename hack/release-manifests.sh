#!/usr/bin/env bash
# Render the installation manifests of a release from deploy/base, with the
# component images replaced by the given references (by digest for a
# release).
#
# Usage: hack/release-manifests.sh <out-dir>
# Environment (all required): CONTROLLER_IMAGE, NODE_AGENT_IMAGE,
# ANCHOR_IMAGE, each a full image reference such as
# ghcr.io/wrkode/baryon-controller@sha256:<digest>.
#
# Output in <out-dir>:
#   baryon.yaml         everything deploy/base installs
#   baryon-crds.yaml    the CRDs alone (upgrade step 1)
#   baryon-classes.yaml the example VirtualMachineClasses
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

out="${1:?usage: hack/release-manifests.sh <out-dir>}"
for v in CONTROLLER_IMAGE NODE_AGENT_IMAGE ANCHOR_IMAGE; do
  [ -n "${!v:-}" ] || { echo "$v is required" >&2; exit 2; }
done

# name[:tag] or name@digest -> kustomize image fields
image_fields() {
  local ref="$1"
  if [[ "$ref" == *@* ]]; then
    printf '    newName: %s\n    digest: %s\n' "${ref%@*}" "${ref#*@}"
  else
    printf '    newName: %s\n    newTag: %s\n' "${ref%:*}" "${ref##*:}"
  fi
}

work="$(mktemp -d "$PWD/.release-kustomize.XXXXXX")"
trap 'rm -rf "$work"' EXIT
{
  echo "apiVersion: kustomize.config.k8s.io/v1beta1"
  echo "kind: Kustomization"
  echo "resources:"
  echo "  - ../deploy/base"
  echo "images:"
  echo "  - name: baryon/controller"
  image_fields "$CONTROLLER_IMAGE"
  echo "  - name: baryon/node-agent"
  image_fields "$NODE_AGENT_IMAGE"
  echo "patches:"
  echo "  - target: {kind: Deployment, name: baryon-controller}"
  echo "    patch: |-"
  echo "      - op: test"
  echo "        path: /spec/template/spec/containers/0/env/0/name"
  echo "        value: BARYON_ANCHOR_IMAGE"
  echo "      - op: replace"
  echo "        path: /spec/template/spec/containers/0/env/0/value"
  echo "        value: $ANCHOR_IMAGE"
} > "$work/kustomization.yaml"

mkdir -p "$out"
kubectl kustomize "$work" > "$out/baryon.yaml"
cp deploy/base/crds/virtualization.baryon.io.yaml "$out/baryon-crds.yaml"
cp examples/classes.yaml "$out/baryon-classes.yaml"

# No development image may remain.
if grep -n 'baryon/[a-z-]*:dev' "$out/baryon.yaml"; then
  echo "development image references left in baryon.yaml" >&2
  exit 1
fi
for ref in "$CONTROLLER_IMAGE" "$NODE_AGENT_IMAGE" "$ANCHOR_IMAGE"; do
  grep -qF "$ref" "$out/baryon.yaml" || { echo "$ref missing from baryon.yaml" >&2; exit 1; }
done
echo "rendered $out/baryon.yaml, $out/baryon-crds.yaml, $out/baryon-classes.yaml"
