# Kairos integration

Baryon runs on Hadron hosts managed by the Kairos lifecycle. This document
explains which Kairos-family project does what, which ones Baryon depends on,
and how Baryon will cooperate with Kairos-driven node upgrades.

Facts about upstream projects were checked against upstream code and
releases on 2026-10-02. Observations on a single Baryon-enabled Hadron node
(a VM with nested KVM; Hadron v0.5.3, k3s v1.36.4+k3s1) are from
2026-10-03; they are single-node evidence. Testing on a multi-node cluster
of Hadron nodes running k0s is in progress. Versions are recorded so that drift can be detected; they are not
requirements unless the compatibility contract in
[compatibility.md](compatibility.md) says so.

| Project | Version checked | Source |
|---|---|---|
| Hadron | v0.5.3 (2026-09-18), main at 58ff499 | https://github.com/kairos-io/hadron |
| Kairos (monorepo: agent, immucore, kcrypt, kairos-init, sdk, provider) | v4.3.0 (2026-09-08) | https://github.com/kairos-io/kairos |
| AuroraBoot | v0.27.1 (2026-09-11) | https://github.com/kairos-io/AuroraBoot |
| Kairos Operator | v0.2.2 (2026-09-10) | https://github.com/kairos-io/kairos-operator |
| provider-kubernetes (kubeadm) | v0.4.0 (2026-09-21) | https://github.com/kairos-io/provider-kubernetes |
| Cluster API Provider Kairos | v0.1.3 (2026-09-17) | https://github.com/kairos-io/cluster-api-provider-kairos |
| Kairos Fleet infrastructure provider | v0.1.3 | https://github.com/kairos-io/cluster-api-provider-kairos-fleet |

## Roles

### Hadron

A Linux distribution built from source by the Kairos project. musl libc,
systemd, no runtime package manager. Images are customized by OCI layering:
`FROM ghcr.io/kairos-io/hadron:<tag>`, copy files in, then run kairos-init.
Hadron by itself is not an immutable Kairos system.

For Baryon, Hadron is the operating system substrate. The Baryon-enabled
image adds Cloud Hypervisor, firmware, virtiofsd, baryon-virtd, and their
configuration as an image layer. See [hadron.md](hadron.md).

### Kairos

Turns an OS image into an immutable, A/B-upgradable system. Since the
monorepo consolidation in August 2026 the components live in
`kairos-io/kairos`: kairos-agent (install, upgrade, reset, sysext), immucore
(boot-time mounts), kcrypt (encryption), kairos-init (image builder step), the
SDK, and the k3s/k0s provider.

Kairos owns installation, A/B upgrades, recovery, reset, trusted boot, and the
persistent versus ephemeral filesystem layout. Baryon follows that layout: it
ships files only under `/usr` (binaries in `/usr/bin`, firmware in
`/usr/share`, units and vendor configuration in `/usr/lib/systemd`,
`/usr/lib/modules-load.d`, `/usr/lib/tmpfiles.d`), keeps volatile state in
`/run`, and writes logs to `/var/log`, which Kairos persists. It ships
nothing in `/etc`: on Kairos, `/etc/systemd` and `/etc/modprobe.d` are
persistent update-only bind mounts, so files placed there by an image survive
rollbacks.

### Kairos Kubernetes providers

Kubernetes reaches a Kairos node in one of three ways:

- k3s: `kairos-init -p k3s --provider-k3s-version <v>` at image build time.
  The provider (now inside the monorepo) installs k3s into the image and
  starts it from the `k3s:` cloud-config block. Embedded containerd.
- k0s: same mechanism with `-p k0s`. Embedded containerd.
- kubeadm: provider-kubernetes, a separate project that ships complete
  Hadron-based Kairos images with static kubelet, kubeadm, and containerd.
  Configured through the `cluster:` cloud-config block. Kubernetes 1.35 to
  1.37, amd64 only, described upstream as an early release.

The historical provider-kairos repository was archived on 2026-08-31 and
absorbed into the monorepo. The community provider-kubeadm repository is
archived. Baryon does not depend on either.

Baryon does not depend on which path was used. The three differ in kubelet
root directory, CRI socket path, and cgroup driver; baryon-virtd avoids all
three by deriving pod identity, mount namespace, network namespace, and
cgroup from the connecting Anchor process (ADR 0005). The requirements Baryon
does have are listed in [compatibility.md](compatibility.md).

