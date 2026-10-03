# Storage

Status: filesystem-mode claims Tested with local-path on a single
Baryon-enabled Hadron node running k3s and on a historical kind-based
development cluster (lifecycle E2E, summarized in
[testing.md](testing.md#results); Experiment A in
[runtime-boundary.md](runtime-boundary.md)). Block-mode claims Implemented,
not tested. Other CSI drivers are not tested. Decision: ADR 0006 (Accepted
for filesystem mode).

Baryon has no storage system. The VM root disk is a PersistentVolumeClaim,
provisioned and attached by the cluster's CSI driver to the VM's Anchor Pod.

## Disk sources

| Source | Claim | Content |
|---|---|---|
| `rootDisk.source.http` | created by Baryon, named `<vm>-root`, owned by the VM, `ReadWriteOnce`, filesystem mode | `baryon-anchor import` streams the raw image into `disk.img`, skipping zero blocks, verifies SHA-256 when given (required for plain HTTP), and extends the file to `rootDisk.size`. A redirect from HTTPS to plain HTTP is refused. A marker file prevents a second import. |
| `rootDisk.source.persistentVolumeClaim` | existing, not owned by the VM | filesystem mode: `disk.img` at the volume root; block mode: the whole device |

Images must be raw. qcow2 and other formats are Not supported.

## Handoff to the VMM

| Step | Process | Mechanism |
|---|---|---|
| Attach and mount | CSI, kubelet | into the Anchor Pod: `/var/lib/baryon/disks/root` (filesystem) or `/dev/baryon/root` (block) |
| Resolve the disk | baryon-virtd | `openat2` on the anchor's `/proc/<pid>/root` with `RESOLVE_IN_ROOT` and `RESOLVE_NO_MAGICLINKS`, as `O_PATH` |
| Open the disk | Cloud Hypervisor, as the VM's host identity | `path=/proc/self/fd/N`, reopened with normal permission checks |
| Lock | Cloud Hypervisor | `F_OFD_SETLK`, exclusive for read-write disks |
| Reopen after a guest reset | Cloud Hypervisor | the same `/proc/self/fd/N` path inside the running VMM, with Landlock enforced (Tested on the Hadron node and the kind host) |
| Stop using the disk | container runtime and baryon-virtd | the VMM runs in the Anchor container's cgroup and is stopped with the container, before volume teardown |
| Unmount, detach | kubelet, CSI | standard |

baryon-virtd never opens pod-controlled paths through host directories, and
the VMM can only open what the VM's host identity, which the Anchor
container shares, can open.

## What survives what

| Event | Disk content | Notes |
|---|---|---|
| VM restart, stop and start | kept | the claim is independent of the Anchor Pod |
| VirtualMachine deletion | deleted for imported disks (owner reference); kept for referenced claims | |
| Node reboot | kept (depends on the CSI driver) | local-path keeps data on the node, so the VM is bound to it |
| Kairos A/B upgrade | kept | claims live outside the OS image |

## Limits and open items

- One disk per VM. Additional disks, read-only disks, and CD-ROM images are
  Not supported in the API (the runtime protocol accepts up to eight).
- Live migration and snapshots are Not supported.
- Block-mode device access for the VM's host identity is not verified. Raw
  block volumes are devices: access is governed by the device permissions
  the container runtime sets for the pod, not by file ownership (ADR 0011).
- The claim layout, `disk.img` at the volume root plus the import marker
  `.baryon-import.json`, is a stable format of v1alpha1 (see
  [api.md](api.md)).
- Imported images are mode 0600, owned by the VM's host identity, which is
  stable for the life of the VirtualMachine object (ADR 0011). The pod's
  `fsGroup` is the VM's gid. A VirtualMachine re-created under the same name
  is a new object with a new identity, so it cannot open an image from a
  retained or restored claim unless the storage applies `fsGroup`
  ownership; not tested. See [design-notes.md](design-notes.md).
- Teardown ordering under a CSI driver with NodeStage and attach is not
  tested yet; local-path has neither.
- The import runs inside the Anchor Pod. Kubernetes volume populators are the
  intended long-term mechanism for image import.
