# API reference: virtualization.baryon.io/v1alpha1

The Rust types in `crates/api` are the source of truth. CRDs are generated
with `cargo run -p baryon-api --bin baryon-crdgen` into
`deploy/base/crds/virtualization.baryon.io.yaml`; `hack/verify-crds.sh` fails when
they are stale. Design rationale: ADR 0008.

Status: Implemented. The API is alpha and may change incompatibly. Which
parts are contract, the v0.1 freeze, and the change log per release are in
[api-stability.md](api-stability.md).

## VirtualMachineClass (cluster-scoped, short name `bvmclass`)

| Field | Type | Description |
|---|---|---|
| `spec.cpu.vcpus` | integer, 1 to 256, required | Number of virtual CPUs. Also the Anchor Pod CPU request and limit. Says nothing about the guest's CPU topology. |
| `spec.memory.size` | quantity | Guest memory: a whole number of Mi from 64Mi to 16Ti. |

```yaml
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachineClass
metadata:
  name: general-small
spec:
  cpu:
    vcpus: 2
  memory:
    size: 2Gi
```

`spec` is immutable (CEL rule, message `VirtualMachineClass spec is
immutable; create a new class and change the VMs' className`). To change
the shape of VMs, create a new class and set the VMs' `className` to it.
A running VM whose vCPUs or memory change this way reports
`RestartRequired` (`SpecChanged`) and gets the new shape at its next
start.