### AuroraBoot

Converts a Kairos OCI image into bootable artifacts: ISO (`build-iso`), raw
EFI or BIOS disk images (top-level mode with `--set disk.efi=true`), signed
UKI artifacts (`build-uki`), netboot. Its `web` mode is a fleet server with a
REST API, node registration through the phone-home agent, and a command
channel. It has no CRDs. It does not produce qcow2.

Baryon uses AuroraBoot to turn the Baryon-enabled image into bootable media.
Baryon does not build ISOs or installers itself.

### Kairos Operator

Kubernetes-driven node operations. CRDs in `operator.kairos.io/v1alpha1`:

- `NodeOp`: run a command on selected nodes in a privileged Job, with
  optional cordon, drain, preflight check, reboot, concurrency, and
  stop-on-failure.
- `NodeOpUpgrade`: generates a `NodeOp` that runs
  `kairos-agent upgrade --source dir:/` from the upgrade image. It always
  cordons and drains, and reboots when the active system is upgraded.

and `build.kairos.io/v1alpha2` `OSArtifact` for in-cluster artifact builds.

Two properties of the current drain matter for VMs (kairos-operator
`internal/controller`, v0.2.2 and main):

- It deletes pods directly. It does not use the Eviction API and does not
  honor PodDisruptionBudgets. Upstream issues kairos-io/kairos#4949, #4950,
  #4951 track this.
- It does not wait for deleted pods to terminate.

Consequence for Baryon today (not tested with Kairos Operator yet): if a
`NodeOpUpgrade` targets a node with running VMs, each Anchor Pod is deleted,
baryon-anchor receives SIGTERM, and the guest gets an ACPI shutdown. Because
the operator does not wait for pods to terminate, the upgrade Job and the
reboot can proceed while guests are still shutting down; a guest that has not
finished when the host reboots is stopped without a clean shutdown. VMs are
stopped, not migrated, and their availability is lost for the duration of the
upgrade. Baryon makes no data-safety claim for this path until it has been
tested.

