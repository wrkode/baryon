# 0007: TAP in the pod network namespace with L2 passthrough

Status: Accepted. Validated with kindnet on a historical development
cluster in which a kind node container acted as the host (the "kind host",
see [runtime-boundary.md](../runtime-boundary.md), Experiment B) and with
flannel (k3s default) on a single Baryon-enabled Hadron node (the "Hadron
node"). The Hadron node is one node, so traffic there was same-node through
the `cni0` bridge; cross-node flannel (vxlan) is not tested.
Date: 2026-10-02, revised 2026-10-03 and 2026-10-04

## Context

The VM network is the Anchor Pod network (ADR 0004). CNI configures an
interface, usually `eth0`, inside the pod network namespace and assigns the
pod IP. Kubernetes Services, NetworkPolicies, and DNS address that IP.

Cloud Hypervisor accepts a pre-opened TAP fd. The TAP may live in any
network namespace. Hadron has no `tc` or `bridge` binaries; netlink is used
directly.

CNIs differ: some route even same-subnet traffic through the pod gateway,
some put pods on a shared bridge, some address frames to a MAC recorded at
CNI ADD. Guest images bind their network configuration to the NIC MAC.

## Decision

- baryon-virtd enters the pod network namespace on a dedicated thread using
  the anchor pidfd, opens `/dev/net/tun`, and creates a non-persistent TAP
  `bvm0` with `IFF_TAP | IFF_NO_PI | IFF_VNET_HDR`. Its fd is the only
  reference; the TAP disappears when Cloud Hypervisor exits.
- Each VM has a stable MAC address derived from its VirtualMachine UID
  (`02:` and the last 40 bits of the UID). baryon-virtd sets the pod
  interface to that MAC before the guest starts (veth supports a live address
  change; routes are untouched), and the guest NIC uses it. The guest takes
  the pod IP. Right after the change baryon-virtd broadcasts a gratuitous ARP
  for the pod IP from the pod interface, so peers that cached the CNI-assigned
  MAC (the host side of the CNI, which talked to the pod during the disk
  import) update at once instead of after their neighbor entry expires.
- tc on both interfaces: an ingress qdisc and a u32 match-all filter with a
  mirred egress redirect to the other interface. On the TAP, a higher-priority
  u32 filter matching IPv4 UDP to port 67 uses `gact pass`, so guest DHCP is
  not redirected to the cluster network but delivered to the pod network
  stack.
- Guest DHCP is answered by a responder process that baryon-virtd starts per
  VM with the VMM's confinement (per-VM uid, no capabilities, empty network
  namespace), on a packet socket bound to the TAP that baryon-virtd opens and
  hands over. baryon-virtd itself never parses guest frames. The lease
  carries the pod IP with a /32 mask, the pod network namespace's own
  IPv4 routes as classless static routes (option 121, with host routes for
  off-link gateways), the router, MTU, DNS servers and search domains from
  the pod's resolv.conf, and the VM name as hostname.
- baryon-virtd restarts a responder that exits for as long as the VM runs,
  with backoff from 1 s, doubling to at most 60 s. The count of consecutive
  failures resets after a responder ran for 5 minutes. It reports the
  responder's state: Ready when a responder has started, Restarting after
  a failure, and Failing from the 11th consecutive failure on. The state
  reaches the cluster as the VM's `NetworkReady` condition (ADR 0008,
  runtime protocol 1.2 in ADR 0012). The responder reports at most one
  lease acknowledgement per 60 s, and baryon-virtd keeps the time of the
  last one in the VM's runtime record.
- The SMBIOS system serial number is `ds=nocloud;i=<VM UID>;h=<VM name>`,
  which enables cloud-init's NoCloud datasource and its fallback DHCP
  configuration in cloud images without a metadata server.
