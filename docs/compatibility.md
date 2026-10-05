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
| Kubernetes 1.30 or later for the admission policies, 1.32 or later for `baryon-node-agent-own-node` | ValidatingAdmissionPolicy (`admissionregistration.k8s.io/v1`, GA in 1.30); the node name in pod-bound service account tokens (`authentication.kubernetes.io/node-name`). [security.md](security.md) makes the policies part of the supported installation. | ADR 0009, [security.md](security.md) | Verified on 1.34 (kind host) and 1.36 (k3s, Hadron node) for `baryon-vm-resource` and `baryon-node-agent-own-node`; on 1.36 (k0s API server in CI) for the `baryon-vm-resource` denials and every allow and deny case of `baryon-controller-writes` and `baryon-import-approval` |
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
| Dynamic provisioners set `pv.kubernetes.io/provisioned-by` on the PersistentVolumes they create | Without an import record, Baryon imports only into a volume dynamically provisioned for its claim (ADR 0006); a volume without the annotation is refused (`DiskNotProvisionedForClaim`). | Verified with OpenEBS LVM LocalPV and Ceph RBD block volumes (import integrity E2E, k0s cluster). Other drivers: Inferred from upstream code (sig-storage-lib-external-provisioner, used by the CSI external-provisioner, sets it to the StorageClass provisioner in `controller/controller.go`); a provisioner that does not use it must set the annotation itself |
| Storage tooling preserves PersistentVolume annotations | The import record `virtualization.baryon.io/disk-import` lives on the PersistentVolume. Backup, restore, or migration tooling that drops it makes a used block claim refuse to start (`ImportStateUnknown`) while the VM status still marks the claim as used; if the status is lost too, a volume that looks fresh can be imported again. | Open (no backup or restore tool tested) |
| Thin-provisioned block storage: room for the full size of every imported volume, newly provisioned blocks zeroed | A block import zeroes or writes the whole device; dm-thin has no write-zeroes support, so an import allocates the whole volume. | Inferred from the kernel's dm-thin target; not tested |

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
versioned runtime protocol of ADR 0012. API changes and their upgrade
steps are listed per release in [api-stability.md](api-stability.md#change-log-per-release);
for 0.1.0-rc1 see [Upgrade to 0.1.0-rc1](#upgrade-to-010-rc1).

| Rule | Status |
|---|---|
| A cluster release and a host image interoperate when they share the protocol major version, in any combination of minor versions | Tested: version skew E2E on the k0s cluster, below |
| Hosts of another major version are reported `ProtocolIncompatible` on the VirtualizationNode, their `virtualization.baryon.io/vm` slots become unhealthy, and running VMs continue | Implemented, unit tests |
| A cluster release supports Anchor Pods of the previous release that are still running, and hosts of the same major version | Tested for 1.0 Anchor Pods under the 1.1 cluster release |
| No VM requires a feature beyond the 1.0 set until feature-aware placement exists | Designed (ADR 0012); placement by feature is Planned |
| The minor version only decides whether optional status is available, never whether a peer or a VM is accepted. With a host before 1.2, a running VM's `NetworkReady` is Unknown with reason `RuntimeDoesNotReport` | Implemented, unit tests |
| A baryon-controller of the network health change with a baryon-node-agent of an earlier release, as during a cluster release rollout: the node agent publishes no `status.vms`, so a running VM's `NetworkReady` is Unknown, `NotReported` on 1.2 hosts and `RuntimeDoesNotReport` on earlier hosts, until the DaemonSet has rolled. A baryon-controller of an earlier release ignores `status.vms` and sets no `NetworkReady` | Implemented; not tested |
| Host-side safety fixes (disk path checks, the block device registry in baryon-virtd) are not negotiated | Until releases are tagged, run a host image built from the same commit as the cluster release or later |

Minimum host image per cluster release:

| Cluster release | Protocol | Minimum host image |
|---|---|---|
| a73f37f (first published commit) | 1.0 | a73f37f |
| 597b637 to b0413aa | 1.1 | protocol: any 1.x host; host-side safety fixes: an image built from the same commit as the cluster release or later |
| 29c512c (the network health change) | 1.2 | protocol: any 1.x host; `NetworkReady` needs a 1.2 host (earlier hosts give Unknown, `RuntimeDoesNotReport`); host-side safety fixes: an image built from the same commit as the cluster release or later |

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

Guest network health across releases: the cluster release of the network
health change needs the updated CRD (VirtualizationNode `status.vms`) and
the controller RBAC that adds `get`, `list`, and `watch` on
virtualizationnodes; `kubectl apply --server-side -k deploy/base` installs
both. The new node agent refuses `--vm-slots` outside 1 to 256 and
`--status-interval-seconds` outside 5 to 30, so a DaemonSet that set other
values must be changed before the upgrade.

VM placement across releases: Anchor Pods of an earlier release have no
`virtualization.baryon.io/placement-hash` annotation; baryon-controller
treats one as matching a VM without placement, so an upgrade alone does not
report `PlacementChanged`. They also have no
`virtualization.baryon.io/vm-label-keys` annotation: the first label sync
adds the VM's labels and removes none. The label sync needs the
`baryon-controller-writes` policy of the same release; with the earlier
policy the API server denies it, baryon-controller logs a warning, and the
VM is not affected.

Import records across releases: since this change, the authoritative
record of a completed import is the annotation
`virtualization.baryon.io/disk-import` on the bound PersistentVolume (ADR
0006). Earlier releases recorded imports differently, and baryon-controller
migrates them:

- The published release a73f37f imports only into filesystem-mode claims
  and records the import with the marker file. Such a claim gets an import
  Pod once if its volume passes the freshness check; the importer finds the
  marker, leaves the disk unchanged, and the record is written on the
  volume (observed on the k0s cluster before this change, also during the
  rolling Kairos Operator upgrade, when the record was still a claim
  annotation). A volume that does not pass the check is refused
  (`DiskNotProvisionedForClaim`); adopt it with
  `rootDisk.source.persistentVolumeClaim`.
- Commits c8e6c08 to 1835c0c recorded the import in the claim annotation
  `virtualization.baryon.io/imported-from`, as the URL or as `sha256:` and
  the hex SHA-256 of the URL, for filesystem-mode and block-mode claims.
  baryon-controller no longer writes it. When a VirtualMachine has no
  record on its volume, the claim annotation equals the current source
  digest or URL, and the status written by the earlier controller shows
  StorageReady=True for the same claim name with a transition no earlier
  than the claim's creation, baryon-controller writes the PersistentVolume
  record and treats the disk as imported (`DiskBindingPending` until claim
  and volume are bound to each other). Such a claim, including a block
  device, is not imported again.
- A block-mode VirtualMachine of an earlier release whose status shows a
  ready disk but whose claim annotation does not match is refused
  (`ImportStateUnknown`), never imported again.
- Upgrading requires the new RBAC (PersistentVolume `get` and `patch`, pod
  `patch`) and the policies `baryon-controller-writes` and
  `baryon-import-approval`; `kubectl apply --server-side -k deploy/base`
  installs them. A VirtualMachine's status must be kept for the migration:
  a VirtualMachine of an earlier release restored without status has no
  evidence of its import.

## Upgrade to 0.1.0-rc1

0.1.0-rc1 is the release of the v0.1 API review; the API stays `v1alpha1`
and the runtime protocol stays 1.2. The API changes are listed in
[api-stability.md](api-stability.md#010-rc1-not-yet-released). Upgrade in
this order:

1. Apply the new CRDs:

   ```bash
   kubectl apply --server-side -f deploy/base/crds/virtualization.baryon.io.yaml
   ```

2. Apply every VirtualMachineClass again with `spec.cpu.vcpus` in place of
   `spec.cpu.cores`. A class stored in the earlier shape shows no vCPUs:

   ```bash
   kubectl get virtualmachineclasses
   ```

   Edit each class manifest (`cores: 2` becomes `vcpus: 2`) and apply it,
   for example `kubectl apply -f examples/classes.yaml` for the example
   classes. The API server accepts this one update of a class stored in
   the earlier shape; after it, the class is immutable. A class whose
   `memory.size` is not a whole number of Mi from 64Mi to 16Ti is
   rejected and needs a valid size.

3. Upgrade baryon-controller and baryon-node-agent:

   ```bash
   kubectl apply --server-side -k deploy/base
   ```

What changes for running VMs and nodes:

- Running VMs keep running through all three steps. A VM that starts
  while its class is still in the earlier shape reports `InvalidClass`
  until the class is applied again.
- Between steps 1 and 3, baryon-controller of the earlier release reads
  classes with `cpu.cores`, which the new CRD no longer has, so its
  reconciles of VMs fail and no VM starts or stops. Inferred from the
  types of the earlier release; not tested. Keep the steps close
  together.
- Between steps 1 and 3, baryon-node-agent of the earlier release cannot
  read VirtualizationNodes either: its types require `runtime.protocol`
  and `runtime.hypervisor`, which the API server now drops when it reads
  the objects. Until the new node agents have rolled out, heartbeats stop
  on every node and the Heartbeat column of `kubectl get
  virtualizationnodes` grows; the device plugins keep working, so running
  VMs and their slots are not affected. Inferred from the types of the
  earlier release; not tested. This is another reason to run the three
  steps in this order and quickly.
- VirtualizationNode status loses `runtime.protocol`, `runtime.hypervisor`,
  `architecture`, `kernelRelease`, `osImage`, `kairosVersion`, `cpu`,
  `memoryBytes`, `kvm`, `iommu`, `vfio`, `numaNodes`, `hugepages`, and
  `pciDevices`. The API server drops them from stored objects when it
  reads them. Scripts or dashboards that read these fields must read the
  Node object (`status.nodeInfo`, `status.capacity`) or the node agent's
  `host inventory` log line instead
  ([api.md](api.md#host-inventory)).
- A baryon-controller of an earlier release cannot read a
  VirtualizationNode written by the new node agent, which no longer
  writes `runtime.protocol` and `runtime.hypervisor`; step 3 upgrades
  both together. Inferred from the types of the earlier release; not
  tested.
- The `Ready` True reason of VirtualizationNode is `RuntimeReady`
  (before: `KvmAvailable`). Anything that matches the reason must be
  updated; matching the status is enough to tell whether the node is
  ready.
- Anchor Pods created by a `restartedAt` restart request of an earlier
  release still match and are not reported as `RestartRequired`. Anchor
  Pods created with `placement: {}` by an earlier release still match a
  VM with `placement: {}` or without placement.
- Root disk claims that exist are not changed. New block-mode claims
  request exactly `rootDisk.size` instead of 64 MiB more.

Status: the upgrade steps are Designed; an upgrade run on the k0s cluster
is pending.
