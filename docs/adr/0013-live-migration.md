# 0013: Live migration is not supported

Status: Accepted
Date: 2026-10-04

## Context

Moving a running VM to another node without stopping the guest is the
missing piece for node maintenance: today a drain or a Kairos upgrade stops
the VMs on the node, and the controller starts them again elsewhere when
their storage allows (ADR 0004,
[kairos-integration.md](../kairos-integration.md#cooperation-with-upgrades)).
This ADR records what live migration would require of Baryon and of Cloud
Hypervisor, what was tried, and why Baryon does not offer it.

The evaluation covered Cloud Hypervisor v53.0 (tag `v53.0`, commit
9ed824d6, the version in the Baryon-enabled Hadron image) and the upstream
main branch as of 2026-10-03 (commit eb7425276). It was read from the code;
where the documentation and the code differ, the code is cited. Paths below
are in the Cloud Hypervisor repository unless they start with `crates/`.

### What Cloud Hypervisor v53 provides

- `vm.send-migration` on the source and `vm.receive-migration` on a
  destination VMM started without a VM. Transports: `tcp:` (optionally with
  mutual TLS) and `unix:`. Pre-copy with dirty-page tracking, then pause,
  final copy, and device state. Post-copy exists in v53.
- The destination is configured from the stream. It adopts the source's
  full VM configuration as received (`vmm/src/lib.rs`, the receive of the
  `Config` step), including `landlock_enable` and `landlock_rules`, disk
  paths, and console paths, and it deserializes the full device state. The
  only checks are the migration protocol version (0 in v53) and CPUID
  compatibility. v53 offers no way to override paths or file descriptors on
  the destination. Main can substitute file descriptors on receive for VFIO
  devices only (`vfio_fds`, `iommufd_fd`; commit 59a6563a5, not in v53).
- Network devices passed as file descriptors (`fds=`) are deserialized on
  the destination as `-1` (`vmm/src/vm_config.rs`,
  `deserialize_netconfig_fds`), so restoring the network device fails.
  Upstream tracks network fds on the destination in
  [cloud-hypervisor#7054](https://github.com/cloud-hypervisor/cloud-hypervisor/issues/7054)
  and [#8566](https://github.com/cloud-hypervisor/cloud-hypervisor/issues/8566);
  [#8778](https://github.com/cloud-hypervisor/cloud-hypervisor/issues/8778)
  proposes one `external_fds` list on restore and receive-migration for the
  resources that already take fds (network, VFIO, IOMMU). All three are
  open. None of them covers disks or host paths; a proof of concept,
  [#7150](https://github.com/cloud-hypervisor/cloud-hypervisor/pull/7150),
  was closed unmerged.
- Disks are reopened on the destination by the path string from the source
  configuration. The storage must be reachable at the same path, and is not
  copied.
- The source releases its disk locks before sending device state
  (`vmm/src/lib.rs`, "We release the locks early"). If the migration fails
  after that point the source resumes without taking the locks again; this
  is the case in v53 and on main.
- Switchover: the destination resumes the guest when it receives the
  `Complete` request and sends its acknowledgement afterwards
  (`vmm/src/lib.rs`, the `StateReceived` state of the receive state
  machine). The source treats a missing acknowledgement, or an error in the
  steps after it (`stop_dirty_log`, `complete_migration`), as a failed
  migration and resumes its own copy, without its disk locks
  (`vmm/src/lib.rs`, `send_migration` and `check_migration`). In v53 this
  applies to pre-copy and post-copy. Main stops the source instead of
  resuming it only after a failed post-copy migration (commit bf224df88),
  so on main a pre-copy migration whose final acknowledgement is lost still
  runs the guest on both nodes. Main also adds a `preserve_source` option
  (accepted with the `memfds` memory mode) that hands the paused source VM
  back to its VMM after a successful migration by design.
- Receive endpoint (`vmm/src/migration/transport.rs`): plain `tcp:` has no
  authentication. With `tls_dir`, any client certificate issued by the
  configured CA is accepted; nothing binds the connection to a particular
  source. TLS is refused for `unix:`. After the `Config` step the receiver
  accepts up to 128 additional connections from any peer, and their memory
  payloads are written into guest memory. Payload sizes come from the peer.
  There are no I/O timeouts on accept, handshake, or reads; the only
  timeout is 10 s on the role header of an additional connection.
  `timeout_s` (default one hour) is a pre-copy duration limit only, checked
  between pre-copy iterations.
- v53 has no cancel API (main adds `vm.cancel-migration`).
- No VMM version check beyond the migration protocol version; upstream
  documents compatibility across releases only from v54 on, which is not
  released. The destination CPU must offer every CPUID feature the source
  exposes; v53 has only the host CPU profile, and checks CPUID only.
- Upstream tests cover a single host, direct kernel boot, and TAP devices
  created by the VMM. `test_live_migration_with_landlock` in v53 covers a
  VMM confined by Landlock, so a confined VMM is partly covered. None covers
  firmware boot, fd-passed networking, disks passed as `/proc/self/fd/N`, a
  VMM running as an unprivileged identity, or shared storage across hosts.
  The vhost-user-net migration tests (OVS-DPDK) are marked `#[ignore]`
  (`cloud-hypervisor/tests/integration.rs`).

### What Baryon does today

- Baryon creates `ReadWriteOnce` claims. `ClaimInUse` refuses a claim used
  by a pod of another VirtualMachine and ignores pods of the same VM
  (`claim_user` in `crates/controller/src/reconcile.rs`). `DiskInUse` is a
  node-local registry in baryon-virtd (`claim_block_devices` in
  `crates/virtd/src/vm.rs`). Neither would stop a copy of the same VM on
  another node. What prevents a second copy today is the fixed Anchor Pod
  name, the at-most-one rule (ADR 0004), and `ReadWriteOnce` claims.
- baryon-virtd keys the VM's runtime directory (`/run/baryon/vms/<pod-uid>/`,
  `vm_dir` in `crates/virtd/src/config.rs`), the serial socket, and the
  VMM's Landlock writable directory (`crates/virtd/src/vm.rs`) by the
  Anchor Pod UID.
- Disks reach the VMM as `/proc/self/fd/N` (ADR 0006); baryon-virtd chooses
  the fd numbers.
- The guest keeps its SMBIOS UUID (ADR 0008) and its MAC address (ADR 0007)
  on every node, both derived from the VirtualMachine UID.
- No Baryon component listens on TCP, and baryon-virtd holds no
  credentials ([security.md](../security.md)).
- The Cloud Hypervisor client in `crates/cloud-hypervisor/src/api.rs` can
  call only `vmm.ping`, `vm.info`, `vm.power-button`, and `vmm.shutdown`;
  its generic request method is private.

## Decision

- Live migration is Not supported. The wording used in the documentation
  and release notes: "Live migration is currently unsupported because
  Baryon's network handoff model requires destination-side file descriptor
  injection during Cloud Hypervisor migration, which is not available in
  the currently supported Cloud Hypervisor release." Baryon has no public
  migration API (no `VirtualMachineMigration` or equivalent resource), no
  migration RPC in the runtime protocol, and no migration code in
  baryon-virtd.
- Evacuation stays stop and restart: a drain or a Kairos upgrade stops the
  VMs on the node, and the controller starts them again where their storage
  allows (ADR 0004).
- The blocker is in Cloud Hypervisor: Baryon's network attachment (ADR 0007)
  hands the VMM a TAP file descriptor from the Anchor Pod's network
  namespace, and a v53 destination cannot receive a network device as a file
  descriptor. Working around it would need either a root VMM that creates
  TAP devices itself in a network namespace it can see, which ADR 0005 and
  ADR 0011 rule out, or a migratable VM without a network device, which is
  not a useful product.
- The entry criteria for reconsidering, all required:
  1. Destination resources: a released Cloud Hypervisor accepts externally
     supplied file descriptors and host paths on the destination for
     network devices, disks, the serial console, and the VMM's runtime
     directory. #8778 (`external_fds`) would cover network devices; disks
     and host paths have no upstream issue known to Baryon. Main's
     VFIO-only fd substitution is the likely mechanism to extend. Matching
     `/proc/self/fd/N` disk numbers is possible on the Baryon side, because
     baryon-virtd chooses them, but reopening by fd number alone is not an
     acceptable trust basis: a mismatch means I/O to the wrong object.
  2. Ambiguous failure: a released Cloud Hypervisor can cancel a
     migration, and when a migration fails after `Complete` was sent
     (including a lost acknowledgement), the source stays paused until an
     external authority decides which copy runs. Cloud Hypervisor alone
     cannot resolve a lost acknowledgement, so this criterion depends on
     criterion 3, and criterion 3 depends on it. A Baryon design never
     uses `preserve_source`.
  3. Fencing: one authority for the active copy, held in Kubernetes, and
     enforcement on the data path, so that a copy without the authority
     cannot write: storage-level fencing (for example RBD exclusive lock or
     client blocklisting; unverified, and possibly CSI-dependent) or
     self-fencing on lease loss. It does not rely on Cloud Hypervisor's
     disk locks.
  4. Stream trust: the destination's confinement (seccomp, Landlock,
     identity, network namespace), disks, network file descriptors, and
     console come from the destination's baryon-virtd, never from the
     migration stream.
  5. Receive endpoint: Cloud Hypervisor's migration listener is never
     reachable beyond the VM's own identity. baryon-virtd authenticates the
     peer node, binds the session to one authorized migration, enforces
     deadlines whose expiry kills the destination VMM, and bounds payload
     sizes.
  6. Authorization: a migration is authorized by a Kubernetes object that
     only baryon-controller can create or approve, enforced by admission.
     The destination is taken from authenticated node identity, never from
     an anchor, a user, or a VM field.
  7. Guest address: either a CNI contract for keeping the pod IP, or an
     explicit decision that the guest's address changes.
  8. Version compatibility: a released Cloud Hypervisor with cross-version
     migration guarantees, and a Baryon-side version gate (ADR 0002).
  9. CPU compatibility: a model the scheduler can use (a non-host CPU
     profile, node labels), including mitigation-relevant CPU enumeration,
     because Cloud Hypervisor checks CPUID only (MSRs unverified).
  10. Maintenance: cooperation with Kairos Operator, whose drain deletes
      pods directly, without the Eviction API and with no hook before the
      deletion
      ([kairos-integration.md](../kairos-integration.md#kairos-operator)).
      Baryon does not add a parallel evacuation controller.
  11. The design passes an architecture and a security review, and the
      migration is tested on the k0s cluster with shared storage and the
      VMM confined as in ADR 0005 and ADR 0011.

## What a design would have to answer

These questions were evaluated so that a future design starts from the
recorded constraints. None of them is decided here.

- Placement: the target must be chosen by the Kubernetes scheduler, not by
  Baryon, so the target needs its own Anchor Pod scheduled with the same
  placement. The fixed Anchor Pod name (`<vm>-anchor`) and the at-most-one
  rule (ADR 0004) leave no room for a second pod; migration needs a
  different naming and an explicit "active pod" in the VM status. CPU
  compatibility needs node labels the scheduler can use; Baryon's capability
  vocabulary is deferred (ADR 0008), and v53 has no CPU profile other than
  the host.
- Identity and fencing: the VM keeps its SMBIOS UUID (ADR 0008) and MAC
  address (ADR 0007) on every node, so two running copies are
  indistinguishable to the network and the storage. Cloud Hypervisor
  releases the disk locks before the switchover and does not take them back
  on failure, and its locks are host-local anyway. `ClaimInUse` and
  `DiskInUse` do not apply to two copies of one VM on different nodes (see
  What Baryon does today). A migration design must allow two Anchor Pods of
  one VM, so the protections that prevent a second copy today no longer
  hold, and with a `ReadWriteMany` claim nothing in Baryon fences the two
  copies. Fencing must be new (criterion 3). The node identity check
  (`IdentityConflict`) also refuses a target on the same node.
- Switchover: in v53, and on main for pre-copy, a lost final
  acknowledgement resumes the source while the destination runs. The
  source cannot tell a lost acknowledgement from a destination that never
  resumed, so only an authority outside both VMMs can decide (criterion
  2).
- Storage: both nodes must have the disk at the same time. Migration needs
  `ReadWriteMany` block volumes (Ceph RBD supports them) and identical
  paths, which Baryon's `/proc/self/fd/N` disk handoff (ADR 0006) gives
  only by matching file descriptor numbers; the destination's baryon-virtd
  would still have to verify that each fd refers to the intended volume.
  Node-local volumes (LVM LocalPV) cannot migrate.
- Runtime paths: the runtime directory, serial socket, and Landlock
  writable directory are keyed by the Anchor Pod UID. The target Anchor Pod
  has a new UID, so the configuration received in the stream names paths
  that do not exist on the target.
- Networking: the guest's address is the Anchor Pod's IP (ADR 0007). A new
  Anchor Pod gets a new pod IP from the CNI, so even a successful migration
  changes the guest's address, which defeats the purpose for most workloads.
  Keeping the address needs CNI support for moving a pod IP, which Baryon
  cannot assume (it is not a CNI).
- Stream trust: the stream comes from the source VMM, which its guest may
  control. Everything the destination adopts from it (configuration,
  Landlock settings, paths, device state) is untrusted input (criterion 4).
- Runtime ownership: baryon-virtd would own the transfer on both nodes. The
  VMM runs in an empty network namespace, so the `tcp:` transport is
  unavailable to it. A relay in baryon-virtd between `unix:` sockets on two
  nodes would make the root runtime hold the guest's memory in plaintext as
  it passes, hold node credentials, and listen on TCP; today no Baryon
  component listens on TCP and baryon-virtd holds no credentials.
  baryon-controller never talks to baryon-virtd (ADR 0005); whether a
  migration request reaches baryon-virtd through the Anchor Pods or through
  the node agent is open.
- Failure: with no cancel API and no I/O timeouts in v53, a stuck migration
  can only be ended by killing a VMM. A late failure resumes the source
  without disk locks. Version skew during a Kairos rolling upgrade is the
  normal state of a cluster (ADR 0012), and v53 does not check VMM versions.

## Hard questions left open

- Which Kubernetes object is the authority for the active copy, and how a
  paused source learns the outcome of an ambiguous switchover.
- Storage-level fencing without Baryon becoming CSI-aware.
- Whether runtime paths stay keyed by the Anchor Pod UID.
- Whether a migration that changes the guest's IP address is worth
  supporting.
- Migration-based evacuation with a Kairos Operator that deletes pods
  without the Eviction API.

## Experiment

A controlled experiment outside Baryon checked the part of the path that
Cloud Hypervisor v53 supports, on two nodes of the k0s cluster with Ceph
RBD.

Setup:
- One `ReadWriteMany` Ceph RBD block claim holding the Debian 13 cloud
  image, attached on both nodes through two idle pods, and linked to the same
  path on both nodes.
- Cloud Hypervisor v53 run by hand as root in the host network namespace on
  both nodes: firmware boot, two vCPUs, 1 GiB memory, no network device, a
  NoCloud seed disk that starts a heartbeat: it writes a counter to the root
  disk, syncs it, and then prints it on the serial console.
- Migration over the `tcp:` transport, pre-copy.
- The migration listener was plain TCP without authentication, the VMM ran
  as root without Landlock, and the shared device bypassed Baryon's claim
  and device exclusion. The experiment validates no security property.

This setup deliberately leaves out what Baryon needs and v53 cannot do: an
unprivileged confined VMM, an empty network namespace, and disks and a TAP
device passed as file descriptors.

Results (2026-10-04):
- A pre-copy migration completed. On the destination the guest kept its
  boot ID, and the heartbeat, which syncs a write to the RBD root disk
  before each console line, continued without a gap in its counter. The
  source VMM exited.
- After the migration only the destination held a write lock on the disk,
  an open file description lock on its local device node. The lock is
  host-local: the other node cannot see it, so it does not prevent a second
  VMM on another node from opening the same volume.
- A migration back to the first node failed when its destination was made
  unable to open the disk (the disk path was removed on that node before
  the receiver started). The destination failed at the device state step.
  The source resumed and the guest kept running, but without its disk
  lock, as the code shows.
- The disk had to be configured with an explicit `image_type=raw`; with
  image type detection, Cloud Hypervisor v53 refuses writes to sector 0 of
  a raw disk. baryon-virtd already passes `image_type=raw`.

Conclusion: Cloud Hypervisor v53 moves memory, CPU, and device state of a
firmware-booted guest between Hadron nodes with shared Ceph RBD storage.
What Baryon needs on top (destination-supplied network devices, disks, and
paths, a confined unprivileged VMM without TCP, a trusted and authenticated
transfer, fencing that survives an ambiguous switchover, and a stable guest
address) is missing in Cloud Hypervisor, in Baryon, or in the CNI, as
listed above. The experiment does not change the decision. Its private
record stays out of the repository; no timings are published.

## Consequences

- Node maintenance stops VMs. Workloads that cannot tolerate a restart need
  their own redundancy across nodes, placed with `spec.placement`
  anti-affinity.
- The runbook for Kairos upgrades stays one NodeOpUpgrade per node
  ([kairos-integration.md](../kairos-integration.md#cooperation-with-upgrades)).
- Nothing in the API, the runtime protocol, or baryon-virtd anticipates
  migration; a future design starts from this ADR and supersedes it.
- Cloud Hypervisor's migration endpoints remain in the pinned binary. They
  are unreachable today: the API socket is reachable only by host root, the
  Baryon client cannot call them, and a VMM's network namespace is empty.
  Cloud Hypervisor's seccomp profile allows `userfaultfd` and inet
  sockets, which Baryon does not use; since 2026-10-04 Baryon's own
  seccomp filter denies both on every VMM thread
  ([security.md](../security.md#baryon-seccomp-layer)).

## Alternatives considered

- A migration API that stops the VM and starts it on the target ("cold
  migration"): deleting the Anchor Pod already stops the VM and starts it
  again through a new Anchor Pod, without a new resource. It does not
  choose a target: the new pod can land on the same node unless that node
  is cordoned. The conclusion is unchanged: no new resource.
- Migrating VMs without a network device: technically possible on v53 with
  a root VMM, but no useful VM runs without a network, and a root VMM
  breaks ADR 0005 and ADR 0011.
- A root VMM that creates its own TAP device on the destination by name:
  would need the VMM in the pod's network namespace with `CAP_NET_ADMIN`;
  rejected for the same reasons.
- vhost-user-net as the network attachment for migratable VMs: the VMM
  would connect to a userspace datapath backend through a socket path
  instead of receiving a TAP fd. Rejected: Baryon would ship and run that
  backend, which must track dirty pages for migration; the socket path has
  the same destination path problem as disks; and upstream has no tested
  migration path for it (the vhost-user-net migration tests are ignored).
- Snapshot on one node and restore on another: v53's `vm.restore` accepts
  new network file descriptors (`net_fds`, passed with `SCM_RIGHTS`), so
  unlike live migration it fits Baryon's network attachment. It is not live
  (the guest is paused while its memory is written and read back). On-demand
  restore (`memory_restore_mode=ondemand`) uses `userfaultfd`, which is
  unverified in the confined VMM. Snapshots go to a `file://` directory
  only, so a move needs storage that the target node reaches. Restore takes
  the disk locks again. The Anchor Pod UID path problem applies, the guest
  address still changes with the new Anchor Pod, and snapshots are Not
  supported in Baryon. A candidate for a later suspend-and-move design; not
  evaluated further here.

## Revisions

- 2026-10-04 (before the first commit of this ADR), after the architecture
  and security reviews, with the decision unchanged: the switchover failure
  was corrected (a lost acknowledgement resumes the source in v53
  for pre-copy and post-copy, and on main for pre-copy), stream trust, the
  receive endpoint, the relay and authorization, fencing, Anchor Pod UID
  runtime paths, guest address, version, CPU, and Kairos Operator entry
  criteria were added, vhost-user-net was added to the alternatives, and
  the hard questions left open were recorded.
- 2026-10-04: the consequence on Cloud Hypervisor's seccomp profile now
  records that Baryon's seccomp filter denies `userfaultfd` and inet
  sockets. The decision is unchanged.
- 2026-10-05: the Decision now carries the exact sentence that the
  documentation and release notes repeat. The decision is unchanged.
- 2026-10-27: re-checked against Cloud Hypervisor v54.0, the version Baryon
  moves to. The decision and its exact sentence are unchanged. v54.0 adds
  `vm.cancel-migration`, accepts migrations from the two previous releases,
  and accepts pre-opened VFIO and `iommufd` descriptors on
  `receive-migration` (`vfio_fds`, `iommufd_fd`), but still no network
  descriptors: `VmReceiveMigrationData` in `vmm/src/api/mod.rs` has no
  network field, and `deserialize_netconfig_fds` in `vmm/src/vm_config.rs`
  still restores them as `-1`.
  [cloud-hypervisor#7054](https://github.com/cloud-hypervisor/cloud-hypervisor/issues/7054)
  is open. The sections above describe v53 and are kept as written.
- 2026-10-28: the documented wording joins its two sentences with
  "because"; the meaning and the decision are unchanged.
