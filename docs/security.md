# Security model

This is Baryon's threat model and the record of every privileged path. It is
maintained with the implementation. Each claim carries a status:

- Implemented: in the code.
- Tested: exercised on a single Baryon-enabled Hadron node running k3s with
  flannel ("Hadron node") and on a historical development cluster in which
  a kind node container acted as the host ("kind host", no longer used),
  unless the entry names another environment such as the "k0s cluster"
  (three Baryon-enabled Hadron nodes running k0s); see
  [testing.md](testing.md#results). Tested means the behavior was
  observed, not that it was verified under attack.
- Designed: decided, not implemented.
- Not supported: out of scope today.

Last full review: 2026-10-03 (baryon-security review of the privileged host
implementation and the runtime-boundary experiments), followed on the same
day by a review of block-mode storage, the image import, and the disk
handoff, and by a review of import integrity (where the import record
lives, what counts as a completed import, and who can approve one).
On 2026-10-04 the live migration evaluation (ADR 0013) and its experiment
were reviewed (see [Live migration](#live-migration-not-supported)), and so
were the DHCP responder liveness probes and Baryon's seccomp layer (see
[Baryon seccomp layer](#baryon-seccomp-layer)). On 2026-10-05 the v0.1 API
changes were reviewed, and the v0.1.0-rc1 release gate review covered the
release workflow, the supply chain, and the node agent's privileges (see
[Release and supply chain](#release-and-supply-chain)).
Findings and their resolution are listed in
[Review findings](#review-findings).
On 2026-10-27 ADR 0014 (guest configuration from a Secret and a serial
console through the Anchor Pod) and the move to Cloud Hypervisor v54.0
were reviewed, and on 2026-10-27 and 2026-10-28 two gate reviews covered
the M1 and M2 fixes (ADR 0015, Accepted). On 2026-10-28 the rewritten
ADR 0014 (`spec.guestConfig.userDataSecretRef`, the seed built by
baryon-anchor, the console through `pods/exec`; Accepted as a design) was
reviewed ([Review findings](#review-findings)).

## Trust boundaries

From least to most trusted on a node:

1. Guest OS. Untrusted. It reaches the host only through Cloud Hypervisor
   device emulation (virtio-blk, virtio-net, virtio-rng, serial, ACPI,
   firmware) and through the DHCP responder, a separate unprivileged
   process.
2. Cloud Hypervisor (the VMM). One process per VM, running as the VM's own
   unprivileged host identity with no capabilities, `no_new_privs`, an
   empty network namespace, Baryon's and Cloud Hypervisor's seccomp
   filters, and Landlock. Assumed compromisable by its guest; the
   confinement below is what limits that.
3. baryon-anchor. Unprivileged process in the Anchor Pod. Untrusted by
   baryon-virtd: every field it sends is validated, its identity must match
   the VM it names, and every resource it asks for is bounded by the pod
   cgroup and by what its uid can open. Lifecycle authority over its VM
   (`Attach`, `Stop`) belongs to it as the Anchor supervisor, not to other
   processes with the VM's identity (ADR 0015).
4. baryon-node-agent. DaemonSet pod, uid 0 without capabilities, reaching the
   host only through two mounts. Part of the node's trusted computing
   base: as the kubelet device plugin, its `Allocate` answer tells kubelet
   which host devices and host paths to give a container. A compromised
   node agent can give an Anchor container, and with it a VM identity,
   host block devices or other host paths, or register another resource
   name with kubelet. Treat it as root-equivalent on its node.
5. baryon-controller. Unprivileged Deployment, no host access.
6. baryon-virtd. Root on the host. The trusted computing base of the node
   runtime. It is not a Kubernetes client.
7. Kubernetes API server, and the host OS and kernel (the Hadron image).

Channels across these boundaries:

| From | To | Channel |
|---|---|---|
| guest | VMM | virtio devices, serial, ACPI, SMBIOS, firmware |
| guest | DHCP responder | DHCP frames on the TAP |
| baryon-virtd | DHCP responder | stdin pipe: the lease as one JSON line, then a liveness probe line `{"probe":<n>}` every 10 s. The pipe is non-blocking on baryon-virtd's side and a probe line is at most 31 bytes, below `PIPE_BUF`, so a write either completes or fails; a failed write counts as a missing answer. The responder reads lines of at most 256 bytes and ignores lines it cannot parse |
| DHCP responder | baryon-virtd | stdout pipe: JSON event lines of at most 4 KiB. An acknowledged lease, checked against the lease (the responder reports at most one per 60 s, and baryon-virtd processes at most one per 60 s), and one `Alive` answer per probe, sent by the DHCP serving loop and accepted only for probes sent to the current responder. Any other line (overlong, unparseable, an acknowledgement for another address, an answer to a probe not sent or a second answer) is a protocol violation: baryon-virtd stops reading, kills the responder, and restarts it with backoff |
| VMM | baryon-virtd | process exit, the Cloud Hypervisor REST API (baryon-virtd is the client), serial socket, event file |
| baryon-anchor | baryon-virtd | gRPC on `/run/baryon/anchor/virtd.sock` |
| baryon-node-agent | baryon-virtd | gRPC on `/run/baryon/node/virtd.sock` |
| baryon-controller, baryon-node-agent | API server | HTTPS with service account tokens |
| baryon-node-agent | API server | On SIGTERM, one write of its own VirtualizationNode setting `Ready` and `RuntimeHealthy` to Unknown (`NodeAgentStopped`), within 5 s, with the read `resourceVersion` as a precondition; same field manager and admission policy as the periodic write |
| API server | baryon-controller, then the VM's status | The message of a refused claim or pod (`ClaimRejected`, `PodRejected`), cut to 1024 bytes, copied into the VM's conditions in the VM's namespace |
| baryon-node-agent | API server | VirtualizationNode `status.vms` of its own node: per Anchor Pod UID, the DHCP responder state, a reason (at most 64 bytes), a message (at most 1024 bytes), and two times; no VM names, namespaces, addresses, PIDs, paths, or TAP names; at most 256 entries (CRD validation: `state` enum, `maxLength`, `maxItems`) |
| API server | baryon-controller | VirtualizationNode watch, read for the `NetworkReady` condition of the VMs on each node; the controller takes the reason from the state, never from the node's text, and caps the message at 1024 bytes |

No Baryon component listens on TCP. The Cloud Hypervisor API is never exposed
outside the node and is not reachable by the guest or the anchor. The
controller has no path to baryon-virtd.

## Processes and privileges

| Process | Runs where | Identity | Linux capabilities | Status |
|---|---|---|---|---|
| baryon-virtd | host, systemd unit | root | full root set | Implemented, Tested |
| Cloud Hypervisor | host process started by baryon-virtd | the VM's host identity (a distinct uid and gid per VM in the reserved range 1000000000 to 1999999999, ADR 0011), plus the `kvm` group | none; bounding set empty | Implemented, Tested |
| DHCP responder | host process started by baryon-virtd, one per VM | the same VM identity, no kvm group | none; bounding set empty | Implemented, Tested |
| baryon-anchor | Anchor Pod container | the same VM identity | drop ALL | Implemented, Tested |
| baryon-importer | import Pod container, only while an import is pending; its own image, not part of the Anchor image | the same VM identity | drop ALL | Implemented, Tested (k0s cluster) |
| baryon-node-agent | DaemonSet pod | uid 0 in the container | drop ALL | Implemented, Tested |
| baryon-controller | Deployment pod | uid 65532 | drop ALL | Implemented, Tested |

All pods set `allowPrivilegeEscalation: false`, `readOnlyRootFilesystem:
true`, and seccomp `RuntimeDefault`. No container is privileged or adds a
capability. The Anchor Pod and the import Pod meet Pod Security
`restricted`, have no service account token
(`automountServiceAccountToken: false`), and have no hostPath volume.

baryon-virtd keeps the full root capability set. Reducing it to the
capabilities it uses (namespace entry, TAP creation, cgroup writes, uid and
gid changes, packet sockets, the VMM tmpfs mounts) is future hardening.

## Who does what on the host

| Action | Done by | How | Status |
|---|---|---|---|
| Open /dev/kvm | Cloud Hypervisor, as the VM's identity | /dev/kvm is root:kvm; the VMM has the kvm group. The device plugin adds /dev/kvm to the Anchor container's device list, and the VMM runs in that container's cgroup. | Implemented, Tested |
| Probe /dev/kvm for discovery | baryon-virtd | open and `KVM_GET_API_VERSION` | Implemented, Tested |
| Create the TAP | baryon-virtd | `/dev/net/tun` and `TUNSETIFF` on a dedicated thread that entered the pod network namespace through the anchor pidfd; the fd is inherited by the VMM | Implemented, Tested |
| Change the pod interface MAC, add tc filters | baryon-virtd | netlink, in the pod network namespace | Implemented, Tested |
| Answer guest DHCP | the DHCP responder (`baryon-virtd dhcp-responder`, shown as `baryon-dhcp`), with the same confinement as Cloud Hypervisor but without the kvm group | baryon-virtd opens a packet socket bound to the TAP in the pod network namespace and hands it to the responder; it never parses guest DHCP itself | Implemented, Tested |
| Open PVC data | Cloud Hypervisor, as the VM's identity | baryon-virtd resolves the disk with `openat2(RESOLVE_IN_ROOT, RESOLVE_NO_MAGICLINKS)` under the anchor's `/proc/<pid>/root`, accepts only `/var/lib/baryon/disks/root/disk.img` as a regular file and `/dev/baryon/root` as a block device node on the container's `/dev` mount (`UnknownPath`, `KindMismatch`, `DeviceOutsideDev`, all reported as `StorageFailed`), refuses a block device another VM on the node uses (`DiskInUse`), and passes an `O_PATH` fd; the VMM reopens it through `/proc/self/fd/<n>` with its own credentials. For a block device the kernel's permission check at that reopen, and the Anchor container's device cgroup, which the VMM shares, decide access; baryon-virtd's `check_device_access` (`BlockDeviceAccessDenied`) only reports a likely failure early | Implemented, Tested (filesystem mode: Hadron node, kind host, k0s cluster; block mode: k0s cluster; refusal of a pod-made device node: hostile-disk E2E) |
| Import an image | the import Pod (`baryon-importer import`, an image the Anchor Pod does not have), as the VM's identity, in the pod network namespace | writes the claim before the first Anchor Pod exists, and only after its approval file equals the claim UID; requests the placement-only `virtualization.baryon.io/import` resource and receives neither /dev/kvm nor the anchor socket | Implemented, Tested with `baryon-anchor import` (deletion E2E checks the importer's view; import integrity E2E; k0s cluster); with the separate `baryon-importer`: Tested (import integrity, deletion, and anchor authority E2E; k0s cluster) |
| Approve an import | baryon-controller | sets the annotation `virtualization.baryon.io/import-approved` on the import Pod to the claim UID, at creation or once the claim is bound, after checking that the bound volume was dynamically provisioned for the claim (ADR 0006); the importer reads it through a downward API file. Only baryon-controller may set, change, or remove it (`baryon-import-approval`) | Implemented, Tested (CI API server checks; import integrity E2E on the k0s cluster) |
| Record a completed import | baryon-controller | adds the annotation `virtualization.baryon.io/disk-import` (`{"vmUID","claimUID","source"}`, `source` is `sha256:` and the hex SHA-256 of the URL) to the bound PersistentVolume, only when claim and volume are bound to each other and the import Pod's spec (import resource, mounted claim, `--url=` and `--claim-uid=` arguments) shows an importer that exited 0; then deletes the import Pod. The Anchor Pod is created only once the import Pod is gone. `baryon-controller-writes` limits the write to adding the record once | Implemented, Tested (storage and import integrity E2E, k0s cluster) |
| Place the VMM in a cgroup | baryon-virtd | writes the child into the Anchor container's `cgroup.procs` before `execve`; refuses a container cgroup that is not a systemd scope (`CgroupDriverUnsupported`), as the first step of a start, because only stopping the scope guarantees the VMM is gone before volume teardown | Implemented, Tested (placement); the refusal is Implemented with unit tests for scope detection and was not exercised on a cgroupfs node |
| Keep a running VM's logs from retention | baryon-virtd | holds a shared `flock` on the VM's log directory under `/var/log/baryon/vms/`, which `systemd-tmpfiles` skips; released at cleanup, taken again for re-adopted VMs | Implemented, Tested on the Hadron node (retention with a locked directory) |
| Supervise and stop the VMM | baryon-virtd | pidfd, `pidfd_send_signal`, Cloud Hypervisor API; recovery from a runtime record after a restart | Implemented, Tested |
| VFIO, PCI binding, IOMMU groups | nobody | | Not supported |

## Host paths and devices

| Consumer | Path | Access | Why |
|---|---|---|---|
| baryon-node-agent | `/var/lib/kubelet/device-plugins` (hostPath) | read-write | the device plugin creates its socket there and watches for kubelet restarts |
| baryon-node-agent | `/run/baryon/node` (hostPath) | read-only | NodeService socket |
| Anchor container | `/run/baryon/anchor` | read-only, injected by the device plugin `Allocate`, not by hostPath | AnchorService socket |
| Anchor container | `/dev/kvm` | read-write, injected by the device plugin | the VMM shares the container's device cgroup |
| Anchor container | `/dev/baryon/root` (block mode) | read-write device node created by the container runtime on the container's `/dev` tmpfs, from the claim's `volumeDevices` | the VM's root disk; baryon-virtd accepts a block disk only there |
| import Pod container | the claim at `/var/lib/baryon/disks/root` or `/dev/baryon/root` | read-write | writes the imported image; no /dev/kvm, no anchor socket |
| import Pod container | `/etc/baryon/import/approved` (downward API volume) | read-only | the import approval, the pod's own `virtualization.baryon.io/import-approved` annotation |
| baryon-controller | none | | |

Block-mode disks require containerd `device_ownership_from_security_context =
true` on every Baryon node: kubelet does not apply `fsGroup` to raw block
devices, and without the setting the device node is not accessible to the
VM's identity (`BlockDeviceAccessDenied`). The setting is node-wide. It
applies to every device of every pod that sets `runAsUser`, including
devices from device plugins, so in Anchor containers /dev/kvm is owned by
the VM's identity. A user who can exec into an Anchor container can open
/dev/kvm there, within the pod's limits; the device cgroup already allowed
that.

## Local sockets

| Socket | Mode | Directory | Authorization | Status |
|---|---|---|---|---|
| `/run/baryon/anchor/virtd.sock` | 0666 | `/run/baryon/anchor`, 0755 | Per connection: `SO_PEERCRED` and `SO_PEERPIDFD`; the caller must be non-root and in a pod cgroup. Per attach: the caller's uid and gid must equal the host identity of the VM named in the request (`IdentityMismatch` otherwise), and no other VM on the node may have that identity (`IdentityConflict`). `Attach` and `Stop` additionally require the VM's Anchor supervisor (ADR 0015, `NotAnchorSupervisor` otherwise); no call is accepted from baryon-virtd's PID namespace. `Console` (ADR 0014, Designed): only from a process in the container cgroup that holds the VM's VMM, with the VM's host identity, outside baryon-virtd's PID namespace and in the registered anchor's PID namespace; refused while the VM has no live registered anchor; never lifecycle authority. Per pod UID, bounded concurrent connections and `Console` streams, and identification off the accept loop with a timeout (Designed; today the socket has no per-pod limits). | Implemented, Tested (identity check); the conflict refusal and the supervisor check are Implemented with unit tests |
| `/run/baryon/node/virtd.sock` | 0600, root | `/run/baryon/node`, 0700 | File mode, and NodeService requires peer uid 0 | Implemented, Tested |
| `/run/baryon/vms/<pod-uid>/ctl/api.sock` (Cloud Hypervisor API) | root | `ctl`, 0700, root | File permissions only; the REST API has no authentication. baryon-virtd binds it and passes the listener fd to the VMM, which never needs the path. | Implemented, Tested |
| `/run/baryon/vms/<pod-uid>/vmm/serial.sock` | created by the VMM | `vmm`, a private tmpfs, 0700, owned by the VM's identity | File permissions; baryon-virtd connects only through a checked `O_PATH` descriptor (see VMM confinement) | Implemented, Tested |

The anchor socket is world-writable because anchors run as per-VM
identities. Who
can reach it is limited by the mount (only containers allocated the
`virtualization.baryon.io/vm` resource get it) and by the admission policy
on that resource. A process on the host, outside any pod cgroup, is refused.

The identity check proves that the caller belongs to the VM's runtime
context: its pod, and the VM's host identity. It does not grant lifecycle
authority. Every process in the pod cgroup with the VM's identity passes
it: processes started with `kubectl exec` in the Anchor container, and the
VMM and the DHCP responder, which baryon-virtd places in that cgroup. Up to
v0.1.0-rc1 any of them could call `Stop`, or `Attach` with the identical
spec and become the VM's anchor (finding M1 of 2026-10-27).

Lifecycle authority (ADR 0015). Status: Implemented, with unit
tests in `crates/virtd/src/peer.rs`, `crates/virtd/src/vm.rs`,
`crates/anchor/src/term.rs`, and `crates/anchor/src/run.rs`, and the CI API
server checks for admission (pass in CI); the E2E
`tests/e2e/anchor-authority.sh` (Kubernetes paths: exec,
ephemeral containers, image changes, container restart, Anchor Pod
replacement, baryon-virtd restart, node reboot, deletion) passes on the
k0s cluster. baryon-virtd's refusal of exec'd processes, the VMM,
the DHCP responder, and other pods is covered by unit tests only.

- `Attach` and `Stop` are accepted only from the Anchor supervisor: the
  init process of the Anchor container's PID namespace, in the initial
  user namespace, outside baryon-virtd's PID, mount, and network
  namespaces. baryon-virtd reads these properties through the caller's
  pidfd, so a reused PID cannot stand in for the caller.
- The first attach of a pod binds the VM to that process (PID and pidfd);
  a later call matches only while both the caller's and the registered
  pidfd show their process alive. A live registered anchor is never
  replaced; `Stop` is accepted only from it. Without a live anchor, only
  the init of the container that holds the VMM may register again,
  through `Attach` or `Stop`. An `Attach` from the init of a new container
  of the pod causes the old VMM to be killed (`AnchorContainerReplaced`)
  and is answered `UNAVAILABLE`, so the new anchor retries; a `Stop` from
  it is refused.
- AnchorService calls from baryon-virtd's PID namespace, where the VMM and
  the DHCP responder run, are refused, `Negotiate` included.
- `baryon-anchor run` refuses to run unless it is PID 1, makes itself
  non-dumpable before any other thread starts, and acts on SIGTERM only
  from uid 0 with a sender recorded by the kernel (`SI_USER`, `SI_TKILL`).
  Nodes run Yama `ptrace_scope` 1 or higher (Hadron: 1, and the
  Baryon-enabled image sets it), so only an ancestor can trace the anchor;
  baryon-virtd warns when they do not. The anchor writes its readiness
  file without following symbolic links.
- `baryon-importer import` likewise runs only as the import container's
  init process, makes itself non-dumpable, and holds its target
  exclusively (a lock on the disk directory, `O_EXCL` on a block device).
  For the importer this rule is the boundary (finding M5): it holds
  because the import image contains no other program and its image
  cannot be replaced.
- baryon-controller acts on an `IdentityConflict` termination message only
  together with the anchor's dedicated exit code, which other processes of
  the container cannot set.
- Admission: pods that request the vm or import resource may not set
  `shareProcessNamespace`, `hostPID`, or `hostUsers: false`, have exactly
  one container and no init containers, and every container drops all
  capabilities and cannot escalate privileges. Nobody may change their
  container images or add ephemeral containers to them.
- M1 is fixed per node: a node still on a v0.1.0-rc1 host image authorizes
  lifecycle calls by the identity alone until it is upgraded.

Residuals:

- The VMM and the DHCP responder run in an ancestor PID namespace with the
  VM's uid. They can send the anchor any signal, including SIGKILL and
  SIGSTOP. A killed anchor ends its container and the VM; a stopped anchor
  cannot handle SIGTERM, so deleting the pod stops the VM hard after the
  termination grace period. They can also keep a SIGTERM of their own
  pending, so that the container runtime's SIGTERM is merged with it and
  the VM stops hard the same way; the anchor logs ignored SIGTERMs once
  per doubling of their count. Processes started with `kubectl exec` could
  do the same only through a program in the image, which has none. No
  other VM is affected. Removing this needs the VMM in a PID namespace of
  its own (Planned).
- The Anchor supervisor is the init process of the Anchor container,
  whatever program it runs. That it runs `baryon-anchor run` rests on the
  image baryon-controller set and on `baryon-pod-immutable`.
- Graceful stop depends on the container runtime sending SIGTERM as uid 0
  in the initial user namespace with kill(2) or tgkill(2). Pods with
  `hostUsers: false` and rootless runtimes are Not supported: attach is
  refused, and SIGTERM would be ignored.
- Processes started with `kubectl exec` can write the anchor's readiness
  file, so the VM can show `Ready` while its guest is down, and they can
  write the termination message. This is limited to the user's own VM.
- Cluster administrators are trusted and can bypass these controls.

## Kubernetes authorization

### RBAC

| Subject | Rights | Notes |
|---|---|---|
| baryon-controller | virtualmachines: get, list, watch, patch, update; status: patch; finalizers: update; virtualmachineclasses: get, list, watch; virtualizationnodes: get, list, watch; pods: get, list, watch, create, delete, patch; persistentvolumeclaims: get, list, watch, create; persistentvolumes: get, patch; events: create, patch. All cluster-wide. | Pod and claim rights are needed to place Anchor Pods and import Pods in VM namespaces. A compromised controller can create or delete pods in any namespace, but the pods it can give the vm or import resource are only those it creates itself. PersistentVolume `get` is cluster-wide, so a compromised controller can read every PersistentVolume spec (backend volume handles, node affinity, mount options). PersistentVolume `patch` and pod `patch` are safe only with `baryon-controller-writes` installed, which limits them to adding the import record once and, on Baryon pods, to the import approval, the `vm-label-keys` annotation, and labels with keys that are not reserved; without the policy a compromised controller could rebind volumes, change reclaim policies, or relabel workloads. Claim `patch` is no longer granted. |
| `baryon-vm-edit` (ClusterRole, aggregated into `edit` and `admin`) | virtualmachines: get, list, watch, create, update, patch, delete, deletecollection; virtualmachineclasses: get, list, watch | For namespace users (`deploy/base/tenant/roles.yaml`). No `virtualmachines/status`: baryon-controller is its only writer. Nothing on PersistentVolumes. |
| `baryon-vm-view` (ClusterRole, aggregated into `view`) | virtualmachines, virtualmachineclasses: get, list, watch | Read access for namespace users. |
| baryon-node-agent | nodes: get; virtualizationnodes: get, create; status: get, patch. Cluster-wide. | Writes are narrowed per node by admission, below. `get` is not: a node agent can read every node's VirtualizationNode, which holds runtime versions and health and, per VM, Anchor Pod UIDs and the responder state, no VM names or addresses. Host inventory is no longer published (0.1.0-rc1). |

Tenants cannot read VirtualizationNodes: `baryon-vm-edit` and `baryon-vm-view`
grant nothing on them, and no Baryon role is aggregated into `view`, `edit`,
or `admin` for them. A VirtualizationNode lists Anchor Pod UIDs of every
namespace with their node; tenants see the result for their own VMs only,
as the `NetworkReady` condition.

The `NetworkReady` condition of a VM depends on the node agent of the node
its Anchor Pod runs on. A compromised node agent can write only its own
node's VirtualizationNode (`baryon-node-agent-own-node`), so it can affect
only the `NetworkReady` condition of VMs on its own node, within the
reasons and message bounds above. It cannot change `Ready` or any other
condition through this path.

A stopping node agent writes its node's conditions once more as Unknown
(`NodeAgentStopped`). The write carries the `resourceVersion` it read as a
precondition, so it does not overwrite a newer write of a replacement
agent on the same node. It cannot reach another node's object. The write
is best effort: on a reboot the pod network may already be gone.

### Admission policies

`deploy/base/policy/` holds eight ValidatingAdmissionPolicies. They need
Kubernetes 1.30 or later, and 1.32 or later for the node name in service
account identities. `baryon-vm-resource` and `baryon-node-agent-own-node`
are Implemented and were Tested earlier on the retired kind host
(Kubernetes 1.34) and the historical single Hadron node (k3s 1.36); the CI API server checks (k0s 1.36) confirm the
`baryon-vm-resource` denial for a pod that requests the vm resource, a pod
that requests the import resource, and an init container that requests the
vm resource. `baryon-controller-writes` and `baryon-import-approval` are
Implemented and Tested by the CI API server checks, which run every allow
and deny case below by impersonation, and by the import integrity E2E on
the k0s cluster, which runs with both installed and checks that a namespace
user cannot set an import approval (see [testing.md](testing.md#results)).
`baryon-pod-immutable`, `baryon-no-ephemeral-containers`, and the ADR 0015
rules of `baryon-vm-resource` are Implemented; their CI API server checks
pass in CI, and
`tests/e2e/anchor-authority.sh` checks the image change and ephemeral
container refusals on the k0s cluster.

| Policy | Rule |
|---|---|
| `baryon-vm-resource` | A pod that requests `virtualization.baryon.io/vm` or `virtualization.baryon.io/import` in any container or init container may be created only by `system:serviceaccount:baryon-system:baryon-controller`. Other users, including cluster administrators and workload controllers, are denied. The import resource grants nothing; it is restricted so that other pods cannot use up the slots that place import Pods. Such pods may not set `shareProcessNamespace`, `hostPID`, or `hostUsers: false`, have exactly one container and no init containers, and every container drops all capabilities, adds none, and is neither privileged nor allowed to escalate privileges (ADR 0015). Such pods also get no other host access: no `hostNetwork`, no `hostIPC`, no host ports, and only `persistentVolumeClaim` and `downwardAPI` volumes (so no `hostPath` and no service account token); see [Host integration policy](#host-integration-policy). CI checks: each is denied even for baryon-controller, and a pod with a claim and a downward API volume is allowed; these checks pass in CI. Designed (ADR 0014): the same restriction for the placement-only feature resource of guest configuration. |
| `baryon-pod-immutable` | Nobody may change the container images of a pod that requests `virtualization.baryon.io/vm` or `virtualization.baryon.io/import`, cluster administrators included (ADR 0015, finding M7). kubelet restarts a container whose image changed regardless of the restart policy, so without this policy a namespace user who can update pods would run an image of their choice as the Anchor supervisor, or as the importer with the approval and the claim mounted. CI checks: changing the image of a pod that requests the vm resource is denied, a label change on it is allowed, and changing the image of another pod is allowed. Status: Implemented; the CI API server checks pass in CI; Tested on the k0s cluster by `tests/e2e/anchor-authority.sh` (Anchor Pod and import Pod) |
| `baryon-no-ephemeral-containers` | Nobody may add ephemeral containers to a pod that requests `virtualization.baryon.io/vm` or `virtualization.baryon.io/import`, cluster administrators included (ADR 0015). An ephemeral container is never allocated the vm resource and gets no anchor socket; the policy keeps an image of the requester's choice from running as the VM's identity with the pod's volumes and network namespace. CI checks: adding an ephemeral container to a pod that requests the vm resource is denied; to another pod it is allowed. Status: Implemented; the CI API server checks pass in CI; Tested on the k0s cluster by `tests/e2e/anchor-authority.sh` (Anchor Pod) |
| `baryon-node-agent-own-node` | baryon-node-agent may create or update only the VirtualizationNode named after the node in its token (`userInfo.extra["authentication.kubernetes.io/node-name"]`). Requests without that information are denied. |
| `baryon-controller-writes` | Updates of PersistentVolumes and pods by baryon-controller. A PersistentVolume may only gain the annotation `virtualization.baryon.io/disk-import` when it has none, with spec, labels, finalizers, owner references, and every other annotation unchanged; the volume must be Bound, and the record must have the form `{"vmUID":"<uuid>","claimUID":"<claimRef.uid>","source":"sha256:<64 hex>"}` with the volume's own `claimRef.uid`. A pod update may change only the annotations `virtualization.baryon.io/import-approved` and `virtualization.baryon.io/vm-label-keys` and labels whose key is not reserved, with spec, finalizers, owner references, and every other annotation unchanged, and only on pods labeled `app.kubernetes.io/managed-by=baryon`. Reserved keys: `app.kubernetes.io/managed-by`, and keys whose prefix is `baryon.io`, `kubernetes.io`, `k8s.io`, or a subdomain of them other than `app.kubernetes.io` (the same rule as `reserved_label` in `crates/controller/src/anchor.rs`). CI checks: a PV spec change, another PV annotation, a record for another claim, rewriting and removing the record, and annotating an unmanaged pod are denied; adding the record for the bound claim and approving a Baryon pod are allowed. Copying `app` and `app.kubernetes.io/name` and recording their keys is allowed; adding `node.kubernetes.io/x`, changing `app.kubernetes.io/managed-by`, adding `virtualization.baryon.io/vm-name`, and relabeling an unmanaged pod are denied. |
| `baryon-system-pods` | Pods in the `baryon-system` namespace may not use the host network, PID, or IPC namespace, privileged containers, or host ports. `hostPath` volumes are allowed only in pods of the `baryon-node-agent` service account, only for `/var/lib/kubelet/device-plugins` and `/run/baryon/node`, and every mount of `/run/baryon/node` is read-only. The namespace enforces the privileged Pod Security level for the node agent's host paths, so Pod Security admission does not limit the rest; this policy does. CI checks: the shipped pod templates of baryon-controller and baryon-node-agent are allowed; each of them with the host network, PID, or IPC namespace, a privileged container, or a host port is denied; the node agent with another or an extra host path or a read-write node socket mount is denied, and so are its host paths under another service account and a host path in the controller. Status: Implemented; the CI API server checks pass |
| `baryon-vm-guest-config` | The refusing form of the ADR 0014 policy: no VirtualMachine may set `spec.guestConfig`. This release's CRD has no such field and the API server removes or rejects unknown fields before admission, so the policy admits every VirtualMachine of this release; it is installed now so that it is present when a later release's CRD accepts the field (ADR 0014, release order). CI checks: with the policy installed, a VirtualMachine create, spec update, and metadata update are allowed; with a CRD that declares the field, a create and an update that set it are denied and a VirtualMachine without it is allowed. Status: Implemented; the CI API server checks pass |
| `baryon-import-approval` | Nobody but baryon-controller may set, change, or remove the annotation `virtualization.baryon.io/import-approved` on any pod, at creation or by update. Namespace users can patch pods, so without this policy they could approve an import into a volume the controller would refuse. CI checks: another user changing, removing, or setting the approval is denied; another user changing other annotations of a pod with an approval is allowed. |

The policies are part of the supported installation. Without them:

- `baryon-vm-resource`: any pod admitted through the API server could
  request the vm resource, receive /dev/kvm and the anchor socket, and ask
  baryon-virtd to start a VM in its own pod, running as the identity of the
  VM UID it names.
- `baryon-pod-immutable`: a namespace user who can update pods could
  replace the image of an Anchor Pod or an import Pod and run their own
  program as the Anchor supervisor or as an approved importer (ADR 0015,
  finding M7).
- The container set and security context rules of `baryon-vm-resource`:
  a mutating webhook could add a container next to the anchor, or give the
  container `CAP_SYS_ADMIN`, so that a process other than the anchor could
  be the init of a PID namespace in the initial user namespace (ADR 0015).
- The `shareProcessNamespace`, `hostPID`, and `hostUsers: false` rules of
  `baryon-vm-resource`: the anchor would refuse to run or be refused, and
  the VM would never start; nothing would be taken over (ADR 0015).
- `baryon-no-ephemeral-containers`: an image of the requester's choice
  could run as the VM's identity with the pod's volumes and network
  namespace, with tools the Anchor and import images do not contain.
- `baryon-import-approval`: a namespace user could approve an import into
  a volume baryon-controller would refuse.
- `baryon-controller-writes`: baryon-controller's PersistentVolume and pod
  `patch` rights are unrestricted.
- The host access rules of `baryon-vm-resource` and `baryon-system-pods`:
  a mutating webhook or a defect could give an Anchor Pod the host network
  namespace (baryon-virtd would then hand the host's network to the VM),
  or a pod in `baryon-system` host paths or namespaces that the Baryon
  components do not use.
- `baryon-vm-guest-config`: a later release's CRD could accept
  `spec.guestConfig` before the policy that authorizes the Secret
  reference is installed (ADR 0014).

They are an API server control. They do not constrain static pods, which
the kubelet runs from a manifest directory on the node without admission
(only the API mirror pod is checked, and rejecting it does not stop the
real pod), nor the kubelet's device plugin `Allocate` path. Node-level
protection therefore relies on the immutable Hadron host and on nobody
untrusted being able to write the static pod directory; even then the
anchor identity check limits a pod to the VM whose identity it runs as. A node-level
check through the kubelet pod resources API was considered and not adopted
(ADR 0015): it would prove which pod a caller is in, which the identity
check already ties to the VM. Extended resources
cannot be requested by ephemeral containers or through pod-level resources
today, so neither is a current bypass; the policy checks containers and
init containers.

### Host integration policy

Each Baryon workload gets the minimum host access its role needs, and a
user cannot gain more by creating or changing a Baryon-managed workload:
the VirtualMachine API has no field that requests host access, only
baryon-controller can create pods that request the Baryon resources
(`baryon-vm-resource`), their images cannot be changed and no ephemeral
containers can be added (`baryon-pod-immutable`,
`baryon-no-ephemeral-containers`), and the other host access fields of a
pod cannot be changed after creation. The policies below also refuse host
access that a mutating webhook or a defect would add at creation.

| Component | Host network | Host IPC | Host PID | hostPath | Enforced by |
|---|---|---|---|---|---|
| Anchor Pod | Not required; forbidden. The VM's network is the pod's network namespace, which baryon-virtd hands to Cloud Hypervisor; in the host's namespace the VM would get the host's network | Not required; forbidden | Forbidden (ADR 0015) | Not required; forbidden. The only volume is the root disk claim | `baryon-vm-resource` |
| Import Pod | Not required; forbidden | Not required; forbidden | Forbidden (ADR 0015) | Not required; forbidden. The volumes are the root disk claim and the downward API approval file | `baryon-vm-resource` |
| baryon-controller | Not required; forbidden | Not required; forbidden | Not required; forbidden | Not required; forbidden | `baryon-system-pods` |
| baryon-node-agent, with the device plugin | Not required; forbidden | Not required; forbidden | Not required; forbidden | Required, exactly two directories: `/var/lib/kubelet/device-plugins` read-write (the plugin's socket and kubelet's registration socket) and `/run/baryon/node` read-only (baryon-virtd's node socket). Allowed only for its service account | `baryon-system-pods` |
| baryon-virtd | A host service in the Baryon-enabled Hadron image, not a pod: it runs as root in the host namespaces and enters each Anchor Pod's network namespace to hand it to the VM ([Processes and privileges](#processes-and-privileges)) | | | | The immutable host image |

All Baryon pods run with `RuntimeDefault` seccomp and no capabilities.
Pods that are not Baryon workloads, in other namespaces, are subject to
the cluster's own Pod Security configuration; Baryon grants them nothing,
and they cannot request the Baryon resources.

Status: Implemented. The CI API server checks cover every rule; the E2E
suites on the k0s cluster create Anchor and import Pods under the
policies.

### VM placement

A namespace user who can create VirtualMachines controls the scheduling
fields of the pods baryon-controller creates for them (`spec.placement`,
see [api.md](api.md#placement)). baryon-controller creates those pods in the
VM's namespace, so they pass the same admission as the user's own pods.

baryon-node-agent tolerates every taint (`deploy/base/node-agent/`), so the
`virtualization.baryon.io/vm` and `virtualization.baryon.io/import`
resources exist on every Baryon host, including tainted ones. Which tainted
hosts a VM can reach is therefore decided by the VM's tolerations, within
the limits below.

| Control | Rule | Status |
|---|---|---|
| Tolerations | CRD validation refuses a toleration without a key (it would tolerate every taint) and tolerations of `node.kubernetes.io/unschedulable` (cordon), `node-role.kubernetes.io/control-plane`, and `node.kubernetes.io/out-of-service`. A VM cannot land on a cordoned or control-plane node by tolerating its taint, and the out-of-service taint applies to every VM pod. Custom isolation taints are the administrator's: restrict which namespaces may tolerate them with PodTolerationRestriction, and pin namespaces to nodes with PodNodeSelector. Tolerating the `NoExecute` not-ready and unreachable taints only delays the eviction of the VM's own pod; fencing is unchanged (ADR 0004). | Implemented; Tested by the CI API server checks (all four refused tolerations) and by the placement E2E on the k0s cluster (a tainted node takes only the VM that tolerates its taint; a cordoned node takes no new VM) |
| `priorityClassName` | Names starting with `system-` are refused, so a VM cannot use `system-node-critical` or `system-cluster-critical` to preempt cluster components. Other PriorityClasses are limited with a ResourceQuota scoped to them. A preempted Anchor Pod stops its VM. | Implemented; Tested by the CI API server checks; an unknown PriorityClass gives `PodRejected` in the placement E2E on the k0s cluster |
| Pod affinity and anti-affinity | Terms, required and preferred, may not set `namespaces` or `namespaceSelector`, so a VM's placement cannot observe or react to pods of other namespaces; terms apply within the VM's namespace. The ResourceQuota scope `CrossNamespacePodAffinity` also applies to VM pods. | Implemented; Tested by the CI API server checks |
| VM labels | The VM's labels reach its pods, but reserved keys are never copied, so a namespace user cannot give a VM pod Baryon's own labels (`virtualization.baryon.io/vm-name`, `app.kubernetes.io/managed-by`) or labels under `kubernetes.io` or `k8s.io` through the VirtualMachine. The same rule bounds baryon-controller's pod updates in `baryon-controller-writes`. A label sync removes only keys recorded in the pod's `virtualization.baryon.io/vm-label-keys` annotation; users can edit that annotation, so baryon-controller drops reserved keys from it before acting on it. Labels set by other actors stay. Namespace users can still label pods in their namespace directly, as they could before. As with any pod, a VM whose labels match a Service selector in its namespace becomes a backend of that Service. | Implemented; Tested by the placement E2E on the k0s cluster (copy, sync, removal, another actor's label kept, reserved key not copied), the CI API server checks (controller label rights), and unit tests (reserved keys, recorded keys filtered) |

Admission and quota apply by namespace: PodNodeSelector,
PodTolerationRestriction, and ResourceQuota see the VM's pods in the VM's
namespace, not their creator. These are Kubernetes mechanisms; Baryon has
no test for them.

Baryon components must not be selected by `app.kubernetes.io/*` labels
across namespaces. Tenants can set, for example,
`app.kubernetes.io/part-of=baryon` on their VMs, and the label reaches the
VM's pods. No Baryon admission policy or cross-namespace selector depends
on such labels; the baryon-controller Deployment and the baryon-node-agent
DaemonSet select their own pods by `app.kubernetes.io/name` only within
`baryon-system`. Do not create VirtualMachines in `baryon-system` or grant
namespace users rights there; nothing in Baryon enforces this.

The `baryon-system` namespace enforces Pod Security `privileged`
(`deploy/base/namespace.yaml`), because the node agent mounts host paths.
Creating or updating pods in `baryon-system`, or exec into its pods, is
therefore node-root-equivalent: a pod there can mount any host path, and
the node agent is part of each node's trusted computing base. Grant these
rights only to cluster administrators. Status: Documented.

### Guest user data and console access (ADR 0014, Designed)

| Right | What it gives | Status |
|---|---|---|
| `create` or `update` on VirtualMachines with `spec.guestConfig.userDataSecretRef` | The guest receives the named Secret's `userdata` key, and anyone who can log in to the guest can read it. Without an admission check this would be read access to every Secret of the namespace for any subject bound to `baryon-vm-edit`, which has no rights on Secrets. The admission policy `baryon-vm-guest-config` (Designed) requires that the requester may `get` the Secret when the reference is set or changed; it is load-bearing and fails open when absent, so it ships before the field and baryon-controller checks that it exists. The check is a snapshot: like a Deployment, every later Anchor Pod gets the Secret's current content, including content written after the requester lost access. Revocation is removing the reference or the VM | Designed |
| `update` or `patch` on a Secret that a VirtualMachine references | Control of what the guest's agent runs, typically as root, at the VM's next start | Designed (ADR 0014); inherent to user data |
| `create` on `pods/exec` for an import Pod | Running a program of the import image in the import container while an import is pending, as the VM's identity, with the claim mounted read-write. The import image contains only `baryon-importer`, which refuses to run unless it is the container's init process and holds its target exclusively, so an exec'd importer cannot write a second image (finding M5). This holds only while the image cannot be replaced (`baryon-pod-immutable`, finding M7) | Implemented; the file import lock has a unit test, and the PID 1 rule is checked by `tests/e2e/anchor-authority.sh` while the import Pod runs; Tested (k0s cluster, both volume modes) |
| `create` on `pods/exec` for an Anchor Pod | The VM's console (ADR 0014, Designed), including taking over another session and the last 16 KiB of output; reading the referenced user data (the Secret volume and the seed) through whatever the Anchor image can run; opening connections and `Console` streams on the node's shared anchor socket within per-pod limits (Designed). On Kubernetes before 1.35, `get` on `pods/exec` is enough to open a session over WebSocket. Already in v0.1: opening `/dev/kvm` within the pod's limits, using up the pod's resources until the VM fails, and writing the anchor's readiness file, which can show the VM `Ready` while its guest is down. Up to v0.1.0-rc1 also stopping the VM and attaching as its anchor (finding M1, fixed: lifecycle calls require the Anchor supervisor, ADR 0015). Up to v0.1.0-rc1 also, in block mode, overwriting the running VM's root disk with `baryon-anchor import` (finding M2, fixed: the Anchor image has no import code). Grant it as the right to operate the VM, not to view it | Implemented (exec path); console Designed |
| `pods/ephemeralcontainers` on an Anchor Pod | Nothing: `baryon-no-ephemeral-containers` refuses ephemeral containers in Anchor Pods and import Pods (ADR 0015). Without the policy, everything above from an image of the requester's choice | Implemented; the CI API server checks pass in CI; Tested on the k0s cluster by `tests/e2e/anchor-authority.sh` |
| `update` or `patch` on pods, for an Anchor Pod or an import Pod | Labels and annotations, within the other policies (`baryon-import-approval` keeps the import approval to baryon-controller). Not the container image: `baryon-pod-immutable` refuses that. Without the policy, kubelet would restart the container with an image of the requester's choice as its init process, which in an Anchor Pod is the VM's Anchor supervisor and in an import Pod writes the claim under the approval while the volume records the checked source (finding M7) | Implemented; the CI API server checks pass in CI; E2E `tests/e2e/anchor-authority.sh` passed on the k0s cluster |

RBAC can narrow these rights to named pods (`resourceNames`), not by
label. baryon-controller does not read Secrets; kubelet mounts only the
Secret's `userdata` key into the Anchor Pod, never into the import Pod
(read-only, mode 0440, group the VM's gid, `optional: true`). A compromised
baryon-controller can already mount any Secret of any namespace into a pod
it creates (cluster-wide pod `create`).

## VMM confinement

baryon-virtd starts Cloud Hypervisor with the following, in this order,
between `fork` and `execve`:

1. join the Anchor container's cgroup
2. clear `FD_CLOEXEC` only on the descriptors listed in the launch
   configuration
3. `setsid`
4. `unshare(CLONE_NEWNET)`: a new network namespace with only a loopback
   device, which is down
5. drop every capability from the bounding set
6. `setgroups`, `setgid`, `setuid` to the VM's identity
7. `PR_SET_NO_NEW_PRIVS`
8. install Baryon's seccomp filter (see
   [Baryon seccomp layer](#baryon-seccomp-layer)), built before `fork` so
   that installing it allocates nothing

The DHCP responder is started through the same code path
(`crates/virtd/src/vmm.rs`, `spawn`), so it gets every step above; it does
not get the kvm group.

Cloud Hypervisor then applies its own seccomp filters (`--seccomp true`),
which stack on top of Baryon's, and Landlock rules (`--landlock`).
baryon-virtd adds one Landlock rule: read and write access, including
creating and removing files, beneath the VM's own
runtime directory `/run/baryon/vms/<pod-uid>/vmm` (owned by the VM's identity,
holding the VMM's log, event file, serial socket, and, since Cloud
Hypervisor v54.0, the socket's lock file `serial.sock.lock`). With v53.0,
a guest reboot failed without it: Cloud Hypervisor could not remove and
re-create its serial socket; with it, the guest reset in the lifecycle E2E
passed (Tested). With v54.0 the socket and its lock file are created before
Cloud Hypervisor applies Landlock, and the socket is reused across a guest
reboot (`vmm/src/lib.rs`, `vm_create`; `vmm/src/console_devices.rs`), so
the rule is needed at most to remove both files when the VMM exits; the
tmpfs is unmounted at cleanup in any case. Whether the rule can be removed
with v54.0 is Open; the lifecycle E2E with its guest reset passes with it.
The lock file gets Cloud Hypervisor's default mode (0666 minus the umask);
only the VM's identity and root reach it, through the 0700 directory.

That directory is a private tmpfs per VM (32 MiB, 256 inodes, mode 0700,
owned by the VM's identity, `nosuid,nodev,noexec`), mounted before the VMM starts
and unmounted when the VM ends, so a VMM cannot fill the host's `/run`.
Because the VMM controls its contents, baryon-virtd (root) never follows a
path into it: `crates/virtd/src/vmm_files.rs` opens entries relative to a
directory descriptor, refuses symlinks, requires the VM's uid as owner and a
single link, bounds every read, and connects to the serial socket through an
`O_PATH` descriptor so the entry cannot be swapped between check and
connect. Files baryon-virtd writes itself (the VMM's stdout and stderr, the
DHCP responder log) live in the root-only `ctl` directory.

Measured on the Hadron node, 2026-10-03, for a running VM (`<vm-uid>` and
`<vm-gid>` stand for the VM's host identity, `<kvm-gid>` for the host's
kvm group):

```
Uid:  <vm-uid> (real, effective, saved, fs)
Gid:  <vm-gid>
Groups: <kvm-gid> <vm-gid>     (kvm, pod group)
CapPrm/CapEff/CapBnd/CapAmb: 0000000000000000
NoNewPrivs: 1
network namespace: distinct from the host's, loopback only
seccomp: mode 2 (filter) on 13 of 14 threads; the main thread has none
cgroup: /kubepods.slice/kubepods-pod<uid>.slice/cri-containerd-<id>.scope
```

That measurement predates Baryon's seccomp layer. Measured on the k0s
cluster, 2026-10-04, for a running VM with Baryon's filter
(`Seccomp` and `Seccomp_filters` in `/proc/<pid>/task/<tid>/status`):

```
VMM main thread:         Seccomp 2, Seccomp_filters 1   (Baryon's filter only)
every other VMM thread:  Seccomp 2, Seccomp_filters 2 or 3
                         (device threads inherit the VMM thread's filter)
every responder thread:  Seccomp 2, Seccomp_filters 1   (Baryon's filter only)
```

With Cloud Hypervisor v54.0, measured on a node of the k0s cluster on
2026-10-27 for a running VM, the main thread also carries Cloud
Hypervisor's main-thread filter:

```
VMM main thread:         Seccomp 2, Seccomp_filters 2   (Baryon's and Cloud Hypervisor's)
every other VMM thread:  Seccomp 2, Seccomp_filters 2 or 3
```

What this gives:

- A distinct host identity per VM: one VMM cannot signal, trace, or read
  the files of another. Two VMs with the same identity never run on one
  node: baryon-virtd refuses the second (`IdentityConflict`), and
  baryon-controller places it on another node (ADR 0011).
- No capabilities and `no_new_privs`: setuid binaries and file capabilities
  cannot raise privileges.
- Empty network namespace: a compromised VMM cannot open connections to host,
  node-local, or cluster addresses. Cloud Hypervisor's seccomp profile
  allows `socket` for AF_UNIX, AF_INET, AF_INET6, and (since v54, for
  musl's `getaddrinfo` in TCP migration, upstream commit 98b1d50a0b)
  AF_NETLINK, and `connect` without
  conditions (v54.0 `vmm/src/seccomp_filters.rs`, read on 2026-10-27).
  Baryon's filter denies `socket` for every family other than AF_UNIX on
  every thread; `connect` stays unconditioned, and the empty namespace
  leaves no inet peer to reach. Its only network path is the TAP fd, which
  carries guest frames to the pod interface, where the pod's NetworkPolicy
  applies as for any pod.
- Baryon's seccomp filter on every thread of the VMM and the responder:
  `userfaultfd`, `ptrace`, `process_vm_readv`, `process_vm_writev`,
  `pidfd_getfd`, x32 calls, and non-AF_UNIX sockets fail with `EPERM`, and a
  system call of another architecture kills the process. In the responder
  it is the only filter; on the VMM's main thread it is the only filter
  until Cloud Hypervisor installs its main-thread filter (v54).
- Container cgroup: the pod's memory and CPU limits and any per-container
  cpuset apply to the VMM.

What it does not give:

- The VMM runs in the host mount namespace. File access is limited by DAC
  (its own identity) and by Cloud Hypervisor's Landlock rules, which are
  filesystem rules only. Baryon adds no Landlock policy of its own beyond
  the one writable-directory rule above.
- Baryon's seccomp filter is a short denylist, not an allowlist. Every
  other system call is left to Cloud Hypervisor's filters; in the DHCP
  responder, and on the VMM's main thread while it starts the other
  threads and before the VM is created, nothing else restricts it.
- The DHCP responder has no allowlist filter and no Landlock rules; both
  are Designed (see [Baryon seccomp layer](#baryon-seccomp-layer)).
- Path-based Unix sockets are reachable by path from any network namespace.
  The sockets above are protected by their modes, except the anchor socket,
  whose peer check applies.
- Cloud Hypervisor's seccomp and Landlock profiles are fixed by the pinned
  Cloud Hypervisor version, not reviewed by Baryon line by line. Only the
  calls in the deviation table below were checked, against v53.0 and
  again against v54.0, and they must be checked again on every Cloud
  Hypervisor upgrade.

### Baryon seccomp layer

Status: Implemented (`crates/virtd/src/seccomp.rs`, installed last in the
`pre_exec` closure of `crates/virtd/src/vmm.rs`, after
`PR_SET_NO_NEW_PRIVS`, for both Cloud Hypervisor and the DHCP responder),
with unit tests that run in CI (below). E2E: `tests/e2e/lifecycle.sh`
checks the per-thread seccomp state of the VMM and the responder; Tested
on the k0s cluster (results below).

Cloud Hypervisor installs its own seccomp filter per thread. Up to v53 its
main thread had none; since v54 the main thread installs an allowlist of
its own (`main_thread_rules` in `vmm/src/seccomp_filters.rs`) after it has
started the worker threads and before it asks the VMM thread to create and
boot the VM (`cloud-hypervisor/src/main.rs`), so no guest code runs while
the main thread lacks it. Its configuration offers no way to narrow the
filters. Seccomp filters stack and the strictest result wins, so a filter
installed before `execve` is the floor of every thread of both processes.
In the responder it is the only filter. On threads with Cloud Hypervisor's
filter (action `Trap`), a call that both filters reject ends in `SIGSYS`,
because a trap takes precedence over an error return; the "Baryon filter"
column below gives the result of Baryon's filter alone. The filter:

- kills the process on a system call of another architecture (for example
  an i386 call through `int 0x80` in an x86_64 process);
- fails x32 system calls with `EPERM`;
- fails `userfaultfd`, `ptrace`, `process_vm_readv`, `process_vm_writev`,
  and `pidfd_getfd` with `EPERM`;
- fails `socket` with `EPERM` for every address family other than
  `AF_UNIX`;
- allows everything else, which Cloud Hypervisor's filters then decide.

Each denied call, why Cloud Hypervisor allows it, and why Baryon does not
need it (checked against Cloud Hypervisor v53.0 and v54.0):

| System call | Why Cloud Hypervisor allows it | Feature that needs it | Baryon support status | Baryon filter |
|---|---|---|---|---|
| `userfaultfd` | the VMM thread filter allows it, with the `UFFDIO` ioctls (`vmm/src/uffd.rs`) | `vm.restore` with `memory_restore_mode=ondemand` (`memory_manager.rs`, `restore_by_uffd`) and the receive side of post-copy migration (`start_postcopy_serving`) | snapshots and live migration: Not supported; Baryon's Cloud Hypervisor client cannot call those endpoints | `EPERM` |
| `socket` for families other than `AF_UNIX` | the VMM thread filter allows `AF_INET`, `AF_INET6`, and since v54 `AF_NETLINK` (upstream commit 98b1d50a0b: musl's `getaddrinfo`, called by `TcpStream::connect` in TCP migration); the migration thread filter allows any family | TAP creation with an address (`ip=`, `Tap::set_ip_addr`); the TCP and TLS migration transports | not used: baryon-virtd passes TAP fds without `ip=`, and the TAP ioctls on that path use `AF_UNIX` sockets; live migration: Not supported. The responder creates no sockets; its packet socket is inherited | `EPERM` unless the family is `AF_UNIX` |
| `ptrace`, `process_vm_readv`, `process_vm_writev`, `pidfd_getfd` | no Cloud Hypervisor thread filter allows them, the v54 main-thread filter included; the main thread runs without a Cloud Hypervisor filter until it installs its own | none | not needed | `EPERM` |
| System calls of another ABI | the main thread runs without a Cloud Hypervisor filter until it installs its own (none at all before v54) | none | not needed | another architecture (i386): the process is killed (`SIGSYS`); x32: `EPERM` |

Why each denial matters:

- `userfaultfd`: Cloud Hypervisor's own call, without
  `UFFD_USER_MODE_ONLY`, already fails on Hadron, because the image sets
  `vm.unprivileged_userfaultfd` to 0 and `/dev/userfaultfd` is root-only
  (mode 0600). The user-mode-only variant is available to any process
  whatever the sysctl says, and the filter removes it. That variant cannot
  handle faults in kernel mode, so the gain is a smaller reachable kernel
  surface, not the removal of a known exploitation primitive.
- `ptrace`, `process_vm_readv`, `process_vm_writev`, `pidfd_getfd`: the VMM
  and the responder of a VM run as the same uid. The filter keeps them from
  reaching into each other through threads that have no Cloud Hypervisor
  filter. Hadron's Yama `ptrace_scope` is 1 (checked on a node), which
  already blocks a ptrace attach between the two, since neither descends
  from the other; the filter is defense in depth.
- `socket`: see VMM confinement above. The filter fails `socket(2)` for
  every family but `AF_UNIX`. `io_uring` (allowed, see Kept) can still
  create sockets with `IORING_OP_SOCKET`, which seccomp does not see, so
  the inet socket surface is reduced, not removed; the empty network
  namespace remains the control that leaves no inet peer.
- Foreign ABIs: a filter that checks system call numbers is only meaningful
  for the ABI it was written for.

Kept: `io_uring`. Cloud Hypervisor's block backend may use it, and
`kernel.io_uring_disabled` is 0 on Hadron. Operations submitted through
`io_uring` are not checked by seccomp, including socket creation
(`IORING_OP_SOCKET`). Narrowing it needs a measurement of Cloud
Hypervisor's use first (Planned, below); the DHCP responder never uses it
and its Designed allowlist filter would deny it.

Every update of the pinned Cloud Hypervisor version must check the filter
again: for each denied call, confirm that the new version still uses it
only for a feature Baryon does not support, and run
`tests/e2e/lifecycle.sh` with `NODE_SHELL` set (see [hadron.md](hadron.md#updating-the-pinned-cloud-hypervisor-version)).

Unit tests (`crates/virtd/src/seccomp.rs`; each runs in a forked child with
`no_new_privs` and the filter installed):

- a user-mode-only `userfaultfd` fails with `EPERM`; an unfiltered control
  child must succeed, so the test cannot pass on a host that refuses the
  call anyway (skipped on a kernel without `userfaultfd`);
- `ptrace`, `process_vm_readv`, `process_vm_writev`, and `pidfd_getfd` fail
  with `EPERM` while `getpid` works;
- an `AF_UNIX` socket can be created; `AF_INET`, `AF_INET6`, `AF_NETLINK`,
  and `AF_PACKET` sockets fail with `EPERM`;
- an x32 `getpid` fails with `EPERM`;
- an i386 `getpid` through `int 0x80` kills the child with `SIGSYS`
  (skipped when IA32 emulation is disabled);
- every jump in the program stays inside it, and the program ends in a
  return.

E2E checks (`tests/e2e/lifecycle.sh`, with `NODE_SHELL` set): every
responder thread has seccomp mode 2 with exactly one filter, and every VMM
thread, the main one included, mode 2 with at least two (Cloud Hypervisor
v54.0; with v53.0 the check required exactly one filter on the main
thread). Tested on the k0s cluster: every responder thread had mode 2 with
one filter; with v53.0 the VMM's main thread had one filter and every other
VMM thread two or three ([testing.md](testing.md#k0s-cluster)); with
v54.0 the main thread had two and every other VMM thread two or three
([testing.md](testing.md#cloud-hypervisor-v540)).

Remaining hardening:

| Item | Status |
|---|---|
| A strict allowlist seccomp filter for the DHCP responder | Designed |
| Landlock rules for the DHCP responder | Designed |
| Narrowing `io_uring`, after measuring Cloud Hypervisor's use | Planned |

## Guest attack surface

- Device emulation: virtio-blk, virtio-net, virtio-rng, serial, ACPI. A guest
  that exploits Cloud Hypervisor gains the VMM's confined identity above.
- Host kernel network path: the guest NIC runs with checksum and
  segmentation offloads on (ADR 0007). With checksum offload off, Cloud
  Hypervisor v54.0 drops guest frames that ask for a partial checksum or
  segmentation; with it on, it passes every guest-chosen virtio-net header
  value (GSO type and size, header length, checksum start and offset) to
  the TAP and checks only the header length (`net_util/src/queue_pair.rs`,
  `tx_is_header_valid`); v53.0 had no such check. With the offloads on,
  the host kernel's validation of the header in the TAP write path is the
  control. This is the exposure of every release so far: Cloud Hypervisor
  had the offloads on by default up to v53.0, and Baryon sets them on
  explicitly with v54.0, which defaults to off. Accepted, because with them
  off guest TCP throughput on the k0s cluster fell to a fraction
  ([testing.md](testing.md#cloud-hypervisor-v540)), and upstream
  measurements in cloud-hypervisor#9041 show more VMM CPU per byte (ADR
  0007); where the kernel's segmentation work is accounted (pod cgroup or
  softirq) was not measured. UDP fragmentation offload (`offload_ufo`)
  is a legacy path that current Linux guests do not use; whether to keep
  it is Open. Re-evaluate on advisories in the TAP or virtio-net header
  code.
- Nested virtualization: Baryon starts guests with `nested=off`, so a guest
  cannot use the CPU's virtualization extensions and cannot reach the
  host's nested KVM code. v0.1.0-rc1 passed no `nested` option and Cloud
  Hypervisor v53.0 defaults it to on; on hosts whose KVM module allows
  nesting (`nested=Y` on the k0s cluster nodes), its guests could use the
  extensions (inferred from the default and the host parameter; not
  checked from inside a v0.1.0-rc1 guest). Fixed on `main`: a guest on the
  k0s cluster with Cloud Hypervisor v54.0 had no `vmx` or `svm` flag in
  `/proc/cpuinfo` (Tested by a manual check on 2026-10-27,
  [testing.md](testing.md#cloud-hypervisor-v540)). v0.1.0-rc2 carries the
  fix ([release notes](release-notes/v0.1.0-rc2.md)).
- DHCP: guest-sent DHCP frames are parsed by the DHCP responder process,
  not by baryon-virtd. The responder runs with the VMM's confinement (the
  VM's identity, no capabilities, `no_new_privs`, empty network namespace, Anchor
  container cgroup, Baryon's seccomp filter as its only filter) and holds
  only the packet socket, its stdin and stdout pipes, and its log file. A
  guest that exploits the parser gains that identity, which it could reach
  through the VMM anyway. baryon-virtd reads the responder's events as
  untrusted input: lines are limited to 4 KiB, and an overlong or
  unparseable line, an acknowledgement for another address or MAC, or an
  answer to a probe that was not sent (or a second answer) is a protocol
  violation, after which baryon-virtd stops reading, kills the responder,
  and restarts it with backoff. A compromised responder can therefore add
  at most one warning per restart to baryon-virtd's log, whatever it
  writes. The responder exits when its stdin closes, so it never outlives
  the baryon-virtd that started it. A responder that exits, that answers
  no liveness probe for 25 s, or that breaks its protocol while the VM runs
  is restarted for as long as the VM runs, with backoff from 1 s doubling
  to at most 60 s (ADR 0007, revisions of 2026-10-04). A killed responder
  is waited for at most 60 s. Probe answers are a health signal, not a
  security control: a compromised responder can answer probes without
  serving the guest. Accepted trade-off: a
  guest that can crash the responder's parser gets a fresh responder about
  once a minute for as long as the VM runs, instead of 11 in total. Each
  restart is a new exec with the same confinement, the responder holds no
  state, and without it the guest loses its address when its lease
  (86400 s) expires. The responder reports at most one acknowledgement per
  60 s and logs repeats at debug level only, so guest requests add at most
  one line per 60 s to its log at the default level.
- Serial console: baryon-virtd reads it to detect a login prompt and appends
  it to a size-capped log (8 MiB per file; one previous file is kept on
  rotation). It never writes guest input anywhere that is executed.
- SMBIOS strings (system UUID, serial number with the NoCloud seed, product
  name) are written by baryon-virtd. They are validated so they cannot inject
  Cloud Hypervisor option syntax. The guest only reads them.
- Network: the guest is bridged to the pod interface at layer 2 and can send
  any frame a pod could, including spoofed MAC or IP source addresses.
  NetworkPolicy and any CNI anti-spoofing apply as they do for pods.
- No VFIO or PCI passthrough exists, so the guest has no DMA path to host
  memory.

## Data handling

- Credentials: baryon-controller and baryon-node-agent use their service
  account tokens. baryon-virtd handles no tokens, kubeconfigs, registry
  credentials, or keys. The Anchor Pod has no token.
- Disk import: the URL and optional SHA-256 come from the VirtualMachine spec
  and are used by the unprivileged import Pod, in its pod network
  namespace. A plain `http://` source requires `sha256` (API validation, and
  the importer refuses it too). A redirect from HTTPS to plain HTTP is
  refused. Without a digest, an HTTPS source is trusted as served. What the
  importer can reach is what the import Pod can reach; constrain it with a
  NetworkPolicy that selects `virtualization.baryon.io/import-for` or
  `app.kubernetes.io/managed-by=baryon`. The import Pod carries the VM's
  own labels, so a policy that selects the VM by those labels applies to it
  too (an egress-deny policy for VMs also blocks their imports); a policy
  that selects VMs by `vm-name` does not. Imported images are mode 0600,
  owned by the VM's identity.
- Source URL validation: since 0.1.0-rc1 the API server accepts only an
  `http` or `https` URL with a non-empty host and without userinfo
  (`user:password@`). The importer also refuses userinfo, in the source
  URL and in redirect targets, which covers VirtualMachines stored before
  0.1.0-rc1 (`rootDisk` is immutable and the API server ratchets unchanged
  values, so they are not revalidated). Besides that, the importer refuses
  only a redirect from HTTPS to HTTP. Query strings are not checked.
- Source URL exposure: annotations (the import Pod's `import-source`, the
  PersistentVolume record) and Events carry only a digest of the URL
  (`sha256:<hex>`). The URL itself remains in the VirtualMachine spec, in
  the import Pod's arguments, and in the filesystem-mode import marker on
  the volume. The importer's log lines and failure messages (which reach
  the `ImportFailed` condition that namespace `view` users can read) show
  URLs and redirect targets as scheme, host, port, and path only, without
  userinfo or query; since the 2026-10-05 review this covers every
  importer message. URLs must not carry secrets.
- API server messages in VM conditions: when the API server refuses a
  claim (`ClaimRejected`) or pod (`PodRejected`) that baryon-controller
  creates for a VM, the message, cut to 1024 bytes, is copied to the VM's
  conditions in the same namespace. The refusal was made for the
  controller's identity, so the text can name the controller's service
  account, the namespace's ResourceQuota and usage, or the message of an
  admission policy or webhook evaluated against the controller. Policies
  that treat baryon-controller differently should not put information in
  their messages that the namespace's users must not see.
- Host inventory: since 0.1.0-rc1 VirtualizationNodes carry no host
  inventory. baryon-node-agent logs one `host inventory` line (counts and
  versions: no PCI addresses or IDs, no IOMMU group membership, no NUMA
  CPU lists), readable by whoever can read pod logs in `baryon-system`.
  Inventory fields written by earlier releases are pruned when read and
  removed at the node agent's next write; etcd backups taken earlier keep
  them.
- Block import contract: after a successful import every byte of the
  device is the image's byte at that offset or zero. The importer opens the
  device with `O_EXCL` and `O_NOFOLLOW`, refuses anything but a block
  device, and refuses a device whose logical block size (`BLKSSZGET`) does
  not divide 4096. Before the download it zeroes the first and the last
  1 MiB of the device and flushes them; image data in those two regions is
  held in memory and written only after the size and SHA-256 checks pass,
  so a failed import leaves no partition table, boot loader, or backup GPT
  at either end. Every zero run of the image and everything after the
  image are zeroed explicitly: with `FALLOC_FL_PUNCH_HOLE` (block layer
  write-zeroes that may deallocate and must read back zero), else
  `FALLOC_FL_ZERO_RANGE` (the kernel falls back to writing zeros), else
  written zeros; only `EOPNOTSUPP` moves to the next method. Unaligned
  edges are written as zeros. `BLKDISCARD` is not used, because discard
  gives no guarantee that the range reads back zero. The download must have
  the length of `Content-Length` when the server sends one; without a
  digest, a response without `Content-Length` is refused, for file and
  block imports, so a truncated download is not taken for a complete
  image. See [storage.md](storage.md#block-import).
- Thin storage: every block import zeroes or writes the whole device. dm-thin
  has no write-zeroes support, so on a thin pool an import allocates the
  whole volume. A thin pool must have room for the full size of every
  volume imported into it and must zero newly provisioned blocks
  (Documented residual).
- Stale data from the backend: a new volume may hold an earlier user's data.
  The block import contract leaves none of it on an imported block device
  (checked by the import integrity E2E with a full-device scan); a
  filesystem-mode import writes a new sparse `disk.img`. A claim adopted
  with `rootDisk.source.persistentVolumeClaim` is used as is: Baryon
  neither imports into it nor clears it.
- Import record: the completed import is recorded in the annotation
  `virtualization.baryon.io/disk-import` on the bound PersistentVolume
  (ADR 0006), which namespace users cannot write. Completion is judged from
  the import Pod's spec and its importer's exit code, the import approval
  is set only by baryon-controller (`baryon-import-approval`), and without
  a record an import runs only into a volume dynamically provisioned for
  the claim. Claim metadata and pod annotations, which namespace users can
  change, cannot cause an import: the claim annotation of earlier releases
  counts only together with a StorageReady=True that baryon-controller
  wrote, and then only to record an import, never to start one. The tenant
  roles do not grant `virtualmachines/status`. A disk with an image source
  is reported ready (StorageReady=True) only once its record exists; an
  importer's exit code alone never makes it ready. For VirtualMachines
  created by earlier releases, StorageReady=True written by the earlier
  controller is trusted once, to migrate the record to the volume.
  `status.rootDiskImport` is a second, one-way marker that keeps a used
  block claim from being imported again when its record is lost
  (`ImportStateUnknown`). Accepted residual, with cluster administrators
  trusted: if both the status and the record are lost (for example a
  restore without status and an administrator removing the record), a
  fresh-looking volume can be imported again.
- Restores: snapshot restores and clones carry a data source and are
  refused for a VirtualMachine with an `http` source
  (`DiskNotProvisionedForClaim`). Restore a VM disk by referencing the
  restored claim with `rootDisk.source.persistentVolumeClaim`. A
  VirtualMachine with an `http` source on a restored volume that has no
  record, and that otherwise looks fresh, can import over it.
- Runtime records (`/run/baryon/vms/<pod-uid>/record.json`, tmpfs, mode 0600):
  the VM spec, VMM pid and start time, uid, cgroup paths, guest MAC, and the
  time of the last DHCP acknowledgement. No secrets. A record is trusted after a restart only if the process start time
  and executable still match.
- Logs (`/var/log/baryon/vms/<namespace>_<name>_<pod-uid>/`, persistent on
  Kairos): console output (mode 0600, directories 0700), the Cloud
  Hypervisor log and output, and the DHCP responder log (all 0600, at most
  16 MiB each when copied). While the VM runs, the responder log
  `ctl/dhcp.log` on the host's `/run` is not size-capped; repeated
  acknowledgements are logged only at debug level, and each responder
  start adds a line. Console output can contain anything the guest
  prints, including credentials. On Hadron the parent directory is 0750
  root, and the image's tmpfiles.d entry removes per-VM log entries older
  than 14 days, except under the directories of running VMs, which
  baryon-virtd keeps under a shared BSD lock that systemd-tmpfiles honors
  (Tested on the Hadron node with a short override age). Deleting a
  VirtualMachine does not remove its logs sooner. With interactive console
  sessions (ADR 0014, Designed), the console log also holds what the guest
  echoes and prints during a session (commands typed at a shell and their
  output, and anything cloud-init prints from user data), for the same 14
  days. A new console session first receives the last 16 KiB of console
  output, so it can see what an earlier session displayed: sessions are
  not a confidentiality boundary from each other.
- Guest user data (ADR 0014, Designed): kubelet mounts only the
  referenced Secret's `userdata` key into the Anchor Pod (a tmpfs volume,
  mode 0440, group the VM's gid). baryon-anchor writes the seed image to
  `/dev/shm/baryon-anchor/` in the pod. `/dev/shm` belongs to the pod
  sandbox, so it outlives an Anchor container restart; the new anchor
  replaces the seed left by the previous container. Both are removed with
  the pod; `hostIPC` is refused so the seed never lands in the node's
  `/dev/shm`. baryon-controller and baryon-virtd never read or store the
  content; baryon-virtd opens the seed without following symbolic links
  and accepts only a regular file of the fixed size, owned by the VM's
  uid, on the container's `/dev/shm`. Termination messages and VM status
  carry the reason and the limit only, because Pod status is readable
  without rights on Secrets. Processes with the VM's identity can read
  and replace the seed; that gives them nothing the guest does not have.
  The seed disk stays attached while the VM runs, so any guest process
  that can read it can read the user data; user data is never protected
  from the host.

## Cleanup and stale state

- TAP: non-persistent; destroyed when the VMM's fd closes.
- tc filters on the pod interface: removed at VM stop while the anchor is
  alive; otherwise they disappear with the pod network namespace. The MAC
  change on the pod interface is not reverted; the pod is torn down anyway.
- VMM network namespace: freed by the kernel when the VMM exits.
- Block devices: baryon-virtd's node-local registry entry for a VM's block
  device numbers is dropped when the VM ends; it is kept in the runtime
  record, so a restarted baryon-virtd keeps refusing the devices of
  re-adopted VMs to other VMs. A deleted imported claim's volume is deleted
  by the CSI driver according to its reclaim policy; Baryon does not wipe
  it.
- VMM tmpfs: unmounted (lazily) with the runtime directory when the VM
  ends, and when a stale runtime directory is found at startup.
- Seed image (ADR 0014, Designed): survives an Anchor container restart
  in the pod's `/dev/shm`; replaced by the next anchor before its first
  `Attach`; removed with the pod. No node-side cleanup.
- Pod teardown while baryon-virtd is down: the container runtime stops the
  VMM with the Anchor container, before volumes are unmounted (measured; see
  [runtime-boundary.md](runtime-boundary.md)).
- baryon-virtd restart: VMMs keep running and are re-adopted from their
  records. If a record cannot be written, that VMM is orphaned until it exits
  or its pod is torn down.
- Node reboot: `/run` is cleared and all VMMs are gone. With kubelet graceful
  node shutdown configured, guests get an ACPI shutdown first (Tested on
  the Hadron node); without it, they are stopped hard. See
  [kairos-integration.md](kairos-integration.md).

## Live migration (Not supported)

Live migration is Not supported
([ADR 0013](adr/0013-live-migration.md)). Baryon has no migration
resource, no migration RPC in the runtime protocol, and no migration code
in baryon-virtd. The Cloud Hypervisor client
(`crates/cloud-hypervisor/src/api.rs`) can call only `vmm.ping`,
`vm.info`, `vm.power-button`, and `vmm.shutdown`; its generic request
method is private, so no other endpoint is reachable through it. The
pinned Cloud Hypervisor binary contains `vm.send-migration` and
`vm.receive-migration`, but its API socket is reachable only by host root,
and a VMM has an empty network namespace, so it cannot reach a TCP
migration peer. Nothing in this section is Implemented or Tested.

Threat model for any future design. These are Designed constraints
recorded in ADR 0013, not decisions:

- The migration stream is guest-controlled input. It comes from the source
  VMM, which its guest may control. A Cloud Hypervisor v53 destination
  adopts the received VM configuration (including its Landlock settings,
  disk paths, and console paths) and deserializes full device state,
  checking only the protocol version and CPUID. The destination's
  confinement (seccomp, Landlock, identity, network namespace), disks,
  network file descriptors, and console must come from the destination's
  baryon-virtd, never from the stream.
- The receive endpoint is unauthenticated and unbounded. Plain `tcp:` has
  no authentication; with TLS any client certificate from the CA is
  accepted, with no binding to a source; after the `Config` step up to 128
  additional connections from any peer write memory payloads into guest
  memory; payload sizes come from the peer; there are no I/O timeouts, and
  `timeout_s` is a pre-copy duration limit only. Cloud Hypervisor's
  listener must never be reachable beyond the VM's own identity;
  baryon-virtd must authenticate the peer node, bind the session to one
  authorized migration, enforce deadlines whose expiry kills the
  destination VMM, and bound payload sizes.
- A relay in baryon-virtd would make the root runtime hold the guest's
  memory in plaintext as it passes, hold node credentials, and listen on
  TCP. Today no Baryon component listens on TCP and baryon-virtd holds no
  credentials. A migration must be authorized by a Kubernetes object that
  only baryon-controller can create or approve, enforced by admission, and
  the destination must come from authenticated node identity, never from
  an anchor, a user, or a VM field.
- Two running copies of one VM corrupt its disk. Cloud Hypervisor releases
  its disk locks before the switchover and does not take them back after a
  failure; its locks are host-local. A lost final acknowledgement resumes
  the source while the destination runs (v53: pre-copy and post-copy;
  main: pre-copy). `ClaimInUse` and `DiskInUse` do not cover two copies of
  one VM on different nodes. Fencing must be one authority held in
  Kubernetes and enforced on the data path.
- Guest memory is plaintext to the host during a migration, as it is at
  any time. Confidential computing would need a different design (Not
  supported).

The 2026-10-04 experiment recorded in ADR 0013 used a root VMM in the host
network namespace, plain TCP without authentication, no Landlock, and a
shared device outside Baryon's claim and device exclusion. It validates no
security property.

## Release and supply chain

How a release is built and published ([release.md](release.md)), and what
protects it. Reviewed in the v0.1.0-rc1 release gate review of 2026-10-05;
the release workflow was split and gated for v0.1.0-rc3.

| Item | State |
|---|---|
| GitHub Actions | Every action in `.github/workflows/` is pinned by commit SHA, with the version in a comment |
| Workflow token permissions | No permission by default in the release workflow, `contents: read` in CI. The release workflow runs four jobs with their own permissions: gate (`contents`, `actions`, `statuses` read), build (`contents: read`), attest (`contents: read`, `id-token: write`, `attestations: write`), publish (`contents: write`, `packages: write`). Attest and publish run only for a tag push, the publish job checks out no repository code, and checkouts do not persist credentials. Implemented |
| Untrusted code | CI runs pull requests with `contents: read` and no secrets; runs from fork pull requests need a maintainer's approval (repository setting, all external contributors). The release workflow has no pull request trigger. Implemented |
| Release gate | `hack/release-gate.sh`, the first job of the release workflow, fails closed unless the tag matches the workspace version, is annotated, signed, and verified by GitHub, and points to the commit; the commit is reachable from `main` on GitHub; the latest CI run on that commit's push to `main` succeeded; and a maintainer recorded release qualification on that commit (status `baryon/release-qualification`). Implemented |
| Tag and branch protection | Repository rulesets: `refs/tags/v*` cannot be deleted, moved, or force-updated by anyone; `main` cannot be deleted or force-pushed. Immutable releases are enabled. Implemented (configured 2026-10-30) |
| Release identity | The build job checks that every binary, image label, the host image's `components.json`, and `baryon.yaml` name the tag's version and commit, and that the published images and binaries are the checked ones; the publish job checks that each registry digest equals the built digest. Implemented |
| Dependency gate | CI runs `cargo deny check advisories licenses sources bans` on every change. Implemented |
| DCO | CI checks the `Signed-off-by` trailer of every commit in a push or pull request (`hack/check-dco.sh`). Implemented |
| Host component downloads | Cloud Hypervisor, its firmware, and virtiofsd are downloaded by URL and checked against the SHA-256 in `packaging/versions.env`; CI checks that the pinned artifacts are fetchable and match (`hack/fetch-artifacts.sh`). Implemented |
| Base and builder images | Hadron, kairos-init, the Rust builder image, and Alpine are pinned by digest in `packaging/versions.env` (tag kept for people, digest decides); `components.json` in the host image records the Hadron and kairos-init references with their digests. Implemented |
| Build tools | cargo-auditable is installed by version (`CARGO_AUDITABLE_VERSION`) from crates.io with its own lock file (`cargo install --locked`); syft is the version pinned by the pinned `anchore/sbom-action/download-syft`, which checks its SHA-256. skopeo is the runner's. Implemented |
| Kubernetes distribution binary | k0s is installed into the host image by kairos-init (pinned by digest) at the version in `packaging/versions.env`. Its download verification is kairos-init's and is not verified by Baryon; `components.json` records the version only. Upstream dependency |
| Build context | `.dockerignore` excludes build outputs and `.git` only; the Dockerfiles copy explicit paths. An allowlist `.dockerignore`: Planned |
| Component images | `FROM scratch` with one static binary; no base image, no package manager. Implemented |
| Binary dependency data | Every Baryon binary embeds its Rust crate list (cargo-auditable, `.dep-v0`); CI checks every binary, and the release build fails if an image SBOM lists no crates. Implemented |
| Checksums and SBOMs | Every release carries `SHA256SUMS` of its files, SPDX SBOMs of the source tree, of the baryon-virtd binary, and of each image. Image SBOMs are generated from the OCI layout that is published and name the image by repository and manifest digest; the build fails if an SBOM does not record its image's digest. Implemented |
| Provenance | GitHub artifact attestations (SLSA build provenance) for every release file and every image digest, made by the attest job before anything is published, and verifiable with `gh attestation verify` or offline from the public attestation bundle ([release.md](release.md#verifying-a-release)). Implemented |
| Signing | Release tags are signed with the maintainer's SSH signing key registered on GitHub, and the gate requires GitHub to verify the signature. No file or image is signed with a long-lived project key; the attestations are Sigstore signatures of the workflow identity |
| Earlier releases | v0.1.0-rc1 and v0.1.0-rc2 were published from the private repository with one combined job, without attestations, signed tags, digest-bound SBOMs, or binary dependency data. They stay as they are; a published tag or release is never changed |

## Review findings

Findings of the 2026-10-03 review and their state:

| Finding | Severity | State |
|---|---|---|
| Any pod could request the vm resource and drive Attach; no binding to the VM's identity | High | Fixed: `baryon-vm-resource` admission policy; baryon-virtd requires the VM's identity |
| A VMM compromise landed in the host network namespace | High | Fixed: empty network namespace per VMM |
| NodeService unauthenticated, socket 0666 | Medium | Fixed: 0600 in a 0700 directory, peer uid 0 required |
| Cloud Hypervisor API socket in a traversable directory | Medium | Fixed: moved to a root-only `ctl` directory |
| DHCP parsed in the root process | Medium | Fixed: separate responder process with the VMM's confinement |
| Node agent can write any node's VirtualizationNode | Medium | Fixed: `baryon-node-agent-own-node` admission policy. The read-write device plugin mount is required. |
| Import without integrity, HTTPS to HTTP redirect | Low | Fixed: digest required for plain HTTP, downgrade refused |
| Broad controller pod and claim rights | Low | Accepted and documented (RBAC above) |
| Pod interface MAC not restored | Low | Accepted and documented |
| Bounding set left full in the VMM | Note | Fixed: emptied before `setuid` |
| Runtime record and console log world-readable | Note | Fixed: 0600 and 0700 |

Findings of the 2026-10-03 re-review and the architecture gate:

| Finding | Severity | State |
|---|---|---|
| baryon-virtd, as root, followed paths inside the VMM-owned directory (log tail into status, log copies, serial connect): a compromised VMM could point it at host files or at an endless device | High | Fixed: `vmm_files.rs` (no symlinks, owner and link checks, bounded reads, `O_PATH` connect); root-written files moved to `ctl` |
| VMM-writable files on the host's `/run` had no size limit | Medium | Fixed: private 32 MiB tmpfs per VM |
| Admission policies do not gate static pods or the device plugin path | Medium | Documented (Admission policies); node-level check Designed |
| Pod-liveness check excluded the responder by raw pid; responder failure count reset on every attach | Low | Fixed: exclusion only while the responder's pidfd is alive; the count resets only after a stable run. Since 2026-10-04 the count decides the backoff and the `Failing` state, not a restart limit (risks of 2026-10-04 below) |
| Under the cgroupfs driver nothing outside baryon-virtd stops the VMM before volume teardown | Architecture | Fixed: baryon-virtd refuses non-systemd container cgroups; cgroupfs Not supported |
| Ephemeral containers and pod-level resources are not matched by the vm-resource policy | Note | Not a bypass today (they cannot request extended resources); watched |
| The node-agent policy indexes the node name list without a length check | Note | Fails closed (a CEL error denies) |

Found while fixing these: baryon-virtd did not exit on SIGTERM while anchors
were attached (graceful shutdown waited for their streams), so a restart
with VMs running waited for systemd's 90 s stop timeout. It now exits 2 s
after the signal; VMs keep running and anchors reconnect. A guest reboot
also failed with Landlock enforced (see VMM confinement); that is fixed by
the directory rule.

The review also checked every `unsafe` block (peer credentials, the
`pre_exec` privilege drop, TAP and packet socket calls, the KVM probe) and
found them sound; their SAFETY comments were extended where it asked.

Findings of the 2026-10-03 review of block-mode storage, the image import,
and the disk handoff:

| Finding | Severity | State |
|---|---|---|
| A block device was imported again over the guest's data when the import record was missing | High | Fixed: the record is on the PersistentVolume, an import needs an approval that only baryon-controller sets, and completion is judged from the import Pod's spec (import integrity review below) |
| The import record could name a URL the pod did not import | Medium | Fixed: only an import Pod whose importer arguments carry the current URL counts; since the import integrity review, the claim UID and the import resource too, and the legacy init container path is removed |
| The VMM's OFD lock does not exclude a second VMM on the same block device: each container has its own device node | Medium | Fixed for VMMs: `DiskInUse` registry in baryon-virtd, kept across restarts; the controller refuses a second VM on a claim in use (`ClaimInUse`). The importer is excluded only by the controller's ordering |
| baryon-virtd accepted a block device at any container path and for filesystem-mode disks | Medium | Fixed (`UnknownPath`, `KindMismatch`, `DeviceOutsideDev`); the device cgroup remains a second layer. Tested (hostile-disk E2E) |
| Zeroing with `FALLOC_FL_ZERO_RANGE` disabled deallocation and could fill thin pools | Medium | Partially fixed: `FALLOC_FL_PUNCH_HOLE` first; on devices without write-zeroes support the kernel writes zeros and allocates the device (thin pools: Documented, import integrity review below) |
| The import Pod requested the vm resource and received /dev/kvm and the anchor socket | Medium | Fixed: the placement-only `virtualization.baryon.io/import` resource grants nothing, and admission restricts it to baryon-controller; the deletion E2E checks that the importer sees neither /dev/kvm nor the anchor socket |
| Unaligned image length made tail zeroing fall back to user-space writes | Low | Fixed |
| Size limit checked after writing | Low | Fixed |
| A failed import left bootable content | Low | Fixed: the first and the last 1 MiB are zeroed before the download and receive image data only after verification |
| Importer did not check that the target is a block device | Low | Fixed |
| A corrected URL did not replace a failing import | Low | Fixed: `rootDisk` is immutable; recreate the VirtualMachine |
| The import record (claim annotation, import Pod) is trusted as written by anyone who can edit claims or create pods in the namespace | Low | Superseded by the PersistentVolume record (import integrity review below) |
| Controller claim `patch` is cluster-wide | Low | Fixed: claim `patch` removed |
| containerd `device_ownership_from_security_context` is a node-wide requirement and makes /dev/kvm openable in Anchor containers | Low | Documented (Host paths and devices) |
| A stale import Pod was deleted and the Anchor Pod created in the same reconcile | Low | Fixed: the Anchor Pod is created only after the import Pod is gone |
| The source URL was copied to annotations and events | Low | Partially fixed: annotations hold a digest and the event has no URL; the URL remains in the import Pod arguments, the VirtualMachine, and the filesystem import marker; URLs must not carry secrets |
| NetworkPolicy for VM pods does not select the import Pod | Low | Documented (Data handling, [api.md](api.md#labels-on-objects-baryon-creates)); since the placement change, a policy that selects the VM's own labels selects the import Pod too |
| `check_device_access` is a diagnostic, not a control | Note | Documented (Who does what) |
| The `/dev` mount check read the statx mount id without checking the returned mask, and does not fit user namespaces | Note | Partially fixed: the mask is checked and the check fails closed; Kubernetes user namespaces with bind-mounted device nodes would be refused (Planned item below) |
| A reused device minor after a forced detach could point a reopen at another volume | Note | Open |

Findings of the 2026-10-03 review of import integrity (the import record,
completion evidence, and the import approval), including one finding of the
architecture review:

| Finding | Severity | State |
|---|---|---|
| Import completion was judged from pod metadata that namespace users can write, and the record was written without checking that claim and volume are bound to each other | High | Fixed: completion only from the import Pod's spec (import resource, mounted claim, `--url=` and `--claim-uid=` arguments) and the importer's exit code; the record is written on the PersistentVolume only when claim and volume are mutually bound |
| `ImportStateUnknown` cleared itself on the next reconcile | High | Fixed: `status.rootDiskImport` is kept while the claim UID is unchanged, also on refusal |
| A namespace user's pod named like the import Pod made StorageReady True | Medium | Fixed: such a pod is never used and blocks the VM (`ImportPodConflict`); Tested by the import integrity E2E |
| StorageReady was derived from the init container of a pod with the Anchor Pod's name, and from a stale import Pod | Medium | Fixed: only the import Pod for the current claim UID reports import progress; StorageReady=True needs the record; a pod with the Anchor Pod's name that does not request the vm resource is `AnchorPodConflict` |
| A namespace user could set the import approval on the import Pod (found by the architecture review) | High | Fixed: `baryon-import-approval`; Tested by the CI API server checks and the import integrity E2E |
| Thin pools allocate the whole volume on import (dm-thin has no write-zeroes) | Medium | Documented (Data handling) |
| Claim exclusion between concurrent reconciles of different VMs can race | Low | Partially fixed: the `ClaimInUse` check runs before both the import Pod and the Anchor Pod are created; a race between concurrent reconciles of two VMs remains; `ReadWriteOncePod` for claims Baryon creates is Planned (Future hardening) |
| Logical block sizes above 4 KiB broke the zeroing alignment | Low | Fixed: such devices are refused |
| The initial zeroing of the device ends was not flushed before the download | Low | Fixed: flushed before the download |
| An HTTPS import without a digest accepted a truncated body | Low | Fixed: the length must equal `Content-Length`, and without a digest a response without `Content-Length` is refused |
| `record_import` reported success without writing the record | Low | Fixed: it writes only for a mutually bound claim and volume and otherwise reports `DiskBindingPending` |
| The record rule did not require a Bound PersistentVolume | Note | Fixed: `bound_to` and `baryon-controller-writes` require it |
| The readiness of earlier releases compares timestamps with one-second resolution | Note | Accepted: a StorageReady=True of an earlier claim with the same name that changed in the same second as the current claim's creation counts for the current claim; the effect is a refusal or a disk treated as already imported (the VM boots an empty volume), never an import over a used disk |
| PersistentVolume `get` is cluster-wide and PersistentVolume `patch` depends on `baryon-controller-writes` | Low | Documented (RBAC) |

Findings of the 2026-10-03 review of VM placement:

| Finding | Severity | State |
|---|---|---|
| Placement allowed system PriorityClasses, cross-namespace pod affinity, and tolerations of control-plane and out-of-service taints | Medium | Fixed: CRD validation rules; Tested by the CI API server checks |
| Label sync removed labels set by other actors | Low | Fixed: only recorded keys are removed; Tested by the placement E2E on the k0s cluster |
| Recorded label keys were read from a user-writable annotation without the reserved-key filter | Low | Fixed: the keys are filtered with `reserved_label`; unit test |

Risks of the guest network health change (2026-10-04: the DHCP responder
restart policy, runtime protocol 1.2, VirtualizationNode `status.vms`, and
`NetworkReady`):

| Risk | State |
|---|---|
| Unbounded responder restarts: a guest that can crash the responder's parser gets a new responder about once a minute for as long as the VM runs, instead of 11 in total | Accepted (Guest attack surface; ADR 0007 revision of 2026-10-04): each restart is a fresh exec with the VMM's confinement, the responder holds no state, and without it the guest loses its address when its lease expires |
| Node-sourced text in VM conditions: a node agent writes the reason and message that reach `NetworkReady` | Mitigated: baryon-controller sets the reason from the state, never from the node's text, and caps the node's message at 1024 bytes; the CRD limits `state` to an enum and `reason` and `message` to 64 and 1024 bytes; `baryon-node-agent-own-node` limits a node agent to its own node's VMs |
| Repeated guest DHCP requests driving the responder log and baryon-virtd's work | Mitigated: at most one acknowledgement per 60 s reported by the responder and processed by baryon-virtd; repeats are logged at debug level. The log is not size-capped while the VM runs |
| A responder that hangs without exiting | Fixed (2026-10-04): baryon-virtd probes the serving loop every 10 s through the responder's stdin and kills and restarts a responder that answered no probe for 25 s; `NetworkReady` is True only after the first answer, and after a failure only once the new responder has kept answering for 25 s (review of probes and seccomp below). Tested: unit tests and the hang cases of `tests/e2e/network-health.sh` on the k0s cluster |

Findings of the 2026-10-04 review of the live migration evaluation
(ADR 0013). Live migration is Not supported, so no finding applies to the
running system; each is recorded as an entry criterion for a future design
(see [Live migration](#live-migration-not-supported)):

| Finding | Severity | State |
|---|---|---|
| A lost final acknowledgement, or an error after it, resumes the source while the destination runs (v53 pre-copy and post-copy, main pre-copy); the source resumes without its disk locks | High | Not applicable (no migration); entry criteria 2 and 3 in ADR 0013 |
| The destination adopts the source's configuration from the stream (Landlock settings, disk and console paths) and deserializes full device state | High | Not applicable; entry criterion 4 |
| The receive endpoint is unauthenticated (plain TCP), not bound to a source (TLS), accepts up to 128 additional connections from any peer, and has no I/O timeouts | High | Not applicable; entry criterion 5 |
| A relay in baryon-virtd would hold plaintext guest memory and node credentials and listen on TCP; authorization and destination choice are undefined | Medium | Not applicable; entry criterion 6 and the runtime ownership question in ADR 0013 |
| Disk locks are host-local and released before the switchover; `ClaimInUse` and `DiskInUse` do not cover two copies of one VM on different nodes, so fencing must be enforced on the data path | Medium | Not applicable; entry criterion 3 |
| Memory payload sizes are chosen by the peer | Low | Not applicable; entry criterion 5 |
| The destination checks CPU compatibility by CPUID only; MSRs and mitigation-relevant enumeration are unverified | Low | Not applicable; entry criterion 9 |
| Cloud Hypervisor's seccomp profile allows `userfaultfd` and inet sockets that Baryon does not use | Low | Fixed (2026-10-04): Baryon's seccomp filter denies `userfaultfd` and every socket family other than `AF_UNIX` on every thread of the VMM and the responder ([Baryon seccomp layer](#baryon-seccomp-layer)) |
| The experiment ran outside Baryon's confinement (root VMM, host network namespace, plain TCP, no Landlock, shared device outside claim exclusion) | Note | Documented (ADR 0013 and above); validates no security property |

Findings of the 2026-10-04 review of the DHCP responder liveness probes and
Baryon's seccomp layer:

| Finding | State |
|---|---|
| The unit test for `userfaultfd` did not set `UFFD_USER_MODE_ONLY`, so it passed on hosts that refuse the privileged call anyway | Fixed: the test uses the user-mode-only flag and requires an unfiltered control child to succeed |
| The E2E seccomp check read only the main thread of each process | Fixed: `lifecycle.sh` reads every thread of the VMM and the responder |
| `ptrace`, `process_vm_readv`, `process_vm_writev`, `pidfd_getfd`, and sockets other than `AF_UNIX` were reachable from threads without a Cloud Hypervisor filter | Fixed: denied by Baryon's filter |
| Every bad responder output line produced a warning in baryon-virtd's log, without bound | Fixed: the first protocol violation stops reading and restarts the responder, so warnings are bounded by the restart backoff |
| The outage scripts overwrote any existing affinity of the node agent DaemonSet | Fixed: `network-health.sh` and `node-agent-outage.sh` refuse to run if the DaemonSet already has an affinity |
| A responder that hung while its VM was stopping was restarted | Fixed: no probes and no restart while the VM stops |
| The kill on a foreign architecture was not tested | Fixed: unit test (i386 call through `int 0x80` ends in `SIGSYS`) |
| The reaper of a killed responder waited without bound | Fixed: it waits at most 60 s, then logs a warning and stops waiting |
| The DHCP responder has no allowlist seccomp filter and no Landlock rules | Open (Future hardening, Designed) |
| Cloud Hypervisor's query of the TAP MTU in the VMM's empty network namespace may fail silently | Mitigated: the guest gets the pod interface MTU through DHCP option 26; not verified at runtime |

Findings of the 2026-10-05 review of the v0.1 API changes:

| Finding | Severity | State |
|---|---|---|
| Validation of `rootDisk` and `className` is not applied to objects stored earlier, and the importer did not refuse userinfo | Low | Fixed for URLs: the importer refuses userinfo in the source URL and redirect targets. Names and sizes of stored objects are not re-checked by the controller; the API server refuses invalid claims and pods it creates from them |
| Redirect targets were logged in full and could reach `ImportFailed`; the importer did not refuse userinfo in them | Low | Fixed: logs and messages show scheme, host, port, and path only; userinfo refused |
| The node agent's stop write could overwrite a newer write of the replacement agent on the same node | Low | Fixed: applied with the read `resourceVersion` as a precondition |
| `ClaimRejected` copies API server text decided for the controller's identity into the VM's conditions | Note | Documented (Data handling) |
| Host inventory was readable cluster-wide by every node agent through VirtualizationNodes | Note | Fixed: no longer published; logged by the node agent |
| Explicit nulls passed `has()` checks in CEL rules while the controller saw absent fields | Note | Fixed: `nullable` removed, nulls are pruned; CI case added |
| Duplicates in `runtime.features` or `status.vms` would fail the node agent's whole status write | Note | Fixed: the node agent removes duplicates before writing |

Findings of the 2026-10-05 v0.1.0-rc1 release gate review:

| Finding | Severity | State |
|---|---|---|
| H1: GitHub artifact attestations need a public repository on the current plan; wrkode/baryon is private, so a release has no provenance attestations and its images and files need GitHub access | High | Resolved for v0.1.0-rc1 by disclosure: no attestations are created or claimed for a release from the private repository; the workflow attests only when the repository is public, and the docs say what a private release carries ([Release and supply chain](#release-and-supply-chain)). Making the repository public is the owner's decision |
| M1: base and builder images were pinned by tag | Medium | Fixed: pinned by digest in `packaging/versions.env` |
| M2: baryon-node-agent is node root through the device plugin: its `Allocate` answer decides which host devices and paths a container gets | Medium | Documented ([Trust boundaries](#trust-boundaries)); a separate minimal device plugin process is Planned |
| L1: the release workflow does not check CI results or `main` ancestry, and tags are not protected | Low | Accepted for v0.1.0-rc1; Planned |
| L2: build and publish run in one job that holds the write tokens | Low | Accepted for v0.1.0-rc1; Planned |
| L3: the released baryon-virtd binary came from a separate build, not from the host image | Low | Fixed: the workflow copies it from the host image |
| L4: `vhost_net` and `vhost_vsock` were loaded on every node although unused | Low | Fixed: no longer listed in `modules-load.d`. virtiofsd is still shipped and unused: Accepted; its removal until used is Planned |
| L5: `baryon-system` enforces Pod Security `privileged`, so pod create, update, and exec there are node-root-equivalent | Low | Documented ([Kubernetes authorization](#kubernetes-authorization)) |
| N1: DHCP responder events carry no responder generation, and a killed responder that the reaper stops waiting for after 60 s can remain a zombie | Note | Open |
| N2: the attach log line carried anchor-supplied names before they were validated | Note | Fixed: VM name and namespace are logged cut to 253 bytes, required features to 16 entries of 253 bytes |
| N3: one importer refusal message named the full source URL | Note | Fixed: it shows the URL redacted (scheme, host, port, path) |
| Final gate: the release job holds unused write permissions (dry run, private repository), and the SBOM action receives the job token by default | Low | Accepted for v0.1.0-rc1; covered by the split-jobs item (Planned) |
| Final gate: the k0s binary's integrity rests on kairos-init | Note | Documented ([Release and supply chain](#release-and-supply-chain)); upstream dependency |

Findings of the 2026-10-27 reviews of ADR 0014 (Proposed), the move to
Cloud Hypervisor v54.0, and the M1 and M2 fixes (ADR 0015; M5 to
L10 are from the first gate review of those fixes, M7 to N8 from the second,
on 2026-10-28):

| Finding | Severity | Status |
|---|---|---|
| H1: a Secret reference in a VirtualMachine would give subjects bound to `baryon-vm-edit`, which has no rights on Secrets, read access to any Secret of the namespace | High | Resolved in the ADR: admission policy `baryon-vm-guest-config` on `spec.guestConfig.userDataSecretRef` (Designed; the draft named them `baryon-vm-user-data` and `spec.userData.secretRef`) |
| H2: user data in `VmSpec` would be stored in the runtime record or break the identical-spec reattach after a baryon-virtd restart or a Secret edit | High | Resolved in the ADR: baryon-anchor builds the seed and passes it as a disk; baryon-virtd never handles user data |
| M1: any process in the pod cgroup with the VM's identity, including exec'd processes, the VMM, and the DHCP responder, can call `Stop` and `Attach`, and the newest attach becomes the VM's anchor | Medium | Fixed (ADR 0015): lifecycle calls only from the Anchor supervisor, bound to one live process; non-dumpable anchor with SIGTERM from root only; admission refuses `shareProcessNamespace`, `hostPID`, `hostUsers: false`, extra containers, and ephemeral containers. Unit tests, CI API server checks (local run), and E2E `tests/e2e/anchor-authority.sh` on the k0s cluster |
| M2: `pods/exec` on a block-mode Anchor Pod allows overwriting the running VM's root disk with `baryon-anchor import`, whose approval is optional on its command line | Medium | Fixed: the importer is a separate binary and image, `baryon-importer`, used only by import Pods, and requires the approval file and the claim UID; `baryon-anchor` has no import command. Unit tests; E2E `tests/e2e/anchor-authority.sh` and the import E2E suites on the k0s cluster |
| M5: `pods/exec` on an approved import Pod can run a second `baryon-importer` that writes another image while the first exits 0 and the volume records the original source | Medium | Fixed for `pods/exec`: `baryon-importer` runs only as the container's init process, and the import image has no other program; a file import locks the disk directory (the lock: unit test; the PID 1 rule: E2E `tests/e2e/anchor-authority.sh` on the k0s cluster). The same outcome through an image change is M7 |
| M6: the SIGTERM sender filter is not verified end to end; if kubelet's SIGTERM were ignored, every pod deletion would stop the VM hard after the grace period without a failing test | Medium | Addressed: the authority E2E asserts, with `NODE_SHELL`, the anchor's final `Stop` on Anchor Pod replacement and VM deletion, and in every run that the anchor logged its handling of the runtime's SIGTERM and no ignored SIGTERM; passed on the k0s cluster for replacement and deletion in both volume modes |
| L8: the registered anchor was matched by the connection's recorded PID alone | Low | Fixed: the caller's pidfd must show it alive as well (unit test) |
| L9: `Stop` was refused between a baryon-virtd restart and the anchor's reattach | Low | Fixed: `Stop` follows the container cgroup takeover rule of `Attach` (unit test) |
| L10: baryon-controller acted on an `IdentityConflict` termination message that any process of the Anchor container can write | Low | Fixed: the anchor's dedicated exit code is required as well (unit tests) |
| M7: namespace users who can update pods can change the image of an Anchor Pod or an import Pod; kubelet restarts the container with it as the init process, which is then the Anchor supervisor or an approved importer | Medium | Fixed: admission policy `baryon-pod-immutable` (CI API server checks, local run; E2E `tests/e2e/anchor-authority.sh` on the k0s cluster) |
| L11: admission did not require the containers of Baryon pods to drop all capabilities and forbid privilege escalation, although the supervisor check relies on no `CAP_SYS_ADMIN` in the container | Low | Fixed: `baryon-vm-resource` validates the container security context (CI API server checks, local run) |
| L12: processes with the VM's uid can keep a SIGTERM pending so that the runtime's SIGTERM is merged and the VM stops hard after the grace period; each ignored SIGTERM was logged | Low | Log volume fixed: ignored SIGTERMs are logged once per doubling of their count (unit test). The merge is a residual until the VMM and the responder run in a PID namespace of their own (Planned) |
| L13: the authority E2E skipped the exec'd importer check when the import ended first, and reported the SIGTERM check as passed on an empty anchor log | Low | Fixed: both are failures now; the SIGTERM check requires the anchor's own log line |
| N7: baryon-virtd left a live VMM running when its runtime record could not be adopted after a restart, so a reattaching anchor would start a second VMM on the same disk | Note | Fixed: baryon-virtd kills that VMM through its pidfd (unit test) |
| N8: baryon-importer was dumpable | Note | Fixed: it makes itself non-dumpable after its PID 1 check |
| M3: guest terminal output would reach the operator's terminal unfiltered | Medium | Resolved in the ADR: output filtering in `baryon-anchor console` (Designed); allow-list made precise (L14) |
| M4: console and user data bounds were anchor-side only; no explicit gRPC message limits; Cloud Hypervisor's serial input queue is unbounded | Medium | Resolved in the ADR: bounds enforced by baryon-virtd (Designed) |
| L1: the Landlock writable-directory rationale and the VMM directory contents were stale for v54.0 (`serial.sock.lock`) | Low | Documented; removal of the rule with v54.0 Open |
| L2: the reason for `AF_NETLINK` in Cloud Hypervisor's VMM thread filter was not cited | Low | Documented (upstream commit 98b1d50a0b); Baryon keeps it denied |
| L3: virtio-net offloads on re-expose the kernel's header validation to guest-chosen values; UFO likely unneeded | Low | Accepted (checksum, TSO); UFO Open; re-evaluate on TAP or virtio-net header advisories |
| L4: v0.1.0-rc1 (Cloud Hypervisor v53.0 default) exposed nested virtualization to guests on hosts whose KVM module allows it | Low | Fixed in v0.1.0-rc2 (`nested=off`): in its release qualification, guests on v0.1.0-rc1 hosts listed `vmx`, and the same guests after the host upgrade, and new guests, had no `vmx` or `svm` flag ([testing.md](testing.md#v010-rc2-release-qualification)) |
| L5: console log content during sessions, the replay, termination messages, and the guest-side lifetime of user data were not recorded | Low | Documented (ADR 0014 Consequences, Data handling) |
| L6: console sessions cannot be attributed to a Kubernetes user by baryon-virtd | Low | Designed: session IDs and an audit rule example |
| L7: plain `kubectl exec` by pod name can reach a pod another user created under that name while the VM is stopped | Low | Designed: baryonctl checks the pod's controller owner reference and that its container requests the vm resource (L16); plain `kubectl exec` has no such check |

Findings of the 2026-10-28 review of the rewritten ADR 0014 (Accepted as
a design; nothing implemented):

| Finding | Severity | Status |
|---|---|---|
| M8: the placement-only feature resource of guest configuration was not restricted, so any pod creator could use up its capacity on every node | Medium | Resolved in the ADR: `baryon-vm-resource` and `baryon-pod-immutable` cover it (Designed) |
| M9: `baryon-vm-guest-config` fails open when absent | Medium | Resolved in the ADR: classified load-bearing, shipped in a refusing form one release before the field, checked by baryon-controller, CI API server checks (Designed) |
| M10: the pod's `/dev/shm` outlives the Anchor container, so a seed created as a new file blocks a restart or is reused stale | Medium | Resolved in the ADR: the new anchor replaces it fd-relative (Designed) |
| M11: the node-wide anchor socket has no per-pod connection or stream limits, and `Console` adds long-lived streams from exec'd processes | Medium | Resolved in the ADR: per-pod limits, identification off the accept loop (Designed) |
| L14: the terminal filter allow-list was by final byte only (private-prefix SGR, reporting modes, DECRQM, XTVERSION, ENQ, `ESC Z`, C1) | Low | Resolved in the ADR (Designed) |
| L15: the Secret check is a snapshot with Deployment-like semantics; Secret writers control what the guest runs | Low | Documented |
| L16: baryonctl's owner reference check can be forged by a pod creator | Low | Resolved in the ADR: the vm resource request is required as well (Designed) |
| L17: `hostIPC` was not refused, which would put the seed in the node's `/dev/shm` | Low | Resolved in the ADR (Designed) |
| L18: the seed open, the `Console` caller rule, and the projected Secret keys were imprecise | Low | Resolved in the ADR (Designed) |
| N9: ADR 0015 named a console bridge that the chosen design does not have; a controller downgrade could start a referencing VM without its user data | Note | Resolved: ADR 0015 wording corrected; a downgrade below the release that adds the field is documented as not supported |

Findings of the 2026-10-28 v0.1.0-rc2 release gate review:

| Finding | Severity | Status |
|---|---|---|
| rc2 gate H1: CI's dependency gate failed on the frozen candidate (RUSTSEC-2026-0414, pulldown-cmark, a build dependency) | High (process) | Fixed before the tag: pulldown-cmark 0.13.5; the candidate was frozen again and qualified on the new commit |
| rc2 gate L1: `baryon-vm-resource` does not refuse `hostIPC`, `hostNetwork`, or hostPath volumes; only a mutating webhook could add them, and a `hostNetwork` Anchor is refused by the supervisor check | Low | Accepted for v0.1.0-rc2; `hostIPC` refusal Designed with ADR 0014 (after v0.1 stabilization, [roadmap.md](roadmap.md)) |
| rc2 gate L2: the image SBOMs are generated from the image tag in the local daemon, not from the pushed digest | Low | Open; use the digest outputs (Planned) |

Dependency check of 2026-10-05: cargo audit 0.22.2 and cargo deny 0.20.2
report no vulnerabilities in the Rust dependencies. Cloud Hypervisor
v53.0 is not affected by its published security advisories. k0s
v1.36.4+k0s.1 includes the fix for GHSA-qjgc-wmg7-3jc3.

Check of 2026-10-27 for the move to Cloud Hypervisor v54.0: it is not
affected by its published security advisories (the most recent,
GHSA-f47p-p25q-83rh, is fixed in v52.0 and v51.2). Its release adds a
main-thread seccomp filter, hardens disk image parsing, and fixes
guest-triggerable VMM panics; it also adds `AF_NETLINK` to the VMM thread's
`socket` rule, which Baryon's filter keeps denied
([Baryon seccomp layer](#baryon-seccomp-layer)).

Dependency update of 2026-10-27, after `cargo deny check advisories`
started failing on new advisories: rustls 0.23.46 (RUSTSEC-2026-0346,
0347, 0348, 0349), rustls-webpki 0.103.16 (RUSTSEC-2026-0382),
webpki-roots 1.0.11 (RUSTSEC-2026-0381), webpki-root-certs 1.0.11
(RUSTSEC-2026-0383), and tokio 1.54.1 (RUSTSEC-2026-0386, the unsound
`AsyncFd` constructors `new`, `with_interest`, and their `try_` variants).
baryon-virtd's two uses of
those constructors (the DHCP responder's packet socket and the wait for a
pidfd) now use `AsyncFd::register` and `AsyncFd::register_with_interest`
on an `OwnedFd` that the `AsyncFd` owns. cargo audit and cargo deny report
no advisories after the update.

Dependency update of 2026-10-28, before the v0.1.0-rc2 tag, after
`cargo deny check advisories` failed in CI on RUSTSEC-2026-0414
(pulldown-cmark 0.13.4, exponential event growth with
`ENABLE_WIKILINKS`): pulldown-cmark 0.13.5. pulldown-cmark is a build
dependency only (prost-build, through tonic-prost-build) and parses the
comments of Baryon's own `.proto` files; no Baryon binary contains it.
cargo deny reports no advisories after the update.

## Future hardening

| Item | Status |
|---|---|
| Reduce baryon-virtd's capability set; seccomp and Landlock for baryon-virtd. It must keep `CAP_SYS_PTRACE`, which reading the namespaces of a non-dumpable anchor needs (ADR 0015) | Designed |
| Start the VMM and the DHCP responder in a PID namespace of their own, so they cannot signal the anchor (ADR 0015 residual) | Planned; whether Cloud Hypervisor runs correctly as PID 1 is not verified |
| A seccomp filter in baryon-anchor after startup that denies `execve`, `execveat`, and new processes | Planned |
| A per-pod limit on AnchorService connections | Planned |
| Baryon seccomp layer for the VMM and the DHCP responder, under Cloud Hypervisor's filters: denies `userfaultfd`, `ptrace`, `process_vm_readv`, `process_vm_writev`, `pidfd_getfd`, sockets other than `AF_UNIX`, and foreign ABIs ([Baryon seccomp layer](#baryon-seccomp-layer)) | Tested: unit tests in CI; lifecycle E2E on the k0s cluster |
| A strict allowlist seccomp filter for the DHCP responder | Designed |
| Landlock rules for the DHCP responder | Designed |
| A Baryon-owned Landlock layer for the VMM beyond the writable-directory rule | Designed |
| Kubernetes user namespaces (`hostUsers: false`) as additional hardening for the Anchor container; not the VM identity contract (ADR 0011) | Planned (evaluation) |
| A `/dev` check that works with runtime bind-mounted device nodes before user namespaces are adopted | Planned |
| `ReadWriteOncePod` for claims Baryon creates where the CSI driver supports it | Planned |
| Admission policy limiting baryon-controller's PersistentVolume and pod updates (`baryon-controller-writes`) | Implemented, Tested (CI API server checks) |
| Record and verify the block device identity across VMM recovery | Planned |
| Feature-aware and capability-aware placement (one placement-only resource per runtime protocol feature or consumed host capability, ADR 0012) | Planned; deferred until Baryon consumes a capability |
| Dependency gate in CI (`cargo deny check advisories licenses sources bans`) | Implemented |
| A smaller parsing surface in baryon-virtd | Designed |
| Release workflow check of CI results and `main` ancestry before publishing; protected release tags | Planned |
| Separate build and publish jobs, so that only the publish job holds write tokens | Planned |
| The device plugin in a separate minimal process, apart from the node agent's API client | Planned |
| Remove virtiofsd from the host image until Baryon uses it | Planned |
| AppArmor or SELinux profiles for baryon-virtd and the VMM | Planned |
| AnchorService peer checks: refuse peers in baryon-virtd's PID namespace; `Attach` and `Stop` only from the Anchor supervisor; a live anchor is never replaced (finding M1 of 2026-10-27) | Implemented (ADR 0015) |
| The importer separated from the Anchor binary and image, and its approval and claim UID required (finding M2 of 2026-10-27) | Implemented |
| `kvm_intel.nested=0` and `kvm_amd.nested=0` in the Baryon-enabled image | Planned |

## Not supported

VFIO and PCI passthrough, SR-IOV, snapshots, and confidential computing
(SEV-SNP, TDX, measured boot, attestation) are not supported. Live
migration is currently unsupported because Baryon's network handoff
model requires destination-side file descriptor injection during Cloud
Hypervisor migration, which is not available in the currently supported
Cloud Hypervisor release. See [Live migration](#live-migration-not-supported)
and [ADR 0013](adr/0013-live-migration.md). The current handoff is host-mediated by design: baryon-virtd resolves disks,
creates the TAP, starts the DHCP responder, and writes SMBIOS strings. None
of that is an attestation boundary, and confidential computing would need a
different design.
