# 0015: Lifecycle authority belongs to the Anchor supervisor

Status: Accepted
Date: 2026-10-27

## Context

ADR 0009 and ADR 0011 authorized AnchorService calls by the caller's
kernel credentials: per connection, `SO_PEERCRED` and `SO_PEERPIDFD`; the
caller must be non-root and in a pod cgroup; per attach, its uid and gid
must equal the host identity of the VM it names. A caller that passed
could `Attach` (create or take over the VM's session) and `Stop` the VM.

The previous model assumed that the only process with a VM's host identity
in its pod is the anchor. That is not true. The host identity is the
identity of the VM's whole runtime context:

- the anchor (`baryon-anchor run`, the Anchor container's entrypoint);
- every process started in the Anchor container with `pods/exec`, which
  runs as the container's user;
- Cloud Hypervisor and the DHCP responder, which baryon-virtd starts as the
  VM's identity (ADR 0011) and places in the Anchor container's cgroup
  (ADR 0005).

Review finding M1 of 2026-10-27 ([security.md](../security.md#review-findings)):
any of these processes could stop the VM or become its anchor. The uid
answers which VM a process belongs to. It does not answer whether the
process may control the VM's lifecycle. `SO_PEERCRED` reports the pid, uid,
and gid of the connecting process, nothing about its role, so a check on
it alone cannot tell the anchor from the other processes of the same VM.

## Decision

Possession of a VM's runtime UID proves membership in that VM's runtime
context but does not grant Baryon lifecycle-control authority. Only the
designated Anchor supervisor may perform lifecycle-control operations for
that VM.

```
VM UID
  !=
lifecycle authority

Anchor supervisor identity
  ->
lifecycle authority
```

### The Anchor supervisor

The Anchor supervisor of a VM is the process that runs `baryon-anchor run`
as the init process (PID 1) of the Anchor container's PID namespace. The
container runtime starts it from the entrypoint that baryon-controller
sets in the Anchor Pod. No other process can be the init of that
namespace. Other processes of the pod can have the shape of a supervisor:
the pod sandbox process is the init of a PID namespace of its own. They
have no anchor socket and run no code a namespace user chooses, and after
the first attach the VM is bound to the container cgroup of its anchor.
That the init of the Anchor container runs `baryon-anchor run` rests on
the image that baryon-controller set, which nobody can change afterwards
(see Kubernetes controls below).

The anchor's own refusal to run as any PID other than 1 is a courtesy that
keeps an exec'd copy from touching the container's files. It is not part
of the boundary: baryon-virtd's checks are.

### Operations and what they require

| Operation | Requires |
|---|---|
| `Negotiate` (read-only) | The per-connection checks of ADR 0009 (non-root, in a pod cgroup; `Negotiate` names no VM, so there is no identity to compare), and a caller outside baryon-virtd's PID namespace: allowed to processes of the Anchor container, refused to the VMM and the DHCP responder |
| `Attach`, first attach of a pod (creates the VM) | The identity check, and the caller is an Anchor supervisor |
| `Attach` to an existing VM | The identity check, an Anchor supervisor, and the binding rules below |
| `Stop`, graceful or final | An Anchor supervisor that is the VM's live registered anchor, or, with no live registered anchor, a supervisor in the container that holds the VMM (then registered) |
| `Console` (ADR 0014, Designed) | Not lifecycle authority: a console session never attaches or stops the VM and never becomes its anchor |

Refusals carry the reason `NotAnchorSupervisor` with gRPC status
`PERMISSION_DENIED`. NodeService is unchanged (root only, owner-only
socket).

### How baryon-virtd recognizes the supervisor

All properties come from the kernel and are taken once per connection,
when baryon-virtd accepts it: the credentials from `SO_PEERCRED`, and from
`SO_PEERPIDFD` a pidfd for the exact process that connected. baryon-virtd
reads the caller's namespaces and cgroup through `/proc/<pid>` and then
checks that the pidfd still shows the process alive; while it does, the
numeric PID cannot belong to another process, so the files describe the
caller. A numeric PID alone is never used as an identity.

A caller is an Anchor supervisor when all of these hold:

- It is in the initial user namespace, the same as baryon-virtd. An
  unprivileged process can create a PID namespace only together with a
  new user namespace, so this condition means a PID namespace that the
  container runtime created.
- It is not in baryon-virtd's PID namespace, and its PID in its innermost
  PID namespace is 1. The PIDs come from the `NSpid` field of the pidfd's
  fdinfo, which follows the process, not the number.
- Its mount and network namespaces are not baryon-virtd's.

### Binding a VM to one process

- The first successful `Attach` of a pod registers the caller as the VM's
  anchor: its PID together with a pidfd that baryon-virtd keeps. It also
  records the container cgroup that will hold the VMM.
- A registered anchor is matched by PID and pidfd: the caller's own pidfd
  must show it alive, and then the registered pidfd must show it alive.
  Two live processes never share a PID, so the caller is the registered
  process. Reconnecting, and `Stop`, are allowed only for that process.
- While the registered process is alive, no other process can register,
  whatever its credentials.
- With no live registered anchor (the anchor exited, or baryon-virtd
  restarted), only a supervisor in the container cgroup that holds the
  VMM can register, through `Attach` or through `Stop`. An `Attach` from a
  supervisor of another container of the same pod means that the
  container was replaced. baryon-virtd then kills that VMM (reason
  `AnchorContainerReplaced`) and answers `UNAVAILABLE`, and the new
  container's anchor starts a VM of its own on retry. A `Stop` from such a
  supervisor is refused with `NotAnchorSupervisor` and kills nothing.
- A pod's VM can be driven only from that pod: VMs are keyed by the
  caller's pod UID, and the identity check ties the caller to the VM it
  names. An anchor of another VM has another identity and pod.

### Requirements on the supervisor process

- `baryon-anchor run` refuses to start unless it is PID 1, before it writes
  the readiness or termination files.
- Before any other thread starts, it makes itself non-dumpable
  (`PR_SET_DUMPABLE` 0). Other processes with the VM's identity can then
  neither trace it nor read its `/proc` entries (memory, descriptors,
  environment, root); those entries belong to root.
- The node runs Yama with `kernel.yama.ptrace_scope` 1 or higher, so only
  an ancestor may trace the anchor, also before it makes itself
  non-dumpable. Yama restricts tracing only: in that short window, other
  processes with the VM's identity can still read the anchor's
  `/proc/<pid>/environ`, `maps`, and descriptor list, which hold nothing
  that the pod spec does not. Hadron sets 1, and the Baryon-enabled image
  also sets it in `/usr/lib/sysctl.d/50-baryon.conf`; baryon-virtd warns at
  startup when the scope is lower or Yama is absent.
- Signal authorization: SIGTERM, the request to stop the VM, is blocked in
  every thread and read from a signalfd. The anchor acts on it only when
  the sender is uid 0 and the kernel recorded the sender, that is, for
  signals sent with kill(2) or tgkill(2) (`SI_USER`, `SI_TKILL`). kubelet
  stops containers through the container runtime, which runs as root. A
  SIGTERM from any other user, or with queued siginfo whose fields the
  sender chose, is logged and ignored. The kernel does not deliver SIGKILL
  or SIGSTOP to a PID namespace's init from inside that namespace.
- The anchor's inputs are its environment (`BARYON_VM_SPEC`, fixed when the
  container starts), baryon-virtd's status stream, and SIGTERM. It has no
  listening socket and reads no file that other processes of the pod can
  write, so no other process can make it call `Attach` or `Stop` on its
  behalf. It writes its readiness file without following symbolic links.
- The anchor does not fork or exec, and its descriptors are close-on-exec,
  so no other process inherits its connection to baryon-virtd. Taking its
  descriptors from outside would need the trace access that the two
  preceding rules deny.

### Why the other processes of the VM have no authority

- Cloud Hypervisor and the DHCP responder run in baryon-virtd's PID
  namespace. Every AnchorService call from them is refused, `Negotiate`
  included. Their seccomp layer also denies `ptrace`, `process_vm_readv`,
  `process_vm_writev`, and `pidfd_getfd`. A SIGTERM from them comes from the
  VM's uid, not uid 0, and is ignored.
- A process started with `kubectl exec` runs in the Anchor container's PID
  namespace with a PID other than 1, so `Attach` and `Stop` are refused;
  `Negotiate` stays open to it. It cannot trace the anchor, and it cannot
  make a PID namespace of its own in the initial user namespace without
  `CAP_SYS_ADMIN`, which Anchor containers do not have.

### Kubernetes controls that are part of this boundary

The supervisor check relies on the Anchor container having a PID namespace
of its own whose init is `baryon-anchor run` from the image that
baryon-controller set, and on the pod running only the containers that
baryon-controller created. The admission policies in `deploy/base/policy/`
enforce this for pods that request `virtualization.baryon.io/vm` or
`virtualization.baryon.io/import`.

Load-bearing: without these, a namespace user could run code of their
choice as an Anchor supervisor or as an approved importer.

- Only baryon-controller may create such pods (`baryon-vm-resource`), and it
  sets none of the fields refused below.
- Their container images cannot be changed (`baryon-pod-immutable`). An
  image is a mutable field of a pod, and kubelet restarts a container whose
  image changed regardless of the restart policy. The new image's
  entrypoint would be the init of the container, and so the Anchor
  supervisor, or an importer holding the approval (finding M7).
- Such a pod has exactly one container and no init containers
  (`baryon-vm-resource`), so a mutating webhook cannot add a container that
  is allocated the vm resource or runs next to the anchor.
- Every container drops all capabilities, adds none, and is neither
  privileged nor allowed to escalate privileges (`baryon-vm-resource`).
  Without `CAP_SYS_ADMIN`, no process in the container can create a PID
  namespace of its own in the initial user namespace and be its init.

Fail closed: without these, the real anchor refuses to run or is refused,
and nothing is taken over. The policies report the misconfiguration at
admission instead of as a VM that never starts.

- `shareProcessNamespace` is forbidden (`baryon-vm-resource`): the pause
  process would be PID 1.
- `hostPID` is forbidden (`baryon-vm-resource`): the container would share
  baryon-virtd's PID namespace.
- `hostUsers: false` is forbidden (`baryon-vm-resource`): the anchor would
  not be in the initial user namespace.
- `hostNetwork` is not refused by admission. baryon-controller does not set
  it, and an anchor in the host network namespace is refused because it
  shares baryon-virtd's network namespace.

Defense in depth:

- Ephemeral containers are forbidden (`baryon-no-ephemeral-containers`).
  An ephemeral container is never allocated the vm resource, so it has no
  anchor socket, and one that targets the Anchor container shares a PID
  namespace whose init is the non-dumpable anchor. The policy keeps an
  image of the requester's choice from running as the VM's identity with
  the pod's volumes, network namespace, and cgroup, and so with tools that
  the Anchor and import images do not contain.

The policies are ValidatingAdmissionPolicies with `failurePolicy: Fail`.
The API server checks in `hack/ci/apiserver-checks.sh` cover every rule
above; they pass in CI (the `crds` job, a single-node k0s API server in a
container, which is not a Baryon node). `tests/e2e/anchor-authority.sh`
also checks the ephemeral container and image change refusals on a
cluster of Baryon nodes.

Cluster administrators remain trusted. They can remove or change these
policies, create privileged or host-namespace pods, and act on nodes as
root; Baryon does not protect its lifecycle boundary from them. The
boundary holds against namespace users, including those with `pods/exec`,
`pods/ephemeralcontainers`, and `update` or `patch` on pods, and against a
compromised guest, VMM, or DHCP responder.

### Node requirements

The supervisor check holds only on nodes where:

- each container of a pod gets a PID namespace of its own (kubelet
  `PidNamespaceForPod` maps a pod without `hostPID` and
  `shareProcessNamespace` to the CRI namespace mode `CONTAINER`);
- the container's entrypoint runs as PID 1, with no init injected by the
  runtime;
- Anchor containers run in the initial user namespace;
- the container runtime stops containers by sending SIGTERM as uid 0 in
  the initial user namespace, with kill(2) or tgkill(2);
- Yama `ptrace_scope` is 1 or higher.

A node that breaks one of the first three fails closed, and the VM never
starts. When the anchor is not PID 1 (the first two), `baryon-anchor run`
refuses to run before it connects: the container restarts, its log shows
`refusing to run`, and there is no termination message. When the anchor is
PID 1 in another user namespace (the third), baryon-virtd refuses it with
`NotAnchorSupervisor`. A runtime that breaks the fourth makes every stop hard: the anchor
ignores the SIGTERM and kubelet kills the container after the grace
period. These requirements are listed in
[compatibility.md](../compatibility.md): Yama under
[Host requirements](../compatibility.md#host-requirements), the container
runtime behavior under
[Kubernetes requirements](../compatibility.md#kubernetes-requirements).

### User namespaces

Anchor containers in a user namespace (`hostUsers: false`, or a rootless
runtime) are Not supported: the supervisor check requires the initial user
namespace, and admission refuses `hostUsers: false` on Baryon pods. ADR
0011 left Kubernetes user namespaces open as later hardening. Adopting
them needs a new decision on how baryon-virtd recognizes the supervisor
and how the VMM keeps access to the VM's disks with its identity.

### Restart and recovery

- Registrations live only in baryon-virtd's memory. Authority is never
  restored from a file; it is established again by the live process.
- Anchor Pod replacement: the new pod has a new pod UID and so a VM entry
  of its own. Nothing of the old anchor's registration applies to it.
- Anchor container restart in the same pod: the old anchor is gone, and the
  container runtime stops the VMM with the old container's cgroup. If
  baryon-virtd still holds that VM, the new container's anchor triggers
  `AnchorContainerReplaced` as above.
- baryon-virtd restart: the VMM's runtime record gives its container
  cgroup (ADR 0005); the live anchor of that container reconnects and is
  registered again, through `Attach`, or through `Stop` when it is
  terminating. No other process is the init of that container. An anchor
  that receives SIGTERM while baryon-virtd is unreachable exits without a
  `Stop`; the container runtime then stops the VMM with the container,
  without a guest shutdown.
- A runtime record that names a live VMM but cannot be adopted (its spec
  or container cgroup cannot be read) makes baryon-virtd kill that VMM
  through its pidfd, so that a reattaching anchor never starts a second
  VMM next to it.
- Node reboot: `/run` is a tmpfs, so no records or VMs survive. Anchor Pods
  are created again and authority is established afresh.

### Compatibility

No protocol change. The protection is complete only where both sides are
new:

- A v0.1.0-rc1 anchor on a new host runs as PID 1 and passes the
  supervisor check, but does not make itself non-dumpable or filter
  SIGTERM senders. A cluster upgrade replaces it.
- A new anchor on a v0.1.0-rc1 host has those protections, but that
  baryon-virtd still authorizes lifecycle calls by the identity alone.
  This is the normal state of a cluster upgraded before its hosts (ADR
  0012). M1 is fixed per node, once the node runs the new host image.
- A v0.1.0-rc1 anchor exits with code 1 on an identity conflict, and the
  new controller acts only on the dedicated exit code. Such an anchor
  starts only if its pod was created before the controller upgrade; the
  VM then stays on the node until the pod is replaced.

## Consequences

- Lifecycle authority rests on kernel process properties and Kubernetes
  admission, not on a secret. There is no token for same-identity
  processes to read.
- `pods/exec` on an Anchor Pod no longer grants `Attach` or `Stop`. It is
  still the right to operate the VM: exec'd processes can open /dev/kvm
  within the pod's limits (ADR 0005), use up the pod's resources until the
  VM fails, and write the anchor's readiness file. The VM's `Ready`
  condition follows the Anchor container's readiness, so a forged file can
  show the VM Ready and add it to Service endpoints while its guest is
  down. They can also write the termination message; baryon-controller
  acts on an identity conflict only with the anchor's dedicated exit code
  as well, which they cannot set. All of this is limited to the user's own
  VM.
- The same PID 1 rule applies to baryon-importer: an importer started with
  `kubectl exec` in an import Pod refuses to run, and a file import holds
  an exclusive lock on the disk directory (a block import opens the device
  with `O_EXCL`). For the importer, unlike the anchor, the rule is the
  boundary (finding M5). It holds because the import image contains only
  `baryon-importer` and cannot be replaced (`baryon-pod-immutable`). The
  importer is non-dumpable as well.
- Residual: the VMM and the DHCP responder run in an ancestor PID namespace
  with the VM's uid. They can send the anchor any signal, including SIGKILL
  and SIGSTOP. A killed anchor ends its container and the VM; a stopped
  anchor cannot handle SIGTERM, so deleting the pod stops the VM hard after
  the termination grace period. They can also keep a SIGTERM of their own
  pending, so that the container runtime's SIGTERM is merged with it and
  the VM stops hard the same way; the anchor logs ignored SIGTERMs once per
  doubling of their count. Processes started with `kubectl exec` could do
  the same only through a program in the image, which has none. No other
  VM is affected. Removing this needs the VMM and the responder in a PID
  namespace of their own (Planned).
- The start-up protection depends on Yama on the node. A node without it
  is warned about, not refused.
- baryon-virtd reads the namespaces of a non-dumpable anchor through
  `/proc`, which needs `CAP_SYS_PTRACE`. The planned reduction of
  baryon-virtd's capabilities must keep it.
- Ephemeral containers and image changes are refused for cluster
  administrators too, so `kubectl debug` does not work on Baryon pods. Debug
  a VM from its node (`journalctl -u baryon-virtd`, the per-VM logs under
  `/var/log/baryon/vms`, see [troubleshooting.md](../troubleshooting.md)).
- The check depends on `SO_PEERPIDFD` (Linux 6.5 or later, already
  required) and on the `NSpid` field in pidfd fdinfo, present on the same
  kernels.
- The console session processes of ADR 0014 (`baryon-anchor console`)
  have no lifecycle authority. Any future process that needs it requires
  its own decision; no process inherits it from the VM's identity.

## Alternatives considered

- Keep the identity check alone (the previous model). Rejected: it cannot
  separate the anchor from exec'd processes, the VMM, or the responder.
- A token given to the anchor in its environment or a file. Rejected: every
  process with the VM's identity can read what the anchor can, before the
  anchor has a chance to hide it.
- Verifying through the kubelet pod resources API that the pod was
  allocated the vm resource. Rejected as the answer to M1: it proves the
  same as the admission policy and the device plugin mount, which pod a
  caller is in, not which process it is.
- A separate host identity for the anchor. Rejected: exec'd processes run
  as the container's user, which would be the anchor's identity, and the
  VMM must keep the VM's identity for its disks (ADR 0011).
- Passing a pre-connected descriptor to the anchor at container start.
  Rejected: baryon-virtd cannot inject descriptors into a container that
  the container runtime starts.

## Revisions

- 2026-10-27: after the architecture and security gate: the node
  requirements and the user namespace decision were recorded; the binding
  checks the caller's pidfd as well; `Stop` follows the takeover rule after
  a baryon-virtd restart; admission also fixes the container set and
  `hostUsers`; the readiness file and the termination message were
  recorded as status-integrity residuals, and the identity conflict needs
  the anchor's exit code; baryon-importer got the same PID 1 rule and an
  exclusive target; both upgrade directions were described.
- 2026-10-28: after the second security gate: container images of Baryon
  pods became immutable (finding M7), and every container must drop all
  capabilities and forbid privilege escalation; the admission controls were
  classified by what they protect; the fail-closed behavior on unsupported
  nodes was corrected (an anchor that is not PID 1 refuses to run); the
  importer became non-dumpable; ignored SIGTERMs are logged once per
  doubling; an unadoptable recovered VMM is killed.
- 2026-10-28: Accepted after the architecture and security reviews and
  the E2E anchor authority and regression suites on the k0s cluster
  (docs/testing.md).
- 2026-10-28: the reference to ADR 0014 names its console session
  processes; ADR 0014 chose `pods/exec`, which has no console bridge
  container.
