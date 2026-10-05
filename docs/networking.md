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
| Through a ClusterIP Service, by DNS name, also after the VM moved to a new Anchor Pod | Tested on the k0s cluster, the Hadron node, and the kind host, with a Service that selects `virtualization.baryon.io/vm-name`; a Service that selects the VM's own labels is Inferred (see [Selecting VMs by their labels](#selecting-vms-by-their-labels)) |
| Cross-node traffic | Tested on the k0s cluster (kube-router): one probe pod reached VMs on all three nodes; observed on the kind host (kindnet) in Experiment B; not tested with flannel |
| NetworkPolicy applying to the guest as to the pod | Inferred from the design (the guest traffic is the pod's traffic), not tested |
| Guest-initiated egress (to other pods, Services, outside the cluster) | Inferred, not tested |
| Other CNIs | not tested |

Baryon has no SDN. A VM uses the network of its Anchor Pod, configured by the
cluster's CNI. The guest takes over the pod's IP address, so a Service that
selects the Anchor Pod (label `virtualization.baryon.io/vm-name=<vm>`)
reaches the guest, and cluster DNS names resolve to it.

## Selecting VMs by their labels

baryon-controller copies the VirtualMachine's labels, except reserved keys,
to its Anchor Pod and import Pod, and keeps them in sync on the running
Anchor Pod without a restart (ADR 0007). A Service, a NetworkPolicy, pod
affinity, or a spread constraint can therefore select VMs by their own
labels:

```yaml
apiVersion: v1
kind: Service
metadata:
  name: web
spec:
  selector:
    app: web          # a label on the VirtualMachines
  ports:
    - port: 22
```

- Reserved keys are not copied: `app.kubernetes.io/managed-by`, and keys
  whose prefix is `baryon.io`, `kubernetes.io`, or `k8s.io`, or a
  subdomain of one of them other than `app.kubernetes.io`.
  `app.kubernetes.io/name` and the other recommended labels are copied.
- A sync removes only the labels Baryon copied (listed in the Anchor Pod's
  annotation `virtualization.baryon.io/vm-label-keys`); labels that others
  set on the pod stay.
- The import Pod carries the same labels, set when it is created and not
  synced during the import. Its readiness probe never passes, so a Service
  does not route to it, except a Service with
  `publishNotReadyAddresses: true`, which also publishes the importer.
- A NetworkPolicy that selects the VM's labels also applies to the import
  Pod and so to the image download: an egress-deny policy for VMs also
  blocks HTTP imports. NetworkPolicies add up, so a second policy that
  selects `virtualization.baryon.io/import-for` and allows egress to the
  image server lets the import run.
- A user who edits the `vm-label-keys` annotation cannot make a sync remove
  a reserved label: baryon-controller ignores reserved keys in it.
- The label copy, sync, and removal are Tested by the placement E2E on the
  k0s cluster. Guest reachability through a Service that selects VM labels
  is Inferred; the tested Service path selects `vm-name`.

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

## Guest network service health

baryon-virtd supervises each VM's DHCP responder through its pidfd and
through liveness probes (`crates/virtd/src/vm.rs`,
`crates/virtd/src/dhcp_helper.rs`). Status: Tested by unit tests and by
the E2E test `tests/e2e/network-health.sh` on the k0s cluster with
ceph-rbd and lvm-local, covering a restart after one kill,
`DhcpResponderFailing` under repeated kills, a guest without an address
while its responder was killed from the start, Anchor Pod replacement, a
baryon-virtd restart, the node agent removed from the VM's node, and a
node reboot ([testing.md](testing.md#k0s-cluster)). Hang detection is
Tested by unit tests and by the cases hang, hang-repeated, and
before-lease (which hangs the responder with SIGSTOP instead of killing it)
of the same script on the k0s cluster with ceph-rbd and lvm-local.

Health contract, three levels:

| Level | Evidence | Measured by baryon-virtd |
|---|---|---|
| The process exists | its pidfd | yes |
| The process is responsive | an answer to a liveness probe from the DHCP serving loop | yes |
| The network infrastructure is usable: the guest can get or renew a lease, and the pod network passes its traffic | none, apart from the time of the last lease acknowledgement | no |

Liveness probes:

- The control channel is the responder's existing stdin and stdout pipes;
  there is no extra socket. Every 10 s baryon-virtd writes a line
  `{"probe":<n>}` to the responder's stdin. The pipe is non-blocking on
  baryon-virtd's side, and a probe line is at most 31 bytes, below
  `PIPE_BUF`, so a write either completes or fails; a failed write counts
  as a missing answer.
- The responder's DHCP serving loop answers each probe between packets
  with an `Alive` event on stdout, whether or not the guest sends
  anything. A responder whose loop is stuck stops answering although its
  process exists.
- baryon-virtd accepts answers only to probes it sent to the current
  responder. A responder that answers no probe for 25 s is killed through
  its pidfd and restarted with the restart backoff below, and the hang
  counts as a failure. A hang is therefore detected within about 35 s.
- Any responder output that breaks the protocol (a line longer than
  4 KiB, an unparseable line, an acknowledgement for another address, an
  answer to a probe that was not sent, or a second answer to one probe)
  makes baryon-virtd stop reading, kill the responder, and restart it
  with backoff. baryon-virtd logs one warning per restart, so its log
  output per responder is bounded by the restart backoff.
- A killed responder is waited for at most 60 s. While the VM stops, no
  probes are sent and no responder is restarted.
- Probe answers are a health signal, not a security control: a compromised
  responder can answer probes without serving the guest
  ([security.md](security.md#guest-attack-surface)).

Restarts and reported state:

- A responder that exits, hangs, or breaks its protocol while the VM runs
  is restarted for as long as the VM runs, with backoff from 1 s, doubling
  to at most 60 s. The count of consecutive failures resets after a
  responder ran for 5 minutes (for a hung responder, until its last probe
  answer), not when the anchor reattaches.
- State reported by baryon-virtd (runtime protocol 1.2): `Ready` once a
  responder has answered its first liveness probe (after a failure: once a
  new responder has kept answering for 25 s, so a responder that answers
  once and hangs again does not show `Ready` between its failures),
  `Restarting` after a failure, and `Failing` from the 11th consecutive
  failure on. Until then the VM shows `NetworkReady` Unknown
  (`NotReported`) on a first start, or keeps `Restarting` or `Failing`. The `Restarting` and
  `Failing` messages name the cause: the responder stopped, stopped
  answering liveness probes, or wrote output that breaks its protocol.
  baryon-virtd also reports the time of the last lease acknowledgement and
  the lease expiry (acknowledgement plus 86400 s), and clears the state
  when the VM stops.
- The responder reports at most one acknowledgement per 60 s to
  baryon-virtd and logs repeats within that interval at debug level only;
  baryon-virtd processes at most one acknowledgement per 60 s. The
  reported lease expiry can therefore be up to 60 s earlier than the
  guest's own.
- baryon-virtd keeps the time of the last acknowledgement in the VM's
  runtime record and restores it after its own restart; the new
  baryon-virtd starts a new responder when the anchor reattaches.
- baryon-node-agent publishes the state in its VirtualizationNode
  (`status.vms`, by Anchor Pod UID), and baryon-controller turns it into the
  VM's `NetworkReady` condition ([api.md](api.md#networkready)). The VM's
  `Ready` condition is not affected: a guest that has its address keeps it
  until the lease expires, and the `NetworkReady` message gives that time.

Limits:

- The state `Ready`, and with it `NetworkReady=True`, means that the
  responder runs and its serving loop answers liveness probes, not that it
  served a request, that the guest can renew its lease, or that the pod
  network passes traffic.
- The node agent reports every 30 s by default (at most 30 s), which adds
  up to that much latency to `NetworkReady`.
- The responder log (`ctl/dhcp.log` in the VM's runtime directory, see
  [development.md](development.md#places-on-a-baryon-node)) is not
  size-capped while the VM runs. Repeated acknowledgements are logged only
  at debug level, so the guest's requests add at most one line per 60 s at
  the default log level; each responder start adds a line.
- During a cluster release rollout, a new baryon-controller with a node
  agent of the earlier release reports `NetworkReady` Unknown until the
  DaemonSet has rolled ([compatibility.md](compatibility.md#upgrade-order)).

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
  its 24 h lease. See [Guest network service health](#guest-network-service-health)
  for restarts while the VM runs.
- The separate responder process is Tested on the k0s cluster (restart
  after kills, replacement after a baryon-virtd restart and after a node
  reboot, by `tests/e2e/network-health.sh`) and on the Hadron node
  (confinement after a node reboot, replacement after a baryon-virtd
  restart, a new lease after a guest reset); see
  [testing.md](testing.md#results).
