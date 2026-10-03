# Baryon

Baryon is a Kubernetes-native virtualization platform built specifically for
Hadron and Cloud Hypervisor. Kubernetes schedules and owns each VM through an
ordinary pod. A Kairos-managed immutable Hadron image delivers the host side
as one artifact: kernel, Cloud Hypervisor, firmware, host dependencies,
and the Baryon host runtime.

Kubernetes manages the VM. Hadron provides the virtualization host. Cloud
Hypervisor executes the VM. Kairos manages the machine lifecycle around them.

Baryon is early, pre-release software. The API is `v1alpha1` and will change.

## Status

Status labels follow [docs/architecture.md](docs/architecture.md). Every
Tested entry is single-node evidence: it was observed on one
Baryon-enabled Hadron node running k3s ("Hadron, k3s"), and some entries
also on a historical development cluster in which a kind node container
acted as the host ("kind host", no longer used). The runs are summarized in
[docs/testing.md](docs/testing.md#results). The authoritative integration
environment is a multi-node cluster of Baryon-enabled Hadron nodes running
k0s; testing on it is in progress and no multi-node result is available yet.

| Area | Status |
|---|---|
| VirtualMachine, VirtualMachineClass, VirtualizationNode API | Implemented; CRD schema and CEL rules checked against a k0s API server in CI |
| Controller: Anchor Pods, root disk claims, status, finalizer, stop, start, restart | Tested: lifecycle E2E on Hadron, k3s and on the kind host |
| Deletion in every phase before Ready (importing, booting, unschedulable, stopped) | Tested: deletion E2E on Hadron, k3s and on the kind host |
| Host runtime (baryon-virtd): Cloud Hypervisor lifecycle | Tested: lifecycle E2E on Hadron, k3s and on the kind host |
| Host runtime: recovery after a baryon-virtd restart (baryon-virtd exits 2 s after SIGTERM) | Tested: manual procedure on Hadron, k3s and on the kind host |
| Host cgroup driver | systemd driver required: baryon-virtd refuses other Anchor container cgroups (Implemented). cgroupfs: Not supported |
| Guest reset inside a running VMM, disks reopened under Landlock | Tested: lifecycle E2E on Hadron, k3s and on the kind host |
| VMM confinement: a distinct unprivileged host identity per VM, no capabilities, empty network namespace, seccomp, Landlock, private tmpfs runtime directory | Tested: process values and the tmpfs measured on Hadron, k3s; the lifecycle E2E runs the confined VMM |
| Identity conflict between two VMs on one node: refused by baryon-virtd, VM placed on another node | Implemented, unit tests; not exercised on a cluster |
| DHCP responder as a separate process with the VMM's confinement | Tested on Hadron, k3s and on the kind host |
| Admission policies (`deploy/base/policy/`): vm resource only for baryon-controller, node agent writes only its own node | Tested: CI API server checks (vm resource denial), Hadron, k3s, kind host |
| Pod network for VMs (TAP in the pod namespace, guest takes the pod IP) | Tested from another pod by pod IP and through a ClusterIP Service by DNS name: lifecycle E2E on Hadron, k3s (flannel, single node) and on the kind host (kindnet). Cross-node traffic on Baryon nodes: not tested. NetworkPolicy and guest-initiated egress: Inferred, not tested |
| PVC root disks, HTTP raw image import | Tested: filesystem mode with local-path on Hadron, k3s and on the kind host |
| Block-mode root disks | Implemented, not tested |
| Host capability discovery and VirtualizationNode | Implemented; unit tests in CI; the device plugin is exercised by every lifecycle E2E run |
| Baryon-enabled Hadron image | Tested with k3s: a fresh node provisioned from an AuroraBoot raw EFI disk and a cloud-config. With k0s: Implemented, testing in progress |
| Per-VM log retention (14 days; a running VM's log directory is kept) | Tested: manual procedure on Hadron, k3s |
| Node reboot with running VMs | Tested: manual procedure on Hadron, k3s; a clean guest shutdown needs kubelet graceful node shutdown |
| Kairos A/B upgrade with a running VM (`kairos-agent upgrade`) | Tested: manual procedure on Hadron, k3s |
| Multi-node clusters (k0s, three Hadron nodes) | Testing in progress; not yet Tested |
| VM evacuation before upgrades (VirtualizationNode `Evacuating`, `Evacuated`, `UpgradeSafe`) | Designed. A Kairos Operator `NodeOpUpgrade` on a Baryon node is not tested. |
| VM placement controls (nodeSelector, tolerations, affinity, priority) | Planned |
| Live migration, snapshots, VFIO, GPU passthrough, SR-IOV, hugepages, NUMA placement | Not supported |
| Windows guests, confidential computing | Not supported |

## How it works

```
VirtualMachine --> baryon-controller --> Anchor Pod (scheduling, PVC, pod network,
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
    cores: 2
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
| [runtime-boundary.md](docs/runtime-boundary.md) | experiments and evidence behind the runtime design |
| [networking.md](docs/networking.md), [storage.md](docs/storage.md) | data paths |
| [security.md](docs/security.md) | threat model and privileges |
| [kairos-integration.md](docs/kairos-integration.md) | Hadron, Kairos, AuroraBoot, Kairos Operator, Cluster API |
| [hadron.md](docs/hadron.md) | the Baryon-enabled Hadron image |
| [compatibility.md](docs/compatibility.md) | host, Kubernetes, CNI, CSI, guest requirements |
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
