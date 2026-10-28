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

Last updated: 2026-10-28.

## v0.1.0

v0.1.0-rc1 is published
([release notes](release-notes/v0.1.0-rc1.md)). v0.1.0-rc2 carries the
items under "On main" below and no new user-facing feature
([release notes](release-notes/v0.1.0-rc2.md)); v0.1.0 is planned as the
same stabilized set. Before v0.1.0:

| Item | Status | Notes |
|---|---|---|
| Repository and package visibility | Planned (owner decision) | A public repository enables provenance attestations and anonymous image pulls ([release.md](release.md)) |
| Installation from a private registry with node pull credentials | Planned | Not tested; [installation.md](installation.md#private-repository) documents the image import alternative |
| Rust dependency data in the component image SBOMs | Planned | Today the component image SBOMs list the image only; the crates are in the source tree SBOM ([release.md](release.md)) |

## On main, for v0.1.0-rc2

| Item | Status | Notes |
|---|---|---|
| Cloud Hypervisor v54.0 | Implemented | Pinned in `packaging/versions.env`. Nested virtualization off and virtio-net offloads on, both written explicitly (ADR 0007, [compatibility.md](compatibility.md)); E2E suites except version skew on the k0s cluster ([testing.md](testing.md#cloud-hypervisor-v540)); `tests/e2e/guest-policy.sh` checks the options |
| M1: lifecycle control only by the Anchor supervisor | Implemented | [ADR 0015](adr/0015-anchor-authority.md); unit tests, CI API server checks, and E2E on the k0s cluster |
| M2: disk import only in `baryon-importer` | Implemented | Finding M2 of 2026-10-27 in [security.md](security.md#review-findings); unit tests and E2E on the k0s cluster |

## Next, after v0.1 stabilization: guest access

| Item | Status | Notes |
|---|---|---|
| Guest configuration from a Secret (`spec.guestConfig`, NoCloud seed disk) | Designed | [ADR 0014](adr/0014-guest-access.md) (Accepted); needs the feature placement resource of ADR 0012 and the refusing form of `baryon-vm-guest-config` one cluster release earlier |
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
| Separate build and publish jobs in the release workflow | Planned | Only the publish job would hold write tokens |
| Release workflow check of CI results and `main` ancestry; protected release tags | Planned | Finding L1 of the 2026-10-05 release gate review, accepted for v0.1.0-rc1 ([security.md](security.md#review-findings)) |
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
