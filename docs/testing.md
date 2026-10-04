# Testing

This document describes Baryon's test categories, what each one validates,
how to run them, what an environment needs, and which evidence exists so
far. Other documents state a status label and link to
[Results](#results).

## Test categories

| Category | Location | Validates | Environment | Runs in CI |
|---|---|---|---|---|
| Unit and integration tests | `cargo test --workspace --locked`; integration tests in `crates/host/tests/` | logic of every crate against fixtures, fake sockets, and synthetic sysfs and procfs trees | any Linux host with the pinned toolchain | yes (job `test`) |
| API server checks | `hack/ci/k0s.sh`, `hack/ci/apiserver-checks.sh` | CRD schema, CEL rules, admission policies | a single-node k0s API server in a container; no KVM, not a Baryon host | yes (job `crds`) |
| E2E lifecycle | `tests/e2e/lifecycle.sh` | one VM through create, stop, start, restart, guest reset, and delete, with host-side cleanup checks | a cluster of Baryon nodes | no |
| E2E deletion | `tests/e2e/delete.sh` | deletion in every phase before Ready, and the import Pod's resources and isolation | a cluster of Baryon nodes | no |
| E2E multi-node | `tests/e2e/multinode.sh` | one VM per node, controller restart, baryon-virtd restart and outage on one node, VMM crash, Anchor Pod replacement behind a Service, cordon, deleting one VM, each without effect on the VMs of other nodes | at least three Baryon nodes | no |
| E2E storage | `tests/e2e/storage.sh` | one StorageClass and volume mode: import, boot, one VM per claim (`ClaimInUse`), Anchor Pod replacement and stop and start without a second import, a move to another node, deletion with claim, volume, and host cleanup | a cluster of Baryon nodes with the StorageClass | no |
| E2E version skew | `tests/e2e/skew.sh` | one VM per node under whatever mix of cluster release and host image versions runs, reattach across versions after a baryon-virtd restart | a cluster of Baryon nodes during a rolling upgrade | no |
| E2E import integrity | `tests/e2e/import-integrity.sh` | a block-mode imported disk: no stale backend data after the import, a namespace user's pods named like the Anchor Pod or the import Pod are not trusted, a namespace user cannot change the import record, the VM status, the disk source, or an import approval, exactly one import across start, Anchor Pod replacement, and a baryon-controller restart, `ImportStateUnknown` when an administrator removes the record | a cluster of Baryon nodes with a block-mode StorageClass, `NODE_SHELL`, and a namespace that admits root pods | no |
| E2E placement | `tests/e2e/placement.sh` | `spec.placement` translated into the scheduling fields of the VM's pods: nodeSelector, node affinity, topology spread, tolerations, no eligible node, cordon, a pod refused by the API server (`PodRejected`), baryon-virtd stopped on the only eligible node, `PlacementChanged`; VM labels copied to and synced on the Anchor Pod | at least three Baryon nodes and `NODE_SHELL`; the script labels, taints, and cordons nodes and stops baryon-virtd on one node | no |
| E2E hostile disk | `tests/e2e/hostile-disk.sh` | a claim whose `disk.img` is a block device node made by a pod is refused (`StorageFailed`) and no VMM starts | a cluster of Baryon nodes and a namespace that admits a root pod with the runtime's default capabilities | no |
| Failure-path procedures | manual, listed below | crashes and restarts of baryon-virtd, the anchor, the VMM, and the DHCP responder; deletion while baryon-virtd is down | a cluster of Baryon nodes and a root shell on the node | no |
| Kairos lifecycle procedures | manual, listed below | node reboot, Kairos A/B upgrade, fresh provisioning, log retention | a cluster of Baryon nodes and a root shell on the node | no |
| Identity conflict suite | Planned | the `IdentityConflict` replacement path across nodes | a multi-node cluster of Baryon nodes | no |
| CSI failure-path suite | Planned | teardown ordering under SIGKILL, forced delete, and baryon-virtd down with a CSI driver that stages and attaches | a cluster with such a driver | no |

The E2E tests and the manual procedures need hardware virtualization, which
hosted CI runners do not provide. Never report E2E results from an
environment without KVM: there is no emulation fallback.

## Unit and integration tests

```bash
cargo test --workspace --locked
```

| Crate | Coverage |
|---|---|
| `baryon-host` | discovery against synthetic sysfs, procfs, and /etc trees (Intel host with IOMMU and VFIO, aarch64 host without KVM, malformed input), CPU list parsing; a real-host smoke test runs only with `BARYON_TEST_REAL_HOST=1` |
| `baryon-cloud-hypervisor` | command line construction (including `--landlock-rules` for writable directories) and option injection rejection, API client against a fake Unix socket server (status codes, timeouts), event monitor decoding with split writes and truncation, version parsing |
| `baryon-virtd` | pod cgroup parsing (systemd, cgroupfs, nested roots) and systemd scope detection, peer pidfd, the anchor identity check (`IdentityMismatch`) and the refusal of a second VM with the same identity on a node (`IdentityConflict`), disk resolution confinement (absolute symlink and `..` escapes), the accepted disk paths and kinds (`UnknownPath`, `KindMismatch`), device nodes outside the container's `/dev` mount, one VM per block device on a node (`DiskInUse`), `/proc/self/fd` reopen, access to the VMM-owned directory (bounded tail reads; symlinks, hard links, other file types, and a foreign owner refused; socket connection through a checked descriptor), DHCP responses (routed, bridged, and off-link gateway route tables, NAK, other clients), IPv4 and UDP checksums, DHCP responder protocol (lease configuration round trip, only events matching the lease forwarded, overlong lines rejected), gratuitous ARP frame layout, tc filter encoding, cgroup limits, serial prompt detection and capture, record round trip, spec validation |
| `baryon-runtime-api` | per-VM identity (deterministic, distinct for different VMs, unprivileged and inside the reserved range, verification), guest MAC derivation, anchor spec round trip, protocol versions and features (a peer without a version is 1.0 with the 1.0 features, same major interoperates, another major and missing features refused) |
| `baryon-anchor` | sparse writes, block device import (zero runs, tail zeroing, data past the first 1 MiB, unaligned device end, the size limit holds before writing), the block import contract on a device full of stale data with each of the three zeroing methods (every byte the image's or zero), a small image on a large device, a failed import leaves both the first and the last 1 MiB zero, devices smaller than the two protected regions, waiting for an approval that names the claim UID (empty and foreign values do not proceed), length checks (`Content-Length` mismatch refused, no length without a digest refused), redirect resolution (absolute, root-relative, path-relative; HTTPS to HTTP refused), import against a local HTTP server with checksum and marker handling, protocol negotiation results, termination messages |
| `baryon-api` | CRD generation with CEL rules and list-map markers, defaults, conditions, quantities |
| `baryon-node-agent` | device plugin slots and allocation, the import resource granting nothing, VirtualizationNode status translation including `ProtocolIncompatible` |
| `baryon-controller` | Anchor Pod shape (Guaranteed, restricted, no host access, no init container), import Pod shape (VM's CPU and memory, the import slot and no vm slot, no `vm-name` label, URL digest annotation, the approval file and no approval until the volume is checked, an approved import Pod carries its approval without changing the Anchor Pod hash), block mode, the import decision (`decide`: fresh volume, record for this claim, claim metadata not deciding, records for another claim or source, unreadable record, static, pre-bound, foreign, and cloned volumes, migration of earlier records and fail-closed `ImportStateUnknown`, the record's JSON field names), mutual binding of claim and volume before a record is written, completion only from the import Pod's spec (resource, claim, URL, claim UID, exit code; annotations ignored), an import Pod for another claim UID is stale, readiness of earlier releases only for the current claim, `status.rootDiskImport` kept for the same claim and reset for a recreated one, an exited importer alone does not make the disk ready, required node affinity away from `identityConflictNodes`, detection of an identity conflict from the anchor termination message, ownership of an existing Anchor Pod (controller owner reference with the VM UID), UID preconditions on deletes, an init container of a pod with the Anchor Pod's name is no import evidence, pods that use a claim, status derivation for ready, failed, importing, pending import, unschedulable import Pod, stopped, and blocked VMs, a running pod taking precedence over a blocked class, the two RestartRequired reasons, the watch trigger backoff, placement copied to both pods with Baryon's identity-conflict requirement ANDed into every user node selector term (user terms stay ORed), the reserved label keys and the VM labels on both pods, the `vm-label-keys` annotation and reserved keys dropped from it when it is read, the import Pod's readiness probe, a placement hash separate from the spec hash, scheduler messages cut to 1024 bytes |

Kubernetes API behavior is covered by the API server checks and by the E2E
tests. An envtest-based tier (kube-rs `envtest` crate) would need a Go
toolchain at build time and is not set up.

## API server checks

The CI job `crds` runs:

```bash
hack/verify-crds.sh            # deploy/base/crds matches the Rust types
hack/ci/k0s.sh start           # single-node k0s (v1.36.4+k0s.1) in a container
hack/ci/apiserver-checks.sh    # schema, CEL rules, admission policies
hack/ci/k0s.sh stop
```

`hack/ci/apiserver-checks.sh` applies the CRDs, the example classes, the
namespace, and each admission policy file, then uses server-side dry runs
to confirm that:

- the example VirtualMachine is accepted
- a VirtualMachine with two disk sources is rejected
- a plain-HTTP source without `sha256` is rejected
- a VirtualMachine name with a dot is rejected (DNS-1123 label rule)
- a change of `rootDisk` is rejected and a change of `runPolicy` is
  accepted
- `spec.placement`: a toleration of every taint, of a cordon
  (`node.kubernetes.io/unschedulable`), of control-plane nodes, and of an
  out-of-service node (`node.kubernetes.io/out-of-service`), a system
  PriorityClass, required pod anti-affinity with a
  `namespaceSelector`, and preferred pod affinity with `namespaces` are
  rejected; a placement with a node selector, a toleration, a spread
  constraint, node affinity, same-namespace pod anti-affinity, and a
  PriorityClass is accepted
- `baryon-vm-resource` denies a pod that requests `virtualization.baryon.io/vm`,
  a pod that requests `virtualization.baryon.io/import`, and a pod whose
  init container requests `virtualization.baryon.io/vm`
- a plain pod is admitted

It then applies the controller RBAC and, impersonating baryon-controller's
service account against a PersistentVolume bound to a claim, a Baryon pod,
and an unmanaged pod, confirms that:

- `baryon-controller-writes` denies a PersistentVolume spec change, another
  PersistentVolume annotation, an import record for another claim UID,
  rewriting the record, removing it, adding a `node.kubernetes.io/` label,
  changing `app.kubernetes.io/managed-by`, or adding
  `virtualization.baryon.io/vm-name` on a Baryon pod, relabeling a pod that
  is not labeled `app.kubernetes.io/managed-by=baryon`, and annotating such
  a pod
- the controller may add the import record for the bound claim, set the
  import approval on a Baryon pod, and set VM labels (`app`,
  `app.kubernetes.io/name`) and the `virtualization.baryon.io/vm-label-keys`
  annotation on a Baryon pod
- `baryon-import-approval` denies another user changing, removing, or
  setting the approval, and admits another user changing other annotations
  of a pod that has one

The script works against any cluster: set `KUBECTL` to the kubectl command
to use, for example `KUBECTL=kubectl hack/ci/apiserver-checks.sh`. It
applies objects, so use a disposable cluster.

The k0s container has no KVM and runs no baryon-virtd. It validates API
server behavior only; it is not a Baryon host and its results are not
virtualization evidence.

## E2E tests

### Scripts

| Script | Purpose |
|---|---|
| `tests/e2e/lib.sh` | shared environment and helpers (`apply_vm`, `wait_for`, `host`, `node_of_pod`, `host_wait`, `host_leftovers`, `summary`); sourced by the scripts below |
| `tests/e2e/lifecycle.sh` | one VM through create, stop, start, restart, guest reset, and delete |
| `tests/e2e/delete.sh` | deletion in every phase before Ready |
| `tests/e2e/multinode.sh` | multi-node cases (see the header of the script); needs `NODE_SHELL` and at least three Baryon nodes |
| `tests/e2e/storage.sh` | root disk storage for one StorageClass (`STORAGE_CLASS`) and volume mode (`VOLUME_MODE`) |
| `tests/e2e/skew.sh` | one VM per node under the current version mix; needs `NODE_SHELL` |
| `tests/e2e/hostile-disk.sh` | a hostile `disk.img` is refused |
| `tests/e2e/import-integrity.sh` | integrity of a block-mode import against stale data and namespace users; needs `NODE_SHELL`, `STORAGE_CLASS` (block volumes), and a namespace that admits root pods |
| `tests/e2e/placement.sh` | VM placement and VM labels; needs `NODE_SHELL` and at least three Baryon nodes |
| `tests/e2e/repeat.sh <count> <script> [args...]` | runs a script repeatedly, stops at the first failing run, and writes each run's output to `_output/e2e/<script>-<n>.log` in the current directory |

The scripts are multi-node aware: each host check runs on the node that the
VM's Anchor Pod was bound to, taken from the pod's `spec.nodeName`. A stop
and start or a restart may place the VM on another node; the checks for
each Anchor Pod run on that pod's node.

`lifecycle.sh` checks:

| Phase | Checks |
|---|---|
| create | VM becomes Ready; guest reachable over the pod network; guest reachable through a ClusterIP Service by DNS name |
| stop | Anchor Pod removed; Ready reason is `Stopped`; host runtime state removed; root disk claim kept |
| start | VM Ready again; start created a new Anchor Pod |
| restart | restart replaced the Anchor Pod; VM Ready; old VMM state removed; guest reachable with the new pod IP; Service follows the VM to its new Anchor Pod |
| reset | VMM accepted the reset; guest booted again after the reset; guest answers ping after the reset |
| delete | VirtualMachine removed; Anchor Pod gone; root disk claim garbage collected; host runtime state removed; for each of the three Anchor Pods of the run, on its node: no runtime directory, no VMM process, no DHCP responder process (`baryon-dhcp`), and no mounts (pod volumes, the VMM tmpfs) left in `/proc/mounts` |

That is 33 checks with `NODE_SHELL` set, 15 without it (host checks and the
reset are skipped with a `SKIP` line), and 28 with `NODE_SHELL` set and
`SKIP_NETWORK=1`.

The reachability checks read the guest's SSH banner on TCP port 22 from a
probe pod in the test namespace: at the VM's reported address, and through a
ClusterIP Service that selects `virtualization.baryon.io/vm-name=<vm>`, at
`<service>.<namespace>.svc.cluster.local`. These are the only guest network
paths the E2E tests cover; NetworkPolicy and guest-initiated egress are not
tested.

The reset step sends `PUT /api/v1/vm.reboot` to the VM's Cloud Hypervisor API
socket with `curl` on the node, so the node needs `curl`. It counts kernel
boots (`Linux version`) in the console log to detect the second boot. After
the reset it checks ping, not SSH: a hard reset shortly after the first boot
left the Debian guest without SSH host keys, because cloud-init's first-boot
writes were not yet on disk.

`delete.sh` cases, each checking that the VirtualMachine, its Anchor Pod and
import Pod, and the root disk claim are gone:

| Case | How the phase is reached | Host checks |
|---|---|---|
| import | delete while the import Pod runs; before that, check that the import Pod requests `virtualization.baryon.io/import` and not `virtualization.baryon.io/vm`, that no Anchor Pod exists yet, and (with `NODE_SHELL`) that the importer process sees neither /dev/kvm nor the anchor socket | runtime directory, VMM, DHCP responder, mounts |
| booting | delete after the VMM started, while the Ready reason is `Booting` | runtime directory, VMM, DHCP responder, mounts |
| pending | delete while the Anchor Pod is unschedulable (a VirtualMachineClass with 1Ti memory) | none (no pod on a node) |
| stopped | delete a VM with `runPolicy: Stopped` | none (no pod) |

The number of checks depends on `NODE_SHELL` and on the script version; the
counts of recorded runs are in [Results](#results). `delete.sh` needs the
VirtualMachineClass `general-small` from `examples/classes.yaml`.

`storage.sh` checks disk persistence on the guest console: the Debian cloud
image generates its SSH host keys on the first boot only, so a later boot
that generates them again ran from a fresh disk. It moves the VM to another
node (cordon, restart) only when the volume is not bound to a node, and
starts a second VirtualMachine on the same claim to check `ClaimInUse`.

`storage.sh` checks that the completed import is recorded on the bound
PersistentVolume for the claim's UID and the URL digest.

`import-integrity.sh` runs as a namespace user: a service account bound to
the Kubernetes `edit` role through the aggregated `baryon-vm-edit` role.
In order, it:

1. creates a stopped block-mode VirtualMachine as that user and fills the
   claim's whole device with a marker from a root pod (the stale data);
2. creates, as that user, a pod named `<vm>-anchor` owned by the VM with an
   `import` init container, and a pod named `<vm>-import` owned by the VM,
   then starts the VM: the VM reports `AnchorPodConflict` and StorageReady
   is not True; after the fake Anchor Pod is deleted it reports
   `ImportPodConflict`, no Anchor Pod is created, and no import record is
   written;
3. deletes the fake import Pod, waits for Ready, checks the record on the
   PersistentVolume for this claim UID and a first boot from the imported
   image (the guest generates SSH host keys);
4. stops the VM and scans the whole device: no marker is left;
5. as the user, removes and changes claim annotations, and checks that the
   user cannot change the PersistentVolume, set an import approval on a
   pod, write the VM status, or change the disk source;
6. starts the VM, replaces the Anchor Pod, and restarts baryon-controller
   and then the VM; after each, exactly one `Importing` event, no import
   Pod, and a boot from the existing disk (no new SSH host keys);
7. checks `status.rootDiskImport` for this claim, stops the VM, removes
   the record as an administrator, and starts the VM: it is refused with
   `ImportStateUnknown` and not imported; restoring the record lets it
   boot the existing disk;
8. deletes the VM and checks the node for leftovers.

`placement.sh` steers placement only with node labels and taints it sets
itself (administrator policy) and runtime health only by stopping
baryon-virtd on one node; it removes the labels, taints, and cordons and
starts baryon-virtd again on exit. It uses the first three nodes labeled
`virtualization.baryon.io/host=true`, here called a, b, and c:

| Case | What it does | Checks |
|---|---|---|
| selector | a VM with `nodeSelector` for a label set on a | Ready; on a |
| labels | on the VM of the selector case: change and add labels on the VirtualMachine, set a label on the Anchor Pod as another actor, remove a VM label, set a reserved key on the VirtualMachine | VM label copied to the Anchor Pod; changed and added labels reach it; removed label leaves it; same Anchor Pod (no restart); the other actor's label stays; the reserved key is not copied |
| affinity | required node affinity for a label set on b and c | Ready; on b or c |
| spread | two VMs created together with the affinity of the previous case and a `topologySpreadConstraints` entry (`maxSkew: 1`, `kubernetes.io/hostname`, `DoNotSchedule`) over a shared label | both Ready; on different nodes |
| tolerations | c tainted `NoSchedule`; a VM selecting c without and one with the toleration | the first is `Unschedulable` on `Scheduled` and `Ready`, with a scheduler message that names the taint; the second is Ready on c |
| none | a VM whose `nodeSelector` matches no node, then the label is set on b | `Unschedulable`; Ready on b once the node is labeled |
| cordon | a, the only eligible node, cordoned; uncordoned | `Unschedulable`; Ready after the uncordon |
| rejected | a VM with an unknown `priorityClassName` | `Ready` reason `PodRejected` |
| runtime | baryon-virtd stopped on a; a new VM selecting a; baryon-virtd started | a's VirtualizationNode not Ready; the new VM `Unschedulable` (the scheduler message is logged); the VM of the selector case keeps its Anchor Pod; after the start both VMs Ready with that Anchor Pod unchanged |
| changed | `spec.placement` of the affinity case's VM changed | `RestartRequired` reason `PlacementChanged` |
| deleting | delete every VM | all VirtualMachines gone; on the node of each of the eight Anchor Pods of the run, no runtime directory, VMM, DHCP responder, or mounts left |

That is 65 checks: 33 for the cases and the deletion and 32 host checks.

`hostile-disk.sh` creates a filesystem-mode claim, lets a root pod create a
block device node as `disk.img` on it (`mknod`, which needs `CAP_MKNOD`),
and starts a VM on that claim. It expects `StorageFailed` with a message
that names the wrong disk kind, and (with `NODE_SHELL`) no VMM for the
Anchor Pod.

### Environment variables

`lib.sh` reads the following; `lifecycle.sh` also reads `PROBE_IMAGE` and
`SKIP_NETWORK`; `multinode.sh` and `hostile-disk.sh` read `PROBE_IMAGE`;
`multinode.sh`, `storage.sh`, `skew.sh`, `hostile-disk.sh`,
`import-integrity.sh`, and `placement.sh` read `STORAGE_CLASS` (required by
`import-integrity.sh`); `import-integrity.sh` also reads `PROBE_IMAGE` and
`SCAN_IMAGE`; `storage.sh` reads `VOLUME_MODE`; `skew.sh` reads
`EXPECT_MIXED`.

| Variable | Meaning |
|---|---|
| `KUBECONFIG` | standard kubectl configuration; the scripts call `kubectl` |
| `KUBE_CONTEXT` | kubectl context (default: current) |
| `NAMESPACE` | namespace for the test VMs and the probe pod (default: `default`) |
| `IMAGE_URL` | raw disk image URL (default: the Debian 13 genericcloud image over HTTPS) |
| `IMAGE_SHA256` | expected SHA-256; required when `IMAGE_URL` is plain `http://` (the scripts exit with status 2 otherwise) |
| `NODE_SHELL` | command that runs a shell command as root on a Baryon node, invoked as `$NODE_SHELL <node-name> sh -c <command>`, where `<node-name>` is the Kubernetes node name. When unset, host checks are skipped with a `SKIP` line. |
| `TIMEOUT` | seconds to wait for each transition (default: 300) |
| `PROBE_IMAGE` | image with `sh`, `nc`, and `ping` for the probe pod (default: `busybox:1.36`) |
| `SKIP_NETWORK` | `1` skips the SSH, Service, and ping checks and the probe pod |
| `STORAGE_CLASS` | StorageClass of the root disk claims (default: the cluster's default StorageClass) |
| `VOLUME_MODE` | `storage.sh` only: `Filesystem` (default) or `Block` |
| `EXPECT_MIXED` | `skew.sh` only: `1` fails unless the nodes run at least two different baryon-virtd builds |
| `SCAN_IMAGE` | `import-integrity.sh` only: image with `python3` for the root pods that fill and scan the device (default: `python:3.13-alpine`) |

`NODE_SHELL` is supplied by the user; Baryon ships no node access tool.
Any mechanism that runs a command as root on the named node works. An
example wrapper over SSH, assuming the node name resolves to the node and
the login user may run `sudo`:

```bash
#!/usr/bin/env bash
# node-shell <node-name> sh -c <command>: run <command> as root on the node.
set -euo pipefail
node="$1"
[ "$2" = sh ] && [ "$3" = -c ] || { echo "usage: $0 <node> sh -c <command>" >&2; exit 2; }
printf '%s\n' "$4" | ssh "$node" sudo sh -s
```

### Running

From the repository root, with Baryon deployed (see
[development.md](development.md)):

```bash
export KUBECONFIG=<path to the cluster kubeconfig>
NODE_SHELL=<path to your node-shell command> tests/e2e/lifecycle.sh
NODE_SHELL=<path to your node-shell command> tests/e2e/delete.sh
NODE_SHELL=<path to your node-shell command> tests/e2e/multinode.sh
NODE_SHELL=<path to your node-shell command> STORAGE_CLASS=<class> VOLUME_MODE=Block tests/e2e/storage.sh
NODE_SHELL=<path to your node-shell command> tests/e2e/hostile-disk.sh
NODE_SHELL=<path to your node-shell command> STORAGE_CLASS=<block-capable class> tests/e2e/import-integrity.sh
NODE_SHELL=<path to your node-shell command> EXPECT_MIXED=1 tests/e2e/skew.sh
NODE_SHELL=<path to your node-shell command> tests/e2e/placement.sh
```

To import from a server inside your network instead of the public Debian
image, set `IMAGE_URL`, and `IMAGE_SHA256` when the URL is plain HTTP:

```bash
IMAGE_URL=http://<server>/debian-13-genericcloud-amd64.raw \
IMAGE_SHA256=<sha256 of that file> \
NODE_SHELL=<path to your node-shell command> \
tests/e2e/lifecycle.sh
```

To repeat a script, prefix it with `tests/e2e/repeat.sh <count>`:

```bash
NODE_SHELL=<path to your node-shell command> tests/e2e/repeat.sh 10 tests/e2e/lifecycle.sh
```

### What deletion removes

Deleting a VirtualMachine removes: the VirtualMachine, its Anchor Pod, the
root disk claim Baryon created for an `http` source (and with it the
imported image), the runtime directory `/run/baryon/vms/<pod-uid>/`, the VMM
and DHCP responder processes, the VMM tmpfs, and the pod volumes. It keeps:
the per-pod log directories under `/var/log/baryon/vms/` until log retention
removes them (on Hadron, entries untouched for 14 days, see
[hadron.md](hadron.md)), and claims named in
`rootDisk.source.persistentVolumeClaim`, which the user owns. `lifecycle.sh`
and `delete.sh` check the removed items; log retention, including that the
directory of a running VM is kept, is a manual procedure.

## Manual procedures

Automating these procedures is Planned. Each needs a root shell on the node
that runs the VM.

Failure-path procedures:

| Case | Procedure | Expected result |
|---|---|---|
| baryon-virtd restart | `systemctl restart baryon-virtd` with a VM running; check the VMM pid and count the DHCP responders after the anchor reattaches | same VMM pid, exactly one new DHCP responder, the guest keeps answering |
| DHCP responder crash | `kill -9` the `baryon-dhcp` process of the VM | restarted by baryon-virtd |
| DHCP responder confinement | read its `/proc/<pid>/status`, namespaces, cgroup, and open descriptors | the VM's uid and gid without the kvm group, no capabilities, `NoNewPrivs` 1, own network namespace, Anchor container cgroup |
| VMM confinement | the same for the `cloud-hypervisor` process | values as in [security.md](security.md#vmm-confinement) |
| anchor crash | `kill -9` the `baryon-anchor run` process | the container runtime stops the VMM with the container; kubelet restarts the container and the VM boots again |
| VMM crash | `kill -9` the `cloud-hypervisor` process | the Anchor container restarts with reason `VmmKilled` (or `VmmExited` for a re-adopted VMM) and the VM boots again |
| guest reset | `PUT /api/v1/vm.reboot` on the VM's API socket | HTTP 204, a second kernel boot in the console log, a new DHCP lease |
| forced deletion, unresponsive guest | pause the VM through the Cloud Hypervisor API, then `kubectl delete pod --grace-period=0 --force` | the VMM is gone before kubelet starts volume teardown |
| deletion with baryon-virtd down | stop baryon-virtd, then delete the pod or the VirtualMachine | no VMM, responder, or pod mount left; the stale runtime directory is removed when baryon-virtd starts again |

Kairos lifecycle procedures:

| Case | Procedure | Expected result |
|---|---|---|
| node reboot | reboot the node with a VM running, without and with kubelet graceful node shutdown; read the console log and the Cloud Hypervisor log under `/var/log/baryon/vms/` | without graceful shutdown the guest is stopped hard; with it the guest shuts down cleanly; in both cases the VM is Ready again on a replacement Anchor Pod |
| Kairos A/B upgrade | `kairos-agent upgrade` with a VM running, then reboot (procedure in [kairos-integration.md](kairos-integration.md#manual-ab-upgrade)) | the node boots the new image (`/usr/share/baryon/components.json`), and the VM is Ready again |
| fresh provisioning | boot a node from an AuroraBoot artifact with a cloud-config that sets the node label and the graceful shutdown drop-in ([hadron.md](hadron.md#node-configuration)) | baryon-virtd and Kubernetes active, the drop-in in place, kubelet holding the shutdown inhibitor, VMs run |
| log retention | a temporary `/etc/tmpfiles.d/baryon.conf` with a short age, then `systemd-tmpfiles --clean` | stale per-pod log directories removed; the running VM's directory kept |
| Kairos Operator upgrade of one node | the runbook in [kairos-integration.md](kairos-integration.md#cooperation-with-upgrades): `NodeOpUpgrade` with a per-node `nodeSelector` and concurrency 1, with VMs on the node | the guests shut down cleanly before the reboot; VMs on storage that is not bound to the node start on another node, VMs on node-local storage start again when the node is back |

## Environments

### Requirements

An environment for the E2E tests and the manual procedures needs:

- x86_64 nodes with KVM (`/dev/kvm`). If the nodes are VMs, nested
  virtualization must be enabled on the hypervisor that runs them.
- The Baryon-enabled Hadron image on every Baryon node
  ([hadron.md](hadron.md)), and the node label
  `virtualization.baryon.io/host=true`.
- Kubernetes 1.32 or later. For the integration environment this is k0s;
  the CI API server checks use k0s v1.36.4+k0s.1.
- The kubelet systemd cgroup driver; cgroupfs is Not supported.
- kubelet graceful node shutdown with `shutdownGracePeriod` minus
  `shutdownGracePeriodCriticalPods` of at least 90 s.
- CSI storage that provides filesystem-mode `ReadWriteOnce` claims, as the
  default StorageClass or named in `rootDisk.storageClassName`; block-mode
  claims for `storage.sh` with `VOLUME_MODE=Block`; a StorageClass usable
  from every node for `multinode.sh` and `skew.sh`.
- For block mode, containerd with `device_ownership_from_security_context =
  true` ([compatibility.md](compatibility.md#csi-requirements)).
- For `hostile-disk.sh`, a namespace that admits a pod running as root with
  the container runtime's default capabilities.
- For `placement.sh`, at least three Baryon nodes on which the test may set
  and remove labels, taints, and cordons and stop baryon-virtd on one node
  for a while; run it on a cluster without other workloads that depend on
  those nodes.
- For `import-integrity.sh`, a StorageClass that provisions block volumes,
  a namespace that admits pods running as root (the filler and scanner
  read and write the raw device), and `deploy/base/tenant/roles.yaml`
  installed (part of `deploy/base`).
- Network connectivity: a CNI that meets
  [compatibility.md](compatibility.md#cni-requirements); pods can reach
  `IMAGE_URL` and pull `PROBE_IMAGE`; the machine running the tests can
  reach the API server and, through `NODE_SHELL`, every Baryon node.

All requirements are listed in [compatibility.md](compatibility.md).

### Environment classes

| Class | Role | Status |
|---|---|---|
| Multi-node cluster of Baryon-enabled Hadron nodes running k0s (three nodes; nested KVM when the nodes are VMs) | the authoritative integration environment | Tested results below ("k0s cluster") |
| Single Baryon-enabled Hadron node running k3s | single-node integration evidence | Tested results below; one node, so no cross-node traffic |
| Single-node k0s API server in a container (CI) | API server checks | runs in CI; not a Baryon host |
| Development cluster in which a kind node container acted as the host | historical evidence for Linux, containerd, kubelet, and Cloud Hypervisor behavior | no longer used; not a Baryon environment |

## Results

Results come from three environments:

- **k0s cluster** (the integration environment): three VMs with nested KVM
  (node1, node2, node3) running the Baryon-enabled
  Hadron image (Hadron v0.5.3, kernel 7.2.6-hadron, Kairos v4.3.0) with k0s
  v1.36.4+k0s.1: node1 is the controller and also runs workloads, node2 and
  node3 are workers. containerd from k0s with the Baryon drop-in (systemd
  cgroups, device ownership; [compatibility.md](compatibility.md#k0s-node-configuration)),
  graceful node shutdown 150 s and 30 s through a k0s worker profile, the
  k0s default CNI (kube-router; no network provider set in the k0s
  configuration), Cloud Hypervisor v53.0. Storage: OpenEBS
  LVM LocalPV v1.10.1 (`WaitForFirstConsumer`, "lvm-local") and Rook v1.20.8
  with Ceph v19.2.3 RBD ("ceph-rbd") through the
  ceph-csi-drivers chart 1.0.4 with `kubeletDirPath` set to the k0s kubelet
  root.
- **Hadron single node (k3s)**: one VM with nested KVM running the
  Baryon-enabled Hadron image (Hadron v0.5.3, kernel 7.2.6-hadron, Kairos
  through kairos-init v4.3.0), k3s v1.36.4+k3s1 with its embedded
  containerd, systemd cgroup driver, flannel (vxlan backend, `cni0` bridge),
  local-path provisioner, Cloud Hypervisor v53.0. Every pod ran on that one
  node.
- **kind host (historical)**: a development cluster in which a kind node
  container acted as the Baryon host, no longer used. kind v0.30.0,
  Kubernetes v1.34.0, containerd v2.1.3, systemd cgroup driver, kindnet,
  local-path provisioner, host kernel 6.8 with nested KVM, Cloud Hypervisor
  v53.0. It also had a control plane node, from which cross-node pod
  traffic to the guest was observed in Experiment B.

Results from the kind host are not results for a Baryon host; they are
listed because some behavior was observed only there. All runs were on
2026-10-03.

### k0s cluster

Results are counted as passed checks of the total. The commit names the
code that ran. "working tree" is the change set that introduced the import
Pod, the immutable `rootDisk`, the importer hardening, the disk checks in
baryon-virtd, `ClaimInUse`, and the controller watch backoff, since
committed as 4d2b5df to 1835c0c. "import integrity change" is the change on
top of 1835c0c that moved the import record to the PersistentVolume and
added the import approval, the block import contract, and the tenant
roles; "import integrity change, earlier revision" is the same change
before the `baryon-import-approval` policy and the split of the requeue
interval for refused VMs were added. "placement change" is the change on
top of 5f75b47 that added `spec.placement`, the VM labels on Baryon's pods,
`PlacementChanged`, and `PodRejected`; "placement change, first build" is
an earlier build of the same change, and "placement change, before the last
four changes" is the build that differs from the final code only by the
four changes named in its row.

| What was shown | Code | Kind | Result and notes |
|---|---|---|---|
| VM lifecycle with host cleanup checks (lvm-local) | 410aa30; working tree | E2E lifecycle | 33 of 33 on each |
| Deletion in every phase before Ready | 410aa30; working tree | E2E deletion | 410aa30: 16 of 16; working tree: 19 of 19, with the import case checking that no Anchor Pod exists while importing, that the import Pod requests only the placement slot, and that the importer has neither /dev/kvm nor the anchor socket. An earlier working-tree run failed the pending case because an unschedulable import Pod was not reported as `Unschedulable`; fixed before the 19 of 19 run |
| Multi-node cases (lvm-local) | 410aa30; working tree | E2E multi-node | 69 of 69 on each |
| Root disk storage, block mode | c8e6c08 | E2E storage | lvm-local 33 of 33; ceph-rbd 44 of 44, including a move to another node with CSI detach and attach and RBD NodeStage |
| Root disk storage, filesystem mode | c8e6c08 | E2E storage | ceph-rbd 44 of 44 (with the move); lvm-local 33 of 33 |
| Root disk storage with the import Pod and `ClaimInUse` (a second VM on the running VM's claim reports `ClaimInUse` and gets no Anchor Pod) | working tree | E2E storage | lvm-local Block 36 of 36; ceph-rbd Block 47 of 47; ceph-rbd Filesystem 47 of 47; lvm-local Filesystem 36 of 36 |
| A block device node made by a pod as `disk.img` is refused | working tree | E2E hostile disk | 4 of 4 |
| Import integrity on block volumes (stale data, a namespace user's pods and edits, one import across start, Anchor Pod replacement, and controller restart, `ImportStateUnknown` and recovery) | import integrity change; earlier revision | E2E import integrity | import integrity change: lvm-local Block 52 of 52 and ceph-rbd Block 52 of 52, including the check that a namespace user cannot set an import approval; earlier revision: 51 of 51 on each |
| Root disk storage with the PersistentVolume record check | import integrity change; earlier revision | E2E storage | import integrity change: lvm-local Block 36 of 36, ceph-rbd Filesystem 47 of 47; earlier revision: lvm-local Block 36 of 36, ceph-rbd Block 47 of 47, ceph-rbd Filesystem 47 of 47, lvm-local Filesystem 36 of 36 |
| VM placement and VM labels (lvm-local): selector, labels, affinity, spread, tolerations, no eligible node, cordon, `PodRejected`, baryon-virtd stopped on the only eligible node, `PlacementChanged` | placement change, before the last four changes | E2E placement | 65 of 65. In the runtime case the logged scheduler message was `Insufficient virtualization.baryon.io/import`. On the final code (reserved keys dropped from the recorded label keys, a 60 s requeue for `PodRejected`, the CI check for the out-of-service toleration, a code comment): 65 of 65, and lifecycle 33 of 33 |
| Regression of the other suites with placement | placement change, first build | E2E lifecycle, deletion, storage, import integrity, multi-node, skew, hostile disk | lifecycle 33 of 33, deletion 19 of 19, storage ceph-rbd Block 47 of 47, import integrity lvm-local Block 52 of 52, multi-node 69 of 69, skew 30 of 30, hostile disk 4 of 4. With the review fixes (import Pod labels and probe, `PodRejected`, label-keys annotation, reserved-key rule), before the last four changes: the same suites passed with the same counts |
| Regression of the other suites | import integrity change; earlier revision | E2E lifecycle, deletion, hostile disk, multi-node, skew | import integrity change: lifecycle 33 of 33, deletion 19 of 19; earlier revision: lifecycle 33 of 33, deletion 19 of 19, hostile disk 4 of 4, multi-node 69 of 69, skew 30 of 30 |
| Version skew, 1.0 cluster release (c8e6c08) with mixed 1.0 and 1.1 hosts | c8e6c08 and 597b637 | E2E skew | 31 of 31 |
| Version skew, 1.1 cluster release (597b637) with mixed hosts | c8e6c08 and 597b637 | E2E skew | 31 of 31; new anchors on 1.0 hosts took the UNIMPLEMENTED path of `Negotiate`; the node agent published `protocolVersion` 1.0 for old hosts and 1.1 with features for new ones |
| A 1.0 anchor reattached to a 1.1 baryon-virtd after a restart | c8e6c08 and 597b637 | E2E skew | passed |
| VMs of the 1.0 release through the cluster release upgrade | c8e6c08 to 597b637 | observation | kept running, with `RestartRequired=True` (`AnchorImageChanged`) |
| Version skew with all hosts on 1.1 | 597b637; working tree | E2E skew | the first run failed: the controller stopped reconciling after the API server restart that came with the control-plane node upgrade (see the controller recovery row); with the watch backoff fix, 30 of 30 |
| Controller recovery after a control-plane node reboot | 597b637; working tree | observation | before the fix, no VM was reconciled for minutes after every watch failed once with 410 Expired; with the fix, the controller reconciled shortly after the in-cluster API path returned, which itself lagged behind the API server accepting direct requests (cluster networking, not Baryon) |
| Migration of a filesystem claim from the previous release (marker file, no digest annotation) | working tree | observation | the import Pod found the marker, left the disk untouched (the guest did not regenerate its SSH host keys), and the digest was recorded |
| Node reboot with a running VM (lvm-local) | 410aa30 | manual | guest shut down cleanly (`reboot: Power down`); the VM came back without intervention on a replacement Anchor Pod on the same node once the node was Ready |
| Kairos A/B upgrade (`kairos-agent upgrade --source ocifile:...`) of all three nodes with VMs running | 410aa30 | manual | each node booted the new image and the VMs were Ready again; a VM survived the upgrade of the control-plane node |
| Kairos Operator v0.2.2 `NodeOpUpgrade` on one node (`force`, concurrency 1, upgrade image preloaded in containerd with a fixed tag) | working tree | manual | the drain deleted both Anchor Pods at once; both guests shut down cleanly before the reboot; the VM on ceph-rbd restarted on another node; the VM on lvm-local stayed Pending (`ClaimPending`, `Unschedulable` on the cordoned node) until its node returned and was then Ready; the operator reported the reboot complete and uncordoned the node while it was still NotReady |
| Kairos Operator reboot (`nsenter -i -m -t 1 -- reboot`) with a VM running, no drain | working tree | manual | kubelet's logind delay inhibitor held; the guest powered off cleanly; the VM came back |
| Kairos Operator v0.2.2 rolling `NodeOpUpgrade` of all three nodes (`nodeSelector` `virtualization.baryon.io/host=true`, concurrency 1, `force`, image preloaded on every node), one VM on ceph-rbd (node1) and one on lvm-local on each of node2 and node3 | working tree | manual | Completed; every node booted the new image (baryon-virtd hash equal to the image's). Concurrency 1 did not serialize the node outages: the drains of the three nodes started one after another without waiting for the previous node to return, because v0.2.2 counts a reboot as complete before the node is back, which frees the concurrency slot, so the node outages overlapped. Guests shut down cleanly at each drain. The lvm-local VMs came back on their own nodes once those returned. The ceph-rbd VM was down longest: no node was schedulable while all were cordoned or rebooting, and its restart was then delayed until the Ceph CSI attacher, whose controller had restarted with the control-plane node, regained its leader lease after the in-cluster API path returned; until then the volume stayed attached to the old node. Once the attacher led, detach and attach completed and the VM was Ready. All VMs recovered without intervention. VMs whose claims carried an older import record (no digest) got an import Pod after their drain; on filesystem claims the importer found the marker and left the disk unchanged |

### Hadron single node and kind host

These E2E runs used earlier versions of the scripts that ran on a single
node (25 checks in `lifecycle.sh`, with the host leftovers checked once for
all Anchor Pods of the run).

| What was shown | Environment | Kind | Result and notes |
|---|---|---|---|
| VM lifecycle: create, stop, start, restart, guest reset, delete, with host cleanup checks | Hadron single node (k3s); kind host | E2E lifecycle | all checks passed, including 10 consecutive runs on each environment; on Hadron, 113 to 120 s per run while another VM on the node kept running |
| Deletion in every phase before Ready | Hadron single node (k3s); kind host | E2E deletion | 16 of 16 on each environment |
| Guest reachable from another pod by pod IP and through a ClusterIP Service by DNS name, also after a restart with a new pod IP | Hadron single node (k3s); kind host | E2E lifecycle | passed; on Hadron the guest answered 2 to 3 s after `Ready=True` |
| Gratuitous ARP after the MAC change | Hadron single node (k3s); kind host | E2E lifecycle and observation | without it the guest was unreachable from other pods for 30 to 50 s after `Ready=True` (kind host); with it reachable at or shortly after `Ready=True` |
| Guest reset inside the running VMM with Landlock enforced | Hadron single node (k3s); kind host | E2E lifecycle; manual | without the Landlock rule for the VM's `vmm` directory the reset failed (`Address in use`); with it: HTTP 204, a second kernel boot, a new DHCP lease, ping |
| Admission policies `baryon-vm-resource` and `baryon-node-agent-own-node` enforced | Hadron single node (k3s); kind host | deployment check | both enforced |
| Anchor Pod admitted in a namespace with `pod-security.kubernetes.io/enforce: restricted` | kind host | observation | admitted |
| baryon-virtd restart with a VM running | Hadron single node (k3s); kind host | manual | same VMM pid, one new DHCP responder, guest answered; about 2 s (90 s before baryon-virtd exited 2 s after SIGTERM) |
| DHCP responder crash | kind host | manual | restarted within about 1 s |
| DHCP responder confinement | Hadron single node (k3s), after a node reboot; kind host | manual | VM uid and gid without the kvm group, `CapEff` and `CapBnd` 0, `NoNewPrivs` 1, own network namespace, Anchor container cgroup; open descriptors: stdin and stdout pipes, the log, the packet socket |
| VMM confinement | Hadron single node (k3s) | manual | values in [security.md](security.md#vmm-confinement); VMM tmpfs mounted with `size=32768k,nr_inodes=256,mode=700`, owned by the VM's identity |
| Anchor crash | Hadron single node (k3s) | manual | VMM gone within 1 s with the container; VM Ready again 24 s after the kill |
| VMM crash after a baryon-virtd restart | Hadron single node (k3s) | manual | Anchor container restarted with reason `VmmExited`; VM Ready again 13 s after the kill |
| Deletion with baryon-virtd stopped | Hadron single node (k3s); kind host (Experiment C timeline) | manual | no VMM, responder, or pod mount left; stale runtime directory removed when baryon-virtd started again; the guest got no ACPI shutdown |
| Forced deletion of a paused guest | kind host only | manual (Experiment A) | the VMM was killed before volume teardown started |
| Node reboot without graceful node shutdown | Hadron single node (k3s) | manual | guest stopped hard; the old Anchor Pod failed admission (`UnexpectedAdmissionError`) and was replaced; VM Ready 72 s after the reboot command |
| Node reboot with graceful node shutdown (KubeletConfiguration drop-in, 150 s and 30 s) | Hadron single node (k3s) | manual | kubelet held a logind delay inhibitor; guest shut down cleanly; old Anchor Pod `Succeeded` and replaced; VM Ready 67 to 71 s after the reboot command; the node was reachable again about 40 s after the reboot command |
| Kairos A/B upgrade with a VM running, six times in a row | Hadron single node (k3s) | manual | each time the node booted the new image (baryon-virtd hash or `gitCommit` in `components.json` checked) and the VM was Ready again on a replacement Anchor Pod; the recovery system was not upgraded |
| Fresh provisioning from an AuroraBoot raw EFI disk with a cloud-config | Hadron single node (k3s) | manual | recovery boot installed the system and rebooted; the graceful shutdown drop-in was in place and kubelet held the inhibitor with no manual step; baryon-virtd and k3s active; lifecycle and deletion E2E passed afterwards |
| Log retention with a 1 s override age | Hadron single node (k3s) | manual | stale per-pod log directories removed; the running VM's directory, under baryon-virtd's shared `flock`, kept |
| Runtime-boundary experiments A, B, and C | kind host | experiment | see [runtime-boundary.md](runtime-boundary.md) |

Not yet covered by any recorded result: the `IdentityConflict` replacement
path, cross-node traffic over flannel, CSI teardown under SIGKILL, forced
delete, and baryon-virtd down with a staging driver, `DiskInUse` on a
cluster, and the boot-ID-based reboot detection of the upstream Kairos
Operator main branch.
