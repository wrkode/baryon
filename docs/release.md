# Releases

How Baryon is versioned, what a release contains, how to verify it, and
how a release is made.

## Versioning

Baryon follows [Semantic Versioning](https://semver.org/). Every crate of
the workspace has the version in the workspace `Cargo.toml`; there is no
independent crate versioning. Tags are `v<version>`, for example
`v0.1.0-rc1`.

While the major version is 0:

- A minor release (0.x.0) may change the API, with the upgrade path in its
  release notes and in [api-stability.md](api-stability.md#change-log-per-release).
- A patch release (0.x.y) fixes bugs and avoids API changes; a breaking
  change in a patch release is made only for a correctness or security
  problem, and its release notes say so.
- A release candidate (`-rcN`) is a build proposed for the release. It is
  published as a GitHub prerelease.

The API version (`v1alpha1`) and the runtime protocol version (ADR 0012)
are separate from the release version: see
[api-stability.md](api-stability.md) and
[compatibility.md](compatibility.md#version-skew).

## Release contents

A release is a GitHub release of [wrkode/baryon](https://github.com/wrkode/baryon)
with these files, built by `.github/workflows/release.yaml` from the tag:

| File | Content |
|---|---|
| `baryon.yaml` | Everything `deploy/base` installs: CRDs, namespace, node agent, controller, admission policies, tenant roles. Component images are pinned by digest. |
| `baryon-crds.yaml` | The CRDs alone, for the first step of an upgrade |
| `baryon-classes.yaml` | The example VirtualMachineClasses |
| `baryon-virtd-v<version>-x86_64-linux-musl` | The static baryon-virtd binary, copied from the host image |
| `baryon-v<version>-source.tar.gz` | The source tree of the tag |
| `images.txt` | The image references with their digests |
| `*.spdx.json` | SPDX SBOMs of the source tree and of each image. The Rust dependencies are in the source tree SBOM; the SBOMs of the component images list the image only, because their static binaries carry no dependency data |
| `SHA256SUMS` | SHA-256 checksums of the files above |

Images on ghcr.io:

| Image | Content |
|---|---|
| `ghcr.io/wrkode/baryon-controller:v<version>` | baryon-controller |
| `ghcr.io/wrkode/baryon-node-agent:v<version>` | baryon-node-agent |
| `ghcr.io/wrkode/baryon-anchor:v<version>` | baryon-anchor (Anchor Pod) |
| `ghcr.io/wrkode/baryon-importer:v<version>` | baryon-importer (import Pod); not part of the anchor image |
| `ghcr.io/wrkode/baryon-hadron:v<version>-k0s` | The Baryon-enabled Hadron host image with k0s, the version pinned in `packaging/versions.env` |

The component images are `FROM scratch` with one static binary. The
inputs of the builds are pinned in `packaging/versions.env`: the base and
builder images (Hadron, kairos-init, the Rust builder image
`rust:1.99.0-alpine`, Alpine) by digest, and the downloaded host
components (Cloud Hypervisor, its firmware, virtiofsd) by SHA-256. The
host image's component manifest, `/usr/share/baryon/components.json` on
the host, records the Hadron and kairos-init image references with their
digests.

Repository visibility: GitHub artifact attestations are available for this
repository only while it is public (they are not available for private
repositories owned by a user account on its current GitHub plan). The
release workflow creates them only when the repository is public. A
release published from a private repository carries the files above,
including `SHA256SUMS` and the SBOMs, but no attestations, and its files
and ghcr.io images need GitHub access to the repository. wrkode/baryon is
private at the time of writing; whether to make it public is the owner's
decision.

## Verifying a release

Checksums, for every release:

```bash
sha256sum --check --ignore-missing SHA256SUMS
```

Build provenance, only for a release published while the repository is
public: the release workflow then creates GitHub artifact attestations
(SLSA build provenance, signed with the workflow's short-lived identity)
for every release file and image. Verify them with the GitHub CLI:

```bash
gh attestation verify baryon.yaml --repo wrkode/baryon
gh attestation verify oci://ghcr.io/wrkode/baryon-controller:v0.1.0-rc1 --repo wrkode/baryon
```

A release from a private repository has no attestations; verify
`SHA256SUMS` only. The checksums come from the same workflow run as the
files, so they detect corruption, not a substituted release.

What is signed: at most the attestations, by GitHub's attestation service
for the workflow run. No file or image is signed with a key of the
project, and the tags are not GPG-signed. There is no other signing
infrastructure.

Build stamps: every Baryon binary reports its version and the git commit
it was built from (`virtdGitCommit` on VirtualizationNodes, the `starting`
log line of each component). A build from a tree with uncommitted changes
carries `-dirty`; release builds come from the clean tag and never do.

## Making a release

1. All changes for the release are on `main`, signed off (DCO), and CI is
   green: format, clippy, tests, CRD drift, the API server checks,
   dependency advisories and licenses (`cargo deny`), DCO, and the image
   builds.
2. The workspace version in `Cargo.toml` is the release version, and
   `docs/release-notes/v<version>.md` exists.
   To check the release build before tagging, run the release workflow
   manually (`workflow_dispatch`): it builds every release file and image
   from the selected branch without pushing images, creating
   attestations, or creating a release.
3. Release validation on a real cluster passes (see
   [testing.md](testing.md)); hardware E2E does not run in GitHub-hosted
   CI, which has no KVM.
4. Architecture, security, and documentation reviews pass.
5. Tag and push: `git tag -a v<version> -m "Baryon v<version>"` and
   `git push origin v<version>`. The workflow builds, publishes, and
   creates the GitHub release; a version with a prerelease part becomes a
   prerelease. It creates attestations only when the repository is public.
6. If the repository is public, make the five ghcr.io packages
   (`baryon-controller`, `baryon-node-agent`, `baryon-anchor`,
   `baryon-importer`, `baryon-hadron`) public in their package settings on GitHub. Packages
   published from a personal account start private, and the images must
   be public for an installation without credentials. While the
   repository is private, this step does not apply: its packages stay
   available only to accounts with access.

### When the release workflow fails

The workflow pushes the images before it creates the GitHub release. If it
fails after pushing images, fix forward: correct the problem on `main` and
release a new release candidate with a new tag, for example `v0.1.0-rc2`
after `v0.1.0-rc1` (with its own workspace version and release notes). Do
not move, delete and re-create, or reuse a tag that was pushed: images and
files that carry its name may already have been pulled, and a second build
under the same name would differ from them. Images left from the failed
run stay under the failed tag's name; they are not part of any release.
