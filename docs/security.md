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
Findings and their resolution are listed in
[Review findings](#review-findings).

## Trust boundaries

From least to most trusted on a node:

1. Guest OS. Untrusted. It reaches the host only through Cloud Hypervisor
   device emulation (virtio-blk, virtio-net, virtio-rng, serial, ACPI,
   firmware) and through the DHCP responder, a separate unprivileged
   process.
2. Cloud Hypervisor (the VMM). One process per VM, running as the VM's own
   unprivileged host identity with no capabilities, `no_new_privs`, an empty network
   namespace, seccomp, and Landlock. Assumed compromisable by its guest; the
   confinement below is what limits that.
3. baryon-anchor. Unprivileged process in the Anchor Pod. Untrusted by
   baryon-virtd: every field it sends is validated, its identity must match
   the VM it names, and every resource it asks for is bounded by the pod
   cgroup and by what its uid can open.
4. baryon-node-agent. DaemonSet pod, uid 0 without capabilities, reaching the
   host only through two mounts.
5. baryon-controller. Unprivileged Deployment, no host access.
6. baryon-virtd. Root on the host. The trusted computing base of the node
   runtime. It is not a Kubernetes client.
7. Kubernetes API server, and the host OS and kernel (the Hadron image).

Channels across these boundaries:

| From | To | Channel |
|---|---|---|
| guest | VMM | virtio devices, serial, ACPI, SMBIOS, firmware |
| guest | DHCP responder | DHCP frames on the TAP |
| DHCP responder | baryon-virtd | one JSON event line per acknowledged lease (length-limited, checked against the lease) |
| VMM | baryon-virtd | process exit, the Cloud Hypervisor REST API (baryon-virtd is the client), serial socket, event file |
| baryon-anchor | baryon-virtd | gRPC on `/run/baryon/anchor/virtd.sock` |
| baryon-node-agent | baryon-virtd | gRPC on `/run/baryon/node/virtd.sock` |
| baryon-controller, baryon-node-agent | API server | HTTPS with service account tokens |

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
| baryon-anchor import | import Pod container, only while an import is pending | the same VM identity | drop ALL | Implemented, Tested (k0s cluster) |
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
| Import an image | the import Pod (`baryon-anchor import`), as the VM's identity, in the pod network namespace | writes the claim before the first Anchor Pod exists, and only after its approval file equals the claim UID; requests the placement-only `virtualization.baryon.io/import` resource and receives neither /dev/kvm nor the anchor socket | Implemented, Tested (deletion E2E checks the importer's view; import integrity E2E; k0s cluster) |
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
| `/run/baryon/anchor/virtd.sock` | 0666 | `/run/baryon/anchor`, 0755 | Per connection: `SO_PEERCRED` and `SO_PEERPIDFD`; the caller must be non-root and in a pod cgroup. Per attach: the caller's uid and gid must equal the host identity of the VM named in the request (`IdentityMismatch` otherwise), and no other VM on the node may have that identity (`IdentityConflict`). | Implemented, Tested (identity check); the conflict refusal is Implemented with unit tests |
| `/run/baryon/node/virtd.sock` | 0600, root | `/run/baryon/node`, 0700 | File mode, and NodeService requires peer uid 0 | Implemented, Tested |
| `/run/baryon/vms/<pod-uid>/ctl/api.sock` (Cloud Hypervisor API) | root | `ctl`, 0700, root | File permissions only; the REST API has no authentication. baryon-virtd binds it and passes the listener fd to the VMM, which never needs the path. | Implemented, Tested |
| `/run/baryon/vms/<pod-uid>/vmm/serial.sock` | created by the VMM | `vmm`, a private tmpfs, 0700, owned by the VM's identity | File permissions; baryon-virtd connects only through a checked `O_PATH` descriptor (see VMM confinement) | Implemented, Tested |

The anchor socket is world-writable because anchors run as per-VM
identities. Who
can reach it is limited by the mount (only containers allocated the
`virtualization.baryon.io/vm` resource get it) and by the admission policy
on that resource. A process on the host, outside any pod cgroup, is refused.

