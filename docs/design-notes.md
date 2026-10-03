# Design notes: hard questions

Each question is answered with evidence or marked Open. All recorded
evidence is single-node. "Hadron node" means one Baryon-enabled Hadron node
running k3s; "kind host" means a historical development cluster in which a
kind node container acted as the host, no longer used. Both are described
in [testing.md](testing.md#results), which summarizes the runs behind every
"Tested" below; the experiments are in
[runtime-boundary.md](runtime-boundary.md). An answer whose evidence names
only the kind host has not been repeated on a Hadron node. Multi-node
testing on Hadron nodes running k0s is in progress.

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
| How is a cluster release paired with a host image? | The anchor sends its protocol (`baryon.runtime.v1alpha1`) in each attach; baryon-virtd refuses any other value (`FAILED_PRECONDITION`), and the VM reports `AttachRefused`. A supported skew policy for rolling upgrades is Open. | `crates/virtd/src/services.rs` (Implemented) |

## Storage

| Question | Answer | Evidence |
|---|---|---|
| How does a CSI volume become accessible to a host VMM? | baryon-virtd resolves the path inside the Anchor mount namespace as an `O_PATH` fd; the VMM reopens it through `/proc/self/fd/N` as the VM's host identity. | ADR 0006; Experiment A |
| How do filesystem and block PVCs differ? | Filesystem: `disk.img` on the mounted volume, imported or pre-populated. Block: the device node in the container, pre-populated. Same handoff. | Block mode Open |
| Who owns locks? | Cloud Hypervisor takes an OFD write lock at boot; it is released when the VMM exits. | Cloud Hypervisor v53 `block/src/io/fcntl.rs` |
| What survives pod restart? | The claim and its content. A container restart restarts the VM; a pod replacement may move it. | ADR 0004 |
| What survives node reboot? | Claims (per CSI driver; local-path keeps the data on the node). Runtime records and VMMs do not. The controller replaces the Anchor Pod and the VM boots again from the same claim. | Tested on the Hadron node (local-path); see Recovery below |
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
| Which guest network paths are tested? | From another pod to the pod IP, and through a ClusterIP Service by DNS name. NetworkPolicy and guest-initiated egress are Inferred, not tested. | [networking.md](networking.md) |
| Must guest DHCP survive a baryon-virtd restart? | Today it does not: the responder exits with baryon-virtd and is restarted only once the anchor reattaches, so a guest that reboots in between gets no address until then. Whether this is acceptable is Open. | [networking.md](networking.md) |

## Scheduling

| Question | Answer | Evidence |
|---|---|---|
| Does the Anchor Pod completely determine VM placement? | Yes. Placement controls in the VM API (nodeSelector, tolerations, affinity, priority) are Planned. | ADR 0004 |
| How are virtualization capabilities expressed? | The `virtualization.baryon.io/vm` extended resource marks usable nodes; VirtualizationNode reports details. | ADR 0004, ADR 0009 |
| How will NUMA requirements affect scheduling? | Through the Anchor container's resources: the CPU and memory managers assign cpusets to the container, and the VMM inherits them because it shares the container cgroup. Not tested with the static policy. | Open |
| How will device allocation integrate with DRA? | Designed direction: ResourceClaims on the Anchor Pod, with baryon-node-agent as the DRA driver and baryon-virtd handing devices to the VMM. | Planned |

## Security

| Question | Answer | Evidence |
|---|---|---|
| Which privileges are unavoidable? | baryon-virtd as root on the host (namespace entry, TAP creation, cgroup placement, spawning as another uid, mounting the VMM tmpfs). | [security.md](security.md) |
| Who can reach the anchor socket and start a VM? | Only containers allocated the `virtualization.baryon.io/vm` resource receive the socket mount. With `baryon-vm-resource` installed, only baryon-controller can create pods that request it; baryon-virtd then requires the anchor's uid and gid to equal the VM's host identity (`PermissionDenied` otherwise), so a pod can only drive the VM it was created for. The policy has to be installed; without it any pod could request the resource and start a VM of its own, running as the identity of the VM UID it names. | `deploy/base/policy/vm-resource.yaml`; Tested by the CI API server checks, on the Hadron node and the kind host |
| Which privileges exist only because of the prototype? | baryon-virtd keeps the full root capability set; reducing it is Designed. The anchor check proves pod membership and the VM's host identity, not that the pod was allocated the vm resource; a check through the kubelet pod resources API is Designed. Guest DHCP is no longer parsed in the root process: a per-VM responder with the VMM's confinement parses it (Tested on the Hadron node and the kind host). | [security.md](security.md) |
| Can privileged operations be isolated in baryon-virtd? | Yes: the controller, node agent, and anchor hold no host privilege beyond their sockets. The node agent runs as uid 0 without capabilities; the node socket is 0600 in a 0700 directory and requires peer uid 0. | ADR 0005, ADR 0009 |
| Can the controller remain unprivileged? | Yes. It acts only on Anchor Pods it controls (owner reference with the VM UID) and deletes pods with a UID precondition. | `deploy/base/controller/`, `crates/controller/src/reconcile.rs` |
| Can the launcher avoid full privilege? | There is no launcher; baryon-anchor runs with no capabilities as a non-root uid. | ADR 0005 |
| Per-VM host identities or Kubernetes user namespaces? | A distinct host identity per VM is the contract (ADR 0011). Kubernetes user namespaces may be evaluated later as additional hardening for the Anchor container. | ADR 0011 |

## Recovery

| Question | Answer | Evidence |
|---|---|---|
| What happens after node reboot? | VMMs and records are gone. Without kubelet graceful node shutdown, the guest is stopped hard at the reboot; after boot, kubelet re-admits the existing Anchor Pod before the device plugin has registered, admission fails (`UnexpectedAdmissionError`, no healthy devices for `virtualization.baryon.io/vm`), and the pod fails. With graceful node shutdown, the guest shuts down cleanly and the old pod ends `Succeeded`. In both cases the controller replaces the finished pod with a new Anchor Pod (new pod UID and IP, same MAC), which stays Pending until the device plugin registers. | Tested on the Hadron node (manual procedure) |
| What does a guest reset do? | The VMM keeps running and re-creates the VM's devices; the guest boots again and gets a new lease. A hard reset shortly after the first boot can lose first-boot writes: the Debian guest lost its SSH host keys. | Tested on the Hadron node and the kind host |
| Should VMs automatically restart? | Yes while `runPolicy: Running`: a VMM exit ends the anchor container and kubelet restarts it with back-off. A guest-initiated power-off also restarts the VM. | Experiment C (VMM SIGKILL); Tested on the Hadron node (VMM crash) |
| How is an orphaned VM process reconciled? | A VMM whose anchor does not return within the reattach or recovery grace, or whose pod has no other process left, is stopped or killed. With the systemd cgroup driver, stopping the container scope also stops it, even while baryon-virtd is down; that is why the cgroupfs driver is refused. | Experiment C; Tested on the Hadron node (deletion with baryon-virtd stopped) |
| How is an orphaned Anchor Pod reconciled? | The Anchor Pod is owned by the VirtualMachine and garbage collected; the controller deletes it when the VM is stopped or deleted. A pod named `<vm>-anchor` that the VM does not control is left alone and reported as `AnchorPodConflict`. | lifecycle and deletion E2E; `crates/controller/src/reconcile.rs` |
| Is the systemd cgroup driver requirement permanent? | It is required today (cgroupfs is Not supported). Whether a different stop mechanism could lift it is Open. | Experiment C; [compatibility.md](compatibility.md) |

## Kairos lifecycle

| Question | Answer | Evidence |
|---|---|---|
| How is the Baryon-enabled Hadron artifact built? | Hadron base image, plus the Baryon host layer, then kairos-init with the chosen Kubernetes provider; AuroraBoot for bootable media. | [hadron.md](hadron.md) |
| How does Kairos Operator deploy an updated artifact? | `NodeOpUpgrade` with the new image. Not tested. | [kairos-integration.md](kairos-integration.md) |
| Does a Kairos A/B upgrade work on a Baryon node? | Yes for a manual `kairos-agent upgrade --source ocifile:...` and reboot with a VM running: the node booted the new image each time, and the VM was Ready again on a replacement Anchor Pod. The recovery system is upgraded separately (`--recovery`); not done. | Tested on the Hadron node, manual procedure in [kairos-integration.md](kairos-integration.md) |
| Does a `NodeOpUpgrade` reboot wait for the kubelet shutdown inhibitor? | Not verified. | Open |
| How can Baryon eventually signal upgrade safety? | Designed: evacuation, then a node label that a `NodeOpUpgrade` selects on. | [kairos-integration.md](kairos-integration.md) |
| What Baryon state must survive a Kairos A/B upgrade? | None in the OS image. Disks live in claims; logs in `/var/log`. | ADR 0003 |
| What host and cluster version skew is supported during rolling upgrades? | Mismatched protocols are refused (`AttachRefused`); a supported skew policy is Open. | Open |

## Cluster API

| Question | Answer | Evidence |
|---|---|---|
| What does Baryon assume about how the host cluster was created? | Nothing beyond [compatibility.md](compatibility.md). | |
| Can a CAPI-created Hadron cluster run Baryon without special integration? | Yes for k3s; it is an ordinary cluster. Its image must be the Baryon-enabled image. For k0s, whether its kubelet uses the systemd cgroup driver by default is not verified, and cgroupfs is Not supported. | research of cluster-api-provider-kairos v0.1.3 |
| What future machine capability metadata would be useful? | A Machine label marking Baryon-enabled images, and pre-drain hooks for evacuation. | Planned |

## Migration constraints recorded so far

- Cloud Hypervisor does not accept network fds on `vm.receive-migration`
  (cloud-hypervisor#7054, #8566).
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
  migration; a Unix socket brokered by baryon-virtd would be needed.
- baryon-virtd refuses a second VM with the same host identity on a node
  (`IdentityConflict`), which also refuses a same-node migration target.

## Open hard questions

- Is the systemd cgroup driver requirement permanent?
- A durable VM identity across re-create, restore, and move.
- Must guest DHCP survive a baryon-virtd restart?
- Migration against the fixed Anchor Pod name and the at-most-one-VMM rule.
- The supported host image and cluster release version skew during rolling
  upgrades.
- Does the reboot of a Kairos Operator `NodeOpUpgrade` wait for the kubelet
  shutdown inhibitor?
