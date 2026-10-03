# Baryon architecture

Baryon is a Kubernetes-native virtualization platform built for Hadron and
Cloud Hypervisor. Kubernetes manages the VM. Hadron provides the
virtualization host. Cloud Hypervisor executes the VM. Kairos manages the
machine lifecycle around them.

This document describes the architecture as implemented and marks the status
of each part:

| Label | Meaning |
|---|---|
| Implemented | Code exists and runs. |
| Tested | Implemented and covered by a test. The test kind is named. |
| Experimental | Implemented for evaluation. May change or be removed. |
| Designed | Recorded in this document or an ADR. No implementation yet. |
| Planned | Intended, not designed in detail. |
| Not supported | Out of scope or unavailable. |

The runtime boundary between the VM Anchor Pod, baryon-virtd, and Cloud
Hypervisor was decided by experiment; the evidence is in
[runtime-boundary.md](runtime-boundary.md). Every Tested label below is
single-node evidence summarized in [testing.md](testing.md#results), from
one Baryon-enabled Hadron node running k3s with flannel ("Hadron node") and,
where named, from a historical development cluster in which a kind node
container acted as the host ("kind host", no longer used). Multi-node
testing on Hadron nodes running k0s is in progress.

## Scope

Baryon is:

- a Kubernetes API for virtual machines (`virtualization.baryon.io`)
- a controller that turns that API into Kubernetes resources
- a node agent that publishes host capabilities and serves a kubelet device
  plugin
- a host runtime, baryon-virtd, that runs Cloud Hypervisor on Hadron nodes
- a Hadron image layer that delivers the host runtime and its dependencies

Baryon is not a generic virtualization abstraction, a general-purpose host
virtualization management daemon, a VMM plugin system, a CNI, a CSI driver, a Kubernetes distribution, an OS
installer, an OS upgrade system, or a Cluster API replacement. Cloud
Hypervisor is the only VMM.

## Layers and ownership

```
+------------------------------------------------------------------+
| Baryon Kubernetes API                                            |
|   VirtualMachine, VirtualMachineClass, VirtualizationNode        |
+------------------------------------------------------------------+
| Baryon runtime                                                   |
|   baryon-controller   (Deployment, unprivileged)                 |
|   VM Anchor Pod       (one per running VM, baryon-anchor)        |
|   baryon-node-agent   (DaemonSet: device plugin, node status)    |
|   baryon-virtd        (Hadron systemd service, host runtime)     |
+------------------------------------------------------------------+
| Kubernetes                                                       |
|   scheduler, kubelet, containerd, CNI, CSI, admission            |
|   k3s, k0s, or kubeadm through provider-kubernetes               |
+------------------------------------------------------------------+
| Hadron + Kairos (Baryon-enabled image)                           |
|   kernel, systemd, KVM, IOMMU, VFIO, TUN, hugepages              |
|   Cloud Hypervisor, UEFI firmware, virtiofsd, baryon-virtd       |
|   Kairos agent, immucore, A/B images                             |
+------------------------------------------------------------------+
| Machine lifecycle                                                |
|   AuroraBoot, Kairos Operator, Cluster API, Kairos providers     |
+------------------------------------------------------------------+
```

The diagram expresses ownership, not runtime dependencies. At runtime Baryon
depends on Kubernetes and on the Baryon-enabled Hadron image. It does not
depend on AuroraBoot, Kairos Operator, or Cluster API. See
[kairos-integration.md](kairos-integration.md).

| Responsibility | Owner | Notes |
|---|---|---|
| VM scheduling, placement, priority, taints, affinity | Kubernetes scheduler, through the VM Anchor Pod | Baryon never chooses a node. It only excludes nodes where the VM hit a host identity conflict (`status.identityConflictNodes`, ADR 0011). |
| Which nodes can run VMs | kubelet, through the `virtualization.baryon.io/vm` device plugin resource | Healthy only while baryon-virtd answers and KVM is usable. |
| Resource reservation and limits | Kubernetes, through the Anchor container's requests and limits | The VMM runs in the Anchor container's cgroup (ADR 0005). |
| Volume attach, mount, and lifecycle | Kubernetes and CSI, through the Anchor Pod | Baryon resolves the result inside the pod (ADR 0006). |
| Pod network | Kubernetes and CNI, through the Anchor Pod network namespace | Baryon attaches a TAP inside that namespace (ADR 0007). |
| VM API and reconciliation | baryon-controller | Only writer of VirtualMachine status. |
| Node virtualization capabilities | baryon-virtd discovers; baryon-node-agent publishes | Only writer of VirtualizationNode. |
| Cloud Hypervisor process lifecycle | baryon-virtd | |
| Storage and network handoff into the VMM | baryon-virtd | |
| Guest execution | Cloud Hypervisor | |
| Host OS content and upgrades | Hadron image, Kairos | Baryon installs nothing on the host after boot. |
| Bootable artifacts and provisioning | AuroraBoot | |
| Kubernetes-driven OS upgrades | Kairos Operator | Evacuation integration is Designed. |
| Kubernetes machine lifecycle | Cluster API, when used | Optional. |

## Components

### Kubernetes API (Implemented, see [api.md](api.md))

Group `virtualization.baryon.io`, version `v1alpha1`. CRDs are generated from
the Rust types in `crates/api` and kept in `deploy/base/crds/`.

- `VirtualMachine` (namespaced): user intent for one VM.
- `VirtualMachineClass` (cluster-scoped): reusable compute shape.
- `VirtualizationNode` (cluster-scoped, one per Baryon node): observed host
  capabilities and runtime health.

The API describes intent. It never carries Cloud Hypervisor arguments, host
paths, PIDs, TAP names, or socket paths.

### Admission policies (Implemented; Tested by the CI API server checks, on the Hadron node, and on the kind host)

Two ValidatingAdmissionPolicies in `deploy/base/policy/`, part of the supported
installation (see [security.md](security.md)):

- `vm-resource.yaml` (`baryon-vm-resource`): only the baryon-controller
  service account may create pods that request `virtualization.baryon.io/vm`.
- `node-agent.yaml` (`baryon-node-agent-own-node`): baryon-node-agent may
  write only the VirtualizationNode of the node named in its pod-bound
  service account token.

They need Kubernetes 1.30 or later, and 1.32 or later for the node name in
the token. Deployment is described in
[development.md](development.md#deploying-to-a-cluster-of-hadron-nodes).

### baryon-controller (Implemented; Tested by the lifecycle and deletion E2E on the Hadron node and the kind host)

A Deployment in `baryon-system`, one replica, non-root, no capabilities, no
host access. For each VirtualMachine it:

- resolves the VirtualMachineClass
- creates the root disk claim for imported disks (owned by the VM) or
  references an existing claim
- creates the Anchor Pod when `runPolicy` is `Running` and deletes it when
  `Stopped`, on deletion, or on a restart request. It acts on an existing pod
  named `<vm>-anchor` only if this VirtualMachine is its controller owner
  (owner reference with the VM UID, kind VirtualMachine, `controller: true`);
  otherwise it leaves the pod alone and reports `Ready=False` with reason
  `AnchorPodConflict`. Every pod delete carries a UID precondition.
- derives status from the Anchor Pod and the claim (ADR 0008) and records
  Events
- when baryon-virtd refuses the VM on a node for a host identity conflict
  (`IdentityConflict` in the anchor's termination message), records the
  node in `status.identityConflictNodes`, replaces the Anchor Pod, and keeps
  the new pod off the recorded nodes with required node affinity; the list
  is cleared when the VM is stopped (ADR 0011; Implemented, unit tests)
- holds a finalizer so that the Anchor Pod, and therefore the VMM, is gone
  before the VirtualMachine is removed

The controller never talks to baryon-virtd.

### VM Anchor Pod (Implemented)

One pod per running VM, named `<vm>-anchor`, in the VM namespace (ADR 0004):

- requests equal limits for every container (Guaranteed QoS); memory is
  guest memory plus VMM overhead
- one `virtualization.baryon.io/vm` resource: schedules the pod on a Baryon
  node, grants /dev/kvm, mounts the anchor socket
- the root disk claim as a volume
- the pod network
- runs as the VM's host identity, a distinct unprivileged uid and gid per
  VM, stable for the life of the VirtualMachine object (ADR 0011); the pod's
  `fsGroup` is the VM's gid
- labels `virtualization.baryon.io/vm-name`, `virtualization.baryon.io/vm-uid`,
  and `app.kubernetes.io/managed-by=baryon`
- no emptyDir: the readiness status file is in `/dev/shm/baryon-anchor`, so
  `kubectl drain` should not need `--delete-emptydir-data` (Inferred from
  kubectl's drain filter, not tested)
- termination grace period 90 s: 60 s for the guest shutdown plus 30 s for
  baryon-virtd's escalation and margin
- no service account token, no capabilities, no hostPath; Pod Security
  "restricted" (admission in a namespace with `enforce=restricted` Tested on
  the kind host)

Its container runs `baryon-anchor run`. An init container,
`baryon-anchor import`, writes the disk image on first start when the VM
imports its disk over HTTP. Before attaching, the anchor negotiates the
runtime protocol with baryon-virtd (ADR 0012): it sends its protocol version
and the features its VM needs, and ends with the termination message
`ProtocolIncompatible: ...` or `UnsupportedFeature: ...` when the host
cannot run the VM, so a cluster release and a host image that do not fit
are reported, not misread. A baryon-virtd without negotiation is treated as
protocol 1.0. After losing
baryon-virtd the anchor keeps its last readiness for 30 s
(`DISCONNECTED_GRACE`) before it reports not ready.

### baryon-node-agent (Implemented)

A DaemonSet on nodes labelled `virtualization.baryon.io/host=true`, running
as uid 0 without capabilities. It reads from baryon-virtd's node socket
(NodeService requires peer uid 0) and:

- serves the kubelet device plugin for `virtualization.baryon.io/vm`
  (64 slots by default; healthy only when baryon-virtd answers and KVM is
  usable)
- writes the VirtualizationNode for its node with a heartbeat

### baryon-virtd (Implemented; Tested by the lifecycle E2E on the Hadron node and the kind host)

The host virtualization runtime: a systemd service in the Baryon-enabled
Hadron image, running as root, not a Kubernetes client (ADR 0005). It:

- discovers host capabilities (crate `baryon-host`) on each request from the
  node agent
- identifies Anchor connections by `SO_PEERCRED` and `SO_PEERPIDFD` and
  derives pod UID, cgroup, network namespace, and mount namespace from the
  connecting process
- refuses an attach (gRPC `PermissionDenied`, `IdentityMismatch`) unless
  the anchor's uid and gid equal the host identity of the VM named in the
  request, and refuses a second VM with the same identity on the node
  (`IdentityConflict`)
- refuses to start a VM whose Anchor container cgroup is not a systemd
  scope inside a systemd pod slice (reason `CgroupDriverUnsupported`), as
  the first step of a start, before any network or storage work: only with
  the kubelet systemd cgroup driver does stopping the container stop the
  VMM before volume teardown while baryon-virtd is down. The refusal is
  Implemented; scope detection has unit tests, and no cgroupfs node was
  tested
- creates the TAP and the L2 passthrough in the pod network namespace, sets
  the stable VM MAC, and announces it with a gratuitous ARP
- opens a packet socket on the TAP and starts a per-VM DHCP responder
  process (`baryon-virtd dhcp-responder`, shown as `baryon-dhcp`) with the
  VMM's confinement; baryon-virtd does not parse guest frames itself
  (Tested on the Hadron node and the kind host)
- resolves the disk inside the pod mount namespace
- starts Cloud Hypervisor as the VM's host identity, without capabilities and with
  an empty bounding set, in a new, empty network namespace, in the Anchor
  container's cgroup, with seccomp and Landlock; the only extra Landlock
  rule makes the VM's own `vmm` runtime directory writable, which a guest
  reset needs (Tested on the Hadron node and the kind host)
- mounts a private tmpfs on that `vmm` directory (32 MiB, 256 inodes, mode
  0700, owned by the VM's identity, `nosuid,nodev,noexec`) and unmounts it when the
  VM ends, so a VMM cannot fill the host's `/run`; it reads and connects to
  everything there only through `crates/virtd/src/vmm_files.rs`
  (directory-relative, no symlinks, owner must be the VM's uid, one link,
  bounded reads)
- supervises the VMM, captures the serial console, reports status, stops the
  guest with ACPI and escalates, and re-adopts running VMMs after its own
  restart (a re-adopted VM counts as booted only if its record has boot
  evidence); on SIGTERM it exits after 2 s and leaves VMs running
- writes each VM's console log under `/var/log/baryon/vms/` and holds a
  shared BSD lock (`flock`) on the log directory of every running VM, so
  that the image's log retention (`systemd-tmpfiles`, 14 days) skips it;
  the lock is released when the VM ends and taken again for re-adopted VMs
  after a baryon-virtd restart (retention with a locked directory Tested on
  the Hadron node; the retake after a restart is Implemented, not tested)

### Cloud Hypervisor

Executes the VM. Baryon pins one version per release (v53.0 today) and uses a
narrow internal crate, `baryon-cloud-hypervisor`, to build the command line,
call the REST API on the Unix socket, and read the event monitor. There is no
VMM abstraction.

## Request flow

```
kubectl apply VirtualMachine
        |
        v
baryon-controller --> root disk claim (import) + Anchor Pod
        |
        v
kube-scheduler: node with a free virtualization.baryon.io/vm slot
        |
        v
kubelet: CSI mounts the claim, CNI sets up the pod netns,
         device plugin adds /dev/kvm and the anchor socket mount,
         init container imports the disk image
        |
        v
baryon-anchor --gRPC over Unix socket--> baryon-virtd
                                            |  peer credentials and pidfd:
                                            |  pod UID, uid/gid, netns, root, cgroup;
                                            |  uid/gid must be the VM's identity
                                            v
                         TAP + tc in pod netns, pod MAC = VM MAC,
                         gratuitous ARP for the pod IP,
                         disk O_PATH fd from the pod mount namespace,
                         Cloud Hypervisor as the VM's identity in the container cgroup,
                         in its own empty network namespace,
                         DHCP responder process on a packet socket on the TAP,
                         confined like the VMM
                                            |
baryon-anchor <------- VM status stream ----+
        |
        v
guest DHCP from the DHCP responder (pod IP, pod routes) -> anchor readiness
        |
        v
baryon-controller: VirtualMachine Ready=True, node, address
```

Deletion: the controller deletes the Anchor Pod. kubelet sends SIGTERM to
baryon-anchor, which asks baryon-virtd for a final stop: ACPI power button,
then after the grace period a VMM shutdown, SIGTERM, and SIGKILL. The anchor
exits after the VMM; with the systemd cgroup driver, stopping the container
scope stops anything left in the container cgroup before CNI and volume
teardown; the controller removes its finalizer when the pod is gone; the
claim is garbage collected.

## Source of truth

| State | Source of truth | Location |
|---|---|---|
| Desired VM state | Kubernetes API (VirtualMachine) | etcd |
| Placement | Kubernetes (Anchor Pod `spec.nodeName`) | etcd |
| VM disk content | the claim | CSI backend |
| Runtime status seen by the cluster | the Anchor Pod status, written by kubelet | etcd |
| Running VMM | the host kernel (process, pidfd) | node |
| Runtime records | baryon-virtd, reconstructable | `/run/baryon/vms/<pod-uid>/` (tmpfs) |
| Host capabilities | the host kernel | sysfs, procfs |

Runtime records are an index over kernel state. After a restart baryon-virtd
verifies every record (process start time, executable) and discards records
that no longer match.

## Communication

| From | To | Transport | Purpose |
|---|---|---|---|
| baryon-controller | Kubernetes API | HTTPS | VMs, Pods, claims, Events |
| baryon-node-agent | Kubernetes API | HTTPS | VirtualizationNode |
| kubelet | baryon-node-agent | gRPC, kubelet device plugin socket | slots, allocation |
| baryon-anchor | baryon-virtd | gRPC over `/run/baryon/anchor/virtd.sock` (0666; peer checks per connection and per attach) | attach, stop, VM status |
| baryon-node-agent | baryon-virtd | gRPC over `/run/baryon/node/virtd.sock` (0600 in a 0700 directory; peer uid 0 required) | host info, VM list |
| baryon-virtd | Cloud Hypervisor | command line, inherited fds, HTTP over its API socket in the root-only `ctl` directory of the VM runtime directory | VM operations |
| baryon-virtd | DHCP responder | inherited packet socket; lease as one JSON line on stdin; acknowledged leases as JSON lines on stdout, checked against the lease | guest addressing |

No component listens on TCP. The Cloud Hypervisor API is never exposed
outside baryon-virtd. The protocol is defined in `crates/runtime-api/proto/`
(ADR 0009).

## Crates

Dependency direction is strictly from binaries to libraries. Every binary
also depends on `baryon-version`.

```
baryon-controller --> baryon-api, baryon-runtime-api
baryon-node-agent --> baryon-api, baryon-host, baryon-runtime-api
baryon-anchor ------> baryon-runtime-api
baryon-virtd -------> baryon-runtime-api, baryon-host, baryon-cloud-hypervisor
```

| Crate | Purpose | Kubernetes dependency |
|---|---|---|
| `baryon-api` | CRD types, Conditions, quantities, CRD generation | yes (types only) |
| `baryon-host` | host capability discovery from sysfs and procfs | no |
| `baryon-cloud-hypervisor` | Cloud Hypervisor command line, API client, events | no |
| `baryon-runtime-api` | gRPC protocol, the controller-to-anchor spec, the per-VM host identity (`VmIdentity`), and the guest MAC derivation | no |
| `baryon-virtd` | host runtime daemon | no |
| `baryon-anchor` | Anchor Pod process and disk importer | no |
| `baryon-node-agent` | device plugin and VirtualizationNode publisher | yes |
| `baryon-controller` | cluster controller | yes |
| `baryon-version` | build information | no |

## Deliberate limits of v1alpha1

- One VM per Anchor Pod, one root disk, one network interface on the pod
  network.
- UEFI firmware boot only. The runtime protocol's `Boot` message offers only
  firmware boot; `baryon-cloud-hypervisor` can build a direct kernel boot
  command line, but neither the protocol nor the API exposes it (Planned).
- No live migration, snapshots, VFIO, hugepages, NUMA placement, or virtiofs
  (Not supported). Constraints on these are listed in
  [design-notes.md](design-notes.md).
- No leader election: the controller runs as a single replica.
- No VM placement controls (nodeSelector, tolerations, affinity, priority)
  in the API; Planned.
