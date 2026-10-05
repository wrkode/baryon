# 0008: v1alpha1 API surface and status ownership

Status: Accepted
Date: 2026-10-02, revised 2026-10-03, 2026-10-04, and 2026-10-05

## Context

The API must express user intent, map to a clear reconciliation boundary per
kind, and leave room for later features without exposing implementation
details. Runtime information exists on the VM's node (baryon-virtd); desired
state and scheduling information exist in the cluster.

## Decision

Group `virtualization.baryon.io`, version `v1alpha1`, three kinds:

| Kind | Scope | Writer of status | Purpose |
|---|---|---|---|
| VirtualMachine | Namespaced | baryon-controller only | user intent for one VM |
| VirtualMachineClass | Cluster | none | compute shape: vCPUs, memory; immutable |
| VirtualizationNode | Cluster, named after and owned by the Node | baryon-node-agent on that node only | whether the node can run Baryon VMs: runtime health and versions, heartbeat, host-side state of each VM by Anchor Pod UID |

VirtualMachine spec contains only fields with an implementation:
`className`, `runPolicy` (`Running` or `Stopped`), `rootDisk` (an HTTP
source imported into a claim Baryon creates, or an existing claim),
`networks` (a list-map keyed by a DNS-1123 label name; exactly one, the pod
network), and `placement`. CEL rules and schema limits enforce one disk
source; a size for imported disks, a whole number of bytes from 1Mi to
1Pi; `size` and `storageClassName` only for an `http` source; DNS-1123
subdomains for `className`, `storageClassName`, and `claimName`; an `http`
or `https` URL with a host and without credentials; a SHA-256 digest when
the URL is plain `http`; the single pod network; and an immutable
`rootDisk` (`self == oldSelf`, including `size`): the disk's content and
claim are bound to it, so another disk means a new VirtualMachine.
`rootDisk.size` is the size the guest sees in filesystem mode, and the
minimum in block mode: in filesystem mode the image is extended to exactly
that size and the claim requests 64 MiB more for the import marker and
filesystem metadata; in block mode the claim requests exactly that size
and the guest sees the whole device, which the provisioner may round
up.

VirtualMachineClass has `cpu.vcpus` (1 to 256, no statement about guest
CPU topology) and `memory.size` (a whole number of Mi from 64Mi to 16Ti).
Its `spec` is immutable: a new shape is a new class, and VMs move to it by
changing `className`, which applies at their next start
(`RestartRequired`).

`placement` uses the Kubernetes core/v1 types with their pod meaning:
`nodeSelector`, `affinity` (`Affinity`), `tolerations` (at most 32),
`topologySpreadConstraints` (at most 8), and `priorityClassName` (at most
253 characters). It is copied to the VM's pods (ADR 0004); `nodeName`,
`schedulerName`, and other PodSpec fields are not exposed. CEL rules refuse
a toleration without a key and tolerations of
`node.kubernetes.io/unschedulable`, `node-role.kubernetes.io/control-plane`,
and `node.kubernetes.io/out-of-service`; a `priorityClassName` starting
with `system-`; and `namespaces` or `namespaceSelector` in pod affinity and
anti-affinity terms, required and preferred, which therefore apply within
the VM's namespace. A placement change is not applied to a running VM: it
is reported through `RestartRequired`.

Restart is requested
with the `virtualization.baryon.io/restartedAt` annotation, following the
`kubectl rollout restart` convention. A value that differs from the one
the current Anchor Pod was created with restarts a running VM exactly
once; the value is opaque; removing the annotation does nothing; a
stopped VM is not affected. The value is not part of the spec hash that
decides `SpecChanged`.

Runtime status reaches the controller through the Anchor Pod, written only by
kubelet:

- anchor container readiness: the guest produced boot evidence. While the
  anchor cannot reach baryon-virtd (for example during its restart, which
  leaves the VM running) it keeps the last state for 30 s before reporting
  not ready.
- anchor container restarts: VM restarts in this pod
- anchor container termination message: `Reason: detail` from baryon-virtd,
  for example `VmmKilled: VMM was killed by signal 9`
- pod IPs and node