`cpu.vcpus` replaced `cpu.cores` in 0.1.0-rc1. A class stored before
that change has no `cpu.vcpus` (the API server drops `cpu.cores` when it
reads the object). Such a class may be updated once to the current shape,
for example with `kubectl apply`; after that it is immutable. Until then,
a VM that starts with it reports `InvalidClass` with the message
"VirtualMachineClass <name> has no cpu.vcpus; apply it again with
cpu.vcpus". VMs that already run keep running. A new class without
`cpu.vcpus` is rejected (`cpu.vcpus is required`). See
[compatibility.md](compatibility.md#upgrade-to-010-rc1).

## VirtualMachine (namespaced, short name `bvm`, category `baryon`)

The name must be a DNS-1123 label (at most 63 characters, no dots; enforced
by a CEL rule): it is the guest hostname and the value of the
`virtualization.baryon.io/vm-name` label on the objects Baryon creates.

### Spec

| Field | Type | Description |
|---|---|---|
| `className` | string, required, DNS-1123 subdomain, 1 to 253 characters | VirtualMachineClass name. |
| `runPolicy` | `Running` (default) or `Stopped` | Whether the VM runs. |
| `rootDisk.source.http.url` | string, at most 2048 characters | Raw disk image to import: an `http` or `https` URL with a host and without credentials (`user:password@` is rejected). A plain `http://` URL requires `sha256`. Must not carry secrets such as tokens in the path or query: the URL appears in the VirtualMachine, in the import Pod's arguments, in the filesystem-mode import marker, and can appear in the `ImportFailed` message. Annotations and Events carry only `sha256:` and the hex SHA-256 of the URL. |
| `rootDisk.source.http.sha256` | string, exactly 64 hex characters | Expected image SHA-256. Required when `url` is plain `http://`; recommended for `https://` (see [Root disk size and import identity](#root-disk-size-and-import-identity)). |
| `rootDisk.source.persistentVolumeClaim.claimName` | string, DNS-1123 subdomain, 1 to 253 characters | Existing claim with a raw disk: `disk.img` at the volume root (filesystem mode) or the whole device (block mode). |
| `rootDisk.size` | quantity, a whole number of bytes from 1Mi to 1Pi | Virtual disk size. Required for `http`; rejected with `persistentVolumeClaim`. See [Root disk size and import identity](#root-disk-size-and-import-identity). |
| `rootDisk.storageClassName` | string, DNS-1123 subdomain, 1 to 253 characters | StorageClass of the claim Baryon creates. Default StorageClass when unset. Rejected with `persistentVolumeClaim`. |
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

Exactly one of `http` and `persistentVolumeClaim` must be set. The API
server enforces this and the following with CEL rules and schema limits:

- `size` is required for `http` and is a whole number of bytes from 1Mi to
  1Pi (`size must be a whole number of bytes between 1Mi and 1Pi`); both
  `4Gi` and an integer number of bytes are accepted
- `size` and `storageClassName` apply only to the claim Baryon creates:
  they are rejected with `persistentVolumeClaim` (`size and
  storageClassName apply only to the claim Baryon creates for an http
  source`), and so is `volumeMode`
- `className`, `storageClassName`, and `claimName` are DNS-1123
  subdomains, the Kubernetes object name format: lowercase letters,
  digits, `-`, and `.`, 1 to 253 characters
- `url` is an `http` or `https` URL with a host and without credentials
  (`url must be an http or https URL with a host and without credentials`)
- a plain `http` URL needs `sha256` (`sha256 is required when the url is
  plain http`)
- exactly one network, the pod network

The CI API server checks (a k0s API server, see
[testing.md](testing.md#api-server-checks)) confirm that a VM with two disk
sources, a plain-HTTP source without `sha256`, a name with a dot, a change
of `rootDisk`, the invalid sizes `0`, `100m`, `512Ki`, `banana`, and
`2Pi`, an empty or invalid `claimName`, `size` or `storageClassName` with
an existing claim, an empty `storageClassName`, a URL with credentials, a
URL without a host, an `ftp` URL, and a `className` that is not a DNS-1123
subdomain are rejected, and that a change of `runPolicy`, a size of `4Gi`
or of `4294967296` bytes, and an existing claim named `disk.v1` are
accepted. The placement rules and their checks are in
[Placement](#placement).

### Root disk size and import identity

`rootDisk.size` is the size of the disk the guest sees in filesystem
mode, and the minimum in block mode:

| Volume mode of the claim Baryon creates | Claim request | What the guest sees |
|---|---|---|
| `Filesystem` (default) | `size` plus 64 MiB, rounded up to a whole MiB, for the import marker and the filesystem's own metadata | `disk.img`, extended to exactly `size` |
| `Block` | exactly `size`, rounded up to a whole MiB | the whole device, which is at least `size`; the provisioner may round the volume up |

The importer refuses an image larger than `size`. Releases before
0.1.0-rc1 also added 64 MiB to block-mode claims.

Baryon identifies an import by the SHA-256 of the URL, not by the image
content: the import record and the import Pod carry `sha256:` and the hex
digest of the URL. A URL whose content changes over time, such as a
`latest` path, gives different VMs different images, and Baryon does not
notice. Set `sha256` to pin the content; the importer then refuses any
other image.

The import runs in a separate import Pod, `<vm>-import`, before the VM's
first Anchor Pod exists (see [architecture.md](architecture.md#import-pod-implemented-tested-by-the-deletion-and-storage-e2e-on-the-k0s-cluster)).
The importer (`baryon-importer import`, `crates/importer/src/import.rs`, in its own image) applies
the same digest rule on its own, refuses a redirect from HTTPS to plain HTTP,
and resolves relative redirect locations against the current URL path. It
refuses an image larger than `rootDisk.size` before writing past it.

The root disk claim layout is contract
([api-stability.md](api-stability.md#the-contract)): in filesystem mode,
`disk.img` at the volume root; in block mode, the whole device. A claim
prepared outside Baryon must follow it and needs nothing else. The
importer also writes `.baryon-import.json` (the import marker) next to an
imported `disk.img`; its content is internal.

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
[compatibility.md](compatibility.md#release-specific-notes)).

To use existing data, such as a restored snapshot, a clone, or a claim
from a deleted VirtualMachine, reference the claim with
`rootDisk.source.persistentVolumeClaim` in a new VirtualMachine. A
VirtualMachine with an `http` source imports only into a volume that was
dynamically provisioned for the claim Baryon created, and refuses
anything else (see the storage reasons below).

### Restarting a VM

To restart a running VM, set the annotation
`virtualization.baryon.io/restartedAt` to a new non-empty value:

```bash
kubectl annotate virtualmachine debian --overwrite virtualization.baryon.io/restartedAt="$(date -u +%FT%TZ)"
```

The contract:

- A value that differs from the one the current Anchor Pod was created
  with restarts a running VM exactly once: baryon-controller deletes the
  Anchor Pod (event `Restarting`) and creates a new one that carries the
  value.
- The value is opaque to Baryon; an RFC 3339 time, as above, is
  recommended. An empty value requests nothing.
- Removing the annotation does nothing. Releases before 0.1.0-rc1
  reported `RestartRequired` with reason `SpecChanged` after a removal;
  that was a bug.
- Only a difference from the current Anchor Pod's value counts. For
  example, removing the annotation and then setting it back to the value
  the current Anchor Pod was created with does nothing.
- On a stopped VM the annotation has no effect: there is no Anchor Pod to
  replace, and the next start creates one anyway.
- It is an annotation, so it does not change `metadata.generation`.
- Anchor Pods created by a restart request of an earlier release still
  match after an upgrade and are not reported as `RestartRequired`.

Status: Implemented; unit test for the hash rule
(`crates/controller/src/anchor.rs`, `restart_requests_do_not_change_the_hash`).
The restart itself is Tested by the lifecycle E2E (see
[testing.md](testing.md#e2e-tests)), also with this change.

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

Because the fields are copied to pods, they have the meaning and the
availability they have in the cluster's Kubernetes version: a field or
behavior that the cluster's Kubernetes version does not support (for
example a newer `topologySpreadConstraints` field) behaves on VM pods as
it does on any pod there. An empty `placement: {}` places the VM like no
placement, and changing between `placement: {}` and no placement does not
report `PlacementChanged`.

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
  Anchor Pod is created before (ADR 0004). Such a toleration without
  `tolerationSeconds` keeps the VM on a dead node indefinitely: the pod is
  never evicted, so the VM does not start anywhere else until someone
  deletes the Anchor Pod. Inferred from the fencing rule and the
  Kubernetes taint-based eviction behavior; not tested.

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
only when Baryon consumes it and it is tested; where its requirement goes
in the API is decided by the ADR that ships it (ADR 0012). VirtualizationNode
does not publish host inventory; baryon-node-agent logs it
([VirtualizationNode](#virtualizationnode-cluster-scoped-short-name-vnode)).

### Status

| Field | Description |
|---|---|
| `conditions` | See [Condition contract](#condition-contract). |
| `nodeName` | Node of the current Anchor Pod. |
| `anchorPod` | Name of the current Anchor Pod. Use this field instead of deriving the name from the VM name. |
| `addresses` | Guest addresses: the Anchor Pod IPs. Each entry has `type` (only `InternalIP`) and `address`. An atomic list: every status write replaces it as a whole. |
| `macAddress` | Guest MAC address, stable for the life of the VM. |
| `rootDiskClaim` | Claim holding the root disk. Use this field instead of deriving the name from the VM name. |
| `restartCount` | Restarts of the Anchor container in the current Anchor Pod, as kubelet counts them (each one restarts the VM). Starts again from 0 with a new Anchor Pod (stop and start, restart request, node reboot). |
| `observedGeneration` | Generation the status was computed for. |
| `rootDiskImport.claimUID` | Internal; may change or disappear in any release. Set by baryon-controller when the root disk claim with this UID held an imported or used disk; reset when the claim is recreated. A one-way marker that only prevents another import into the same claim; the record on the PersistentVolume is the authority (ADR 0006). |
| `identityConflictNodes` | Internal; may change or disappear in any release. Nodes where this VM cannot run because another VM there has the same host identity (a set, at most 16). New Anchor Pods avoid them. Cleared when the VM is stopped. |

Optional fields are not nullable in the CRD schemas: an explicit `null`
is dropped by the API server, the same as an absent field.

#### Condition contract

Status: Implemented; the derivation is covered by unit tests in
`crates/controller/src/status.rs` and `crates/node-agent/src/publisher.rs`.

| | VirtualMachine | VirtualizationNode |
|---|---|---|
| Only writer | baryon-controller | baryon-node-agent on that node |
| Types | `Scheduled`, `StorageReady`, `Running`, `Ready`, `NetworkReady`, `RestartRequired` | `Ready`, `RuntimeHealthy` |
| `observedGeneration` on each condition | the VM generation the status was computed for | not set (no spec) |
| When the conditions count | always; compare `observedGeneration` with `metadata.generation` after a spec change | only while `status.heartbeatTime` is within 90 s; otherwise treat them as Unknown ([VirtualizationNode](#virtualizationnode-cluster-scoped-short-name-vnode)) |

Rules for every condition:

- Transitions: `lastTransitionTime` changes only when the status
  (`True`, `False`, `Unknown`) changes. The reason and message can change
  while the status stays the same.
- Reasons: the Baryon reasons listed in this section are contract
  ([api-stability.md](api-stability.md#the-contract)). Reasons that
  baryon-controller copies from the scheduler and kubelet, such as
  `Unschedulable`, `SchedulingGated`, `ContainerCreating`,
  `CrashLoopBackOff`, `ErrImagePull`, and `CreateContainerConfigError`,
  are not Baryon contract: Kubernetes defines them.
- Messages are human text, not for parsing, and can change in any
  release. Messages that baryon-controller copies from the scheduler, the
  API server (`PodRejected`, `ClaimRejected`), a node (`NetworkReady`), or
  the Anchor Pod's `Ready` condition (`NodeUnreachable`) are cut to at
  most 1024 bytes. Failure messages from the Anchor
  container's termination message are at most 4000 bytes (baryon-anchor
  cuts them); kubelet's waiting messages are copied as kubelet wrote
  them. Baryon puts no secrets in messages. The `ImportFailed` message can
  contain the source URL or a redirect location, for example `GET <url>
  returned 404 Not Found`, which is one reason the URL must not carry
  secrets. Events never contain the source URL.

VirtualMachine conditions:

| Type | Meaning | True | False | Unknown |
|---|---|---|---|---|
| `Scheduled` | The current Anchor Pod is bound to a node. | `Scheduled` | There is no Anchor Pod, or it is not bound; the reasons are in [Not running reasons](#not-running-reasons). | not used |
| `StorageReady` | The root disk claim exists and is bound, and for an `http` source its import is recorded on the PersistentVolume. For a `persistentVolumeClaim` source, a bound claim; its content is not checked. | `DiskReady` | See [StorageReady reasons](#storageready-reasons). | not used |
| `Running` | The Anchor container of the current Anchor Pod runs. It runs the VMM or is starting it, and a VMM exit ends the container. It is not evidence that the guest runs; that is `Ready`. | `AnchorRunning` | See [Not running reasons](#not-running-reasons). | `NodeUnreachable`; see [Node unreachable](#node-unreachable) |
| `Ready` | The guest produced boot evidence in the current VMM run: it acquired its address over DHCP or showed a console login prompt. | `GuestBooted` | `Booting` while `Running` is True; otherwise the `Running` reason. | `NodeUnreachable`; see [Node unreachable](#node-unreachable) |
| `NetworkReady` | baryon-virtd on the VM's node reports the VM's DHCP responder running and answering liveness probes. Derived only while `Running` is True. | `AddressServiceRunning` | `DhcpResponderRestarting`, `DhcpResponderFailing` | `RuntimeUnreachable`, `RuntimeDoesNotReport`, `NotReported`, `NotRunning`, `NodeUnreachable`; see [NetworkReady](#networkready) |
| `RestartRequired` | The current Anchor Pod differs from the pod baryon-controller would create now; the difference applies at the next start. | `SpecChanged` (the class, or a spec field other than `placement`, changed after the Anchor Pod was created), `PlacementChanged` (see [Placement](#placement)), `AnchorImageChanged` (baryon-controller now uses a different anchor image). When several apply, the first in this order is reported. | `UpToDate` | not used |

How the VM conditions relate:

- `Ready` True implies `Running` True: both come from the same Anchor
  container.
- `Ready` is latched per VMM run: once the guest showed boot evidence,
  `Ready` stays True until the VMM exits or the Anchor Pod stops, also
  across a reboot inside the guest. It turns False when the anchor cannot
  reach baryon-virtd for 30 s (ADR 0008).
- `StorageReady` can be False while a running VM stays `Ready`: an
  existing Anchor Pod decides `Running` and `Ready`, and a storage problem
  found later, such as a missing import record, matters for the next
  start.
- No condition reports the health of applications in the guest. `Ready`
  means boot evidence was observed, not that every guest service is up;
  on a disk's first boot the guest typically becomes reachable a few
  seconds later.

##### Not running reasons

False reasons of `Scheduled`, `Running`, and `Ready` (for `Ready` also
`Booting`, above). Reasons marked "no pod" are reported only while there
is no Anchor Pod; while a pod exists, the pod decides.

| Reason | Conditions | Meaning |
|---|---|---|
| `Pending` | all three | Creating the Anchor Pod, or the pod is not bound and the scheduler gave no reason. |
| `Stopped` | all three, no pod | `runPolicy` is `Stopped`. |
| `Stopping` | `Running`, `Ready` | The Anchor Pod is being deleted; the guest is shutting down. On a node that stopped reporting, `NodeUnreachable` (Unknown) is reported instead. |
| `Starting` | `Running`, `Ready` | The Anchor container has not started, or waits without a kubelet reason. |
| `ClassNotFound` | all three, no pod | The VirtualMachineClass does not exist. |
| `InvalidClass` | all three, no pod | The class has no `cpu.vcpus` (stored before 0.1.0-rc1) or an unusable `memory.size`. |
| `InvalidSpec` | all three, no pod; `Running` and `Ready` also with a pod | The spec cannot be used, for example `rootDisk.size` cannot be read. With a pod: baryon-anchor could not use its VM spec. |
| `ClaimNotFound` | all three, no pod; also `StorageReady` | The claim named in `rootDisk.source.persistentVolumeClaim` does not exist. |
| `ClaimConflict` | all three, no pod; also `StorageReady` | A claim with the name Baryon uses for this VM's root disk exists and this VM does not own it. |
| `ClaimRejected` | all three, no pod; also `StorageReady` | The API server refused the root disk claim Baryon creates (HTTP 422 or 403). See below. |
| `ClaimInUse` | all three, no pod | A Baryon pod of another VirtualMachine uses the root disk claim. See below. |
| `AnchorPodConflict` | all three | A pod with the Anchor Pod's name exists that this VM does not control. See below. |
| `ImportPodConflict` | all three, no pod | A pod with the import Pod's name exists that this VM does not control. See below. |
| `PodRejected` | all three, no pod | The API server refused the import Pod or the Anchor Pod (HTTP 422 or 403). See below. |
| the `StorageReady` reason | all three | While the disk is not ready and no Anchor container runs, for example `ImportingDisk` or an import refusal. |
| failure reasons | `Running`, `Ready` | The previous run of the Anchor container ended with a reason from baryon-virtd or baryon-anchor; see [Failure reasons](#failure-reasons). |
| `VmStopped` | `Running`, `Ready` | The Anchor container's termination message has no `Reason:` prefix; the message is the termination message. |

##### Node unreachable

`Running` and `Ready` are Unknown with reason `NodeUnreachable` when the
Anchor Pod's `Ready` condition is False while kubelet's last status of
the Anchor container is ready. That is what Kubernetes leaves when a node
stops reporting: the node lifecycle controller marks the node's pods not
ready and keeps kubelet's last container status. The message is the pod
condition's message, cut to 1024 bytes, or "the Anchor Pod's node
stopped reporting; the VM's state is unknown". `NetworkReady` is
Unknown with the same reason. `NodeUnreachable` takes precedence over
`Stopping`: an Anchor Pod evicted from such a node stays Terminating, and
the guest may still run. A VM that was not Ready yet when its node was
lost (its Anchor container running but not ready, for example while
booting) keeps `Running` True and `Ready` False (`Booting`), because the
pod looks like a booting one; only `NetworkReady` (Unknown after 90 s)
shows the loss. baryon-controller creates no new Anchor Pod
while the old pod object exists (fencing, ADR 0004); what ends that is in
the [lifecycle contract](kairos-integration.md#lifecycle-contract).
Status: Tested: unit test
`a_ready_vm_on_a_node_that_stopped_reporting_is_unknown` in
`crates/controller/src/status.rs`, and a power loss of a VM's node on the
k0s cluster (manual check; see the
[lifecycle contract](kairos-integration.md#lifecycle-contract)). Releases before 0.1.0-rc1 reported
the last state instead: `Ready` True until the pod was evicted, then
`Stopping`.

##### StorageReady reasons

| Reason | Meaning |
|---|---|
| `ClaimNotFound`, `ClaimConflict`, `ClaimRejected` | The claim problem found while resolving the disk; the same reason and message as on `Scheduled` and `Ready`. |
| `ClaimMissing` | There is no root disk claim to report for another reason, for example `InvalidSpec`; the reason on `Scheduled` and `Ready` says why. |
| `ClaimPending` | The claim is not bound yet. `WaitForFirstConsumer` claims bind when the import Pod or the Anchor Pod is scheduled. |
| `ImportPending` | The claim is bound and the import Pod has not started its importer. |
| `ImportingDisk` | The importer runs, or it finished and baryon-controller has not yet recorded the import. |
| `ImportFailed` | The importer failed; the message says why. An importer that ended without a message (for example killed for exceeding its memory) is reported with its exit code. |
| `DiskNotProvisionedForClaim`, `DiskInitializedForAnotherClaim`, `DiskSourceMismatch`, `DiskImportRecordInvalid`, `ImportStateUnknown`, `DiskBindingPending` | Import refusals; see [Import refusals](#import-refusals). |

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

`ClaimRejected`: the API server refused to create the root disk claim
for an `http` source (HTTP 422 or 403), for example for an exceeded
storage quota or an admission policy that does not allow the claim's
StorageClass. `StorageReady`, `Scheduled`, `Running`, and `Ready` carry
the reason and the API server's message, cut to 1024 bytes. Releases
before 0.1.0-rc1 only logged the refusal. As for `PodRejected`,
baryon-controller tries again every 60 s while `ClaimRejected` is the
reason on `Ready`, because a quota that frees up or a StorageClass created
later can make the claim acceptable. Fix the cause, for example the
namespace's quota; `storageClassName` itself cannot be changed because
`rootDisk` is immutable, so a wrong StorageClass needs a new
VirtualMachine.

`ImportingDisk` with the message "the importer finished; recording the
import": the importer exited 0 and baryon-controller has not yet written
the record on the PersistentVolume. An importer's exit code alone never
makes the disk ready.

##### Import refusals

None of them imports or starts the VM, and their messages
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

##### Failure reasons

Failure reasons reach `Running` and `Ready` through the Anchor
container's termination message (`Reason: detail`), written by
baryon-anchor from baryon-virtd's last VM status or from its own checks.
They describe the previous run of the Anchor container in the current
Anchor Pod.

| Reason | Source | Meaning |
|---|---|---|
| `AttachRefused` | baryon-virtd | baryon-virtd refused the attach without a more specific reason, for example an invalid VM spec. |
| `IdentityConflict` | baryon-virtd | Another VM on the node runs with the same host identity; baryon-controller places the VM elsewhere (see [Host identity](#host-identity)). |
| `NotAnchorSupervisor` | baryon-virtd | baryon-virtd did not recognize the anchor as the Anchor supervisor (ADR 0015): the anchor is PID 1 of its container but runs in a user namespace other than the initial one, or shares baryon-virtd's mount or network namespace. Seen on a node that does not meet the [node requirements](compatibility.md#kubernetes-requirements), for example a rootless runtime. An anchor that is not PID 1 (an init injected by the runtime, a shared PID namespace) does not get this far: it refuses to run, and the container restarts with `refusing to run` in its log. |
| `ProtocolIncompatible`, `UnsupportedFeature` | runtime protocol negotiation | The node's baryon-virtd speaks another major version, or lacks a feature the VM needs (ADR 0012). |
| `InvalidSpec` | baryon-anchor | The anchor could not use the VM spec it was given. |
| `StorageFailed` | baryon-virtd | baryon-virtd refuses to hand the disk to the VMM, for example a path or file kind the Anchor container does not use for its disk, or a device node outside the container's `/dev`. |
| `DiskInUse` | baryon-virtd | Another VM on the node already uses the same block device. |
| `BlockDeviceAccessDenied` | baryon-virtd | The block device node is not readable and writable by the VM's host identity; the container runtime must give block volume devices the pod's `runAsUser` and `runAsGroup` (containerd: `device_ownership_from_security_context = true`, see [compatibility.md](compatibility.md#csi-requirements)). |
| `ResourcesInvalid`, `ResourcesUnbounded`, `ResourcesExceedPodLimits` | baryon-virtd | The Anchor container's cgroup limits cannot be read, are not set, or are smaller than the VM needs. |
| `NetworkFailed` | baryon-virtd | The TAP or the network handoff in the pod network namespace failed. |
| `CgroupFailed`, `CgroupDriverUnsupported` | baryon-virtd | The VMM could not be placed in the Anchor container's cgroup, or that cgroup is not a systemd scope (kubelet cgroupfs driver, Not supported). |
| `RuntimeDirFailed` | baryon-virtd | The VM's runtime directory could not be created. |
| `KvmUnavailable`, `VmmStartFailed` | baryon-virtd | KVM was not usable, or Cloud Hypervisor did not start. |
| `VmmKilled`, `VmmExited`, `VmmFailed`, `OutOfMemory` | baryon-virtd | The VMM was killed by a signal, exited while baryon-virtd was not its parent, failed with an error, or was killed by the OOM killer in the pod cgroup. |
| `GuestPoweredOff` | baryon-virtd | The guest shut down; the Anchor container exits and kubelet starts it again, which restarts the VM. |

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
| Unknown | `RuntimeUnreachable` | The node has no report, its report is stale (the controller has not seen its heartbeat change for 90 s), or its `RuntimeHealthy` condition is not True (its reason is in the message, for example `NodeAgentStopped` right after the node agent was stopped). |
| Unknown | `RuntimeDoesNotReport` | The node's baryon-virtd speaks runtime protocol 1.1 or earlier and does not report the responder. |
| Unknown | `NotReported` | The node's report has no entry for the current Anchor Pod. On a VM's first start this lasts until the responder has answered its first liveness probe and the node agent has reported it. |
| Unknown | `NotRunning` | The VM is not running on a node. |
| Unknown | `NodeUnreachable` | The VM's `Running` and `Ready` are Unknown with `NodeUnreachable`: the Anchor Pod's node stopped reporting. |

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
| Unknown, `NodeUnreachable` | Unknown, `NodeUnreachable` | The Anchor Pod's node stopped reporting; whether the VM runs is unknown. |

baryon-controller emits a Warning event `NetworkNotReady` each time
`NetworkReady` changes to False, and a Normal event `NetworkReady` only
when it changes from False to True, not on every start.

During a node agent rollout, the VMs on a node whose node agent is being
replaced can show `NetworkReady` Unknown with reason `RuntimeUnreachable`
and the message "the node's Baryon runtime is not healthy
(NodeAgentStopped); the state of the guest network service is unknown"
until the new node agent publishes. No Warning event
is emitted for it, because events fire only on a change to False. `Ready`
is not affected.

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
        # Recommended: pin the image content. Download the image once,
        # compute its SHA-256 (sha256sum), and set it here.
        # sha256: <64 hex characters>
```

The URL above is a `latest` path, whose content changes when Debian
publishes a new image. Without `sha256`, two VMs created at different times can
get different images, because Baryon identifies the import by the URL
digest, not by the content (see
[Root disk size and import identity](#root-disk-size-and-import-identity)).

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

The `vm-name` and `vm-uid` labels and the copy rule below are contract
([api-stability.md](api-stability.md#the-contract)); the annotations are
internal and can change in any release.

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
node affinity on `metadata.name`. It acts only when the anchor's
termination message starts with `IdentityConflict:` and the anchor exited
with code 3, because other processes of the Anchor container can write the
message (ADR 0015); a v0.1.0-rc1 anchor exits with 1, so its VM stays on
the node until its pod is replaced. The list is cleared when the VM is
stopped. On a cluster where every Baryon node is recorded, the VM stays not
ready until it is stopped and started again after the conflicting VM is
gone. baryon-controller reads the recorded nodes from the API server on
every reconcile, so a node it has just recorded is kept. Status:
Implemented; unit tests, and Tested on the k0s cluster by
`tests/e2e/identity-conflict.sh`, which injects a conflict with the
current anchor and with a v0.1.0-rc1 anchor; recovery of the latter:
[troubleshooting.md](troubleshooting.md#an-anchor-pod-with-the-v010-rc1-anchor-image).

The guest MAC address is a separate, guest-visible identity, derived from
the VirtualMachine UID (`02:` followed by the last 40 bits of the UID) and
reported in `status.macAddress`.

## VirtualizationNode (cluster-scoped, short name `vnode`)

One object per Baryon node, named after the Node and owned by it, written
only by the baryon-node-agent on that node (enforced by the
`baryon-node-agent-own-node` admission policy in `deploy/base/policy/`). No spec.

It reports whether the node can run Baryon VMs, its runtime versions,
and its runtime health. It does not publish host inventory: Kubernetes
already publishes what the scheduler uses on the Node object (CPU, memory,
architecture, kernel, OS image), devices belong to Dynamic Resource
Allocation, and baryon-node-agent logs the rest (see
[Host inventory](#host-inventory)).

| Status field | Description |
|---|---|
| `conditions` | `Ready` and `RuntimeHealthy`; see the table below. Valid only while `heartbeatTime` is recent. |
| `heartbeatTime` | Last refresh by the node agent: every 30 s by default, 5 to 30 s (`--status-interval-seconds`). |
| `runtime.virtdVersion` | Version of baryon-virtd on the node. |
| `runtime.virtdGitCommit` | Optional, informational. The commit baryon-virtd was built from; a `-dirty` suffix marks a build from a tree with uncommitted changes, `unknown` a build without the information. Omitted when baryon-virtd reports none. The format is not contract. |
| `runtime.hypervisorVersion` | Version of Cloud Hypervisor on the node. |
| `runtime.protocolVersion` | Internal. Runtime protocol version of baryon-virtd, `<major>.<minor>`; `1.0` for a baryon-virtd that does not report one. Compatibility is reported by `RuntimeHealthy`. |
| `runtime.features` | Internal. Runtime protocol features baryon-virtd implements (ADR 0012), a set. |
| `vmCount` | VMs known to baryon-virtd. Omitted while baryon-virtd is unreachable. |
| `vms` | Internal: read by baryon-controller for the VMs' `NetworkReady` condition; users read `NetworkReady`. A map keyed by `anchorPodUID`, at most 256 entries (the node agent publishes the first 256 and logs a warning when there are more): `anchorPodUID`, and `network` with `state` (`Ready`, `Restarting`, or `Failing`), `reason` (at most 64 bytes), `message` (at most 1024 bytes), `lastTransitionTime`, and `leaseExpiresTime`. Entries carry no VM names, namespaces, addresses, PIDs, paths, or TAP names. A baryon-virtd before runtime protocol 1.2 gives entries without `network`. |

Conditions:

| Type | Meaning | True | False | Unknown |
|---|---|---|---|---|
| `Ready` | The node can run Baryon VMs: baryon-virtd is reachable, speaks a compatible runtime protocol, and KVM is usable. | `RuntimeReady` | `RuntimeUnreachable`, `ProtocolIncompatible`, `KvmUnavailable` | `NodeAgentStopped` |
| `RuntimeHealthy` | baryon-virtd answered, host discovery succeeded, and its runtime protocol major version matches the cluster release (ADR 0012). | `RuntimeReachable` | `RuntimeUnreachable`, `ProtocolIncompatible` | `NodeAgentStopped` |

| Reason | Meaning |
|---|---|
| `RuntimeUnreachable` | The node agent cannot get a host report from baryon-virtd; the message is the error. `runtime`, `vmCount`, and `vms` are omitted. |
| `ProtocolIncompatible` | baryon-virtd speaks another runtime protocol major version, or answered with a host report whose schema version this node agent does not read. Releases before 0.1.0-rc1 reported the second case as `RuntimeUnreachable`. |
| `KvmUnavailable` | baryon-virtd is healthy and KVM is not usable on the host; the message is the host's reason. |
| `NodeAgentStopped` | The node agent was stopped (SIGTERM, for example for an upgrade or a reboot) and set both conditions to Unknown on its way out. The rest of the status, including `heartbeatTime`, is kept, so the heartbeat ages. |

The `Ready` True reason was `KvmAvailable` before 0.1.0-rc1.

Staleness: a node agent that is stopped sets both conditions to Unknown;
a node agent that crashes, or whose node fails, leaves its last
conditions as they were. Consumers therefore count the conditions only
while `heartbeatTime` is within 90 s, three maximum intervals, and treat
them as Unknown otherwise. `heartbeatTime` is written with the node's
clock. baryon-controller applies the same rule on its own clock: it
treats a node whose heartbeat it has not seen change for 90 s as not
reporting.

Status: Implemented; unit tests in `crates/node-agent/src/publisher.rs`
(an unreachable runtime, an incompatible host report, inventory not in
the status). The `NodeAgentStopped` write on SIGTERM has no unit test; it
is Tested by `tests/e2e/node-agent-outage.sh` on the k0s cluster.

```
$ kubectl get virtualizationnodes
NAME    READY   REASON         VMS   RUNTIME     HYPERVISOR   HEARTBEAT   AGE
node1   True    RuntimeReady   1     0.1.0-rc2   v54.0        12s         12m
```

### Host inventory

baryon-node-agent logs one line with the message `host inventory` when it
starts and whenever the inventory changes: the architecture, kernel
release, OS name, Kairos version, CPU model, online CPU count, nested
virtualization, memory size, whether KVM is usable, whether the IOMMU is
enabled and its group count, the number of NUMA nodes and PCI devices,
and `host_image`: the Hadron base image the Baryon-enabled image was
built from, taken from the image's component manifest
(`/usr/share/baryon/components.json`, field `hadronImage`) and omitted
when the manifest has none. To read it for one node:

```bash
pod="$(kubectl get pods -n baryon-system -l app.kubernetes.io/name=baryon-node-agent --field-selector spec.nodeName=<node> -o name)"
kubectl logs -n baryon-system "$pod" | grep '"host inventory"'
```

The line is logged only at start and on a change, so it is gone once the
container's log has been rotated; restarting the node agent logs it again.
The Node object has the architecture, kernel release, and OS image in
`status.nodeInfo`, and the CPU and memory capacity in `status.capacity`.

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

The resource names and slot counts are not contract
([api-stability.md](api-stability.md#not-contract)). A ResourceQuota on
them, such as `requests.virtualization.baryon.io/vm`, is therefore Not
supported. To cap the number of VMs in a namespace, use an object count
quota on VirtualMachines:

```yaml
apiVersion: v1
kind: ResourceQuota
metadata:
  name: vm-count
  namespace: <namespace>
spec:
  hard:
    count/virtualmachines.virtualization.baryon.io: "10"
```

This counts VirtualMachine objects, including stopped ones. Kubernetes
mechanism; Baryon has no test for it.

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
