# Troubleshooting

How to find out why a node or a VM is not doing what you expect. The
first part answers common questions and points to the failure modes in the
second part. Condition types, reasons, and their exact meaning are defined
in [api.md](api.md#condition-contract); this page uses the same reasons.
Messages are human text and can change between releases; reasons are
contract.

Placeholders: `<vm>` and `<namespace>` name the VirtualMachine, `<node>`
the Kubernetes node, `<node-address>` an address you reach it at over SSH.

## Where to look

| What | Command |
|---|---|
| VMs and their `Ready` reason | `kubectl get virtualmachines -A` |
| A VM's conditions, messages, and Events | `kubectl describe virtualmachine <vm> -n <namespace>` |
| One condition | `kubectl get virtualmachine <vm> -n <namespace> -o jsonpath='{.status.conditions[?(@.type=="Ready")].reason}{": "}{.status.conditions[?(@.type=="Ready")].message}{"\n"}'` |
| The VM's pods | Anchor Pod: `kubectl get virtualmachine <vm> -n <namespace> -o jsonpath='{.status.anchorPod}{"\n"}'` (named `<vm>-anchor`); import Pod: `kubectl get pod <vm>-import -n <namespace>` |
| A pod's Events (scheduling, volume attach, image pull) | `kubectl describe pod <pod> -n <namespace>` |
| The last failure of the Anchor container | `kubectl get pod <vm>-anchor -n <namespace> -o jsonpath='{.status.containerStatuses[?(@.name=="anchor")].lastState.terminated.message}{"\n"}'` |
| Importer log | `kubectl logs <vm>-import -n <namespace> -c import` |
| Nodes | `kubectl get virtualizationnodes` and `kubectl describe virtualizationnode <node>` |
| baryon-controller log | `kubectl -n baryon-system logs deploy/baryon-controller` |
| Node agent of a node | `pod="$(kubectl get pods -n baryon-system -l app.kubernetes.io/name=baryon-node-agent --field-selector spec.nodeName=<node> -o name)"` then `kubectl -n baryon-system logs "$pod"` |
| baryon-virtd on a node | `ssh <node-address> sudo journalctl -u baryon-virtd` and `ssh <node-address> systemctl status baryon-virtd` |
| Per-VM logs on the node | `/var/log/baryon/vms/<namespace>_<vm>_<pod-uid>/`: `console.log`, and after the VM stopped the last 16 MiB of the Cloud Hypervisor log and output and of the DHCP responder log; while the VM runs, those three are under `/run/baryon/vms/<pod-uid>/` ([development.md](development.md#places-on-a-baryon-node), [hadron.md](hadron.md#logs-and-cleanup)) |

`<pod-uid>` is the Anchor Pod's UID:
`kubectl get pod <vm>-anchor -n <namespace> -o jsonpath='{.metadata.uid}{"\n"}'`.
Each start of a VM gets a new Anchor Pod and with it a new log directory.
Logs are JSON; the controller, node agent, and baryon-virtd log at `info`.
The per-VM files and the paths under `/run/baryon` are internal and can
change between releases.

## Questions

### Is this node virtualization-capable?

```bash
kubectl get virtualizationnodes
kubectl get nodes -L virtualization.baryon.io/host
```

A node can run VMs when its VirtualizationNode is `Ready` True with reason
`RuntimeReady` and its heartbeat is less than 90 s old. The conditions
count only while the heartbeat is recent; an older heartbeat means the node
agent is not reporting ([node runtime unavailable](#node-runtime-unavailable)).

| What you see | Failure mode |
|---|---|
| No VirtualizationNode for the node | the node lacks the label `virtualization.baryon.io/host=true` ([no compatible node](#no-compatible-node)), or the node agent cannot start ([Cloud Hypervisor or firmware missing](#cloud-hypervisor-or-firmware-missing)) |
| `Ready` False, `KvmUnavailable` | [KVM unavailable](#kvm-unavailable) |
| `Ready` False, `RuntimeUnreachable` | [node runtime unavailable](#node-runtime-unavailable) |
| `Ready` False, `ProtocolIncompatible` | [runtime protocol mismatch](#runtime-protocol-mismatch) |
| `Ready` Unknown, `NodeAgentStopped` | [node runtime unavailable](#node-runtime-unavailable) |

`Ready` True does not check the kubelet cgroup driver, containerd device
ownership, or graceful node shutdown. A wrong cgroup driver shows up only
when a VM starts ([unsupported cgroup driver](#unsupported-cgroup-driver)).

### Why is this VM not scheduled?

`Scheduled` False. Its reason says why:

- `Unschedulable` (from the scheduler): [no compatible node](#no-compatible-node)
- `ClaimPending`, `ImportPending`, `ImportingDisk`, or an import refusal
  on `StorageReady`: the disk is not ready yet; while an import is pending
  there is no Anchor Pod ([disk import failure](#disk-import-failure),
  [import refusals](#import-refusals))
- `ClaimInUse`: [ClaimInUse](#claiminuse)
- `ClaimRejected`, `PodRejected`: [ClaimRejected and PodRejected](#claimrejected-and-podrejected)
- `ClassNotFound`, `InvalidClass`, `InvalidSpec`, `ClaimNotFound`,
  `ClaimConflict`, `AnchorPodConflict`, `ImportPodConflict`, `Stopped`:
  see [Not running reasons](api.md#not-running-reasons); the message says
  what to fix

### Why is this VM not Ready?

```bash
kubectl get virtualmachine <vm> -n <namespace>
kubectl describe virtualmachine <vm> -n <namespace>
```

The `REASON` column is the `Ready` reason:

| `Ready` reason | Meaning and next step |
|---|---|
| `Booting` (`Running` True) | The VMM runs and the guest has shown no boot evidence yet. If it stays: [Why did a VM fail to boot?](#why-did-a-vm-fail-to-boot) |
| A `Scheduled` or `StorageReady` reason | [Why is this VM not scheduled?](#why-is-this-vm-not-scheduled) |
| `Starting`, `ContainerCreating` | The Anchor container has not started. `kubectl describe pod <vm>-anchor` shows why, for example [CSI attach failure](#csi-attach-failure) |
| A failure reason (`KvmUnavailable`, `CgroupDriverUnsupported`, `BlockDeviceAccessDenied`, `DiskInUse`, `IdentityConflict`, `ProtocolIncompatible`, `VmmStartFailed`, and the others in [Failure reasons](api.md#failure-reasons)) | The previous run of the Anchor container ended with this reason; kubelet restarts it with back-off. See the failure modes below |
| `CrashLoopBackOff` | kubelet's back-off between such restarts; the message carries the last failure |
| `NodeUnreachable` (Unknown) | [NodeUnreachable](#nodeunreachable) |
| `Stopping` | The Anchor Pod is being deleted; the guest is shutting down (at most 90 s) |

### Is networking healthy?

`NetworkReady` reports the host-side DHCP responder of the VM, not the
guest's own network stack:

```bash
kubectl get virtualmachine <vm> -n <namespace> -o jsonpath='{.status.conditions[?(@.type=="NetworkReady")]}{"\n"}'
```

True (`AddressServiceRunning`) is healthy. False or Unknown:
[NetworkReady False or Unknown](#networkready-false-or-unknown). To check
the data path itself, reach the guest from a pod, for example its SSH
port, as in [installation.md](installation.md#8-first-vm). Network
requirements and the data path: [networking.md](networking.md) and the
[CNI requirements](compatibility.md#cni-requirements).

### Which host runtime version is running?

```bash
kubectl get virtualizationnodes -o custom-columns=NODE:.metadata.name,VIRTD:.status.runtime.virtdVersion,COMMIT:.status.runtime.virtdGitCommit,HYPERVISOR:.status.runtime.hypervisorVersion
ssh <node-address> cat /usr/share/baryon/components.json
```

The `components.json` file of the host image lists the Baryon version and
commit, the Hadron and kairos-init images, the Kubernetes distribution, and
the Cloud Hypervisor, firmware, and virtiofsd versions. The controller and
node agent builds are in their `starting` log lines. All places:
[Identifying versions](compatibility.md#identifying-versions).

### Is controller/runtime skew supported?

Within a runtime protocol major version, yes: any 1.x host image works with
any 1.x cluster release (ADR 0012). The v0.1 series speaks major 1. Tested
pairs and the minimum host image for each cluster release are in
[Which combinations work](compatibility.md#which-combinations-work). An
older host image works but lacks the host-side safety fixes of newer
releases, and status that a newer protocol minor added is missing
(`NetworkReady` Unknown, `RuntimeDoesNotReport`). Different major versions
are refused: [runtime protocol mismatch](#runtime-protocol-mismatch).

### Why did a VM fail to boot?

Two cases:

- The VMM did not start or ended: `Running` False or `Ready` False with a
  failure reason such as `VmmStartFailed`, `VmmFailed`, `VmmKilled`,
  `OutOfMemory`, `KvmUnavailable`, or `CgroupDriverUnsupported`. Read the
  message, the Anchor container's last termination message, and
  `journalctl -u baryon-virtd` on the node; for `VmmStartFailed` and
  `VmmFailed`, also the Cloud Hypervisor log and output in the VM's log
  directory ([Where to look](#where-to-look)).
- The VMM runs and the guest shows no boot evidence: `Running` True,
  `Ready` False with `Booting` for a long time. Read the guest's serial
  console, `console.log` in the VM's log directory on the node. The guest
  must be a UEFI-bootable raw disk image with a DHCP client on its first
  NIC, or cloud-init ([guest requirements](compatibility.md#guest-requirements));
  `Ready` turns True when the guest takes its DHCP lease or prints a login
  prompt on `ttyS0`. The importer writes the downloaded bytes as they are
  and converts nothing, so a qcow2 or compressed image does not boot.

`GuestPoweredOff` means the guest shut itself down; the Anchor container
exits and kubelet starts it again, which restarts the VM. To keep a VM off,
set `spec.runPolicy: Stopped`.

### Why was an image import rejected?

Two kinds:

- The importer failed: `StorageReady` False, `ImportFailed`
  ([disk import failure](#disk-import-failure)).
- baryon-controller refused to import into the volume: `StorageReady`
  False with `DiskNotProvisionedForClaim`, `DiskInitializedForAnotherClaim`,
  `DiskSourceMismatch`, `DiskImportRecordInvalid`, `ImportStateUnknown`, or
  `DiskBindingPending` ([import refusals](#import-refusals)).

### Why is a disk claim already in use?

One running VM per claim is the rule. Three checks enforce it:
[ClaimInUse](#claiminuse) in baryon-controller (another VM's pod mounts the
claim), [DiskInUse](#diskinuse) in baryon-virtd (another VM on the node
uses the same block device), and Kubernetes' multi-attach refusal for
`ReadWriteOnce` volumes across nodes ([CSI attach failure](#csi-attach-failure)).

## Failure modes

### KVM unavailable

Symptom: VirtualizationNode `Ready` False, reason `KvmUnavailable`, with
the host's reason as message: `/dev/kvm does not exist`, `/dev/kvm is not
a character device`, `unexpected KVM API version ...`, or
`cannot query /dev/kvm: ...`. The node agent marks the node's VM and import
slots unhealthy, so no new VM is placed there (`Insufficient
virtualization.baryon.io/vm` in the scheduler message). A VM can also
report `KvmUnavailable` as a failure reason when its VMM could not use
`/dev/kvm`.

Confirm on the node:

```bash
ssh <node-address> ls -l /dev/kvm
ssh <node-address> grep -c -E 'vmx|svm' /proc/cpuinfo
```

A count of 0 means the CPU does not expose hardware virtualization to the
operating system. baryon-virtd only warns at startup when `/dev/kvm` is
missing (`KVM device missing; VMs cannot start on this host`); udev loads
the KVM vendor module from the CPU, and the node agent asks again on every
refresh.

Fix: enable VT-x or AMD-V in the machine's firmware; for a node that is a
VM, enable nested virtualization in the hypervisor that runs it. Reboot
the node. The condition turns True at the next node agent refresh.

### Unsupported cgroup driver

Symptom: the VM is not Ready; `Running` and `Ready` False with reason
`CgroupDriverUnsupported` and the message "the Anchor container cgroup
... is not a systemd scope; Baryon requires the kubelet systemd cgroup
driver". Every VM on the node fails the same way; the VirtualizationNode
stays `Ready` True.

Confirm: the message names a cgroupfs path (for example
`/kubepods/...` instead of a `.scope` under a `.slice`). On k0s, check the
containerd drop-in on the node:

```bash
ssh <node-address> sudo cat /etc/k0s/containerd.d/baryon.toml
```

Fix: the kubelet systemd cgroup driver is required; cgroupfs is Not
supported. On k0s v1.36.4, `cgroupDriver: systemd` in the worker profile is
not enough: containerd also needs `SystemdCgroup = true` for runc, through
the drop-in in [k0s node configuration](compatibility.md#k0s-node-configuration)
(the same file sets device ownership for block disks). Write the file, then
reboot the node, so that kubelet and containerd start with it.

### Runtime protocol mismatch

Symptom: VirtualizationNode `Ready` and `RuntimeHealthy` False with reason
`ProtocolIncompatible`; the message names both versions. The node's slots
are unhealthy, so new VMs go to other nodes or stay `Unschedulable`. VMs
already running on the node keep running. A VM whose anchor reaches the
node reports `Running` and `Ready` False with `ProtocolIncompatible`.
`ProtocolIncompatible` also covers a host report whose schema version the
node agent cannot read.

Confirm: compare the host runtime version with the cluster release
([Which host runtime version is running?](#which-host-runtime-version-is-running)).
Different protocol major versions are refused by design (ADR 0012). Status:
Designed and Implemented with unit tests; no second major version exists
yet, so it is not tested end to end.

Fix: bring the host image and the cluster release to the same protocol
major version: upgrade the host image through Kairos, or apply the matching
cluster release ([Upgrade order](compatibility.md#upgrade-order)).

A VM that reports `UnsupportedFeature` runs on a host that lacks a runtime
feature it needs; upgrade the host image. Within the 0.1 series this
cannot occur.

### Cloud Hypervisor or firmware missing

baryon-virtd checks at startup that `/sys/fs/cgroup` is a cgroup v2 mount,
that `/usr/bin/cloud-hypervisor` exists, and that the firmware
`/usr/share/cloud-hypervisor/CLOUDHV.fd` exists. If one is missing it exits
with an error. systemd restarts it after 2 s (`Restart=always`) and, under
its default start rate limit, can give up and leave the unit failed. The
Baryon-enabled Hadron
image contains all three; this happens with a host image that was modified
or built incorrectly.

Symptom:

- `journalctl -u baryon-virtd` on the node repeats one of
  `Cloud Hypervisor binary /usr/bin/cloud-hypervisor not found`,
  `firmware /usr/share/cloud-hypervisor/CLOUDHV.fd not found`, or
  `/sys/fs/cgroup is not a cgroup v2 mount; Baryon requires the unified hierarchy`.
- baryon-virtd creates its socket directories only after these checks, so
  after a boot the node agent pod of the node stays `ContainerCreating`
  with a `FailedMount` event for its `/run/baryon/node` host path, and the
  node's VirtualizationNode is missing or its heartbeat ages. If
  baryon-virtd ran earlier in the same boot, the node agent runs and
  reports `Ready` False, `RuntimeUnreachable`.

Confirm:

```bash
ssh <node-address> systemctl status baryon-virtd
ssh <node-address> ls -l /usr/bin/cloud-hypervisor /usr/share/cloud-hypervisor/CLOUDHV.fd
ssh <node-address> cat /usr/share/baryon/components.json
```

Fix: boot a correct Baryon-enabled Hadron image. Host software is never
installed after boot (ADR 0003); upgrade the node through Kairos to an
image built with `hack/build-hadron-image.sh`, or to the published image.

### Node runtime unavailable

Symptom, one of:

- VirtualizationNode `Ready` and `RuntimeHealthy` False, reason
  `RuntimeUnreachable`: the node agent cannot get a host report from
  baryon-virtd; the message is the error.
- `Ready` and `RuntimeHealthy` Unknown, reason `NodeAgentStopped`: the node
  agent was stopped (for a reboot, an upgrade, or a DaemonSet rollout).
- The Heartbeat column keeps growing past 90 s: the node agent crashed, its
  pod has not started yet ([after repeated reboots](#node-agent-pod-delayed-after-repeated-reboots)),
  or the node is down, and the conditions are stale.

In all three cases the node takes no new VMs. Running VMs keep running and
stay `Ready`; their `NetworkReady` becomes Unknown (`RuntimeUnreachable`)
once the node's report is older than 90 s.

Confirm:

```bash
kubectl get virtualizationnode <node> -o jsonpath='{.status.heartbeatTime}{"\n"}'
kubectl get pods -n baryon-system -o wide --field-selector spec.nodeName=<node>
ssh <node-address> systemctl status baryon-virtd
ssh <node-address> sudo journalctl -u baryon-virtd -n 100
```

Fix: start baryon-virtd if it is stopped
(`ssh <node-address> sudo systemctl restart baryon-virtd`; systemd also
restarts it after a failure), or find why the node agent pod is not
running. The
node is `Ready` again at the next refresh, and VMs on it reattach.

### Node agent pod delayed after repeated reboots

Symptom: after a node was rebooted several times within a short time, the
node is `Ready` but its baryon-node-agent pod does not start for minutes.
Its VirtualizationNode keeps an old heartbeat or shows `NodeAgentStopped`,
no new VM is placed on the node, and the node's VMs wait. CSI node plugins
deployed as DaemonSets can be delayed the same way, so a VM's volume
cannot be mapped yet (for example `MapVolume` failed because the driver is
not registered).

Cause: Kubernetes, not Baryon. During the shutdown the DaemonSet
controller creates a replacement pod that kubelet rejects (`NodeShutdown`,
pod `Failed`), and the DaemonSet controller backs off per node before it
creates the next pod. Observed on the k0s cluster: minutes after repeated
reboots of the same node within a short time; well under a minute on a
first reboot.

Confirm:

```bash
kubectl get pods -n baryon-system -o wide --field-selector spec.nodeName=<node>
kubectl get events -n baryon-system --field-selector involvedObject.kind=Pod
kubectl get events -n baryon-system --field-selector involvedObject.kind=DaemonSet,reason=FailedDaemonPod
```

Fix: wait. The DaemonSet controller creates the pod when its backoff
expires; the node agent then reports, and the VMs on the node start again
without intervention.

### No compatible node

Symptom: `Scheduled`, `Running`, and `Ready` False with reason
`Unschedulable` and the scheduler's message, for example
`0/3 nodes are available: 3 Insufficient virtualization.baryon.io/vm`, or
`Insufficient virtualization.baryon.io/import` while the import Pod waits.
Baryon has no reason of its own for this; the scheduler's message says
which constraint excluded which nodes.

Confirm:

```bash
kubectl describe pod <vm>-anchor -n <namespace>    # or <vm>-import
kubectl get nodes -L virtualization.baryon.io/host
kubectl get nodes -o custom-columns='NAME:.metadata.name,VMS:.status.allocatable.virtualization\.baryon\.io/vm'
```

Causes and fixes:

- No node has the label `virtualization.baryon.io/host=true`, so the node
  agent runs nowhere and no node has the `virtualization.baryon.io/vm`
  resource. Set the label in the node's cloud-config (`--labels` for k0s)
  ([installation.md](installation.md#cloud-config)).
- The resource is 0 on every eligible node: the runtime is not healthy
  there ([Is this node virtualization-capable?](#is-this-node-virtualization-capable)).
- The VM's `spec.placement` (node selector, affinity, tolerations,
  topology spread) excludes the remaining nodes, or every eligible node is
  cordoned (VM placement cannot tolerate the cordon taint). Placement
  changes apply at the next start ([Placement](api.md#placement)).
- Insufficient CPU or memory for the class: the Anchor Pod requests the
  class's vCPUs and memory.
- A VM on a node-local volume (OpenEBS LVM LocalPV) can start only on that
  volume's node; it waits while that node is down or cordoned.

### CSI attach failure

Symptom: `Running` and `Ready` False with reason `ContainerCreating` (or
`Starting`), and the Anchor Pod stays `ContainerCreating`. Its Events show
`FailedAttachVolume` or `FailedMount`, for example a Multi-Attach error
because a `ReadWriteOnce` volume is still attached to another node. Before
scheduling, `StorageReady` False with `ClaimPending` means the claim is not
bound; with `WaitForFirstConsumer` claims this is normal until the import
Pod or the Anchor Pod is scheduled.

Confirm:

```bash
kubectl describe pod <vm>-anchor -n <namespace>
kubectl get pvc -n <namespace> "$(kubectl get virtualmachine <vm> -n <namespace> -o jsonpath='{.status.rootDiskClaim}')"
kubectl get volumeattachments
```

Fix: these are Kubernetes and CSI states, not Baryon's. A volume moves to
another node only after the CSI driver has detached it from the old one.
After a node reboot or upgrade, the CSI controller may need to regain its
leader lease first (observed with Ceph RBD,
[kairos-integration.md](kairos-integration.md#runbook-upgrade-one-node-with-kairos-operator)).
For a failed node, see [NodeUnreachable](#nodeunreachable). On k0s, CSI
node plugins must use the kubelet root `/var/lib/k0s/kubelet`.

### Disk import failure

Symptom: `StorageReady` False with reason `ImportFailed`; `Scheduled`,
`Running`, and `Ready` carry the same reason while there is no Anchor Pod.
The message is the importer's error. Messages the importer gives include:

| Message | Cause |
|---|---|
| `GET <url> returned 404 Not Found` (or another status) | The URL is wrong or not reachable from the pod network |
| `SHA-256 mismatch: expected <a>, got <b>` | The image content differs from `sha256`, for example a `latest` URL that changed |
| `image is <n> bytes, larger than the requested disk size <size>` | `rootDisk.size` is smaller than the raw image |
| `the download ended after <n> of <m> bytes` | The connection closed early |
| `the server sent no Content-Length and no SHA-256 was given; ...` | Set `sha256` so a truncated download can be detected |
| `refusing redirect from <https-url> to non-HTTPS <url>` | The server redirects HTTPS to plain HTTP |
| `the import was not approved within ... s` | baryon-controller did not approve the import in time; check its log |

An importer that ended without a message (for example killed for exceeding
its memory) is reported with its exit code. A message can contain the
source URL; Events never do.

Confirm:

```bash
kubectl describe virtualmachine <vm> -n <namespace>
kubectl logs <vm>-import -n <namespace> -c import
kubectl -n baryon-system logs deploy/baryon-controller | grep '<vm>'
```

Fix: `rootDisk` is immutable, so a wrong URL, digest, or size needs a new
VirtualMachine: delete this one and create it again with the corrected
`rootDisk`. The import Pod has `restartPolicy: OnFailure`, so kubelet
retries a failed importer with back-off, and a network problem that goes
away resolves itself.
The importer needs egress from its pod to the image URL; a NetworkPolicy
that selects the VM's labels applies to the importer too
([api.md](api.md#labels-on-objects-baryon-creates)).

### Import refusals

Symptom: `StorageReady` False with one of the reasons below; nothing is
imported and the VM does not start. baryon-controller looks at such a VM
again every 120 s (every 30 s in `DiskBindingPending`). Meanings are in
[Import refusals](api.md#import-refusals) and the rule behind them in
[storage.md](storage.md#import-record).

| Reason | Typical cause | Fix |
|---|---|---|
| `DiskNotProvisionedForClaim` | The volume was not dynamically provisioned for this claim: a pre-created, restored, or cloned volume, a claim with `dataSource`, or a provisioner that does not set `pv.kubernetes.io/provisioned-by` | To use existing data, reference the claim with `rootDisk.source.persistentVolumeClaim` in a new VirtualMachine. Otherwise use a provisioner that sets the annotation ([CSI requirements](compatibility.md#csi-requirements)) |
| `DiskInitializedForAnotherClaim`, `DiskSourceMismatch` | The volume's import record names another claim or another source | Adopt the disk with `rootDisk.source.persistentVolumeClaim`, or give the VM a fresh volume |
| `DiskImportRecordInvalid` | The record annotation on the PersistentVolume cannot be read | Do not edit `virtualization.baryon.io/disk-import`; restore it from a backup of the PersistentVolume or adopt the disk |
| `ImportStateUnknown` | A block-mode claim that held a disk has no record on its volume, for example after a restore that dropped PersistentVolume annotations | Restore the record, or adopt the disk with `rootDisk.source.persistentVolumeClaim` |
| `DiskBindingPending` | A VirtualMachine of an earlier release is being migrated, and the claim and volume are not yet bound to each other | Wait; it resolves when they are bound |

### ClaimInUse

Symptom: `Scheduled`, `Running`, and `Ready` False with reason
`ClaimInUse` and the message "claim <claim> is used by pod <pod> of another
VirtualMachine". baryon-controller creates no pod for this VM until the
other pod is gone. Status: Tested (storage E2E on the k0s cluster).

Confirm: find the other VM's pod named in the message.

```bash
kubectl get pod <pod> -n <namespace> -o jsonpath='{.metadata.labels}{"\n"}'
```

Fix: stop or delete the other VirtualMachine, or give this VM its own
claim. One running VM per claim; `ReadWriteMany` block claims shared across
nodes are Not supported.

### DiskInUse

Symptom: `Running` and `Ready` False with reason `DiskInUse`: another VM on
the same node already uses the same block device. This is baryon-virtd's
backstop for two VMs that passed the `ClaimInUse` check at the same time.
Status: Implemented, unit tests; not exercised on a cluster.

Confirm and fix as for [ClaimInUse](#claiminuse): find which other VM uses
the claim (`kubectl get virtualmachines -A -o custom-columns=NAME:.metadata.name,NS:.metadata.namespace,CLAIM:.status.rootDiskClaim`)
and stop one of them.

### Raw block path rejected

Symptom: a VM with a block-mode root disk is not Ready; `Running` and
`Ready` False with reason `BlockDeviceAccessDenied`: the block device node
is not readable and writable by the VM's host identity. Filesystem-mode
disks are not affected.

Cause: kubelet does not apply `fsGroup` to raw block devices. containerd
must give block devices the pod's `runAsUser` and `runAsGroup`, which it
does only with `device_ownership_from_security_context = true`.

Confirm: on k0s, check `/etc/k0s/containerd.d/baryon.toml` on the node as
in [unsupported cgroup driver](#unsupported-cgroup-driver).

Fix: add the setting through the containerd drop-in in
[k0s node configuration](compatibility.md#k0s-node-configuration) and
reboot the node. The setting is node-wide.

A related reason, `StorageFailed`, means baryon-virtd refused to hand the
disk to the VMM because the path, the file kind, or the device node is not
one the Anchor container uses for its disk (for example a device node
outside the container's `/dev`). It is a safety check against a tampered
anchor; report it with the baryon-virtd log if it appears for an ordinary
VM.

### ClaimRejected and PodRejected

Symptom: `Scheduled`, `Running`, and `Ready` False (and for
`ClaimRejected` also `StorageReady`) with reason `ClaimRejected` (the API
server refused the root disk claim) or `PodRejected` (it refused the
import Pod or the Anchor Pod). The message is the API server's, cut to 1024
bytes. baryon-controller tries again every 60 s.

Typical causes: an exceeded ResourceQuota, an admission policy that does
not allow the claim's StorageClass, an unknown PriorityClass in
`spec.placement.priorityClassName`, or a namespace Pod Security level
stricter than the pods need (Anchor Pods meet `restricted`).

Fix: correct the quota, the policy, or the namespace. `storageClassName`
is part of the immutable `rootDisk`; a wrong StorageClass needs a new
VirtualMachine. Placement can be changed and applies at the next start.

### IdentityConflict

Symptom: `Running` and `Ready` False with reason `IdentityConflict`. Another
VM on that node runs with the same host identity, so baryon-virtd refused
this one there before starting anything for it: no VMM, no DHCP responder,
no runtime directory, and the root disk was not opened.

What happens with a current anchor: it exits with code 3, and
baryon-controller records the node in `status.identityConflictNodes`,
emits an `IdentityConflict` Event, and replaces the Anchor Pod with node
affinity that keeps it off the recorded nodes. No action is needed while
another node fits the VM and its storage. On a cluster where every
fitting node is recorded (for example a VM on node-local storage), the VM
stays not ready.

Confirm:

```bash
kubectl get virtualmachine <vm> -n <namespace> -o jsonpath='{.status.identityConflictNodes}{"\n"}'
kubectl get events -n <namespace> --field-selector involvedObject.name=<vm>,reason=IdentityConflict
```

Fix when no node is left: stop the VM (`runPolicy: Stopped`, which clears
the list) and start it again after the conflicting VM is gone
([Host identity](api.md#host-identity)).

#### An Anchor Pod with the v0.1.0-rc1 anchor image

After a cluster upgrade from v0.1.0-rc1, Anchor Pods created by the
v0.1.0-rc1 controller keep running with the v0.1.0-rc1 anchor image until
they are replaced. Such an anchor exits with code 1, not 3, when it is
refused for an identity conflict, and baryon-controller acts on the
conflict only with code 3 (other processes of the container can write the
message, ADR 0015). The conflict happens only when the anchor connects to
baryon-virtd again, for example after the anchor container or baryon-virtd
restarts, while another VM with the same identity runs on the node.

Recognize it by all of these:

- `Ready` and `Running` False with reason `IdentityConflict`, and
  `RestartRequired` True with reason `AnchorImageChanged`;
- `status.identityConflictNodes` does not list the node;
- the Anchor Pod restarts with back-off, its last exit code is 1, and its
  termination message starts with `IdentityConflict:`:

```bash
kubectl get pod <vm>-anchor -n <namespace> -o jsonpath='{.status.containerStatuses[0].lastState.terminated.exitCode} {.status.containerStatuses[0].lastState.terminated.message}{"\n"}'
```

Recover by deleting the Anchor Pod:

```bash
kubectl delete pod <vm>-anchor -n <namespace>
```

baryon-controller creates a new Anchor Pod with the current anchor image.
It runs if its node has no conflict; otherwise it exits with code 3 and
the VM is moved as described above. This is safe: baryon-virtd started
nothing for the refused anchor, so deleting its pod stops no VMM, leaves
nothing on the host, and does not touch the root disk; the new pod is the
only Anchor Pod of the VM, and its anchor is the VM's Anchor supervisor
(ADR 0015); import Pods are not involved. To avoid the case altogether,
replace every Anchor Pod that reports `RestartRequired` after the cluster
upgrade: the Kairos host upgrades do, and so does a restart of the VM.

Status: Tested on the k0s cluster by `tests/e2e/identity-conflict.sh`,
which injects a conflict with the published v0.1.0-rc1 anchor image and
with the current one.

### NotAnchorSupervisor

Symptom: `Running` and `Ready` False with reason `NotAnchorSupervisor`.
baryon-virtd refused the anchor: it is the init process of its container,
but not in the initial user namespace, or in baryon-virtd's mount or
network namespace (ADR 0015).

Cause: the node's container runtime does not meet the
[node requirements](compatibility.md#kubernetes-requirements), for example
a rootless runtime or one that runs containers in a user namespace.
Baryon pods cannot request a user namespace through the API (admission
refuses `hostUsers: false`), so the cause is node or runtime configuration.

Confirm on the node: `journalctl -u baryon-virtd` shows `attach refused`
with the pod UID and the reason.

Related symptom: the Anchor container restarts repeatedly, its log shows
`refusing to run` with "must be the container's init process", and the VM
reports no reason from baryon-virtd. The anchor is not PID 1 of its
container: the runtime injects an init process or shares a PID namespace
between containers. The anchor refuses before it connects and writes no
termination message.

### NetworkReady False or Unknown

| Status, reason | Meaning | What to do |
|---|---|---|
| False, `DhcpResponderRestarting` | The VM's DHCP responder stopped serving, stopped answering liveness probes, or broke its protocol; baryon-virtd restarts it with back-off of at most 60 s | Usually transient. Read the message and `dhcp.log` of the VM ([Where to look](#where-to-look)) |
| False, `DhcpResponderFailing` | It failed more than 10 times in a row; baryon-virtd keeps restarting it every 60 s | Read `journalctl -u baryon-virtd` and `dhcp.log` on the node. The guest keeps its address until the lease time in the message |
| Unknown, `RuntimeUnreachable` | The node has no fresh report, or its `RuntimeHealthy` is not True | [Node runtime unavailable](#node-runtime-unavailable) |
| Unknown, `RuntimeDoesNotReport` | The node's baryon-virtd speaks runtime protocol 1.1 or earlier | Upgrade the host image to get the status |
| Unknown, `NotReported` | No entry for the current Anchor Pod yet; normal early in a start | Wait one node agent interval (at most 30 s) after the responder's first probe answer |
| Unknown, `NotRunning` | The VM is not running | none |
| Unknown, `NodeUnreachable` | The Anchor Pod's node stopped reporting | [NodeUnreachable](#nodeunreachable) |

`NetworkReady` does not change `Ready`. baryon-controller emits a Warning
Event `NetworkNotReady` each time it turns False. Details:
[NetworkReady](api.md#networkready).

### NodeUnreachable

Symptom: `Running`, `Ready`, and `NetworkReady` Unknown with reason
`NodeUnreachable`: Kubernetes marked the Anchor Pod not ready because its
node stopped reporting. Whether the guest still runs is unknown. A VM that
was still booting keeps `Ready` False (`Booting`) instead, and only
`NetworkReady` shows the loss.

What happens: baryon-controller creates no new Anchor Pod while the old
pod object exists (fencing, ADR 0004), so the same VM never runs twice.
After taint-based eviction the old pod stays Terminating until kubelet on
the node confirms the deletion.

Fix: bring the node back; kubelet confirms the deletion and a new Anchor
Pod starts the VM. If the node is permanently gone, first make sure the
machine is off, then delete the old Anchor Pod
(`kubectl delete pod <vm>-anchor -n <namespace> --force --grace-period=0`)
or the Node object; deleting either while the guest still runs would run it
twice. Details and evidence: [lifecycle contract](kairos-integration.md#lifecycle-contract).

### k0s starts without its configuration on the first boot

Symptom, on the first boot of the active system after provisioning: the
controller node does not appear in `kubectl get nodes` (k0s runs without
`--enable-worker`), or a worker never joins and its `k0sworker` log
reports "no join token". Observed on the k0s cluster with the Kairos k0s
provider of kairos-init v4.3.0: the service started before the provider
wrote its systemd override (arguments and token file). Later boots are not
affected. Not yet reported upstream
([kairos-integration.md](kairos-integration.md#kairos-kubernetes-providers)).

Confirm on the node:

```bash
systemctl cat k0scontroller    # on the controller; or k0sworker on a worker
sudo journalctl -u k0sworker -n 50
```

Fix:

```bash
sudo systemctl daemon-reload
sudo systemctl restart k0scontroller   # on the controller
sudo systemctl restart k0sworker       # on a worker
```

## Observing Baryon

No Baryon component serves metrics. No Baryon component listens on TCP,
by design ([security.md](security.md#trust-boundaries)); the state an
operator needs is in the VirtualMachine and VirtualizationNode objects and
in Events.

`examples/kube-state-metrics.yaml` is a kube-state-metrics custom resource
state configuration that turns that state into Prometheus metrics. Pass it
with `--custom-resource-state-config-file`, and grant kube-state-metrics
`list` and `watch` on the Baryon resources, for example:

```yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: kube-state-metrics-baryon
rules:
  - apiGroups: [virtualization.baryon.io]
    resources: [virtualmachines, virtualizationnodes]
    verbs: [list, watch]
  - apiGroups: [apiextensions.k8s.io]
    resources: [customresourcedefinitions]
    verbs: [list, watch]
```

Bind it to the service account kube-state-metrics runs as with a
ClusterRoleBinding. Status: the configuration is Tested against the k0s
cluster with kube-state-metrics v2.20.0, which produced these metrics:

| Metric | Labels | Value |
|---|---|---|
| `baryon_vm_condition` | `name`, `namespace`, `condition`, `reason`, `status` | 1 for the condition's current status, 0 for the other two |
| `baryon_vm_anchor_restarts` | `name`, `namespace` | `status.restartCount` |
| `baryon_vm_info` | `name`, `namespace`, `node`, `class`, `run_policy` | 1 |
| `baryon_node_condition` | `node`, `condition`, `reason`, `status` | as for VMs, for `Ready` and `RuntimeHealthy` |
| `baryon_node_vms` | `node` | `status.vmCount` |
| `baryon_node_heartbeat_timestamp_seconds` | `node` | `status.heartbeatTime` in Unix seconds |
| `baryon_node_runtime_info` | `node`, `virtd_version`, `virtd_git_commit`, `hypervisor_version` | 1 |

kube-state-metrics adds its own labels as configured. Questions these
metrics answer, with example PromQL (the queries are examples, not part of
the test):

| Question | Example |
|---|---|
| How many VMs are in each `Ready` state and reason? | `sum by (status, reason) (baryon_vm_condition{condition="Ready"})` |
| Which VMs fail to launch? | `Ready` False with a failure reason: `baryon_vm_condition{condition="Ready",status="False",reason!~"Booting\|Stopped"} == 1`; restarts of the Anchor container: `baryon_vm_anchor_restarts > 0` |
| Which VMs have a failing guest network service? | `baryon_vm_condition{condition="NetworkReady",status="False"} == 1` |
| Which nodes cannot run VMs, and why (including `ProtocolIncompatible`)? | `baryon_node_condition{condition="Ready",status!="True"} == 1` |
| Which nodes stopped reporting? | `time() - baryon_node_heartbeat_timestamp_seconds > 90` |
| Which import failures and storage conflicts exist? | `baryon_vm_condition{condition="StorageReady",status="False"} == 1`, and `Ready` reasons such as `ClaimInUse` |
| Which runtime versions run where? | `count by (virtd_version, hypervisor_version) (baryon_node_runtime_info)` |

Not available, Planned:

- VM launch duration (from creation or start to `Ready`)
- baryon-virtd reconnect counts of anchors and node agents
- baryon-controller reconcile error counts; reconcile errors are in its
  log only

A metrics endpoint in a Baryon component would need a decision on a TCP
listener, which the current security model excludes
([security.md](security.md)).