kubelet graceful node shutdown made a manual reboot wait for the guest
shutdown on a single k3s node (see [Node reboot](#node-reboot)).
Whether it has the same effect on the reboot that a `NodeOpUpgrade` performs
is not tested.

### Cluster API and the Kairos providers

`cluster-api-provider-kairos` provides a bootstrap provider (`KairosConfig`)
and a control-plane provider (`KairosControlPlane`) for k0s and k3s, with
explicit Hadron support. Upgrades are always machine replacement. Drain is
delegated to Cluster API core, which uses the Eviction API, retries refused
evictions without failing, and supports `pre-drain` and `pre-terminate`
lifecycle hook annotations on Machines.

The Kairos Fleet infrastructure provider claims nodes already enrolled in an
AuroraBoot server, pushes the bootstrap cloud-config through the phone-home
command channel, and reboots them.

A cluster created through Cluster API is an ordinary k0s or k3s cluster from
Baryon's point of view. Baryon needs no Cluster API integration to run on it.

## Dependency classification

| Project | Runtime dependency | Build-time dependency | Provisioning integration | Optional lifecycle integration | Future integration |
|---|---|---|---|---|---|
| Hadron | yes (the host OS) | yes (base image) | | | |
| Kairos agent and immucore | yes (boot, layout, upgrades) | yes (kairos-init) | | | |
| A Kubernetes provider (k3s, k0s, or provider-kubernetes) | yes (one of them) | yes (selected at image build) | | | |
| AuroraBoot | no | yes (bootable artifacts) | yes (ISO, raw, netboot, fleet) | | |
| Kairos Operator | no | | | yes (OS upgrades) | VM evacuation before upgrades |
| Cluster API Provider Kairos | no | | yes (cluster creation) | yes (machine replacement) | pre-drain hook for VM evacuation |
| Kairos Fleet provider | no | | yes (claiming enrolled nodes) | | |
| Cloud Hypervisor | yes (the VMM) | yes (in the image) | | | |

"Runtime dependency" means Baryon VMs cannot run without it. No row in the
right-hand columns is required for a Baryon installation.

## Baryon-enabled image workflow (Implemented, see [hadron.md](hadron.md))

```
ghcr.io/kairos-io/hadron:<tag>
        +
Baryon host layer (Cloud Hypervisor, CLOUDHV.fd, virtiofsd,
                   baryon-virtd, systemd unit, modules-load.d)
        +
kairos-init -s install / -s init  (with -p k3s or -p k0s)
        |
        v
Baryon-enabled Hadron OCI image  ---->  registry
        |
        v
AuroraBoot build-iso / raw EFI disk / build-uki
        |
        v
bootable artifact
```

The provider-kubernetes variant starts from the provider-kubernetes image
instead of the plain Hadron base and copies the same Baryon host layer.

On a single k3s node, an image built with `hack/build-hadron-image.sh`
booted from an AuroraBoot raw EFI disk: the first boot runs the recovery
system, which installs the active system and reboots; then the active
system boots. Later images were installed by Kairos A/B upgrade, and a
freshly provisioned node was upgraded the same way (summary in
[testing.md](testing.md#results)). See [hadron.md](hadron.md).

## Manual A/B upgrade

Status: Tested on a single k3s node, manual procedure. Run six times in a
row, each time with a VM on the node (summary in
[testing.md](testing.md#results)). The commands, run from a machine with
the new image in its local Docker daemon and SSH access to the node
(`<node>`; the login user may run `sudo`):

1. Stream the new image to the persistent partition of the node:

   ```bash
   docker save <registry>/baryon/hadron:<tag> | ssh <node> 'sudo sh -c "cat > /usr/local/baryon-upgrade.tar"'
   ```

2. Upgrade the active system, remove the archive, and reboot:

   ```bash
   ssh <node> 'sudo kairos-agent upgrade --source ocifile:/usr/local/baryon-upgrade.tar'
   ssh <node> 'sudo rm /usr/local/baryon-upgrade.tar'
   ssh <node> 'sudo systemctl reboot'
   ```

3. After the reboot, check which image the node runs:

   ```bash
   ssh <node> cat /usr/share/baryon/components.json
   ```

Result each time: the node booted the new image (identified by the
baryon-virtd hash or the `gitCommit` in `components.json`), and the VM was
Ready again after the reboot on a replacement Anchor Pod. With graceful node
shutdown configured, the guest powered off cleanly at the reboot.
kairos-agent upgrades the recovery system separately (`--recovery`); that
was not done. VMs are stopped, not moved, during the upgrade reboot.

## Node reboot

Status: Tested on a single k3s node, manual procedure. Measured with one VM
running; details in
[hadron.md](hadron.md#reboot-behavior):

- Without kubelet graceful node shutdown (the k3s default), the guest is
  stopped hard at the reboot. After boot, kubelet re-admits the existing
  Anchor Pod before the device plugin has registered; admission fails with
  `UnexpectedAdmissionError` and the pod fails.
- With graceful node shutdown, kubelet holds a systemd-logind delay
  inhibitor, the guest shuts down cleanly, and the old Anchor Pod ends
  `Succeeded`.
- In both cases baryon-controller creates a new Anchor Pod (new pod UID and
  IP, same MAC). It stays Pending until the device plugin registers, and the
  VM boots again (timings in [testing.md](testing.md#results)).
- baryon-virtd exits 2 s after SIGTERM. Before that change it waited for open gRPC streams until systemd killed it at the 90 s
  stop timeout when it was restarted with VMs running. Node reboots were not
  delayed by it: the pods, and with them the client connections, are gone
  before systemd stops baryon-virtd.

Graceful node shutdown is a host requirement in
[compatibility.md](compatibility.md). The Anchor Pod termination grace
period is 90 s, so `shutdownGracePeriod` minus
`shutdownGracePeriodCriticalPods` must be at least 90 s. The Kubernetes 1.36
kubelet has no command-line flag for `shutdownGracePeriod` (upstream
registers none), so k3s started with
`--kubelet-arg=shutdown-grace-period=...` fails to start; use a
KubeletConfiguration file. Baryon cannot observe whether a node has it
configured; a VirtualizationNode condition for it is Planned. For k3s, the
cloud-config in [hadron.md](hadron.md#node-configuration) writes the drop-in
`/var/lib/rancher/k3s/agent/etc/kubelet.conf.d/90-graceful-shutdown.conf`
(`shutdownGracePeriod: 150s`, `shutdownGracePeriodCriticalPods: 30s`) from
the Kairos `initramfs` stage, which runs after the persistent mounts and
before k3s starts. A freshly provisioned node had the file in place and the
kubelet's shutdown inhibitor active (150 s) with no manual step. For k0s
the configuration is part of the multi-node testing in progress. For provider-kubernetes the setting has to be
baked into the image, because that provider ignores `kubeletConfiguration`
from cloud-config; not tested.

## Kubernetes-driven upgrade (Designed)

The upgrade itself stays with Kairos:

```
Baryon-enabled image vN+1 pushed to a registry
        |
        v
NodeOpUpgrade (Kairos Operator) targeting selected nodes
        |
        v
cordon, drain, kairos-agent upgrade, reboot
        |
        v
node boots image vN+1, baryon-virtd validates the host
        |
        v
VirtualizationNode Ready=True, device plugin slots healthy again
```

## Cooperation with upgrades (Designed, not implemented)

Baryon will not add an upgrade controller. It will perform VM evacuation
itself and expose node state that an upgrade workflow can wait on.

VirtualizationNode Conditions reserved for this:

| Condition | Meaning | Status |
|---|---|---|
| `Ready` | Host runtime healthy and accepting VMs. | Implemented |
| `RuntimeHealthy` | baryon-virtd reachable, discovery succeeded, and its runtime protocol major version matches the cluster release (reason `ProtocolIncompatible` otherwise, ADR 0012). | Implemented |
| `Evacuating` | Baryon is stopping or moving VMs off the node. | Designed |
| `Evacuated` | No Baryon VMs remain on the node. | Designed |
| `UpgradeSafe` | Evacuated and no new VMs will be placed. | Designed |

Design direction, to be recorded in its own ADR before implementation:

1. The evacuation request has a single writer and is separate from status:
   either Node cordon (`spec.unschedulable`) is taken as the signal, or a
   dedicated request object is introduced. VirtualizationNode has no spec
   field for it today.
2. On a request, baryon-node-agent marks its device plugin slots unhealthy so
   no new VMs are placed, and the VMs on the node are stopped (migrated once
   migration exists).
3. When `Evacuated=True`, a label such as
   `virtualization.baryon.io/upgrade-safe=true` is set so that a
   `NodeOpUpgrade` whose `nodeSelector` requires it upgrades only evacuated
   nodes. The label must be removed before the node accepts VMs again, or the
   next upgrade could select a node with running VMs.
4. After the reboot, baryon-virtd validates the host, the node agent reports
   `Ready=True`, and the slots become healthy.

This relies on upstream behavior that is inferred from Kairos Operator code
and not documented: `nodeSelector` is re-evaluated on reconcile (on Job and
Pod events and a periodic requeue). Whether a NodeOp that already reached a
terminal state picks up nodes labelled later is not verified. If Kairos
Operator gains an eviction-based drain or a preflight retry verdict, the
integration should use that instead.

For the hard reboot case, the compatibility contract requires kubelet
graceful node shutdown, so that a reboot that does not go through Kubernetes
still terminates Anchor Pods first (see [Node reboot](#node-reboot)).

For Cluster API machine deletion, the equivalent hook is the
`pre-drain.delete.hook.machine.cluster.x-k8s.io/<name>` annotation on the
Machine object in the management cluster. That requires access to the
management cluster, which Baryon does not have by default, so it is listed as
a future integration only.

## State across a Kairos A/B upgrade

| State | Survives upgrade | Why |
|---|---|---|
| VM disks | yes | in PVCs, outside the OS image |
| Kubernetes objects | yes | in the API server |
| baryon-virtd runtime records | no, by design | in `/run`, rebuilt after boot |
| Running VMs | no | the node reboots; baryon-controller replaces the Anchor Pods and the VMs boot again (observed on a single k3s node after node reboots and after each upgrade reboot) |
| VM console logs | yes | under `/var/log/baryon`, which Kairos persists |
| Baryon host binaries and units | replaced | part of the image (checked after each manual upgrade on a single k3s node: the baryon-virtd hash or the `gitCommit` in `components.json`) |

Nothing Baryon needs must survive in the OS image across an upgrade. That is
deliberate: it makes upgrades and rollbacks safe with respect to Baryon.