The anchor check proves pod membership and the VM's identity, not that the
caller is the anchor binary. A compromised VMM runs as the same identity in the
same container cgroup and passes it; it can then only reattach to or stop its
own VM, because a different spec for the same pod is refused.

## Kubernetes authorization

### RBAC

| Subject | Rights | Notes |
|---|---|---|
| baryon-controller | virtualmachines: get, list, watch, patch, update; status: patch; finalizers: update; virtualmachineclasses: get, list, watch; pods: get, list, watch, create, delete, patch; persistentvolumeclaims: get, list, watch, create; persistentvolumes: get, patch; events: create, patch. All cluster-wide. | Pod and claim rights are needed to place Anchor Pods and import Pods in VM namespaces. A compromised controller can create or delete pods in any namespace, but the pods it can give the vm or import resource are only those it creates itself. PersistentVolume `get` is cluster-wide, so a compromised controller can read every PersistentVolume spec (backend volume handles, node affinity, mount options). PersistentVolume `patch` and pod `patch` are safe only with `baryon-controller-writes` installed, which limits them to adding the import record once and, on Baryon pods, to the import approval, the `vm-label-keys` annotation, and labels with keys that are not reserved; without the policy a compromised controller could rebind volumes, change reclaim policies, or relabel workloads. Claim `patch` is no longer granted. |
| `baryon-vm-edit` (ClusterRole, aggregated into `edit` and `admin`) | virtualmachines: get, list, watch, create, update, patch, delete, deletecollection; virtualmachineclasses: get, list, watch | For namespace users (`deploy/base/tenant/roles.yaml`). No `virtualmachines/status`: baryon-controller is its only writer. Nothing on PersistentVolumes. |
| `baryon-vm-view` (ClusterRole, aggregated into `view`) | virtualmachines, virtualmachineclasses: get, list, watch | Read access for namespace users. |
| baryon-node-agent | nodes: get; virtualizationnodes: get, create; status: get, patch. Cluster-wide. | Narrowed per node by admission, below. |

### Admission policies

