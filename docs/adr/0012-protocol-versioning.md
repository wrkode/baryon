# 0012: Runtime protocol versions, features, and negotiation

Status: Proposed
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
    major 1. A breaking change is a new package and a new major version.
    Peers of different major versions refuse each other (reason
    `ProtocolIncompatible`).
  - Minor versions only add fields, messages, RPCs, and features. They never
    change the meaning of an existing field or remove behavior a released
    feature promises. Peers ignore what they do not know.
  - 1.0 is the protocol of the first public release, which has no version
    fields. A peer that sends no version speaks 1.0.
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
  share the protocol major version, in any combination of minor versions. A
  VM that needs a feature runs only on hosts that implement it.

## Consequences

- Clusters can be upgraded in either order within a major version: the
  cluster release first, or the hosts first, node by node.
- A release that adds a feature should be rolled out to the hosts first. A VM
  that needs the feature and lands on a host without it is not ready with
  reason `UnsupportedFeature`. Placement by feature is Planned; until then
  the scheduler does not know which hosts implement which features.
- A future major version needs a transition plan when it is introduced: the
  node agent of the new release marks hosts of the old major version
  incompatible, so new VMs avoid them while running VMs continue.
- 1.0 anchors keep working with 1.1 hosts because baryon-virtd treats an
  attach without a version as 1.0 with the 1.0 features.

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
  from VirtualizationNode status.
- gRPC server reflection to detect RPCs. Rejected: it says which methods
  exist, not which VM capabilities work, and it would expose more of the
  socket's surface.

## Revisions
