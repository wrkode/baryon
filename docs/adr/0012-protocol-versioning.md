# 0012: Runtime protocol versions, features, and negotiation

Status: Accepted
Date: 2026-10-03

## Context

Baryon ships on two release trains. baryon-controller, baryon-node-agent,
and the anchor image are the cluster release, upgraded with the cluster
deployment. baryon-virtd and Cloud Hypervisor are part of the host image,
upgraded node by node with a Kairos A/B upgrade, often by Kairos Operator
over hours. Neither order is under Baryon's control, so a cluster runs mixed
versions during every upgrade.

Until this ADR the anchor sent the protobuf package name in each attach and
baryon-virtd refused any other value (ADR 0009). Within the package,
compatibility relied only on protobuf's additive rules: nothing told a peer
whether the other side implemented what a VM needed, and there was no way to
express a breaking change except changing the package.

## Decision

- The runtime protocol has a version `<major>.<minor>`.
  - The major version is the protobuf package. `baryon.runtime.v1alpha1` is
    major 1. A breaking change is a new package and a new major version;
    from major 2 on the packages are named `baryon.runtime.v2`,
    `baryon.runtime.v3`, and so on. Peers of different major versions
    refuse each other (reason `ProtocolIncompatible`).
  - Minor versions only add fields, messages, RPCs, and features. They never
    change the meaning of an existing field or remove behavior a released
    feature promises. Peers ignore what they do not know.
  - 1.0 is the protocol of the first published commit (a73f37f), which has
    no version fields. A peer that sends no version speaks 1.0.
  - 1.1 adds `ProtocolVersion`, `AnchorService.Negotiate`, the version and
    required features in `AttachRequest`, and the version and features in
    `RuntimeInfo`.
- Features are named capabilities that a VM needs from baryon-virtd, for
  example `disk.block`. A released name keeps its meaning. The 1.0 features
  are `boot.firmware`, `disk.file`, `disk.block`, and `network.pod`; a 1.0
  peer is treated as implementing exactly these. Constants and checks live in
  `crates/runtime-api/src/protocol.rs`.
- Per VM: the anchor computes the features its VM needs and calls
  `Negotiate` before `Attach`. A 1.0 baryon-virtd answers UNIMPLEMENTED and
  is treated as 1.0. If baryon-virtd lacks a feature or speaks another major
  version, the anchor does not attach and ends with reason
  `UnsupportedFeature` or `ProtocolIncompatible`, which becomes the reason
  of the VM's Ready condition. baryon-virtd checks the version and the
  required features again on `Attach` and refuses with
  `FAILED_PRECONDITION`.
- Per node: baryon-node-agent publishes the protocol version and features of
  baryon-virtd in VirtualizationNode `status.runtime`. When the host speaks
  another package or major version than the node agent, the node agent
  reports `RuntimeHealthy=False` and `Ready=False` with reason
  `ProtocolIncompatible` and marks its device plugin devices unhealthy, so
  kubelet sets the node's `virtualization.baryon.io/vm` allocatable to zero
  and the scheduler places no new Anchor Pods there. VMs already running on
  the node keep running.
- Values that the cluster release and the host compute independently are
  part of the major version: the VM host identity (`VmIdentity`, ADR 0011)
  and the guest MAC derivation. Changing either requires a new major
  version. The anchor spec (`BARYON_VM_SPEC`) is not covered: the controller
  and the anchor belong to the same cluster release.
- The host report keeps its own `report_version` (ADR 0009).
- Supported skew: a cluster release and a host image interoperate when they
  share the protocol major version, in any combination of minor versions.
  A VM that needs a feature the host lacks is refused before it starts
  (`UnsupportedFeature`); nothing steers it to a host that has the feature
  yet.
- Required features: no feature beyond the 1.0 set may become required by a
  VM until feature-aware placement exists. The planned mechanism is one
  placement-only extended resource per feature, served by the node agent
  only on hosts that implement the feature, like the
  `virtualization.baryon.io/import` resource that grants nothing (ADR
  0004).
- Host capability requirements (IOMMU, IOMMUFD, hugepages, NUMA, VFIO,
  Cloud Hypervisor features) are deferred. Baryon consumes none of them
  today (VFIO, hugepage-backed guest memory, and NUMA pinning are Not
  supported), and KVM and runtime health are already enforced by the
  `virtualization.baryon.io/vm` slot. Entry criterion: a capability gets a
  name when Baryon consumes it and it is tested. When one is needed, the
  mechanism is the one planned above for features: a per-capability
  placement-only extended resource served by the node agent, not Node
  labels written by a Baryon component (writer identity, staleness, and
  RBAC: `nodes/patch` cannot be limited to labels, ADR 0004). The
  requirement then belongs on VirtualMachineClass, not in the VM's
  `spec.placement` (ADR 0008). Consumable devices (VFIO, GPUs) go to DRA,
  never to this mechanism. NUMA stays with the node's Topology Manager;
  there is no NUMA field.