`deploy/base/policy/` holds four ValidatingAdmissionPolicies. They need
Kubernetes 1.30 or later, and 1.32 or later for the node name in service
account identities. `baryon-vm-resource` and `baryon-node-agent-own-node`
are Implemented and Tested on the kind host (Kubernetes 1.34) and the
Hadron node (k3s 1.36); the CI API server checks (k0s 1.36) confirm the
`baryon-vm-resource` denial for a pod that requests the vm resource, a pod
that requests the import resource, and an init container that requests the
vm resource. `baryon-controller-writes` and `baryon-import-approval` are
Implemented and Tested by the CI API server checks, which run every allow
and deny case below by impersonation, and by the import integrity E2E on
the k0s cluster, which runs with both installed and checks that a namespace
user cannot set an import approval (see [testing.md](testing.md#results)).

| Policy | Rule |
|---|---|
| `baryon-vm-resource` | A pod that requests `virtualization.baryon.io/vm` or `virtualization.baryon.io/import` in any container or init container may be created only by `system:serviceaccount:baryon-system:baryon-controller`. Other users, including cluster administrators and workload controllers, are denied. The import resource grants nothing; it is restricted so that other pods cannot use up the slots that place import Pods. |
| `baryon-node-agent-own-node` | baryon-node-agent may create or update only the VirtualizationNode named after the node in its token (`userInfo.extra["authentication.kubernetes.io/node-name"]`). Requests without that information are denied. |
| `baryon-controller-writes` | Updates of PersistentVolumes and pods by baryon-controller. A PersistentVolume may only gain the annotation `virtualization.baryon.io/disk-import` when it has none, with spec, labels, finalizers, owner references, and every other annotation unchanged; the volume must be Bound, and the record must have the form `{"vmUID":"<uuid>","claimUID":"<claimRef.uid>","source":"sha256:<64 hex>"}` with the volume's own `claimRef.uid`. A pod update may change only the annotations `virtualization.baryon.io/import-approved` and `virtualization.baryon.io/vm-label-keys` and labels whose key is not reserved, with spec, finalizers, owner references, and every other annotation unchanged, and only on pods labeled `app.kubernetes.io/managed-by=baryon`. Reserved keys: `app.kubernetes.io/managed-by`, and keys whose prefix is `baryon.io`, `kubernetes.io`, `k8s.io`, or a subdomain of them other than `app.kubernetes.io` (the same rule as `reserved_label` in `crates/controller/src/anchor.rs`). CI checks: a PV spec change, another PV annotation, a record for another claim, rewriting and removing the record, and annotating an unmanaged pod are denied; adding the record for the bound claim and approving a Baryon pod are allowed. Copying `app` and `app.kubernetes.io/name` and recording their keys is allowed; adding `node.kubernetes.io/x`, changing `app.kubernetes.io/managed-by`, adding `virtualization.baryon.io/vm-name`, and relabeling an unmanaged pod are denied. |
| `baryon-import-approval` | Nobody but baryon-controller may set, change, or remove the annotation `virtualization.baryon.io/import-approved` on any pod, at creation or by update. Namespace users can patch pods, so without this policy they could approve an import into a volume the controller would refuse. CI checks: another user changing, removing, or setting the approval is denied; another user changing other annotations of a pod with an approval is allowed. |

The policies are part of the supported installation. Without them:

- `baryon-vm-resource`: any pod admitted through the API server could
  request the vm resource, receive /dev/kvm and the anchor socket, and ask
  baryon-virtd to start a VM in its own pod, running as the identity of the
  VM UID it names.
- `baryon-import-approval`: a namespace user could approve an import into
  a volume baryon-controller would refuse.
- `baryon-controller-writes`: baryon-controller's PersistentVolume and pod
  `patch` rights are unrestricted.

They are an API server control. They do not constrain static pods, which
the kubelet runs from a manifest directory on the node without admission
(only the API mirror pod is checked, and rejecting it does not stop the
real pod), nor the kubelet's device plugin `Allocate` path. Node-level
protection therefore relies on the immutable Hadron host and on nobody
untrusted being able to write the static pod directory; even then the
anchor identity check limits a pod to the VM whose identity it runs as. A node-level
check through the kubelet pod resources API is Designed. Extended resources
cannot be requested by ephemeral containers or through pod-level resources
today, so neither is a current bypass; the policy checks containers and
init containers.

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

Cloud Hypervisor then applies its own seccomp filters (`--seccomp true`) and
Landlock rules (`--landlock`). baryon-virtd adds one Landlock rule: read and
write access, including creating and removing files, beneath the VM's own
runtime directory `/run/baryon/vms/<pod-uid>/vmm` (owned by the VM's identity,
holding only the VMM's log, event file, and serial socket). Without it, a
guest reboot failed: Cloud Hypervisor could not remove and re-create its
serial socket. With it, the guest reset in the lifecycle E2E passes (Tested).

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

What this gives:

- A distinct host identity per VM: one VMM cannot signal, trace, or read
  the files of another. Two VMs with the same identity never run on one
  node: baryon-virtd refuses the second (`IdentityConflict`), and
  baryon-controller places it on another node (ADR 0011).
- No capabilities and `no_new_privs`: setuid binaries and file capabilities
  cannot raise privileges.
- Empty network namespace: a compromised VMM cannot open connections to host,
  node-local, or cluster addresses. Cloud Hypervisor's seccomp profile still
  allows `socket` for AF_UNIX, AF_INET, and AF_INET6 and `connect` without
  conditions (v53.0 `vmm/src/seccomp_filters.rs`, read on 2026-10-03);
  it is the empty namespace, not seccomp, that leaves nothing to connect to.
  Its only network path is the TAP fd, which carries guest frames to the pod
  interface, where the pod's NetworkPolicy applies as for any pod.
- Container cgroup: the pod's memory and CPU limits and any per-container
  cpuset apply to the VMM.

What it does not give:

- The VMM runs in the host mount namespace. File access is limited by DAC
  (its own identity) and by Cloud Hypervisor's Landlock rules, which are
  filesystem rules only. Baryon adds no seccomp policy and no Landlock
  policy of its own beyond the one writable-directory rule above.
- Path-based Unix sockets are reachable by path from any network namespace.
  The sockets above are protected by their modes, except the anchor socket,
  whose peer check applies.
- seccomp and Landlock profiles are Cloud Hypervisor's. Their contents are
  pinned by the pinned Cloud Hypervisor version, not reviewed by Baryon line
  by line.

## Guest attack surface

- Device emulation: virtio-blk, virtio-net, virtio-rng, serial, ACPI. A guest
  that exploits Cloud Hypervisor gains the VMM's confined identity above.
- DHCP: guest-sent DHCP frames are parsed by the DHCP responder process,
  not by baryon-virtd. The responder runs with the VMM's confinement (the
  VM's identity, no capabilities, `no_new_privs`, empty network namespace, Anchor
  container cgroup) and holds only the packet socket, its stdin and stdout
  pipes, and its log file. A guest that exploits the parser gains that
  identity, which it could reach through the VMM anyway. baryon-virtd reads
  the responder's events as untrusted input: lines are limited to 4 KiB and
  an event is ignored unless its address and MAC match the lease. The
  responder exits when its stdin closes, so it never outlives the
  baryon-virtd that started it; a responder that exits early is restarted
  with backoff, at most 10 times in a row.
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
- Source URL exposure: annotations (the import Pod's `import-source`, the
  PersistentVolume record) and Events carry only a digest of the URL
  (`sha256:<hex>`). The URL itself remains in the VirtualMachine spec,
  in the import Pod's arguments, and in the filesystem-mode import marker
  on the volume. URLs must not carry secrets.
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
  the VM spec, VMM pid and start time, uid, cgroup paths, and guest MAC. No
  secrets. A record is trusted after a restart only if the process start time
  and executable still match.
- Logs (`/var/log/baryon/vms/<namespace>_<name>_<pod-uid>/`, persistent on
  Kairos): console output (mode 0600, directories 0700), the Cloud
  Hypervisor log and output, and the DHCP responder log (all 0600, at most
  16 MiB each when copied). Console output can contain anything the guest
  prints, including credentials. On Hadron the parent directory is 0750
  root, and the image's tmpfiles.d entry removes per-VM log entries older
  than 14 days, except under the directories of running VMs, which
  baryon-virtd keeps under a shared BSD lock that systemd-tmpfiles honors
  (Tested on the Hadron node with a short override age). Deleting a
  VirtualMachine does not remove its logs sooner.

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
| Pod-liveness check excluded the responder by raw pid; responder failure count reset on every attach | Low | Fixed: exclusion only while the responder's pidfd is alive; the count resets only after a stable run |
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

## Future hardening

| Item | Status |
|---|---|
| Verify the anchor through the kubelet pod resources API (pod was allocated the vm resource), in addition to the identity check | Designed |
| Reduce baryon-virtd's capability set; seccomp and Landlock for baryon-virtd | Designed |
| A Baryon-owned seccomp or Landlock layer for the VMM on top of Cloud Hypervisor's | Designed |
| Kubernetes user namespaces (`hostUsers: false`) as additional hardening for the Anchor container; not the VM identity contract (ADR 0011) | Planned (evaluation) |
| A `/dev` check that works with runtime bind-mounted device nodes before user namespaces are adopted | Planned |
| `ReadWriteOncePod` for claims Baryon creates where the CSI driver supports it | Planned |
| Admission policy limiting baryon-controller's PersistentVolume and pod updates (`baryon-controller-writes`) | Implemented, Tested (CI API server checks) |
| Record and verify the block device identity across VMM recovery | Planned |
| Feature-aware and capability-aware placement (one placement-only resource per runtime protocol feature or consumed host capability, ADR 0012) | Planned; deferred until Baryon consumes a capability |
| Dependency gate in CI (cargo-deny or cargo-audit) and a smaller parsing surface in baryon-virtd | Designed |
| AppArmor or SELinux profiles for baryon-virtd and the VMM | Planned |

## Not supported

VFIO and PCI passthrough, SR-IOV, live migration, snapshots, and confidential
computing (SEV-SNP, TDX, measured boot, attestation) are not supported. The
current handoff is host-mediated by design: baryon-virtd resolves disks,
creates the TAP, starts the DHCP responder, and writes SMBIOS strings. None
of that is an attestation boundary, and confidential computing would need a
different design.
