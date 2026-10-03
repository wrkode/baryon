# Storage

Status:

| Area | Status |
|---|---|
| Filesystem-mode root disks | Tested: storage E2E with OpenEBS LVM LocalPV and Ceph RBD on the k0s cluster; lifecycle E2E with local-path on the Hadron k3s node and the kind host |
| Block-mode root disks | Tested: storage E2E with OpenEBS LVM LocalPV and Ceph RBD on the k0s cluster, including a move of a Ceph RBD VM to another node (CSI detach, attach, and NodeStage) |
| Import into a separate import Pod, recorded on the claim before the first Anchor Pod | Tested: storage and deletion E2E on the k0s cluster |
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
| `rootDisk.source.http` | created by Baryon, named `<vm>-root`, owned by the VM, `ReadWriteOnce`, in `rootDisk.volumeMode` (default `Filesystem`) | Written by `baryon-anchor import` in the import Pod `<vm>-import` before the first Anchor Pod exists. The importer streams the raw image, verifies SHA-256 when given (required for plain HTTP), refuses a redirect from HTTPS to plain HTTP, and refuses an image larger than `rootDisk.size` before writing past it. Filesystem mode: into `disk.img`, skipping zero blocks, extended to `rootDisk.size`; a marker file prevents a second import. Block mode: see [Block import](#block-import). baryon-controller then records the import on the claim (annotation `virtualization.baryon.io/imported-from`, `sha256:` and the hex SHA-256 of the URL) and deletes the import Pod. |
| `rootDisk.source.persistentVolumeClaim` | existing, not owned by the VM | filesystem mode: `disk.img` at the volume root; block mode: the whole device. Not imported. |

Images must be raw. qcow2 and other formats are Not supported.

`rootDisk` is immutable, including `size`. To use another disk or image,
create a new VirtualMachine. Disk growth needs an explicit flow (Planned).

### Block import

The importer opens the device `/dev/baryon/root` with `O_EXCL` and
`O_NOFOLLOW` and refuses anything that is not a block device. `O_EXCL`
excludes other exclusive openers (mounts, other importers), not a VMM,
which opens without it; the controller's ordering keeps the importer and
the VMM apart.

1. The first 1 MiB of the device is zeroed before the download starts.
2. The image is streamed. Its first 1 MiB is held in memory; the rest is
   written in place. Runs of zero blocks are zeroed, not written.
3. After the size and SHA-256 checks pass, the rest of the device after the
   image is zeroed, the first 1 MiB of the image is written, and the device
   is synced.

A failed import therefore leaves no partition table or boot loader at the
start of the device. The image tail and a backup GPT at the end of the
device are not cleared by a failed import.

Zeroing uses, in this order: `FALLOC_FL_PUNCH_HOLE` (write-zeroes that may
deallocate, so thin-provisioned storage is not filled), then
`FALLOC_FL_ZERO_RANGE`, then written zeros. The next method is tried only
when the previous one reports `EOPNOTSUPP`; any other error fails the
import. Parts of a range that are not 4 KiB aligned are written as zeros.
On a device without write-zeroes support the kernel completes the request
by writing zeros, which allocates the whole device on a thin backend.

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
| Anchor Pod replacement | kept | the import record on the claim prevents a second import |
| VirtualMachine deletion | deleted for imported disks (owner reference); kept for referenced claims | |
| Move to another node | kept, when the volume is not bound to a node | Tested with Ceph RBD in both volume modes; LVM LocalPV and local-path volumes are bound to their node |
| Node reboot | kept (depends on the CSI driver) | a node-local volume binds the VM to its node |
| Kairos A/B upgrade | kept | claims live outside the OS image |

## Limits and open items

- One disk per VM. Additional disks, read-only disks, and CD-ROM images are
  Not supported in the API (the runtime protocol accepts up to eight).
- One running VM per claim. A `ReadWriteMany` block claim used as a root
  disk by VMs on different nodes is Not supported.
- Live migration and snapshots are Not supported.
- Block mode requires a container runtime that creates granted device nodes
  on the container's `/dev` tmpfs and gives them the pod's `runAsUser` and
  `runAsGroup`. Verified with runc under containerd; CRI-O and crun are not
  verified. Kubernetes user namespaces with bind-mounted device nodes would
  be refused by the `/dev` check (Planned item in
  [security.md](security.md#future-hardening)).
- The claim layout, `disk.img` at the volume root plus the import marker
  `.baryon-import.json`, is a stable format of v1alpha1 (see
  [api.md](api.md)).
- The import record on the claim is trusted as written. Removing the
  `imported-from` annotation makes the next replacement of the Anchor Pod
  import again, which overwrites a block device; filesystem-mode claims are
  protected by the marker file.
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
