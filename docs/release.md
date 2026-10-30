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
| `baryon-<image>-v<version>.spdx.json` | An SPDX SBOM of each image (`<image>` is `controller`, `node-agent`, `anchor`, `importer`, or `hadron-k0s`), generated from the image as published and naming its manifest digest; it lists the Rust crates compiled into the Baryon binaries |
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
| publish | `contents: write`, `packages: write` | Copies each OCI layout to ghcr.io unchanged (`skopeo copy --preserve-digests`), refuses to move a tag that exists with another digest, checks that the registry digest and the tag are the built digest, checks that every image can be pulled without credentials, and creates the GitHub release (a prerelease for a version with a prerelease part) |

The publish job checks out no repository code, and the attest job runs
before it, so nothing is published without its attestations. The build
job holds no write permission. The attest and publish jobs run in the
GitHub environment `release`, which only `v*` tags may use. The build
job's outputs (the SHA-256 of `SHA256SUMS`, of the image list, and of the
release notes, and each image digest), which GitHub keeps apart from the
workflow artifacts, bind what attest and publish receive to what the
build checked: both jobs compare the artifacts with them, and require the
release files to be exactly those listed in `SHA256SUMS`. The attest job
takes the image digests from those outputs. syft, which writes the SBOMs,
is downloaded by URL and checked against the SHA-256 pinned in
`packaging/versions.env`.

### Release gate

The gate fails closed: any check it cannot make fails the release.

- The tag equals `v` and the workspace version in `Cargo.toml`, and
  `docs/release-notes/v<version>.md` exists.
- The tag is annotated, points to the workflow's commit, and is signed
  by a key listed in the repository variable `RELEASE_ALLOWED_SIGNERS`
  (`git verify-tag`), and GitHub verifies that signature; no release of
  that tag exists yet (any other answer than not found fails the gate).
- The commit is reachable from `main` as fetched from GitHub, not from a
  local branch.
- The latest run of the CI workflow (`.github/workflows/ci.yaml`) on the
  push of that exact commit to `main` completed successfully.
- Release qualification passed on that exact commit: the commit has a
  `success` status with context `baryon/release-qualification`, set by a
  user account listed in the repository variable `RELEASE_QUALIFIERS`
  (today only the maintainer) after the qualification below. A dry run
  reports a missing status and continues.

