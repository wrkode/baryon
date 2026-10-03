# 0005: Host-owned Cloud Hypervisor confined to the Anchor container

Status: Accepted. Validated on a historical development cluster in which a
kind node container acted as the host (see
[runtime-boundary.md](../runtime-boundary.md), Experiment C) and on a single
Baryon-enabled Hadron node (k3s, systemd cgroup driver). The cgroupfs driver
is Not supported.
Date: 2026-10-02, revised 2026-10-03

## Context

Each VM has a VM Anchor Pod (ADR 0004). The question is where the Cloud
Hypervisor process runs and who owns it.

Model A runs Cloud Hypervisor inside the Anchor container. The container then
needs /dev/kvm, a TAP (CAP_NET_ADMIN in the pod network namespace, or a TAP
fd from somewhere privileged), the VMM binary and firmware in the container
image, and the VMM version becomes a property of the Baryon container image
rather than of the host substrate.

Model B runs Cloud Hypervisor as a host process owned by baryon-virtd. The VMM
comes from the Hadron image and can use host features (VFIO, hugepages,
virtiofsd) without granting them to VM namespaces. Its risks: resource
accounting, access to the pod's mount and network namespaces, and teardown
ordering against kubelet.

Facts that shaped the decision:

- Cloud Hypervisor v53.0 accepts an inherited TAP fd (`--net fd=N`) without
  capabilities; the TAP may live in another network namespace. Disks are
  opened by path only. `vm.create` over the API drops fds.
- kubelet root directory, CRI socket, and cgroup driver differ between k3s,
  k0s, and kubeadm.
- The container's view of a volume is a separate mount from kubelet's, so a
  VMM holding a disk does not stop kubelet from unmounting it. Teardown
  safety must come from ordering.
- A process in a container's cgroup is subject to that container's device
  cgroup, cpuset, and limits, and is stopped by the container runtime with the
  container (measured: VMM gone 80 ms into StopContainer, 300 ms before
  kubelet volume teardown, with baryon-virtd not running).

## Decision

Model B, with the VMM placed inside the Anchor container's cgroup:

1. baryon-anchor connects to baryon-virtd over a Unix socket. baryon-virtd
   identifies the caller from `SO_PEERCRED` and `SO_PEERPIDFD` and derives
   the pod UID and cgroup from `/proc/<pid>/cgroup`, the network namespace
   from the pidfd, and the mount namespace root from `/proc/<pid>/root`. No
   kubelet paths, CRI calls, or cgroup driver settings are used.
2. baryon-virtd starts Cloud Hypervisor from the host image:
   - as the Anchor container's uid and gid (the VM's host identity, ADR
     0011), with its supplementary groups and
     the group owning /dev/kvm; no capabilities and an empty bounding set;
     `no_new_privs`; its own session; stdio on files, never pipes
   - in a new, empty network namespace: the VMM reaches the guest network only
     through the inherited TAP fd, and has no route to host or cluster
     addresses
   - in the Anchor container's cgroup, entered before `execve`. This cgroup
     must be a systemd scope (the kubelet systemd cgroup driver): stopping
     the scope is what stops the VMM before kubelet tears down volumes and
     network, also while baryon-virtd is down. baryon-virtd refuses to start
     a VM in any other cgroup layout (`CgroupDriverUnsupported`). Under the
     cgroupfs driver nothing outside baryon-virtd would stop the VMM in time
     (runc kills leftover cgroup processes only for containers without a
     private PID namespace, and kubelet kills leftover pod processes only
     after volume teardown; inferred from upstream code, not observed).
   - with its runtime directory on a private, size-limited tmpfs owned by
     the VM's uid
   - with a TAP fd from the pod network namespace and disk `O_PATH` fds
     resolved in the pod mount namespace (ADR 0006, ADR 0007)
   - with seccomp and Landlock enabled
3. /dev/kvm reaches the container's device list, and the anchor socket
   reaches the container, through the `virtualization.baryon.io/vm` device
   plugin (ADR 0009).
4. Every VM executes as a distinct unprivileged host identity (ADR 0011),
   from the range 1000000000 to 1999999999 that hosts reserve for it. The
   controller runs the Anchor container as that identity. VMMs of different
   VMs therefore cannot signal or trace each other. baryon-virtd checks the
   anchor against the same identity (shared code in `baryon-runtime-api`)
   and refuses an anchor whose uid or gid differs, so a pod cannot drive a
   VM it was not created for. It also refuses a second VM with the same
   identity on a node (`IdentityConflict`).