Host-side state that the anchor does not report reaches the controller
through the VirtualizationNode of the Anchor Pod's node. `status.vms` lists,
per Anchor Pod UID, the state of the VM's guest network service (the DHCP
responder, ADR 0007) as baryon-virtd reports it (runtime protocol 1.2, ADR
0012). It carries no VM names, namespaces, addresses, PIDs, paths, or TAP
names, and at most 256 entries. baryon-controller reads it from a watch and
stays the only writer of VirtualMachine status. A node's report counts only
while the controller has seen the node's heartbeat change within the last
90 s, measured on the controller's own clock, so clock skew between nodes
and the controller does not matter. The anchor does not report this
itself: it has no API access, and its single readiness probe already
carries guest readiness.

The import state reaches the controller through the import Pod
(`<vm>-import`), also written only by kubelet: its importer container state
and termination message, and its `PodScheduled` condition while it cannot
be placed. A completed import is recorded on the claim's PersistentVolume,
not in the VirtualMachine (ADR 0006). The importer init container of Anchor
Pods of earlier releases is no longer read.

`status.rootDiskImport.claimUID` records that the root disk claim with that
UID held an imported or used disk. It is a one-way marker written by
baryon-controller that only prevents another import into the same claim;
it is reset when the claim is recreated. It is not an API guarantee: the
PersistentVolume record is the authority (ADR 0006).

