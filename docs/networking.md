# Networking

Status: Tested by the multi-node E2E on three Baryon-enabled Hadron nodes
running k0s with the k0s default CNI, kube-router (the "k0s cluster"), and
by the lifecycle E2E on a single Baryon-enabled Hadron node running k3s with
flannel (vxlan backend, `cni0` bridge), the "Hadron node", and on a
historical development cluster in which a kind node container acted as the
host, with kindnet, the "kind host" (no longer used); summary in
[testing.md](testing.md#results). Decision: ADR 0007. Experiment B in
[runtime-boundary.md](runtime-boundary.md) records how the design was
reached.

| Guest network path | Status |
|---|---|
| From another pod to the pod IP (SSH banner, ping) | Tested on the k0s cluster, the Hadron node, and the kind host |
| Through a ClusterIP Service, by DNS name, also after the VM moved to a new Anchor Pod | Tested on the k0s cluster, the Hadron node, and the kind host |
| Cross-node traffic | Tested on the k0s cluster (kube-router): one probe pod reached VMs on all three nodes; observed on the kind host (kindnet) in Experiment B; not tested with flannel |
| NetworkPolicy applying to the guest as to the pod | Inferred from the design (the guest traffic is the pod's traffic), not tested |
| Guest-initiated egress (to other pods, Services, outside the cluster) | Inferred, not tested |
| Other CNIs | not tested |

Baryon has no SDN. A VM uses the network of its Anchor Pod, configured by the
cluster's CNI. The guest takes over the pod's IP address, so a Service that
selects the Anchor Pod (label `virtualization.baryon.io/vm-name=<vm>`)
reaches the guest, and cluster DNS names resolve to it.

## Data path

```
       other pods / Services / outside
                    |
               (CNI datapath)
                    |
   host side of veth (CNI-owned)
                    |
+-------------------|------------------------------- pod network namespace
|                 eth0 (pod interface, MAC = VM MAC, pod IP)
|                   |  tc ingress: u32 match all -> mirred redirect to bvm0
|                   v
|                 bvm0 (TAP, created by baryon-virtd)
|                   ^  tc ingress: UDP dport 67 -> gact pass (to the DHCP responder)
|                   |              everything else -> mirred redirect to eth0
+-------------------|-----------------------------------------------------
                    |  TAP fd inherited by Cloud Hypervisor
                    |  (the VMM runs in its own empty network namespace)
                    v
            virtio-net in the guest (MAC = VM MAC)
```

## Who does what

| Step | Process | Mechanism |
|---|---|---|
| Pod interface and IP | CNI plugin (kubelet) | standard CNI ADD |
| Enter the pod network namespace | baryon-virtd, dedicated thread | `setns` with the anchor's pidfd |
| Create the TAP | baryon-virtd | `/dev/net/tun`, `TUNSETIFF` (`IFF_TAP`, `IFF_NO_PI`, `IFF_VNET_HDR`), non-persistent |
| Set the VM MAC on the pod interface | baryon-virtd | rtnetlink, live address change |
| tc configuration | baryon-virtd | rtnetlink: ingress qdiscs, u32 filters, mirred and gact actions |
| Open the DHCP socket | baryon-virtd | packet socket bound to `bvm0`, opened in the pod network namespace (needs CAP_NET_RAW) |
| Announce the VM MAC | baryon-virtd | gratuitous ARP request for the pod IP, broadcast from the pod interface through a packet socket, at the end of the network handoff (after the MAC change and tc configuration, before the VMM starts) |
| Use the TAP | Cloud Hypervisor | inherited fd (`--net fd=`), no capabilities, from its own network namespace |
| Guest addressing | DHCP responder, one process per VM started by baryon-virtd (`baryon-virtd dhcp-responder`, shown as `baryon-dhcp`) | serves the inherited packet socket; runs with the VMM's confinement (the VM's host identity, no capabilities, `no_new_privs`, empty network namespace, Anchor container cgroup), without the kvm group |
| Remove tc configuration | baryon-virtd | on VM stop, if the pod still exists |
| Remove the TAP | kernel | when Cloud Hypervisor's fd closes |
| Pod network teardown | CNI plugin (kubelet) | standard CNI DEL, after the VMM is gone |

## Guest configuration

baryon-virtd builds the lease from the pod network namespace and the pod's
`/etc/resolv.conf` and passes it to the responder. The lease contains:

- the pod IP with a /32 mask
- the pod network namespace's IPv4 routes on the pod interface as classless
  static routes (option 121), on-link routes first, plus host routes for
  off-link gateways; the router option for clients without option 121
- the MTU of the pod interface
- DNS servers and search domains from the pod's `/etc/resolv.conf`
- the VM name as hostname
- a 24 hour lease; renewals are answered while the VM runs

Cloud images need a DHCP client on their first NIC. For cloud-init images
Baryon sets the SMBIOS system serial number to
`ds=nocloud;i=<VM UID>;h=<VM name>`, which enables the NoCloud datasource and
its fallback DHCP configuration. The instance id is the VirtualMachine UID,
so per-instance first-boot actions run once per VM.

## Gratuitous ARP

Before the MAC change, the pod used the CNI-assigned MAC, for example during
the disk import. Peers that talked to the pod then, such as the host side of
the pod's veth, keep a neighbor entry with the old MAC. Without an
announcement, the guest stayed unreachable from other pods after
`Ready=True` until those entries expired; with the gratuitous ARP it was
reachable at or shortly after `Ready=True` (Tested on the Hadron node and
the kind host; measurements in [testing.md](testing.md#results)). If sending the
announcement fails, baryon-virtd logs a warning and continues.

## VMM network namespace

baryon-virtd starts Cloud Hypervisor in a new network namespace
(`unshare(CLONE_NEWNET)` before the uid change, `crates/virtd/src/vmm.rs`).
That namespace has only a loopback device, which is down. The TAP device
lives in the pod network namespace; the VMM reaches it only through the
inherited descriptor, which keeps working across namespaces. The VMM
therefore has no network path of its own to host, node-local, or cluster
addresses. Tested: on the Hadron node the VMM's network namespace differed
from the host's and contained only loopback. See [security.md](security.md).

## Identity

| Property | Stable across | Changes when |
|---|---|---|
| MAC address | VM restarts, stop and start, node changes, node reboots | never (derived from the VirtualMachine UID) |
| IP address | VM restarts within one Anchor Pod | a new Anchor Pod is created (stop and start, restart request, rescheduling, node reboot) |

## Limits

- One interface on the pod network. Multus, SR-IOV, VFIO NICs, vhost-user,
  and vDPA are Not supported.
- IPv4 DHCP only.
- CNI requirements are in [compatibility.md](compatibility.md).
- The DHCP responder is a child of baryon-virtd and exits when its stdin
  closes, so it never outlives the baryon-virtd that started it. It is
  restarted only while an anchor is attached and alive, because the pod
  network namespace is reachable only through the anchor pidfd. A guest
  reboot or reset while baryon-virtd is down therefore gets no address until
  baryon-virtd is back and the anchor has reattached; a running guest keeps
  its 24 h lease.
- A responder that exits early is restarted with backoff (up to 60 s). After
  10 consecutive failures it is not restarted for that VM (only logged), and
  the guest keeps its current lease. The failure count resets only when a
  responder ran for 5 minutes, not on reattach.
- The separate responder process is Tested on the kind host (confinement,
  crash restart, one new responder after a baryon-virtd restart, a new lease
  after a guest reset) and on the Hadron node (confinement after a node
  reboot, replacement after a baryon-virtd restart); see
  [testing.md](testing.md#results).
