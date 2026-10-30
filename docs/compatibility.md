# Compatibility contract

Baryon does not depend on how a cluster or node was provisioned (manual
Kairos install, AuroraBoot, Kairos Fleet, Cluster API, or anything else). It
depends on the requirements below. The [support matrix](#v01-support-matrix)
says which versions v0.1 is tested with. The requirement tables after it
say what Baryon needs from a node and a cluster; each requirement names
the decision that creates it and how it has been verified so far.

Verification states in the requirement tables: **Verified** (observed on a
named environment), **Inferred** (from upstream code or documentation, not
observed), **Open** (not checked).

Environments: "k0s cluster" means three Baryon-enabled Hadron nodes (node1
to node3, VMs with nested KVM) running k0s, the integration environment;
"Hadron node" means one Baryon-enabled Hadron node running k3s; "kind host"
means a historical development cluster in which a kind node container acted
as the host, no longer used and not a Baryon environment. All are described
in [testing.md](testing.md#results).

## v0.1 support matrix

What the v0.1 series (0.1.0-rc1 and later 0.1 releases) is tested with.
Support claims follow tests: only rows marked Tested are support claims,
even where the architecture does not depend on the component. Expected
and Not tested rows make no claim. States:

- **Tested**: observed on the k0s cluster with the results in
  [testing.md](testing.md#results), unless the row names another
  environment.
- **Expected**: not tested; expected to work for the reason given.
- **Not tested**: no evidence either way.
- **Not supported**: known not to work, or excluded for v0.1, for the
  reason given.

| Component | Version | State |
|---|---|---|
| Baryon | 0.1.0-rc3 (workspace version 0.1.0-rc3, API `v1alpha1`, runtime protocol 1.2) | Tested on the k0s cluster in its release qualification, which the release gate requires before the tag, from a clean checkout of the tagged commit itself, with the 0.1.0-rc2 and the 0.1.0-rc3 host images; results recorded after the tag in [v0.1.0-rc3 release qualification](testing.md#v010-rc3-release-qualification), and the published artifacts, including an installation from the public release without credentials, in its smoke test. 0.1.0-rc2 (the same API, protocol, and host runtime): Tested on the k0s cluster in its [release qualification](testing.md#v010-rc2-release-qualification), with builds from a clean checkout of the tagged commit itself (df7de3b): upgrade from the published v0.1.0-rc1 artifacts with VMs running, host upgrades of all three nodes from Cloud Hypervisor v53.0 to v54.0 with Kairos Operator, the E2E suites with ceph-rbd and lvm-local, guest policy, and `tests/e2e/repeat.sh`. The published release artifacts, rebuilt by the release workflow from the tag, passed a smoke test after the tag ([testing.md](testing.md#published-v010-rc2-artifacts-smoke-test)). 0.1.0-rc1 (the same API and protocol, Cloud Hypervisor v53.0): Tested on the k0s cluster in the [v0.1.0-rc1 release validation](testing.md#v010-rc1-release-validation), with release-like builds of c3a3735, which differs from the release commit only in the release workflow and documentation: upgrade from a release-like build of 8db33bb, host upgrades of all three nodes with Kairos Operator, rollback cases, uninstall and reinstall from the release files, and the E2E suites lifecycle, node agent outage, storage (ceph-rbd Block, lvm-local Filesystem), import integrity (lvm-local and ceph-rbd), multi-node, placement, hostile disk, deletion, skew with all hosts on one build, and network health with node reboot (ceph-rbd and lvm-local): 536 of 536 checks. `tests/e2e/repeat.sh` was not run. The published release artifacts, rebuilt by the release workflow from the tag, passed a smoke test after the tag ([testing.md](testing.md#published-artifacts-smoke-test)) |
| Hadron | v0.5.3 (`ghcr.io/kairos-io/hadron:v0.5.3`, pinned by digest in `packaging/versions.env`), default kernel variant | Tested. The `cloud` kernel variant: Not supported, it lacks VFIO, VHOST_NET, NET_SCHED, and Landlock ([hadron.md](hadron.md)). Other Hadron releases: Not tested |
| Kairos | kairos-init v4.3.0 (`quay.io/kairos/kairos-init:v4.3.0`, pinned by digest in `packaging/versions.env`), image built with `hack/build-hadron-image.sh` from `packaging/hadron` | Tested. Other Kairos releases: Not tested |
| Linux kernel | 7.2.6-hadron (the Hadron v0.5.3 default kernel) | Tested. Other kernels that meet the [host requirements](#host-requirements) (6.5 or later, Landlock ABI 3, cgroup v2): Not tested on a Baryon host |
| Cloud Hypervisor | v54.0, static binary, SHA-256 pinned in `packaging/versions.env`, with nested virtualization off and the virtio-net offloads on written explicitly (ADR 0005, ADR 0007) | Tested: the E2E suites except version skew on the k0s cluster ([testing.md](testing.md#cloud-hypervisor-v540)). v0.1.0-rc1 ships v53.0 (Tested in its release validation). Other versions: Not tested; the Baryon-enabled image ships only the pinned build |
| UEFI firmware | CLOUDHV.fd from cloud-hypervisor/edk2 `ch-97eeb7b09`, SHA-256 pinned in `packaging/versions.env` | Tested |
| virtiofsd | v1.14.0 | Shipped in the image, not used by Baryon |
| Architecture | x86_64 | Tested. aarch64: Not supported, binaries and firmware are built for x86_64 only (aarch64 support is Planned) |
| k0s | v1.36.4+k0s.1, one controller that also runs workloads and two workers | Tested. v1.36.4+k0s.1 is also the minimum 1.36 patch: it is the first k0s 1.36 release with the fix for GHSA-qjgc-wmg7-3jc3. Needs the containerd drop-in and worker profile in [k0s node configuration](#k0s-node-configuration). See [Distribution support policy](#distribution-support-policy) for k3s and kubeadm |
| Kubernetes | 1.36.4 (from k0s v1.36.4+k0s.1) | Tested. Other minor versions: Not tested; see [Kubernetes support policy for v0.1](#kubernetes-support-policy-for-v01) |
| Container runtime | containerd 2.3.5 and runc 1.4.3 (bundled with k0s v1.36.4+k0s.1) | Tested, with `SystemdCgroup = true` and `device_ownership_from_security_context = true`. CRI-O and crun: Not tested |
| kubelet cgroup driver | systemd | Tested. cgroupfs: Not supported; baryon-virtd refuses the VM (`CgroupDriverUnsupported`) |
| CNI | kube-router v2.10.0 (the k0s default; no network provider set in the k0s configuration) | Tested, including traffic between nodes and through a ClusterIP Service |
| Other CNIs | any | Not tested; must meet the [CNI requirements](#cni-requirements). Datapaths that address frames to the MAC recorded at CNI ADD, such as some eBPF datapaths, are expected to fail |
| Cluster DNS | CoreDNS 1.14.7 (the k0s default) | Tested (Service DNS name from a probe pod) |
| Filesystem-mode root disk claims | ReadWriteOnce | Tested: storage E2E with OpenEBS LVM LocalPV (last with the API review change) and Ceph RBD (last with the change set committed as 4d2b5df to 1835c0c) |
| Raw block-mode root disk claims | ReadWriteOnce | Tested: storage E2E with Ceph RBD and import integrity E2E with OpenEBS LVM LocalPV and Ceph RBD, last with the API review change; storage E2E with OpenEBS LVM LocalPV last with the change set committed as 4d2b5df to 1835c0c. Needs containerd `device_ownership_from_security_context = true` |
| Ceph RBD | Rook v1.20.8 (`rook/ceph:v1.20.8`), Ceph 19.2.3 (`quay.io/ceph/ceph:v19.2.3`), ceph-csi v3.17.1 (`quay.io/cephcsi/cephcsi:v3.17.1`) and ceph-csi-operator v1.0.4 (`quay.io/cephcsi/ceph-csi-operator:v1.0.4`) through the ceph-csi-drivers chart 1.0.4 | Tested: filesystem and block mode, ReadWriteOnce, and a move to another node with CSI detach and attach. Builds: see the two rows above; the move is part of every Ceph RBD storage E2E run (`storage.sh` moves the VM when its volume is not bound to a node), and testing.md names it for the c8e6c08 runs |
| OpenEBS LVM LocalPV | lvm-driver 1.10.1 (`openebs/lvm-driver:1.10.1`) | Tested: filesystem and block mode, node-local, `WaitForFirstConsumer`. A VM on it starts again only on its node |
| CephFS | any | Not tested |
| local-path provisioner | k3s default | Not tested with v0.1; it ran only on the single-node k3s Hadron node with an earlier release (historical evidence) |
| Other CSI drivers | any | Expected to work if they meet the [CSI requirements](#csi-requirements): Baryon consumes claims only through the Anchor Pod and the import Pod, as ordinary pod volumes. Not tested |
| `ReadWriteMany` block claims shared across nodes | any | Not supported: one running VM per claim (`ClaimInUse`) |
| Kairos Operator | v0.2.2, `NodeOpUpgrade` | Tested: the upgrade of all three nodes, one `NodeOpUpgrade` per node, with the current [runbook](kairos-integration.md#runbook-upgrade-one-node-with-kairos-operator) including its step 5 check, in the v0.1.0-rc1 release validation; see also the [lifecycle contract](kairos-integration.md#lifecycle-contract). One `NodeOpUpgrade` for several nodes completed in one test, with overlapping node outages (with v0.2.2, `concurrency` does not prevent them); use one `NodeOpUpgrade` per node. Other versions: Not tested |
| Guest OS | Debian 13 genericcloud raw image (amd64) | Tested |
| Other Linux guests | any | Expected to work if they meet the [guest requirements](#guest-requirements) (UEFI-bootable raw image, DHCP client or cloud-init). Not tested |
| Windows guests | any | Not supported; not tested |

## Not supported in v0.1

- Live migration is currently unsupported because Baryon's network
  handoff model requires destination-side file descriptor injection
  during Cloud Hypervisor migration, which is not available in the
  currently supported Cloud Hypervisor release. See [ADR 0013](adr/0013-live-migration.md). A
  drain or node upgrade stops the VMs on the node; they start again where
  their storage allows ([lifecycle contract](kairos-integration.md#lifecycle-contract)).
- Windows guests.
- GPU passthrough and other VFIO or PCI device passthrough; SR-IOV.
- Snapshots and VM cloning.
- Confidential computing (SEV-SNP, TDX, attestation).
- Hugepage-backed guest memory and NUMA placement.
- Nested virtualization inside guests.
- More than one disk or one network interface per VM.
- Disk image formats other than raw (qcow2, compressed images).
- aarch64.
- The kubelet cgroupfs driver.
- Kubernetes distributions other than k0s: k3s is Not supported for v0.1;
  kubeadm is Not tested ([Distribution support policy](#distribution-support-policy)).
- Multi-cluster management: Baryon manages the VMs of one cluster.

## Kubernetes support policy for v0.1

- Tested minor version: 1.36, as k0s v1.36.4+k0s.1 on the k0s cluster.
  The CI API server checks run against the same k0s version. Earlier 1.36
  patches are not used: k0s v1.36.4+k0s.1 is the first k0s 1.36 release
  with the fix for GHSA-qjgc-wmg7-3jc3.
- Other minor versions are Not tested for v0.1. Earlier commits ran on
  Kubernetes 1.34 (the kind host, historical, not a Baryon environment)
  and 1.36 (k3s v1.36.4+k3s1 on a single Hadron node, historical); this
  is evidence about Kubernetes behavior, not support.
- Requirement: a Baryon installation needs Kubernetes 1.32 or later,
  because the admission policies that are part of the supported
  installation need it ([Kubernetes requirements](#kubernetes-requirements)).
  This is a floor below which Baryon cannot work, not a statement that
  1.32 to 1.35 are supported.
- Client types are generated for Kubernetes 1.36 and use only stable
  fields (ADR 0008).

## Distribution support policy

Baryon's architecture does not depend on the Kubernetes distribution:
baryon-virtd derives pod identity, namespaces, and cgroups from the
connecting Anchor process, not from distribution paths (ADR 0005). Support
claims still follow tests.

| Distribution | State for v0.1 |
|---|---|
| k0s | Tested: v1.36.4+k0s.1 on the k0s cluster, three Baryon-enabled Hadron nodes (VMs with nested KVM): Hadron v0.5.3, kairos-init v4.3.0, one controller that also runs workloads and two workers, kube-router, OpenEBS LVM LocalPV and Ceph RBD, systemd cgroup driver, Cloud Hypervisor v53.0 (v0.1.0-rc1) and v54.0 (v0.1.0-rc2). Needs the containerd drop-in and worker profile in [k0s node configuration](#k0s-node-configuration). Nested virtualization on three nodes is evidence, not a certification. CI also runs a single-node k0s v1.36.4+k0s.1 API server in a container; it validates only API server behavior (CRD schema, CEL rules, admission policies) and is not a Baryon host |
| k3s | Not supported for v0.1 until tested on more than one node. An earlier release ran on a single Baryon-enabled Hadron node (VM with nested KVM): Hadron v0.5.3, kairos-init v4.3.0, k3s v1.36.4+k3s1, flannel (vxlan backend, `cni0` bridge), local-path, systemd cgroup driver, Cloud Hypervisor v53.0. That is historical evidence. `hack/build-hadron-image.sh` builds a k3s image only when `KUBERNETES_DISTRO=k3s` is set; the default is k0s |
| kubeadm (provider-kubernetes) | Not tested |
| kind | Not a Baryon environment. A development cluster in which a kind node container acted as the host was used for the runtime experiments and early E2E runs; it is no longer used |

## Host requirements

"k0s cluster" and "Hadron node" evidence was taken on Hadron v0.5.3 with
kernel 7.2.6-hadron; "kind host" evidence on a host kernel 6.8.

| Requirement | Why | Source | Verification |
|---|---|---|---|
| x86_64. aarch64 is Planned. | Binaries and firmware built for x86_64 only. | ADR 0002 | Verified (kind host, Hadron node) |
| `/dev/kvm` usable, KVM API 12 | VMs need KVM. | ADR 0002 | Verified (kind host and Hadron node, both nested). On the Hadron node, `/dev/kvm` appeared shortly after baryon-virtd started, when udev loaded `kvm_intel`; discovery runs on each node agent request, so this had no effect. |
| Linux 6.5 or later | `SO_PEERPIDFD` for race-free peer identification. | ADR 0005 | Verified on 6.8 (kind host) and 7.2.6-hadron (Hadron node) |
| Landlock ABI 3 (Linux 6.2 or later) | Cloud Hypervisor `--landlock`, plus `--landlock-rules` granting read-write access to the VM's own `vmm` runtime directory, which Cloud Hypervisor v53.0 needed to re-create the serial socket on a guest reset (with v54.0, see [VMM confinement](security.md#vmm-confinement)). | ADR 0005 | Verified on 6.8 and 7.2.6-hadron, including a guest reset under Landlock on both |
| Yama `kernel.yama.ptrace_scope` 1 or higher | Only an ancestor may trace the Anchor supervisor, including in the moment before it makes itself non-dumpable. baryon-virtd warns at startup when the scope is lower or Yama is absent. | ADR 0015 | Verified: scope 1 read on a Hadron v0.5.3 node of the k0s cluster on 2026-10-05 ([testing.md](testing.md#k0s-cluster)); Hadron sets it in `/etc/sysctl.d/60-hadron-hardening.conf`. The Baryon-enabled image also sets it in `/usr/lib/sysctl.d/50-baryon.conf`: Tested, scope 1 read on the three nodes of the k0s cluster running that image on 2026-10-28 |
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
| Each container in its own PID namespace, with its entrypoint as PID 1 and no injected init | baryon-virtd accepts `Attach` and `Stop` only from the init process of the Anchor container's PID namespace. kubelet `PidNamespaceForPod` maps a pod without `hostPID` and `shareProcessNamespace` to the CRI mode `CONTAINER`. Otherwise the anchor is not PID 1, refuses to run (`refusing to run` in the container log), and the VM never starts. | ADR 0015 | Tested: containerd 2.3.5 with runc on the k0s cluster (`tests/e2e/anchor-authority.sh`) |
| Anchor containers in the initial user namespace; `hostUsers: false` and rootless runtimes are Not supported | The supervisor check requires the initial user namespace; admission refuses `hostUsers: false` on Baryon pods. | ADR 0015 | Implemented; the admission refusal of `hostUsers: false` passes the CI API server checks in CI. The supervisor check's user namespace condition has unit tests |
| The container runtime stops containers with SIGTERM sent as uid 0 by kill(2) or tgkill(2) | The anchor stops its VM gracefully only on such a SIGTERM; any other is ignored and kubelet kills the container after the grace period. | ADR 0015 | Tested: containerd 2.3.5 with runc on the k0s cluster; the anchor handled the runtime's SIGTERM on Anchor Pod replacement and VM deletion (`tests/e2e/anchor-authority.sh`) |
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
| The guest does not run a hypervisor of its own | Guests start with nested virtualization off (`--cpus ...,nested=off`), the Cloud Hypervisor v54.0 default, written explicitly; v53.0, which v0.1.0-rc1 ships, defaulted to on. | Verified: no `vmx` or `svm` flag in a guest's `/proc/cpuinfo` on the k0s cluster (manual check, [testing.md](testing.md#cloud-hypervisor-v540)) |

## Version skew

Baryon has two parts that are upgraded separately:

- the **host image**: the Baryon-enabled Hadron image with baryon-virtd,
  Cloud Hypervisor, and the firmware. It is upgraded with the OS, through
  Kairos, one node at a time ([lifecycle contract](kairos-integration.md#lifecycle-contract)).
- the **cluster release**: the CRDs, admission policies, RBAC,
  baryon-controller, baryon-node-agent, and the anchor and importer
  images. It is upgraded with `kubectl apply` (from 0.1.0-rc1 to
  0.1.0-rc2 and from 0.1.0-rc2 to 0.1.0-rc3, see
  [installation.md](installation.md#10-upgrading); to
  0.1.0-rc1, see [Upgrade to 0.1.0-rc1](#upgrade-to-010-rc1); API changes and their
  upgrade steps per release are in
  [api-stability.md](api-stability.md#change-log-per-release)).

The two talk through the versioned runtime protocol. This section is the
operator contract; [ADR 0012](adr/0012-protocol-versioning.md) describes
the mechanism (negotiation, features, major transitions).

### Which combinations work

The runtime protocol version is `<major>.<minor>`. The 0.1 series speaks
major 1; minors 1.0, 1.1, and 1.2 exist, and 0.1.0-rc1, 0.1.0-rc2, and
0.1.0-rc3 speak 1.2.

Rule (ADR 0012): any 1.x host image works with any 1.x cluster release.
Tested pairs, on the k0s cluster with the version skew E2E (evidence
below):

| Cluster release | Host images | Status |
|---|---|---|
| 1.0 | 1.0 and 1.1 | Tested |
| 1.1 | 1.0 and 1.1 | Tested |
| 1.2 | 1.1 and 1.2 | Tested |

Other pairs, for example a 1.2 cluster release with a 1.0 host, are allowed
by ADR 0012 and not tested.

| Combination | Result | Status |
|---|---|---|
| Cluster release newer than the host image (same major) | Works. Status that the newer minor added is absent for VMs on older hosts: with a host before 1.2, a running VM's `NetworkReady` is Unknown with reason `RuntimeDoesNotReport` | Tested for the pairs above (1.1 cluster release with 1.0 hosts, 1.2 cluster release with a 1.1 host) |
| Host image newer than the cluster release (same major) | Works. The cluster release ignores what the newer host adds | Tested for the pair above (1.0 cluster release with 1.1 hosts) |
| Host image and cluster release of different major versions | Refused: the node is not Ready and takes no new VMs; VMs already running on it keep running. A host of a new major version is reported `ProtocolIncompatible` only if it keeps serving major 1's `GetHost` and `GetRuntimeInfo`, as the transition rule of ADR 0012 requires; a host that does not is reported `RuntimeUnreachable` | Designed (ADR 0012). The checks are Implemented with unit tests; no second major version exists, so it is not tested end to end |
| A VM that needs a feature the host does not implement | Refused on that host (`UnsupportedFeature`). Within the 0.1 series no VM needs a feature beyond the 1.0 features, so this cannot occur between 0.1 releases | Implemented, unit tests |

The minor version decides only whether optional status is available. It
never decides whether a node or a VM is accepted. Required features are
checked per VM, when its anchor attaches.

Release rule (ADR 0012): a 0.1.x release must not make a runtime feature
required unless feature-aware placement ships in the same release.
Without it, nothing steers a VM that needs the feature to a host that
implements it.

Host-side safety fixes (the disk path checks and the block device
registry in baryon-virtd) are not part of the protocol and are not
negotiated, so the protocol rule alone does not give them. Minimum host
image:

| Cluster release | Protocol | Minimum host image |
|---|---|---|
| 0.1.0-rc3 | 1.2 | Protocol: any 1.x host. Host-side safety fixes and the lifecycle authority check (ADR 0015): a host image of 0.1.0-rc2 or later (0.1.0-rc3 changes nothing on the host but the build: the binaries carry their dependency list and the image its release labels). `NetworkReady` needs a 1.2 host (earlier hosts give Unknown, `RuntimeDoesNotReport`) |
| 0.1.0-rc2 | 1.2 | Protocol: any 1.x host. Host-side safety fixes and the lifecycle authority check (ADR 0015): a host image of 0.1.0-rc2 or later. `NetworkReady` needs a 1.2 host (earlier hosts give Unknown, `RuntimeDoesNotReport`) |
| 0.1.0-rc1 | 1.2 | Protocol: any 1.x host. Host-side safety fixes: a host image of 0.1.0-rc1 or later. `NetworkReady` needs a 1.2 host (earlier hosts give Unknown, `RuntimeDoesNotReport`) |

The commits before 0.1.0-rc1 are listed in
[Pre-release commits](#pre-release-commits).

### Upgrade order

Within a major version, either order works:

- cluster release first, then the host images node by node. Tested: the
  1.1 cluster release with mixed 1.0 and 1.1 hosts, the 1.2 cluster
  release with one 1.1 host, and the 0.1.0-rc1 release validation: the
  cluster release first, then the hosts one at a time, with a new VM
  started while the hosts were mixed (every host on runtime protocol 1.2).
- host images first, node by node, then the cluster release. Tested: the
  1.0 cluster release with mixed 1.0 and 1.1 hosts.

A host image older than the cluster release lacks the host-side safety
fixes of that release, even though the protocol accepts it: finish the
host rollout, so that every node runs a host image of the cluster
release or later.

If a node shows `Ready` False with reason `ProtocolIncompatible` or
`RuntimeUnreachable` after a host or cluster upgrade, compare the host
runtime version with the cluster release
([Identifying versions](#identifying-versions)).

Running VMs keep running through a cluster release upgrade: a cluster
release supports the Anchor Pods of the previous release that are still
running (Tested for 1.0 Anchor Pods under the 1.1 cluster release). When
the anchor image changes, they report `RestartRequired=True`
(`AnchorImageChanged`) and use the new anchor from their next start
(observed from the 1.0 to the 1.1 cluster release). A host image upgrade
reboots the node, which stops its VMs
([lifecycle contract](kairos-integration.md#lifecycle-contract)).

During a rollout of the cluster release, a baryon-controller of the
network health change or later with a baryon-node-agent of an earlier
release gets no `status.vms` from that node agent, so a running VM's
`NetworkReady` is Unknown, `NotReported` on 1.2 hosts and
`RuntimeDoesNotReport` on earlier hosts, until the DaemonSet has rolled. A
baryon-controller of an earlier release ignores `status.vms` and sets no
`NetworkReady`. Implemented; not tested.

Evidence on the k0s cluster (version skew E2E, `tests/e2e/skew.sh`, one VM
per node, a baryon-virtd restart on every node; details in
[testing.md](testing.md#k0s-cluster)):

- 1.0 cluster release (c8e6c08) with mixed 1.0 and 1.1 hosts: 31 of 31
  checks.
- 1.1 cluster release (597b637) with mixed hosts: 31 of 31. New anchors on
  1.0 hosts took the UNIMPLEMENTED path of `Negotiate` and attached. The
  node agent published `protocolVersion` 1.0 for old hosts and 1.1 with
  features for new ones.
- A 1.0 anchor reattached to a 1.1 baryon-virtd after a restart.
- VMs of the 1.0 release kept running through the cluster release upgrade,
  with `RestartRequired=True` (`AnchorImageChanged`).
- 1.2 cluster release (the network health change) with one host on the
  previous host image (1.1): 33 of 34. `NetworkReady` was True on the 1.2
  hosts and Unknown (`RuntimeDoesNotReport`) on the 1.1 host, and every VM
  reattached after the baryon-virtd restarts, including a 1.2 anchor on
  the 1.1 host. The failed check was the build stamp: both host images
  carried the same commit because the newer one was built from a tree with
  uncommitted changes. The build scripts now mark such builds `-dirty`.

These runs used builds before 0.1.0-rc1. 0.1.0-rc1 ran the skew E2E with
all hosts on one build (33 of 33); it did not change the runtime protocol.

### Identifying versions

| What | Where | How to read it |
|---|---|---|
| baryon-controller build | its log line `starting`: field `build` (`baryon-controller <version> (<commit>)`) and field `anchor_image`; the Deployment's image | `kubectl logs -n baryon-system deployment/baryon-controller \| grep '"starting"'` and `kubectl get deployment baryon-controller -n baryon-system -o jsonpath='{.spec.template.spec.containers[0].image}{"\n"}'` |
| baryon-node-agent build | its log line `starting`: field `build` (`baryon-node-agent <version> (<commit>)`); the DaemonSet's image | `kubectl logs -n baryon-system <node-agent-pod> \| grep '"starting"'` and `kubectl get daemonset baryon-node-agent -n baryon-system -o jsonpath='{.spec.template.spec.containers[0].image}{"\n"}'` |
| Host runtime (baryon-virtd) build | VirtualizationNode `status.runtime.virtdVersion` (the Runtime column) and `status.runtime.virtdGitCommit` | `kubectl get virtualizationnodes -o custom-columns=NODE:.metadata.name,VIRTD:.status.runtime.virtdVersion,COMMIT:.status.runtime.virtdGitCommit,HYPERVISOR:.status.runtime.hypervisorVersion` |
| Cloud Hypervisor | VirtualizationNode `status.runtime.hypervisorVersion` (the Hypervisor column): the version in the host image's component manifest, or the output of `cloud-hypervisor --version` when the manifest has none | as above |
| Host image | the `host_image` field of the node agent's `host inventory` log line: the Hadron base image the Baryon-enabled image was built from ([api.md](api.md#host-inventory)). On the node, `/usr/share/baryon/components.json` lists the Baryon version and commit of the image, the Hadron and kairos-init images, the Kubernetes distribution and version, and the Cloud Hypervisor, firmware, and virtiofsd versions with their checksums | `kubectl logs -n baryon-system <node-agent-pod> \| grep '"host inventory"'`; on the node, `cat /usr/share/baryon/components.json` |
| Runtime compatibility | VirtualizationNode `RuntimeHealthy` condition. `status.runtime.protocolVersion` and `status.runtime.features` are internal and only help diagnosis | `kubectl get virtualizationnode <node> -o jsonpath='{.status.conditions[?(@.type=="RuntimeHealthy")].reason}{"\n"}'` |

`<node-agent-pod>` is the node agent pod of the node:
`kubectl get pods -n baryon-system -l app.kubernetes.io/name=baryon-node-agent --field-selector spec.nodeName=<node> -o name`.

The commit is the hash of the source the binary was built from,
abbreviated to 12 characters by `hack/build-static.sh` and
`hack/build-hadron-image.sh`. A
`-dirty` suffix means the build came from a tree with uncommitted changes
to tracked files, so it is not a build of that commit; `unknown` means a
build without the information (a plain `cargo build`). baryon-virtd and
`components.json` of a host image carry the same commit, because
`hack/build-hadron-image.sh` stamps both. `virtdGitCommit` is absent while
baryon-virtd is unreachable and when the node agent predates the field.

### What you see

| Situation | VirtualizationNode | VirtualMachine | Running VMs and placement |
|---|---|---|---|
| Cluster release newer than the host image (same major) | `Ready` True (`RuntimeReady`), `RuntimeHealthy` True; `runtime.protocolVersion` shows the older minor | Usual conditions; status of the newer minor is missing (next row) | VMs run; new VMs are placed on the node |
| Optional status missing: the host predates the minor that adds it | As above | `NetworkReady` Unknown, reason `RuntimeDoesNotReport` (hosts before 1.2) | VMs run. Upgrade the host image to get the status |
| Host image newer than the cluster release (same major) | `Ready` True, `RuntimeHealthy` True; the node agent publishes only the fields it knows | Usual conditions; a cluster release before 1.2 sets no `NetworkReady` | VMs run; new VMs are placed on the node |
| Protocol major differs (Designed) | `Ready` False and `RuntimeHealthy` False, reason `ProtocolIncompatible`; the message names both versions. A host of a new major version that does not keep serving major 1's `GetHost` and `GetRuntimeInfo` shows `RuntimeUnreachable` instead. In both cases the node agent marks the node's `virtualization.baryon.io/vm` and `virtualization.baryon.io/import` slots unhealthy, so their allocatable is zero | A VM that needs a new Anchor Pod is placed on another node, or stays `Unschedulable` when no other node fits. A VM whose anchor still reaches the host (for example a pod scheduled before the slots became unhealthy) reports `Running` False and `Ready` False with reason `ProtocolIncompatible` | VMs already running on the node keep running. Compare the host runtime version with the cluster release, and bring both to the same major version |
| A required feature is missing on the host | `Ready` True: the node is compatible. `runtime.features` lacks the feature | `Running` False and `Ready` False with reason `UnsupportedFeature`; the message names the missing features and says to upgrade the host image. kubelet restarts the Anchor container in place with back-off (`(restart back-off)` in the message); while a retry runs, `Running` is True and `Ready` False (`Booting`), with the refusal in the message | The VM does not start on that node. baryon-controller does not move it: the Anchor Pod stays on the node and every retry is refused until the host image is upgraded, after which the next retry attaches. A new Anchor Pod (after a restart request, a stop and start, or a drain) is placed by the scheduler, which may choose the same node. Cannot occur within the 0.1 series |
| baryon-virtd refuses the attach for another reason | Unchanged | `Running` False and `Ready` False with reason `AttachRefused`, or the reason baryon-virtd gave (such as `IdentityConflict` or `NotAnchorSupervisor`); the message has the detail | As for `UnsupportedFeature`, except `IdentityConflict`, for which baryon-controller replaces the Anchor Pod away from the node only when the anchor also exited with the identity conflict exit code, 3 (ADR 0015). A v0.1.0-rc1 anchor exits with 1, so its VM stays on the node until the pod is replaced |

### Release-specific notes

Host access policies across releases (0.1.0-rc3): the policies of
0.1.0-rc3 check pods at creation only, so pods of an earlier release keep
running. baryon-controller and baryon-node-agent of 0.1.0-rc3 create no pod
that they refuse; a cluster that added host access to Baryon pods or to
pods in `baryon-system` (for example through a mutating webhook) must stop
doing so before the upgrade, or the new pods are refused. The refusing
`baryon-vm-guest-config` admits every VirtualMachine of v0.1.

Upgrading from 0.1.0-rc1 to 0.1.0-rc3: through 0.1.0-rc2 (each step is
tested); the direct upgrade is Not tested.

Anchor Pods with the v0.1.0-rc1 anchor image after an upgrade to
0.1.0-rc2 or later: such an anchor exits with code 1, not 3, when
baryon-virtd refuses it for a host identity conflict, so baryon-controller
does not move its VM away from the node. Recognize and recover it as in
[troubleshooting.md](troubleshooting.md#identityconflict). Replacing every
Anchor Pod after the cluster upgrade (the hosts' Kairos upgrades do this,
and so does a restart of each VM, `RestartRequired` shows which ones)
removes the case.

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

Anchor authority across releases (ADR 0015): the protection is
complete only where both the host image and the Anchor Pods include the
anchor authority change (the M1 and M2 fixes, after v0.1.0-rc1). A node
on a v0.1.0-rc1 host image authorizes `Attach` and `Stop` by the identity
alone until the node is upgraded; finding M1 is fixed per node. A
v0.1.0-rc1 anchor on a new host passes the supervisor check, but does not
make itself non-dumpable or filter SIGTERM senders until its Anchor Pod is
replaced with the new anchor image. The new cluster release also needs
the `baryon-no-ephemeral-containers` and `baryon-pod-immutable` policies,
the new `baryon-vm-resource` rules, and the `BARYON_IMPORTER_IMAGE` value
in the controller Deployment; `kubectl apply --server-side -k deploy/base`
installs them.

### Pre-release commits

Minimum host image for the published commits before 0.1.0-rc1, kept for
clusters that still run one of them:

| Cluster release | Protocol | Minimum host image |
|---|---|---|
| a73f37f (first published commit) | 1.0 | a73f37f |
| 597b637 to b0413aa | 1.1 | protocol: any 1.x host; host-side safety fixes: an image built from the same commit as the cluster release or later |
| 29c512c (the network health change) to the commit before 0.1.0-rc1 | 1.2 | protocol: any 1.x host; `NetworkReady` needs a 1.2 host (earlier hosts give Unknown, `RuntimeDoesNotReport`); host-side safety fixes: an image built from the same commit as the cluster release or later |

## Upgrade to 0.1.0-rc1

0.1.0-rc1 is the release of the v0.1 API review; the API stays `v1alpha1`
and the runtime protocol stays 1.2. The API changes are listed in
[api-stability.md](api-stability.md#010-rc1). Upgrade in
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

The commands above are for an installation from a checkout. With the
release files, step 1 applies `baryon-crds.yaml`, step 2 applies your
class manifests, and step 3 applies `baryon.yaml`
([installation.md, Upgrading](installation.md#10-upgrading)).

What changes for running VMs and nodes:

- Running VMs keep running through all three steps. A VM that starts
  while its class is still in the earlier shape reports `InvalidClass`
  until the class is applied again.
- Between steps 1 and 3, baryon-controller of the earlier release reads
  classes with `cpu.cores`, which the new CRD no longer has, so its
  reconciles of VMs fail and no VM starts or stops. Tested in the
  release validation (earlier release: a build of 8db33bb): between the
  steps its watch of VirtualizationNodes failed
  (`` missing field `protocol` ``). The reconcile failures
  (`` missing field `cores` ``) and a VM created meanwhile that got no
  status and no pods were observed in the controller rollback case, the
  same state of an earlier controller with the new CRDs
  ([testing.md](testing.md#v010-rc1-release-validation)). Keep the steps close
  together.
- Between steps 1 and 3, baryon-node-agent of the earlier release cannot
  read VirtualizationNodes either: its types require `runtime.protocol`
  and `runtime.hypervisor`, which the API server now drops when it reads
  the objects. Until the new node agents have rolled out, heartbeats stop
  on every node and the Heartbeat column of `kubectl get
  virtualizationnodes` grows; the device plugins keep working, so running
  VMs and their slots are not affected. Tested in the release validation:
  the heartbeats of all three nodes stopped after step 1, the earlier node
  agents logged `publishing VirtualizationNode failed`
  (`` missing field `protocol` ``), and running VMs were not affected.
  This is another reason to run the three steps in this order and
  quickly.
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
  both together. Tested in the release validation (the earlier
  controller's watch errors above).
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

Status: Tested. The three steps were run in this order when the k0s
cluster was upgraded in place to the API review change from a checkout,
and again in the v0.1.0-rc1 release validation, from a release-like build
of 8db33bb to one of c3a3735 with the release files and a pause after
step 1. Running VMs kept running through all three steps,
with the same Anchor Pods, and the Cloud Hypervisor process checked (the
VM on node2) kept its PID and start time
([testing.md](testing.md#v010-rc1-release-validation)).
