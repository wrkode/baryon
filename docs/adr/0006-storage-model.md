# 0006: PVC disks are resolved in the Anchor mount namespace

Status: Accepted for filesystem-mode claims: Tested with local-path on a
single Baryon-enabled Hadron node (k3s) and on a historical development
cluster in which a kind node container acted as the host, including the disk reopen after a guest reset
with Landlock enforced (see [testing.md](../testing.md)). Block mode is
Implemented, not tested, and CSI drivers with staging and attach are not
tested; both remain open questions of this ADR.
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
  `openat2(RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS)` as `O_PATH`. It checks
  that the result is a regular file or a block device.
- The `O_PATH` fd is inherited by Cloud Hypervisor, which is given
  `path=/proc/self/fd/N,image_type=raw`. Cloud Hypervisor reopens the file
  with its own credentials (the Anchor uid), so ordinary permission checks
  apply as if the pod opened the disk.
- Teardown safety comes from ordering, not from the open file: the VMM runs
  in the Anchor container's cgroup and is stopped by the container runtime
  before network and volume teardown (ADR 0005).
- Raw disk images only in v1alpha1. Baryon can import a raw image from an
  HTTP(S) URL into a filesystem-mode claim it creates, with an init container
  (`baryon-anchor import`, streamed, sparse, SHA-256 verified), or onto a
  block-mode claim it creates (`rootDisk.volumeMode: Block`), zeroing the
  rest of the device. A completed import is recorded in an annotation on the
  claim, because a raw device has no room for a marker.
- Claims created by Baryon use `ReadWriteOnce` and are owned by the VM. The
  OFD lock prevents two VMMs on one node from opening the same disk; ADR 0004
  prevents two Anchor Pods per VM.

## Consequences

- No kubelet directory layout, no host bind mounts, and no CSI driver
  knowledge in baryon-virtd.
- The VMM can only open disks the Anchor uid could open. Disk images created
  by the importer are owned by that uid, mode 0600. Block devices must be
  accessible to it (for example through `fsGroup` or containerd's
  `device_ownership_from_security_context`); to be verified.
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
- Migration requires the disk to be reachable from the destination VMM.
  Concurrent access during migration needs a shared access mode and is future
  work.
- The importer lives in the Anchor Pod and runs on every start that finds no
  import marker. Kubernetes volume populators are the natural long-term home
  for image import.

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
