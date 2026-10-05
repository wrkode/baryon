# Kairos integration

Baryon runs on Hadron hosts managed by the Kairos lifecycle. This document
explains which Kairos-family project does what, which ones Baryon depends on,
and how Baryon cooperates with Kairos-driven node upgrades.

Facts about upstream projects were checked against upstream code and
releases on 2026-10-02. Observations are from 2026-10-03, on a single
Baryon-enabled Hadron node running k3s (v1.36.4+k3s1) and on the k0s
cluster: three Baryon-enabled Hadron nodes (node1 to node3, VMs with nested
KVM; Hadron v0.5.3, Kairos v4.3.0) running k0s v1.36.4+k0s.1, described in
[testing.md](testing.md#results). Versions are recorded so that drift can be
detected; they are not requirements unless the compatibility contract in
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

Upstream observation (k0s provider, Kairos v4.3.0): on the first boot of
the active system, the k0s controller and worker services started before
the provider had written their systemd override (arguments, token file), so
the controller ran without `--enable-worker` and the workers failed for
lack of a join token. A `systemctl daemon-reload` and a restart of the k0s
service fixed it; later boots were not affected. Not yet reported upstream.
The commands are in
[troubleshooting.md](troubleshooting.md#k0s-starts-without-its-configuration-on-the-first-boot).

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
  #4951 track eviction and waiting.
- It does not wait for deleted pods to terminate.

What this means for Baryon, observed with v0.2.2 on the k0s cluster, is in
[Cooperation with upgrades](#cooperation-with-upgrades): each deleted Anchor
Pod gives its guest an ACPI shutdown, and the guests finished before the
reboot. The operator reboots with `nsenter -i -m -t 1 -- reboot`; on Hadron
`reboot` is systemctl, which honors kubelet's shutdown inhibitor, so a guest
still running at the reboot also gets a clean shutdown when kubelet graceful
node shutdown is configured (Tested).

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
| Kairos Operator | no | | | yes (OS upgrades; Tested for one node and for all three nodes of the k0s cluster) | eviction-based drain and waiting for pod termination (upstream) |
| Cluster API Provider Kairos | no | | yes (cluster creation) | yes (machine replacement) | pre-drain hook for VM evacuation |
| Kairos Fleet provider | no | | yes (claiming enrolled nodes) | | |
| Cloud Hypervisor | yes (the VMM) | yes (in the image) | | | |

"Runtime dependency" means Baryon VMs cannot run without it. No row in the
right-hand columns is required for a Baryon installation.

## Lifecycle contract

Who owns what when a node changes:

- Kairos owns the node and its OS: installation, A/B upgrades and
  rollback, recovery, and trusted boot. Kubernetes-driven OS upgrades go
  through Kairos Operator (`NodeOpUpgrade`). Baryon has no OS upgrade
  controller and does not gate upgrades.
- Kubernetes owns cordon, drain, eviction, scheduling, and volume attach
  and detach.
- Baryon owns VM runtime reconciliation. baryon-controller replaces an
  Anchor Pod that is gone and reports VM status; baryon-virtd runs the
  VMMs and reattaches them after its own restart; baryon-node-agent
  publishes the VirtualizationNode state that an upgrade workflow can read
  (`Ready`, `RuntimeHealthy`, `heartbeatTime`, `vmCount`; see
  [Cooperation with upgrades](#cooperation-with-upgrades)).
- Live migration is currently unsupported. Baryon's network handoff
  model requires destination-side file descriptor injection during Cloud
  Hypervisor migration, which is not available in the currently supported
  Cloud Hypervisor release. See [ADR 0013](adr/0013-live-migration.md). A
  node event that removes an Anchor Pod stops the VM; it starts again
  where its storage allows.

Upgrade procedure for v0.1: one `NodeOpUpgrade` per node with Kairos
Operator v0.2.2, starting the next node only after the previous node has
rebooted and its VMs are back
([runbook](#runbook-upgrade-one-node-with-kairos-operator)). The
`NodeOpUpgrade` of one node is Tested on the k0s cluster. The check of the
runbook's step 5, which tells by the Node's boot ID and the
VirtualizationNode heartbeat that the node has rebooted and reports
again, is Designed; the tested runs waited for the VirtualizationNode
`Ready` condition instead. One node per `NodeOpUpgrade` is the procedure
tested with v0.2.2, not a permanent limitation of Kairos Operator: v0.2.2
counts a node's reboot as complete
before the node is back, so `concurrency: 1` in one `NodeOpUpgrade` for
several nodes does not keep their outages from overlapping. Such a run
completed on the k0s cluster and every VM recovered, but it is not the
procedure for v0.1. The manual `kairos-agent upgrade` procedure
([Manual A/B upgrade](#manual-ab-upgrade)) is Tested as well.

What happens to the VMs on a node:

| Event | VMs on the node | Evidence |
|---|---|---|
| Cordon | Running VMs keep running; a cordon does not touch running pods. New VMs, and VMs whose Anchor Pod is replaced, are not placed on the node: VM placement cannot tolerate the cordon taint | No new VM on a cordoned node: Tested (multi-node and placement E2E, cordon cases). Running VMs: Kubernetes cordon semantics, not checked separately |
| Drain | Anchor Pods are removed (`kubectl drain` evicts them; the `NodeOpUpgrade` drain of v0.2.2 deletes them). Each guest gets an ACPI shutdown, within the Anchor Pod's 90 s termination grace period. baryon-controller creates a new Anchor Pod, and the scheduler places it where the VM's storage allows: a VM on Ceph RBD starts on another node after CSI detach and attach; a VM on OpenEBS LVM LocalPV waits for its node (`ClaimPending`, and `Unschedulable` while the node is cordoned) | Tested with the `NodeOpUpgrade` drain: manual procedure on the k0s cluster ([testing.md](testing.md#k0s-cluster)). `kubectl drain`: Not tested |
| Graceful node shutdown (a reboot or shutdown that does not go through Kubernetes) | kubelet terminates the Anchor Pods first, and the guests shut down cleanly. Needs kubelet graceful node shutdown with `shutdownGracePeriod` minus `shutdownGracePeriodCriticalPods` at least 90 s; the tested nodes set 150 s and 30 s through Kairos cloud-config | Tested: manual procedure on the k0s cluster (including the Kairos Operator reboot without drain) and the Hadron k3s node ([Node reboot](#node-reboot)) |
| Node reboot without graceful node shutdown | The guests are stopped hard. After boot the old Anchor Pod fails admission (`UnexpectedAdmissionError`) and is replaced | Tested on the Hadron k3s node. Not a supported configuration: graceful node shutdown is a host requirement ([compatibility.md](compatibility.md#host-requirements)) |
| Reboot, including the reboot of an A/B upgrade | baryon-controller creates a new Anchor Pod (new pod UID and IP, same MAC). A VM on node-local storage comes back on the same node once the node is Ready; a VM on shared storage may start on another node | Tested: manual procedures on the k0s cluster and the Hadron k3s node |
| baryon-controller restart | VMs are untouched; reconciliation resumes after the restart | Tested: multi-node E2E (case controller) on the k0s cluster |
| baryon-virtd restart | VMs keep running (the same VMM process); each anchor reattaches and baryon-virtd recovers the VM's runtime state; the DHCP responder is replaced | Tested: multi-node E2E (case virtd) on the k0s cluster; manual procedure on the Hadron k3s node |
| baryon-virtd stopped | The VirtualizationNode goes `Ready` False and the node takes no new VMs; VMs on other nodes are untouched; the node's VM is Ready again once baryon-virtd is back | Tested: multi-node E2E (case runtime-down) on the k0s cluster |
| Node failure (crash, power loss, partition) | The VM is down, or its state is unknown during a partition. Once Kubernetes marks the Anchor Pod not ready, the VM's `Running` and `Ready` are Unknown with reason `NodeUnreachable` ([api.md](api.md#node-unreachable)). A VM that was still booting keeps `Ready` False (`Booting`); only `NetworkReady` (Unknown after 90 s) shows the loss. baryon-controller creates no new Anchor Pod while the old pod object exists (fencing, ADR 0004). After taint-based eviction the pod stays Terminating until kubelet on the node confirms it, an administrator deletes it after confirming that the node is off, or the node gets the `node.kubernetes.io/out-of-service` taint. Then the VM starts where its storage allows | `NodeUnreachable`: Implemented, unit test. Fencing: ADR 0004. Behavior after the out-of-service taint: inferred from upstream pod garbage collection, not observed. Power loss: Tested on the k0s cluster (the node of a VM pinned to it lost power; ceph-rbd): once Kubernetes marked the pod not ready, `Running`, `Ready`, and `NetworkReady` were Unknown (`NodeUnreachable`); no new Anchor Pod was created while the node was off, also after the taint eviction marked the old pod Terminating; after power-on kubelet confirmed the deletion and a new Anchor Pod brought the VM back to Ready. The out-of-service taint was not tested |
| Kairos rollback after a failed upgrade boot | The node boots the previous host image. An older host image of the same protocol major version works with the cluster release ([compatibility.md](compatibility.md#which-combinations-work)), but lacks the host-side safety fixes of newer releases; VMs come back as after a reboot | Not tested; pending in the release validation |
| Deleting the Node object | The VirtualizationNode is garbage-collected through its owner reference to the Node, and Kubernetes deletes the pods bound to the missing node, which removes the fencing; baryon-controller then creates a new Anchor Pod. Delete the Node object only after confirming that the machine is off, because a guest that still runs there would run twice | Not tested; inferred from owner references and upstream pod garbage collection |
| baryon-node-agent outage | VMs keep running and stay `Ready`. Their `NetworkReady` becomes Unknown (`RuntimeUnreachable`) once the node's report is older than 90 s. A stopping node agent sets the node's `Ready` and `RuntimeHealthy` to Unknown (`NodeAgentStopped`) | Tested: node-agent-outage E2E and network-health E2E (case agent) on the k0s cluster |

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

Status: Tested, manual procedure: six times in a row on a single k3s node,
each time with a VM on the node, and on each of the three nodes of the k0s
cluster with VMs running, where a VM also survived the upgrade of the
control-plane node (summary in [testing.md](testing.md#results)). The
commands, run from a machine with
the new image in its local Docker daemon and SSH access to the node
(`<node>`; the login user may run `sudo`):

1. Stream the new image to the persistent partition of the node:

   ```bash
   docker save <registry>/baryon-hadron:<tag> | ssh <node> 'sudo sh -c "cat > /usr/local/baryon-upgrade.tar"'
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

Status: Tested, manual procedure, on a single k3s node and on the k0s
cluster, with one VM running; details for the k3s node in
[hadron.md](hadron.md#reboot-behavior). On the k0s cluster, with graceful
node shutdown and the VM on an LVM local volume, the guest shut down
cleanly, and the VM came back without intervention once the node was
Ready, on a replacement Anchor Pod on the same node.

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
- baryon-virtd exits 2 s after SIGTERM. Before that change it waited for
  open gRPC streams until systemd killed it at the 90 s stop timeout when
  it was restarted with VMs running. Node reboots were not
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
kubelet's shutdown inhibitor active (150 s) with no manual step. For k0s,
graceful node shutdown is set in a k0s worker profile in the k0s
`ClusterConfig`, delivered by Kairos cloud-config
([hadron.md](hadron.md#node-configuration)); Tested on the k0s cluster. For
provider-kubernetes the setting has to be baked into the image, because that
provider ignores `kubeletConfiguration` from cloud-config; not tested.

## Kubernetes-driven upgrade

The upgrade itself stays with Kairos:

```
Baryon-enabled image vN+1 pushed to a registry
        |
        v
NodeOpUpgrade (Kairos Operator) targeting selected nodes
        |
        v
cordon, drain (pod delete), kairos-agent upgrade, reboot
        |
        v
node boots image vN+1, baryon-virtd validates the host
        |
        v
VirtualizationNode Ready=True, device plugin slots healthy again
```

## Cooperation with upgrades

Baryon has no upgrade controller and does not move running VMs. Live
migration is currently unsupported. Baryon's network handoff model
requires destination-side file descriptor injection during Cloud
Hypervisor migration, which is not available in the currently supported
Cloud Hypervisor release. See [ADR 0013](adr/0013-live-migration.md). A
node upgrade therefore stops the VMs on the node, and they start again on a node
that can reach their storage. Baryon only exposes node state that an upgrade
workflow can read.

VirtualizationNode state with semantics for upgrades:

| State | Meaning | Status |
|---|---|---|
| `Ready` condition | The node can run Baryon VMs: baryon-virtd reachable, a compatible runtime protocol, and KVM usable (reason `RuntimeReady`). | Implemented |
| `RuntimeHealthy` condition | baryon-virtd reachable, discovery succeeded, and its runtime protocol major version matches the cluster release (reason `ProtocolIncompatible` otherwise, ADR 0012). | Implemented |
| `status.heartbeatTime` | Last refresh by the node agent. The conditions count only while it is within 90 s. A node agent stopped for the reboot sets both conditions to Unknown (`NodeAgentStopped`); one that crashes leaves them unchanged ([api.md](api.md#virtualizationnode-cluster-scoped-short-name-vnode)). | Implemented |
| `status.vmCount` | VMs known to baryon-virtd on the node; 0 answers "no VMs here". | Implemented |

Earlier drafts reserved `Evacuating`, `Evacuated`, and `UpgradeSafe`
conditions and an upgrade-safe node label. They were removed: without
migration there is nothing to evacuate beyond what a drain does, and a node
label that gates `NodeOpUpgrade` would need its own ADR (single writer,
removal before the node accepts VMs again).

### Runbook: upgrade one node with Kairos Operator

Status: Tested on the k0s cluster with Kairos Operator v0.2.2, for one
node; the step 5 check is Designed (see the step). A single
`NodeOpUpgrade` for all three nodes was also run (below). With v0.2.2,
`concurrency` does not bound how many nodes are down at once, so this
runbook upgrades one node per `NodeOpUpgrade` and starts the next only
after the previous node and its VMs are back: step 5 checks that by the
Node's boot ID, recorded in step 3, and the VirtualizationNode heartbeat.

1. Optionally drain the node with kubectl first (this also cordons it).
   kubectl drain uses the Eviction API and honors PodDisruptionBudgets. Anchor Pods have no
   emptyDir and are owned by their VirtualMachine, so no extra drain flags
   are needed for them:

   ```bash
   kubectl drain <node> --ignore-daemonsets
   ```

   Without this step, the `NodeOpUpgrade` drain deletes the Anchor Pods
   directly, which ignores PodDisruptionBudgets.
2. Wait until no VM runs on the node:

   ```bash
   kubectl wait virtualizationnode/<node> --for=jsonpath='{.status.vmCount}'=0 --timeout=10m
   ```

   The node agent omits `vmCount` while baryon-virtd is unreachable, so the
   wait does not pass on a node whose runtime is down.

3. Record the node's boot ID, for step 5. Kubernetes changes it when the
   node reboots:

   ```bash
   node=<node>
   boot0="$(kubectl get node "$node" -o jsonpath='{.status.nodeInfo.bootID}')"
   ```

   Upgrade the node with a `NodeOpUpgrade` that selects only this node,
   with concurrency 1:

   ```yaml
   apiVersion: operator.kairos.io/v1alpha1
   kind: NodeOpUpgrade
   metadata:
     name: upgrade-<node>
     namespace: default
   spec:
     image: <registry>/baryon-hadron:<tag>
     nodeSelector:
       matchLabels:
         kubernetes.io/hostname: <node>
     concurrency: 1
     upgradeActive: true
     force: true
   ```

   The tested runs used `force: true` and an upgrade image that was already
   present in each node's containerd under a fixed tag.
4. If you drained the node in step 1, uncordon it when it is Ready again:
   `kubectl uncordon <node>`.
5. Before the next node, wait until this node has rebooted, its node
   agent reports `Ready` again, and its VMs are back (every
   VirtualMachine `Ready=True`, or `vmCount` back to its value before the
   upgrade). `kubectl wait --for=condition=Ready` on the
   VirtualizationNode is not enough: a `Ready=True` written before the
   reboot satisfies it at once. Instead, wait until the Node's boot ID
   differs from `boot0`, then until the VirtualizationNode's
   `heartbeatTime` changes and `Ready` is True. The node agent of the
   previous boot cannot write after the reboot, so a changed heartbeat
   comes from the new one; no clocks are compared. In the same shell as
   step 3:

   ```bash
   (
     wait_until() {  # wait_until <seconds> <what> <command...>
       local deadline=$(( $(date +%s) + $1 )) what=$2
       shift 2
       until "$@"; do
         [ "$(date +%s)" -lt "$deadline" ] || { echo "timed out: $what" >&2; exit 1; }
         sleep 10
       done
     }
     rebooted() {
       local b
       b="$(kubectl get node "$node" -o jsonpath='{.status.nodeInfo.bootID}')" &&
         [ -n "$b" ] && [ "$b" != "$boot0" ]
     }
     reporting() {
       local ready beat
       read -r ready beat < <(kubectl get virtualizationnode "$node" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status} {.status.heartbeatTime}{"\n"}') &&
         [ "$ready" = True ] && [ -n "$beat" ] && [ "$beat" != "$hb0" ]
     }
     wait_until 1800 "node $node did not reboot" rebooted
     hb0="$(kubectl get virtualizationnode "$node" -o jsonpath='{.status.heartbeatTime}')"
     wait_until 900 "VirtualizationNode $node is not Ready after the reboot" reporting
   ) && kubectl wait virtualmachine/<vm> -n <namespace> --for=condition=Ready --timeout=15m
   ```

   The parentheses run the wait in a subshell, so a timeout ends it with
   exit status 1 and skips the VM wait without closing your shell; stop
   there and check the node. Run the `kubectl wait virtualmachine` command
   for each VM that was running on the node. A VM with
   `runPolicy: Stopped` never becomes Ready.

   The node agent also sets `Ready` and `RuntimeHealthy` to Unknown
   (`NodeAgentStopped`) when it is stopped for the reboot. That is a
   signal for people watching `kubectl get virtualizationnodes`, not
   something this check relies on: the write is best effort (the node
   agent reaches the API server over the pod network and gives it 5 s
   during shutdown), and a node agent that is killed does not write it.

   Status: Designed; not yet run on the k0s cluster. The tested runs
   below used `kubectl wait --for=condition=Ready`.

   Only then create the `NodeOpUpgrade` for the next node.

Observed on the k0s cluster (v0.2.2, `force`, concurrency 1, two VMs on the
node):

- The drain deleted both Anchor Pods at once. Both guests shut down
  cleanly (console `reboot: Power down`) before the reboot.
- The VM on Ceph RBD restarted on another node.
- The VM on an LVM local volume stayed Pending (`ClaimPending`, and
  `Unschedulable` while its node was cordoned) until its node returned, and
  was then Ready.
- The operator reported the reboot complete and uncordoned the node while
  the node was still NotReady (reboot detection of v0.2.2).
- In a separate test, the operator's reboot with a VM running and no drain:
  kubelet's logind delay inhibitor held, the guest powered off cleanly, and
  the VM came back.

Observed on the k0s cluster with one `NodeOpUpgrade` for all three nodes
(v0.2.2, `nodeSelector` `virtualization.baryon.io/host=true`, concurrency 1,
`force`, the image preloaded on every node; one VM on Ceph RBD on node1 and
one on an LVM local volume on each of node2 and node3):

- The upgrade Completed and every node booted the new image (the
  baryon-virtd hash equal to the image's).
- Concurrency 1 did not serialize the node outages. The drains of the three
  nodes started one after another without waiting for the previous node to
  return: v0.2.2 counts a node's reboot as complete before the node is
  back, as in the single-node test, which frees the concurrency slot, so
  the nodes rebooted at overlapping times.
- Guests shut down cleanly at each drain.
- The LVM VMs came back on their own nodes once those returned.
- The Ceph RBD VM was down longest. No node was schedulable while all were
  cordoned or rebooting. Its restart was then delayed until the Ceph CSI
  attacher, whose controller had restarted with the control-plane node,
  regained its leader lease after the in-cluster API path returned; until
  then the volume stayed attached to the old node. Once the attacher led,
  detach and attach completed and the VM was Ready.
- All VMs recovered without intervention.
- VMs whose claims carried an older import record (no digest) got an
  import Pod after their drain; on filesystem claims the importer found the
  marker and left the disk unchanged (see
  [compatibility.md](compatibility.md#release-specific-notes)).

Limits:

- PodDisruptionBudgets give no protection under the `NodeOpUpgrade` drain of
  v0.2.2, which deletes pods. Use the kubectl drain in step 1 when a budget
  must hold.
- An administrator who cordoned or drained the node before the
  `NodeOpUpgrade` must uncordon it by hand.
- VM placement cannot tolerate the cordon taint
  (`node.kubernetes.io/unschedulable`) or the out-of-service taint (CEL
  rules, see [api.md](api.md#placement)), so a cordoned node takes no new
  import Pod or Anchor Pod during an upgrade, whatever a VM's tolerations.
  A drain removes Anchor Pods regardless of tolerations. A VM whose
  placement allows only the node being upgraded stays `Unschedulable`
  until the node is schedulable again, like a VM on node-local storage.
- A PodDisruptionBudget that selects VM labels counts a running import Pod
  as unhealthy (it is never Ready), so the kubectl drain in step 1 can be
  blocked while an import runs. The `NodeOpUpgrade` drain of v0.2.2 deletes
  pods and is unaffected.
- With v0.2.2, `concurrency` does not bound simultaneous node outages,
  because reboot completion is counted before the node is back. To upgrade
  one node at a time, run one `NodeOpUpgrade` per node as in the runbook.
  The upstream main branch detects reboot completion by the node's boot ID
  (not in v0.2.2); not tested.
- VMs on shared storage can be down longer than VMs on node-local storage
  when the CSI controller itself restarts with an upgraded node: its
  attacher must regain its leader lease before the volume can move.
- Sequential upgrades of several nodes can restart a VM on shared storage
  several times, once for each node it lands on before that node is
  upgraded.
- VMs on node-local storage are unavailable until their node is back.
- Eviction-based drain and waiting for pod termination in Kairos Operator
  are tracked upstream (kairos-io/kairos#4949, #4950, #4951).

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
| Running VMs | no | the node reboots; baryon-controller replaces the Anchor Pods and the VMs boot again (observed on a single k3s node and on the k0s cluster after node reboots and upgrade reboots) |
| VM console logs | yes | under `/var/log/baryon`, which Kairos persists |
| Baryon host binaries and units | replaced | part of the image (checked after each manual upgrade: the baryon-virtd hash or the `gitCommit` in `components.json`) |

Nothing Baryon needs must survive in the OS image across an upgrade. That is
deliberate: it makes upgrades and rollbacks safe with respect to Baryon.
