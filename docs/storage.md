# Storage

Status:

| Area | Status |
|---|---|
| Filesystem-mode root disks | Tested: storage E2E with OpenEBS LVM LocalPV and Ceph RBD on the k0s cluster; lifecycle E2E with local-path on the Hadron k3s node and the kind host |
| Block-mode root disks | Tested: storage E2E with OpenEBS LVM LocalPV and Ceph RBD on the k0s cluster, including a move of a Ceph RBD VM to another node (CSI detach, attach, and NodeStage) |
| Import into a separate import Pod, recorded on the bound PersistentVolume before the first Anchor Pod | Tested: storage and deletion E2E on the k0s cluster |
| Import integrity on block volumes: no stale data after an import, one import across start, Anchor Pod replacement, and a baryon-controller restart, `ImportStateUnknown` when the record is removed, `AnchorPodConflict` and `ImportPodConflict` for a namespace user's pods | Tested: import integrity E2E with OpenEBS LVM LocalPV and Ceph RBD block volumes on the k0s cluster, including the check that a namespace user cannot set an import approval |
| Refusals `DiskNotProvisionedForClaim`, `DiskInitializedForAnotherClaim`, `DiskSourceMismatch`, `DiskImportRecordInvalid`; the import approval wait | Implemented, unit tests; the approval annotation policy Tested by the CI API server checks and the import integrity E2E |
| One running VM per claim | `ClaimInUse` (controller, a best-effort guard: concurrent reconciles of two VMs can both pass it) Tested by the storage E2E; `DiskInUse` (baryon-virtd) Implemented, unit tests |
| Disk path, kind, and device location checks in baryon-virtd | Tested: hostile-disk E2E on the k0s cluster; unit tests |
| Other CSI drivers | not tested |

