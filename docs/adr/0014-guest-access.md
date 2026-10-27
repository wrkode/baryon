# 0014: Guest user data from a Secret and a serial console through the Anchor Pod

Status: Proposed
Date: 2026-10-27

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

This ADR decides how a user passes configuration such as SSH keys to a
guest, and how a user gets an interactive serial console without a shell
on the node. Both have to stay within the existing boundaries: no Baryon
component listens on TCP, baryon-virtd is not a Kubernetes client,
baryon-controller is unprivileged and does not read Secrets, and the
Anchor Pod gains no privileges ([security.md](../security.md), ADR 0005,
ADR 0009, ADR 0011).

### Upstream facts

Read from the code on 2026-10-27:

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
  with kairos-init, consumes the same seed as a cloud-init guest.
  Hadron itself (v0.5.3 to v0.6.3) does not process cloud-config; the
  Kairos layer installed by kairos-init does.
- Cloud Hypervisor v54.0, `serial_buffer/src/socket.rs` and
  `vmm/src/serial_manager.rs`: in socket mode (`--serial socket=<path>`)
  the serial port serves one client at a time; a new connection shuts down
  the current one and replaces it. Output produced while no client is
  connected is kept in a 1 MiB ring buffer and replayed to the next client.
  v53.0 has the same one-client behavior (`serial_manager.rs`). v54.0
  binds the socket under a lock on a `<socket>.lock` file next to it
  (`vmm/src/locked_unix_listener.rs`).
- Cloud Hypervisor v54.0 can also expose the virtio console on a socket
  (`--console socket=<path>`), with the same one-client rule (release
  notes, #8687). Firmware and boot loaders write to the serial port, not to
  the virtio console.
- Cloud Hypervisor opens disks through `/proc/self/fd/N` of an inherited
  descriptor (ADR 0006); a second disk with `readonly=on` and
  `image_type=raw` needs nothing else.

## Decision

### Guest user data

- API: an optional `spec.userData` on the VirtualMachine with a reference
  to a Secret in the VM's namespace:

  ```yaml
  spec:
    userData:
      secretRef:
        name: vm1-user-data   # data key "userdata"
  ```

  The Secret's `userdata` key holds the user data in whatever format the
  guest's agent reads (`#cloud-config` for cloud-init and yip, a script,
  or a MIME multipart document). Baryon does not parse it. There is no
  inline form: user data commonly carries credentials, and Secrets have
  their own RBAC, while every reader of a VirtualMachine would see an
  inline value. The field is mutable; a change of the reference takes
  effect at the next start and is reported through `RestartRequired`
  (the Secret name joins the Anchor Pod's spec hash only when it is set,
  so the hashes of existing VMs do not change). A change of the Secret's
  content is not detected or reported.
- Authorization: referencing a Secret must not give more than reading it.
  A ValidatingAdmissionPolicy, `baryon-vm-user-data`, part of the supported
  installation like the existing ones, refuses a create, or an update that changes
  `spec.userData`, unless the requester may `get` the referenced Secret
  (the `authorizer` variable of ValidatingAdmissionPolicy, available in
  `admissionregistration.k8s.io/v1` on the supported Kubernetes versions).
  Updates that do not change the field, such as the controller's own, are
  not checked. Without the policy, `create` on VirtualMachines would be
  equivalent to `get` on every Secret of the namespace.
- Who reads the Secret: kubelet. baryon-controller adds a Secret volume
  for the reference to the Anchor Pod only, never to the import Pod:
  read-only, mode 0440, readable through the pod's `fsGroup` (the VM's
  gid, ADR 0011), `optional: true`. The controller needs no access to
  Secrets and never sees their content.
- Seed image: baryon-anchor, unprivileged and inside the Anchor container,
  reads the file once per process and writes a small VFAT image labelled
  `CIDATA` with two files, `user-data` and `meta-data`, to its state
  directory in the container's `/dev/shm` (charged to the pod's memory).
  `meta-data` holds `instance-id` (the VM UID) and `local-hostname` (the
  VM name), the same values as the SMBIOS serial. Limit: 256 KiB of user
  data. If the VM references user data and the file is missing (the Secret
  or key does not exist) or exceeds the limit, the anchor does not attach
  and exits with a termination message, which baryon-controller reports as
  `Ready` False with reason `UserDataUnavailable` or `UserDataTooLarge`.
  The message never quotes the content. The anchor restarts with its
  container's back-off, so a VM whose Secret is created later starts once
  kubelet has mounted it. A VM never boots without the user data it asks
  for.
