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
| `baryon-virtd-<version>-x86_64-linux-musl` | The static baryon-virtd binary, as shipped in the host image |
| `baryon-<version>-source.tar.gz` | The source tree of the tag |
| `images.txt` | The image references with their digests |
| `*.spdx.json` | SPDX SBOMs of the source tree and of each image |
| `SHA256SUMS` | SHA-256 checksums of the files above |

Images on ghcr.io:

| Image | Content |
|---|---|
| `ghcr.io/wrkode/baryon-controller:<version>` | baryon-controller |
| `ghcr.io/wrkode/baryon-node-agent:<version>` | baryon-node-agent |
| `ghcr.io/wrkode/baryon-anchor:<version>` | baryon-anchor (Anchor Pod and import Pod) |
| `ghcr.io/wrkode/baryon-hadron:<version>-k0s` | The Baryon-enabled Hadron host image with k0s, the version pinned in `packaging/versions.env` |

The component images are `FROM scratch` with one static binary. The host
image's inputs are pinned with checksums in `packaging/versions.env`; its
component manifest is `/usr/share/baryon/components.json` on the host.

## Verifying a release

Checksums:

```bash
sha256sum --check --ignore-missing SHA256SUMS
```

Build provenance: the release workflow creates GitHub artifact
attestations (SLSA build provenance, signed with the workflow's
short-lived identity) for every release file and image. Verify them with
the GitHub CLI:

```bash
gh attestation verify baryon.yaml --repo wrkode/baryon
gh attestation verify oci://ghcr.io/wrkode/baryon-controller:v0.1.0-rc1 --repo wrkode/baryon
```

What is signed: only the attestations, by GitHub's attestation service for
the workflow run. No file or image is signed with a key of the project, and
the tags are not GPG-signed. There is no other signing infrastructure.

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
3. Release validation on a real cluster passes (see
   [testing.md](testing.md)); hardware E2E does not run in GitHub-hosted
   CI, which has no KVM.
4. Architecture, security, and documentation reviews pass.
5. Tag and push: `git tag -a v<version> -m "Baryon v<version>"` and
   `git push origin v<version>`. The workflow builds, publishes, and
   creates the GitHub release; a version with a prerelease part becomes a
   prerelease.

Packages published to ghcr.io from a personal account are private until
their visibility is changed in the package settings on GitHub; the
images must be public for an installation without credentials.
