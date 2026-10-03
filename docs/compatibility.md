# Compatibility contract

Baryon does not depend on how a cluster or node was provisioned (manual
Kairos install, AuroraBoot, Kairos Fleet, Cluster API, or anything else). It
depends on the requirements below. Each one names the decision that creates
it and how it has been verified so far.

Verification states: **Verified** (observed on a named environment),
**Inferred** (from upstream code or documentation, not observed), **Open**
(not checked).

Environments: "k0s cluster" means three Baryon-enabled Hadron nodes (node1
to node3, VMs with nested KVM) running k0s, the integration environment;
"Hadron node" means one Baryon-enabled Hadron node running k3s; "kind host"
means a historical development cluster in which a kind node container acted
as the host, no longer used and not a Baryon environment. All are described
in [testing.md](testing.md#results).

## Certified configuration

None yet. The first certified configuration is planned to be:

| Component | Version |
|---|---|
| Hadron | v0.5.x (default kernel, not `cloud`) |
| Kairos | v4.3.x |
| Kubernetes provider | k3s v1.36.x through `kairos-init -p k3s` |
| CNI | flannel (k3s default) |
| CSI | local-path (k3s default) |
| Cloud Hypervisor | v53.0 (static build) |
| UEFI firmware | cloud-hypervisor/edk2 `ch-97eeb7b09` CLOUDHV.fd |
| virtiofsd | v1.14.0 (shipped, not used yet) |

## Kubernetes distributions

| Distribution | State |
|---|---|
| k0s | Tested on three Baryon-enabled Hadron nodes (VMs with nested KVM): Hadron v0.5.3 (kernel 7.2.6-hadron), Kairos v4.3.0, k0s v1.36.4+k0s.1 with one controller that also runs workloads and two workers, the k0s default CNI (kube-router; no network provider set in the k0s configuration), OpenEBS LVM LocalPV and Ceph RBD, systemd cgroup driver, Cloud Hypervisor v53.0. Needs the containerd drop-in and worker profile described in [k0s node configuration](#k0s-node-configuration). Nested virtualization on three nodes is evidence, not a certification. CI also runs a single-node k0s v1.36.4+k0s.1 API server in a container, which validates only API server behavior (CRD schema, CEL rules, admission policies); it is not a Baryon host. |
| k3s | Tested on a single Baryon-enabled Hadron node (VM with nested KVM): Hadron v0.5.3 (kernel 7.2.6-hadron), Kairos through kairos-init v4.3.0, k3s v1.36.4+k3s1, flannel (vxlan backend, `cni0` bridge), local-path, systemd cgroup driver, Cloud Hypervisor v53.0. Multi-node behavior with k3s is not tested. |
| kubeadm (provider-kubernetes) | not tested |
| kind | not a Baryon environment. A development cluster in which a kind node container acted as the host was used for the runtime experiments and early E2E runs; it is no longer used. |

## Host requirements

"k0s cluster" and "Hadron node" evidence was taken on Hadron v0.5.3 with
kernel 7.2.6-hadron; "kind host" evidence on a host kernel 6.8.

| Requirement | Why | Source | Verification |
|---|---|---|---|
| x86_64. aarch64 is Planned. | Binaries and firmware built for x86_64 only. | ADR 0002 | Verified (kind host, Hadron node) |
| `/dev/kvm` usable, KVM API 12 | VMs need KVM. | ADR 0002 | Verified (kind host and Hadron node, both nested). On the Hadron node, `/dev/kvm` appeared shortly after baryon-virtd started, when udev loaded `kvm_intel`; discovery runs on each node agent request, so this had no effect. |
| Linux 6.5 or later | `SO_PEERPIDFD` for race-free peer identification. | ADR 0005 | Verified on 6.8 (kind host) and 7.2.6-hadron (Hadron node) |
| Landlock ABI 3 (Linux 6.2 or later) | Cloud Hypervisor `--landlock`, plus `--landlock-rules` granting read-write access to the VM's own `vmm` runtime directory, which a guest reset needs to re-create the serial socket. | ADR 0005 | Verified on 6.8 and 7.2.6-hadron, including a guest reset under Landlock on both |
| cgroup v2, unified hierarchy | Pod cgroup derivation, container placement. | ADR 0005 | Verified (kind host, Hadron node) |
| kubelet systemd cgroup driver; cgroupfs is Not supported | Only stopping the Anchor container's systemd scope stops the host-spawned VMM before volume teardown while baryon-virtd is down. Under cgroupfs, runc kills leftover cgroup processes only for containers without a private PID namespace, and kubelet kills leftover pod processes only after volume teardown (inferred from upstream code, not observed). baryon-virtd refuses a VM whose Anchor container cgroup is not a systemd scope inside a systemd pod slice (`CgroupDriverUnsupported`). | ADR 0005 | Verified with the systemd driver on the kind host, on k3s (Hadron node), and on k0s (k0s cluster). k0s v1.36.4 does not use it by default; see [k0s node configuration](#k0s-node-configuration). Before the fix, baryon-virtd refused every VM on those nodes with `CgroupDriverUnsupported` |
| Kernel modules: kvm, tun, sch_ingress, cls_u32, act_mirred, act_gact | TAP and tc passthrough. | ADR 0007 | Verified (kind host, Hadron node); the image lists them in `/usr/lib/modules-load.d/baryon.conf` except the KVM vendor module, which udev loads |
| Host ids 1000000000 to 1999999999 not assigned to other users or groups | Reserved for per-VM host identities. | ADR 0011 | Open (not checked on Hadron) |
| Network namespaces available to baryon-virtd (`unshare(CLONE_NEWNET)`) | Each VMM starts in an empty network namespace. | ADR 0005 | Verified (kind host, Hadron node) |
| baryon-virtd running as root in the host PID and mount namespace | It must see Anchor processes in `/proc` and enter their namespaces. | ADR 0005 | Verified (kind host, inside the node container; Hadron node, as a systemd unit) |
| `/run` is a tmpfs | Runtime records must not survive a reboot. | ADR 0003 | Inferred from the Kairos layout documentation; not checked on a node |
| `systemd-tmpfiles` that skips directories under a BSD lock (tmpfiles.d(5)) and runs its clean timer | Per-VM log retention: stale log directories are removed after 14 days, the directory of a running VM is kept because baryon-virtd holds a shared `flock` on it. | [hadron.md](hadron.md) | Verified on the Hadron node with a short override age |
| kubelet graceful node shutdown, with `shutdownGracePeriod` minus `shutdownGracePeriodCriticalPods` at least 90 s | A reboot that does not go through Kubernetes still stops Anchor Pods first, so guests shut down cleanly. The Anchor Pod termination grace period is 90 s. Without it, guests are stopped hard at reboot. | [kairos-integration.md](kairos-integration.md) | Verified on the Hadron node (k3s, KubeletConfiguration drop-in, 150 s and 30 s) and on the k0s cluster (k0s worker profile, 150 s and 30 s, delivered by Kairos cloud-config); see [hadron.md](hadron.md) for the configuration. On both, a node reboot with a running VM shut the guest down cleanly |

Baryon cannot observe whether graceful node shutdown is configured on a
node: the requirement is not checked at runtime. A VirtualizationNode
condition that reports it is Planned.

## Kubernetes requirements

A supported installation needs Kubernetes 1.32 or later. The CRDs alone
need 1.29; the admission policies, which are part of the supported
installation ([security.md](security.md)), need 1.30 for
ValidatingAdmissionPolicy and 1.32 for the node name in service account
identities.

| Requirement | Why | Source | Verification |
|---|---|---|---|
| Kubernetes 1.32 or later for a supported installation | The admission policies below. | [security.md](security.md) | Verified on 1.34 (kind host) and 1.36 (k3s, Hadron node) |
| Kubernetes 1.29 or later for the CRDs | CRD validation rules (`x-kubernetes-validations`, GA in 1.29). Client types are generated for 1.36 (k8s-openapi `latest`) and only stable fields are used. | ADR 0008 | Verified on 1.34 (kind host), 1.36 (k3s v1.36.4+k3s1, Hadron node), and 1.36 (k0s v1.36.4+k0s.1 API server in CI) |
| Kubernetes 1.30 or later for the admission policies, 1.32 or later for `baryon-node-agent-own-node` | ValidatingAdmissionPolicy (`admissionregistration.k8s.io/v1`, GA in 1.30); the node name in pod-bound service account tokens (`authentication.kubernetes.io/node-name`). [security.md](security.md) makes the policies part of the supported installation. | ADR 0009, [security.md](security.md) | Verified on 1.34 (kind host), 1.36 (k3s, Hadron node), and 1.36 (k0s API server in CI, `baryon-vm-resource` denial only) |
| kubelet with one cgroup per pod (`cgroupsPerQOS`, default) | Pod identity from cgroup paths. | ADR 0005 | Verified (systemd driver: kind host, Hadron node) |
| kubelet cgroup driver systemd | See Host requirements; cgroupfs is Not supported. baryon-virtd still parses both path forms to identify the pod. | ADR 0005 | Verified (kind host, Hadron node) |
| containerd as the CRI runtime, with runc | Stopping the container (its systemd scope) must kill every process in the container cgroup. Block mode: the runtime must create granted device nodes on the container's `/dev` tmpfs. | ADR 0005, ADR 0006 | Verified with containerd 2.1 (kind host), the containerd embedded in k3s v1.36.4+k3s1 (Hadron node), and the containerd shipped with k0s v1.36.4+k0s.1 (k0s cluster), all with runc and the systemd driver. CRI-O and crun: Open |
| containerd `device_ownership_from_security_context = true` for block-mode root disks | kubelet does not apply `fsGroup` to raw block devices; with this setting containerd gives each device node of a pod that sets `runAsUser` that user and group, so the VM's host identity can open its block disk. The setting is node-wide: it applies to every device of every such pod, including device plugin devices, so /dev/kvm in Anchor containers is owned by the VM's identity. | ADR 0006, ADR 0011 | Verified on the k0s cluster (Ceph RBD device nodes mode 0660, LVM LocalPV mode 0600). Without it a block-mode VM fails with `BlockDeviceAccessDenied`. Filesystem mode does not need it |
| kubelet device plugins enabled, plugin directory mountable | Scheduling gate (`virtualization.baryon.io/vm` and `virtualization.baryon.io/import`), /dev/kvm, socket delivery. | ADR 0004, ADR 0009 | Verified (`/var/lib/kubelet/device-plugins`, kind host, Hadron node, k0s cluster) |
| Pod Security level for VM namespaces: restricted is sufficient | Anchor Pods use no hostPath, capabilities, or host namespaces. | ADR 0004 | Verified on the kind host: Anchor Pods admitted in a namespace with `pod-security.kubernetes.io/enforce: restricted` (version latest) |
| `baryon-system` namespace allows privileged pods | baryon-node-agent mounts host paths. | ADR 0009 | Verified (kind host, Hadron node) |

After a node reboot, kubelet re-admits pods before the device plugin has
registered again. An Anchor Pod that survived the reboot then fails admission
(`UnexpectedAdmissionError`), and baryon-controller replaces it with a new
Anchor Pod, which stays Pending until the device plugin registers. Observed
on the Hadron node; see [kairos-integration.md](kairos-integration.md).

The kubelet root directory differs between providers (k3s and kubeadm:
`/var/lib/kubelet`; k0s: `/var/lib/k0s/kubelet`), but the kubelet's device
plugin directory is fixed at `/var/lib/kubelet/device-plugins` whatever the
root directory is (verified on k0s), so the node agent's manifest is the
same for all of them. CSI node plugins, in contrast, must be configured with
the kubelet root (on k0s, `/var/lib/k0s/kubelet`).

## k0s node configuration

Found on the k0s cluster with k0s v1.36.4+k0s.1 (Verified):

- k0s v1.36.4 generates a containerd configuration without `SystemdCgroup`
  for runc, and the 1.36 kubelet takes its cgroup driver from the CRI
  runtime. Pods therefore landed in cgroupfs paths even with
  `cgroupDriver: systemd` in the worker profile, and baryon-virtd refused
  VMs with `CgroupDriverUnsupported`. A containerd drop-in in
  `/etc/k0s/containerd.d/`, which k0s merges into its configuration, fixes
  this and also sets the device ownership that block mode needs:

  ```toml
  # /etc/k0s/containerd.d/baryon.toml
  version = 3
  [plugins."io.containerd.cri.v1.runtime"]
    device_ownership_from_security_context = true
  [plugins."io.containerd.cri.v1.runtime".containerd.runtimes.runc.options]
    SystemdCgroup = true
  ```

  On the k0s cluster it was added after provisioning, followed by a
  reboot; writing it from Kairos cloud-config at provisioning
  ([hadron.md](hadron.md#node-configuration)) is the intended form and is
  not yet tested from first boot.
- The kubelet cgroup driver and graceful node shutdown
  (`shutdownGracePeriod: 150s`, `shutdownGracePeriodCriticalPods: 30s`) are
  set in a k0s worker profile in the k0s `ClusterConfig`, delivered by
  Kairos cloud-config, not baked into the image.
- The kubelet root is `/var/lib/k0s/kubelet`; the device plugin directory
  stays `/var/lib/kubelet/device-plugins`.

## CNI requirements

| Requirement | Why | Verification |
|---|---|---|
| Pod interface is an Ethernet device (veth) in the pod network namespace | TAP passthrough at L2. | Verified (kindnet; flannel with the `cni0` bridge; kube-router) |
| Live MAC change of the pod interface is supported and tolerated after CNI ADD | Stable VM MAC (ADR 0007). | Verified (kindnet; flannel vxlan with the `cni0` bridge; kube-router) |
| The CNI learns pod MACs by ARP; it does not address frames to a MAC recorded at ADD | Same. baryon-virtd broadcasts a gratuitous ARP after the change so that existing neighbor entries update at once. | Verified (kindnet, flannel, kube-router); eBPF datapaths expected to fail, Open |
| The pod's own kernel does not need to answer on the pod interface | All ingress is redirected to the guest. | Verified (kindnet, flannel, kube-router) |
| IPv4 pod address | DHCP responder is IPv4 only. | Verified (kindnet, flannel, kube-router) |

kindnet was verified on the kind host, including traffic from a pod on
another node (Experiment B). kube-router, the k0s default, was verified on
the k0s cluster, including traffic from one probe pod to VMs on all three
nodes and through a ClusterIP Service (multi-node E2E). flannel was
verified on a single Hadron node, so cross-node traffic over its vxlan
backend is Open. Other CNIs are not tested.

## CSI requirements

| Requirement | Why | Verification |
|---|---|---|
| Filesystem-mode RWO claims for imported disks | Importer writes `disk.img`. | Verified (local-path, which is a hostPath provisioner, on the kind host and the Hadron node; OpenEBS LVM LocalPV v1.10.1 and Ceph RBD through Rook v1.20.8 on the k0s cluster) |
| Block-mode claims: device node accessible to the VM's host identity | The VMM reopens the device as that identity; kubelet does not apply `fsGroup` to raw block devices, so access is governed by the device ownership the container runtime sets for the pod (ADR 0011). | Verified with containerd `device_ownership_from_security_context = true` (OpenEBS LVM LocalPV, Ceph RBD; k0s cluster) |
| Drivers with NodeStage and attach | Teardown ordering under real detach. | Verified for stop, move to another node, and deletion with Ceph RBD (k0s cluster). SIGKILL, forced delete, and baryon-virtd down: Open |
| One running VM per claim | Baryon refuses a second VM on a claim (`ClaimInUse`). `ReadWriteMany` block claims shared across nodes are Not supported. | Verified for `ClaimInUse` (storage E2E, k0s cluster) |

## Guest requirements

| Requirement | Why | Verification |
|---|---|---|
| UEFI-bootable raw disk image | Firmware boot with CLOUDHV.fd. | Verified (Debian 13 genericcloud) |
| DHCP client on the first NIC, or cloud-init (NoCloud via SMBIOS serial enables its fallback DHCP) | Guest addressing. | Verified (Debian 13 genericcloud) |
| Serial console on ttyS0 (optional) | Console login prompt as boot evidence and console log. | Verified (Debian 13 genericcloud) |

## Version skew

baryon-virtd ships in the host image and is upgraded with the OS (Kairos);
baryon-controller, baryon-node-agent, and the anchor image are the cluster
release, upgraded with the cluster deployment. They communicate through the
versioned runtime protocol of ADR 0012.

| Rule | Status |
|---|---|
| A cluster release and a host image interoperate when they share the protocol major version, in any combination of minor versions | Tested: version skew E2E on the k0s cluster, below |
| Hosts of another major version are reported `ProtocolIncompatible` on the VirtualizationNode, their `virtualization.baryon.io/vm` slots become unhealthy, and running VMs continue | Implemented, unit tests |
| A cluster release supports Anchor Pods of the previous release that are still running, and hosts of the same major version | Tested for 1.0 Anchor Pods under the 1.1 cluster release |
| No VM requires a feature beyond the 1.0 set until feature-aware placement exists | Designed (ADR 0012); placement by feature is Planned |
| Host-side safety fixes (disk path checks, the block device registry in baryon-virtd) are not negotiated | Until releases are tagged, run a host image built from the same commit as the cluster release or later |

Minimum host image per cluster release:

| Cluster release | Protocol | Minimum host image |
|---|---|---|
| a73f37f (first published commit) | 1.0 | a73f37f |
| 597b637 and later | 1.1 | protocol: any 1.x host; host-side safety fixes: an image built from the same commit as the cluster release or later |

Evidence on the k0s cluster, 2026-10-03 (version skew E2E,
`tests/e2e/skew.sh`, one VM per node, a baryon-virtd restart on every node):

- 1.0 cluster release with mixed 1.0 and 1.1 hosts: 31 of 31 checks.
- 1.1 cluster release with mixed hosts: 31 of 31. New anchors on 1.0 hosts
  took the UNIMPLEMENTED path of `Negotiate` and attached.
- A 1.0 anchor reattached to a 1.1 baryon-virtd after a restart.
- VMs of the old release kept running through the cluster release upgrade,
  with `RestartRequired=True` (`AnchorImageChanged`).
- The node agent published `protocolVersion` 1.0 for old hosts and 1.1 with
  features for new ones.

Import records across releases: the published release a73f37f imports
only into filesystem-mode claims and records the import with the marker
file. Claims from it get an import Pod once, which finds the marker, leaves
the disk unchanged, and records the digest (observed on the k0s cluster for
filesystem claims without a digest record, also during the rolling Kairos
Operator upgrade). Commits c8e6c08 to 597b637, published together with
this change, recorded the source URL itself in the `imported-from`
annotation, for filesystem-mode and block-mode claims. baryon-controller
accepts an annotation equal to the current URL as a completed import and
rewrites it to the `sha256:` digest (`ensure_claim` in
`crates/controller/src/reconcile.rs`), so such a claim, including a block
device, is not imported again.
