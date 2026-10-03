# Security model

This is Baryon's threat model and the record of every privileged path. It is
maintained with the implementation. Each claim carries a status:

- Implemented: in the code.
- Tested: exercised on a single Baryon-enabled Hadron node running k3s with
  flannel ("Hadron node") and on a historical development cluster in which
  a kind node container acted as the host ("kind host", no longer used),
  unless the entry names one environment; see
  [testing.md](testing.md#results). All of it is single-node evidence.
  Tested means the behavior was observed, not that it was verified under
  attack.
- Designed: decided, not implemented.
- Not supported: out of scope today.

Last full review: 2026-10-03 (baryon-security review of the privileged host
implementation and the runtime-boundary experiments). Findings and their
resolution are listed in [Review findings](#review-findings).

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
| baryon-node-agent | DaemonSet pod | uid 0 in the container | drop ALL | Implemented, Tested |
| baryon-controller | Deployment pod | uid 65532 | drop ALL | Implemented, Tested |

All pods set `allowPrivilegeEscalation: false`, `readOnlyRootFilesystem:
true`, and seccomp `RuntimeDefault`. No container is privileged or adds a
capability. The Anchor Pod meets Pod Security `restricted`, has no
service account token (`automountServiceAccountToken: false`), and has no
hostPath volume.

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
| Open PVC data | Cloud Hypervisor, as the VM's identity | baryon-virtd resolves the disk with `openat2(RESOLVE_IN_ROOT, RESOLVE_NO_MAGICLINKS)` under the anchor's `/proc/<pid>/root` and passes an `O_PATH` fd; the VMM reopens it through `/proc/self/fd/<n>` with its own credentials | Implemented, Tested (filesystem mode); block mode Implemented, not tested |
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
| baryon-controller | none | | |

A user who can exec into an Anchor container can open /dev/kvm there, within
the pod's limits.

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
| baryon-controller | virtualmachines: get, list, watch, patch, update; status: patch; finalizers: update; virtualmachineclasses: get, list, watch; pods: get, list, watch, create, delete; persistentvolumeclaims: get, list, watch, create; events: create, patch. All cluster-wide. | Pod and claim rights are needed to place Anchor Pods in VM namespaces. A compromised controller can create or delete pods in any namespace, but the pods it can give the vm resource are only those it creates itself. |
| baryon-node-agent | nodes: get; virtualizationnodes: get, create; status: get, patch. Cluster-wide. | Narrowed per node by admission, below. |

### Admission policies

`deploy/base/policy/` holds two ValidatingAdmissionPolicies. They need
Kubernetes 1.30 or later, and 1.32 or later for the node name in service
account identities. Both are Implemented and Tested on the kind host
(Kubernetes 1.34) and the Hadron node (k3s 1.36); the CI API server checks
(k0s 1.36) confirm the `baryon-vm-resource` denial.

| Policy | Rule |
|---|---|
| `baryon-vm-resource` | A pod that requests `virtualization.baryon.io/vm` in any container or init container may be created only by `system:serviceaccount:baryon-system:baryon-controller`. Other users, including cluster administrators and workload controllers, are denied. |
| `baryon-node-agent-own-node` | baryon-node-agent may create or update only the VirtualizationNode named after the node in its token (`userInfo.extra["authentication.kubernetes.io/node-name"]`). Requests without that information are denied. |

Without these policies, any pod admitted through the API server could
request the vm resource, receive /dev/kvm and the anchor socket, and ask
baryon-virtd to start a VM in its own pod, running as the identity of the
VM UID it names. The policies are part of the supported installation.

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
  and are used by the unprivileged import init container, in the pod network
  namespace. A plain `http://` source requires `sha256` (API validation, and
  the importer refuses it too). A redirect from HTTPS to plain HTTP is
  refused. Without a digest, an HTTPS source is trusted as served. What the
  importer can reach is what the pod can reach; constrain it with
  NetworkPolicy. Imported images are mode 0600, owned by the VM's identity.
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

## Future hardening

| Item | Status |
|---|---|
| Verify the anchor through the kubelet pod resources API (pod was allocated the vm resource), in addition to the identity check | Designed |
| Reduce baryon-virtd's capability set; seccomp and Landlock for baryon-virtd | Designed |
| A Baryon-owned seccomp or Landlock layer for the VMM on top of Cloud Hypervisor's | Designed |
| Kubernetes user namespaces (`hostUsers: false`) as additional hardening for the Anchor container; not the VM identity contract (ADR 0011) | Planned (evaluation) |
| Dependency gate in CI (cargo-deny or cargo-audit) and a smaller parsing surface in baryon-virtd | Designed |
| AppArmor or SELinux profiles for baryon-virtd and the VMM | Planned |

## Not supported

VFIO and PCI passthrough, SR-IOV, live migration, snapshots, and confidential
computing (SEV-SNP, TDX, measured boot, attestation) are not supported. The
current handoff is host-mediated by design: baryon-virtd resolves disks,
creates the TAP, starts the DHCP responder, and writes SMBIOS strings. None
of that is an attestation boundary, and confidential computing would need a
different design.
