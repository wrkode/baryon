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
| E2E deletion | `tests/e2e/delete.sh` | deletion in every phase before Ready | a cluster of Baryon nodes | no |
| Failure-path procedures | manual, listed below | crashes and restarts of baryon-virtd, the anchor, the VMM, and the DHCP responder; deletion while baryon-virtd is down | a cluster of Baryon nodes and a root shell on the node | no |
| Kairos lifecycle procedures | manual, listed below | node reboot, Kairos A/B upgrade, fresh provisioning, log retention | a cluster of Baryon nodes and a root shell on the node | no |
| Multi-node suite | Planned | cross-node guest traffic, placement on another node after stop and start, identity conflict handling across nodes | a multi-node cluster of Baryon nodes | no |
| CSI suite | Planned | block-mode claims, a CSI driver with NodeStage and attach, teardown ordering under real detach | a cluster with such a driver | no |

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
| `baryon-virtd` | pod cgroup parsing (systemd, cgroupfs, nested roots) and systemd scope detection, peer pidfd, the anchor identity check (`IdentityMismatch`) and the refusal of a second VM with the same identity on a node (`IdentityConflict`), disk resolution confinement (absolute symlink and `..` escapes), `/proc/self/fd` reopen, access to the VMM-owned directory (bounded tail reads; symlinks, hard links, other file types, and a foreign owner refused; socket connection through a checked descriptor), DHCP responses (routed, bridged, and off-link gateway route tables, NAK, other clients), IPv4 and UDP checksums, DHCP responder protocol (lease configuration round trip, only events matching the lease forwarded, overlong lines rejected), gratuitous ARP frame layout, tc filter encoding, cgroup limits, serial prompt detection and capture, record round trip, spec validation |
| `baryon-runtime-api` | per-VM identity (deterministic, distinct for different VMs, unprivileged and inside the reserved range, verification), guest MAC derivation, anchor spec round trip |
| `baryon-anchor` | sparse writes, redirect resolution (absolute, root-relative, path-relative; HTTPS to HTTP refused), import against a local HTTP server with checksum and marker handling, termination messages |
| `baryon-api` | CRD generation with CEL rules and list-map markers, defaults, conditions, quantities |
| `baryon-node-agent` | device plugin slots and allocation, VirtualizationNode status translation |
| `baryon-controller` | Anchor Pod shape (Guaranteed, restricted, no host access), block mode, required node affinity away from `identityConflictNodes`, detection of an identity conflict from the anchor termination message, ownership of an existing Anchor Pod (controller owner reference with the VM UID), UID preconditions on deletes, status derivation for ready, failed, importing, stopped, and blocked VMs, a running pod taking precedence over a blocked class, the two RestartRequired reasons |

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
namespace, and the admission policies, then uses server-side dry runs to
confirm that:

- the example VirtualMachine is accepted
- a VirtualMachine with two disk sources is rejected
- a plain-HTTP source without `sha256` is rejected
- a VirtualMachine name with a dot is rejected (DNS-1123 label rule)
- `baryon-vm-resource` denies a pod that requests `virtualization.baryon.io/vm`
- a plain pod is admitted

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

`delete.sh` cases, each checking that the VirtualMachine, the Anchor Pod, and
the root disk claim are gone:

| Case | How the phase is reached | Host checks |
|---|---|---|
| import | delete while the import init container runs | runtime directory, VMM, DHCP responder, mounts |
| booting | delete after the VMM started, while the Ready reason is `Booting` | runtime directory, VMM, DHCP responder, mounts |
| pending | delete while the Anchor Pod is unschedulable (a VirtualMachineClass with 1Ti memory) | none (no pod on a node) |
| stopped | delete a VM with `runPolicy: Stopped` | none (no pod) |

That is 16 checks with `NODE_SHELL` set and 8 without it. `delete.sh` needs
the VirtualMachineClass `general-small` from `examples/classes.yaml`.

### Environment variables

`lib.sh` reads the following; `lifecycle.sh` also reads `PROBE_IMAGE` and
`SKIP_NETWORK`.

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
  default StorageClass or named in `rootDisk.storageClassName`.
- Network connectivity: a CNI that meets
  [compatibility.md](compatibility.md#cni-requirements); pods can reach
  `IMAGE_URL` and pull `PROBE_IMAGE`; the machine running the tests can
  reach the API server and, through `NODE_SHELL`, every Baryon node.

All requirements are listed in [compatibility.md](compatibility.md).

### Environment classes

| Class | Role | Status |
|---|---|---|
| Multi-node cluster of Baryon-enabled Hadron nodes running k0s (three nodes; nested KVM when the nodes are VMs) | the authoritative integration environment | testing in progress; no results yet, so nothing is Tested on it |
| Single Baryon-enabled Hadron node running k3s | single-node integration evidence | Tested results below; one node, so no cross-node traffic |
| Single-node k0s API server in a container (CI) | API server checks | runs in CI; not a Baryon host |
| Development cluster in which a kind node container acted as the host | historical evidence for Linux, containerd, kubelet, and Cloud Hypervisor behavior | no longer used; not a Baryon environment |

## Results

All recorded results are single-node evidence: each environment had one
Baryon host. Multi-node results are not available yet. Results come from
two environments:

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
listed because some behavior was observed only there.

The recorded E2E runs used earlier versions of the scripts that ran on a
single node (25 checks in `lifecycle.sh`, with the host leftovers checked
once for all Anchor Pods of the run). No result of the current, multi-node
aware version is recorded yet. All runs were on 2026-10-03.

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

Not yet covered by any recorded result: multi-node behavior (cross-node
traffic over flannel or another CNI on Baryon nodes, placement on another
node, the `IdentityConflict` replacement path), k0s nodes, block-mode
claims, a CSI driver with NodeStage and attach, and a Kairos Operator
`NodeOpUpgrade`.