- Delivery to the guest: the seed image is a second disk in the anchor's
  `VmSpec`, with a dedicated role and `read_only`, resolved by
  baryon-virtd in the Anchor mount namespace like the root disk (ADR
  0006) and passed to Cloud Hypervisor as an inherited descriptor with
  `image_type=raw`, `readonly=on`, and disk serial `baryon-cidata`.
  baryon-virtd accepts it only at the fixed seed path, only as a regular
  file, and only read-only. baryon-virtd never reads, parses, stores, or
  logs user data: its records and its reattach comparison contain the
  path, not the content. The anchor writes the image to a new file and
  renames it into place, never in place, so a restarted anchor that
  reattaches to a running VM does not change the image that VM's VMM has
  open. The image disappears with the pod.
- This seed disk is not a user disk: "one disk per VM" in
  [compatibility.md](../compatibility.md#not-supported-in-v01) still holds
  for disks the user provides.
- Lifetime and guest agents: the content is delivered at every start of the
  VM, from the Secret as mounted at that time. The instance ID stays the
  VM UID, so cloud-init runs its per-instance modules (users, SSH keys)
  once per VirtualMachine, not again when the user data changes; its
  per-boot modules see the new content. yip runs its stages on every boot
  and applies the current content each time.
- Network configuration is not part of the seed: the guest network is
  configured by DHCP from the pod address (ADR 0007). `network-config` and
  `vendor-data` are not supported.
- Runtime protocol: the minor version that ships this part adds the seed
  disk role and the feature `guest.user-data` (ADR 0012). A VM with user
  data requires the feature. ADR 0012 allows a required feature beyond
  the 1.0 set only with feature-aware placement, so this part ships with
  the mechanism ADR 0012 plans: a placement-only extended resource for the
  feature, served by baryon-node-agent only on hosts that implement it,
  and requested by the Anchor Pod and the import Pod of every VM with user
  data (the import Pod decides the node of a node-local volume). A VM with
  user data is then never placed on an earlier host, and an anchor that
  still meets one is refused with `UnsupportedFeature` instead of booting
  the guest without its data.

### Serial console

- baryon-virtd stays the only client of the Cloud Hypervisor serial
  socket. A second client would replace it and end the console log
  capture, so console sessions are served from baryon-virtd's connection:
  it copies the guest output to every session and passes the input of one
  session to the guest.
- A new AnchorService RPC, `Console`, is a bidirectional stream over the
  anchor socket. The caller is identified by its peer credentials, like
  every AnchorService call, so a session can only reach the VM of the
  caller's own pod. In addition, as for `Attach`, the caller's uid and gid
  must be the VM's host identity (ADR 0011): a console is read and write
  access to the guest. The first message selects the mode: interactive or
  read-only, and whether to take over an existing interactive session.
- Sessions: at most one interactive session per VM; a second interactive
  request fails unless it asks to take over, which ends the earlier
  session with a message. At most 8 read-only sessions per VM. A new
  session first receives the last 16 KiB of console output, starting at a
  line boundary, so that a prompt is visible. Login prompt detection and
  the console log are unchanged, except that console text is no longer
  taken as boot evidence once interactive input was sent before the first
  evidence.
- Bounds, enforced by baryon-virtd: explicit gRPC decode limits per RPC
  (console input frames of at most 4 KiB); a bounded number of input bytes
  in flight to the serial socket, because Cloud Hypervisor's serial input
  queue is unbounded (`devices/src/legacy/serial.rs`); reading from the
  client stops while a write to the socket is pending; every session has
  an output queue bounded in bytes, and a session that cannot keep up is
  closed. Neither the log capture nor the guest waits for a session.
- Caller restrictions: a `Console` session is not an anchor session. It
  does not count towards the VM's sessions and never becomes the VM's
  anchor. `Console` is accepted from processes in the Anchor container
  (exec), never from peers in baryon-virtd's own PID namespace, which are
  the VMM and the DHCP responder of the VM. This ADR also closes the
  existing gap that any process of the pod with the VM's identity can call
  `Attach` and `Stop` today (finding M1 of 2026-10-27 in
  [security.md](../security.md#review-findings)):
  AnchorService calls from baryon-virtd's PID namespace are refused, and
  `Attach` and `Stop` are accepted only from the Anchor container's init
  process; a live anchor is never replaced by a newer caller.
- Entry point: a new `baryon-anchor console` subcommand. It runs in the
  Anchor container through the Kubernetes `pods/exec` subresource, puts its
  terminal in raw mode, and bridges it to the `Console` stream. The
  escape sequence `Ctrl-]` followed by `.`, recognized in user input only,
  ends the session; everything else, `Ctrl-C` included, goes to the
  guest. Guest output is untrusted terminal data: by default the command
  passes printable text, C0 controls, and CSI sequences (cursor movement
  and SGR) and removes clipboard (OSC 52), hyperlink (OSC 8), DCS, APC, PM,
  and SOS sequences; `--raw` turns the filter off. It resets the terminal
  state when the session starts and ends, prints the session ID, and never
  writes the container's termination message, which sets the VM's `Ready`
  reason. It works with plain `kubectl exec`, where `<pod>` is the VM's
  `status.anchorPod` (the name is not derived from the VM name):

  ```bash
  pod="$(kubectl -n <namespace> get virtualmachine <vm> -o jsonpath='{.status.anchorPod}')"
  kubectl -n <namespace> exec -it "$pod" -c anchor -- /baryon-anchor console
  ```
- The command line of `baryon-anchor console` (the subcommand and its
  flags `--read-only`, `--force`, and `--raw`) is an interface between
  releases: baryonctl of one release runs it in Anchor images of others.
  It is stable within a major version, listed in
  [api-stability.md](../api-stability.md), and only extended. Anchor Pods
  of a release without the subcommand cannot serve a console until the VM
  restarts on the new anchor image; baryonctl reports that, using the
  VM's `RestartRequired` condition (`AnchorImageChanged`) where it is
  set.
- Access control is Kubernetes RBAC: `create` on `pods/exec` for the
  Anchor Pod, which RBAC can narrow by `resourceNames` to named pods but
  not by label. The streaming path is the API server and kubelet, which
  audit the exec request, its command included; Baryon adds no listener,
  no TCP port, and no credentials. `pods/exec` on an Anchor Pod is the
  right to operate the VM, not to view it: besides the console it allows
  stopping the VM, opening `/dev/kvm` within the pod's limits, and, until
  the importer requires the controller's approval at its fixed path, in
  block mode overwriting the running VM's root disk with `baryon-anchor
  import` (finding M2 of 2026-10-27 in
  [security.md](../security.md#review-findings), an existing gap that this
  ADR does not create). `pods/ephemeralcontainers` on an Anchor Pod
  gives the same with an image of the user's choice.
- Audit: baryon-virtd logs each session's start and end with a session
  ID, the mode, the peer pid and container cgroup, the session it took
  over, and the byte counts in each direction; never the content. Console
  input is not logged; guest output, including echoed input, goes to the
  console log as before. The installation docs give an API server audit
  rule for `pods/exec` on Anchor Pods, which attributes a session to a
  Kubernetes user.
- Runtime protocol: the minor version that ships this part adds the
  `Console` RPC. It is not a feature in the sense of ADR 0012, because no
  VM needs it to run; a host without it answers UNIMPLEMENTED, which
  `baryon-anchor console` reports as a host image without console
  support.
- `baryonctl`: a new Rust command line client in `crates/baryonctl`, whose
  first command is `baryonctl console <vm> [-n <namespace>] [--read-only]
  [--force]`. It finds the VM's running Anchor Pod from the VM status,
  checks that the pod has a controller owner reference to the
  VirtualMachine's UID (the status names the pod but carries no pod UID),
  so a pod another user created under the expected name is not used, and
  then calls `pods/exec` through the Kubernetes API with kube-rs, handles
  the local terminal, and runs `/baryon-anchor console`. Installed as
  `kubectl-baryon`, it also runs as `kubectl baryon console <vm>`. It uses
  the user's kubeconfig and needs no permissions beyond `create` on
  `pods/exec` and `get` on VirtualMachines and pods.

Status of both parts: Designed. Each part is implemented in its own
change, with unit tests, an E2E suite on the k0s cluster, the
security review, and the dependency review for new crates (VFAT writer,
kube-rs WebSocket support, terminal handling). The VFAT writer runs in
baryon-anchor, unprivileged, writes fixed file names that never come from
user data, and produces an image of fixed size; a small in-tree writer is
preferred to a crate, and a test-only independent reader checks it.
Before the user data part is Implemented, an experiment on the k0s
cluster checks that Cloud Hypervisor v54.0 under Baryon's Landlock rules
opens the seed through `/proc/self/fd/N` at boot and again after a guest
reboot.

When this ADR is accepted, these accepted ADRs get revisions: ADR 0004
(Secret volume on the Anchor Pod; `pods/exec` as a user path), ADR 0006
(the seed disk role), ADR 0008 (`spec.userData`, the new reasons), ADR
0009 (the `Console` RPC and the peer restrictions), ADR 0012 (the first
required feature and its placement resource), and ADR 0013 (entry
criterion 1 gains the seed disk and console sessions; a destination
rebuilds the seed from the Secret).

## Consequences

- Stock cloud images become usable: an SSH key in the user data is enough
  to log in over the pod network or the console.
- The VirtualMachine API gains one optional field; the change is additive
  in `v1alpha1` (api-stability.md). The installation gains one admission
  policy.
- The runtime protocol gains a minor version per part. Hosts of earlier
  minor versions keep running VMs without user data; VMs with user data
  are placed only on hosts that serve the feature's placement resource.
  Consoles are available on hosts and Anchor Pods of the release that
  ships them.
- Every VM with user data has a second, read-only virtio-blk device in
  the guest. Any guest process that can read it can read the user data for
  as long as the VM runs; user data is never protected from the host.
- Data on the node: kubelet's Secret volume (tmpfs) and the anchor's seed
  image in the pod's `/dev/shm`, both removed with the pod. baryon-virtd
  stores no user data. Termination messages and status carry the reason
  and the size only, because Pod status is readable without Secret
  rights.
- With interactive sessions the console log on the node also holds what
  the guest echoes and prints during a session, including commands typed
  at a shell and anything cloud-init prints from user data, for the same
  14 days as today. The replay shows a new session what an earlier one
  displayed, so read-only sessions are not a confidentiality boundary.
- `pods/exec` on Anchor Pods becomes a documented user path. The
  installation docs gain an example Role and the audit rule.
- The release files gain a `baryonctl` binary for Linux x86_64, next to
  the baryon-virtd binary ([release.md](../release.md)).
- What user data and a console expose is recorded in
  [security.md](../security.md).

## Alternatives considered

- An aggregated API server with a `virtualmachines/console` subresource.
  It would give a dedicated RBAC verb on the VirtualMachine rather than
  `pods/exec` on the pod. It needs a Baryon component that listens on TCP,
  an APIService with serving certificates, and a streaming path from that
  server to the node. Not now; `baryon-anchor console` could later be its
  backend.
- Connecting clients directly to the Cloud Hypervisor serial socket, for
  example by mounting the VMM directory into the Anchor Pod. The socket
  serves one client, so each session would end the log capture, and the
  VMM's directory would be exposed to the pod.
- The virtio console on a socket (Cloud Hypervisor v54) instead of the
  serial port: firmware and boot loader output does not appear on it, and
  the guest needs `console=hvc0`. Not used.
- User data inline in the VirtualMachine: visible to every reader of the
  VM. Not offered.
- baryon-controller reading the Secret and passing the content in the
  `BARYON_VM_SPEC` environment variable: the content would be in the Pod
  object, and the controller would need access to Secrets.
- A NoCloud `seedfrom` URL (`s=` in the SMBIOS serial) served by a
  metadata endpoint: needs an HTTP server reachable from the guest. The
  DHCP responder could answer on the TAP device, but it would need a TCP
  implementation, and the endpoint would be one more guest-facing service.
- An ISO9660 seed: cloud-init and yip read both, but the names
  `user-data` and `meta-data` are not valid ISO9660 names without the
  Joliet or Rock Ridge extensions, which makes the writer larger than a
  VFAT one.
- baryon-virtd building the seed in a sealed memfd from user data sent in
  the attach request (the first draft of this ADR). baryon-virtd would
  hold credentials in a root process, and its runtime record and its
  identical-spec reattach check would either store the user data on the
  node or refuse every reattach after a baryon-virtd restart or a Secret
  edit. The anchor-built seed keeps user data out of baryon-virtd and runs
  the VFAT writer unprivileged.
- A seed file in `/run/baryon`: user data would be stored on the node by
  a root process and need cleanup on every exit path.
- `kubectl attach` to an Anchor container started with a TTY: the
  container's own process would have to be the console bridge, sessions
  could not be told apart, and the anchor's lifetime would be tied to the
  terminal.

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
