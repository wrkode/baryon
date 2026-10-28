# API stability

This document says which parts of the Baryon API you can rely on, what
"alpha" means for them, and how API changes are announced. The field
reference is [api.md](api.md); the design is ADR 0008.

## Scope and version

| Item | Value |
|---|---|
| API group | `virtualization.baryon.io` |
| Version | `v1alpha1` |
| Kinds | VirtualMachine (namespaced), VirtualMachineClass (cluster-scoped), VirtualizationNode (cluster-scoped) |
| Served and stored versions | one: `v1alpha1` is the only served version and the storage version |
| Conversion webhook | none |
| Storage version migration | none |

No conversion webhook, second version, or storage version migration
exists, and none is implied by this document. They are Not supported
until they are implemented and tested. Because there is a single stored
version, a schema change applies to every stored object the next time it
is read: the API server drops fields that the new schema no longer has.

## What alpha means

`v1alpha1` comes with no backward compatibility guarantee. Between
releases, fields, defaults, validation rules, condition types, and
condition reasons may change.

What is guaranteed: every release that changes the API documents the
upgrade path, in the [change log](#change-log-per-release) below and in
[compatibility.md](compatibility.md#version-skew). How a cluster release
handles objects that an earlier release created, such as running Anchor
Pods of the previous release, follows ADR 0012.

## Frozen for v0.1

Status: in effect from v0.1.0-rc1.

From v0.1.0-rc1 on, no breaking API change is made before v0.1.0, and
after v0.1.0 none before the next minor release, unless it fixes a
correctness or security problem. Such a change is listed in the
[change log](#change-log-per-release) with its upgrade path.

A breaking change is any of:

- removing or renaming a field, a condition type, a documented reason, a
  label, or an annotation that [The contract](#the-contract) lists
- tightening validation so that an object that was valid before is
  rejected
- changing a default
- changing the meaning of a field, a condition, or a documented reason

A correctness or security exception gets a release note in the change log
below and a revision of ADR 0008.

Allowed during the freeze: new optional fields, validation that accepts
more objects than before, and new condition reasons. A new reason may
cover a situation that had no specific reason before. It may not replace
a documented reason for the same situation: that changes the meaning of
the documented reason and is breaking. Clients must treat a reason they
do not know as the condition's status alone: a new reason does not change
what `True`, `False`, or `Unknown` mean.

Runtime protocol during the 0.1 series: it stays at major version 1,
and new minor versions may add only optional status. A 0.1.x release
must not make a runtime feature required unless feature-aware placement
ships in the same release (ADR 0012), so that any 1.x host keeps
running every VM of any 0.1.x cluster release
([compatibility.md](compatibility.md#which-combinations-work)).

"Frozen" does not mean permanent stability. `v1alpha1` stays alpha, and
the next minor release may change the API, with a documented upgrade path.

## The contract

These parts of the API are what this document's guarantees cover.

VirtualMachine:

- every `spec` field and its validation, as listed in
  [api.md](api.md#spec)
- `status.observedGeneration`
- `status.conditions`: the condition types, the meaning of `True`,
  `False`, and `Unknown` for each type, and the documented Baryon reasons
  ([condition contract](api.md#condition-contract))
- `status.nodeName`, `status.anchorPod`, `status.addresses`,
  `status.macAddress`, `status.rootDiskClaim`, `status.restartCount`
- the `virtualization.baryon.io/restartedAt` annotation and its semantics
  ([api.md](api.md#restarting-a-vm))
- the labels `virtualization.baryon.io/vm-name` and
  `virtualization.baryon.io/vm-uid` on the objects Baryon creates,
  `virtualization.baryon.io/import-for` on import Pods (for example for
  NetworkPolicies), `app.kubernetes.io/managed-by: baryon` on the objects
  Baryon manages, and the rule for copying the VM's own labels to its
  pods, including the reserved keys
  ([api.md](api.md#labels-on-objects-baryon-creates))
- the root disk claim layout: `disk.img` at the volume root in filesystem
  mode, the whole device in block mode

VirtualMachineClass:

- `spec.cpu.vcpus` and `spec.memory.size` with their validation
- the immutability of `spec`

VirtualizationNode:

- `status.conditions`: `Ready` and `RuntimeHealthy`, the meaning of each
  status, and the documented Baryon reasons
- `status.heartbeatTime`
- `status.vmCount`
- `status.runtime.virtdVersion` and `status.runtime.hypervisorVersion`
- `status.runtime.virtdGitCommit` as an optional, informational string:
  the field and its meaning (the build of baryon-virtd) are contract, its
  format is not

## Not contract

These can change or disappear in any release, including during the v0.1
freeze. Do not build on them.

- VirtualMachine `status.rootDiskImport` and
  `status.identityConflictNodes`
- VirtualizationNode `status.vms`, `status.runtime.protocolVersion`, and
  `status.runtime.features`. Users read the VM's `NetworkReady` condition
  instead of `status.vms`, and the node's `RuntimeHealthy` condition
  instead of the protocol version.
- reasons that Baryon copies from kubelet and the scheduler, for example
  `Unschedulable`, `SchedulingGated`, `ContainerCreating`,
  `CrashLoopBackOff`, `ErrImagePull`, and `CreateContainerConfigError`
- condition and Event message text. Messages are for people, not for
  parsing.
- names of the objects Baryon derives from the VM name, such as
  `<vm>-anchor`, `<vm>-import`, and the root disk claim. Read
  `status.anchorPod` and `status.rootDiskClaim` instead.
- the annotations `virtualization.baryon.io/disk-import`,
  `virtualization.baryon.io/placement-hash`,
  `virtualization.baryon.io/vm-label-keys`, the import approval, and
  `virtualization.baryon.io/imported-from`, and the spec hash annotation
  on Anchor Pods
- the structure of the Anchor Pod and the import Pod (containers,
  volumes, probes, environment), the names and slot counts of the
  extended resources (`virtualization.baryon.io/vm` and
  `virtualization.baryon.io/import`), the host identity range, and
  anything in the Cloud Hypervisor invocation. Because the resource names
  are not contract, a ResourceQuota on them (for example
  `requests.virtualization.baryon.io/vm`) is Not supported; to cap the
  number of VMs in a namespace, use an object count quota,
  `count/virtualmachines.virtualization.baryon.io`
- Event reasons. They follow the condition reasons where they overlap,
  but are informational.

Anything that the contract does not list is not contract.

## Reading conditions

The condition contract, with the owner, the meaning of each status, the
reasons, and the transition rules of every condition, is in
[api.md](api.md#condition-contract).

VirtualizationNode conditions count only while `status.heartbeatTime` is
recent: within 90 s, three maximum node agent intervals of 30 s. When the
heartbeat is older, treat `Ready` and `RuntimeHealthy` as Unknown,
whatever they say. A node agent that is stopped (SIGTERM) tries to set
both to Unknown with reason `NodeAgentStopped`; the write is best effort
(5 s during shutdown). A node agent that crashes or whose node fails
leaves its last conditions, and only the age of the heartbeat shows it. The heartbeat is written with the node's clock, so allow for
clock skew between the node and the reader. baryon-controller avoids clock
skew by measuring on its own clock whether it saw the heartbeat change
within 90 s.

For VirtualMachine conditions, compare `status.observedGeneration` with
`metadata.generation`: conditions computed for an earlier generation do
not yet reflect the latest spec change.

## Status labels

Status labels in this document and in the rest of the documentation
follow [architecture.md](architecture.md): Implemented, Tested,
Experimental, Designed, Planned, Not supported.

| Part | Status |
|---|---|
| The `v1alpha1` API | Implemented. Its CRD schema and CEL rules are Tested by the API server checks ([testing.md](testing.md#api-server-checks)) |
| The v0.1 freeze | In effect from v0.1.0-rc1 |
| Deprecation policy after alpha | Planned |

## Deprecation after alpha

Planned. A deprecation policy, with the time a deprecated field or version
stays served, is defined when `v1beta1` is planned. Until then, the alpha
rules above apply.

## Change log per release

### 0.1.0-rc2

No breaking API change: no field, default, validation rule, or condition
type of `v1alpha1` changed since 0.1.0-rc1. Additions:

- The failure reason `NotAnchorSupervisor` (from baryon-virtd, on
  `Running` and `Ready`): the anchor was not recognized as the Anchor
  supervisor (ADR 0015, [api.md](api.md#failure-reasons)).
- baryon-controller acts on an `IdentityConflict` termination message only
  together with the anchor's dedicated exit code 3 (ADR 0012). An anchor
  of 0.1.0-rc1 exits with 1 on an identity conflict, so after the upgrade
  such a VM is not moved away from its node and stays not ready there
  until its Anchor Pod is replaced, for example by deleting it.

The admission policies of
the installation changed (ADR 0015): Baryon pods are refused when they
share the process namespace, use the host PID namespace or a user
namespace, run more than one container, keep or add capabilities, or allow
privilege escalation, and their container images and ephemeral containers
cannot be changed or added. baryon-controller creates no such pods.

### 0.1.0-rc1

API changes; the API stays `v1alpha1`. Details are in
[api.md](api.md).

- `virtualization.baryon.io/restartedAt`: a new non-empty value restarts a
  running VM exactly once. Removing the annotation does nothing; before,
  it reported `RestartRequired` with reason `SpecChanged` (a bug). The
  value is opaque (RFC 3339 recommended). It has no effect on a stopped
  VM and does not change `metadata.generation`. Anchor Pods created by a
  restart request of an earlier release still match.
- `rootDisk` validation (`rootDisk` stays immutable): `size` must be a
  whole number of bytes from 1Mi to 1Pi; `size` and `storageClassName`
  are rejected unless the source is `http`; `storageClassName` and
  `persistentVolumeClaim.claimName` must be DNS-1123 subdomains (at most
  253 characters, not empty); `http.url` must be an `http` or `https` URL
  with a host and without credentials. `className` must be a DNS-1123
  subdomain (at most 253 characters). The rules apply to writes from
  0.1.0-rc1 on; VirtualMachines stored earlier keep their values, since
  `rootDisk` is immutable. The importer refuses credentials in URLs of
  older objects too.
- New reason `ClaimRejected`: the API server refused the root disk claim
  Baryon creates (HTTP 422 or 403, for example an exceeded quota or an
  admission policy). It used to be logged only.
- `rootDisk.size` semantics: in filesystem mode the image is extended to
  exactly `size`, and the claim requests `size` plus 64 MiB, rounded up to
  MiB. In block mode the claim requests exactly `size`, rounded up to MiB,
  and the guest sees the whole device, which is at least `size`. Before,
  block-mode claims also requested 64 MiB more.
- VirtualMachineClass: `spec.cpu.cores` was renamed to `spec.cpu.vcpus`
  (1 to 256; says nothing about guest CPU topology), and the printer
  column is now `vCPUs`. `spec` is immutable: to change a shape, create a
  new class and change the VMs' `className`. `spec.memory.size` must be a
  whole number of Mi from 64Mi to 16Ti. A class without `cpu.vcpus` is
  rejected; a class stored in the earlier shape is reported as
  `InvalidClass` on VMs that start with it until it is applied again with
  `cpu.vcpus`, which the API server accepts once. Running VMs keep running.
- VirtualizationNode status: removed `runtime.protocol`,
  `runtime.hypervisor`, `architecture`, `kernelRelease`, `osImage`,
  `kairosVersion`, `cpu`, `memoryBytes`, `kvm`, `iommu`, `vfio`,
  `numaNodes`, `hugepages`, and `pciDevices`. They repeated Node status or
  described features Baryon does not support (VFIO, NUMA placement,
  hugepages, PCI passthrough); device inventory belongs to Dynamic
  Resource Allocation. baryon-node-agent logs the host inventory instead.
  Printer columns: Ready, Reason, VMs, Runtime, Hypervisor, Heartbeat,
  Age.
- VirtualizationNode staleness: a stopping node agent sets `Ready` and
  `RuntimeHealthy` to Unknown with reason `NodeAgentStopped`. Consumers
  apply the 90 s heartbeat rule ([Reading conditions](#reading-conditions)).
  The `Ready` True reason `KvmAvailable` was renamed `RuntimeReady`. A host
  report whose schema version the node agent does not read is now
  `ProtocolIncompatible` instead of `RuntimeUnreachable`.
- New optional VirtualizationNode field `status.runtime.virtdGitCommit`:
  the commit baryon-virtd was built from, `-dirty` for a build from a
  tree with uncommitted changes, `unknown` for a build without the
  information. Informational; the format is not contract. The node
  agent's `host inventory` log line also gains `host_image`, the Hadron
  base image named in the host image's component manifest.
- VirtualMachine `Running` and `Ready` can now be Unknown, with the new
  reason `NodeUnreachable` (before, Unknown was not used for them):
  Kubernetes marked the Anchor Pod not ready while kubelet's last status
  of the Anchor container is ready, which is what the node lifecycle
  controller leaves when a node stops reporting. It takes precedence over
  `Stopping` for a Terminating Anchor Pod on such a node. `NetworkReady`
  is then Unknown with reason `NodeUnreachable` as well. Clients that
  assumed `Running` and `Ready` are only True or False must handle
  Unknown.
- List semantics: VirtualMachine `status.addresses` is an atomic list
  whose `type` is the enum `InternalIP`; `status.identityConflictNodes`
  is a set; VirtualizationNode `status.vms` is a map keyed by
  `anchorPodUID`; `status.runtime.features` is a set. Optional fields are
  no longer nullable in the CRD schemas: the API server drops explicit
  nulls.
- An empty `placement: {}` places the VM like no placement and does not
  report `PlacementChanged`.
- `Running` is documented as: the Anchor container of the current Anchor
  Pod runs. It is not evidence that the guest runs.

Upgrade steps, in this order ([compatibility.md](compatibility.md#upgrade-to-010-rc1)):

1. Apply the new CRDs.
2. Apply every VirtualMachineClass again with `cpu.vcpus` instead of
   `cpu.cores`.
3. Upgrade baryon-controller and baryon-node-agent.
