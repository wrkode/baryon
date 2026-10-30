# 0014: Guest configuration from a Secret and a serial console through the Anchor Pod

Status: Accepted
Date: 2026-10-27, revised 2026-10-28 and 2026-10-30

## Context

In v0.1 a user reaches a guest only over the pod network, and only with
credentials the guest image already contains:

- The v1alpha1 API has no field for guest configuration. baryon-virtd
  presents `ds=nocloud;i=<vm-uid>;h=<vm-name>` as the SMBIOS system serial
  number (`nocloud_serial` in `crates/virtd/src/vm.rs`), which gives
  cloud-init an instance ID and a hostname, and nothing else.
- The Debian cloud image used by the E2E suites has no password login and
  no SSH key, so a VM created from it cannot be logged in to at all.
- baryon-virtd keeps one connection to the guest's serial port for the VM's
  lifetime and writes the output to `/var/log/baryon/vms/` on the node
  (`crates/virtd/src/serial.rs`). Nobody can type into it, and reading it
  needs a root shell on the node.

This ADR decides how a user passes initialization data such as SSH keys to
a guest, and how a user gets an interactive serial console without a shell
on the node. Both stay within the existing boundaries: no Baryon component
listens on TCP, baryon-virtd is not a Kubernetes client, baryon-controller
is unprivileged and does not read Secrets, and the Anchor Pod gains no
privileges ([security.md](../security.md), ADR 0005, ADR 0009, ADR 0011).

ADR 0015 is authoritative for lifecycle-control security: possession of a
VM's runtime identity proves membership in its runtime context but grants
no lifecycle authority; only the Anchor supervisor, the init process of the
Anchor container, may call `Attach` and `Stop`. Since finding M2 of
2026-10-27 the Anchor image contains no import code; disk import runs only
in baryon-importer, in an import Pod. This ADR adds no lifecycle authority
and no import path, and every new process it introduces has less authority
than the Anchor supervisor.

### Two lifecycle contexts

Kairos configuration and guest configuration are separate and must not be
confused:

- Host side. A Hadron node is configured by Kairos: its cloud-config is
  processed by yip, the engine of kairos-agent, at install, boot, and
  upgrade (ADR 0003, [hadron.md](../hadron.md)). It configures the
  Baryon host. Baryon does not generate it, and nothing in this ADR
  changes it.
- Guest side. A VM's guest is configured by its own agent, inside the
  guest, from data that Baryon delivers: cloud-init in a typical cloud
  image, or yip in a Kairos-based guest image. Baryon only delivers the
  data; it does not run, parse, or validate the guest's agent.

cloud-init and yip both read NoCloud-compatible data, which is why one
delivery format serves both kinds of guest. yip also processes the host's
cloud-config, but sharing an engine or a format does not join the two
lifecycles: host cloud-config is the node operator's, guest configuration
is the VM owner's, and neither reaches the other.

### Upstream facts

Read from the code:

- cloud-init 26.2, `cloudinit/sources/DataSourceNoCloud.py`, `_get_data`:
  the NoCloud datasource reads the SMBIOS system serial number first, then
  the kernel command line, then seed directories, then every `vfat` or
  `iso9660` filesystem labelled `cidata` (or `CIDATA`), and merges the
  results with the earlier source taking precedence for meta-data keys. A
  seed filesystem holds `user-data`, `meta-data`, and optionally
  `network-config` and `vendor-data`. The instance ID and hostname that
  Baryon sets through SMBIOS therefore win over any `meta-data` in a seed.
- yip v1.25.1, the cloud-config engine of kairos-agent and kairos-init
  (their `go.mod`), `pkg/plugins/datasourceProviders/provider_cdrom.go` and
  `cdrom_shared.go`: the `cdrom` provider looks for a filesystem labelled
  `cidata` or `CIDATA`, mounts it as `iso9660` or else `vfat`, and reads
  `user-data` (or `config`). A Kairos-based guest, such as a Hadron image
  with kairos-init, consumes the same seed as a cloud-init guest. Inside
  such a guest, Hadron itself (v0.5.3 to v0.6.3) does not process
  cloud-config; the Kairos layer that kairos-init installs in the guest
  image does.
- Cloud Hypervisor v54.0, `serial_buffer/src/socket.rs` and
  `vmm/src/serial_manager.rs`: in socket mode (`--serial socket=<path>`)
  the serial port serves one client at a time; a new connection shuts down
  the current one and replaces it. Output produced while no client is
  connected is kept in a 1 MiB ring buffer and replayed to the next client.
  v53.0 has the same one-client behavior. v54.0 binds the socket under a
  lock on a `<socket>.lock` file next to it
  (`vmm/src/locked_unix_listener.rs`). Its serial input queue is unbounded
  (`devices/src/legacy/serial.rs`).
