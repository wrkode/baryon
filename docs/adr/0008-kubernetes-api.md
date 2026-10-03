# 0008: v1alpha1 API surface and status ownership

Status: Accepted
Date: 2026-10-02, revised 2026-10-03

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
| VirtualMachineClass | Cluster | none | compute shape: vCPUs, memory |
| VirtualizationNode | Cluster, named after and owned by the Node | baryon-node-agent on that node only | host capabilities, runtime health, heartbeat |

VirtualMachine spec contains only fields with an implementation:
`className`, `runPolicy` (`Running` or `Stopped`), `rootDisk` (an HTTP
source imported into a claim Baryon creates, or an existing claim), and
`networks` (a list-map keyed by a DNS-1123 label name; exactly one, the pod
network). CEL rules enforce one disk source, a size for imported disks, an `http` or `https` URL, a SHA-256 digest when
the URL is plain `http`, and the single pod network. Restart is requested
with the `virtualization.baryon.io/restartedAt` annotation, following the
`kubectl rollout restart` convention.

Runtime status reaches the controller through the Anchor Pod, written only by
kubelet:

- anchor container readiness: the guest produced boot evidence. While the
  anchor cannot reach baryon-virtd (for example during its restart, which
  leaves the VM running) it keeps the last state for 30 s before reporting
  not ready.
- anchor container restarts: VM restarts in this pod
- anchor container termination message: `Reason: detail` from baryon-virtd,
  for example `VmmKilled: VMM was killed by signal 9`
- import init container state and termination message
- pod IPs and node

The controller derives VirtualMachine Conditions (`Scheduled`,
`StorageReady`, `Running`, `Ready`, `RestartRequired`) from these and from the
root claim, and writes status with server-side apply. Conditions are a
list-map keyed by `type`. There is no phase field; `kubectl get` shows Ready
and its reason. An existing Anchor Pod decides `Running` and `Ready`; class
or spec problems show only when there is no pod. `Running` means the Anchor
container runs (it runs or starts the VMM); the controller cannot see the
VMM itself. `RestartRequired` distinguishes `SpecChanged` (spec or class)
from `AnchorImageChanged` (a newer runtime from a controller upgrade).

Anchor Pods carry the labels `virtualization.baryon.io/vm-name`,
`virtualization.baryon.io/vm-uid`, and `app.kubernetes.io/managed-by=baryon`;
root disk claims Baryon creates carry `virtualization.baryon.io/vm-name` and
`app.kubernetes.io/managed-by=baryon`.
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

## Consequences

- One writer per object; no node component holds write access to
  VirtualMachines.
- Runtime detail is limited to what fits a reason and message; detailed
  runtime state stays on the node (baryon-virtd logs, console logs).
- New fields require an implementation. Direct kernel boot, additional
  disks, Multus networks, CPU pinning, hugepages, and VFIO are Designed or
  Planned and are not in the schema.
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