- When the VM stops, baryon-virtd removes the qdiscs it added.
- VMs are selected by their own labels. baryon-controller copies the
  VirtualMachine's `metadata.labels`, except reserved keys, to the Anchor
  Pod and the import Pod when it creates them, and keeps them in sync on
  the running Anchor Pod without a restart. Reserved keys are
  `app.kubernetes.io/managed-by` and keys whose prefix is `baryon.io`,
  `kubernetes.io`, or `k8s.io`, or a subdomain of one of them other than
  `app.kubernetes.io`, so recommended labels such as
  `app.kubernetes.io/name` propagate. The Anchor Pod's annotation
  `virtualization.baryon.io/vm-label-keys` lists the copied keys, and a
  sync removes only those, never a reserved key (users can edit the
  annotation); labels that others set on the pod stay. The import Pod's
  labels are set at creation and not synced. It has a readiness probe that
  never passes, so Services that select the VM's labels never route to it,
  unless a Service sets `publishNotReadyAddresses: true`.

## Consequences

- Services and cluster DNS reach the guest unchanged: the lifecycle E2E
  reaches the guest through a ClusterIP Service by DNS name, also after the
  Anchor Pod is replaced (Tested on the kind host and the Hadron node). NetworkPolicy
  applies to the pod IP and so to the guest, and guest-initiated (egress)
  traffic leaves through the pod interface like pod traffic; both are
  Inferred, not tested.
