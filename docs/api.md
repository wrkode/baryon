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
| `rootDisk.source.http.url` | string, at most 2048 characters | Raw disk image to import, `http://` or `https://`. A plain `http://` URL requires `sha256`. Must not carry secrets (tokens, credentials): the URL appears in the import Pod's arguments and in the VirtualMachine. |
| `rootDisk.source.http.sha256` | string, exactly 64 hex characters | Expected image SHA-256. Required when `url` is plain `http://`; recommended for `https://`. |
| `rootDisk.source.persistentVolumeClaim.claimName` | string | Existing claim with a raw disk: `disk.img` at the volume root (filesystem mode) or the whole device (block mode). |
| `rootDisk.size` | quantity | Virtual disk size. Required for `http`. |
| `rootDisk.storageClassName` | string | StorageClass of the claim Baryon creates. Default StorageClass when unset. |
| `rootDisk.volumeMode` | `Filesystem` (default) or `Block` | Volume mode of the claim Baryon creates for an `http` source. Not allowed with `persistentVolumeClaim`: an existing claim is used in its own volume mode. |
| `networks` | list-map keyed by `name`, at most 8 entries | `name` is a DNS-1123 label (1 to 63 characters). v1alpha1 accepts exactly one entry, with `pod: {}`. Defaults to `[{name: default, pod: {}}]`. |

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
accepted.