- Major transitions: a host of major N+1 keeps serving major N's
  `AnchorService.Negotiate` and `NodeService.GetHost` and `GetRuntimeInfo`
  (`RuntimeInfo` only) for at least one release, answering with its own
  version, so that major N clients report `ProtocolIncompatible` instead of
  reading UNIMPLEMENTED as 1.0.
- Cluster objects of the previous release: the controller of release N
  understands Anchor Pods of release N-1 that are still running: the reason
  vocabulary in their termination messages (for example
  `IdentityConflict`), the container names `anchor` and `import`, the
  annotations `virtualization.baryon.io/spec-hash` and
  `virtualization.baryon.io/restartedAt`, and, for disks imported by an
  earlier release, the claim annotation `virtualization.baryon.io/imported-from`
  together with the StorageReady=True that the earlier controller wrote
  (ADR 0006). Legacy paths are removed one release later.
- Support window: a cluster release supports Anchor Pods of the previous
  release and hosts of the same major version. The minimum host image for
  each cluster release is recorded in
  [compatibility.md](../compatibility.md#version-skew). Host-side safety
  fixes (for example the disk path checks and the block device registry in
  baryon-virtd) are not protocol features and are not negotiated: until
  releases are tagged, hosts should run an image built from the same commit
  as the cluster release or later.

## Consequences

- Clusters can be upgraded in either order within a major version: the
  cluster release first, or the hosts first, node by node.
- Until feature-aware placement exists, no VM requires more than the 1.0
  features, so `UnsupportedFeature` cannot occur between components that
  follow this rule; the check stays as the guard for later features.
- A future major version is introduced with the transition rule above: the
  node agent of the new release marks hosts of the old major version
  incompatible, so new VMs avoid them while running VMs continue, and old
  clients get a clear refusal from new hosts.
- 1.0 anchors keep working with 1.1 hosts because baryon-virtd treats an
  attach without a version as 1.0 with the 1.0 features. New anchors on 1.0
  hosts take the UNIMPLEMENTED path of `Negotiate` and attach as before.
- Evidence (version skew E2E on three Hadron nodes running k0s,
  2026-10-03): a 1.0 cluster release with mixed 1.0 and 1.1 hosts passed
  31 of 31 checks; a 1.1 cluster release with mixed hosts passed 31 of 31;
  a 1.0 anchor reattached to a 1.1 baryon-virtd after a restart; VMs of the
  old release kept running through the cluster release upgrade with
  `RestartRequired=True` (`AnchorImageChanged`); the node agent published
  `protocolVersion` 1.0 for old hosts and 1.1 with features for new ones.
  See [testing.md](../testing.md#results).

## Alternatives considered

- Exact equality of the package name only (the previous rule). Rejected: it
  can express only "identical" or "incompatible", so every change would
  break rolling upgrades.
- Comparing release versions of the components. Rejected: release numbers
  do not describe wire compatibility, and the two release trains are
  versioned independently.
- Feature labels on Node objects, set by the node agent, with node affinity
  on Anchor Pods. Deferred to the placement work: it needs write access to
  Node labels for the node agent or a controller that computes affinity
  from VirtualizationNode status. The placement work (2026-10-03) did not
  adopt it: capability placement, when needed, uses placement-only
  extended resources (Decision above).
- gRPC server reflection to detect RPCs. Rejected: it says which methods
  exist, not which VM capabilities work, and it would expose more of the
  socket's surface.

## Revisions

- 2026-10-03: added the major transition rule and the package naming from
  major 2, defined 1.0 as the protocol of the first published commit, the
  rule that no feature beyond 1.0 becomes required before feature-aware
  placement (replacing the sentence that a VM needing a feature runs only on
  hosts that implement it, which nothing enforced), the handling of
  previous-release Anchor Pods, the support window, and the mixed-version
  evidence.
- 2026-10-03: accepted after the architecture gate.
- 2026-10-03: the legacy import init container's `--url=` argument is no
  longer read as import evidence (ADR 0006); disks imported by earlier
  releases are recognized by the claim annotation and the earlier
  controller's StorageReady=True instead.
- 2026-10-03: host capability requirements were deferred with the VM
  placement work: the entry criterion for a capability name, the mechanism
  (placement-only extended resources, not Node labels), requirements on
  VirtualMachineClass, DRA for consumable devices, and no NUMA field.
  Anchor Pods of an earlier release have no
  `virtualization.baryon.io/placement-hash` annotation; the controller
  treats one as matching a VM without placement.