- The DHCP responder exits with baryon-virtd and is restarted only while an
  anchor is attached (the pod network namespace is reached through the
  anchor's pidfd). The responder is not re-adopted like the VMM: the new
  baryon-virtd starts a new responder for a re-adopted VM when its anchor
  reattaches, and restores the time of the last acknowledgement from the
  runtime record. A running guest keeps its 24 h lease across the restart;
  a guest that reboots, resets, or renews while baryon-virtd is down gets
  no answer until the anchor has reattached
  ([design-notes.md](../design-notes.md)).
- A guest whose responder keeps failing keeps its address until its lease
  expires; `NetworkReady` is False from the first failure (within one node
  agent interval) and its message
  gives the lease expiry. A responder that hangs without exiting is not
  detected: baryon-virtd sees only its exit (pidfd).
- The guest routes exactly like the pod: same-node and cross-node traffic
  both worked with kindnet, which routes the pod subnet through the gateway.
  With flannel (a bridge, `cni0`), guest ping and SSH from other pods worked,
  including after node reboots and Anchor Pod replacement with a new pod IP.
- Without the gratuitous ARP, the guest was unreachable from other pods for
  tens of seconds after the VM became Ready on the kind host (the host still had a
  neighbor entry for the old MAC). With it, the guest answered at or shortly
  after Ready on the kind host and the Hadron node (measurements in
  [testing.md](../testing.md#results)).
- Kubelet HTTP and TCP probes against the pod IP reach the guest; Anchor
  readiness uses exec probes.
- The VM IP changes when the Anchor Pod is recreated; the MAC does not.
- Services, NetworkPolicies, pod affinity and anti-affinity, and topology
  spread constraints select a VM by the labels on its VirtualMachine, like
  pods of a workload controller. The label copy, sync, and removal are
  Tested by the placement E2E on the k0s cluster, and spread constraints
  over VM labels by the same E2E. Reaching a guest through a Service that
  selects VM labels is Inferred: the tested Service path selects
  `virtualization.baryon.io/vm-name`.
- A NetworkPolicy that selects a VM's labels also selects its import Pod,
  so it applies to the importer's downloads: an egress-deny policy for VMs
  also blocks their HTTP imports. NetworkPolicies add up, so a second
  policy that selects `virtualization.baryon.io/import-for` and allows
  egress to the image server lets the import run.
- A Service with `publishNotReadyAddresses: true` that selects VM labels
  also routes to the import Pod while an import runs.
- Migration note: live migration is Not supported (ADR 0013). If a
  migration design is accepted, two pods of one VM would exist during a
  migration and would carry the same labels. The recorded rule for that
  case is that only the active pod carries the VM's labels, and they move
  at switchover.
- CNI compatibility requirements: the pod interface is an Ethernet device
  supporting live MAC change; the CNI tolerates the MAC change after ADD and
  learns guest MACs by ARP; it does not need the pod's own kernel to answer on
  the pod interface. CNIs that address frames to a MAC recorded at ADD time are
  expected to fail; not tested.
- IPv4 DHCP only. IPv6 reaches the guest only through router advertisements
  from the CNI side, if any.
- Readiness from the DHCP acknowledgement precedes full guest reachability on
  a disk's first boot by about 10 s (cloud-init first-boot work).
- Pod-network live migration needs CNI support for keeping the IP and MAC
  across pods; it is not a property this design provides. ADR 0013 lists a
  CNI contract for keeping the pod IP, or an explicit decision that the
  guest's address changes, as an entry criterion.

## Alternatives considered

- Per-pod MAC (the CNI-assigned one). Implemented first; rejected because a
  persistent disk's network configuration is bound to the first MAC and the
  guest NIC stayed down on the next start.
- Dropping guest DHCP in tc and reading it from the packet socket first.
  Implemented first; does not work: device-bound protocol-specific packet
  sockets run after tc ingress.
- Pod prefix and router in the lease. Implemented first; same-node peers
  were unreachable on a routed CNI.
- Linux bridge in the pod network namespace. Rejected: the bridge and the
  pod interface would need the same MAC or the CNI-facing MAC would change
  anyway, with more namespace changes than tc.
- NAT from a private guest subnet. Rejected: the guest would not own the pod
  IP, and Service and NetworkPolicy semantics change.
- Cloud Hypervisor creating the TAP. Rejected: CAP_NET_ADMIN in the VMM.

## Revisions

- 2026-10-03: gratuitous ARP after the MAC change, added after the kind host
  showed a 30 to 50 s reachability gap; validation on the Hadron node with flannel;
  the DHCP responder moved out of the root process after the security
  review; after the architecture gate, the Service path was tested,
  NetworkPolicy and egress were marked Inferred, and the responder's
  lifetime consequences were recorded.
- 2026-10-03: VM labels are copied to the Anchor Pod and the import Pod
  and kept in sync, so Services and policies select VMs by their own labels
  (reserved keys excepted, removal limited to the keys Baryon copied); the
  import Pod's never-passing readiness probe and the migration rule for
  labels were recorded. After the architecture review: reserved keys in the
  user-writable label-keys annotation are ignored, the import Pod's labels
  are not synced, an egress-deny NetworkPolicy on VM labels blocks imports,
  and `publishNotReadyAddresses` Services reach the importer.
- 2026-10-04: baryon-virtd restarts a failed DHCP responder for as long as
  the VM runs, instead of not restarting it after 10 consecutive failures.
  After 10 consecutive failures it reports the responder as failing
  (`NetworkReady=False`, `DhcpResponderFailing`) and keeps restarting it
  every 60 s. Rationale: the responder holds no state; without it the guest
  loses its address when its lease (86400 s) expires; and each restart is a
  fresh exec with the VMM's confinement (the VM's unprivileged host
  identity, the VMM's cgroup). Accepted exposure: a guest that can crash
  the responder's parser gets a new responder about once a minute for as
  long as the VM runs, instead of 11 in total
  ([security.md](../security.md#guest-attack-surface)). Also recorded: the
  responder reports at most one acknowledgement per 60 s, baryon-virtd
  keeps the last acknowledgement time in the runtime record, the responder
  state is reported as `NetworkReady` (ADR 0008), and the question whether
  the responder must be re-adopted after a baryon-virtd restart is answered
  (it is not; a new one is started when the anchor reattaches).
- 2026-10-04: the migration lines now link ADR 0013 (live migration Not
  supported) and its guest address entry criterion. The decision is
  unchanged.
