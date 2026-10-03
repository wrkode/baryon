# 0003: Host lifecycle belongs to Hadron and Kairos

Status: Accepted
Date: 2026-10-02, revised 2026-10-03

## Context

A virtualization host needs a specific kernel configuration, a VMM binary,
firmware, kernel modules loaded at boot, and a host runtime service. These
could be installed after boot (a privileged DaemonSet copying binaries), or
delivered as part of the operating system image.

Hadron is immutable once Kairos-ified: `/usr` is read-only, `/etc` and `/var`
are ephemeral overlays with selected persistent bind mounts, and the image is
replaced as a unit through A/B upgrades. Kairos already owns installation,
upgrades, rollback, recovery, and trusted boot. AuroraBoot already builds
bootable media. Kairos Operator already performs Kubernetes-driven upgrades.

The Hadron v0.5.3 default kernel configuration enables the required options
as modules: KVM, KVM_INTEL, KVM_AMD, VFIO, VFIO_PCI, IOMMUFD, TUN, VHOST_NET,
VHOST_VSOCK, NET_SCH_INGRESS, NET_CLS_U32, NET_ACT_MIRRED, NET_ACT_GACT;
HUGETLBFS,
INTEL_IOMMU, AMD_IOMMU, SECCOMP, and SECURITY_LANDLOCK are built in. Hadron
ships no modules-load.d configuration, no Cloud Hypervisor, no UEFI firmware
for VMs, no virtiofsd, and no `tc` binary.

## Decision

- The host artifact is a Baryon-enabled Hadron/Kairos OCI image. It contains
  Cloud Hypervisor, CLOUDHV.fd, virtiofsd, baryon-virtd, its systemd unit,
  module loading configuration, and a component version manifest.
- These are added as an image layer under `/usr`, before kairos-init runs, as
  upstream Hadron and Kairos recommend.
- Baryon never installs or modifies host software after boot.
- Bootable artifacts are produced by AuroraBoot. Baryon has no ISO builder or
  installer.
- OS upgrades are performed by Kairos (kairos-agent, Kairos Operator).
  Baryon will expose node evacuation state and perform VM evacuation, but it
  will not drive upgrades.
- Baryon keeps no state that must survive in the OS image across an upgrade.
  Runtime records live in `/run` and are rebuilt; logs live in `/var/log`.

## Consequences

- A host component change requires a new image and a Kairos upgrade. That is
  the intended cost of an immutable host.
- The Kubernetes provider (k3s, k0s, or provider-kubernetes) is chosen at
  image build time. The Baryon host layer is the same for all three, but that
  does not make all three supported: a node must meet the compatibility
  contract in [compatibility.md](../compatibility.md), including the kubelet
  systemd cgroup driver (ADR 0005). k0s is tested on three nodes and k3s
  on a single node; provider-kubernetes is not tested.
- The `cloud` Hadron kernel variant lacks VFIO, VHOST_NET, NET_SCHED, and
  Landlock and is not a supported Baryon host.
- Trusted boot images load the full root filesystem into RAM, so the size of
  the Baryon layer costs host memory on those nodes.

## Alternatives considered

- A privileged DaemonSet that installs Cloud Hypervisor and the host daemon
  onto any Linux node. Rejected: mutates an immutable host, competes with
  Kairos for host content, and breaks rollback semantics.
- Delivering the host layer only as a systemd-sysext. Deferred: Kairos
  supports sysexts, but they are not available in the initramfs, need
  signing under trusted boot, and add a second delivery path. The host layer
  is built `FROM scratch` so it can become a sysext later.

## Revisions

- 2026-10-03: the kernel option list was corrected (NET_CLS_U32 and
  NET_ACT_GACT, which the implementation uses); a common host layer was
  qualified as not implying support for every provider.
- 2026-10-03: the tested providers were updated (k0s on three nodes).