The import runs in a separate import Pod, `<vm>-import`, before the VM's
first Anchor Pod exists (see [architecture.md](architecture.md#import-pod-implemented-tested-by-the-deletion-and-storage-e2e-on-the-k0s-cluster)).
The importer (`baryon-anchor import`, `crates/anchor/src/import.rs`) applies
the same digest rule on its own, refuses a redirect from HTTPS to plain HTTP,
and resolves relative redirect locations against the current URL path. It
refuses an image larger than `rootDisk.size` before writing past it.

VM placement controls (nodeSelector, tolerations, affinity, priority) are
Planned; the API has no fields for them today. The only placement field
baryon-controller sets on the Anchor Pod is a required node affinity that
keeps it off the nodes in `status.identityConflictNodes` (see
[Host identity](#host-identity)).

The root disk claim layout is a stable format of v1alpha1: in filesystem
mode, `disk.img` at the volume root, plus `.baryon-import.json` (the import
marker) for imported disks. A claim prepared outside Baryon must follow it.
In block mode the disk is the whole device. A block device has no room for
a marker, so baryon-controller records a completed import in the
`virtualization.baryon.io/imported-from` annotation of the claim it
created, with the value `sha256:` followed by the hex SHA-256 of the source
URL (not the URL). Claims recorded by commits c8e6c08 to 597b637 hold the
URL itself; baryon-controller accepts a value equal to the current URL as a
completed import and rewrites it to the digest. This annotation is internal state of a Baryon-owned
object, not an API. It is trusted as written: anyone who can update claims
in the namespace can set or remove it. Removing it makes the next
replacement of the Anchor Pod import again, which overwrites a block device
(see [security.md](security.md#data-handling)). Filesystem-mode claims from
the previous release, which have the marker file and no annotation, get an
import Pod that finds the marker, leaves the disk untouched, and records the
digest (observed on the k0s cluster).

To restart a running VM, set the `virtualization.baryon.io/restartedAt`
annotation to a new value:

```bash
kubectl annotate virtualmachine debian --overwrite virtualization.baryon.io/restartedAt="$(date -u +%FT%TZ)"
```

### Status

| Field | Description |
|---|---|
| `conditions` | See below. |
| `nodeName` | Node of the current Anchor Pod. |
| `anchorPod` | Name of the current Anchor Pod (`<vm>-anchor`). |
| `addresses` | Guest addresses, the Anchor Pod IPs, type `InternalIP`. |
| `macAddress` | Guest MAC address, stable for the life of the VM. |
| `rootDiskClaim` | Claim holding the root disk. |
| `restartCount` | Restarts of the current Anchor Pod's anchor container (each one restarts the VM). Resets to 0 when the Anchor Pod is replaced (stop and start, restart request, node reboot). |
| `identityConflictNodes` | Nodes where this VM cannot run because another VM there has the same host identity (at most 16). New Anchor Pods avoid them. Cleared when the VM is stopped. |
| `observedGeneration` | Generation the status was computed for. |

Conditions:

| Type | True when | Typical reasons when False |
|---|---|---|
| `Scheduled` | The Anchor Pod is bound to a node. | `Pending`, `Unschedulable`, `Stopped`, `ClassNotFound`, `AnchorPodConflict`, `ClaimInUse`, and while the disk is not ready the `StorageReady` reason |
| `StorageReady` | The claim is bound and the disk image is in place. | `ClaimPending`, `ImportPending`, `ImportingDisk`, `ImportFailed`, `ClaimNotFound`, `ClaimConflict` |
| `Running` | The Anchor container is running; it runs or starts the VMM (reason `AnchorRunning`). It does not by itself mean that the VMM runs. | `Starting`, `Stopping`, `Stopped`, `Pending`, and the failure reasons below |
| `Ready` | The guest acquired its address over DHCP or showed a console login prompt (reason `GuestBooted`). | `Booting`, `ImportPending`, `ImportingDisk`, `ImportFailed`, `Pending`, `Unschedulable`, `Stopping`, `Stopped`, `AnchorPodConflict`, `ClaimInUse`, and failure reasons such as `VmmKilled`, `VmmExited`, `ResourcesUnbounded`, `StorageFailed`, `DiskInUse`, `BlockDeviceAccessDenied`, `NetworkFailed`, `CgroupDriverUnsupported`, `AttachRefused`, `ProtocolIncompatible`, `UnsupportedFeature`, `IdentityConflict` |
| `RestartRequired` | The VM must be restarted to apply a change: `SpecChanged` (the spec or the class changed after the Anchor Pod was created) or `AnchorImageChanged` (baryon-controller now uses a different anchor image). | `UpToDate` |

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
VirtualMachine, and `controller: true`). baryon-controller leaves that pod
alone and never reports it as the VM's; delete or rename it.

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

### kubectl

```
$ kubectl get virtualmachines
NAME     READY   REASON        NODE    ADDRESS       AGE
debian   True    GuestBooted   node1   10.244.1.30   62s
```

`kubectl describe virtualmachine <name>` shows conditions with messages and
the Events recorded by baryon-controller (`ClaimCreated`, `Importing`,
`DiskImported`, `Starting`, `Ready`, `NotReady`, `Stopping`, `Restarting`,
`AnchorPodFinished`, `IdentityConflict`, `Deleted`). Events never contain
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

The import Pod has no `vm-name` label, so Services that select VMs do not
select it. A NetworkPolicy that should apply to the importer's downloads
must select `virtualization.baryon.io/import-for` or
`app.kubernetes.io/managed-by=baryon`.

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
| `heartbeatTime` | Last refresh by the node agent (every 30 s by default). |
| `architecture`, `kernelRelease`, `osImage`, `kairosVersion` | Host identity. |
| `runtime` | baryon-virtd version, protocol package, `protocolVersion` (`<major>.<minor>`, `1.0` for a baryon-virtd that does not report one), `features` (runtime protocol features, ADR 0012), hypervisor and its version. |
| `cpu` | Model, online CPUs, virtualization extension, nested virtualization. |
| `memoryBytes`, `hugepages`, `numaNodes` | Memory topology. |
| `kvm` | Usable, API version, reason when not usable. |
| `iommu` | Enabled, group count, IOMMUFD device present. |
| `vfio` | Legacy container device present, vfio-pci driver registered. |
| `pciDevices` | PCI functions except bridges: address, vendor, device, class, driver, IOMMU group, NUMA node. |
| `vmCount` | VMs known to baryon-virtd. |

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
| `virtualization.baryon.io/vm` | 64 by default | each Anchor Pod, once | /dev/kvm and the baryon-virtd anchor socket |
| `virtualization.baryon.io/import` | the same as `vm`, with the same health | each import Pod, once | nothing; it only places the import Pod on a node that can run the VM |

With the `baryon-vm-resource` admission policy installed
(`deploy/base/policy/vm-resource.yaml`), only baryon-controller may create
pods that request either resource, in any container or init container; a
pod created by any other user that requests one is denied.
