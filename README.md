# Baryon

Baryon is a Kubernetes-native virtualization platform for Hadron using
Cloud Hypervisor. It combines Kubernetes scheduling and storage and network
integration with a Kairos-managed immutable virtualization host.

Kubernetes manages the VM. Hadron provides the virtualization host. Cloud
Hypervisor executes the VM. Kairos manages the machine lifecycle around them.

Status: this tree is version 0.1.0-rc2, the second release candidate
([release notes](docs/release-notes/v0.1.0-rc2.md)). The API is `v1alpha1`
and can change ([api-stability.md](docs/api-stability.md)). Baryon makes no
claim of production readiness. Intended work and its status:
[roadmap.md](docs/roadmap.md).

## Why Hadron

Hadron is a Linux distribution built from source by the Kairos project
(musl libc, systemd, no runtime package manager) whose default kernel has
KVM, TUN, vhost, seccomp, and Landlock. Baryon adds Cloud Hypervisor, its
UEFI firmware, and the host runtime baryon-virtd as an image layer, so a
Baryon host is one immutable image that Kairos installs and upgrades A/B
([hadron.md](docs/hadron.md), ADR 0003).

## Why Cloud Hypervisor

Cloud Hypervisor takes a pre-opened TAP file descriptor for the network
and opens disks through inherited file descriptors (`/proc/self/fd/N`),
which lets baryon-virtd hand the VM Anchor Pod's network and storage to the
VMM, and it supports seccomp and Landlock confinement. Baryon pins one tested
Cloud Hypervisor version per release and has no other VMM (ADR 0002).

## What is supported

