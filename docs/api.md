# API reference: virtualization.baryon.io/v1alpha1

The Rust types in `crates/api` are the source of truth. CRDs are generated
with `cargo run -p baryon-api --bin baryon-crdgen` into
`deploy/base/crds/virtualization.baryon.io.yaml`; `hack/verify-crds.sh` fails when
they are stale. Design rationale: ADR 0008.

Status: Implemented. The API is alpha and may change incompatibly.

## VirtualMachineClass (cluster-scoped, short name `bvmclass`)

| Field | Type | Description |
|---|---|---|
| `spec.cpu.cores` | integer, 1 to 256 | vCPUs. Also the Anchor Pod CPU request and limit. |
| `spec.memory.size` | quantity | Guest memory. At least 64Mi and a whole number of Mi. |

```yaml
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachineClass
metadata:
  name: general-small
spec:
  cpu:
    cores: 2
  memory:
    size: 2Gi
```

## VirtualMachine (namespaced, short name `bvm`, category `baryon`)

The name must be a DNS-1123 label (at most 63 characters, no dots; enforced
by a CEL rule): it is the guest hostname and the value of the
`virtualization.baryon.io/vm-name` label on the objects Baryon creates.

### Spec

| Field | Type | Description |
|---|---|---|
| `className` | string, required | VirtualMachineClass name. |
| `runPolicy` | `Running` (default) or `Stopped` | Whether the VM runs. |
| `rootDisk.source.http.url` | string, at most 2048 characters | Raw disk image to import, `http://` or `https://`. A plain `http://` URL requires `sha256`. Must not carry secrets (tokens, credentials): the URL appears in the VirtualMachine, in the import Pod's arguments, and in the filesystem-mode import marker. Annotations and Events carry only `sha256:` and the hex SHA-256 of the URL. |
| `rootDisk.source.http.sha256` | string, exactly 64 hex characters | Expected image SHA-256. Required when `url` is plain `http://`; recommended for `https://`. |
| `rootDisk.source.persistentVolumeClaim.claimName` | string | Existing claim with a raw disk: `disk.img` at the volume root (filesystem mode) or the whole device (block mode). |
| `rootDisk.size` | quantity | Virtual disk size. Required for `http`. |
| `rootDisk.storageClassName` | string | StorageClass of the claim Baryon creates. Default StorageClass when unset. |
| `rootDisk.volumeMode` | `Filesystem` (default) or `Block` | Volume mode of the claim Baryon creates for an `http` source. Not allowed with `persistentVolumeClaim`: an existing claim is used in its own volume mode. |
| `networks` | list-map keyed by `name`, at most 8 entries | `name` is a DNS-1123 label (1 to 63 characters). v1alpha1 accepts exactly one entry, with `pod: {}`. Defaults to `[{name: default, pod: {}}]`. |
| `placement.nodeSelector` | map of strings | Node labels the node must have. See [Placement](#placement). |
| `placement.affinity` | core/v1 `Affinity` | Node affinity, pod affinity, and pod anti-affinity. Pod affinity terms may not set `namespaces` or `namespaceSelector`. |
| `placement.tolerations` | list of core/v1 `Toleration`, at most 32 | Tolerations of node taints. Each needs a non-empty `key`; some keys are refused. |
| `placement.topologySpreadConstraints` | list of core/v1 `TopologySpreadConstraint`, at most 8 | Spread of VMs over topology domains. |
| `placement.priorityClassName` | string, at most 253 characters | PriorityClass of the VM's pods. Names starting with `system-` are refused. |

`rootDisk` is immutable after creation, including `size` (CEL rule
`self == oldSelf`, message `rootDisk is immutable; create a new
VirtualMachine to use another disk`). The disk's content and claim are bound
to it: to use another disk or image, create a new VirtualMachine. Growing a
disk needs an explicit flow (Planned).

Exactly one of `http` and `persistentVolumeClaim` must be set; the API server
enforces this, the size requirement, the URL scheme, the digest for plain
HTTP (`sha256 is required when the url is plain http`), and the single pod
network with CEL rules. The CI API server checks (a k0s API server, see
[testing.md](testing.md#api-server-checks)) confirm that a VM with two disk
sources, a plain-HTTP source without `sha256`, a name with a dot, and a
change of `rootDisk` are rejected, and that a change of `runPolicy` is
accepted. The placement rules and their checks are in
[Placement](#placement).

The import runs in a separate import Pod, `<vm>-import`, before the VM's
first Anchor Pod exists (see [architecture.md](architecture.md#import-pod-implemented-tested-by-the-deletion-and-storage-e2e-on-the-k0s-cluster)).
The importer (`baryon-anchor import`, `crates/anchor/src/import.rs`) applies
the same digest rule on its own, refuses a redirect from HTTPS to plain HTTP,
and resolves relative redirect locations against the current URL path. It
refuses an image larger than `rootDisk.size` before writing past it.

The root disk claim layout is a stable format of v1alpha1: in filesystem
mode, `disk.img` at the volume root, plus `.baryon-import.json` (the import
marker) for imported disks. A claim prepared outside Baryon must follow it.
In block mode the disk is the whole device.

A completed import is recorded by baryon-controller in the annotation
`virtualization.baryon.io/disk-import` on the claim's bound
PersistentVolume, as JSON with `vmUID`, `claimUID`, and `source` (`sha256:`
and the hex SHA-256 of the source URL). The record, and the freshness rule
that decides whether Baryon imports into a volume at all, are described in
[storage.md](storage.md#import-record) and ADR 0006. The annotation is
internal state, not an API: do not write or remove it, and keep it when
backing up or restoring volumes. The claim annotation
`virtualization.baryon.io/imported-from` written by earlier releases is no
longer written; baryon-controller reads it only to migrate VirtualMachines
of earlier releases (see
[compatibility.md](compatibility.md#version-skew)).

To use existing data, such as a restored snapshot, a clone, or a claim
from a deleted VirtualMachine, reference the claim with
`rootDisk.source.persistentVolumeClaim` in a new VirtualMachine. A
VirtualMachine with an `http` source imports only into a volume that was
dynamically provisioned for the claim Baryon created, and refuses
anything else (see the storage reasons below).

To restart a running VM, set the `virtualization.baryon.io/restartedAt`
annotation to a new value:

```bash
kubectl annotate virtualmachine debian --overwrite virtualization.baryon.io/restartedAt="$(date -u +%FT%TZ)"
```

### Placement

Status: Implemented; Tested by the placement E2E on the k0s cluster and by
the CI API server checks (see [testing.md](testing.md#results)). Design:
ADR 0004 and ADR 0008.

`spec.placement` holds Kubernetes scheduling constraints, with the meaning
they have on a pod. Kubernetes remains the scheduler: baryon-controller
copies `nodeSelector`, `affinity`, `tolerations`,
`topologySpreadConstraints`, and `priorityClassName` to the VM's import Pod
and Anchor Pod, and the Kubernetes scheduler places those pods. Baryon never
picks a node. Other pod fields, such as `nodeName` and `schedulerName`, are
not part of the API.

```yaml
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata:
  name: web-1
  labels:
    app: web
spec:
  className: general-small
  rootDisk:
    size: 10Gi
    source:
      http:
        url: https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.raw
  placement:
    nodeSelector:
      disk: ssd
    tolerations:
      - {key: dedicated, operator: Equal, value: vms, effect: NoSchedule}
    topologySpreadConstraints:
      - maxSkew: 1
        topologyKey: kubernetes.io/hostname
        whenUnsatisfiable: DoNotSchedule
        labelSelector: {matchLabels: {app: web}}
```

Baryon adds one required node requirement of its own: while
`status.identityConflictNodes` is not empty, `metadata.name NotIn <those
nodes>` (a `matchFields` requirement, see [Host identity](#host-identity)).
It is ANDed into every term of
`affinity.nodeAffinity.requiredDuringSchedulingIgnoredDuringExecution`
(Kubernetes ORs the terms and ANDs the requirements within a term), so no
user term reaches a recorded node. Whether a node runs a healthy Baryon
runtime is not a placement term: the pods request the
`virtualization.baryon.io/vm` or `virtualization.baryon.io/import`
resource (see [Extended resources](#extended-resources)).

Validation, by CEL rules in the CRD:

- Every toleration needs a non-empty `key`, so a toleration of every taint
  (`operator: Exists` without a key) is refused.
- Tolerations of `node.kubernetes.io/unschedulable` (a cordoned node),
  `node-role.kubernetes.io/control-plane`, and
  `node.kubernetes.io/out-of-service` are refused.
- `priorityClassName` may not start with `system-`.
- Pod affinity and pod anti-affinity terms, required and preferred, may not
  set `namespaces` or `namespaceSelector`. They apply within the VM's
  namespace.

The CI API server checks confirm that a toleration of every taint, of a
cordon, of control-plane nodes, and of an out-of-service node, a system
PriorityClass, required pod anti-affinity with a `namespaceSelector`, and
preferred pod affinity with `namespaces` are rejected, and that a placement
with every field is accepted.

Administrator policy: the VM's pods are created by baryon-controller in the
VM's namespace, and Kubernetes admission judges a pod by its namespace, not
by who created it. PodNodeSelector, PodTolerationRestriction, and
ResourceQuota with the `PriorityClass` and `CrossNamespacePodAffinity`
scopes therefore apply to VM pods like to other pods of the namespace. To
keep namespaces off nodes reserved with a custom taint, restrict
tolerations with PodTolerationRestriction; to limit high PriorityClasses,
use a ResourceQuota scoped to them. These are Kubernetes mechanisms; Baryon
has no test for them.

Priority and node failure:

- A preempted Anchor Pod stops its VM, like any deletion of the Anchor Pod.
  baryon-controller then creates a new Anchor Pod, which the scheduler
  places like any other pod.
- Tolerating the `NoExecute` taints `node.kubernetes.io/not-ready` and
  `node.kubernetes.io/unreachable` only delays the eviction of the VM's pod
  from a failed node, and with it the VM's replacement. It does not change
  fencing: the Anchor Pod object stays until it is deleted, and no new
  Anchor Pod is created before (ADR 0004). Inferred from the fencing rule;
  not tested.

Changes: placement is not applied to a running VM. The Anchor Pod carries
the annotation `virtualization.baryon.io/placement-hash`; when the VM's
placement no longer matches it, `RestartRequired` is True with reason
`PlacementChanged` and the message "the scheduling constraints changed;
they apply when the VM is restarted (a VM on a node-local volume stays on
that volume's node)". Restart the VM (`restartedAt`, or stop and start) to
apply the change. An Anchor Pod of an earlier release has no placement hash
and matches a VM without placement. An import Pod that already exists
keeps the placement it was created with.

When the import Pod or the Anchor Pod cannot be scheduled, `Scheduled` is
False with the scheduler's reason (`Unschedulable`) and message, cut to
1024 bytes; the full message is on the pod. `Running` and `Ready` carry the
same reason. When the API server refuses to create the pod (HTTP 422 or
403: an invalid placement, an unknown PriorityClass, a quota, an admission
policy), `Scheduled`, `Running`, and `Ready` are False with reason
`PodRejected` and the API server's message. Baryon computes no node
eligibility of its own beside the scheduler, so there is no
"no compatible node" reason: the scheduler's message says which
constraint excluded which nodes. A node whose baryon-virtd is down or
speaks an incompatible protocol has no free `virtualization.baryon.io/vm`
or `virtualization.baryon.io/import` slot, and the scheduler reports it
that way, for example `Insufficient virtualization.baryon.io/import`.

Capability requirements (IOMMU, IOMMUFD, hugepages, NUMA, VFIO, Cloud
Hypervisor features) are not part of placement. Baryon consumes none of
them today: VFIO, hugepage-backed guest memory, and NUMA pinning are Not
supported. KVM and runtime health are already enforced by the
`virtualization.baryon.io/vm` slot. A capability gets a name in the API
only when Baryon consumes it and it is tested; the Designed direction is
recorded in ADR 0012. VirtualizationNode reports host capabilities for
information only.

### Status

| Field | Description |
|---|---|
| `conditions` | See below. |
| `nodeName` | Node of the current Anchor Pod. |
| `anchorPod` | Name of the current Anchor Pod (`<vm>-anchor`). |
| `addresses` | Guest addresses, the Anchor Pod IPs, type `InternalIP`. |
| `macAddress` | Guest MAC address, stable for the life of the VM. |
| `rootDiskClaim` | Claim holding the root disk. |
| `rootDiskImport.claimUID` | Set by baryon-controller when the root disk claim with this UID held an imported or used disk; reset when the claim is recreated. A one-way marker that only prevents another import into the same claim. It is not an API guarantee and must not be relied on: the record on the PersistentVolume is the authority (ADR 0006). |
| `restartCount` | Restarts of the current Anchor Pod's anchor container (each one restarts the VM). Resets to 0 when the Anchor Pod is replaced (stop and start, restart request, node reboot). |
| `identityConflictNodes` | Nodes where this VM cannot run because another VM there has the same host identity (at most 16). New Anchor Pods avoid them. Cleared when the VM is stopped. |
| `observedGeneration` | Generation the status was computed for. |

Conditions:

| Type | True when | Typical reasons when False |
|---|---|---|
| `Scheduled` | The Anchor Pod is bound to a node. | `Pending`, `Unschedulable`, `PodRejected`, `Stopped`, `ClassNotFound`, `AnchorPodConflict`, `ImportPodConflict`, `ClaimInUse`, and while the disk is not ready the `StorageReady` reason |
| `StorageReady` | The claim is bound and the disk is in place: for an `http` source, its import is recorded on the PersistentVolume. | `ClaimPending`, `ImportPending`, `ImportingDisk`, `ImportFailed`, `ClaimNotFound`, `ClaimConflict`, and the import refusals `DiskNotProvisionedForClaim`, `DiskInitializedForAnotherClaim`, `DiskSourceMismatch`, `DiskImportRecordInvalid`, `ImportStateUnknown`, `DiskBindingPending` |
| `Running` | The Anchor container is running; it runs or starts the VMM (reason `AnchorRunning`). It does not by itself mean that the VMM runs. | `Starting`, `Stopping`, `Stopped`, `Pending`, and the failure reasons below |
| `Ready` | The guest acquired its address over DHCP or showed a console login prompt (reason `GuestBooted`). | `Booting`, `ImportPending`, `ImportingDisk`, `ImportFailed`, the import refusals, `Pending`, `Unschedulable`, `PodRejected`, `Stopping`, `Stopped`, `AnchorPodConflict`, `ImportPodConflict`, `ClaimInUse`, and failure reasons such as `VmmKilled`, `VmmExited`, `ResourcesUnbounded`, `StorageFailed`, `DiskInUse`, `BlockDeviceAccessDenied`, `NetworkFailed`, `CgroupDriverUnsupported`, `AttachRefused`, `ProtocolIncompatible`, `UnsupportedFeature`, `IdentityConflict` |
| `NetworkReady` | baryon-virtd on the VM's node reports the VM's DHCP responder running and answering liveness probes (reason `AddressServiceRunning`). Derived only while `Running` is True; see [NetworkReady](#networkready). | `DhcpResponderRestarting`, `DhcpResponderFailing`. Unknown: `RuntimeUnreachable`, `RuntimeDoesNotReport`, `NotReported`, `NotRunning` |
| `RestartRequired` | The VM must be restarted to apply a change: `SpecChanged` (the class, or a spec field other than `placement`, changed after the Anchor Pod was created), `PlacementChanged` (`spec.placement` changed, see [Placement](#placement)), or `AnchorImageChanged` (baryon-controller now uses a different anchor image). When several apply, the first in this order is reported. | `UpToDate` |

Precedence: an existing Anchor Pod decides `Running` and `Ready`. Class or
spec problems such as `ClassNotFound` are shown only when there is no Anchor
Pod; while a pod exists they matter for the next start.

While an import is pending there is no Anchor Pod: the import Pod's state
decides `StorageReady`, and its reason (`ImportPending`, `ImportingDisk`,
`ImportFailed`, or the scheduler's `PodScheduled` reason such as
`Unschedulable` when the import Pod cannot be placed) is also the reason of
`Scheduled`, `Running`, and `Ready`.

`ClaimInUse`: a Baryon pod (an Anchor Pod or an import Pod) of another
VirtualMachine in the namespace mounts the root disk claim. baryon-controller
creates no pod for this VM until that pod is gone. The check is a
best-effort guard: reconciles of different VMs run concurrently, so two VMs
started at the same time on one claim can both pass it. The backstops are
`DiskInUse` and the VMM's file lock on one node, and the attach/detach
controller's multi-attach refusal for `ReadWriteOnce` claims across nodes. One running VM per claim
is the rule; a `ReadWriteMany` block claim shared by VMs on different nodes
is Not supported.

`AnchorPodConflict`: a pod named `<vm>-anchor` exists that this
VirtualMachine does not control (no owner reference with the VM UID, kind
VirtualMachine, and `controller: true`), or that does not request the
`virtualization.baryon.io/vm` resource, which only baryon-controller can
give a pod. baryon-controller leaves that pod alone and never reports it as
the VM's; delete or rename it.

`ImportPodConflict`: a pod named `<vm>-import` exists that this
VirtualMachine does not control or that does not request the
`virtualization.baryon.io/import` resource. baryon-controller never uses
it as import evidence and creates neither an import Pod nor an Anchor Pod
until it is gone; delete it.

`PodRejected`: the API server refused to create the import Pod or the
Anchor Pod (HTTP 422 or 403), for example for an unknown PriorityClass, an
exceeded quota, or an admission policy. The message is the API server's,
cut to 1024 bytes. baryon-controller looks at such a VM again every 60 s,
because a quota that frees up or a PriorityClass created later can make the
pod acceptable; otherwise fix the placement or the namespace policy.

`ImportingDisk` with the message "the importer finished; recording the
import": the importer exited 0 and baryon-controller has not yet written
the record on the PersistentVolume. An importer's exit code alone never
makes the disk ready.

Import refusals. None of them imports or starts the VM, and their messages
name the adoption path (`rootDisk.source.persistentVolumeClaim` in a new
VirtualMachine) where it applies. baryon-controller does not watch
PersistentVolumes; it looks at a refused VM again every 120 s, and at one
in `DiskBindingPending` every 30 s.

| Reason | Meaning |
|---|---|
| `DiskNotProvisionedForClaim` | The volume has no import record and was not dynamically provisioned for this claim (no `pv.kubernetes.io/provisioned-by`, bound to another claim UID, or older than the claim), or the claim has a `dataSource` or `dataSourceRef` (a restore, clone, or populator). |
| `DiskInitializedForAnotherClaim` | The volume's record names another claim UID. |
| `DiskSourceMismatch` | The volume's record names another source. |
| `DiskImportRecordInvalid` | The volume's record cannot be read. |
| `ImportStateUnknown` | A block-mode claim that held an imported or used disk (`status.rootDiskImport`, or a ready disk of an earlier release) has no record on its volume. Restoring the record lets the VM start on the existing disk. |
| `DiskBindingPending` | A VirtualMachine of an earlier release is being migrated, and the record cannot be written until the claim and the volume are bound to each other. |

Failure reasons come from baryon-virtd through the anchor container's
termination message (`Reason: detail`). `AttachRefused` means baryon-virtd
refused the attach without a more specific reason, for example an invalid
VM spec.
`IdentityConflict` means baryon-virtd refused the VM on its node because
another VM there runs with the same host identity (see
[Host identity](#host-identity)). `ProtocolIncompatible` and
`UnsupportedFeature` come from the runtime protocol negotiation (ADR 0012).
`StorageFailed` covers a disk that baryon-virtd refuses to hand to the VMM,
for example a path or file kind the Anchor container does not use for its
disk, or a device node outside the container's `/dev`. `DiskInUse`: another
VM on the node already uses the same block device.
`BlockDeviceAccessDenied`: the block device node is not readable and
writable by the VM's host identity; the container runtime must give block
volume devices the pod's `runAsUser` and `runAsGroup` (containerd:
`device_ownership_from_security_context = true`, see
[compatibility.md](compatibility.md#csi-requirements)).
`Ready=True` means boot evidence was observed, not that every guest service
is up; on a disk's first boot the guest typically becomes reachable a few
seconds later.

#### NetworkReady

`NetworkReady` reports the host-side service of the guest network, the
VM's DHCP responder ([networking.md](networking.md#guest-network-service-health)).
It says nothing about the guest's own network stack and does not change
`Ready`. baryon-controller derives it from the VirtualizationNode of the
Anchor Pod's node (`status.vms`), only while the VM's `Running` condition
is True. Status: Tested by unit tests and by the E2E
test `tests/e2e/network-health.sh` on the k0s cluster for True
(`AddressServiceRunning`), False (`DhcpResponderFailing`, and before the
guest acquired an address), and Unknown (`RuntimeUnreachable` while the
node agent of the VM's node is gone for longer than 90 s), with `Ready`
staying True ([testing.md](testing.md#k0s-cluster)). `tests/e2e/skew.sh`
checks True from hosts at runtime protocol 1.2 and Unknown
(`RuntimeDoesNotReport`) from a host at 1.1, on a cluster with mixed
hosts. Tested on the k0s cluster: responder hangs (`network-health.sh`
cases hang, hang-repeated, and before-lease, which hangs the responder
instead of killing it), and a baryon-controller restart while a node agent
is down (`tests/e2e/node-agent-outage.sh`: `NetworkReady` stayed Unknown,
`RuntimeUnreachable`, and never read True from the stale report).

| Status | Reason | Meaning |
|---|---|---|
| True | `AddressServiceRunning` | The responder runs and its DHCP serving loop answers liveness probes (message: "the DHCP responder for the guest runs and answers liveness probes"). It does not show that the guest can get or renew a lease, or that the pod network passes traffic. |
| False | `DhcpResponderRestarting` | The responder stopped serving and baryon-virtd restarts it with backoff of at most 60 s. The message names the cause: it stopped, it stopped answering liveness probes, or it wrote output that breaks its protocol. |
| False | `DhcpResponderFailing` | The responder failed more than 10 times in a row; baryon-virtd keeps restarting it every 60 s. The message names the cause of the last failure. |
| Unknown | `RuntimeUnreachable` | The node has no report, its report is stale (the controller has not seen its heartbeat change for 90 s), or its `RuntimeHealthy` condition is not True (its reason is in the message). |
| Unknown | `RuntimeDoesNotReport` | The node's baryon-virtd speaks runtime protocol 1.1 or earlier and does not report the responder. |
| Unknown | `NotReported` | The node's report has no entry for the current Anchor Pod. On a VM's first start this lasts until the responder has answered its first liveness probe and the node agent has reported it. |
| Unknown | `NotRunning` | The VM is not running on a node. |

baryon-virtd sends a liveness probe to the responder every 10 s and
reports it as running only after its first answer; after a failure, only
after a new responder has kept answering for 25 s. A responder that
answers no probe for 25 s is killed and restarted, which counts as a
failure. baryon-virtd therefore detects a hang within about 35 s, and the
condition shows it at most one node agent interval later. Before the first
answer, a first start shows Unknown (`NotReported`), and a restart keeps
`DhcpResponderRestarting` or `DhcpResponderFailing`.

baryon-controller sets the reason from the reported state, never from the
node's text, and caps the node's message at 1024 bytes. When False, the
message includes "guest lease valid until <time>" if baryon-virtd saw a
lease acknowledgement, or says that the guest has not acquired an address.
The time can be up to 60 s earlier than the guest's own lease expiry,
because acknowledgements are reported at most once per 60 s. The condition
lags the host by up to one node agent interval (at most 30 s).

Reading `Ready` and `NetworkReady` together:

| Ready | NetworkReady | Meaning |
|---|---|---|
| False, `Booting` | True | A normal start: the responder runs and answers liveness probes, and the guest has not yet acquired its address or shown a login prompt. |
| False, `Booting` | Unknown, `NotReported` | Early in a start, until the responder has answered its first liveness probe and the node agent has reported it. |
| True | True | The guest booted and its address service runs and answers liveness probes. |
| True | False | The guest has its address and keeps it until the time in the message; it loses it after that unless the responder recovers. |
| any | Unknown, `NotRunning` | The VM is not running. |

baryon-controller emits a Warning event `NetworkNotReady` each time
`NetworkReady` changes to False, and a Normal event `NetworkReady` only
when it changes from False to True, not on every start.

### kubectl

```
$ kubectl get virtualmachines
NAME     READY   REASON        NODE    ADDRESS       AGE
debian   True    GuestBooted   node1   10.244.1.30   62s
```

`kubectl describe virtualmachine <name>` shows conditions with messages and
the Events recorded by baryon-controller (`ClaimCreated`, `Importing`,
`DiskImported`, `Starting`, `Ready`, `NotReady`, `NetworkReady`,
`NetworkNotReady`, `Stopping`, `Restarting`, `AnchorPodFinished`,
`IdentityConflict`, `Deleted`). Events never contain
the source URL.

### Example

```yaml
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata:
  name: debian
spec:
  className: general-small
  rootDisk:
    size: 10Gi
    source:
      http:
        url: https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.raw
```

### Labels on objects Baryon creates

| Label | On | Value |
|---|---|---|
| `virtualization.baryon.io/vm-name` | Anchor Pods, root disk claims | VirtualMachine name; use it to select a VM's Anchor Pod, for example in a Service |
| `virtualization.baryon.io/vm-uid` | Anchor Pods | VirtualMachine UID |
| `virtualization.baryon.io/import-for` | import Pods | VirtualMachine UID |
| `app.kubernetes.io/managed-by` | objects created by Baryon | `baryon` |
| the VM's own labels, except reserved keys | Anchor Pods, import Pods | the VirtualMachine's `metadata.labels` |

| Annotation | On | Value |
|---|---|---|
| `virtualization.baryon.io/vm-label-keys` | Anchor Pods | comma-separated keys of the VM labels Baryon copied |
| `virtualization.baryon.io/placement-hash` | Anchor Pods | hash of the placement the pod was created with |

VM labels: baryon-controller copies the VirtualMachine's labels to the
Anchor Pod and the import Pod when it creates them, and keeps them in sync
on the running Anchor Pod without a restart. Services, NetworkPolicies, pod
affinity and anti-affinity, and topology spread constraints can therefore
select VMs by their own labels, and recommended labels such as
`app.kubernetes.io/name` propagate. Reserved keys are not copied:
`app.kubernetes.io/managed-by`, keys whose prefix is `baryon.io` or a
subdomain of it, and keys whose prefix is `kubernetes.io`, `k8s.io`, or a
subdomain of either, other than `app.kubernetes.io`. A sync removes only
labels listed in `virtualization.baryon.io/vm-label-keys`; labels that
others set on the pod stay. Users can edit that annotation, so
baryon-controller ignores reserved keys in it and never removes a reserved
label. Status: Tested by the placement E2E on the k0s
cluster (copied, changed without a restart, removed, another actor's label
kept, a reserved key not copied).

The import Pod's labels are set when it is created and are not synced
while the import runs; a label change reaches the Anchor Pod. The import
Pod carries the VM's labels so that the scheduler counts it in
the VM's spread constraints and pod anti-affinity; with node-local storage
that binds on first use, the import Pod decides the VM's node. It has a
readiness probe that never passes, so a Service that selects the VM's
labels never sends traffic to it. It has no `vm-name` label, so Services
that select `virtualization.baryon.io/vm-name` do not select it. A
NetworkPolicy that selects the VM's own labels also applies to the
importer's downloads, so an egress-deny policy for VMs also blocks HTTP
imports; one that should apply only to the importer must select
`virtualization.baryon.io/import-for`. A PodDisruptionBudget that selects
the VM's labels counts the import Pod, which is never Ready, as unhealthy
during an import (see ADR 0004).

The `vm-name` label replaced an earlier label key that was the same string as
the extended resource `virtualization.baryon.io/vm`.

### Host identity

Each VM executes as a distinct, unprivileged host identity (uid and gid):
the Anchor container, Cloud Hypervisor, and the VM's DHCP responder run as
it. The identity is stable for the life of a VirtualMachine object and is
checked by baryon-virtd. Hosts reserve the ids 1000000000 to 1999999999 for
VM identities. How an identity is allocated is internal and not part of the
API (ADR 0011).

If another VM on the same node has the same identity, baryon-virtd refuses
the VM there. baryon-controller then records the node in
`status.identityConflictNodes`, emits an `IdentityConflict` event, replaces
the Anchor Pod, and keeps the new pod off the recorded nodes with required
node affinity on `metadata.name`. The list is cleared when the VM is
stopped. On a cluster where every Baryon node is recorded, the VM stays not
ready until it is stopped and started again after the conflicting VM is
gone. Status: Implemented, with unit tests; not exercised on a cluster.

The guest MAC address is a separate, guest-visible identity, derived from
the VirtualMachine UID (`02:` followed by the last 40 bits of the UID) and
reported in `status.macAddress`.

## VirtualizationNode (cluster-scoped, short name `vnode`)

One object per Baryon node, named after the Node and owned by it, written
only by the baryon-node-agent on that node (enforced by the
`baryon-node-agent-own-node` admission policy in `deploy/base/policy/`). No spec.

| Status field | Description |
|---|---|
| `conditions` | `Ready` (runtime healthy and KVM usable), `RuntimeHealthy` (baryon-virtd answered and speaks a compatible runtime protocol). Reasons include `RuntimeUnreachable`, `ProtocolIncompatible`, `KvmUnavailable`. |
| `heartbeatTime` | Last refresh by the node agent: every 30 s by default, 5 to 30 s (`--status-interval-seconds`). baryon-controller treats a node whose heartbeat it has not seen change for 90 s as not reporting. |
| `architecture`, `kernelRelease`, `osImage`, `kairosVersion` | Host identity. |
| `runtime` | baryon-virtd version, protocol package, `protocolVersion` (`<major>.<minor>`, `1.0` for a baryon-virtd that does not report one), `features` (runtime protocol features, ADR 0012), hypervisor and its version. |
| `cpu` | Model, online CPUs, virtualization extension, nested virtualization. |
| `memoryBytes`, `hugepages`, `numaNodes` | Memory topology. |
| `kvm` | Usable, API version, reason when not usable. |
| `iommu` | Enabled, group count, IOMMUFD device present. |
| `vfio` | Legacy container device present, vfio-pci driver registered. |
| `pciDevices` | PCI functions except bridges: address, vendor, device, class, driver, IOMMU group, NUMA node. |
| `vmCount` | VMs known to baryon-virtd. |
| `vms` | Host-side state of each VM on the node, at most 256 entries (the node agent publishes the first 256 and logs a warning when there are more): `anchorPodUID`, and `network` with `state` (`Ready`, `Restarting`, or `Failing`), `reason` (at most 64 bytes), `message` (at most 1024 bytes), `lastTransitionTime`, and `leaseExpiresTime`. Entries carry no VM names, namespaces, addresses, PIDs, paths, or TAP names. A baryon-virtd before runtime protocol 1.2 gives entries without `network`. |

```
$ kubectl get virtualizationnodes
NAME    READY   ARCH     KVM    IOMMU   HYPERVISOR   VMS   AGE
node1   True    x86_64   true   true    v53.0        1     12m
```

## Extended resources

baryon-node-agent advertises two extended resources on every Baryon node,
visible in the Node's `status.allocatable`:

| Resource | Slots | Requested by | Grants |
|---|---|---|---|
| `virtualization.baryon.io/vm` | 64 by default, 1 to 256 (`--vm-slots`) | each Anchor Pod, once | /dev/kvm and the baryon-virtd anchor socket |
| `virtualization.baryon.io/import` | the same as `vm`, with the same health | each import Pod, once | nothing; it only places the import Pod on a node that can run the VM |

While baryon-virtd on a node is unreachable or speaks an incompatible
runtime protocol, the node agent reports both resources unhealthy and
kubelet sets their allocatable to zero. A VM whose placement allows only
such nodes stays `Unschedulable`, with a scheduler message such as
`Insufficient virtualization.baryon.io/import`, and starts once the runtime
is back; a VM already running on the node keeps its Anchor Pod. Tested by
the placement E2E on the k0s cluster (baryon-virtd stopped on the only
eligible node).

With the `baryon-vm-resource` admission policy installed
(`deploy/base/policy/vm-resource.yaml`), only baryon-controller may create
pods that request either resource, in any container or init container; a
pod created by any other user that requests one is denied.

## Access for namespace users

`deploy/base/tenant/roles.yaml`, part of `deploy/base`, defines two
ClusterRoles that aggregate into the Kubernetes default user-facing roles:

| ClusterRole | Aggregated into | Grants |
|---|---|---|
| `baryon-vm-edit` | `edit`, `admin` | virtualmachines: get, list, watch, create, update, patch, delete, deletecollection; virtualmachineclasses: get, list, watch |
| `baryon-vm-view` | `view` | virtualmachines, virtualmachineclasses: get, list, watch |

A user bound to `edit` or `admin` in a namespace can therefore manage
VirtualMachines there. Neither role grants `virtualmachines/status`, which
only baryon-controller writes, or anything on PersistentVolumes, where the
import records live. Status: Tested by the import integrity E2E on the k0s
cluster, which creates, starts, stops, and restarts its VirtualMachine as a
service account bound to `edit` and checks that this account cannot write
the VirtualMachine status or the PersistentVolume.

Neither role grants anything on VirtualizationNodes, whose `status.vms`
lists the Anchor Pod UIDs of all namespaces by node. Namespace users see
the host-side network state of their own VMs only, as `NetworkReady`.
Status: Implemented (role definitions); not covered by an E2E check.
