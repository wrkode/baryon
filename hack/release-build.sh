#!/usr/bin/env bash
# Build every release artifact of the checked-out commit and check its
# identity, without publishing anything (docs/release.md).
#
# Usage: hack/release-build.sh <version>
# Output:
#   _release/  the release files: manifests, images.txt, the baryon-virtd
#              binary, the source archive, SPDX SBOMs, SHA256SUMS
#   _oci/      one OCI image layout per image (<name>/, tag <version>),
#              whose manifest digests are the published digests: the
#              publish job copies them to the registry unchanged
#   _oci/digests  "<name> sha256:<digest>" per image
#
# Environment:
#   REGISTRY   image repository prefix (default ghcr.io/wrkode)
#   SKOPEO     skopeo command (default skopeo)
#   SYFT       syft command (default syft)
# Needs a clean checkout: the version stamps and image labels name the
# commit.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
version="${1:?usage: hack/release-build.sh <version>}"
registry="${REGISTRY:-ghcr.io/wrkode}"
read -r -a skopeo <<< "${SKOPEO:-skopeo}"
read -r -a syft <<< "${SYFT:-syft}"
fail() { echo "release build: $*" >&2; exit 1; }

[ -z "$(git status --porcelain)" ] || fail "the checkout is not clean"
[ "$version" = "v$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)" ] \
  || fail "$version is not the workspace version"
crate_version="${version#v}"
sha="$(git rev-parse HEAD)"
short="$(git rev-parse --short=12 HEAD)"
# shellcheck source=../packaging/versions.env
. packaging/versions.env
rm -rf _release _oci
mkdir -p _release _oci/archive

# Source SBOM and archive first, from the clean tree only.
"${syft[@]}" scan "dir:." --source-name "github.com/wrkode/baryon" --source-version "$sha" \
  -o "spdx-json=_release/baryon-$version-source.spdx.json"
git archive --format=tar.gz --prefix="baryon-$version/" -o "_release/baryon-$version-source.tar.gz" HEAD

hack/build-static.sh
bins=target/static/x86_64-unknown-linux-musl/release
for c in anchor importer node-agent controller; do
  docker build -f "packaging/images/$c.Dockerfile" \
    --build-arg "BARYON_VERSION=$version" --build-arg "BARYON_REVISION=$sha" \
    -t "$registry/baryon-$c:$version" .
done
hack/build-hadron-image.sh "$registry/baryon-hadron:$version-k0s"
[ -z "$(git status --porcelain)" ] || fail "the build changed the checkout"

# One OCI layout per image. The conversion from the Docker archive fixes the
# manifest digest (gzip layers); the layout is the only copy that is
# published, so the digest below is the digest in the registry.
images=(baryon-anchor:"$version" baryon-importer:"$version" baryon-node-agent:"$version" \
  baryon-controller:"$version" baryon-hadron:"$version-k0s")
: > _oci/digests
for i in "${images[@]}"; do
  name="${i%%:*}" tag="${i#*:}"
  docker save -o "_oci/archive/$name.tar" "$registry/$i"
  "${skopeo[@]}" copy --quiet --dest-compress-format gzip \
    "docker-archive:_oci/archive/$name.tar" "oci:_oci/$name:$tag"
  digest="sha256:$("${skopeo[@]}" inspect --raw "oci:_oci/$name:$tag" | sha256sum | cut -d' ' -f1)"
  # The identity checks below inspect the Docker image; the layout must hold
  # that image: the same layer content (the uncompressed layer digests) and
  # the same labels. The config digest itself differs, because the
  # conversion writes the config in the OCI format.
  norm='import json, sys; c = json.load(sys.stdin); print(json.dumps([c["rootfs"]["diff_ids"], c.get("config", {}).get("Labels")], sort_keys=True))'
  layout_id="$("${skopeo[@]}" inspect --config "oci:_oci/$name:$tag" | python3 -c "$norm")"
  docker_id="$(docker inspect "$registry/$i" | python3 -c 'import json, sys; c = json.load(sys.stdin)[0]; print(json.dumps([c["RootFS"]["Layers"], c["Config"].get("Labels")], sort_keys=True))')"
  [ "$layout_id" = "$docker_id" ] \
    || fail "the OCI layout of $name does not hold the checked image"
  echo "$name $digest" >> _oci/digests
done
rm -rf _oci/archive
digest_of() { sed -n "s/^$1 //p" _oci/digests; }
ref_of() { echo "$registry/$1@$(digest_of "$1")"; }

CONTROLLER_IMAGE="$(ref_of baryon-controller)" NODE_AGENT_IMAGE="$(ref_of baryon-node-agent)" \
ANCHOR_IMAGE="$(ref_of baryon-anchor)" IMPORTER_IMAGE="$(ref_of baryon-importer)" \
  hack/release-manifests.sh _release