The controller derives VirtualMachine Conditions (`Scheduled`,
`StorageReady`, `Running`, `Ready`, `RestartRequired`, `NetworkReady`) from
these and from the root claim, and writes status with server-side apply. Conditions are a
list-map keyed by `type`. There is no phase field; `kubectl get` shows Ready
and its reason. An existing Anchor Pod decides `Running` and `Ready`; class
or spec problems show only when there is no pod. `Running` means the Anchor
container runs (it runs or starts the VMM); the controller cannot see the
VMM itself. `RestartRequired` distinguishes `SpecChanged` (spec other than placement, or class),
`PlacementChanged` (`spec.placement`, compared through the Anchor Pod's
`virtualization.baryon.io/placement-hash`; an Anchor Pod without the
annotation matches a VM without placement), and `AnchorImageChanged` (a
newer runtime from a controller upgrade). When the import Pod or the Anchor
Pod cannot be scheduled, `Scheduled` and `Ready` are False with the
scheduler's reason and message (cut to 1024 bytes); when the API server
refuses to create one (HTTP 422 or 403), the reason is `PodRejected` with
the API server's message, and when it refuses the root disk claim Baryon
creates, `ClaimRejected`. An empty `placement: {}` is treated like no
placement, for scheduling and for `PlacementChanged`.
`NetworkReady` is derived from the node's report only while `Running` is
True, and does not change `Ready`. Its reason comes from the reported
state, never from the node's text; the node's message is capped at 1024
bytes. The reasons are listed in [api.md](../api.md#networkready).

VirtualizationNode status holds the conditions `Ready` (baryon-virtd
reachable, a compatible runtime protocol, KVM usable; True reason
`RuntimeReady`) and `RuntimeHealthy`, `heartbeatTime`, `runtime`
(`virtdVersion`, `hypervisorVersion`, and the internal `protocolVersion`
and `features`), `vmCount`, and the internal `vms`. A node agent that
stops sets both conditions to Unknown (`NodeAgentStopped`); a crashed one
cannot, so consumers count the conditions only while `heartbeatTime` is
within 90 s.

List semantics are declared for every status list: VM `addresses` is
atomic with `type` limited to `InternalIP`, `identityConflictNodes` and
`runtime.features` are sets, conditions and VirtualizationNode `vms` are
maps (keyed by `type` and `anchorPodUID`). Optional fields are not
nullable: no Baryon field distinguishes null from absent.

Anchor Pods carry the labels `virtualization.baryon.io/vm-name`,
`virtualization.baryon.io/vm-uid`, and `app.kubernetes.io/managed-by=baryon`;
root disk claims Baryon creates carry `virtualization.baryon.io/vm-name` and
`app.kubernetes.io/managed-by=baryon`. The VM's own labels, except reserved
keys, are copied to its Anchor Pod and import Pod (ADR 0007): reserved are
`app.kubernetes.io/managed-by` and keys whose prefix is `baryon.io`,
`kubernetes.io`, or `k8s.io`, or a subdomain of one of them other than
`app.kubernetes.io`.
Each VM executes as a distinct unprivileged host identity, stable for the
life of the VirtualMachine object (ADR 0011); how it is allocated is not
part of the API. The guest MAC, SMBIOS UUID, and NoCloud instance-id derive
from `metadata.uid`. Both are part of the runtime protocol (ADR 0009).

When baryon-virtd refuses a VM on a node because another VM there has the
same host identity, the controller records the node in
`status.identityConflictNodes` (at most 16 entries), sets `Ready=False` with
reason `IdentityConflict`, emits an `IdentityConflict` event, replaces the
Anchor Pod, and keeps the new pod off the recorded nodes with required node
affinity on `metadata.name`. The list is cleared when the VM is stopped.

Status never contains PIDs, socket paths, TAP names, host paths, or Cloud
Hypervisor configuration.

Namespace users get access through two ClusterRoles in
`deploy/base/tenant/roles.yaml`, aggregated into the Kubernetes default
roles: `baryon-vm-edit` (into `edit` and `admin`: every verb on
virtualmachines, read on virtualmachineclasses) and `baryon-vm-view` (into
`view`: read on both). Neither grants `virtualmachines/status`, so
baryon-controller stays its only writer, and neither grants anything on
PersistentVolumes.

## Consequences

- One writer per object; no node component holds write access to
  VirtualMachines.
- A compromised node agent can affect only the `NetworkReady` condition of
  VMs whose Anchor Pod runs on its own node: `baryon-node-agent-own-node`
  limits its writes to that node's VirtualizationNode.
- `NetworkReady` reflects the node agent's last report, so it lags the
  host by up to one node agent interval (at most 30 s).
- Runtime detail is limited to what fits a reason and message; detailed
  runtime state stays on the node (baryon-virtd logs, console logs).
- `rootDisk` cannot be changed in place. Disk growth needs an explicit flow
  (Planned).
- New fields require an implementation. Direct kernel boot, additional
  disks, Multus networks, CPU pinning, hugepages, and VFIO are Designed or
  Planned and are not in the schema.
- Host capability requirements (IOMMU, hugepages, NUMA, VFIO, runtime
  protocol features) are not in the schema. Where a requirement goes in
  the API is decided by the ADR that ships the capability; it is not
  committed to VirtualMachineClass in advance. With Dynamic Resource
  Allocation, device requests would be claims per VM, not a property of
  a shared class (ADR 0012).
- VirtualizationNode publishes no host inventory: Kubernetes publishes
  what the scheduler uses on the Node, devices belong to Dynamic Resource
  Allocation, and the node agent logs the rest. Its conditions are valid
  only while its heartbeat is recent (90 s), because a crashed node agent
  cannot update them.
- Which parts of the API are contract, the v0.1 freeze, and the change log
  per release are in [api-stability.md](../api-stability.md).
- `placement` is bounded by the cluster's own controls: namespace admission
  (PodNodeSelector, PodTolerationRestriction) and ResourceQuota scopes see
  the VM's pods in the VM's namespace.
- `runPolicy: Stopped` deletes the Anchor Pod. A stopped VM consumes no node
  resources.

## Alternatives considered

- baryon-node-agent writing runtime conditions into VirtualMachine status
  (the first draft of this ADR). Rejected after review: two writers, a
  cluster-wide write permission for every node agent, stale conditions when a
  node dies, and a second source for guest readiness next to the anchor
  readiness probe.
- A `phase` enum. Rejected: Kubernetes API conventions discourage phases.
- Runtime status on a separate per-VM object. Rejected: one more kind to
  reconcile and garbage collect.
- Embedding Cloud Hypervisor configuration in the spec. Rejected by ADR 0002.

## Revisions

- 2026-10-02 (draft, before the first commit): runtime conditions moved from
  node-agent writes to controller-only derivation after review (see
  Alternatives).
- 2026-10-03: the digest rule for plain HTTP sources; after the architecture
  gate, status precedence, the meaning of `Running`, the two RestartRequired
  reasons, the readiness grace during a baryon-virtd restart, the label key
  `virtualization.baryon.io/vm-name` (previously the same string as the
  extended resource), and `networks` as a list-map with bounded names.
- 2026-10-03: `status.identityConflictNodes` and the `IdentityConflict`
  handling (ADR 0011); the host identity is specified by ADR 0011 instead
  of here.
- 2026-10-03: `rootDisk` became immutable, including `size`; the import
  state is read from the import Pod instead of an Anchor Pod init container
  (ADR 0004).
- 2026-10-03: `status.rootDiskImport` (ADR 0006), the tenant roles
  `baryon-vm-edit` and `baryon-vm-view`, and the removal of the import
  state read from Anchor Pod init containers of earlier releases.
- 2026-10-03: `spec.placement` with the Kubernetes types `nodeSelector`,
  `Affinity`, `Toleration`, `TopologySpreadConstraint`, and
  `priorityClassName`, its CEL rules, the `PlacementChanged` restart
  reason, the scheduler's reason and message on `Scheduled` and `Ready`,
  `PodRejected`, the copy of the VM's labels to its pods, and the rule that
  capability requirements, when they ship, belong on VirtualMachineClass.
- 2026-10-04: the `NetworkReady` condition and VirtualizationNode
  `status.vms` (host-side state per Anchor Pod UID, read by the controller,
  which stays the only writer of VM status), the 90 s staleness rule on the
  controller's clock, and why the anchor does not report this state.
- 2026-10-05 (v0.1 API review; the API stays `v1alpha1`):
  - `restartedAt`: a new non-empty value restarts a running VM exactly
    once; removing the annotation is a no-op (it reported `SpecChanged`
    before, a bug); the value is opaque, has no effect on a stopped VM, and
    does not change `metadata.generation`. The value left the spec hash;
    Anchor Pods created by a restart request of an earlier release still
    match.
  - `rootDisk` validation, `rootDisk` staying immutable: `size` a whole
    number of bytes from 1Mi to 1Pi; `size` and `storageClassName` only
    for an `http` source; DNS-1123 subdomains for `storageClassName`,
    `persistentVolumeClaim.claimName`, and `className`; an `http.url` with
    a host and without credentials.
  - The reason `ClaimRejected` for a root disk claim the API server
    refuses, reported on the VM instead of only logged.
  - `rootDisk.size` semantics: filesystem claims request `size` plus
    64 MiB and the image is extended to exactly `size`; block claims
    request exactly `size` (before, also 64 MiB more).
  - VirtualMachineClass `cpu.cores` renamed `cpu.vcpus`, `memory.size`
    validated in the schema, and the class `spec` immutable, with a
    one-time update allowed for a class stored in the earlier shape.
  - VirtualizationNode purpose narrowed to whether the node can run
    Baryon VMs, its runtime versions, and its runtime health. Removed
    `runtime.protocol`, `runtime.hypervisor`, `architecture`,
    `kernelRelease`, `osImage`, `kairosVersion`, `cpu`, `memoryBytes`,
    `kvm`, `iommu`, `vfio`, `numaNodes`, `hugepages`, and `pciDevices`:
    they repeated Node status or described unsupported features; device
    inventory belongs to Dynamic Resource Allocation; the node agent logs
    the host inventory instead.
  - VirtualizationNode staleness: `NodeAgentStopped` on SIGTERM and the
    90 s heartbeat rule for consumers; the `Ready` True reason renamed
    `KvmAvailable` to `RuntimeReady`; an unreadable host report is
    `ProtocolIncompatible` instead of `RuntimeUnreachable`.
  - List semantics on every status list, and no nullable optional fields.
  - An empty `placement: {}` places and hashes like no placement.
  - `Running` documented as the Anchor container of the current Anchor
    Pod running, not evidence that the guest runs.
  - Capability and device requirements are no longer committed to
    VirtualMachineClass (replacing the 2026-10-03 rule): the ADR that ships
    a capability decides where its requirement goes; with Dynamic Resource
    Allocation, device requests would be per-VM claims.
  - The contract, the v0.1 freeze, and the change log are recorded in
    [api-stability.md](../api-stability.md).
- 2026-10-05: the v0.1 API freeze (docs/api-stability.md) is in effect
  from v0.1.0-rc1: no breaking API change before v0.1.0 unless it fixes a
  correctness or security problem.
