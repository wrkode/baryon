# 0006: PVC disks are resolved in the Anchor mount namespace

Status: Accepted for filesystem-mode and block-mode claims. Filesystem mode
is Tested with local-path on a single Baryon-enabled Hadron node (k3s) and
on a historical development cluster in which a kind node container acted as
the host, including the disk reopen after a guest reset with Landlock
enforced, and with OpenEBS LVM LocalPV and Ceph RBD on three Hadron nodes
running k0s. Block mode is Tested with OpenEBS LVM LocalPV and Ceph RBD on
the k0s cluster, including a move to another node through CSI detach,
attach, and NodeStage (see [testing.md](../testing.md#results)). The
import record on the PersistentVolume and the import approval are Tested by
the import integrity E2E with OpenEBS LVM LocalPV and Ceph RBD block volumes
on the k0s cluster.
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

An image import overwrites the disk, so it must run exactly once, before
any guest uses the disk. A raw block device has no room for a marker, and
namespace users can edit claim metadata and patch pods in their namespace,
so neither can hold trusted import state.

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
  creates (`rootDisk.volumeMode: Block`). The import runs in a separate
  import Pod before the first Anchor Pod exists (ADR 0004).
- Block import contract: after a successful import every byte of the device
  is the image's byte at that offset or zero. The importer zeroes every
  zero run of the image and everything after it explicitly, and zeroes the
  first and last 1 MiB of the device before the download; image data in
  those two regions is written only after the size and SHA-256 checks pass.
  Details are in [storage.md](../storage.md#block-import).
- Import record: a completed import is recorded in the annotation
  `virtualization.baryon.io/disk-import` on the claim's bound
  PersistentVolume, as JSON
  `{"vmUID":"<vm-uid>","claimUID":"<claim-uid>","source":"sha256:<hex>"}`,
  where `source` is `sha256:` and the hex SHA-256 of the source URL. The
  PersistentVolume has the lifetime of the data, is bound to one claim UID,
  and namespace users cannot write PersistentVolumes. `claimUID` and
  `source` are the binding. `vmUID` is recorded for auditing and is not
  compared: the data belongs to the claim, which can outlive the
  VirtualMachine (for example after a deletion with orphan propagation).
  baryon-controller writes the record only when the claim and the volume
  are bound to each other: the claim is Bound to the volume, and the volume
  is Bound with a claimRef that names the claim's namespace, name, and UID.
  The claim annotation `virtualization.baryon.io/imported-from` of earlier
  releases is no longer written; it is read only to migrate VirtualMachines
  of earlier releases. There is no CRD, no ConfigMap, and no
  PersistentVolume watch for import state.
- Import decision (`decide` in `crates/controller/src/disk_import.rs`), for
  a VirtualMachine with an `http` source:
  - a record for this claim UID and source: imported; the disk is used as
    is.
  - a record for another claim UID: refused, `DiskInitializedForAnotherClaim`.
  - a record for another source: refused, `DiskSourceMismatch`.
  - an unreadable record: refused, `DiskImportRecordInvalid`.
  - no record, and the VirtualMachine is of an earlier release: the claim
    annotation equals the source digest or the URL, and the status shows
    StorageReady=True for the same claim name with a transition no earlier
    than the claim's creation. baryon-controller writes the
    PersistentVolume record (`DiskBindingPending` while the claim and the
    volume are not yet bound to each other) and treats the disk as
    imported.
  - no record, and a block-mode claim is known to have been used
    (`status.rootDiskImport` names this claim UID, or the readiness of an
    earlier release above): refused, `ImportStateUnknown`. A
    filesystem-mode claim is still protected by the importer's marker file.
  - no record otherwise: import only into a fresh volume. The volume carries
    `pv.kubernetes.io/provisioned-by`, its `claimRef.uid` is the claim's
    UID, it was created no earlier than the claim, and the claim has no
    `dataSource` or `dataSourceRef`. Anything else (a static or pre-bound
    volume, a restored or cloned claim) is refused,
    `DiskNotProvisionedForClaim`.

  A refusal never imports. Its message points to the adoption path: to use
  existing data, reference the claim with
  `rootDisk.source.persistentVolumeClaim` in a new VirtualMachine.
  Refusals are reported on `StorageReady` and `Ready`.
- Import approval: the import Pod mounts a downward API file of its own
  annotation `virtualization.baryon.io/import-approved`, and the importer
  (`--approval-file`, `--claim-uid`) writes nothing until that file equals
  its claim UID. baryon-controller creates the import Pod already approved
  when the claim is bound to a fresh volume. Otherwise (a claim that binds
  when the import Pod is scheduled, `WaitForFirstConsumer`) it sets the
  approval once the claim is bound and the volume passes the freshness
  check, and it deletes the import Pod when the decision refuses. The
  importer fails after 30 minutes without an approval and is restarted
  (`restartPolicy: OnFailure`). The admission policy
  `baryon-import-approval` lets nobody but baryon-controller set, change, or
  remove the annotation on any pod.
- Completion evidence: only the import Pod's spec, which is immutable,
  counts. The pod requests the controller-only
  `virtualization.baryon.io/import` resource, mounts the claim, the
  importer's arguments carry `--url=<url>` and `--claim-uid=<claim-uid>`,
  and the importer exited 0. Annotations play no part. A pod named like the
  import Pod that does not request the import resource or is not owned by
  the VirtualMachine is never used (`ImportPodConflict`); an import Pod for
  another claim UID is deleted. A disk with an `http` source is reported
  StorageReady=True only once its record exists; an importer that exited 0
  without a record shows `ImportingDisk` ("recording the import").
- `status.rootDiskImport.claimUID` is a one-way marker that the claim held
  an imported or used disk. It only prevents another import into that
  claim. It is kept while the claim UID is unchanged, also when the disk is
  refused, and reset for a recreated claim. The PersistentVolume record is
  the authority.
- baryon-controller's writes to existing PersistentVolumes and pods are
  limited by the admission policy `baryon-controller-writes`: a
  PersistentVolume may only gain the record annotation when it has none,
  with nothing else changing, and only while it is Bound and the record's
  `claimUID` equals its `claimRef.uid`; a pod update may only change the
  approval annotation, and only on pods labeled
  `app.kubernetes.io/managed-by=baryon`. Both policies are part of the
  supported installation.
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
- The importer runs in the import Pod only when the decision above allows
  an import. Kubernetes volume populators remain the natural long-term home
  for image import.
- Namespace users cannot cause an import through claim metadata or pod
  annotations, and the tenant roles do not grant VirtualMachine status.
  The claim annotation of earlier releases counts only together with a
  StorageReady=True written by baryon-controller, and then only to record
  an import, never to start one. Removing or rewriting the PV record needs
  PersistentVolume write access, which cluster administrators have.
- Accepted residual (cluster administrators are trusted): if both the VM
  status and the PersistentVolume record are lost, for example after a
  restore without status and an administrator removing the record, a
  volume that still looks fresh can be imported again.
- Storage tooling must preserve PersistentVolume annotations, and dynamic
  provisioners must set `pv.kubernetes.io/provisioned-by`. Statically
  provisioned or pre-bound volumes, restored snapshots, and clones cannot
  back an `http` source; they are used through
  `rootDisk.source.persistentVolumeClaim`.
- The controller does not watch PersistentVolumes. A refused VirtualMachine
  is looked at again every 120 s, and one in `DiskBindingPending` every
  30 s, so a corrected volume is picked up without a watch.
- RBAC grants baryon-controller `get` on all PersistentVolumes, which
  exposes PersistentVolume specs to a compromised controller, and `patch`,
  which is safe only with `baryon-controller-writes` installed.
- Every block import zeroes or writes the whole device. This is fast where
  the backend supports write-zeroes and proportional to the device size
  otherwise. dm-thin has no write-zeroes support, so an import allocates
  the whole volume on a thin pool. A thin pool must have room for the full
  size of every volume imported into it and must zero newly provisioned
  blocks (Documented residual, [security.md](../security.md#data-handling)).

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
- Import record on the claim (the decision until 2026-10-03). Rejected:
  namespace users can edit claim metadata, so they could remove the record
  and cause a second import over a used block device.
- Import record in VirtualMachine status alone. Rejected: status belongs to
  the VirtualMachine, not to the data; a new VirtualMachine starts with
  empty status, and backup tools commonly restore objects without status.
  Status is kept as a second, one-way marker (`status.rootDiskImport`).
- A CRD or ConfigMap holding import records. Rejected: one more object to
  keep consistent with the volume's lifetime and to garbage collect, and a
  ConfigMap in the VM namespace is writable by namespace users. The
  PersistentVolume already has the data's lifetime and is not writable by
  them.
- A PersistentVolume watch. Rejected for now: a cluster-wide watch of every
  PersistentVolume for a state that changes rarely; refused VMs are
  requeued instead.

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
- 2026-10-03: the authoritative import record moved from the claim
  annotation `virtualization.baryon.io/imported-from` to the annotation
  `virtualization.baryon.io/disk-import` on the bound PersistentVolume,
  written only when claim and volume are bound to each other. Without a
  record, an import runs only into a volume dynamically provisioned for the
  claim; other cases are refused with a reason and the adoption hint. The
  importer waits for an approval that only baryon-controller can set
  (`baryon-import-approval`), and completion is judged only from the import
  Pod's spec and its importer's exit code; the legacy init container
  evidence path was removed. `status.rootDiskImport` was added as a one-way
  marker. baryon-controller lost claim `patch` and gained PersistentVolume
  `get` and `patch` and pod `patch`, narrowed by `baryon-controller-writes`.
  The block import contract (every byte is the image's or zero, both ends
  zeroed before the download) and the thin storage residual were recorded.
  The earlier Consequence that the import record is trusted as written no
  longer applies.