cid="$(docker create "$registry/baryon-hadron:$version-k0s")"
docker cp "$cid:/usr/bin/baryon-virtd" "_release/baryon-virtd-$version-x86_64-linux-musl"
docker rm "$cid" >/dev/null
chmod 0755 "_release/baryon-virtd-$version-x86_64-linux-musl"
{
  echo "controller: $(ref_of baryon-controller)"
  echo "node-agent: $(ref_of baryon-node-agent)"
  echo "anchor: $(ref_of baryon-anchor)"
  echo "importer: $(ref_of baryon-importer)"
  echo "host (k0s $K0S_VERSION): $(ref_of baryon-hadron)"
} > _release/images.txt

# SBOMs of the images as published: scanned from the OCI layouts, named by
# repository and manifest digest. Each must record that digest, and list the
# Rust crates of the Baryon binaries (cargo-auditable).
for i in "${images[@]}"; do
  name="${i%%:*}" tag="${i#*:}" digest="$(digest_of "${i%%:*}")"
  out="_release/$name-$version.spdx.json"
  [ "$name" = baryon-hadron ] && out="_release/baryon-hadron-k0s-$version.spdx.json"
  "${syft[@]}" scan "oci-dir:_oci/$name" --source-name "$registry/$name" --source-version "$digest" \
    -o "spdx-json=$out"
  grep -qF "$digest" "$out" || fail "$out does not record $digest"
  crates="$(python3 -c 'import json, sys; print(sum(1 for p in json.load(open(sys.argv[1]))["packages"] if any(r.get("referenceLocator", "").startswith("pkg:cargo/") for r in p.get("externalRefs", []))))' "$out")"
  [ "$crates" -gt 20 ] || fail "$out lists $crates Rust crates; the binary dependency data is missing"
done
"${syft[@]}" scan "file:_release/baryon-virtd-$version-x86_64-linux-musl" \
  --source-name baryon-virtd --source-version "$version" \
  -o "spdx-json=_release/baryon-virtd-$version-x86_64-linux-musl.spdx.json"

# Release identity: every artifact names this version and commit.
check() { "$@" || fail "identity check failed: $*"; }
stamp() { # stamp <binary>: "<version line> commit=<embedded commits>"
  echo "$("$1" --version 2>&1 | head -1) commit=$(strings "$1" | grep -o "$short\(-dirty\)\?" | sort -u | tr '\n' ' ')"
}
for c in anchor importer node-agent controller; do
  check [ "$(stamp "$bins/baryon-$c")" = "baryon-$c $crate_version commit=$short " ]
  labels="$(docker inspect -f '{{index .Config.Labels "org.opencontainers.image.version"}} {{index .Config.Labels "org.opencontainers.image.revision"}}' "$registry/baryon-$c:$version")"
  check [ "$labels" = "$version $sha" ]
  # The published image contains the binary that was checked above.
  cid="$(docker create "$registry/baryon-$c:$version" x)"
  docker cp "$cid:/baryon-$c" "_oci/check-$c"
  docker rm "$cid" >/dev/null
  check cmp -s "_oci/check-$c" "$bins/baryon-$c"
  rm -f "_oci/check-$c"
done
host="$registry/baryon-hadron:$version-k0s"
comp="$(docker run --rm --entrypoint cat "$host" /usr/share/baryon/components.json)"
check grep -qF "\"version\": \"$version\", \"gitCommit\": \"$short\"" <<< "$comp"
check grep -qF "\"$CLOUD_HYPERVISOR_VERSION\"" <<< "$comp"
labels="$(docker inspect -f '{{index .Config.Labels "org.opencontainers.image.version"}} {{index .Config.Labels "org.opencontainers.image.revision"}}' "$host")"
check [ "$labels" = "$version $sha" ]
check [ "$(stamp "_release/baryon-virtd-$version-x86_64-linux-musl")" = "baryon-virtd $crate_version commit=$short " ]
# The Anchor image holds only the anchor; the importer only the importer.
for c in anchor importer; do
  cid="$(docker create "$registry/baryon-$c:$version" x)"
  files="$(docker export "$cid" | tar -t | grep -v '/$' | grep -v '^\(etc\|proc\|sys\|dev\)/' | grep -vx '.dockerenv' | tr '\n' ' ')"
  docker rm "$cid" >/dev/null
  check [ "$files" = "baryon-$c " ]
done
check bash -c "! strings '$bins/baryon-anchor' | grep -q import-approved"
for c in controller node-agent anchor importer; do
  check grep -qF "$(ref_of "baryon-$c")" _release/baryon.yaml
done

(cd _release && sha256sum -- * > SHA256SUMS)
echo "release build: $version ($sha) built and checked"
cat _oci/digests
