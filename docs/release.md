# Releases

How Baryon is versioned, what a release contains, how to verify it, and
how a release is made.

## Versioning

Baryon follows [Semantic Versioning](https://semver.org/). Every crate of
the workspace has the version in the workspace `Cargo.toml`; there is no
independent crate versioning. Tags are `v<version>`, for example
`v0.1.0-rc2`.

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
| `baryon-virtd-v<version>-x86_64-linux-musl` | The static baryon-virtd binary, identical to the one in the host image |
| `baryon-v<version>-source.tar.gz` | The source tree of the tag |
| `images.txt` | The image references with their digests |
| `baryon-<image>-v<version>.spdx.json` | An SPDX SBOM of each image, generated from the image as published and naming its manifest digest; it lists the Rust crates compiled into the Baryon binaries |
| `baryon-virtd-v<version>-x86_64-linux-musl.spdx.json` | An SPDX SBOM of the baryon-virtd binary with its Rust crates |
| `baryon-v<version>-source.spdx.json` | An SPDX SBOM of the source tree (`Cargo.lock`) |
| `SHA256SUMS` | SHA-256 checksums of the files above |

Images on ghcr.io, public:

| Image | Content |
|---|---|
| `ghcr.io/wrkode/baryon-controller:v<version>` | baryon-controller |
| `ghcr.io/wrkode/baryon-node-agent:v<version>` | baryon-node-agent |
| `ghcr.io/wrkode/baryon-anchor:v<version>` | baryon-anchor (Anchor Pod) |
| `ghcr.io/wrkode/baryon-importer:v<version>` | baryon-importer (import Pod); not part of the anchor image |
| `ghcr.io/wrkode/baryon-hadron:v<version>-k0s` | The Baryon-enabled Hadron host image with k0s, the version pinned in `packaging/versions.env` |

The component images are `FROM scratch` with one static binary. Every
image carries the OCI labels `org.opencontainers.image.version` (the tag)
and `org.opencontainers.image.revision` (the full commit); the host image
also names its Hadron base in `org.opencontainers.image.base.name`. The
inputs of the builds are pinned in `packaging/versions.env`: the base and
builder images (Hadron, kairos-init, the Rust builder image
`rust:1.99.0-alpine`, Alpine) by digest, the downloaded host components
(Cloud Hypervisor, its firmware, virtiofsd) by SHA-256, and the
cargo-auditable version. The host image's component manifest,
`/usr/share/baryon/components.json` on the host, records the Hadron and
kairos-init image references with their digests.

### Dependency data in the binaries

Every Baryon binary (baryon-virtd, baryon-anchor, baryon-importer,
baryon-controller, baryon-node-agent) is built with
[cargo-auditable](https://github.com/rust-secure-code/cargo-auditable),
which embeds the list of Rust crates and versions compiled into it, as
compressed JSON in a `.dep-v0` section (a few kilobytes; no timestamps).
Scanners that read it, such as syft, grype, trivy (image and rootfs
scans), and osv-scanner (images), report the crates of a released binary
or image without the source tree. CI checks that every binary has the
section. The binaries' behavior, version reporting, and the seccomp
filters do not change: the section is data that the program never reads.

## Release workflow

`.github/workflows/release.yaml` runs on a pushed `v*` tag. A manual run
(`workflow_dispatch`) on `main` is a dry run: the gate and the build run,
and nothing is attested or published. The jobs run in this order, each
with only the permissions it needs; the workflow has no other trigger, so
no pull request runs it.

| Job | Permissions | What it does |
|---|---|---|
| gate | `contents: read`, `actions: read`, `statuses: read` | `hack/release-gate.sh`: the release gate below |
| build | `contents: read` | `hack/release-build.sh`: builds every file and image, converts each image once into an OCI image layout whose manifest digest is the published digest, renders the manifests with those digests, generates the SBOMs from the layouts, checks the release identity, and writes `SHA256SUMS` |
| attest | `contents: read`, `id-token: write`, `attestations: write` | GitHub artifact attestations (SLSA build provenance) for every release file and every image digest |
| publish | `contents: write`, `packages: write` | Copies each OCI layout to ghcr.io unchanged (`skopeo copy --preserve-digests`), checks that the registry digest and the tag are the built digest, and creates the GitHub release (a prerelease for a version with a prerelease part) |

The publish job checks out no repository code, and the attest job runs
before it, so nothing is published without its attestations. The build
job holds no write permission.

### Release gate

The gate fails closed: any check it cannot make fails the release.

- The tag equals `v` and the workspace version in `Cargo.toml`, and
  `docs/release-notes/v<version>.md` exists.
- The tag is annotated, points to the workflow's commit, and carries a
  signature that GitHub verifies (`verification.verified` of the tag
  object), and no release of that tag exists yet.
- The commit is reachable from `main` as fetched from GitHub, not from a
  local branch.
- The latest run of the CI workflow (`.github/workflows/ci.yaml`) on the
  push of that exact commit to `main` completed successfully.
- Release qualification passed on that exact commit: the commit has a
  `success` status with context `baryon/release-qualification`, set by a
  maintainer (a user account, not a workflow) after the qualification
  below. A dry run reports a missing status and continues.

The build job then checks the identity of every artifact against the tag
and the commit: each component binary reports the version and is stamped
with the commit (no `-dirty`), the binary in each image is the one that
was checked, each image's version and revision labels match, the host
image's `components.json` records the version, the commit, and the pinned
Cloud Hypervisor version, the released baryon-virtd is the host image's,
the Anchor image holds only `baryon-anchor` and the importer image only
`baryon-importer`, `baryon.yaml` references every component image by its
built digest, and every image SBOM records that digest and lists Rust
crates. Any mismatch fails the build.

### Tags and releases

Release tags are annotated tags signed with the maintainer's SSH signing
key that GitHub verifies (`git tag -s`; the key is the one registered for
signing on the maintainer's GitHub account). The signature shows who
created the tag; artifact attestations (below) are the means of verifying
release artifacts.

Repository rules, configured on GitHub:

- `refs/tags/v*` (ruleset "release tags"): a tag can be created, but not
  deleted, moved, or force-updated, by anyone, including administrators.
  A mistaken tag is replaced by a new release candidate, never reused.
- `refs/heads/main` (ruleset "main history"): no deletion and no force
  push, so the history that the gate checks against cannot be rewritten.
- Immutable releases are enabled: once published, a release's tag and
  files cannot be changed or deleted; its title and notes can be edited.

A change to these rules is made by the repository owner in the repository
settings and recorded here.

## Verifying a release

Everything below works without GitHub credentials unless it says
otherwise. `<version>` is the release, for example `v0.1.0-rc3`.

Checksums of the downloaded release files:

```bash
gh release download <version> --repo wrkode/baryon   # or download each file from the release page
sha256sum --check SHA256SUMS
```

Build provenance (GitHub artifact attestations, signed through Sigstore
with the release workflow's identity). Each attestation binds the
artifact's SHA-256 digest to the repository, the workflow file, the tag,
and the commit. With the GitHub CLI, which needs a GitHub login for this
command:

```bash
gh attestation verify baryon.yaml --repo wrkode/baryon \
  --signer-workflow wrkode/baryon/.github/workflows/release.yaml \
  --source-ref refs/tags/<version> --source-digest <commit>
gh attestation verify oci://ghcr.io/wrkode/baryon-controller@<digest> --repo wrkode/baryon \
  --signer-workflow wrkode/baryon/.github/workflows/release.yaml \
  --source-ref refs/tags/<version> --source-digest <commit>
```

Without a login, fetch the attestation bundle from the public API and
verify it offline:

```bash
digest="sha256:$(sha256sum baryon.yaml | cut -d' ' -f1)"   # or an image digest from images.txt
curl -s "https://api.github.com/repos/wrkode/baryon/attestations/$digest" \
  | jq '.attestations[0].bundle' > baryon.yaml.bundle.json
gh attestation verify baryon.yaml --bundle baryon.yaml.bundle.json --repo wrkode/baryon \
  --signer-workflow wrkode/baryon/.github/workflows/release.yaml \
  --source-ref refs/tags/<version> --source-digest <commit>
```

Image identity: `images.txt` names every image by digest, and
`baryon.yaml` uses the same digests. Each image SBOM names its image by
repository and manifest digest; the digest in the SBOM must equal the one
in `images.txt`:

```bash
grep -o 'pkg:oci/[^"]*' baryon-controller-<version>.spdx.json | head -1
grep controller images.txt
```

Rust dependencies of a binary or image, from the data that cargo-auditable
embeds (syft shown; grype, trivy, and osv-scanner read the same data):

```bash
syft scan file:baryon-virtd-<version>-x86_64-linux-musl
syft scan registry:ghcr.io/wrkode/baryon-controller@<digest>
```

The tag signature: the release page shows the tag as Verified, and so does
`gh api repos/wrkode/baryon/git/tags/<tag-object-sha> --jq .verification`.
To check it with Git, list the maintainer's signing keys as allowed
signers:

```bash
curl -s https://api.github.com/users/wrkode/ssh_signing_keys \
  | jq -r '.[] | "william.rizzo@gmail.com " + .key' > allowed_signers
git -c gpg.ssh.allowedSignersFile=allowed_signers verify-tag <version>
```

Releases before v0.1.0-rc3 (v0.1.0-rc1, v0.1.0-rc2) were published from
the private repository: they have checksums and SBOMs but no
attestations, their image SBOMs name local tags rather than digests and
do not list Rust crates, and their tags are not signed.

What is signed: the attestations, by GitHub's attestation service for
the workflow run, and the release tags, by the maintainer's SSH key. No
file or image is signed with a long-lived key of the project.

Build stamps: every Baryon binary reports its version and the git commit
it was built from (`virtdGitCommit` on VirtualizationNodes, the `starting`
log line of each component). A build from a tree with uncommitted changes
carries `-dirty`; release builds come from the clean tag and never do.

## Making a release

1. All changes for the release are on `main`, signed off (DCO), and CI is
   green on the candidate commit: format, clippy, tests, CRD drift, the
   API server checks, dependency advisories and licenses (`cargo deny`),
   DCO, the image builds, and the dependency data in every binary.
2. The workspace version in `Cargo.toml` is the release version, and
   `docs/release-notes/v<version>.md` exists.
3. A dry run of the release workflow on `main` (`workflow_dispatch`)
   passes: the gate (all checks but the qualification status) and the
   complete build with its identity checks.
4. Release qualification on a real cluster passes (see
   [testing.md](testing.md)); hardware E2E does not run in GitHub-hosted
   CI, which has no KVM. It runs on the exact candidate commit: images,
   the host image, and the release files are built from a clean checkout
   of that commit, and the tag is placed on that commit. Any change after
   qualification, documentation included, makes a new candidate that is
   qualified again. Because the results cannot be part of the commit they
   qualify, they are recorded in [testing.md](testing.md) on `main` after
   the tag, naming the commit, and the release notes link there.
5. Architecture, security, and documentation reviews pass.
6. Record the qualification on the commit:
   `gh api repos/wrkode/baryon/statuses/<commit> -f state=success
   -f context=baryon/release-qualification -f description="<summary>"`.
7. Tag and push: `git tag -s -a v<version> -m "Baryon v<version>" <commit>`
   and `git push origin v<version>`. The workflow gates, builds, attests,
   publishes, and creates the GitHub release.
8. Smoke-test the published artifacts, not the pre-tag builds, without
   credentials, and record the result on `main`. A correction found after
   the tag goes to `main` and, where readers need it, into the GitHub
   release notes; the tag and the files never change.

### When the release workflow fails

If the gate, the build, or the attest job fails, nothing was published. A
transient failure, or a gate failure whose cause is fixed outside the
repository (for example a qualification status set after the tag), is
retried by re-running the workflow run; the tag stays where it is. A
failure that needs a change to the repository is fixed forward: correct
the problem on `main` and release a new release candidate with a new tag
(with its own workspace version and release notes), qualified again. The
tag ruleset prevents deleting or moving a pushed tag, and a tag name is
never reused: images and files that carry its name may already have been
pulled.

The publish job pushes the images before it creates the GitHub release.
If it fails after pushing images, re-run it: the pushes are of the same
OCI layouts, so the digests are the same. Images left from a run that is
abandoned stay under that tag's name; they are not part of any release.
