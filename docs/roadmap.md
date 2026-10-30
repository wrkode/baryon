# Roadmap

This page lists what Baryon intends to work on, in the order it is
expected to happen. It records intent, not commitments: an item can move,
change, or be dropped when its design or upstream facts say so. Each item
carries a status label from [architecture.md](architecture.md), or Blocked
upstream for an item that waits on a change outside Baryon; an item is
removed from this page once it is released and documented in the release
notes.

Support claims are made only in [compatibility.md](compatibility.md) and
the release notes, after tests. A Planned or Designed item here is not
supported.

Last updated: 2026-10-30.

## v0.1.0

v0.1.0-rc1 and v0.1.0-rc2 are published
([v0.1.0-rc1](release-notes/v0.1.0-rc1.md),
[v0.1.0-rc2](release-notes/v0.1.0-rc2.md)); v0.1.0-rc2 carries Cloud
Hypervisor v54.0 and the fixes M1 and M2 (Implemented). v0.1.0-rc3
carries the items under "On main" below and no new virtualization
feature ([release notes](release-notes/v0.1.0-rc3.md)); v0.1.0 is planned
as the same functional scope. Before v0.1.0:

| Item | Status | Notes |
|---|---|---|
| Installation from a registry mirror or with node pull credentials | Planned | Not tested; the public installation needs neither, and [installation.md](installation.md#restricted-registries-and-mirrors) documents loading the images onto the nodes |

## On main, for v0.1.0-rc3

| Item | Status | Notes |
|---|---|---|
| Public repository, releases, and images | Implemented | Anonymous installation from the public release ([installation.md](installation.md)) |
| Release workflow with separated privileges and a fail-closed gate | Implemented | Gate, build, attest, publish jobs; tag, version, `main` ancestry, CI, and qualification checks ([release.md](release.md#release-workflow)) |
| Build provenance attestations | Implemented | For every release file and image digest ([release.md](release.md#verifying-a-release)) |
| Rust dependency data in every Baryon binary and image SBOM | Implemented | cargo-auditable; image SBOMs bound to the published digests |
| Signed release tags, tag and branch rulesets, immutable releases | Implemented | [release.md](release.md#tags-and-releases) |
| Host access policy for every Baryon workload | Implemented | `baryon-vm-resource` and `baryon-system-pods` ([security.md](security.md#host-integration-policy)) |
| Refusing form of `baryon-vm-guest-config` | Implemented | ADR 0014; installed ahead of `spec.guestConfig`, admits every VirtualMachine of v0.1 |
| Identity conflict E2E and recovery | Implemented | `tests/e2e/identity-conflict.sh`; recovery of a v0.1.0-rc1 anchor in [troubleshooting.md](troubleshooting.md#identityconflict) |

## Next, after v0.1 stabilization: guest access

| Item | Status | Notes |
|---|---|---|
| Guest configuration from a Secret (`spec.guestConfig`, NoCloud seed disk) | Designed | [ADR 0014](adr/0014-guest-access.md) (Accepted); needs the feature placement resource of ADR 0012. The refusing form of `baryon-vm-guest-config` ships from v0.1.0-rc3, so the release that adds the field requires a cluster at v0.1.0-rc3 or later |
| Interactive serial console through the Anchor Pod (`pods/exec`, one interactive writer) | Designed | [ADR 0014](adr/0014-guest-access.md) (Accepted) |
| `baryonctl console` | Planned | Designed in [ADR 0014](adr/0014-guest-access.md) (Accepted), as a later part after the console through `kubectl exec`; also installable as the kubectl plugin `kubectl-baryon` |
| Guest network throughput measurement in the E2E suites | Planned | Needs guest credentials, which the guest configuration item provides; until then `tests/e2e/guest-policy.sh` has an optional throughput floor (`MIN_MBPS`) for a guest image that has credentials |

## Storage

| Item | Status | Notes |
|---|---|---|
| More than one disk per VM | Planned | Not supported in v0.1 |
| Growing a root disk | Planned | [storage.md](storage.md) |
| Import of qcow2 and compressed images | Planned | Cloud Hypervisor runs raw images only in Baryon |
| `ReadWriteOncePod` for claims Baryon creates | Planned | [security.md](security.md) |
| Block device identity recorded and verified across VMM recovery | Planned | [security.md](security.md) |

## Security and release hardening

| Item | Status | Notes |
|---|---|---|
| VMM and DHCP responder in a PID namespace of their own, so they cannot signal the anchor | Planned | Residual of [ADR 0015](adr/0015-anchor-authority.md); whether Cloud Hypervisor runs correctly as PID 1 is not verified |
| A seccomp filter in baryon-anchor after startup that denies `execve`, `execveat`, and new processes | Planned | [security.md](security.md) |
| A per-pod limit on AnchorService connections and streams | Designed | Required before the console ships ([ADR 0014](adr/0014-guest-access.md), finding M11) |
| Device plugin in a separate minimal process | Planned | Finding M2 of the 2026-10-05 release gate review in [security.md](security.md#review-findings) |
| Allowlist seccomp filter and Landlock rules for the DHCP responder | Designed | [security.md](security.md#baryon-seccomp-layer) |
| Remove virtiofsd from the host image until Baryon uses it | Planned | |
| Decide on `offload_ufo` for the guest NIC | Planned | Finding L3 of 2026-10-27 in [security.md](security.md#review-findings): checksum and TSO offloads accepted, UFO Open |
| Nested virtualization disabled in the host's KVM module (`kvm_intel.nested=0`, `kvm_amd.nested=0`) | Planned | |
| AppArmor or SELinux profiles for baryon-virtd and the VMM | Planned | |
| Kubernetes user namespaces for the Anchor container | Not supported | Admission refuses `hostUsers: false` on Baryon pods because the Anchor supervisor check requires the initial user namespace ([ADR 0015](adr/0015-anchor-authority.md)). Adopting them needs a new decision, which would also need a `/dev` check that works with bind-mounted device nodes |
| An allowlist `.dockerignore` | Planned | |

## Operations

| Item | Status | Notes |
|---|---|---|
| Further `baryonctl` commands (start, stop, status) | Planned | After the console |
| Metrics for VM launch duration, reconnects, and reconcile errors | Planned | A metrics endpoint needs a decision on a TCP listener, which the current security model excludes |
| VirtualizationNode condition reporting whether graceful node shutdown is configured | Planned | [hadron.md](hadron.md) |
| Hadron releases after v0.5.3 | Planned | Each needs the host image build and the E2E suites on the k0s cluster |

## Larger features

| Item | Status | Notes |
|---|---|---|
| Device passthrough through Kubernetes DRA | Planned | [design-notes.md](design-notes.md) |
| More than one network interface per VM | Planned | |
| Hugepage-backed guest memory and NUMA placement | Planned | |
| Snapshots and cloning | Planned | |
| aarch64 hosts | Planned | |
| Kubernetes distributions other than k0s (kubeadm, k3s) | Planned | [compatibility.md](compatibility.md#distribution-support-policy) |

## Waiting on upstream

Items here are Blocked upstream: they wait on a change outside Baryon.

| Item | Status | Upstream dependency | Notes |
|---|---|---|---|
| Live migration | Blocked upstream | Cloud Hypervisor: network devices passed as file descriptors to a migration destination (v54.0 accepts pre-opened VFIO and `iommufd` descriptors on `receive-migration`, but no network descriptors). Most of ADR 0013's entry criteria are Baryon design work (fencing, authorization, the receive endpoint, disks and host paths on the destination, Kairos Operator cooperation) | [ADR 0013](adr/0013-live-migration.md#decision) |
| Upgrade completion detection without the runbook's boot ID check | Blocked upstream | Kairos Operator reporting the reboot of a node | [kairos-integration.md](kairos-integration.md) |

## Out of scope

These follow from the project's boundaries ([architecture.md](architecture.md))
and are not planned: a VMM other than Cloud Hypervisor or a VMM plugin
layer, an OS installer or ISO builder, an OS upgrade controller, a CNI or
CSI implementation, and a Kubernetes distribution.