Support claims follow tests on the k0s cluster: three x86_64
Baryon-enabled Hadron nodes (node1 to node3, VMs with nested KVM) running
k0s v1.36.4+k0s.1, with Cloud Hypervisor v54.0 (v53.0 in v0.1.0-rc1),
kube-router, OpenEBS LVM LocalPV, Ceph RBD through Rook, and Debian 13
guests. Exact versions are in
the [v0.1 support matrix](docs/compatibility.md#v01-support-matrix).

| Area | Status |
|---|---|
| VM lifecycle: create from an HTTP raw image or an existing claim, start, stop, restart, delete | Tested: E2E on the k0s cluster |
| Root disks on filesystem-mode and raw block-mode claims | Tested: storage and import integrity E2E with OpenEBS LVM LocalPV and Ceph RBD |
| Pod network: the guest takes the pod IP; Services select a VM's Anchor Pod by its `virtualization.baryon.io/vm-name` label | Tested: multi-node E2E. Services that select the VM's own labels (copied to the Anchor Pod): Implemented; not tested |
| Placement through Kubernetes scheduling fields (`spec.placement`) | Tested: placement E2E |
| Recovery after a baryon-virtd, baryon-controller, or node agent restart, a VMM crash, and a node reboot | Tested: E2E and manual procedures |
| Guest network service health (`NetworkReady`) | Tested: network-health E2E |
| Host upgrades through Kairos A/B and Kairos Operator `NodeOpUpgrade`, one node at a time; VMs stop and start again | Tested: manual procedures |
| Mixed host and cluster versions within a runtime protocol major version | Tested for the pairs in [version skew](docs/compatibility.md#version-skew) |
| VMM confinement: a distinct unprivileged host identity per VM, no capabilities, empty network namespace, seccomp, Landlock ([security.md](docs/security.md)) | Tested: the lifecycle E2E on the k0s cluster runs the confined VMM and checks the seccomp state of every VMM thread; process values measured on a single Hadron node |
| Admission policies and namespace user roles | Tested: CI API server checks and import integrity E2E. The ADR 0015 admission rules: their API server checks pass in CI; the image change and ephemeral container refusals are Tested by the anchor authority E2E on the k0s cluster |

Evidence for each row: [testing.md](docs/testing.md#results). Status
labels: [architecture.md](docs/architecture.md).

## What is not supported

- Live migration is currently unsupported. Baryon's network handoff model
  requires destination-side file descriptor injection during Cloud
  Hypervisor migration, which is not available in the currently supported
  Cloud Hypervisor release. See [ADR 0013](docs/adr/0013-live-migration.md).
  A drain or node upgrade stops the VMs on the node; they start again
  where their storage allows.
- Windows guests.
- GPU passthrough and other VFIO or PCI device passthrough.
- SR-IOV.
- Snapshots and cloning.
- More than one disk or network per VM.
- Multi-cluster operation: Baryon manages the VMs of one cluster.
- Also Not supported in v0.1: confidential computing, nested
  virtualization inside guests, hugepage-backed guest memory, NUMA
  placement, disk image formats other than raw,
  aarch64, the kubelet cgroupfs driver, and k3s (kubeadm is Not tested).
  The full list:
  [Not supported in v0.1](docs/compatibility.md#not-supported-in-v01).

## Architecture

```
Kubernetes API (VirtualMachine, VirtualMachineClass)
        |
        v
baryon-controller        unprivileged Deployment; never talks to the host
        |  creates
        v
VM Anchor Pod            scheduled by Kubernetes; root disk claim through CSI,
        |                pod network through CNI, CPU and memory limits
        |  Unix socket on the node
        v
baryon-virtd             host service in the Baryon-enabled Hadron image
        |  starts and supervises
        v
Cloud Hypervisor         one process per VM, as the VM's own unprivileged
                         identity, in the Anchor container's cgroup
```

- A VM with an HTTP image source first gets an import Pod, which writes the
  image into the root disk claim; the Anchor Pod starts after the import is
  recorded.
- baryon-node-agent, a DaemonSet on every Baryon node, advertises VM slots
  to the scheduler and publishes each node's runtime health as a
  VirtualizationNode.
- Baryon does not schedule, provide storage, or provide networking:
  Kubernetes, the CSI driver, and the CNI plugin do.

Details: [architecture.md](docs/architecture.md).

## Install

[docs/installation.md](docs/installation.md) covers the whole path: the
host image (published or built from a checkout), bootable media with
AuroraBoot, the Kairos cloud-config for k0s nodes, storage, the release
manifests, verification, the first VM, uninstalling, and upgrading. With a
cluster of Baryon-enabled Hadron nodes and a CSI driver in place, the
release install is:

```bash
kubectl apply --server-side -f https://github.com/wrkode/baryon/releases/download/v0.1.0-rc2/baryon.yaml
kubectl apply -f https://github.com/wrkode/baryon/releases/download/v0.1.0-rc2/baryon-classes.yaml
```

These URLs resolve once the release workflow has published v0.1.0-rc2,
and only while the repository is public. A release from the private
repository needs GitHub access for its files and images
([installation.md, Private repository](docs/installation.md#private-repository)).

## First VM

```yaml
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
        sha256: <SHA-256 of the image>
```

```
$ kubectl get virtualmachines
NAME     READY   REASON        NODE    ADDRESS       AGE
debian   True    GuestBooted   node1   10.244.1.30   62s
```

Step by step, including how to pin the digest and reach the guest:
[installation.md](docs/installation.md#8-first-vm). When something does not
work: [troubleshooting.md](docs/troubleshooting.md).

## Compatibility

- Tested versions: [v0.1 support matrix](docs/compatibility.md#v01-support-matrix)
- Host, Kubernetes, CNI, CSI, and guest requirements:
  [compatibility.md](docs/compatibility.md)
- Which host and cluster versions work together:
  [version skew](docs/compatibility.md#version-skew)
- What happens to VMs when a node is cordoned, drained, rebooted, or
  upgraded: [lifecycle contract](docs/kairos-integration.md#lifecycle-contract)

## Documentation

| Document | Content |
|---|---|
| [installation.md](docs/installation.md) | install, verify, first VM, uninstall, upgrade |
| [troubleshooting.md](docs/troubleshooting.md) | diagnosing nodes and VMs; observing Baryon with kube-state-metrics |
| [release.md](docs/release.md) | versioning, release contents, verifying a release |
| [architecture.md](docs/architecture.md) | components, ownership, request flow |
| [api.md](docs/api.md) | API reference |
| [api-stability.md](docs/api-stability.md) | API contract, v0.1 freeze, change log per release |
| [compatibility.md](docs/compatibility.md) | v0.1 support matrix; host, Kubernetes, CNI, CSI, guest requirements; version skew and upgrades |
| [kairos-integration.md](docs/kairos-integration.md) | Hadron, Kairos, AuroraBoot, Kairos Operator, Cluster API; lifecycle contract and upgrade runbook |
| [hadron.md](docs/hadron.md) | the Baryon-enabled Hadron image |
| [networking.md](docs/networking.md), [storage.md](docs/storage.md) | data paths |
| [security.md](docs/security.md) | threat model and privileges |
| [runtime-boundary.md](docs/runtime-boundary.md) | experiments and evidence behind the runtime design |
| [design-notes.md](docs/design-notes.md) | open and answered design questions |
| [development.md](docs/development.md), [testing.md](docs/testing.md) | working on Baryon |
| [adr/](docs/adr/) | architecture decision records |

## Building

```bash
cargo test --workspace --locked
hack/build-static.sh        # static musl binaries
hack/build-hadron-image.sh <registry>/baryon-hadron:<tag>   # Baryon-enabled Hadron image (docs/hadron.md)
```

Building, deploying from a checkout, and running the tests are described in
[docs/development.md](docs/development.md) and
[docs/testing.md](docs/testing.md).

## Contributing

Contributions are accepted under the Developer Certificate of Origin: every
commit carries a `Signed-off-by` trailer, created with `git commit -s`.
Before sending a change, run the checks listed in
[docs/development.md](docs/development.md#build-and-check).

## License

Baryon is licensed under the Apache License, Version 2.0. See
[LICENSE](LICENSE) and [ADR 0010](docs/adr/0010-license.md).