The gate is code in the tagged commit, so it is a process control against
releasing a wrong or unqualified commit, not a security boundary: a
principal with write access to the repository could tag a commit whose
workflow omits it, and the result would still carry attestations for that
tag. What such a principal cannot produce is the tag signature, made with
a key on the maintainer's machine. Consumers therefore check the tag
signature against the pinned key and the commit's ancestry on `main`, as
well as the attestations ([Verifying a release](#verifying-a-release)).
The repository settings narrow it further: only administrators may create
`v*` tags, and only `v*` tags may use the `release` environment.

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
key (`git tag -s`). The key, pinned here and in the repository variable
`RELEASE_ALLOWED_SIGNERS`:

```text
william.rizzo@gmail.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJ53Xu8rRSofCQqb91XUgZqQam+5Q2e7tOwr0egG/W5x
```

Fingerprint `SHA256:ALS0VrMI2tXe5rUJX6PSHkkl432pIt1shD8+axLi/Zs`. The
signature shows that the maintainer created the tag; together with the
commit's ancestry on `main` it is the consumer's check that a release was
made by the maintainer, and the artifact attestations (below) tie each
file and image to the release workflow run of that tag. Releases before
v0.1.0-rc3 have unsigned tags.

Repository rules, configured on GitHub:

- `refs/tags/v*` (ruleset "release tags"): a tag cannot be deleted,
  moved, or force-updated, by anyone, including administrators. A
  mistaken tag is replaced by a new release candidate, never reused.
- `refs/tags/v*` (ruleset "release tag creation"): only repository
  administrators may create a release tag.
- The environment `release`, used by the attest and publish jobs, accepts
  only `v*` tags.
- `refs/heads/main` (ruleset "main history"): no deletion and no force
  push, so the history that the gate checks against cannot be rewritten.
- Immutable releases are enabled: once published, a release's tag and
  files cannot be changed or deleted; its title and notes can be edited.

Image tags on ghcr.io can be changed by anyone with package write
access; the digests in `baryon.yaml` and `images.txt` are what the
attestations cover and what an installation uses. A change to these rules
is made by the repository owner in the repository settings and recorded
here.

## Verifying a release

Everything below works without GitHub credentials unless it says
otherwise. `<version>` is the release, for example `v0.1.0-rc3`.

Checksums of the downloaded release files (`gh release download <version>
--repo wrkode/baryon` also downloads them, but needs a GitHub login):

```bash
base=https://github.com/wrkode/baryon/releases/download/<version>
curl -fsSLO "$base/SHA256SUMS"
for f in $(awk '{print $2}' SHA256SUMS); do curl -fsSLO "$base/$f"; done
sha256sum --check SHA256SUMS
```

The tag: check its signature against the pinned key above, and that its
commit is on `main`:

```bash
git clone https://github.com/wrkode/baryon.git && cd baryon
echo 'william.rizzo@gmail.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJ53Xu8rRSofCQqb91XUgZqQam+5Q2e7tOwr0egG/W5x' > ../allowed_signers
git -c gpg.ssh.allowedSignersFile=../allowed_signers verify-tag <version>
git merge-base --is-ancestor "<version>^{commit}" origin/main && echo "on main"
git rev-parse "<version>^{commit}"   # the <commit> for the commands below
```

Build provenance (GitHub artifact attestations, signed through Sigstore
with the release workflow's identity). Each attestation binds the
artifact's SHA-256 digest to the repository, the workflow file, the tag,
and the commit. With the GitHub CLI, which needs a GitHub login for this
command:

```bash
gh attestation verify baryon.yaml --repo wrkode/baryon \
  --signer-workflow wrkode/baryon/.github/workflows/release.yaml \
  --source-ref refs/tags/<version> --source-digest <commit> --deny-self-hosted-runners
gh attestation verify oci://ghcr.io/wrkode/baryon-controller@<digest> --repo wrkode/baryon \
  --signer-workflow wrkode/baryon/.github/workflows/release.yaml \
  --source-ref refs/tags/<version> --source-digest <commit> --deny-self-hosted-runners
```

Without a login, fetch the attestation bundle from the public API and
verify it offline:

```bash
digest="sha256:$(sha256sum baryon.yaml | cut -d' ' -f1)"   # or an image digest from images.txt
curl -s "https://api.github.com/repos/wrkode/baryon/attestations/$digest" \
  | jq '.attestations[0].bundle' > baryon.yaml.bundle.json
gh attestation verify baryon.yaml --bundle baryon.yaml.bundle.json --repo wrkode/baryon \
  --signer-workflow wrkode/baryon/.github/workflows/release.yaml \
  --source-ref refs/tags/<version> --source-digest <commit> --deny-self-hosted-runners
```

Image identity: `images.txt` names every image by digest, and
`baryon.yaml` uses the same digests. Each image SBOM names its image by
repository and manifest digest in a package URL, which encodes `/` as
`%2F` and `:` as `%3A`; the digest in the SBOM must equal the one in
`images.txt`:

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

Status: these commands are first exercised on a published release by
the smoke test of v0.1.0-rc3, without a GitHub login for everything but
the online `gh attestation verify` form; its result is recorded in
[testing.md](testing.md).

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
   passes: the gate (all checks but those of the tag and the qualification
   status) and the complete build with its identity checks. A dry run's
   artifacts can be downloaded from the public workflow run; they are not
   attested and not a release.
4. Release qualification on a real cluster passes (see
   [testing.md](testing.md)); hardware E2E does not run in GitHub-hosted
   CI, which has no KVM. Qualification proves the commit and the build
   procedure, not the published bytes: the release workflow builds again,
   with different digests, and only the smoke test of step 8 checks the
   published artifacts. It runs on the exact candidate commit: images,
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
tag rulesets prevent deleting or moving a pushed tag, and a tag name is
never reused: images and files that carry its name may already have been
pulled.

Once the publish job has started, use "Re-run failed jobs" only, which
reuses the build of the same run: the pushes are of the same OCI layouts,
so the digests are the same. Re-running all jobs would build again with
different digests; the publish job then refuses to move a tag that exists
with another digest, and the release has to be fixed forward. Images left
from a run that is abandoned stay under that tag's name; they are not part
of any release.
