# The Baryon-enabled Hadron image

Status: image build Implemented (`packaging/hadron/Dockerfile`). The k0s
variant runs on three nodes (the k0s cluster, VMs with nested KVM) and the
k3s variant on a single node; each was provisioned from an AuroraBoot raw
EFI disk with a Kairos cloud-config and runs VMs (Tested: E2E suites). Node
reboots with running VMs and manual Kairos A/B upgrades are Tested as
manual procedures on both; Kairos Operator `NodeOpUpgrade` of one node and
of all three nodes is Tested on the k0s cluster. Results are summarized in
[testing.md](testing.md#results). Facts about Hadron were checked against
kairos-io/hadron v0.5.3 and main (58ff499) on 2026-10-02.

## What Hadron provides

Hadron is built from source by the Kairos project: musl libc, systemd, no
runtime package manager. It is customized by OCI layering and turned into a
Kairos system by kairos-init. Its default kernel configuration (x86_64) has
what Baryon needs:

| Feature | Config | Baryon use |
|---|---|---|
| KVM, KVM_INTEL, KVM_AMD | m | VMs |
| TUN, VHOST_NET, VHOST_VSOCK, VSOCKETS | m | TAP, future vsock |
| NET_SCH_INGRESS, NET_CLS_U32, NET_ACT_MIRRED, NET_ACT_GACT | m | network passthrough |
| PACKET | y | DHCP responder |
| VFIO, VFIO_PCI, IOMMUFD | m | future device assignment |
| INTEL_IOMMU, AMD_IOMMU | y (Intel default off: needs `intel_iommu=on`) | future device assignment |
| HUGETLBFS, TRANSPARENT_HUGEPAGE | y | future hugepages |
| SECCOMP, SECURITY_LANDLOCK | y | VMM sandboxing |
| CGROUPS, cgroup v2 | y | placement |

`VFIO_DEVICE_CDEV` is not set, so future VFIO use goes through the legacy
`/dev/vfio/vfio` container interface. The `cloud` kernel variant lacks VFIO,
VHOST_NET, NET_SCHED, and Landlock and is not a supported Baryon host. On
arm64 the SMMU drivers are not enabled.

Hadron ships no Cloud Hypervisor, VM firmware, virtiofsd, or `tc`. Baryon adds
the first three and uses netlink instead of `tc`.

## What Baryon adds

The host layer (`packaging/hadron/Dockerfile`, stage `host-layer`, built
`FROM scratch`) contains only files under `/usr`:

| Path | Content | Source |
|---|---|---|
| `/usr/bin/cloud-hypervisor` | Cloud Hypervisor static build | upstream release, pinned and checksummed in `packaging/versions.env` |
| `/usr/share/cloud-hypervisor/CLOUDHV.fd` | UEFI firmware for Cloud Hypervisor | cloud-hypervisor/edk2 release, pinned and checksummed |
| `/usr/libexec/virtiofsd` | virtiofsd static build | upstream release, pinned and checksummed; not used yet |
| `/usr/bin/baryon-virtd` | Baryon host runtime, static musl | built from this repository |
| `/usr/share/baryon/components.json` | versions and checksums of the above | generated at build time |
| `/usr/lib/systemd/system/baryon-virtd.service` | systemd unit, no `[Install]` section | `packaging/hadron/files/` |
| `/usr/lib/systemd/system/multi-user.target.wants/baryon-virtd.service` | relative symlink that enables the unit | created in the image stage |
| `/usr/lib/modules-load.d/baryon.conf` | tun, vhost_net, vhost_vsock, sch_ingress, cls_u32, act_mirred, act_gact | `packaging/hadron/files/` |
| `/usr/lib/tmpfiles.d/baryon.conf` | `/var/log/baryon` (0750, root) and `/var/log/baryon/vms` (0700, root, entries removed after 14 days untouched) | `packaging/hadron/files/` |

Why only `/usr`: on Kairos `/usr` is replaced as a whole by each A/B upgrade,
while `/etc/systemd`, `/etc/modprobe.d`, and similar paths are persistent
bind mounts refreshed with update-only rsync, so files placed there by an
image survive later images and rollbacks. `/usr/local` is the persistent
partition and hides image content on installed systems.

baryon-virtd keeps its runtime records in `/run/baryon` (tmpfs, cleared on
reboot) and its logs in `/var/log/baryon` (persistent on Kairos). It needs no
other persistent state.

## Logs and cleanup

Each Anchor Pod gets its own log directory,
`/var/log/baryon/vms/<namespace>_<vm>_<pod-uid>/`, with the console output,
the Cloud Hypervisor log and output, and the DHCP responder log. Every start,
restart, and node reboot creates a new one. The tmpfiles.d entry
`d /var/log/baryon/vms 0700 root root 14d` lets `systemd-tmpfiles-clean`
(its timer is present on Hadron) remove entries older than 14 days. It ages
files individually, so baryon-virtd holds a shared BSD lock on the log
directory of every running VM; systemd-tmpfiles skips locked directories,
and the logs of a running VM are never removed however quiet its console
is. The lock is released when the VM ends.
Console output can contain credentials. To keep logs longer, override the
file with one of the same name in `/etc/tmpfiles.d`. Retention is Tested
manually on a single k3s node with a short override age (stale directories
removed, a running VM's locked directory kept; see
[testing.md](testing.md#results)).

Deleting a VirtualMachine removes the VirtualMachine, its Anchor Pod, the
root disk claim Baryon created for an `http` source (and with it the
imported image), the runtime directory under `/run/baryon/vms/`, the VMM and
DHCP responder processes, the VMM tmpfs, and the pod volumes. It keeps the
per-pod log directories until retention removes them, and claims named in
`rootDisk.source.persistentVolumeClaim`, which the user owns. The removal is
Tested ([testing.md](testing.md#what-deletion-removes)).

## Build

```bash
hack/build-hadron-image.sh <registry>/baryon/hadron:<tag>
```

Stages: verified downloads of the pinned artifacts, a static build of
baryon-virtd in `rust:1.99-alpine`, the host layer, then the image: the
Hadron base (`ghcr.io/kairos-io/hadron:v0.5.3`), the host layer, the unit
symlink with a layout check, and finally kairos-init
(`quay.io/kairos/kairos-init:v4.3.0`) with the Kubernetes provider:

```
/kairos-init -s install -m generic -t false -p k3s --provider-k3s-version <v> --version <baryon version>
/kairos-init -s init    -m generic -t false -p k3s --provider-k3s-version <v> --version <baryon version>
```

`KUBERNETES_DISTRO=k0s` selects k0s (set `KUBERNETES_VERSION` to the k0s
version); an empty value builds a core image without Kubernetes. For provider-kubernetes (kubeadm), copy the host layer
onto `ghcr.io/kairos-io/provider-kubernetes:<release>` instead; not tested.

kairos-init runs last because its install and init stages rebuild the initrd
from the final file layout.

The images used for the single-node k3s results were built this way with
k3s v1.36.4+k3s1 (`packaging/versions.env`); the images of the k0s cluster
with `KUBERNETES_DISTRO=k0s` and k0s v1.36.4+k0s.1. The first was turned into a
raw EFI disk; later images were installed by Kairos A/B upgrade, and a
freshly provisioned node was upgraded twice more (summary in
[testing.md](testing.md#results)).

## Bootable artifacts

AuroraBoot turns the image into bootable media. A raw EFI disk from an
image in the local Docker daemon, with the cloud-config and the output in
`<work-dir>`:

```bash
docker run --rm --privileged -v /var/run/docker.sock:/var/run/docker.sock -v "<work-dir>:/work" \
  quay.io/kairos/auroraboot:v0.27.1 \
  --set disable_http_server=true --set disable_netboot=true \
  --set disk.efi=true --set container_image=docker:<registry>/baryon/hadron:<tag> \
  --set state_dir=/work/aurora --cloud-config /work/cloud-config.yaml
```

This is the form used for the single-node k3s results and the k0s cluster.

ISO (`auroraboot build-iso`) and trusted boot UKI (`auroraboot build-uki`)
use the same image. Trusted boot is not tested: it loads the whole root file
system into RAM and requires signed system extensions.

## Boot sequence

Observed on a single k3s node booted from the raw EFI disk (the ordering in
step 2 is set by the unit file):

1. The first boot runs the recovery system, which installs the active system
   and reboots.
2. The active system boots. systemd starts baryon-virtd before k3s (the unit
   has `Before=k3s.service` and the equivalent k0s and kubelet units).
3. `/dev/kvm` appears shortly after baryon-virtd starts, when udev loads
   `kvm_intel`. baryon-virtd only warns at startup when `/dev/kvm` is missing,
   and host discovery runs on each node agent request, so this has no effect.
4. k3s starts, baryon-node-agent registers the device plugin, and the node
   accepts VMs.

## Node configuration

The node needs, through Kairos cloud-config or the provisioning system:

- a Kubernetes configuration (for k3s, a `k3s:` block; for k0s, a `k0s:`
  block and, on the controller, the k0s `ClusterConfig`)
- the node label `virtualization.baryon.io/host=true`, which selects the
  node for baryon-node-agent (for k3s: `--node-label` in `k3s.args`; for
  k0s: `--labels` in `k0s.args`)
- kubelet graceful node shutdown, so that a reboot stops Anchor Pods, and
  therefore guests, before the host goes down (see below)
- on k0s, the kubelet systemd cgroup driver and, for block-mode root disks,
  containerd device ownership ([compatibility.md](compatibility.md#k0s-node-configuration))

A k3s example, the form used for the single-node k3s results (add users,
SSH keys, and the rest of your node configuration):

```yaml
#cloud-config
k3s:
  enabled: true
  args:
    - --node-label=virtualization.baryon.io/host=true
stages:
  initramfs:
    - name: kubelet graceful node shutdown
      directories:
        - path: /var/lib/rancher/k3s/agent/etc/kubelet.conf.d
          permissions: 0700
      files:
        - path: /var/lib/rancher/k3s/agent/etc/kubelet.conf.d/90-graceful-shutdown.conf
          permissions: 0644
          content: |
            apiVersion: kubelet.config.k8s.io/v1beta1
            kind: KubeletConfiguration
            shutdownGracePeriod: 150s
            shutdownGracePeriodCriticalPods: 30s
```

A k0s example for the controller node, which also runs workloads, the form
used for the k0s cluster (add users, SSH keys, the API address, and the rest
of your node configuration). Workers use the same containerd stage, a
join token from the controller written to a file under `/etc/k0s`, and a
`k0s-worker:` block (`enabled: true`, `args: [--labels=virtualization.baryon.io/host=true]`);
they take the `default` worker profile from the controller.

```yaml
#cloud-config
stages:
  initramfs:
    - name: "containerd: systemd cgroups, device ownership"
      directories:
        - path: /etc/k0s/containerd.d
          permissions: 0755
      files:
        - path: /etc/k0s/containerd.d/baryon.toml
          permissions: 0600
          content: |
            version = 3
            [plugins."io.containerd.cri.v1.runtime"]
              device_ownership_from_security_context = true
            [plugins."io.containerd.cri.v1.runtime".containerd.runtimes.runc.options]
              SystemdCgroup = true
    - name: "k0s cluster configuration"
      directories:
        - path: /etc/k0s
          permissions: 0755
      files:
        - path: /etc/k0s/k0s.yaml
          permissions: 0600
          content: |
            apiVersion: k0s.k0sproject.io/v1beta1
            kind: ClusterConfig
            metadata:
              name: k0s
            spec:
              workerProfiles:
                - name: default
                  values:
                    cgroupDriver: systemd
                    shutdownGracePeriod: 150s
                    shutdownGracePeriodCriticalPods: 30s
k0s:
  enabled: true
  args:
    - --enable-worker
    - --no-taints
    - --config=/etc/k0s/k0s.yaml
    - --labels=virtualization.baryon.io/host=true
```

The worker profile and the node label were delivered this way on the k0s
cluster. The containerd drop-in was added to the nodes after provisioning
and took effect after a reboot; delivering it from the `initramfs` stage as
shown is not yet tested from first boot. Without it, k0s v1.36.4 runs pods
under cgroupfs and baryon-virtd refuses every VM
(`CgroupDriverUnsupported`). On the first boot of the active system the
Kairos k0s provider may start k0s before writing its configuration
([kairos-integration.md](kairos-integration.md#kairos-kubernetes-providers)).

### Graceful node shutdown

The Anchor Pod termination grace period is 90 s (60 s guest shutdown plus
30 s margin). kubelet gives regular pods `shutdownGracePeriod` minus
`shutdownGracePeriodCriticalPods`, so that difference must be at least 90 s.
Without graceful node shutdown, which is the k3s default, the guest is
stopped hard at reboot.

The Kubernetes 1.36 kubelet has no command-line flag for
`shutdownGracePeriod` (upstream registers none), so k3s started with
`--kubelet-arg=shutdown-grace-period=...` fails to start. Use a
KubeletConfiguration file. For k3s, a drop-in in
`/var/lib/rancher/k3s/agent/etc/kubelet.conf.d/` works:

```yaml
# /var/lib/rancher/k3s/agent/etc/kubelet.conf.d/90-graceful-shutdown.conf
apiVersion: kubelet.config.k8s.io/v1beta1
kind: KubeletConfiguration
shutdownGracePeriod: 150s
shutdownGracePeriodCriticalPods: 30s
```

With this file on the k3s node, kubelet held a systemd-logind delay inhibitor
(`InhibitDelayMaxSec=150`), and on reboot the guest shut down cleanly
(Tested, manual procedure). Baryon cannot observe whether a node has graceful
node shutdown configured; a VirtualizationNode condition for it is Planned.

The cloud-config above writes the file from the Kairos `initramfs` stage,
which runs after the persistent mounts and before k3s starts. A freshly
provisioned node had the file in place and the kubelet's shutdown inhibitor
active (150 s) with no manual step.

On k0s, `shutdownGracePeriod` and `shutdownGracePeriodCriticalPods` are set
in the worker profile of the k0s `ClusterConfig` (example above). On the
k0s cluster a node reboot with a running VM shut the guest down cleanly
(Tested, manual procedure). provider-kubernetes is not tested.

## Reboot behavior

Observed on a single k3s node with one VM running (Tested, manual
procedure; summary in [testing.md](testing.md#results)). On the k0s cluster
a reboot with graceful node shutdown behaved like the second row, and the
VM came back without intervention once the node was Ready.

| Configuration | Guest at reboot | Old Anchor Pod |
|---|---|---|
| No graceful node shutdown (k3s default) | stopped hard: the console log ends at the login prompt; the Cloud Hypervisor log shows the VM shut down at the second of the reboot | after boot, kubelet re-admits it before the device plugin has registered; admission fails with `UnexpectedAdmissionError` (no healthy devices for `virtualization.baryon.io/vm`) and the pod fails |
| Graceful node shutdown (drop-in above) | clean shutdown: the console shows `systemd-shutdown` and `reboot: Power down` | ended `Succeeded` |

In both cases baryon-controller replaces the finished pod with a new Anchor
Pod (new pod UID and IP, same MAC), which stays Pending until the device
plugin registers. The VM then boots from the same root disk claim, and the
guest answered ping and SSH from other pods (flannel). The reboots that
completed the later Kairos A/B upgrades used graceful node shutdown and
behaved the same way.

baryon-virtd exits 2 s after SIGTERM (`SHUTDOWN_GRACE`). Before that
change it waited for open gRPC streams (anchor sessions,
node agent watch) until systemd killed it at the 90 s stop timeout, whenever
it was restarted while VMs were running. Node reboots were not delayed: by
the time systemd stops baryon-virtd, the pods and with them the client
connections are gone.

## Upgrades

A new Baryon release means a new image, applied with Kairos: manually with
`kairos-agent upgrade` (Tested on a single k3s node and on all three nodes of
the k0s cluster, manual procedure), or with a Kairos Operator
`NodeOpUpgrade` (Tested on the k0s cluster for one node and for all three
nodes; runbook in
[kairos-integration.md](kairos-integration.md#cooperation-with-upgrades)). The node reboots into the new
image. VMs on it are stopped, not moved: with graceful node shutdown their
Anchor Pods shut the guests down first; without it the guests are stopped
hard. The reboot that completes an upgrade is a node reboot; the measured
behavior is in [Reboot behavior](#reboot-behavior). The upgrade procedure and
its result are in [kairos-integration.md](kairos-integration.md).
