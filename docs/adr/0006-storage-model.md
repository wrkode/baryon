# 0006: PVC disks are resolved in the Anchor mount namespace

Status: Accepted for filesystem-mode and block-mode claims. Filesystem mode
is Tested with local-path on a single Baryon-enabled Hadron node (k3s) and
on a historical development cluster in which a kind node container acted as
the host, including the disk reopen after a guest reset with Landlock
enforced, and with OpenEBS LVM LocalPV and Ceph RBD on three Hadron nodes
running k0s. Block mode is Tested with OpenEBS LVM LocalPV and Ceph RBD on
the k0s cluster, including a move to another node through CSI detach,
attach, and NodeStage (see [testing.md](../testing.md#results)).
Date: 2026-10-02, revised 2026-10-03

## Context

The VM root disk is a PVC attached to the Anchor Pod by CSI. A
filesystem-mode PVC is mounted into the container; a block-mode PVC appears
as a device node in the container's /dev. Under ADR 0005 the VMM is started
from the host, where neither path is visible, and Cloud Hypervisor only
accepts disk paths.

Cloud Hypervisor takes an exclusive OFD lock (`F_OFD_SETLK`, `F_WRLCK`) on
each read-write disk at boot and releases it when the last fd closes. There
is no option to disable the lock.

The container sees a volume through its own bind mount. kubelet's unmount of
its mount succeeds whether or not the VMM still has the file open.

## Decision

- The Anchor Pod mounts the root disk at a fixed container path:
  `/var/lib/baryon/disks/root/disk.img` for filesystem mode, or the device
  path `/dev/baryon/root` for block mode.
- baryon-virtd opens the Anchor process root (`/proc/<pid>/root`, guarded by
  the peer pidfd) and resolves the disk path with
  `openat2(RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS)` as `O_PATH`. It accepts
  only the two paths above, each with its kind: a regular file at the
  filesystem-mode path and a block device at the block-mode path. A block
  device node must be on the container's `/dev` tmpfs, where the container
  runtime creates the nodes it grants.
- The `O_PATH` fd is inherited by Cloud Hypervisor, which is given
  `path=/proc/self/fd/N,image_type=raw`. Cloud Hypervisor reopens the file
  with its own credentials (the Anchor uid), so ordinary permission checks
  apply as if the pod opened the disk.
- Teardown safety comes from ordering, not from the open file: the VMM runs
  in the Anchor container's cgroup and is stopped by the container runtime
  before network and volume teardown (ADR 0005). This holds independent of
  the CSI driver, because the VMM runs in the Anchor container's systemd
  scope and is stopped with it before kubelet starts volume teardown.
- Raw disk images only in v1alpha1. Baryon can import a raw image from an
  HTTP(S) URL into a filesystem-mode claim it creates (`baryon-anchor
  import`, streamed, sparse, SHA-256 verified), or onto a block-mode claim it
  creates (`rootDisk.volumeMode: Block`), zeroing the rest of the device. The
  import runs in a separate import Pod before the first Anchor Pod exists
  (ADR 0004). A completed import is recorded in an annotation on the claim,
  because a raw device has no room for a marker; the annotation holds a
  digest of the URL, not the URL.
- Claims created by Baryon use `ReadWriteOnce` and are owned by the VM. One
  running VM per claim: baryon-controller creates no import or Anchor Pod
  for a VM while a Baryon pod of another VM in the namespace mounts the
  claim (`ClaimInUse`). `ClaimInUse` is a best-effort guard: reconciles of
  different VMs run concurrently, so two VMs started at the same time on
  one claim can both pass it; the backstops are `DiskInUse` and the VMM's
  file lock on one node, the attach/detach controller's multi-attach
  refusal for `ReadWriteOnce` claims across nodes, and `ReadWriteMany`
  block root disks being Not supported. baryon-virtd refuses a second VM
  on a node with the same block device (`DiskInUse`). ADR 0004 prevents two
  Anchor Pods per VM. The VMM's OFD lock excludes a second VMM on the same filesystem-mode
  disk file on a node, but not on a block device: each container has its
  own device node.

## Consequences

- No kubelet directory layout, no host bind mounts, and no CSI driver
  knowledge in baryon-virtd.
- The VMM can only open disks the Anchor uid could open. Disk images created
  by the importer are owned by that uid, mode 0600. kubelet does not apply
  `fsGroup` to raw block devices, so block devices are accessible only when
  the container runtime gives the device node the pod's `runAsUser` and
  `runAsGroup` (containerd: `device_ownership_from_security_context =
  true`, a node-wide setting, see ADR 0011). Verified on the k0s cluster;
  without the setting the VM fails with `BlockDeviceAccessDenied`.
- `/proc/self/fd/N` reopening relies on Cloud Hypervisor keeping inherited
  fds open for the life of the process (a guest reboot reopens disks). This
  is undocumented upstream behavior. The lifecycle E2E resets the guest with
  Landlock enforced on every run with host access (`NODE_SHELL` set), so a
  change upstream is caught when the pinned Cloud Hypervisor version is
  updated.
- Imported images are owned by the VM's host identity (ADR 0011), which is
  stable for the life of the VirtualMachine object. A new VirtualMachine
  over a retained or restored claim runs as a different identity and needs
  the storage to apply `fsGroup` ownership (not tested).
- Not tested: teardown under SIGKILL of the anchor, forced pod deletion,
  and baryon-virtd down, with a CSI driver that stages and attaches. The
  ordinary stop, move, and delete paths are Tested with Ceph RBD; the
  ordering argument above does not depend on the driver.
- Migration requires the disk to be reachable from the destination VMM.
  Concurrent access during migration needs a shared access mode and is future
  work.
- The importer runs in the import Pod, only while no import is recorded for
  the current URL. Kubernetes volume populators remain the natural
  long-term home for image import.
- The import record is trusted as written: anyone who can update claims in
  the namespace can set or remove it, and removing it makes the next Anchor
  Pod replacement import again, which overwrites a block device.

## Alternatives considered

- Host-visible kubelet paths (`<kubelet-root>/pods/<uid>/volumes/...`).
  Rejected: provider-specific roots, and a root process opening
  pod-controlled paths on the host must defend against symlink escapes.
- Bind-mounting the disk into a Baryon runtime directory. Rejected: an extra
  host mount to clean up, and it provides no ordering guarantee either.
- Passing the disk as an fd from the Anchor over SCM_RIGHTS. Equivalent
  safety, but requires fd passing on the runtime protocol and still needs the
  same reopen through `/proc/self/fd`. Kept as a fallback.
- vhost-user-blk backends. Rejected for v1alpha1: an extra process per disk
  for no current benefit.

## Revisions

- 2026-10-03: accepted for filesystem mode after the guest reset with
  Landlock enforced passed on the kind host and the Hadron node; the ownership
  consequence for re-created VMs was recorded.
- 2026-10-03: import onto block-mode claims that Baryon creates. The import
  record moved from a marker file alone to a claim annotation set by
  baryon-controller after the import init container succeeded. Known window:
  if the Anchor Pod is lost after a successful import and before
  baryon-controller records it (for example the controller is down and the
  node fails), the next Anchor Pod imports again and overwrites the device;
  filesystem-mode claims are still protected by the marker file.
- 2026-10-03: block mode Tested with OpenEBS LVM LocalPV and Ceph RBD on
  three Hadron nodes running k0s. The import moved from an init container
  of the Anchor Pod to a separate import Pod, and the import is recorded on
  the claim before the first Anchor Pod exists; this closes the Known window
  above for new imports (Anchor Pods of the previous release that still
  import in an init container are recognized until the legacy path is
  removed). The annotation holds `sha256:` and the hex SHA-256 of the URL
  instead of the URL. The lock statement was corrected: the OFD lock does
  not exclude a second VMM on a block device; one VM per claim is enforced
  by `ClaimInUse` in the controller and `DiskInUse` in baryon-virtd.
  baryon-virtd accepts only the two disk paths with their kinds and device
  nodes on the container's `/dev`. kubelet does not apply `fsGroup` to raw
  block devices; the containerd setting is required. Volume populators
  remain the long-term home for import.
- 2026-10-03: accepted for block-mode claims after the architecture gate.
  The untested teardown paths with a staging and attaching CSI driver moved
  from the status to a Consequences entry, with the reason the teardown
  ordering does not depend on the driver. `ClaimInUse` was described as a
  best-effort guard with its backstops.
