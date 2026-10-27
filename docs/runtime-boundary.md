# Runtime boundary

This document records the experiments that decide the boundary between the
VM Anchor Pod, baryon-virtd, and Cloud Hypervisor. ADR 0005 (process
ownership) and ADR 0007 (network) were accepted after the observations below
were repeated on a Baryon-enabled Hadron node. ADR 0006 (storage) is
Accepted for filesystem-mode and block-mode claims; block mode and a CSI
driver with staging and attach (Ceph RBD) are Tested on the k0s cluster (see
[k0s cluster results](#k0s-cluster-results)). The guest reboot reopen under
Landlock is Tested on the Hadron node and the kind host (Experiment A).

Status: this is a historical record. Experiments A, B, and C ran on the kind
host with real VMs; process ownership and cgroup placement are decided (see
Experiment C). What the Hadron node confirmed is summarized in
[Hadron node results](#hadron-node-results), and what the three-node k0s
cluster added in [k0s cluster results](#k0s-cluster-results). A summary of the E2E tests and manual procedures is in
[testing.md](testing.md#results); this document keeps the experiment
observations. Open items are listed per experiment.

## Shared setup

### Environments

| Name | Description |
|---|---|
| kind host | A development cluster in which a kind node container acted as the host; no longer used. kind v0.30.0, Kubernetes v1.34.0, containerd v2.1.3, kubelet cgroup driver systemd with `cgroupRoot: /kubelet`, kindnet CNI, local-path provisioner, a control plane node and one worker node. Host kernel 6.8 (x86_64, nested KVM). baryon-virtd ran as a systemd unit inside the worker node container, which acted as the host. Cloud Hypervisor v53.0 static, CLOUDHV.fd from cloud-hypervisor/edk2 `ch-97eeb7b09`. |
| Hadron node | A single VM with nested KVM running the Baryon-enabled Hadron image, first booted from an AuroraBoot raw EFI disk and later upgraded several times with Kairos A/B upgrades, then provisioned again from scratch: Hadron v0.5.3, kernel 7.2.6-hadron, Kairos (kairos-init v4.3.0), k3s v1.36.4+k3s1 with its embedded containerd, systemd cgroup driver, flannel with the vxlan backend and the `cni0` bridge, local-path provisioner. baryon-virtd runs as the systemd unit from the image. Cloud Hypervisor v53.0 and CLOUDHV.fd `ch-97eeb7b09` from the image. |

The kind host was not a Baryon host and is not a Baryon environment. Its
results are evidence about Linux, containerd, kubelet, and Cloud Hypervisor
behavior. The Hadron node is a Baryon host on a single node under nested
virtualization.

### Guest

Debian 13 genericcloud raw image (`debian-13-genericcloud-amd64.raw`,
3 GiB), UEFI boot.

### Experiment pod

The experiments used a hand-written Anchor Pod of the same shape
baryon-controller generates, running as a fixed unprivileged uid
(`<vm-uid>` in the measurements below). The experiment manifests and
scripts are no longer in the repository. Today baryon-virtd accepts an
anchor only if it runs as its VM's host identity (ADR 0011), and with the
`baryon-vm-resource` admission policy installed only baryon-controller may
create pods that request the vm resource; a hand-written Anchor Pod is
therefore not a supported way to start a VM.

### Pod identification used by all experiments

baryon-anchor connects to `/run/baryon/anchor/virtd.sock`. baryon-virtd takes
`SO_PEERCRED` and `SO_PEERPIDFD` from the connection and parses the pod UID
and pod cgroup from `/proc/<pid>/cgroup`.

Observed: the pod UID was derived correctly from the systemd-driver path
`/kubelet.slice/kubelet-kubepods.slice/kubelet-kubepods-burstable.slice/kubelet-kubepods-burstable-pod<uid>.slice/cri-containerd-<id>.scope`
on every run. No kubelet path, CRI call, or cgroup driver setting was used.

## Experiment A: storage handoff

### Problem

A PVC is attached to the Anchor Pod by CSI and is visible only in the pod's
mount namespace. A host-owned Cloud Hypervisor must open it without knowing
the kubelet directory layout, and must not be using it when kubelet tears the
volume down.

### Hypothesis

1. `openat2(<fd of /proc/<pid>/root>, path, O_PATH, RESOLVE_IN_ROOT |
   RESOLVE_NO_MAGICLINKS)` resolves the disk inside the container and absolute
   symlinks cannot escape.
2. Cloud Hypervisor, running as the Anchor uid, can reopen the inherited
   `O_PATH` fd through `path=/proc/self/fd/N`.
3. The VMM is no longer using the disk when kubelet tears the volume down.
4. A guest reboot inside the same Cloud Hypervisor process reopens the disk.

### Implementation

`crates/virtd/src/handoff/storage.rs`, disk import by
`baryon-anchor import` (`crates/anchor/src/import.rs`).

### Observations

- The importer streamed the 3 GiB raw image over HTTP into a filesystem-mode
  local-path PVC in 15 s, verified SHA-256, and wrote it sparse; a second pod
  start skipped the import using the marker file.
- Resolution worked on every run (`disk resolved in anchor mount namespace,
  kind File, size 4294967296`). Unit tests (`storage.rs`) confirm that an
  absolute symlink to a host file and `../` traversal fail to resolve.
- Cloud Hypervisor v53.0 opened `path=/proc/self/fd/<n>` as the pod's uid
  and booted the guest. In the VMM's fd table the disk appears as
  `/var/lib/baryon/disks/root/disk.img`, the path inside the container.
- First run failed with `Error creating log file: Permission denied`: the
  VMM user could not traverse the root-owned 0700 `/run/baryon/vms`. Fixed
  by making it 0711 (traversable, not listable).
- Hypothesis 3 as originally written ("kubelet cannot unmount the volume
  while the VMM holds it") is false (architecture review B1): the container
  sees the volume through its own bind mount, which is a different mount
  from kubelet's, so kubelet's unmount succeeds regardless of the VMM. The
  safety property must come from ordering: the VMM must exit before kubelet
  starts volume teardown. Observed ordering (kubelet log against baryon-virtd
  log; the placement was not recorded, but in the third case baryon-virtd,
  not the container runtime, killed the VMM, which matches `pod-child`
  placement):
  - graceful delete: VMM exited 1.5 s after the stop request; kubelet volume
    teardown started afterwards.
  - forced delete (`--grace-period=0 --force`) with a responsive guest:
    kubelet still sent SIGTERM; guest shut down; VMM exited, and
    `UnmountVolume started` for the root disk followed 313 ms later.
  - forced delete with the guest paused (cannot answer ACPI): anchor received
    SIGTERM (t = 0) and requested a final stop; kubelet SIGKILLed it at
    t + 2.02 s; baryon-virtd killed the VMM (exit by signal 9) at
    t + 2.09 s; `UnmountVolume started` at t + 2.29 s.
- Hypothesis 4, on the kind host, with a guest reset through the Cloud Hypervisor
  API (`vm.reboot`): the first attempt failed with `Error creating console
  devices: Address in use`. Cloud Hypervisor's Landlock rules did not allow
  it to remove and re-create its serial socket in its own directory; with
  `--no-landlock` the reset worked, which isolated the cause. Fixed by
  passing `--landlock-rules path=/run/baryon/vms/<pod-uid>/vmm,access=rw`
  (`LaunchConfig.landlock_writable_dirs` in
  `crates/cloud-hypervisor/src/launch.rs`). After the fix the reset returned
  204, the guest booted again (a second kernel boot in the console log),
  received a new DHCP lease from the responder (two acknowledgements in
  `ctl/dhcp.log`), and answered ping. The disk was reopened through
  `/proc/self/fd/N` with Landlock enforced. Hypothesis 4 holds: Tested on
  the kind host and on the Hadron node; the lifecycle E2E includes the
  reset (see [testing.md](testing.md#results)).
- A hard reset shortly after the first boot left the Debian guest without
  SSH host keys: cloud-init's first-boot writes were not yet on disk. The
  E2E therefore checks ping, not SSH, after a reset.

### Failure modes

- History: with `pod-child` placement (removed from baryon-virtd), if
  baryon-virtd is not running while a pod is torn down, nothing kills the VMM
  before volume teardown. With `container` placement under the systemd
  cgroup driver, the container runtime stops the VMM with the container,
  before volume teardown, even when baryon-virtd is down (Experiment C).
- local-path volumes are hostPath volumes: kubelet's "unmount" does not
  unmount anything and there is no NodeStage, so the experiment does not
  exercise CSI detach. A CSI driver with staging is required to close A.

### Security implications

baryon-virtd never opens pod-controlled paths through host directories; the
resolution is confined to the container root, and the VMM reopens the file
with the pod user's credentials, so DAC applies as if the pod opened it.

### Operational implications

- The disk path in Cloud Hypervisor's configuration is `/proc/self/fd/N`. A
  snapshot or migration must re-provide the fd at the same number.
- Disk images must be owned by or accessible to the Anchor user. The importer
  creates them mode 0600 owned by that user.

### Decision

ADR 0006 is Accepted (filesystem mode after these experiments, block mode
after the k0s cluster results). The resolution and reopen mechanism works
for filesystem-mode local-path claims on the kind host and the Hadron node.
With `container` placement (Experiment C), the container runtime stops the
VMM before volume teardown, independent of baryon-virtd and of the CSI
driver; the stop, move, and delete paths are Tested with Ceph RBD, and the
failure paths (SIGKILL, forced delete, baryon-virtd down) with a CSI driver
that stages and attaches are not tested.

### Remaining questions

- Resolved: block-mode PVC device node ownership. kubelet does not apply
  `fsGroup` to raw block devices; under containerd defaults the VM's
  identity cannot open the device (`BlockDeviceAccessDenied`), and with
  `device_ownership_from_security_context = true` the node belongs to the
  pod's `runAsUser` and `runAsGroup` (Tested on the k0s cluster).
- Partly answered: CSI driver with NodeStage and attach. A Ceph RBD volume
  in both modes was detached, staged, and attached on another node during a
  stop and start, and deleted cleanly (Tested on the k0s cluster). Teardown
  ordering under SIGKILL, forced delete, and baryon-virtd down with such a
  driver is still open.
- Resolved: guest reboot reopen with Landlock and seccomp enforced (Tested
  on the kind host and the Hadron node).

## Experiment B: networking handoff

### Problem

CNI configures the pod network inside the Anchor Pod network namespace. A
host-owned Cloud Hypervisor in the host network namespace must use that
network without Baryon reimplementing CNI, and without capabilities.

### Hypothesis

1. A TAP created by baryon-virtd inside the pod network namespace (setns
   through the anchor pidfd) works from Cloud Hypervisor in the host network
   namespace through an inherited fd.
2. tc ingress plus a mirred redirect between `eth0` and the TAP gives the
   guest L2 connectivity as the pod.
3. A DHCP responder can configure an unmodified cloud image.
4. The TAP disappears when the VMM exits.

### Implementation

`crates/virtd/src/handoff/network.rs`, `crates/virtd/src/handoff/dhcp.rs`,
and, since the security review, `crates/virtd/src/dhcp_helper.rs` (the
separate responder process).

### Observations

- TAP `bvm0` (`tun type tap pi off vnet_hdr on persist off`) was created in
  the pod network namespace; Cloud Hypervisor used it through `--net fd=`
  with no capabilities.
- tc configuration as installed (from `tc filter show`): on `bvm0` ingress,
  priority 1 u32 match IPv4 UDP dport 67 with `gact pass`, priority 2 u32
  match-all with `mirred egress redirect dev eth0`; on `eth0` ingress, u32
  match-all with `mirred egress redirect dev bvm0`.
- First design dropped guest DHCP with `gact drop` and expected the packet
  socket to see it first. It did not: device-bound protocol-specific packet
  sockets run after tc ingress in `__netif_receive_skb_core`. Changed to
  `gact pass`, so DHCP reaches the pod network stack and the responder's
  `ETH_P_IP` packet socket on the TAP.
- An unmodified Debian cloud image has cloud-init disabled when no
  datasource is found, so it never runs DHCP. Baryon now sets the SMBIOS
  system serial number to `ds=nocloud;i=<VM UID>;h=<VM name>`; cloud-init
  then enables its fallback network configuration (DHCP on the first NIC)
  and sets the hostname. No metadata server is involved. The serial is passed
  on the Cloud Hypervisor command line; its option parser splits on the
  first `=` only, so the value is safe.
- With DHCP working, the guest acquired the pod IP 5 to 6 s after the VMM
  started. From a pod on the other node the guest answered ping and its own
  sshd banner (`SSH-2.0-OpenSSH_10.0p2 Debian-7+deb13u4`).
- Same-node pods could not reach the guest when the lease carried the pod
  prefix (/24) and router: kindnet routes even same-subnet traffic through
  the gateway (a route for the pod subnet via the gateway address), so the guest ARPed for peers
  directly and nobody answered. Fixed by giving the guest a /32 and the pod
  network namespace's own routes as classless static routes (option 121).
  After the fix, ping and ssh worked from both nodes.
- Persistent disks broke the per-pod MAC: on the second VM start the guest
  NIC stayed down because cloud-init had rendered its configuration bound to
  the previous pod's MAC. Fixed by giving each VM a stable MAC derived from
  the VirtualMachine UID (`02:` plus the last 40 bits of the UID) and setting
  the pod interface to that MAC before boot (veth supports live MAC change,
  `IFF_LIVE_ADDR_CHANGE`; routes are untouched). Two consecutive starts on
  the same disk then both got DHCP and were reachable from both nodes.
- First boot of a fresh disk: the guest is reachable about 10 s after the
  DHCP acknowledgement (cloud-init first-boot work). Readiness based on the
  DHCP acknowledgement therefore precedes reachability on first boot.
- After the VMM exited, `bvm0` was gone; baryon-virtd removed its ingress
  qdiscs from `eth0`.
- Later, on the kind host, the guest was unreachable from other pods for a while
  after `Ready=True` (measurement in [testing.md](testing.md#results)): the
  host side still had a neighbor entry for the pod IP with the CNI-assigned
  MAC, learned during the disk import. Fixed by a gratuitous ARP for the pod
  IP from the pod interface after the MAC change (`announce` in
  `crates/virtd/src/handoff/network.rs`).

### Failure modes

- CNIs that record the pod interface MAC at ADD time and address frames to
  it (for example eBPF datapaths that rewrite the destination MAC) are
  expected to break with the MAC change. Not tested.
- IPv4 only. The guest gets IPv6 addresses only through router
  advertisements from the CNI side, if any.

### Security implications

- During these experiments baryon-virtd parsed guest-sent DHCP packets as
  root. The parser is dhcproto; only one client MAC and one address are
  served. Since then the responder runs as a separate per-VM process with
  the VMM's confinement, and baryon-virtd only opens the packet socket and
  hands it over (`crates/virtd/src/dhcp_helper.rs`; ADR 0007 revision;
  [security.md](security.md)).

### Operational implications

- Kubelet HTTP and TCP probes against the pod IP reach the guest; anchor
  readiness uses exec probes.
- In these experiments the DHCP responder ran inside baryon-virtd. After a
  baryon-virtd restart it was restored when the anchor reattached (observed:
  2 s); renewals in between fail harmlessly because the lease is 24 h. The
  separate responder process exits with baryon-virtd and is started again
  when the anchor reattaches (Tested on the kind host, the Hadron node, and
  the k0s cluster; see
  [testing.md](testing.md#results) and [networking.md](networking.md)).
- Since 2026-10-04 baryon-virtd restarts a failed responder for as long as
  the VM runs, instead of giving up after 10 consecutive failures, and
  reports its state to the cluster as the VM's `NetworkReady` condition
  (ADR 0007 and ADR 0008 revisions; Tested: `tests/e2e/network-health.sh`
  on the k0s cluster, see [testing.md](testing.md#k0s-cluster) and
  [networking.md](networking.md#guest-network-service-health)). It also
  probes the responder's serving loop for liveness over its stdin and
  stdout pipes every 10 s and restarts a responder that answers no probe
  for 25 s (ADR 0007 revision; Tested: unit tests and the hang cases of
  `tests/e2e/network-health.sh` on the k0s cluster).

### Decision

TAP in the pod network namespace with L2 passthrough, stable MAC, and
gratuitous ARP, confirmed on the Hadron node with flannel and recorded in
ADR 0007 (Accepted).

Since these experiments the VMM runs in its own empty network namespace
instead of the host network namespace (ADR 0005 revision); the inherited TAP
fd works the same way (Hadron node results). The DHCP responder runs as a
separate confined process (ADR 0007 revision).

### Remaining questions

- CNI compatibility with the MAC change beyond kindnet and flannel, in
  particular eBPF datapaths.
- Resolved after these experiments: the DHCP responder no longer runs in
  the root process (Tested on the kind host and the Hadron node; see
  [Hadron node results](#hadron-node-results)).

## Experiment C: process ownership

### Problem

Choose between Cloud Hypervisor inside the Anchor container (Model A) and
Cloud Hypervisor owned by baryon-virtd on the host (Model B).

### Hypothesis

1. A host-owned VMM can run with the pod's identity, without capabilities,
   inside the pod's resource limits.
2. It survives a baryon-virtd restart and an anchor container restart.
3. Pod deletion stops it gracefully, and it is killed before volume teardown
   when graceful shutdown is not possible.

### Implementation

`crates/virtd/src/vmm.rs` (spawn, credentials, cgroup), `crates/virtd/src/vm.rs`
(supervision, recovery), `crates/anchor/src/run.rs`.

### Observations

Credentials of the running VMM (from `/proc/<pid>/status`, kind host, before
the security review changes; current values are in
[Hadron node results](#hadron-node-results)):

```
Uid: <vm-uid> (all four)      Gid: <vm-gid> (all four)
Groups: <kvm-gid> <vm-gid>    (kvm group, pod group)
CapPrm: 0  CapEff: 0  CapAmb: 0  CapBnd: full
NoNewPrivs: 1
Seccomp: 2 on every worker thread (vcpu0, vcpu1, vmm, http-server,
         disk0_q0, net0_qp0, net0_ctrl, serial-manager, event-monitor),
         0 on the idle main thread
```

Since then the bounding set is emptied before `setuid`, the VMM gets its own
network namespace, and baryon-virtd requires the anchor, and therefore the
VMM, to run as the VM's host identity (ADR 0011).

History: placement `pod-child` (VMM in `<pod cgroup>/baryon-vmm`). This
placement was removed from baryon-virtd; the observations are kept as the
record of why it was rejected.

- Works under the systemd driver. After deletion the pod cgroup, including the
  empty `baryon-vmm` child, was removed by kubelet and systemd.
- The VMM memory (412 MiB touched guest memory) is charged to the pod cgroup.
- A pod whose init container had no limits was Burstable with pod-level
  `memory.max = max`, so the VMM was unbounded. baryon-virtd now refuses to
  start a VM in a pod without a memory limit (`ResourcesUnbounded`), and every
  Anchor Pod container must set requests equal to limits. With that, the pod
  was Guaranteed with `memory.max = 2415919104` and `cpu.max = 200000 100000`.
- Architecture review B2: creating a cgroup below a systemd-managed pod
  slice is outside systemd's delegation rules, and the CPU and memory
  managers apply cpusets per container, so the VMM does not inherit them. Not
  yet measured.

Placement, `container` (VMM in the Anchor container's cgroup):

- Without a device grant, Cloud Hypervisor failed with
  `CreateHypervisor(VmCreate(Operation not permitted))`: the container's
  device cgroup (eBPF) does not allow /dev/kvm, and it applies to every
  process in that cgroup.
- With baryon-node-agent's device plugin, the Anchor container requests
  `virtualization.baryon.io/vm: 1`; kubelet adds /dev/kvm to its device list
  and mounts the anchor socket directory. The pod spec has no hostPath and no
  node selector. The VMM ran in the container's own scope
  (`kubelet-kubepods-pod<uid>.slice/cri-containerd-<id>.scope`), the guest
  booted and was reachable from both nodes.
- baryon-virtd restart: VMM kept running (same pid).
- anchor SIGKILL: the container runtime killed the VMM 88 ms later as part
  of stopping the container; kubelet restarted the container and a new VM
  booted. With this placement the VM does not survive an anchor container
  restart.
- forced pod deletion with baryon-virtd stopped (nothing from Baryon
  watching), timestamps from an in-node watcher, containerd, and kubelet:

  | Time | Event |
  |---|---|
  | t = 0 | containerd StopContainer begins |
  | t + 80 ms | VMM process gone |
  | t + 112 ms | StopContainer returns |
  | t + 113 ms to t + 218 ms | StopPodSandbox (CNI teardown) |
  | t + 380 ms | kubelet `UnmountVolume started` for the root disk |

  The container runtime stops the VMM with the container, before network
  and volume teardown, independent of baryon-virtd.

Lifecycle, by the placement each observation was made under:

| Event | Placement | Observed |
|---|---|---|
| baryon-virtd restart with a running VM | `pod-child` (history) | VMM kept running (same pid); the new baryon-virtd re-adopted it from its runtime record; the anchor reconnected and reattached. |
| baryon-virtd restart with a running VM | `container` | VMM kept running (same pid) on the kind host and the Hadron node; the DHCP responder was replaced. baryon-virtd used to wait on SIGTERM for open gRPC streams (anchor sessions, node agent watch) until systemd killed it at the 90 s stop timeout whenever it was restarted with VMs running (node reboots were not delayed: the pods and their connections are gone by the time systemd stops it); it now exits 2 s after the signal (`SHUTDOWN_GRACE` in `crates/virtd/src/main.rs`). Timings in [testing.md](testing.md#results). |
| anchor SIGKILL (container crash) | `pod-child` (history) | baryon-virtd saw the anchor exit, found the pod sandbox still alive, kept the VMM; kubelet restarted the container; the new anchor reattached to the same VMM. |
| anchor SIGKILL (container crash) | `container` | The container runtime stopped the VMM with the container (kind host, see above; Hadron node). kubelet restarted the container and a new VM booted. The VM does not survive an anchor container restart. |
| Cloud Hypervisor SIGKILL | not recorded | Reported `Failed`, reason `VmmKilled`, message "VMM was killed by signal 9", visible as the container termination message; container restarted after kubelet back-off; VM booted again with the same MAC and IP. |
| Cloud Hypervisor SIGKILL after a baryon-virtd restart | `container` | Hadron node: the Anchor container restarted with reason `VmmExited`, because a re-adopted VMM is not baryon-virtd's child and its exit status is unknown. |
| pod deletion (graceful) | not recorded | anchor requested a final stop; ACPI power button; guest powered off ("reboot: Power down"); VMM exited shortly after the request; no VMM, runtime directory, or cgroup left. |
| forced deletion, guest unresponsive | `pod-child` (inferred, see Experiment A) | baryon-virtd killed the VMM after the anchor's SIGKILL, before volume teardown started. |
| deletion with baryon-virtd down | `container` | kind host: the timeline above. Hadron node: no VMM, responder, or pod mount left; the guest got no ACPI shutdown; the stale runtime directory was removed when baryon-virtd started again. |

Model A comparison: not run yet. Its requirements are known from the
placement observation above: /dev/kvm in the container (device plugin or
privileged), CAP_NET_ADMIN or a pre-created TAP fd, the VMM binary and
firmware in the container image, and VM death on every container restart.

### Failure modes

- Pod deletion while baryon-virtd is down: handled by the container runtime
  under the systemd cgroup driver (see above); without baryon-virtd the guest
  gets no ACPI shutdown.
- Under the kubelet cgroupfs driver nothing stops the host-spawned VMM before
  volume teardown when baryon-virtd is down: runc kills leftover cgroup
  processes only for containers without a private PID namespace, and kubelet
  kills leftover pod processes only after volume teardown (inferred from
  upstream code, not observed; see ADR 0005). With the systemd
  driver, stopping the container scope does it. baryon-virtd therefore
  refuses to start a VM whose Anchor container cgroup is not a systemd scope
  inside a systemd pod slice (start failure reason
  `CgroupDriverUnsupported`).
- History: `pod-child` placement under the CPU manager static policy
  (review B2).

### Decision

Model B with `container` placement, on the evidence above:

- The VMM binary and firmware come from the host image; the VMM is started
  and supervised by baryon-virtd with the Anchor container's uid, no
  capabilities, `no_new_privs`, seccomp, and Landlock. After the security
  review it also gets an empty capability bounding set and its own empty
  network namespace (ADR 0005 revision of 2026-10-03).
- It runs inside the Anchor container's cgroup, so the pod's limits,
  per-container cpusets, device list, and accounting apply, and the
  container runtime stops it with the container. Teardown ordering does not
  depend on baryon-virtd.
- It survives baryon-virtd restarts. It does not survive an anchor
  container restart; the VM is then started again, with power-loss
  semantics for the guest.
- /dev/kvm and the anchor socket reach the container through the
  `virtualization.baryon.io/vm` device plugin.

`pod-child` placement was removed from baryon-virtd (the
`--vmm-cgroup pod-child` flag no longer exists). It created cgroups under
systemd-managed slices and escaped per-container CPU and memory managers.

The decision is recorded in ADR 0005 (Accepted) and was confirmed on the
Hadron node (k3s, systemd driver) and on the k0s cluster. The kubelet
cgroupfs driver is Not supported (see Failure modes). k0s v1.36.4 does not
use the systemd driver by default: its generated containerd configuration
lacks `SystemdCgroup`, and baryon-virtd refused VMs there with
`CgroupDriverUnsupported` until a containerd drop-in set it (see
[compatibility.md](compatibility.md#k0s-node-configuration)).

### Remaining questions

- Resolved: container stop behavior under the cgroupfs driver. It does not
  stop the VMM before volume teardown, so cgroupfs is refused
  (`CgroupDriverUnsupported`). Open: whether the systemd driver requirement is
  permanent ([design-notes.md](design-notes.md)).
- CPU manager static policy: confirm the VMM inherits the container cpuset.
- Resolved: the bounding set is now emptied in the spawn path (`CapBnd` 0
  measured on the Hadron node).
- Resolved: node reboot behavior on the Hadron node (see
  [Hadron node results](#hadron-node-results)).

## Hadron node results

What the Hadron node (one node, Hadron v0.5.3, k3s v1.36.4+k3s1, flannel)
confirmed, across several Baryon-enabled Hadron images installed by
AuroraBoot and by Kairos A/B upgrades. A summary of the runs is in
[testing.md](testing.md#results).

The Hadron node is a single node. Every pod, including the probe pods, ran
on that node, so cross-node traffic over flannel's vxlan backend and
rescheduling to another node were not exercised.

### Environment

- The first image booted from an AuroraBoot raw EFI disk: the first boot
  ran the recovery system, which installed the active system and rebooted;
  then the active system booted. Later images were installed with
  `kairos-agent upgrade` (Tested, manual procedure).
- `/dev/kvm` appeared shortly after baryon-virtd started, when udev loaded
  `kvm_intel`. Host discovery runs on each node agent request, so this had no
  effect.
- Cluster components and both admission policies were deployed from
  `deploy/`. Both policies work on k3s 1.36.
- A node provisioned from scratch came up with the graceful shutdown
  drop-in from its cloud-config in place and the kubelet's shutdown
  inhibitor active, with no manual step.

### Storage (Tested)

- The importer wrote the raw disk image into a filesystem-mode claim from
  the k3s local-path provisioner, and the guest booted from it through the
  same `O_PATH` and `/proc/self/fd/N` handoff as on the kind host.
- The claim was kept across stop and start, restart, and node reboots, and
  the VM booted from it again each time.
- A guest reset inside the running VMM reopened the disk with Landlock
  enforced.
- Not covered: block mode and a CSI driver with staging and attach.

### Networking with flannel (Tested)

- flannel with the vxlan backend; pod interfaces are veths on the `cni0`
  bridge. The live MAC change of the pod interface was tolerated.
- Ping and SSH from other pods worked after the first boot, after a restart
  with a new pod IP, after a guest reset (ping), and after node reboots.
- The gratuitous ARP made the guest reachable shortly after `Ready=True`.

### Process ownership and placement (Tested)

- baryon-virtd derived the pod UID and the container cgroup from the k3s
  systemd-driver path
  (`/kubepods.slice/kubepods-pod<uid>.slice/cri-containerd-<id>.scope` for the
  Guaranteed Anchor Pod). baryon-virtd runs from the image unit without
  provider-specific flags.
- The VMM ran in the Anchor container's scope, so `container` placement
  (ADR 0005) is confirmed under k3s with the systemd driver. An anchor crash
  stopped the VMM with the container.
- The lifecycle E2E host checks (through `NODE_SHELL`) found no runtime
  directory, VMM process, or DHCP responder process left.

### VMM sandbox and DHCP responder (Tested)

The VMM's measured confinement is in
[security.md](security.md#vmm-confinement): the VM's host identity, the
kvm and pod groups, all capability sets empty, `NoNewPrivs` 1, its own
network namespace with loopback only, seccomp on all threads except the
main thread, and the Anchor container scope. The DHCP responder (after a
node reboot) had the VM's uid and gid without the kvm group,
`CapEff` and `CapBnd` 0, `NoNewPrivs` 1, its own network namespace, and the
Anchor container cgroup.

These values were measured before Baryon's seccomp layer. Since
2026-10-04 baryon-virtd installs its own seccomp filter on both processes
before `execve`, so every responder thread runs under one filter (Baryon's)
and every VMM thread under Baryon's and Cloud Hypervisor's. The exception
was the VMM's main thread with Cloud Hypervisor v53.0, which had Baryon's
filter only; since v54.0 it also has Cloud Hypervisor's (measured on the k0s cluster; see
[security.md](security.md#baryon-seccomp-layer)).

### Recovery and reboot (Tested, manual procedures)

- baryon-virtd restart: the VMM kept running and the DHCP responder was
  replaced.
- VMM crash and anchor crash: the Anchor container restarted and the VM
  booted again.
- Deletion with baryon-virtd stopped: nothing was left on the node; the
  guest got no ACPI shutdown.
- Node reboot without kubelet graceful node shutdown: the guest was stopped
  hard; after boot, kubelet re-admitted the existing Anchor Pod before the
  device plugin had registered, admission failed with
  `UnexpectedAdmissionError`, and baryon-controller replaced the pod (new pod
  UID and IP, same MAC); the new pod stayed Pending until the device plugin
  registered.
- Node reboot with graceful node shutdown: kubelet held a systemd-logind
  delay inhibitor, the guest shut down cleanly, the old Anchor Pod ended
  `Succeeded`, and baryon-controller created a new one.
- Kairos A/B upgrade with a VM running, six times (four on the first
  installation, two after the node was provisioned again from scratch): the
  node booted the new image and the VM was Ready again on a replacement
  Anchor Pod each time. The recovery system was not upgraded.
  Procedure in [kairos-integration.md](kairos-integration.md).
- Log retention: with a temporary 1 s retention age,
  `systemd-tmpfiles --clean` removed the stale per-pod log directories and
  kept the running VM's directory, which baryon-virtd held under a shared
  `flock`.

### Remaining questions

- Forced deletion of a paused guest on a Hadron node.
- Resolved on the k0s cluster: cross-node traffic between Baryon nodes and
  VM placement on another node after a stop and start (see
  [k0s cluster results](#k0s-cluster-results)). The `IdentityConflict`
  replacement path is still not exercised on a cluster.
- Resolved: the reboot performed by a Kairos Operator `NodeOpUpgrade`
  honors the kubelet shutdown inhibitor (k0s cluster).
- Resolved: k0s does not use the systemd cgroup driver by default; a
  containerd drop-in sets it.

## k0s cluster results

What three Baryon-enabled Hadron nodes running k0s v1.36.4+k0s.1 (the k0s
cluster in [testing.md](testing.md#results); kube-router, OpenEBS LVM
LocalPV, Ceph RBD) added to the evidence above:

- Storage: block-mode and filesystem-mode root disks on LVM LocalPV and
  Ceph RBD, imported and booted through the same `O_PATH` and
  `/proc/self/fd/N` handoff. Ceph RBD block devices appeared in the Anchor
  container mode 0660 and LVM LocalPV devices mode 0600, owned by the VM's
  identity with containerd `device_ownership_from_security_context`. A VM
  on Ceph RBD moved to another node through CSI detach, NodeStage, and
  attach in both modes and booted from its existing disk.
- baryon-virtd's disk checks refused a block device node that a pod had
  created as `disk.img` on a filesystem volume (`StorageFailed`, wrong disk
  kind), and no VMM started.
- Networking: one probe pod reached VMs on all three nodes, and a Service
  followed a VM to its replacement Anchor Pod.
- Process ownership: with the containerd drop-in, Anchor containers ran in
  systemd scopes and VMs started; before it, every VM was refused with
  `CgroupDriverUnsupported`.
- Recovery: baryon-virtd restarts and outages and VMM crashes on one node
  left the VMs on the other nodes untouched (multi-node E2E). Node reboots,
  manual Kairos A/B upgrades, and Kairos Operator upgrades of one node at a
  time stopped guests cleanly; the VMs started again on the same node or,
  with Ceph RBD, on another node.
