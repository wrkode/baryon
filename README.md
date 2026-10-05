# Baryon

Baryon is a Kubernetes-native virtualization platform built specifically for
Hadron and Cloud Hypervisor. Kubernetes schedules and owns each VM through an
ordinary pod. A Kairos-managed immutable Hadron image delivers the host side
as one artifact: kernel, Cloud Hypervisor, firmware, host dependencies,
and the Baryon host runtime.

Kubernetes manages the VM. Hadron provides the virtualization host. Cloud
Hypervisor executes the VM. Kairos manages the machine lifecycle around them.

Baryon is early, pre-release software. The API is `v1alpha1` and will
change; which parts are contract, the v0.1 freeze, and the change log are
in [docs/api-stability.md](docs/api-stability.md).

Which versions v0.1 is tested with (Hadron, Kairos, Cloud Hypervisor, k0s
and Kubernetes, container runtime, CNI, CSI drivers, guests) is in the
[support matrix](docs/compatibility.md#v01-support-matrix); which host and
cluster versions work together is in
[version skew](docs/compatibility.md#version-skew); what happens to VMs
when a node is cordoned, drained, rebooted, or upgraded is in the
[lifecycle contract](docs/kairos-integration.md#lifecycle-contract).

## Status

Status labels follow [docs/architecture.md](docs/architecture.md). Tested
entries name the environment they were observed on, summarized in
[docs/testing.md](docs/testing.md#results):

- "k0s cluster": the integration environment, three Baryon-enabled Hadron
  nodes (node1 to node3, VMs with nested KVM) running k0s, with OpenEBS LVM
  LocalPV and Ceph RBD storage.
- "Hadron k3s node": one Baryon-enabled Hadron node running k3s.
- "kind host": a historical development cluster in which a kind node
  container acted as the host, no longer used.

| Area | Status |
|---|---|
| VirtualMachine, VirtualMachineClass, VirtualizationNode API | Implemented; CRD schema and CEL rules (the immutable `rootDisk` and class `spec`, the `rootDisk` and class validation) checked against a k0s API server in CI |
| Controller: Anchor Pods, import Pods, root disk claims, status, finalizer, stop, start, restart | Tested: lifecycle E2E on the k0s cluster, the Hadron k3s node, and the kind host |
| Deletion in every phase before Ready (importing, booting, unschedulable, stopped) | Tested: deletion E2E on the k0s cluster, the Hadron k3s node, and the kind host |
| Multi-node operation: one VM per node, controller restart, baryon-virtd restart and outage on one node, VMM crash, Anchor Pod replacement behind a Service, cordon, deleting one VM | Tested: multi-node E2E on the k0s cluster |
| Host runtime (baryon-virtd): Cloud Hypervisor lifecycle | Tested: lifecycle E2E on the k0s cluster, the Hadron k3s node, and the kind host |
| Host runtime: recovery after a baryon-virtd restart (baryon-virtd exits 2 s after SIGTERM) | Tested: multi-node E2E on the k0s cluster; manual procedure on the Hadron k3s node and the kind host |
| Host cgroup driver | systemd driver required: baryon-virtd refuses other Anchor container cgroups (Implemented; the refusal was observed on k0s nodes whose containerd used cgroupfs). cgroupfs: Not supported |
| Guest reset inside a running VMM, disks reopened under Landlock | Tested: lifecycle E2E on the k0s cluster, the Hadron k3s node, and the kind host |
| VMM confinement: a distinct unprivileged host identity per VM, no capabilities, empty network namespace, seccomp, Landlock, private tmpfs runtime directory | Tested: process values and the tmpfs measured on the Hadron k3s node; the lifecycle E2E runs the confined VMM |
| Baryon seccomp layer for the VMM and the DHCP responder, under Cloud Hypervisor's filters: denies `userfaultfd`, `ptrace`, `process_vm_readv`, `process_vm_writev`, `pidfd_getfd`, sockets other than `AF_UNIX`, and foreign ABIs ([security.md](docs/security.md#baryon-seccomp-layer)) | Tested: unit tests in CI; lifecycle E2E on the k0s cluster checks every thread of the VMM and the DHCP responder |
| Identity conflict between two VMs on one node: refused by baryon-virtd, VM placed on another node | Implemented, unit tests; not exercised on a cluster |
| DHCP responder as a separate process with the VMM's confinement | Tested on the k0s cluster, the Hadron k3s node, and the kind host |
| Guest network service health: the DHCP responder restarted for as long as the VM runs, reported as the VM's `NetworkReady` condition (runtime protocol 1.2, VirtualizationNode `status.vms`) | Tested: network-health E2E on the k0s cluster with Ceph RBD and OpenEBS LVM LocalPV, including baryon-virtd restart, node agent outage, and node reboot; unit tests |
| Guest network service health: a responder that hangs without exiting is detected by liveness probes over its stdin and stdout pipes (within about 35 s) and restarted; `NetworkReady` True only after the first probe answer (after a failure, after 25 s of answers) | Tested: network-health E2E on the k0s cluster with Ceph RBD and OpenEBS LVM LocalPV (cases hang, hang-repeated, and before-lease); unit tests |
| `NetworkReady` across a baryon-controller restart while a node agent is down | Tested: node-agent-outage E2E on the k0s cluster (cases down-then-restart and restart-then-down); unit tests |
| Admission policies (`deploy/base/policy/`): vm and import resources only for baryon-controller, node agent writes only its own node | Tested: CI API server checks (vm resource, import resource, and init container denial); enforced on the Hadron k3s node and the kind host |
| Admission policies for the import and VM labels: baryon-controller may only add the import record to a PersistentVolume and, on Baryon pods, set the import approval and the VM's labels except reserved keys (`baryon-controller-writes`); only baryon-controller may set the import approval (`baryon-import-approval`) | Tested: CI API server checks, every allow and deny case by impersonation; `baryon-import-approval` also by the import integrity E2E on the k0s cluster |
| Roles for namespace users (`baryon-vm-edit`, `baryon-vm-view`, aggregated into `edit`, `admin`, and `view`; no VirtualMachine status, no PersistentVolumes) | Tested: the import integrity E2E runs as a service account bound to `edit` on the k0s cluster |
| Pod network for VMs (TAP in the pod namespace, guest takes the pod IP) | Tested from a probe pod to VMs on all three nodes and through a ClusterIP Service by DNS name: multi-node E2E on the k0s cluster (k0s default CNI, kube-router); lifecycle E2E on the Hadron k3s node (flannel) and the kind host (kindnet). NetworkPolicy and guest-initiated egress: Inferred, not tested |
| PVC root disks, HTTP raw image import (filesystem mode) | Tested: storage E2E with OpenEBS LVM LocalPV and Ceph RBD on the k0s cluster; lifecycle E2E with local-path on the Hadron k3s node and the kind host |
| Block-mode root disks | Tested: storage E2E with OpenEBS LVM LocalPV and Ceph RBD on the k0s cluster, including a move of a Ceph RBD VM to another node. Requires containerd `device_ownership_from_security_context = true` ([compatibility.md](docs/compatibility.md)) |
| Image import in a separate import Pod, recorded on the bound PersistentVolume before the first Anchor Pod exists | Tested: deletion and storage E2E on the k0s cluster |
| Import integrity: import only into a volume provisioned for the claim, an approval only baryon-controller can give, completion only from the import Pod's spec, refusal instead of a second import, every byte of an imported block device the image's or zero | Tested: import integrity E2E with OpenEBS LVM LocalPV and Ceph RBD block volumes on the k0s cluster, including the check that a namespace user cannot set an import approval; unit tests for the import decision |
| One running VM per claim (`ClaimInUse` in the controller, `DiskInUse` in baryon-virtd) | `ClaimInUse` Tested by the storage E2E on the k0s cluster; `DiskInUse` Implemented, unit tests |
| baryon-virtd accepts only the Anchor container's disk paths, kinds, and `/dev` device nodes | Tested: hostile-disk E2E on the k0s cluster |
| Runtime protocol versions and feature negotiation (ADR 0012) | Tested: version skew E2E on the k0s cluster with mixed protocol 1.0 and 1.1 hosts, under both cluster releases |
| Controller recovery after an API server restart | Implemented (short watch backoff); observed after a control-plane node reboot on the k0s cluster: reconciliation resumed shortly after the in-cluster API path returned |
| VirtualizationNode (runtime health and versions, heartbeat; the host inventory is logged by the node agent, not published) | Implemented; unit tests in CI; the device plugins are exercised by every E2E run. `NodeAgentStopped` on node agent shutdown: Tested (node-agent-outage E2E) |
| Baryon-enabled Hadron image | Tested with k0s (three nodes) and k3s (one node), each provisioned from an AuroraBoot raw EFI disk and a cloud-config |
| Per-VM log retention (14 days; a running VM's log directory is kept) | Tested: manual procedure on the Hadron k3s node |
| Node reboot with running VMs | Tested: manual procedure on the k0s cluster and the Hadron k3s node; a clean guest shutdown needs kubelet graceful node shutdown |
| Kairos A/B upgrade with running VMs (`kairos-agent upgrade`) | Tested: manual procedure on all three nodes of the k0s cluster and on the Hadron k3s node |
| Kairos Operator `NodeOpUpgrade` (v0.2.2) on nodes with running VMs | Tested on the k0s cluster for one node and for all three nodes: guests shut down cleanly at the drain and every VM came back without intervention. With v0.2.2, concurrency 1 does not keep node outages from overlapping; to upgrade one node at a time, use one `NodeOpUpgrade` per node ([kairos-integration.md](docs/kairos-integration.md#cooperation-with-upgrades)) |
| VM evacuation (moving running VMs off a node) | Not supported: a drain stops VMs; they start again where their storage allows |
| VM placement (`spec.placement`: nodeSelector, affinity, tolerations, topology spread, PriorityClass, as Kubernetes types copied to the VM's pods; the Kubernetes scheduler places them) and the VM's labels copied to its pods | Tested: placement E2E on the k0s cluster. The CEL rules (no tolerations of cordon, control-plane, or out-of-service taints, no `system-` PriorityClasses, no cross-namespace pod affinity) are checked against a k0s API server in CI. Changes apply at the next start (`RestartRequired`, `PlacementChanged`) |
| Placement by host capability (IOMMU, hugepages, NUMA, VFIO, runtime protocol features) | Not supported; deferred until Baryon consumes a capability (ADR 0012) |
| Live migration, snapshots, VFIO, GPU passthrough, SR-IOV, hugepages, NUMA placement | Not supported |
| Windows guests, confidential computing | Not supported |

## How it works

```
VirtualMachine --> baryon-controller --> import Pod (imported disks only, runs first)
                                     --> Anchor Pod (scheduling, PVC, pod network,
                                          resources, virtualization.baryon.io/vm)
                                              |
                                        baryon-anchor --Unix socket--> baryon-virtd (host)
                                                                          |
                                                    TAP in the pod netns, disk from the pod,
                                                    Cloud Hypervisor as the VM's identity, in
                                                    the Anchor container's cgroup
```

- Each running VM has one VM Anchor Pod. Kubernetes schedules it, attaches its
  storage through CSI, and connects it through CNI. Baryon does not schedule,
  provide storage, or provide networking.
- A VM whose disk is imported over HTTP first gets an import Pod that writes
  the image into the root disk claim, once baryon-controller has approved
  it, and exits; the Anchor Pod is created after the import is recorded on
  the claim's PersistentVolume and the import Pod is gone.
- Cloud Hypervisor runs as a host process started by baryon-virtd, but as
  the VM's own unprivileged host identity (the same as the Anchor
  container's), without capabilities, in its own empty network namespace,
  and inside the pod container's cgroup, so pod limits apply and, with the
  kubelet systemd cgroup driver, the container runtime stops the VMM with the
  pod.
- The guest takes over the pod IP and keeps a stable MAC address.
- The controller is unprivileged and never talks to the host runtime.

Details: [docs/architecture.md](docs/architecture.md).

## Quick look

```yaml
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachineClass
metadata:
  name: general-small
spec:
  cpu:
    vcpus: 2
  memory:
    size: 2Gi
---
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata:
  name: debian
spec:
  className: general-small
  rootDisk:
    size: 10Gi
    source:
      http:
        url: https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.raw
```

```
$ kubectl get virtualmachines
NAME     READY   REASON        NODE    ADDRESS       AGE
debian   True    GuestBooted   node1   10.244.1.30   62s
```

## Documentation

| Document | Content |
|---|---|
| [architecture.md](docs/architecture.md) | components, ownership, request flow |
| [api.md](docs/api.md) | API reference |
| [api-stability.md](docs/api-stability.md) | API contract, v0.1 freeze, change log per release |
| [runtime-boundary.md](docs/runtime-boundary.md) | experiments and evidence behind the runtime design |
| [networking.md](docs/networking.md), [storage.md](docs/storage.md) | data paths |
| [security.md](docs/security.md) | threat model and privileges |
| [kairos-integration.md](docs/kairos-integration.md) | Hadron, Kairos, AuroraBoot, Kairos Operator, Cluster API; lifecycle contract and upgrade runbook |
| [hadron.md](docs/hadron.md) | the Baryon-enabled Hadron image |
| [compatibility.md](docs/compatibility.md) | v0.1 support matrix; host, Kubernetes, CNI, CSI, guest requirements; version skew and upgrades |
| [design-notes.md](docs/design-notes.md) | open and answered design questions |
| [development.md](docs/development.md), [testing.md](docs/testing.md) | working on Baryon |
| [adr/](docs/adr/) | architecture decision records |

## Building

```bash
cargo test --workspace --locked
hack/build-static.sh        # static musl binaries
hack/build-hadron-image.sh <registry>/baryon/hadron:<tag>   # Baryon-enabled Hadron image (docs/hadron.md)
```

Building, deploying to a cluster of Hadron nodes, and running the tests are
described in [docs/development.md](docs/development.md) and
[docs/testing.md](docs/testing.md).

## Contributing

Contributions are accepted under the Developer Certificate of Origin: every
commit carries a `Signed-off-by` trailer, created with `git commit -s`.
Before sending a change, run the checks listed in
[docs/development.md](docs/development.md#build-and-check).

## License

Baryon is licensed under the Apache License, Version 2.0. See
[LICENSE](LICENSE) and [ADR 0010](docs/adr/0010-license.md).