The environments and runs are in [testing.md](testing.md#results); the
decision is ADR 0006 (Accepted). Experiment A in
[runtime-boundary.md](runtime-boundary.md) records how the handoff was
reached.

Baryon has no storage system. The VM root disk is a PersistentVolumeClaim,
provisioned and attached by the cluster's CSI driver to the VM's Anchor Pod
(and, for an imported disk, first to its import Pod).

## Disk sources

| Source | Claim | Content |
|---|---|---|
| `rootDisk.source.http` | created by Baryon, named `<vm>-root`, owned by the VM, `ReadWriteOnce`, in `rootDisk.volumeMode` (default `Filesystem`) | Written by `baryon-anchor import` in the import Pod `<vm>-import` before the first Anchor Pod exists. The importer streams the raw image, verifies SHA-256 when given (required for plain HTTP), refuses a redirect from HTTPS to plain HTTP, and refuses an image larger than `rootDisk.size` before writing past it. Filesystem mode: into `disk.img`, skipping zero blocks, extended to `rootDisk.size`; a marker file prevents a second import. Block mode: see [Block import](#block-import). baryon-controller then records the import on the bound PersistentVolume and deletes the import Pod (see [Import record](#import-record)). |
| `rootDisk.source.persistentVolumeClaim` | existing, not owned by the VM | filesystem mode: `disk.img` at the volume root; block mode: the whole device. Used as is: not imported and not cleared. This is the adoption path for existing data, including restored and cloned claims. |

Images must be raw. qcow2 and other formats are Not supported.

`rootDisk` is immutable, including `size`. To use another disk or image,
create a new VirtualMachine. Disk growth needs an explicit flow (Planned).

### Import record

A completed import is recorded in the annotation
`virtualization.baryon.io/disk-import` on the PersistentVolume bound to the
claim, as JSON:

```json
{"vmUID":"<vm-uid>","claimUID":"<claim-uid>","source":"sha256:<hex SHA-256 of the URL>"}
```

`claimUID` and `source` bind the record to the claim and the image;
`vmUID` is for auditing. baryon-controller writes the record only when the
claim is Bound to the volume and the volume is Bound with a `claimRef` that
names the claim's namespace, name, and UID. The PersistentVolume has the
lifetime of the data and namespace users cannot write it; claim metadata,
which they can edit, is not used for the decision. The annotation
`virtualization.baryon.io/imported-from` on claims of earlier releases is
no longer written and is read only to migrate those VMs (see
[compatibility.md](compatibility.md#release-specific-notes)). The design is in ADR
0006.

What baryon-controller does for a VirtualMachine with an `http` source
(`crates/controller/src/disk_import.rs`):

| Volume state | Result |
|---|---|
| Record for this claim UID and source | the disk is imported; the VM starts on it |
| Record for another claim UID | refused: `DiskInitializedForAnotherClaim` |
| Record for another source | refused: `DiskSourceMismatch` |
| Record that is not valid JSON of this form | refused: `DiskImportRecordInvalid` |
| No record; claim annotation of an earlier release equal to the source digest or URL, and StorageReady=True for this claim name since the claim was created | the record is written on the volume (`DiskBindingPending` until claim and volume are bound to each other) |
| No record; block claim known to be used (`status.rootDiskImport` names this claim UID, or the readiness of an earlier release) | refused: `ImportStateUnknown` |
| No record; fresh volume | imported |
| No record; anything else | refused: `DiskNotProvisionedForClaim` |

A volume is fresh when it carries `pv.kubernetes.io/provisioned-by` (set by
dynamic provisioners), its `claimRef.uid` is the claim's UID, it was created
no earlier than the claim, and the claim has no `dataSource` or
`dataSourceRef`. Statically provisioned or pre-bound volumes, restored
snapshots, and clones are therefore never imported into; use them through
`rootDisk.source.persistentVolumeClaim` in a new VirtualMachine, which
every refusal message suggests. A used filesystem-mode claim that lost its
record is not refused for that reason: if its volume is fresh, the importer
finds the marker file and leaves the disk unchanged.

Refusals are reported on `StorageReady` and `Ready` and never import. The
controller does not watch PersistentVolumes; it looks at a refused VM again
every 120 s, and at one in `DiskBindingPending` every 30 s, so a corrected
volume is picked up without a watch.

`status.rootDiskImport.claimUID` marks that the claim held an imported or
used disk. It only prevents another import into that claim: it is kept
while the claim UID is unchanged, also when the disk is refused, and reset
for a recreated claim. The record on the volume is the authority. If both
are lost (for example a restore without status and an administrator
removing the record), a fresh-looking volume can be imported again; this
residual is accepted because cluster administrators are trusted.

### Import approval

The importer writes nothing until it is approved. The import Pod mounts a
downward API file, `/etc/baryon/import/approved`, holding its own
annotation `virtualization.baryon.io/import-approved`, and the importer
(`--approval-file`, `--claim-uid`) waits until the file equals its claim
UID.

- When the claim is already bound to a fresh volume, baryon-controller
  creates the import Pod approved.
- When the claim binds on first use (`WaitForFirstConsumer`), the import
  Pod is created without approval, the claim binds when the pod is
  scheduled, and baryon-controller sets the approval once the volume
  passes the freshness check. The import Pod carries the VM's `spec.placement`
  and labels, so the VM's placement, including its spread constraints and
  pod anti-affinity, decides the node the volume binds on (see
  [api.md](api.md#placement)). A node-local volume then keeps the VM on
  that node; a later placement change does not move it. Because the import
  Pod carries the VM's labels, a NetworkPolicy that denies egress to pods
  with those labels also blocks the download; allow the image server for
  pods labeled `virtualization.baryon.io/import-for` (see
  [networking.md](networking.md#selecting-vms-by-their-labels)).
- When the decision refuses, baryon-controller deletes the import Pod.
- After 30 minutes without approval the importer fails and kubelet restarts
  it (`restartPolicy: OnFailure`).

Only baryon-controller may set, change, or remove the approval annotation
(admission policy `baryon-import-approval`, see
[security.md](security.md#admission-policies)).

The import counts as completed only on evidence from the import Pod's spec,
which cannot change after creation: the pod requests
`virtualization.baryon.io/import` (which only baryon-controller can request,
`baryon-vm-resource`), mounts the claim, and its importer's arguments carry
`--url=<url>` and `--claim-uid=<claim-uid>`; and the importer exited 0. An
importer that exited 0 shows `ImportingDisk` ("recording the import") until
the record is written; only then is StorageReady True. A pod named
`<vm>-import` that does not request the import resource or is not owned by
the VM is never used (`ImportPodConflict`); an import Pod for another claim
UID is deleted.

### Block import

Contract: after a successful import every byte of the device is the image's
byte at that offset or zero. A new volume may hold data from the backend's
earlier use; none of it survives an import.

The importer opens the device `/dev/baryon/root` with `O_EXCL` and
`O_NOFOLLOW` and refuses anything that is not a block device. `O_EXCL`
excludes other exclusive openers (mounts, other importers), not a VMM,
which opens without it; the controller's ordering keeps the importer and
the VMM apart. A device whose logical block size (`BLKSSZGET`) does not
divide 4096 is refused.

1. The first and the last 1 MiB of the device are zeroed and flushed before
   the download starts.
2. The image is streamed. Image data that falls into the first or the last
   1 MiB of the device is held in memory; the rest is written in place.
   Runs of zero blocks are zeroed, not written.
3. The download length must equal `Content-Length` when the server sends
   one. Without a `sha256`, a response without `Content-Length` is refused
   (also for filesystem imports), because a truncated body could not be
   detected.
4. After the size and SHA-256 checks pass, everything after the image is
   zeroed, the held data at both ends is written, and the device is synced.

A failed import therefore leaves no partition table, boot loader, or backup
GPT at either end of the device.

Zeroing uses, in this order: `FALLOC_FL_PUNCH_HOLE` (block layer
write-zeroes that may deallocate and must read back zero), then
`FALLOC_FL_ZERO_RANGE` (the kernel writes zeros when the device has no
write-zeroes support), then written zeros. The next method is tried only
when the previous one reports `EOPNOTSUPP`; any other error fails the
import. Parts of a range that are not 4 KiB aligned are written as zeros.
`BLKDISCARD` is not used: discard does not guarantee that the range reads
back zero.

Every block import zeroes or writes the whole device. That is fast where
the backend supports write-zeroes and takes time proportional to the device
size otherwise.

Thin storage (Documented residual): dm-thin has no write-zeroes support, so
an import allocates the whole volume on a thin pool. A thin pool must have
room for the full size of every volume imported into it and must zero newly
provisioned blocks. A claim adopted with
`rootDisk.source.persistentVolumeClaim` is used as is.

## Handoff to the VMM

| Step | Process | Mechanism |
|---|---|---|
| Attach and mount | CSI, kubelet | into the Anchor Pod: `/var/lib/baryon/disks/root` (filesystem) or `/dev/baryon/root` (block) |
| Resolve the disk | baryon-virtd | `openat2` on the anchor's `/proc/<pid>/root` with `RESOLVE_IN_ROOT` and `RESOLVE_NO_MAGICLINKS`, as `O_PATH`. Only `/var/lib/baryon/disks/root/disk.img` as a regular file and `/dev/baryon/root` as a block device are accepted (`UnknownPath`, `KindMismatch`); a block device node must be on the container's `/dev` tmpfs (`DeviceOutsideDev`). All three are reported as `StorageFailed`. |
| One VM per block device on a node | baryon-virtd | a node-local registry refuses a second VM with the same block device number (`DiskInUse`); the device numbers are kept in the runtime record, so the refusal holds across a baryon-virtd restart |
| Access check | baryon-virtd | `check_device_access` reports a block device the VM's identity cannot read and write (`BlockDeviceAccessDenied`). It is a diagnostic: the kernel's permission check when the VMM reopens the device, and the Anchor container's device cgroup, decide |
| Open the disk | Cloud Hypervisor, as the VM's host identity | `path=/proc/self/fd/N`, reopened with normal permission checks |
| Lock | Cloud Hypervisor | `F_OFD_SETLK`, exclusive for read-write disks. For a filesystem-mode disk, two VMMs on one node open the same file and the lock excludes the second. For a block device it does not: each container has its own device node, and the lock is on that node's inode. One VM per block device comes from `ClaimInUse` in the controller and `DiskInUse` on the node |
| Reopen after a guest reset | Cloud Hypervisor | the same `/proc/self/fd/N` path inside the running VMM, with Landlock enforced (Tested on the k0s cluster, the Hadron k3s node, and the kind host) |
| Stop using the disk | container runtime and baryon-virtd | the VMM runs in the Anchor container's cgroup and is stopped with the container, before volume teardown |
| Unmount, detach | kubelet, CSI | standard; a Ceph RBD block volume was detached and attached on another node during the storage E2E |

baryon-virtd never opens pod-controlled paths through host directories, and
the VMM can only open what the VM's host identity, which the Anchor
container shares, can open.

## Block volumes and device ownership

A raw block volume reaches the container as a device node that the
container runtime creates on the container's `/dev`. kubelet does not apply
`fsGroup` to raw block devices, so the node's owner and mode come from the
container runtime. With containerd, `device_ownership_from_security_context =
true` makes the node owned by the pod's `runAsUser` and `runAsGroup`, the
VM's host identity (observed: Ceph RBD nodes mode 0660, LVM LocalPV nodes
mode 0600). Without it the VM fails with `BlockDeviceAccessDenied`. The
setting and its side effects are in
[compatibility.md](compatibility.md#csi-requirements) and
[security.md](security.md#host-paths-and-devices).

## What survives what

| Event | Disk content | Notes |
|---|---|---|
| VM restart, stop and start | kept | the claim is independent of the Anchor Pod; no import runs again (checked by the storage E2E) |
| Anchor Pod replacement | kept | the import record on the PersistentVolume prevents a second import (checked by the storage and import integrity E2E, also after a baryon-controller restart) |
| VirtualMachine deletion | deleted for imported disks (owner reference); kept for referenced claims | |
| Move to another node | kept, when the volume is not bound to a node | Tested with Ceph RBD in both volume modes; LVM LocalPV and local-path volumes are bound to their node |
| Node reboot | kept (depends on the CSI driver) | a node-local volume binds the VM to its node |
| Kairos A/B upgrade | kept | claims live outside the OS image |

## Limits and open items

- One disk per VM. Additional disks, read-only disks, and CD-ROM images are
  Not supported in the API (the runtime protocol accepts up to eight).
- One running VM per claim. A `ReadWriteMany` block claim used as a root
  disk by VMs on different nodes is Not supported.
- Live migration and snapshots are Not supported (live migration:
  [ADR 0013](adr/0013-live-migration.md)).
- Block mode requires a container runtime that creates granted device nodes
  on the container's `/dev` tmpfs and gives them the pod's `runAsUser` and
  `runAsGroup`. Verified with runc under containerd; CRI-O and crun are not
  verified. Kubernetes user namespaces with bind-mounted device nodes would
  be refused by the `/dev` check (Planned item in
  [security.md](security.md#future-hardening)).
- The claim layout, `disk.img` at the volume root plus the import marker
  `.baryon-import.json`, is a stable format of v1alpha1 (see
  [api.md](api.md)).
- Storage tooling (backup, restore, migration between clusters) must
  preserve PersistentVolume annotations, and dynamic provisioners must set
  `pv.kubernetes.io/provisioned-by` (see
  [compatibility.md](compatibility.md#csi-requirements)).
- A volume with the `Retain` reclaim policy keeps its record after the claim
  is deleted; a VirtualMachine with an `http` source whose new claim binds
  to it is refused (`DiskInitializedForAnotherClaim`).
- Imported images are mode 0600, owned by the VM's host identity, which is
  stable for the life of the VirtualMachine object (ADR 0011). The pod's
  `fsGroup` is the VM's gid. A VirtualMachine re-created under the same name
  is a new object with a new identity, so it cannot open an image from a
  retained or restored claim unless the storage applies `fsGroup`
  ownership; not tested. See [design-notes.md](design-notes.md).
- A reused device minor after a forced detach could point a VMM's reopen at
  another volume; recording and verifying the block device identity across
  VMM recovery is Planned.
- Teardown ordering under SIGKILL, forced delete, and baryon-virtd down with
  a CSI driver that stages and attaches is not tested; the ordinary path
  (stop, move, delete) is Tested with Ceph RBD.
- Kubernetes volume populators are the intended long-term mechanism for
  image import (ADR 0006).