5. Lifetime: the VMM survives baryon-virtd restarts and is re-adopted from
   its runtime record. It does not survive an Anchor container restart; the
   container runtime stops it with the container and a new VM is started.
   When the anchor receives SIGTERM it asks for an ACPI shutdown and waits.
   If the anchor exits during a stop, or the pod has no process left besides
   the processes baryon-virtd started (the VMM and the DHCP responder),
   baryon-virtd kills the VMM at once.
6. Guest DHCP is answered by a second confined host process per VM, the DHCP
   responder: started by baryon-virtd with the VMM's identity (without the
   kvm group), the same confinement, and the same cgroup, holding only the
   packet socket on the TAP. It exits when baryon-virtd does and is started
   again on reattach (ADR 0007).

## Consequences

- The controller stays unprivileged and never reaches the host.
- The Anchor container needs no capabilities and no VMM binary. It does get
  /dev/kvm in its device list, because the VMM shares its cgroup; a user who
  can exec into the Anchor container can open /dev/kvm within the pod's
  limits.
- Pod limits, per-container cpusets (CPU and memory managers), and
  container-level accounting apply to the VMM.
- Teardown ordering does not depend on baryon-virtd being up.
- A compromised VMM has no network access of its own: its network namespace
  is empty, and the TAP fd only reaches the guest's side of the pod
  interface. It still runs in the host mount namespace, restricted by
  Cloud Hypervisor's Landlock rules (see [security.md](../security.md)).
- An Anchor container crash restarts the guest with power-loss semantics.
- Two VMs on one node can receive the same identity (probability about
  n^2 / 2^31 for n VMs on one node). baryon-virtd refuses the second, and
  baryon-controller places it on another node (ADR 0011).
- The identity is stable for the life of a VirtualMachine object, and
  imported disk images are owned by it (mode 0600). Re-creating a
  VirtualMachine over a retained or restored claim gives a new identity
  that cannot open the old image unless the storage applies `fsGroup`
  ownership (not tested). The cluster release and the host image must
  agree on the identity, so changing its allocation is a protocol change
  (ADR 0009, ADR 0011).
- The kubelet systemd cgroup driver is a host requirement; cgroupfs is Not
  supported.
- Live migration constraints from this decision: the VMM's empty network
  namespace rules out Cloud Hypervisor's TCP migration transport (a Unix
  socket brokered by baryon-virtd would be needed), and the
  `IdentityConflict` check refuses a second VMM of the same VM on one node.
- Live migration with fd-backed networking needs upstream Cloud Hypervisor
  support (cloud-hypervisor#7054, #8566) or a different network attachment for
  migratable VMs.

## Alternatives considered

- Model A (VMM inside the Anchor container). Rejected: device and capability
  grants to pods in VM namespaces, VMM version tied to the container image, and no
  path to host-level devices without more privilege.
- Model B with the VMM in a `baryon-vmm` child cgroup of the pod cgroup.
  Implemented and measured (`--vmm-cgroup pod-child`), then rejected: it
  creates cgroups under systemd-managed slices, which systemd does not
  delegate; the VMM escapes per-container CPU and memory manager cpusets; and
  teardown ordering depends on baryon-virtd being up. The flag was removed
  before the first commit.
- Model B with Cloud Hypervisor as root. Rejected: root without capabilities
  still owns root-owned files.
- A shared VMM uid for all VMs. Rejected: one compromised VMM could signal or
  trace every other VMM on the node.
- Locating pods through the CRI or kubelet paths. Rejected: provider-specific
  and a confused-deputy risk.

## Revisions

- 2026-10-03: after the security review, the VMM gets an empty network
  namespace and an empty capability bounding set, baryon-virtd checks the
  anchor's uid and gid against the VM's identity, and guest
  DHCP moves into a confined responder process (item 6). Cloud Hypervisor is
  also allowed to create and remove files in its own runtime directory under
  Landlock, which a guest reboot needs to re-create the serial socket. The
  placement decision is unchanged.
- 2026-10-03, after the architecture gate: the systemd scope requirement is
  stated and enforced, the diagnostic pod-child placement was removed from
  baryon-virtd (its measurements stay in runtime-boundary.md), each VM's
  runtime directory became its own tmpfs, and the identity and migration
  consequences were recorded.
- 2026-10-03: the per-VM identity is specified by ADR 0011. Item 4 and the
  identity consequences now refer to it instead of describing how the
  identity is computed; a collision on a node is handled by placing the VM
  on another node instead of being reported only as a start failure.