- Cloud Hypervisor v54.0 can also expose the virtio console on a socket
  (`--console socket=<path>`), with the same one-client rule (release
  notes, #8687). Firmware and boot loaders write to the serial port, not to
  the virtio console.
- Cloud Hypervisor opens disks through `/proc/self/fd/N` of an inherited
  descriptor (ADR 0006); a second disk with `readonly=on` and
  `image_type=raw` needs nothing else.
- containerd v2.3.5, `internal/cri/io/container_io.go`, `Attach`: every
  attach session's stdin is copied into the one stdin of the container,
  without coordination between sessions, and the container's output is
  written to every attached session. The same output also goes to the
  container log (`internal/cri/server/container_start.go`, the `log`
  output), which `kubectl logs` reads through `pods/log`. v2.1.4 and v2.2.0
  have the same code.
- kubectl v1.36.0, `pkg/cmd/attach/attach.go`: `kubectl attach` detaches
  on a client-side key sequence (`--detach-keys`, default `ctrl-p,ctrl-q`)
  and leaves the container running.
- Kubernetes v1.36.0, `pkg/features/kube_features.go`:
  `AuthorizePodWebsocketUpgradeCreatePermission` (beta, on by default
  since 1.35) authorizes the `create` verb for the WebSocket upgrade of
  `pods/exec`, `pods/attach`, and `pods/portforward`, so `get` alone does
  not open a stream. Before 1.35 (the supported minimum for the admission
  policies is 1.32), `get` on `pods/exec` or `pods/attach` also opens a
  stream over WebSocket, which current kubectl uses by default.
- Kubernetes v1.36.0, bootstrap RBAC policy
  (`plugin/pkg/auth/authorizer/rbac/bootstrappolicy/policy.go`): the
  default `view` role grants `pods/log`; the default `edit` role grants
  `pods/exec`, `pods/attach`, `get` on Secrets, and `create` on pods.
  Baryon's `baryon-vm-edit` and `baryon-vm-view` aggregate into `edit` and
  `view` ([api.md](../api.md#access-for-namespace-users)).
- Kubernetes v1.36.0, `staging/src/k8s.io/apiserver/pkg/cel/library/authz.go`:
  CEL in ValidatingAdmissionPolicy has an `authorizer` that checks the
  requester's access to a named resource, for example
  `authorizer.group('').resource('secrets').namespace(ns).name(n).check('get').allowed()`.
- Kubernetes v1.36.0, `pkg/volume/secret/secret.go`: a Secret volume with
  `optional: true` mounts empty when the Secret does not exist and skips a
  missing key. Creating a pod that mounts a Secret needs no access to the
  Secret.
- containerd v2.3.5, `internal/cri/server/podsandbox/sandbox_run_linux.go`
  and `container_create.go`: `/dev/shm` is a tmpfs of the pod sandbox,
  bind-mounted into every container of the pod, so it outlives a container
  restart; with `hostIPC` the containers use the node's `/dev/shm`.

## Decision

### Guest configuration

#### API

`spec.guestConfig` on the VirtualMachine expresses guest initialization
intent, not a mechanism:

```yaml
spec:
  guestConfig:
    userDataSecretRef:
      name: vm1-user-data   # data key "userdata"
```

- `guestConfig` is optional and has one field in its first version,
  `userDataSecretRef`, with one field, `name`: a Secret in the VM's
  namespace. There is no namespace field. Cross-namespace references are
  not supported.
- The Secret's `userdata` key holds the user data in whatever format the
  guest's agent reads: `#cloud-config` for cloud-init and yip, a script,
  or a MIME multipart document. Baryon does not parse it. An empty value
  is delivered as empty user data. A later optional `key` field,
  defaulting to `userdata`, is an additive change. Example:
  `kubectl create secret generic vm1-user-data --from-file=userdata=./user-data`.
- The field is named for its intent, not for one consumer: there is no
  `spec.cloudInit`. NoCloud is the delivery format of this version.
  Further intent (for example a network configuration, or a delivery
  format for guests that cannot read NoCloud) would be new fields under
  `guestConfig`, added in `v1alpha1` as additive changes
  ([api-stability.md](../api-stability.md)).
- There is no inline form. User data commonly carries credentials;
  Secrets have their own RBAC, while every reader of a VirtualMachine would
  see an inline value.
- The field is mutable. A change of the reference takes effect at the
  next start and is reported through `RestartRequired`. The Secret name
  joins the Anchor Pod's spec hash only when it is set, so the hashes of
  existing VMs do not change. A change of the Secret's content is not
  detected and not reported (see the semantics below).

#### Secret authorization

Referencing a Secret must give no more than reading it. Kubernetes lets a
pod creator mount any Secret of the namespace without access to it;
baryon-controller creates the Anchor Pod, so without a check, `create` on
VirtualMachines would be equivalent to `get` on every Secret of the
namespace: the VM owner controls the guest image, and the guest can read
its seed.

- A ValidatingAdmissionPolicy, `baryon-vm-guest-config`, part of the
  supported installation like the existing ones, with `failurePolicy:
  Fail` and a binding with `validationActions: [Deny]`: on a create of a
  VirtualMachine with `spec.guestConfig.userDataSecretRef`, and on an
  update that sets or changes that reference, the requester must be
  allowed to `get` the Secret named by
  `object.spec.guestConfig.userDataSecretRef.name` in `request.namespace`
  (the CEL `authorizer`); an authorization error counts as a denial. The
  check is Kubernetes authorization of the requester, including RBAC and
  any other configured authorizer. Baryon adds no access list of its own.
- The policy is load-bearing, and it fails open when it is absent: without
  it, a reference is read access to any Secret of the namespace. Two
  controls keep it present. The release order below never lets the CRD
  accept the field without the policy. And before it adds the Secret
  volume to an Anchor Pod, baryon-controller checks that the policy and a
  `Deny` binding exist (read-only `get` on those two named cluster-scoped
  objects); otherwise it does not start the VM and reports
  `GuestConfigPolicyMissing`.
- The implementing change adds CI API server checks for: a requester
  without `get` is denied on a create and on a reference change; a requester with `get` on only the
  named Secret (`resourceNames`) is allowed; removing the reference is
  allowed; an update that keeps the reference is allowed for a requester
  without `get`.
- Updates that do not change the reference, such as baryon-controller's
  own, are not checked. Removing the reference is not checked.
- The check is a snapshot of the requester when the reference is set.
  Like a Deployment whose pods mount a Secret, every later Anchor Pod of
  the VM receives the Secret's content at its start without a new check,
  including content written after the requester lost access. Revocation is
  removing the reference or the VM; rotating credentials in a referenced
  Secret delivers them to that guest. Anyone who can update a referenced
  Secret controls what the guest's agent runs, typically as root, at the
  VM's next start.
- baryon-controller needs no access to Secrets and never sees their
  content: it adds a Secret volume to the Anchor Pod and kubelet reads the
  Secret.
- A user bound to the default `edit` role can read every Secret of the
  namespace and create pods anyway; the policy matters for roles that
  grant VirtualMachines without Secrets, such as a binding of
  `baryon-vm-edit` alone.

#### Behavior by Secret state

| State | Behavior |
|---|---|
| The requester cannot `get` the Secret | The create or update is refused by `baryon-vm-guest-config`. The message names the Secret, never its content. |
| The Secret or its `userdata` key does not exist when the VM starts | The admission check needs authorization, not existence, so the VM is created. The anchor does not attach: it writes the termination message `UserDataUnavailable: <detail>` and exits with the generic failure code (not the identity conflict code, ADR 0012), so `Running` and `Ready` are False with that reason, with "(restart back-off)" while kubelet backs off (the ADR 0008 condition contract). `status.restartCount` counts these restarts, although no VMM ran. The VM starts once kubelet has mounted the Secret. A VM never boots without the user data it references. |
| The user data exceeds 256 KiB | Reason `UserDataTooLarge`, as above. The message names the limit, not the size. |
| The Secret is deleted while the VM runs | The running VM is not affected: its seed was built at start. The next start, on a new Anchor Pod, needs the Secret again and reports `UserDataUnavailable` until it exists. To run the VM without user data, remove the reference. |
| The Secret changes before the first boot | The content as kubelet has mounted it when the anchor starts is used. kubelet updates mounted Secrets with a delay, so a change made shortly before the start may be seen at the next start only. |
| The Secret changes after the guest was initialized | Not detected and not reported. The next start delivers the new content. What the guest does with it is the guest agent's decision (see instance identity). |

Termination messages, conditions, Events, logs, labels, and annotations
carry the reason and the limit at most, never the content or its size:
Pod status is readable without Secret rights. A controller of an earlier
release shows the new reasons generically, because it reports any
`Reason:` prefix of a termination message, so they need no skew rule.

#### NoCloud seed

- Generation: baryon-anchor, the Anchor supervisor (ADR 0015),
  unprivileged, inside the Anchor container, reads the mounted `userdata`
  file once per process, before its first `Attach`, and writes a VFAT
  image labelled `CIDATA` with two files, `user-data` and `meta-data`. The
  file names are fixed and never come from user data; the image has a
  fixed size that holds the 256 KiB limit. A small in-tree writer is
  preferred to a crate, and a test-only independent reader checks it.
  `baryon-anchor console` never builds a seed (see the console).
- Location: `/dev/shm/baryon-anchor/cidata.img`, in the anchor's state
  directory. `/dev/shm` belongs to the pod sandbox, so it outlives an
  Anchor container restart; its pages are charged to the memory cgroup of
  the container that writes them. At start, before its first `Attach`,
  the anchor opens the state directory with `O_NOFOLLOW`, removes any seed
  left by a previous container fd-relative (`unlinkat`), and creates a new
  one with `O_CREAT|O_EXCL|O_NOFOLLOW`, mode 0400. While the VM runs the
  anchor never rewrites it. Other processes with the VM's identity can
  change or replace the file; that gives them nothing the guest does not
  have, and baryon-virtd does not rely on its content.
- `baryon-vm-resource` refuses `hostIPC` on Baryon pods (added by the
  implementing change; the current policy does not check it), so the seed
  never lands in the node's `/dev/shm`.
- Secret volume: only the `userdata` key is projected
  (`items: [{key: userdata, path: userdata}]`), read-only, mode 0440,
  readable through the pod's `fsGroup` (the VM's gid, ADR 0011),
  `optional: true`. It is added to the Anchor Pod only, never to the
  import Pod: baryon-importer never sees user data (M2). The controller
  builds both pods from one function, so the implementing change tests
  that the import Pod has no Secret volume.
- `meta-data` holds `instance-id` (the VM UID) and `local-hostname` (the VM
  name), the same values as the SMBIOS serial.
- Delivery: the seed is a second disk in the anchor's `VmSpec`, with a
  dedicated role and `read_only`, resolved by baryon-virtd in the Anchor
  mount namespace like the root disk (ADR 0006) and passed to Cloud
  Hypervisor as an inherited descriptor with `image_type=raw`,
  `readonly=on`, and disk serial `baryon-cidata`. baryon-virtd opens it
  in the Anchor mount namespace with
  `RESOLVE_IN_ROOT|RESOLVE_NO_SYMLINKS|RESOLVE_NO_MAGICLINKS` and
  `O_RDONLY|O_NOFOLLOW|O_NONBLOCK`, only at the fixed seed path, and
  accepts it only when `fstat` shows a regular file of exactly the fixed
  size, owned by the VM's uid, with one link, on the filesystem of the
  container's `/dev/shm`.
- baryon-virtd never reads, parses, stores, or logs user data. It opens
  the seed file to pass the descriptor; its runtime record and its
  identical-spec reattach comparison contain the path, not the content.
- The seed disk is not a user disk: "one disk per VM" in
  [compatibility.md](../compatibility.md#not-supported-in-v01) still holds
  for disks the user provides.
- Network configuration is not part of the seed: the guest network is
  configured by DHCP from the pod address (ADR 0007). `network-config` and
  `vendor-data` are not supported.

Who can read the user data on the node and in the guest:

- On the node: kubelet's Secret volume (tmpfs) and the seed in the pod's
  `/dev/shm`. Root on the node can read both; the cluster administrator
  is trusted (ADR 0015). Other processes with the VM's identity can read
  them: the anchor, processes run with `pods/exec` in the Anchor
  container, and, outside their Landlock and mount confinement, the VMM
  and the DHCP responder. Each of these is limited to its own VM by the
  per-VM identity (ADR 0011), and none gains anything the guest does not
  have, because the guest reads the seed by design.
- In the guest: any guest process that can read the seed device, for as
  long as the VM runs. User data is never protected from the host or from
  the guest's own administrator.
- Not: the import Pod, baryon-controller, baryon-virtd's records and logs,
  the VirtualMachine, the Pod object, status, or the console log (except
  what the guest itself prints, see the console).

#### Lifetime, replacement, and recovery

- Cleanup: the Secret volume and the seed disappear with the Anchor Pod.
  No user data is stored outside the pod; there is no node-side cleanup.
  The seed survives an Anchor container restart in the pod's `/dev/shm`
  and is replaced by the next anchor before its first `Attach`.
- The seed stays attached while the VMM runs, so a guest reboot inside the
  VMM sees the same content. It does not persist beyond the Anchor Pod.
- Anchor restart and Anchor Pod replacement: a new Anchor container is a
  new supervisor, and baryon-virtd stops the VM of the old one (ADR 0015).
  The new anchor replaces the seed with one built from the Secret as
  mounted in its pod, and the VM boots with it.
- baryon-virtd restart: the VMM keeps its open seed descriptor; the
  anchor reattaches with the same `VmSpec`, which names the seed path, not
  its content. Nothing is rebuilt.
- Node reboot: the Anchor Pod is replaced, as above.

#### Instance identity

The instance ID is the VM UID; the hostname is the VM name. They are stable
across starts, Anchor Pod replacements, node reboots, and Secret changes,
and change only when the VirtualMachine is deleted and created again.
cloud-init therefore runs its per-instance modules (users, SSH keys) once
per VirtualMachine and its per-boot modules on every boot. A Kairos guest
applies its boot stages on every boot (Kairos documentation; not verified
in yip's code), with the content delivered at that start. Changing the
user data of an initialized VM is supported only as "delivered at the next
start"; whether the guest applies it is up to the guest's agent. Dynamic
rotation of user data is not part of this design.

#### Version skew of guest configuration

- Runtime protocol: the minor version that ships guest configuration adds
  the seed disk role and the feature `disk.seed` (ADR 0012), named for
  what the host implements, like `disk.block`. A VM with `guestConfig`
  requires the feature.
- Placement: ADR 0012 allows a required feature beyond the 1.0 set only
  with feature-aware placement, so this part ships with the mechanism ADR
  0012 plans: a placement-only extended resource for the feature, served
  by baryon-node-agent only on hosts that implement it, and requested by
  the Anchor Pod and the import Pod of every VM with `guestConfig` (the
  import Pod decides the node of a node-local volume). A VM with
  `guestConfig` is never placed on an earlier host and stays
  `Unschedulable` while no such host exists; an anchor that still meets
  one is refused with `UnsupportedFeature` before the guest boots. The
  resource is named under `virtualization.baryon.io/` and served per node
  from the features the host reports, not from the node agent's own
  version. Like the import resource it grants nothing, and
  `baryon-vm-resource` and `baryon-pod-immutable` cover it with the vm and
  import resources: only baryon-controller may create pods that request
  it, so other pods cannot use up its capacity.
- Host rollback: rolling a host back through Kairos A/B to an image
  without the feature while a VM with `guestConfig` runs there is not
  supported. The Kairos runbook drains the node first, which replaces the
  Anchor Pod elsewhere; without a drain, a restarted anchor on that host
  is refused with `UnsupportedFeature` and stays not ready until its
  Anchor Pod is replaced
  ([kairos-integration.md](../kairos-integration.md#lifecycle-contract)).
- Controller, admission policy, and CRD: the documented upgrade order
  stays CRDs first ([installation.md](../installation.md#10-upgrading)).
  At least one cluster release before the one that adds the field ships
  `baryon-vm-guest-config` in a refusing form: it denies every create or
  update that sets `spec.guestConfig`, with expressions guarded by
  `has()` so that it admits every VirtualMachine while the schema has no
  such field. The release that adds the field replaces it, under the same
  name, with the authorizing form, in `baryon.yaml`. Between step 1 (the
  CRD accepts the field) and step 3 (the new controller and the
  authorizing policy), every VM that sets the field is refused, so an
  earlier controller never sees one and no reference goes unauthorized.
  Upgrading across both releases in one step is not supported.
- The refusing form ships in v0.1.0-rc3 and therefore in v0.1.0
  (`deploy/base/policy/guest-config.yaml`), so every v0.1.x installation
  already has it when a later release adds the field. It is harmless
  before the field exists: the CRD of v0.1 has no `spec.guestConfig`, the
  API server removes or rejects the unknown field before admission, and
  the only expression, `!has(object.spec) || !has(object.spec.guestConfig)`,
  is true for every VirtualMachine of v0.1. The CI API server checks
  confirm this with the policy installed (create, spec update, and
  metadata update allowed; the API server reports no type checking
  warning) and confirm the refusal with a CRD that declares the field
  (create and update that set it denied). Upgrade gate for the release
  that adds the field: its upgrade notes require a cluster release that
  carries the refusing form (v0.1.0-rc3 or later) to be installed first;
  from an earlier one, upgrade to such a release before. That release
  also checks the condition itself, because a cluster can lack the policy
  despite its version (the policy deleted, or the cluster release rolled
  back below v0.1.0-rc3): its upgrade notes start with a preflight that
  the refusing policy object and its `Deny` binding exist, and its
  controller refuses to start a VM with the field without the policy
  (`GuestConfigPolicyMissing`, above).
- A fresh installation of the release that adds the field has no refusing
  form to rely on: its `baryon.yaml` creates the CRD, the controller, and
  the authorizing policy together, in an order kustomize decides. The
  controller's check that the policy and its binding exist is what keeps
  a VM with the field from starting before the policy is in place.
- During the rollout of the release that adds the field, the earlier
  controller can still be running for a short time. It does not know the
  field and starts such a VM without its user data and without its
  placement resource. The release that adds the field therefore keeps the
  refusing form until its own controller runs, or its upgrade notes
  require the earlier controller to be scaled to zero first; the choice is
  part of that release's implementation.
- Rolling the cluster release back below the release that adds the field
  while a VirtualMachine sets it is not supported: the earlier controller
  would start the VM without its user data and without its placement
  resource.
- Clients that request strict field validation, kubectl's default, get an
  error for the field on an earlier CRD; clients that do not get a warning
  and a pruned spec.

### Serial console

#### Transport comparison

Both options use the Kubernetes API server and kubelet streaming path, so
neither needs a Baryon listener, TCP port, or credentials. baryon-virtd
stays the only client of the Cloud Hypervisor serial socket in both, because
a second client would replace it and end the console log capture.

Option A, `pods/exec`: `baryonctl console` (or `kubectl exec`) runs
`/baryon-anchor console` in the Anchor container. Each session is its own
process, which calls a console-only RPC on the anchor socket.

Option B, `pods/attach`: a second container in the Anchor Pod, the console
bridge, whose main process holds the console connection; users attach to
its stdin and stdout.

| Aspect | A: `pods/exec` | B: `pods/attach` |
|---|---|---|
| RBAC | `create` on `pods/exec`. Runs any command in any container of the pod; cannot be narrowed to a command. | `create` on `pods/attach`. Reaches only the stdio of a container's main process. Narrower. |
| Same for the default roles | `edit` grants both `pods/exec` and `pods/attach`, so for `edit` users the two are equal. | as A |
| Commands available | Only what the Anchor image contains: `/baryon-anchor` and its subcommands. The image is controller-chosen and immutable (`baryon-pod-immutable`). This is defense in depth, not the boundary. | None. |
| Interaction with ADR 0015 | An exec'd process is never the Anchor supervisor: it cannot `Attach` or `Stop`. Unchanged by this ADR. | A second container is the init of its own PID namespace, which ADR 0015's admission rules exclude today (exactly one container). Allowing it needs a new rule set, such as "exactly one container requests the vm resource", and a new review of the supervisor boundary. The bridge gets no anchor socket, so it reaches the console only through a relay that the anchor serves. |
| Interaction with M2 | No import code in the image; an exec'd process cannot import. | No import code; the bridge cannot import. |
| Number of writers | One process per session, so baryon-virtd can tell sessions apart and enforce one interactive writer, takeover, and read-only sessions. | containerd copies the stdin of every attach session into the one container stdin and sends output to all. Every attacher is a writer; one writer, takeover, and read-only viewers cannot be enforced. |
| Confidentiality of output | Output goes only to the session's own stream. | Output is the container's stdout, which containerd writes to the container log, readable with `pods/log`, which the default `view` role grants. Guest output, including user data that cloud-init prints and anything typed at a shell, would reach every `view` user of the namespace. |
| TTY | The exec stream allocates a TTY in the container; `baryon-anchor console` puts it in raw mode. | The container is created with `stdin` and `tty`; kubectl puts the local terminal in raw mode. |
| Kubernetes audit | The exec request records the user, the pod, the container, and the command. | The attach request records the user, the pod, and the container. |
| Reconnect | A new exec is a new session; it receives the recent output replay. | A new attach joins the shared stream. |
| Resource isolation from the VMM | Session processes and the seed are charged to the Anchor container's cgroup, which holds the VMM (ADR 0005); the anchor's overhead budget must cover the seed and the bounded number of session processes. | The bridge has its own requests and limits. |
| Pod shape and resources | Unchanged: one container. | One more container in every Anchor Pod, with its own requests and limits in the Guaranteed envelope, a change of every Anchor Pod's spec hash, and a container restart policy for the bridge. |
| Operational simplicity | `kubectl exec` works without baryonctl. | `kubectl attach` works without baryonctl. |

Decision: Option A, `pods/exec`. The comparison is decided by what the two
transports can enforce, not by the image being minimal:

- Option B cannot keep one interactive writer, a takeover rule, or
  read-only sessions, because containerd merges all attach sessions into
  one stdin and one output.
- Option B publishes the guest's console output in the container log,
  which widens read access from console users to every holder of
  `pods/log`.
- Option B weakens the one-container rule that ADR 0015 relies on and
  changes the shape and resources of every Anchor Pod.

Its narrower verb does not outweigh these, and for the default `edit` role
it is not narrower at all. The cost of Option A is recorded instead of
hidden: `pods/exec` on an Anchor Pod is the right to operate the VM's
guest. Besides the console it allows reading the VM's user data (the Secret
volume and the seed, through whatever the image can run), opening
`/dev/kvm` within the pod's limits, using up the pod's resources, writing
the anchor's readiness file (ADR 0015), and opening connections and
`Console` streams on the node's shared anchor socket within the per-pod
limits below. It does not allow stopping the VM or attaching as its
anchor (ADR 0015), or importing or overwriting
a disk (M2); ephemeral containers and image changes on Anchor Pods are
refused (`baryon-no-ephemeral-containers`, `baryon-pod-immutable`). A
console grant that is narrower than `pods/exec` needs a dedicated
subresource, which the alternatives list as later work.

#### Console authority

| Process | Authority |
|---|---|
| baryon-anchor (`run`, the Anchor supervisor) | Lifecycle supervision: `Attach` and `Stop` for its VM (ADR 0015). Builds the seed. Never serves a console. |
| `baryon-anchor console` (one process per session, run by `pods/exec`) | The serial stream of its pod's VM only, through the `Console` RPC. No `Attach`, `Stop`, import, or reconfiguration. It does not make itself the supervisor (the PID 1 rule and the non-dumpable step belong to `run`), builds no seed, and never writes the readiness file or the termination message. |
| baryon-importer | Import only (M2). No console, no user data. |
| Cloud Hypervisor | VM execution. No `Console` calls. |
| DHCP responder | DHCP only. No `Console` calls. |
| baryon-virtd | The privileged host runtime: the only serial socket client, serving sessions from its connection. |

The console client is a subcommand of the existing `baryon-anchor` binary
rather than a second binary. Its authority comes from which process it is,
checked by baryon-virtd per call, not from which binary it is, so a second
binary in the same image would add no separation.

- `Console` is a new AnchorService RPC: a bidirectional stream over the
  anchor socket. baryon-virtd identifies the caller by its peer
  credentials, like every AnchorService call. It accepts `Console` only
  when the caller's container cgroup equals the container cgroup recorded
  for the VM (the one that holds its VMM), its uid and gid are the VM's
  host identity (ADR 0011), it is outside baryon-virtd's PID namespace
  (which excludes the VMM and the DHCP responder of the VM), and its PID
  namespace is the registered anchor's. `Console` is refused while the VM
  has no live registered anchor. A session reaches only the VM of the
  caller's own pod.
- A `Console` session is not an anchor session. It does not count towards
  the VM's sessions, is never the VM's registered anchor, and gives no
  `Attach` or `Stop`. The binding and takeover rules of ADR 0015 are
  unchanged.
- The import Pod has no anchor socket (it is not allocated the vm
  resource), so exec into it gives no console.

#### Session semantics

- Writers: at most one interactive session per VM.
- Read-only sessions: not part of the first implementation; a later part
  may add them, at most 8 per VM. A read-only session is a convenience
  against typing by mistake, not a permission: anyone who can open one
  can open an interactive one.
- Takeover: a second interactive request fails unless it asks to take
  over (`--force`); the earlier session then ends with a message that
  names the taking-over session ID.
- Disconnect: when the client goes away, baryon-virtd ends the session.
  Input not yet written is discarded; the guest receives no signal and
  keeps running; the serial log capture continues.
- Reconnect: a new session. It first receives the last 16 KiB of console
  output, starting at a line boundary, so that a prompt is visible. The
  replay shows a new session what an earlier one displayed, so sessions
  are not a confidentiality boundary from each other.
- Anchor Pod replacement and Anchor container restart: the VM is stopped
  (ADR 0015), so its sessions end with a message that the VM stopped. A
  new session goes to the new Anchor Pod named in `status.anchorPod`.
- Node reboot: as an Anchor Pod replacement.
- baryon-virtd restart: the sessions end, because their streams are on
  baryon-virtd's socket. The VM keeps running; the restarted baryon-virtd
  reconnects to the serial socket and receives what Cloud Hypervisor
  buffered (up to 1 MiB). A client reconnects with a new session.
- Terminal mode: `baryon-anchor console` puts its TTY in raw mode, with
  no local echo and no signal generation; the remote side, kubectl or
  baryonctl, puts the user's terminal in raw mode.
- Detach: `Ctrl-]` followed by `.`, recognized in user input only, ends
  the session. `Ctrl-]` followed by any other byte sends both bytes.
- `Ctrl-C` and every other key go to the guest; they do not end the
  session or stop the VM.
- Boot evidence: login prompt detection and the console log are unchanged,
  except that console text is no longer taken as boot evidence once
  interactive input was sent before the first evidence, so typed text
  cannot fake a boot.

#### Bounds

Enforced by baryon-virtd, so that a session cannot affect the guest, the
log capture, or other VMs:

- explicit gRPC decode limits per RPC; console input frames of at most
  4 KiB;
- a bounded number of input bytes in flight to the serial socket, because
  Cloud Hypervisor's serial input queue is unbounded; reading from the
  client stops while a write to the socket is pending;
- every session has an output queue of at most 256 KiB; a session that
  cannot keep up is closed. Neither the log capture nor the guest waits
  for a session;
- at most 64 KiB of console input in flight per VM;
- per Anchor Pod UID, a bounded number of concurrent anchor connections
  and of `Console` streams, refused attempts included, and an HTTP/2
  `max_concurrent_streams` per connection; identifying a peer runs off the
  accept loop with a timeout, so one pod cannot delay connections from
  others; a `Console` stream that sends no frame for a bounded idle time
  is closed.

#### Terminal output

Guest output is untrusted terminal data:

- It is never interpreted as a command by any Baryon component. Login
  prompt detection matches text; nothing else reads the content.
- `baryon-anchor console` filters it by default, including the replay and
  when the session has no TTY (`kubectl exec -i`). It passes valid UTF-8
  text without C1 code points (U+0080 to U+009F), the C0 controls CR, LF,
  BS, HT, and BEL, and CSI sequences without intermediate bytes and
  without a private prefix whose final byte is one of the cursor
  movement, erase, insert and delete, scroll, scroll region, and SGR
  finals, each at most 64 bytes with bounded parameters. The only SM and
  RM modes passed are cursor visibility (`?25`), the alternate screen
  (`?47`, `?1047`, `?1049`), and autowrap (`?7`). Everything else is
  removed, and parsing resynchronizes after split and unterminated
  sequences: every other escape sequence (OSC, DCS, APC, PM, SOS,
  `ESC Z`, `ESC c`, character set designations), every byte from 0x80 to
  0x9F outside valid UTF-8, the other C0 controls (including ENQ, SO, and
  SI), and every request that makes the terminal answer into its input
  (device status, device attributes, DECRQM, XTVERSION, window
  operations, and mouse and focus reporting modes).
- `--raw` turns the filter off, for users who accept the guest's output
  as it is; the guest can then query the user's terminal and read its
  answers.
- The filter reduces exposure; it does not make guest output safe for
  every terminal. Terminal emulators differ in what they accept, and the
  filter cannot guarantee that no allowed sequence misbehaves in a
  particular one.
- Terminal restoration: at the start and at the end of a session,
  including an error or the VM stopping, `baryon-anchor console` writes a
  fixed reset (SGR reset, reset scroll region, cursor visible, autowrap
  on, leave the alternate screen) and restores its TTY mode. The client
  restores the user's terminal mode on exit, including on an error or a
  signal. A client that is killed cannot
  restore its terminal; that is the client terminal's behavior.

#### Entry points

- `kubectl exec`, where `<pod>` is the VM's `status.anchorPod` (the name
  is not derived from the VM name):

  ```bash
  pod="$(kubectl -n <namespace> get virtualmachine <vm> -o jsonpath='{.status.anchorPod}')"
  kubectl -n <namespace> exec -it "$pod" -c anchor -- /baryon-anchor console
  ```

  The plain `kubectl exec` path does not check that the pod is the VM's
  Anchor Pod: a user who runs it against a pod of another user's choice
  types into that pod.
- `baryonctl console <vm> [-n <namespace>] [--force] [--raw]`, a later
  part: a Rust command line client in `crates/baryonctl`. It finds the
  VM's running Anchor Pod from the VM status, checks that the pod has a
  controller owner reference to the VirtualMachine's UID and that its
  `anchor` container requests `virtualization.baryon.io/vm`, which only
  baryon-controller can create (`baryon-vm-resource`; an owner reference
  alone can be set by any pod creator), and that the pod's UID is
  unchanged between the lookup and the exec. It then calls `pods/exec`
  through the Kubernetes API, handles the local terminal, and runs
  `/baryon-anchor console`. Installed as `kubectl-baryon`, it also runs as
  `kubectl baryon console <vm>`. It uses the user's kubeconfig and needs
  `create` on `pods/exec` and `get` on VirtualMachines and pods.
- The command line of the console client (the path `/baryon-anchor`, the
  container `anchor`, the subcommand `console`, and the flags it
  implements) is an interface between releases: baryonctl of release N
  runs it in Anchor images of release N-1. It is kept for at least the
  support window of ADR 0012, listed in
  [api-stability.md](../api-stability.md) by the change that implements
  it, and only extended. A flag
  enters the contract in the release that implements it; an Anchor image
  that does not know a flag refuses it with a message and never ignores
  it.

#### Audit

baryon-virtd logs each session's start and end with a session ID, the
mode, the peer pid and container cgroup, the session it took over, and the
byte counts in each direction; never the content. Console input is not
logged; guest output, including echoed input, goes to the console log as
before. baryon-virtd cannot name the Kubernetes user; the API server audit
log of the exec request does, and the installation docs give an audit rule
for `pods/exec` on Anchor Pods.

#### Version skew of the console

- The minor version of the runtime protocol that ships the console adds
  the `Console` RPC. It is not a feature in the sense of ADR 0012, because
  no VM needs it to run, and it needs no placement: a host without it
  answers UNIMPLEMENTED, which `baryon-anchor console` reports as a host
  without console support.
- Anchor Pods of a release without the subcommand cannot serve a console
  until the VM restarts on the new Anchor image; baryonctl reports that,
  using the VM's `RestartRequired` condition (`AnchorImageChanged`) where
  it is set.

### Status and implementation order

Status of every part: Designed. Nothing in this ADR is implemented in
v0.1. The parts are planned after v0.1 stabilization, in this order:
guest configuration (the refusing form of `baryon-vm-guest-config` is
Implemented since v0.1.0-rc3, so the release that adds the field replaces
it with the authorizing form); the console through `kubectl exec`, with
one interactive writer and takeover; then baryonctl and, if wanted,
read-only sessions. Each part is its own change, with unit tests, an E2E
suite on the k0s cluster, the security review, and the dependency review
for new crates (VFAT writer, kube-rs WebSocket support, terminal
handling). Before guest configuration is Implemented, an experiment on the k0s cluster checks
that Cloud Hypervisor under Baryon's Landlock rules opens the seed through
`/proc/self/fd/N` at boot and again after a guest reboot.

The change that implements each part also revises the accepted ADRs it
affects: ADR 0004 (the Secret volume on the Anchor Pod; `pods/exec` as a
user path), ADR 0005 (the overhead budget for the seed and console
sessions), ADR 0006 (the seed disk role), ADR 0008
(`spec.guestConfig`, the new reasons), ADR 0009 (the `Console` RPC and its
caller rule), ADR 0012 (the first required feature and its placement
resource), ADR 0013 (entry criterion 1 gains the seed disk and console
sessions; a destination rebuilds the seed from the Secret), and ADR 0015
(the feature resource and `hostIPC` in `baryon-vm-resource`). Until each
implementing change lands, the accepted ADRs describe the implemented
system; where they differ, this ADR describes the design that change
brings in.

## Consequences

- Stock cloud images become usable: an SSH key in the user data is enough
  to log in over the pod network or the console.
- The VirtualMachine API gains one optional field; the change is additive
  in `v1alpha1`. The installation gains one admission policy.
- The runtime protocol gains a minor version per part. Hosts of earlier
  minor versions keep running VMs without guest configuration; VMs with it
  are placed only on hosts that serve the feature's placement resource.
  Consoles are available on hosts and Anchor Pods of the release that
  ships them.
- Every VM with guest configuration has a second, read-only virtio-blk
  device in the guest, readable by any guest process with access to it for
  as long as the VM runs.
- With interactive sessions the console log on the node also holds what
  the guest echoes and prints during a session, including commands typed
  at a shell and anything cloud-init prints from user data, for the same
  14 days as today.
- `pods/exec` on Anchor Pods becomes a documented user path, with the
  rights listed above. The installation docs gain an example Role and the
  audit rule.
- With baryonctl, the release files gain a `baryonctl` binary for Linux
  x86_64, next to the baryon-virtd binary ([release.md](../release.md)).
- What guest configuration and a console expose is recorded in
  [security.md](../security.md).

## Alternatives considered

- `pods/attach` to a console bridge container: compared above. Not used,
  because containerd shares one stdin and one output between all attach
  sessions and writes the output to the container log.
- An aggregated API server with a `virtualmachines/console` subresource.
  It would give a dedicated RBAC verb on the VirtualMachine rather than
  `pods/exec` on the pod, and is the way to a console grant narrower than
  `pods/exec`. It needs a Baryon component that listens on TCP, an
  APIService with serving certificates, and a streaming path from that
  server to the node. Later work; `baryon-anchor console` could be its
  backend.
- Connecting clients directly to the Cloud Hypervisor serial socket, for
  example by mounting the VMM directory into the Anchor Pod. The socket
  serves one client, so each session would end the log capture, and the
  VMM's directory would be exposed to the pod.
- The virtio console on a socket instead of the serial port: firmware and
  boot loader output does not appear on it, and the guest needs
  `console=hvc0`. Not used.
- `spec.cloudInit`: names one consumer, while the same seed serves yip
  guests, and leaves no place for other guest initialization intent.
- User data inline in the VirtualMachine: visible to every reader of the
  VM. Not offered.
- Cross-namespace Secret references: they need a grant model across
  namespaces. Not supported.
- baryon-controller reading the Secret and passing the content in the
  `BARYON_VM_SPEC` environment variable: the content would be in the Pod
  object, and the controller would need access to Secrets.
- A NoCloud `seedfrom` URL (`s=` in the SMBIOS serial) served by a
  metadata endpoint: needs an HTTP server reachable from the guest, one
  more guest-facing service.
- An ISO9660 seed: cloud-init and yip read both, but the names
  `user-data` and `meta-data` are not valid ISO9660 names without the
  Joliet or Rock Ridge extensions, which makes the writer larger than a
  VFAT one.
- baryon-virtd building the seed from user data sent in the attach
  request: baryon-virtd would hold credentials in a root process, and its
  runtime record and identical-spec reattach check would either store the
  user data on the node or refuse every reattach after a baryon-virtd
  restart or a Secret edit.
- A seed file in `/run/baryon`: user data would be stored on the node by
  a root process and need cleanup on every exit path.
- Rebuilding the seed when the Secret changes, or detaching it after the
  first boot: the first is dynamic rotation, which a boot-time
  initialization model does not need; the second would break yip guests,
  which read the seed on every boot.

## Revisions

- 2026-10-27: Proposed.
- 2026-10-27, after the architecture and security reviews, still
  Proposed: the seed image is built by baryon-anchor and passed as a disk,
  so user data no longer travels in `VmSpec` or reaches baryon-virtd; an
  admission policy requires `get` on the referenced Secret; the first
  required feature ships with its placement resource (ADR 0012); the
  console is not a protocol feature; caller restrictions, bounds,
  terminal output filtering, audit, the command line contract, and the
  affected ADRs were added.
- 2026-10-27, still Proposed: lifecycle authority moved to ADR 0015, and
  the rights of `pods/exec` and `pods/ephemeralcontainers` on an Anchor
  Pod were updated for the M1 and M2 fixes.
- 2026-10-28, still Proposed: rewritten on the authority model of ADR 0015
  (Accepted). The API is `spec.guestConfig.userDataSecretRef`, named for
  intent; the host-side and guest-side configuration contexts are
  distinguished; the Secret states, seed lifetime, replacement, recovery,
  and instance identity are specified; `pods/exec` and `pods/attach` are
  compared, with `pods/exec` chosen; the console authority, session
  semantics, terminal output handling, and the version skew of the
  controller and CRD were added. The affected ADRs are revised by the
  implementing changes, not at acceptance.
- 2026-10-28, after the architecture and security gates of the rewrite:
  the guest configuration policy is classified as load-bearing, ships in
  a refusing form one release before the field, and is checked by
  baryon-controller, so the documented CRDs-first order stays; controller
  rollback below that release is not supported; the feature is named
  `disk.seed` and its placement resource is restricted by admission; the
  seed location, its replacement after a container restart, the open
  flags in baryon-virtd, and the projection of only the `userdata` key
  are specified; `hostIPC` is refused; the reasons follow the ADR 0008
  condition contract; the Secret check is a snapshot; per-pod limits on
  the anchor socket and numeric session bounds were added; the terminal
  filter allow-list, the `Console` caller rule, the baryonctl pod check,
  and the console command line contract were made precise; resource
  isolation was added to the transport comparison; read-only sessions and
  baryonctl are later parts.
- 2026-10-28: Accepted as a design after the architecture, security, and
  technical writing reviews. Nothing in it is implemented; each part
  needs its own implementation, tests, and reviews.
- 2026-10-30, for v0.1.0-rc3, still Accepted: the refusing form of
  `baryon-vm-guest-config` is implemented and ships in v0.1.0-rc3 and
  v0.1.0, ahead of the field, with CI API server checks that it admits
  every VirtualMachine of v0.1 and refuses the field once a CRD declares
  it. The upgrade gate for the release that adds the field is a cluster
  release that carries the refusing form. Nothing else in this ADR is
  implemented; `spec.guestConfig` does not exist in v0.1.
