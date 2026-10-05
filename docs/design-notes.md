# Design notes: hard questions

Each question is answered with evidence or marked Open. "k0s cluster" means
three Baryon-enabled Hadron nodes running k0s (node1 to node3, nested KVM),
the integration environment; "Hadron node" means one Baryon-enabled Hadron
node running k3s; "kind host" means a historical development cluster in
which a kind node container acted as the host, no longer used. All are
described in [testing.md](testing.md#results), which summarizes the runs
behind every "Tested" below; the experiments are in
[runtime-boundary.md](runtime-boundary.md). An answer whose evidence names
only the kind host has not been repeated on a Hadron node.

## Runtime

| Question | Answer | Evidence |
|---|---|---|
| Who owns the Cloud Hypervisor process? | baryon-virtd starts and supervises it as a host process, as the VM's host identity (shared with the Anchor container), inside the Anchor container's cgroup, in its own empty network namespace. | ADR 0005; Experiment C; Hadron node |
| How is a running VM rediscovered after a baryon-virtd restart? | From `/run/baryon/vms/<pod-uid>/record.json`, verified against `/proc/<pid>/stat` start time and `/proc/<pid>/exe`, then re-adopted through a pidfd. The anchor reconnects and reattaches, and a new DHCP responder starts. baryon-virtd exits 2 s after SIGTERM (`SHUTDOWN_GRACE`), so a restart no longer waits for systemd's stop timeout. | Tested on the Hadron node and the kind host (same VMM pid, one new responder) |
| How is a re-adopted VM reported? | The record stores when boot evidence was seen (`guest_booted_at_unix_ms`). A VM re-adopted after a baryon-virtd restart is reported running with reason `Recovered` only if boot evidence was recorded; otherwise it is reported starting with reason `Booting`. | `crates/virtd/src/vm.rs`, `crates/virtd/src/record.rs` (Implemented) |
| Where does runtime state live? | `/run/baryon` (tmpfs): sockets, per-VM records (0600), and in each VM's directory a root-only `ctl/` (Cloud Hypervisor API socket, VMM output, DHCP responder log) and a VMM-owned `vmm/` on its own tmpfs (32 MiB, 256 inodes). baryon-virtd writes the console log to `/var/log/baryon/vms/` and copies the VMM and DHCP responder logs there when the VM stops (files 0600, directories 0700). | `crates/virtd/src/vm.rs`, `crates/virtd/src/vmm_files.rs` |
| Can log retention remove the logs of a running VM? | No. The image's tmpfiles.d entry ages `/var/log/baryon/vms` (14 days), and baryon-virtd holds a shared `flock` on the log directory of every running VM, which `systemd-tmpfiles` skips. | Tested on the Hadron node (manual procedure with a short override age) |
| Can a VMM affect the host through its runtime directory? | Its `vmm/` directory is a size- and inode-limited tmpfs, so it cannot fill `/run`. baryon-virtd reads and connects to files there only directory-relative, without following symlinks, requiring the VM uid as owner and a single link, and with bounded reads (64 KiB for status, 16 MiB per preserved log). | `crates/virtd/src/vmm_files.rs` (Implemented, unit tests) |
| How are stale processes detected? | A record whose pid is gone, has another start time, or runs another binary is discarded and its directory removed. | `crates/virtd/src/vm.rs`, `Manager::adopt` |
| What is the source of truth? | Kubernetes for desired state; the kernel for running processes; records are an index. | [architecture.md](architecture.md) |
| How is a cluster release paired with a host image? | Through the runtime protocol version `<major>.<minor>` and named features (ADR 0012). Any minor versions of the same major interoperate; another major is refused (`ProtocolIncompatible`) and the node's VM slots become unhealthy. The anchor negotiates before attaching; a 1.0 host answers UNIMPLEMENTED and is treated as 1.0. No VM requires a feature beyond 1.0 until feature-aware placement exists. | Tested: version skew E2E on the k0s cluster with mixed 1.0 and 1.1 hosts under both cluster releases (31 of 31 each); a 1.0 anchor reattached to a 1.1 baryon-virtd |

## Storage

| Question | Answer | Evidence |
|---|---|---|
| How does a CSI volume become accessible to a host VMM? | baryon-virtd resolves the path inside the Anchor mount namespace as an `O_PATH` fd; the VMM reopens it through `/proc/self/fd/N` as the VM's host identity. | ADR 0006; Experiment A |
| How do filesystem and block PVCs differ? | Filesystem: `disk.img` on the mounted volume, imported or pre-populated. Block: the device node `/dev/baryon/root` on the container's `/dev`, imported (written from the start of the device, the rest zeroed) or pre-populated. Same handoff. Block mode needs containerd `device_ownership_from_security_context = true`, because kubelet does not apply `fsGroup` to raw block devices. | Tested: storage E2E with OpenEBS LVM LocalPV and Ceph RBD in both modes on the k0s cluster |
| Who owns locks? | Cloud Hypervisor takes an OFD write lock at boot; it is released when the VMM exits. It excludes a second VMM on the same disk file on a node, but not on a block device: each container has its own device node, so the lock is on a different inode. | Cloud Hypervisor v53 `block/src/io/fcntl.rs`; [storage.md](storage.md#handoff-to-the-vmm) |
| How many VMs may use one claim? | One running VM. baryon-controller creates no import or Anchor Pod for a VM while a Baryon pod of another VM in the namespace mounts the claim (`ClaimInUse`); concurrent reconciles can race, so baryon-virtd also refuses a second VM on a node with the same block device (`DiskInUse`, kept across its restarts). `ReadWriteMany` block root disks across nodes are Not supported. | `ClaimInUse` Tested by the storage E2E on the k0s cluster; `DiskInUse` Implemented, unit tests |
| When is an imported disk written? | By a separate import Pod, before the VM's first Anchor Pod exists, and only after baryon-controller approved it. The completed import is recorded on the bound PersistentVolume and the import Pod is deleted; then the Anchor Pod is created. A filesystem claim that already has the marker but no record, on a fresh volume, gets an import Pod that leaves the disk untouched, and the record is written. | Tested: storage, deletion, and import integrity E2E on the k0s cluster; migration of a filesystem claim from the previous release observed on the k0s cluster before the record moved to the volume (the guest did not regenerate its SSH host keys) |
| Where does the import state live, and why there? | In the annotation `virtualization.baryon.io/disk-import` on the claim's bound PersistentVolume: `{"vmUID","claimUID","source"}`, with `claimUID` and `source` as the binding. The PersistentVolume has the lifetime of the data, is bound to one claim UID, and namespace users cannot write it, unlike the claim annotation used before, which they could remove to cause a second import over a used block device. | ADR 0006; `crates/controller/src/disk_import.rs` (unit tests); Tested by the import integrity E2E on the k0s cluster (a namespace user removes claim annotations without effect and cannot change the PersistentVolume) |
| Why not the VirtualMachine status alone, or a CRD? | Status belongs to the VirtualMachine, not to the data: a new VirtualMachine starts with empty status, and restores commonly drop status. It is kept as a second, one-way marker (`status.rootDiskImport`) that refuses a used block claim whose record is lost (`ImportStateUnknown`). A CRD or ConfigMap would be one more object to keep consistent with the volume's lifetime, and a ConfigMap in the VM namespace is writable by namespace users. | ADR 0006 (Alternatives) |
| When may Baryon import into a volume that has no record? | Only when the volume was dynamically provisioned for the claim: `pv.kubernetes.io/provisioned-by` set, `claimRef.uid` equal to the claim's UID, created no earlier than the claim, and no `dataSource` or `dataSourceRef` on the claim. Static, pre-bound, restored, and cloned volumes are refused (`DiskNotProvisionedForClaim`) and are used through `rootDisk.source.persistentVolumeClaim`. | ADR 0006; unit tests |
| How is an import approved, and why? | The importer waits until a downward API file of the import Pod's annotation `virtualization.baryon.io/import-approved` equals its claim UID. baryon-controller sets it only after checking the bound volume, which may not exist yet when the pod is created (`WaitForFirstConsumer`). Only baryon-controller may set it (`baryon-import-approval`), because namespace users can patch pods. | ADR 0006; unit tests; CI API server checks |
| What counts as a completed import? | The import Pod's immutable spec (the controller-only import resource, the mounted claim, the `--url=` and `--claim-uid=` arguments) and an importer exit code of 0, then the record on a volume mutually bound to the claim. StorageReady is True only once the record exists. Annotations, which namespace users can change, play no part. | `crates/controller/src/reconcile.rs` (unit tests); Tested by the import integrity E2E (`ImportPodConflict` and `AnchorPodConflict` for a namespace user's pods) |
| Why requeue instead of watching PersistentVolumes? | Import refusals wait for an administrator, and a cluster-wide watch of every PersistentVolume would cost memory for a state that changes rarely. A refused VM is looked at again every 120 s, and one in `DiskBindingPending` every 30 s. | `crates/controller/src/reconcile.rs` |
| What survives pod restart? | The claim and its content. A container restart restarts the VM; a pod replacement may move it. | ADR 0004 |
| What survives node reboot? | Claims (per CSI driver; local-path and LVM LocalPV keep the data on the node). Runtime records and VMMs do not. The controller replaces the Anchor Pod and the VM boots again from the same claim. | Tested on the Hadron node (local-path) and the k0s cluster (LVM LocalPV); see Recovery below |
| Does a guest reboot reopen the disk inside the same VMM? | Yes, through the same `/proc/self/fd/N` path, with Landlock enforced. Cloud Hypervisor needs write access to its own `vmm` directory to re-create the serial socket, so baryon-virtd adds that one Landlock rule; without it the reset failed with `Address in use`. | Tested on the Hadron node and the kind host (lifecycle E2E guest reset); Experiment A |
| What does deleting a VM remove? | The VirtualMachine, the Anchor Pod, the claim Baryon created for an `http` source, the runtime directory, the VMM and responder processes, the VMM tmpfs, and the pod volumes. Per-pod logs stay until log retention; user-owned claims stay. | Tested on the Hadron node and the kind host (lifecycle and deletion E2E); [testing.md](testing.md#what-deletion-removes) |

## Identity

| Question | Answer | Evidence |
|---|---|---|
| What identifies a VM on the host and to the guest? | On the host, each VM executes as a distinct unprivileged host identity, stable for the life of the VirtualMachine object and checked by baryon-virtd; its allocation is internal. To the guest, the MAC is derived from the VirtualMachine `metadata.uid`, the SMBIOS system UUID is that UID, and the NoCloud instance-id contains it. The cluster release and the host image must agree on all of these, so changing them is a runtime protocol change. | ADR 0011, ADR 0007, ADR 0009 |
| What happens when two VMs on one node have the same host identity? | baryon-virtd refuses the second (`IdentityConflict`). baryon-controller records the node in `status.identityConflictNodes`, replaces the Anchor Pod, and keeps it off the recorded nodes. The list is cleared when the VM is stopped. | ADR 0011; `crates/controller/src/reconcile.rs` (Implemented, unit tests; not exercised on a cluster) |
| What happens when a VirtualMachine is re-created over a retained or restored claim? | The new object has a new UID, so it gets a new host identity (which cannot open the old mode 0600 image unless the storage applies `fsGroup` ownership; not tested), a new MAC, and a new instance-id, so cloud-init runs its first boot again. | Inferred from ADR 0011 and ADR 0007; not tested |
| Can a VM keep its identity across re-create, restore, or a move to another cluster? | Not today. A durable VM identity independent of `metadata.uid` is an Open question. | Open |

## Networking

| Question | Answer | Evidence |
|---|---|---|
| Who creates TAP? | baryon-virtd, inside the pod network namespace. | ADR 0007 |
| Which namespace contains TAP? | The Anchor Pod network namespace. | Experiment B |
| How does Cloud Hypervisor access it? | Inherited fd (`--net fd=`). The VMM runs in its own empty network namespace (loopback only) and needs no capabilities; the TAP fd is its only network path. | Experiment B; Hadron node measurement in [security.md](security.md) |
| What does CNI teardown do? | Runs after the container stop, by which time the VMM and its TAP are gone. baryon-virtd removes its qdiscs when the pod still exists. | Experiment C timeline |
| How does VM identity remain stable on the network? | The MAC is derived from the VirtualMachine UID and set on the pod interface. The IP is the pod IP and changes with the Anchor Pod. | ADR 0007; Tested on the Hadron node (same MAC, new IP after node reboots) |
| How do peers learn the new MAC? | baryon-virtd broadcasts a gratuitous ARP for the pod IP after the MAC change. Without it the guest stayed unreachable from other pods after Ready until their neighbor entries expired. | Tested on the Hadron node and the kind host |
| Which guest network paths are tested? | From another pod to the pod IP, and through a ClusterIP Service by DNS name, including from one probe pod to VMs on three nodes (kube-router on the k0s cluster). NetworkPolicy and guest-initiated egress are Inferred, not tested. | [networking.md](networking.md) |
| Must guest DHCP survive a baryon-virtd restart? | The responder does not need to survive it, and does not: it exits with baryon-virtd and is not re-adopted. The new baryon-virtd starts a new responder for each re-adopted VM when its anchor reattaches, and restores the time of the last lease acknowledgement from the runtime record. A running guest keeps its 24 h lease across the restart. A guest that reboots, resets, or renews before the anchor has reattached gets no answer until then. Depending on when the node agent reports, `NetworkReady` can be Unknown during the gap (`RuntimeUnreachable` or `NotReported`). | `crates/virtd/src/vm.rs` (`on_attach`, `adopt`); ADR 0007 revision of 2026-10-04; Tested: `tests/e2e/network-health.sh` case virtd (a new responder, `NetworkReady` True, same Anchor Pod) on the k0s cluster with ceph-rbd and lvm-local, [testing.md](testing.md#k0s-cluster) |
| How does the cluster learn that a VM's address service failed? | baryon-virtd sees a responder exit through its pidfd and a hang through liveness probes: every 10 s it writes a probe to the responder's stdin, the DHCP serving loop answers on stdout, and a responder that answers no probe for 25 s is killed and restarted (ADR 0007 revision of 2026-10-04). A probe answer shows that the serving loop runs, not that the guest can get or renew a lease or that the pod network passes traffic. baryon-virtd reports the DHCP responder state in `VmStatus` (runtime protocol 1.2); baryon-node-agent publishes it per Anchor Pod UID in its VirtualizationNode (`status.vms`); baryon-controller, still the only writer of VM status, derives `NetworkReady` from it while the node's heartbeat is fresh (seen changing within 90 s on the controller's clock). The anchor does not report it: it has no API access, and its one readiness probe carries guest readiness. `Ready` is unchanged by it. | ADR 0008 revision of 2026-10-04; unit tests in `crates/controller/src/status.rs` and `nodes.rs` and `crates/node-agent/src/publisher.rs`; Tested: `tests/e2e/network-health.sh` on the k0s cluster with ceph-rbd and lvm-local, including `NetworkReady` Unknown (`RuntimeUnreachable`) while the node agent of the VM's node is gone for longer than 90 s. Unit tests: probe answers and protocol violations (`crates/virtd/src/dhcp_helper.rs`), probe answers from the serving loop (`crates/virtd/src/handoff/dhcp.rs`), the cause in the status message (`crates/virtd/src/vm.rs`); the supervisor's probe timeout and answer acceptance have no unit test. Tested on the k0s cluster: `network-health.sh` cases hang, hang-repeated, and before-lease (a hung responder) with ceph-rbd and lvm-local, and `tests/e2e/node-agent-outage.sh` cases down-then-restart and restart-then-down (a controller restart while a node agent is down; `NetworkReady` never read True from the stale report). [testing.md](testing.md#k0s-cluster) |

## Scheduling

| Question | Answer | Evidence |
|---|---|---|
| Does the Anchor Pod completely determine VM placement? | Yes, the Anchor Pod and, for an imported disk, the import Pod before it. The VM's `spec.placement` (`nodeSelector`, `affinity`, `tolerations`, `topologySpreadConstraints`, `priorityClassName`, Kubernetes types) is copied to both pods; the Kubernetes scheduler places them. Baryon adds only its identity-conflict term, ANDed into every user node selector term. | ADR 0004, ADR 0008; unit tests in `crates/controller/src/anchor.rs` (placement on both pods, term ANDing); Tested by the placement E2E on the k0s cluster (selector, affinity with two eligible nodes, spread, tolerations, cordon) |
| Why does the import Pod carry the VM's placement and labels? | With storage that binds on first use (`WaitForFirstConsumer`, for example LVM LocalPV), the import Pod's node decides the volume's node and so the VM's node. Without the VM's labels the scheduler would not count it in the VM's spread constraints and anti-affinity. Its readiness probe never passes, so Services never route to it. | Tested by the placement E2E on the k0s cluster with lvm-local (two VMs created together with a spread constraint landed on two nodes) |
| Why is there no `NoCompatibleNode` reason? | Baryon computes no node eligibility beside the scheduler; a second computation would drift from the scheduler's (taints, capacity, volume topology, preemption). A pod that cannot be placed reports the scheduler's reason (`Unschedulable`) and message, cut to 1024 bytes, on `Scheduled` and `Ready`; a pod the API server refuses at creation reports `PodRejected` with the API server's message, and the VM is looked at again every 60 s, since a quota or a later-created PriorityClass can make the pod acceptable. | `crates/controller/src/status.rs`, `crates/controller/src/reconcile.rs` (unit test for the cap); Tested by the placement E2E (the scheduler message names the taint; an unknown PriorityClass gives `PodRejected`) |
| How does runtime health gate placement? | Through the device plugin resources: a node whose baryon-virtd is down or protocol-incompatible has zero `virtualization.baryon.io/vm` and `virtualization.baryon.io/import` slots. A VM whose placement allows only such nodes stays `Unschedulable` (scheduler message such as `Insufficient virtualization.baryon.io/import`); a VM already running there keeps its Anchor Pod. | Tested by the placement E2E on the k0s cluster (baryon-virtd stopped on the only eligible node) |
| What happens when placement changes on a running VM? | Nothing moves. The Anchor Pod's `virtualization.baryon.io/placement-hash` no longer matches, and `RestartRequired` is True with reason `PlacementChanged`; the change applies at the next start. A VM on a node-local volume stays on that volume's node. | `crates/controller/src/reconcile.rs`; Tested by the placement E2E |
| How do Services and policies select VMs? | By the VM's own labels, copied to its pods except reserved keys and kept in sync on the running Anchor Pod without a restart; a sync removes only the keys Baryon recorded in the user-writable `vm-label-keys` annotation, and never a reserved key. The import Pod gets the labels at creation only; a NetworkPolicy or PodDisruptionBudget over VM labels also covers it. | ADR 0007; Tested by the placement E2E (copied, changed without a restart, removed, another actor's label kept, reserved key not copied); unit test for the reserved-key filter on recorded keys |
| How do administrators bound what VM placement may request? | With Kubernetes namespace controls, because the VM's pods are created in the VM's namespace: PodNodeSelector, PodTolerationRestriction, and ResourceQuota scopes for PriorityClass and cross-namespace pod affinity. CEL rules refuse tolerations without a key or of cordon, control-plane, and out-of-service taints, `system-` PriorityClasses, and cross-namespace pod affinity terms. | CEL rules Tested by the CI API server checks; the namespace controls are Kubernetes behavior, not tested with Baryon |
| How are virtualization capabilities expressed? | The `virtualization.baryon.io/vm` extended resource marks usable nodes (KVM and runtime health); the placement-only `virtualization.baryon.io/import` resource places import Pods on the same nodes. VirtualizationNode reports only whether the node can run Baryon VMs, its runtime versions, and its runtime health; it publishes no host inventory (the Node object has what the scheduler uses, and baryon-node-agent logs the rest). Capability requirements (IOMMU, IOMMUFD, hugepages, NUMA, VFIO, Cloud Hypervisor features) are deferred: Baryon consumes none of them today, and a capability gets a name only when Baryon consumes it and it is tested. The mechanism then is a per-capability placement-only extended resource from the node agent, not Node labels written by a Baryon component (writer identity, staleness, RBAC). Where the requirement goes in the API is decided by the ADR that ships the capability. Consumable devices (VFIO, GPUs) go to DRA, where device requests would be per-VM claims. | ADR 0004, ADR 0008 revision of 2026-10-05, ADR 0009, ADR 0012 |
| How will NUMA requirements affect scheduling? | Through the Anchor container's resources: the CPU and memory managers assign cpusets to the container, and the VMM inherits them because it shares the container cgroup. NUMA stays with the node's Topology Manager; the API has no NUMA field. Not tested with the static policy. | Open (ADR 0012 records that there is no NUMA field) |
| How will device allocation integrate with DRA? | Designed direction: ResourceClaims on the Anchor Pod, with baryon-node-agent as the DRA driver and baryon-virtd handing devices to the VMM. | Planned |

## Security

| Question | Answer | Evidence |
|---|---|---|
| Which privileges are unavoidable? | baryon-virtd as root on the host (namespace entry, TAP creation, cgroup placement, spawning as another uid, mounting the VMM tmpfs). | [security.md](security.md) |
| Who can reach the anchor socket and start a VM? | Only containers allocated the `virtualization.baryon.io/vm` resource receive the socket mount. With `baryon-vm-resource` installed, only baryon-controller can create pods that request it; baryon-virtd then requires the anchor's uid and gid to equal the VM's host identity (`PermissionDenied` otherwise), so a pod can only drive the VM it was created for. The policy has to be installed; without it any pod could request the resource and start a VM of its own, running as the identity of the VM UID it names. | `deploy/base/policy/vm-resource.yaml`; Tested by the CI API server checks, on the Hadron node and the kind host |
| Which privileges exist only because of the prototype? | baryon-virtd keeps the full root capability set; reducing it is Designed. The anchor check proves pod membership and the VM's host identity, not that the pod was allocated the vm resource; a check through the kubelet pod resources API is Designed. Guest DHCP is no longer parsed in the root process: a per-VM responder with the VMM's confinement parses it (Tested on the Hadron node and the kind host). | [security.md](security.md) |
| Can privileged operations be isolated in baryon-virtd? | Yes: the controller, node agent, and anchor hold no host privilege beyond their sockets. The node agent runs as uid 0 without capabilities; the node socket is 0600 in a 0700 directory and requires peer uid 0. | ADR 0005, ADR 0009 |
| Can the controller remain unprivileged? | Yes. It acts only on Anchor Pods it controls (owner reference with the VM UID) and deletes pods with a UID precondition. Its PersistentVolume and pod `patch` rights are narrowed by `baryon-controller-writes` to adding the import record once and, on Baryon pods, setting the import approval, the `vm-label-keys` annotation, and labels with keys that are not reserved; PersistentVolume `get` is cluster-wide. | `deploy/base/controller/`, `deploy/base/policy/controller-writes.yaml`, `crates/controller/src/reconcile.rs`; CI API server checks |
| Can the launcher avoid full privilege? | There is no launcher; baryon-anchor runs with no capabilities as a non-root uid. | ADR 0005 |
| Per-VM host identities or Kubernetes user namespaces? | A distinct host identity per VM is the contract (ADR 0011). Kubernetes user namespaces may be evaluated later as additional hardening for the Anchor container. | ADR 0011 |

## Recovery

| Question | Answer | Evidence |
|---|---|---|
| What happens after node reboot? | VMMs and records are gone. Without kubelet graceful node shutdown, the guest is stopped hard at the reboot; after boot, kubelet re-admits the existing Anchor Pod before the device plugin has registered, admission fails (`UnexpectedAdmissionError`, no healthy devices for `virtualization.baryon.io/vm`), and the pod fails. With graceful node shutdown, the guest shuts down cleanly and the old pod ends `Succeeded`. In both cases the controller replaces the finished pod with a new Anchor Pod (new pod UID and IP, same MAC), which stays Pending until the device plugin registers. | Tested on the Hadron node and the k0s cluster (manual procedure; on the k0s cluster with graceful shutdown the VM came back without intervention once the node was Ready) |
| What does a guest reset do? | The VMM keeps running and re-creates the VM's devices; the guest boots again and gets a new lease. A hard reset shortly after the first boot can lose first-boot writes: the Debian guest lost its SSH host keys. | Tested on the Hadron node and the kind host |
| Should VMs automatically restart? | Yes while `runPolicy: Running`: a VMM exit ends the anchor container and kubelet restarts it with back-off. A guest-initiated power-off also restarts the VM. | Experiment C (VMM SIGKILL); Tested on the Hadron node (VMM crash) |
| How is an orphaned VM process reconciled? | A VMM whose anchor does not return within the reattach or recovery grace, or whose pod has no other process left, is stopped or killed. With the systemd cgroup driver, stopping the container scope also stops it, even while baryon-virtd is down; that is why the cgroupfs driver is refused. | Experiment C; Tested on the Hadron node (deletion with baryon-virtd stopped) |
| How is an orphaned Anchor Pod reconciled? | The Anchor Pod is owned by the VirtualMachine and garbage collected; the controller deletes it when the VM is stopped or deleted. A pod named `<vm>-anchor` that the VM does not control is left alone and reported as `AnchorPodConflict`. | lifecycle and deletion E2E; `crates/controller/src/reconcile.rs` |
| Is the systemd cgroup driver requirement permanent? | It is required today (cgroupfs is Not supported). Whether a different stop mechanism could lift it is Open. | Experiment C; [compatibility.md](compatibility.md) |
| How fast does the controller recover after an API server restart? | kube-runtime backs off all of a controller's watches together after an error in any of them, and an API server restart makes every watch fail once with 410 Expired. With the default backoff (800 ms to 30 s, reset after 120 s) no VM was reconciled for minutes. baryon-controller uses a backoff of 0.5 s doubling to 5 s; after a control-plane node reboot it reconciled shortly after the in-cluster API path returned, which itself lagged behind the API server (cluster networking). | `crates/controller/src/main.rs` (`TriggerBackoff`, unit test); observed on the k0s cluster |

## Kairos lifecycle

| Question | Answer | Evidence |
|---|---|---|
| How is the Baryon-enabled Hadron artifact built? | Hadron base image, plus the Baryon host layer, then kairos-init with the chosen Kubernetes provider; AuroraBoot for bootable media. | [hadron.md](hadron.md) |
| How does Kairos Operator deploy an updated artifact? | `NodeOpUpgrade` with the new image. With v0.2.2 on one node: the drain deleted the Anchor Pods, the guests shut down cleanly before the reboot, a VM on Ceph RBD restarted on another node, and a VM on an LVM local volume stayed Pending until its node returned and was then Ready. | Tested on the k0s cluster for one node and for a rolling upgrade of all three nodes; with v0.2.2, concurrency 1 did not serialize the node outages (reboot completion is counted before the node is back). [kairos-integration.md](kairos-integration.md#cooperation-with-upgrades) |
| Does a Kairos A/B upgrade work on a Baryon node? | Yes for a manual `kairos-agent upgrade --source ocifile:...` and reboot with a VM running: the node booted the new image each time, and the VM was Ready again on a replacement Anchor Pod; on the k0s cluster a VM survived the upgrade of the control-plane node. The recovery system is upgraded separately (`--recovery`); not done. | Tested on the Hadron node (six times) and on all three nodes of the k0s cluster, manual procedure in [kairos-integration.md](kairos-integration.md) |
| Does a `NodeOpUpgrade` reboot wait for the kubelet shutdown inhibitor? | Yes. The operator reboots with `nsenter -i -m -t 1 -- reboot`; on Hadron `reboot` is systemctl, which honors kubelet's logind delay inhibitor. With a VM running and no drain, the guest powered off cleanly and the VM came back. | Tested on the k0s cluster (Kairos Operator v0.2.2) |
| How does Baryon signal that a node can be upgraded? | `status.vmCount` on the VirtualizationNode answers "no VMs here"; an upgrade runbook waits for it to be 0. After the reboot, the runbook waits until the Node's `status.nodeInfo.bootID` changed and then until the VirtualizationNode's `heartbeatTime` changed with `Ready` True: the node agent of the previous boot cannot write after the reboot, so a `Ready=True` written before it cannot satisfy the wait, and no clocks are compared. `NodeAgentStopped`, set by a stopping node agent, is best effort and only a signal for people. An upgrade-safe label or evacuation conditions are not planned until migration exists; a label would need its own ADR. | [kairos-integration.md](kairos-integration.md#cooperation-with-upgrades) |
| What Baryon state must survive a Kairos A/B upgrade? | None in the OS image. Disks live in claims; logs in `/var/log`. | ADR 0003 |
| What host and cluster version skew is supported during rolling upgrades? | Any minor versions of the same protocol major version, in either upgrade order; Anchor Pods of the previous release keep running under a new cluster release (`RestartRequired=True`, `AnchorImageChanged`). The minimum host image per cluster release is in [compatibility.md](compatibility.md#version-skew). | ADR 0012; Tested by the version skew E2E on the k0s cluster |

## Cluster API

| Question | Answer | Evidence |
|---|---|---|
| What does Baryon assume about how the host cluster was created? | Nothing beyond [compatibility.md](compatibility.md). | |
| Can a CAPI-created Hadron cluster run Baryon without special integration? | It is an ordinary cluster; its image must be the Baryon-enabled image. For k0s, the nodes need the containerd drop-in and the worker profile in [compatibility.md](compatibility.md#k0s-node-configuration), because k0s v1.36.4 does not use the systemd cgroup driver by default. Not tested through Cluster API. | research of cluster-api-provider-kairos v0.1.3; k0s cluster |
| What future machine capability metadata would be useful? | A Machine label marking Baryon-enabled images, and pre-drain hooks for evacuation. | Planned |

## Migration constraints recorded so far

Live migration is Not supported; the evaluation, its entry criteria, and
the experiment are in [ADR 0013](adr/0013-live-migration.md).

- Cloud Hypervisor does not accept network fds on `vm.receive-migration`
  (cloud-hypervisor#7054, #8566); #8778 proposes one `external_fds` list
  on restore and receive-migration for network, VFIO, and IOMMU fds. Disks
  and host paths on the destination are not covered by any of them. Main
  substitutes fds on receive for VFIO devices only.
- Cloud Hypervisor releases the source's disk locks before sending device
  state and does not take them back if the migration fails; its locks are
  host-local.
- A lost final acknowledgement resumes the source while the destination
  already runs the guest: in v53 for pre-copy and post-copy, on main for
  pre-copy. Only an authority outside both VMMs can decide which copy
  runs.
- The runtime directory, serial socket, and Landlock writable directory are
  keyed by the Anchor Pod UID; a target Anchor Pod has a new UID, so the
  configuration in the migration stream names paths that do not exist on
  the target.
- A new Anchor Pod gets a new pod IP; pod-network migration needs CNI support
  for keeping IP and MAC.
- Snapshot and migration compatibility across Cloud Hypervisor majors is
  limited (n-2 from v54); Kairos A/B upgrades change the version per node.
- The SMBIOS UUID (VirtualMachine UID) and the derived MAC are already stable
  across hosts.
- The fixed Anchor Pod name (`<vm>-anchor`) and the at-most-one-VMM rule
  (ADR 0004) leave no room for a second pod for the same VM during a
  migration. Whether migration needs a different pod naming is Open.
- The VMM's network namespace is empty, so it has no TCP transport for
  migration; a Unix socket brokered by baryon-virtd would be needed, which
  would make the root runtime a network-facing relay (ADR 0013).
- baryon-virtd refuses a second VM with the same host identity on a node
  (`IdentityConflict`), which also refuses a same-node migration target.
- The VM's labels are copied to its Anchor Pod, so during a migration two
  pods would carry them. The recorded rule (ADR 0007): only the active pod
  carries the VM's labels, and they move at switchover.

## Open hard questions

- Is the systemd cgroup driver requirement permanent?
- A durable VM identity across re-create, restore, and move.
- Migration against the fixed Anchor Pod name and the at-most-one-VMM rule
  (Not supported; the open questions are listed in
  [ADR 0013](adr/0013-live-migration.md#hard-questions-left-open)).
- Exclusion between a VM's import Pod and its Anchor Pod with more than one
  controller replica (it relies on the single replica today).
- An explicit flow for growing a root disk (`rootDisk` is immutable).
