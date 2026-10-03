# 0007: TAP in the pod network namespace with L2 passthrough

Status: Accepted. Validated with kindnet on a historical development
cluster in which a kind node container acted as the host (the "kind host",
see [runtime-boundary.md](../runtime-boundary.md), Experiment B) and with
flannel (k3s default) on a single Baryon-enabled Hadron node (the "Hadron
node"). The Hadron node is one node, so traffic there was same-node through
the `cni0` bridge; cross-node flannel (vxlan) is not tested.
Date: 2026-10-02, revised 2026-10-03

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
- The SMBIOS system serial number is `ds=nocloud;i=<VM UID>;h=<VM name>`,
  which enables cloud-init's NoCloud datasource and its fallback DHCP
  configuration in cloud images without a metadata server.
- When the VM stops, baryon-virtd removes the qdiscs it added.

## Consequences

- Services and cluster DNS reach the guest unchanged: the lifecycle E2E
  reaches the guest through a ClusterIP Service by DNS name, also after the
  Anchor Pod is replaced (Tested on the kind host and the Hadron node). NetworkPolicy
  applies to the pod IP and so to the guest, and guest-initiated (egress)
  traffic leaves through the pod interface like pod traffic; both are
  Inferred, not tested.
- The DHCP responder exits with baryon-virtd and is restarted only while an
  anchor is attached (the pod network namespace is reached through the
  anchor's pidfd). A guest that reboots or resets while baryon-virtd is down
  gets no address until baryon-virtd is back; after 10 consecutive responder
  failures baryon-virtd stops restarting it, and the guest keeps its 24 h
  lease. Whether the responder should be re-adopted like the VMM is an open
  question ([design-notes.md](../design-notes.md)).
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
  across pods; it is not a property this design provides.

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
