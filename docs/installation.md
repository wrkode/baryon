# Installation

This guide takes a cluster from bare machines to a running VM with the
release artifacts of Baryon, and covers uninstalling and upgrading. It is
written for v0.1.0-rc3. Its release files and images are public: no
GitHub account or registry credentials are needed. The
[from a checkout](#from-a-checkout) path uses images you build and push
yourself instead.

Status labels follow [architecture.md](architecture.md). "Tested" names the
environment, described in [testing.md](testing.md#results); the k0s
cluster is three Baryon-enabled Hadron nodes running k0s.

Where to find the reasons behind each step:

| Topic | Document |
|---|---|
| Versions v0.1 is tested with | [compatibility.md, v0.1 support matrix](compatibility.md#v01-support-matrix) |
| Host, Kubernetes, CNI, CSI, and guest requirements | [compatibility.md](compatibility.md) |
| What the host image contains and how it is built | [hadron.md](hadron.md) |
| Kairos, AuroraBoot, Kairos Operator, and the lifecycle contract | [kairos-integration.md](kairos-integration.md) |
| The VirtualMachine API | [api.md](api.md) |
| When something does not work | [troubleshooting.md](troubleshooting.md) |

## 1. Prerequisites

- Machines: x86_64, with hardware virtualization exposed to the operating
  system (KVM; VT-x or AMD-V enabled in the firmware, or nested
  virtualization when the machines are themselves VMs), booting with UEFI.
  aarch64 is Not supported in v0.1.
- Tested topology: three nodes, one k0s controller that also runs
  workloads and two workers, all VMs with nested KVM. Other topologies and
  bare-metal machines: Not tested.
- Kubernetes: the tested distribution is k0s v1.36.4+k0s.1, which the
  published host image contains. Baryon needs Kubernetes 1.32 or later for
  its admission policies; only 1.36 is tested
  ([Kubernetes support policy](compatibility.md#kubernetes-support-policy-for-v01)).
- Tools on your workstation: `kubectl`, `docker` (for AuroraBoot), `ssh`,
  `curl`, and `sha256sum`; optionally `gh` (GitHub CLI) to verify
  attestations ([Verify the release](#verify-the-release)).
- Network: nodes pull images from `ghcr.io` without credentials, and VM
  disk imports download the image URL from an import Pod over the pod
  network.
- Access: none. The repository, the releases, and the images are public;
  no GitHub account is needed to install Baryon. Installing from a
  restricted mirror is covered in
  [Restricted registries and mirrors](#restricted-registries-and-mirrors).

Read the [v0.1 support matrix](compatibility.md#v01-support-matrix) before
choosing components. Only rows marked Tested there are support claims.

## 2. Obtain the host image

Every Baryon node runs the Baryon-enabled Hadron image: Hadron, Kairos,
k0s, Cloud Hypervisor, its UEFI firmware, and baryon-virtd in one immutable
OCI image ([hadron.md](hadron.md)).

Option A, the published image (k0s v1.36.4+k0s.1):

```bash
docker pull ghcr.io/wrkode/baryon-hadron:v0.1.0-rc3-k0s
```

Option B, build it from a checkout:

```bash
git clone https://github.com/wrkode/baryon.git
cd baryon
git checkout v0.1.0-rc3
hack/build-hadron-image.sh <registry>/baryon-hadron:v0.1.0-rc3-k0s
```

- The script needs Docker with BuildKit. It installs k0s by default
  (`KUBERNETES_DISTRO=k0s`); every input (Hadron, kairos-init, k0s, Cloud
  Hypervisor, firmware, virtiofsd) is pinned with its checksum in
  `packaging/versions.env`.
- Each build compiles baryon-virtd from the checkout and stamps the commit
  into the binary and into `/usr/share/baryon/components.json`. A tree with
  uncommitted changes to tracked files gets a `-dirty` suffix. A build that
  stands for a release must come from a clean checkout of its tag.
- Stages and options: [hadron.md, Build](hadron.md#build).

Compare the image digest with the host entry of `images.txt`, after
verifying `images.txt` with `SHA256SUMS`, and verify the image's build
provenance ([Verify the release](#verify-the-release)).

## 3. Provision the nodes

### Bootable media

AuroraBoot turns the image into bootable media. A raw EFI disk from the
image in the local Docker daemon, with the cloud-config and the output in
`<work-dir>`:

```bash
docker run --rm --privileged -v /var/run/docker.sock:/var/run/docker.sock -v "<work-dir>:/work" \
  quay.io/kairos/auroraboot:v0.27.1 \
  --set disable_http_server=true --set disable_netboot=true \
  --set disk.efi=true --set container_image=docker:ghcr.io/wrkode/baryon-hadron:v0.1.0-rc2-k0s \
  --set state_dir=/work/aurora --cloud-config /work/cloud-config.yaml
```

This is the form used for the k0s cluster (AuroraBoot v0.27.1, Tested). The
raw disk is written under `<work-dir>/aurora`. Each node needs its own
cloud-config, so run AuroraBoot once per node, or once per role if the
cloud-configs of the workers are identical. AuroraBoot can also build an
ISO from the same image (`build-iso`; Not tested with Baryon); see the
AuroraBoot documentation of the Kairos project and
[hadron.md, Bootable artifacts](hadron.md#bootable-artifacts).

On the first boot, the machine runs the Kairos recovery system, which
installs the active system and reboots into it.

### Cloud-config

Each node gets a Kairos cloud-config. Why each setting is needed is in
[hadron.md, Node configuration](hadron.md#node-configuration) and
[compatibility.md, k0s node configuration](compatibility.md#k0s-node-configuration).
In short:

- the containerd drop-in sets the systemd cgroup driver (baryon-virtd
  refuses VMs under cgroupfs, `CgroupDriverUnsupported`) and device
  ownership for raw block root disks (`BlockDeviceAccessDenied` without
  it)
- the k0s worker profile sets the kubelet systemd cgroup driver and
  graceful node shutdown, so a reboot shuts guests down cleanly
- the node label `virtualization.baryon.io/host=true` selects the node for
  the baryon-node-agent DaemonSet (its `nodeSelector`); a node without it
  runs no VMs

Provision the controller node first. Its cloud-config, with placeholders
for your user, key, and the controller's address:

```yaml
#cloud-config
users:
  - name: <user>
    groups: [admin]
    ssh_authorized_keys:
      - "<ssh-public-key>"
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
              api:
                address: <controller-address>
                sans: [<controller-address>]
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

When the controller runs, create a join token for the workers on it:

```bash
ssh <user>@<controller-address> sudo k0s token create --role=worker
```

Each worker's cloud-config writes that token to `/etc/k0s/token` and takes
the `default` worker profile from the controller:

```yaml
#cloud-config
users:
  - name: <user>
    groups: [admin]
    ssh_authorized_keys:
      - "<ssh-public-key>"
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
    - name: "k0s join token"
      directories:
        - path: /etc/k0s
          permissions: 0755
      files:
        - path: /etc/k0s/token
          permissions: 0600
          content: |
            <worker-join-token>
k0s-worker:
  enabled: true
  args:
    - --token-file=/etc/k0s/token
    - --labels=virtualization.baryon.io/host=true
```

The join token grants access to the cluster: keep the worker
cloud-configs and the media built from them private.

Status: the worker profile, the node label, the join token file, and the
k0s arguments were delivered this way on the k0s cluster (Tested). The
containerd drop-in was added to the nodes after provisioning, followed by a
reboot; delivering it from the `initramfs` stage as shown is Not tested
from first boot. Check it after the first boot with
`sudo cat /etc/k0s/containerd.d/baryon.toml` on each node.

### Known issue on the first boot of the active system

Observed on the k0s cluster with the Kairos k0s provider of kairos-init
v4.3.0: on the first boot of the active system, the `k0scontroller` or
`k0sworker` service can start before the provider has written its systemd
override (arguments and token file). The controller then runs without
`--enable-worker` and does not appear as a node, or a worker fails with
"no join token". Later boots are not affected. Fix it once on the
affected node:

```bash
sudo systemctl daemon-reload
sudo systemctl restart k0scontroller   # on the controller
sudo systemctl restart k0sworker       # on a worker
```

See also [troubleshooting.md](troubleshooting.md#k0s-starts-without-its-configuration-on-the-first-boot).

## 4. Form the cluster

Get the admin kubeconfig from the controller and check the nodes:

```bash
ssh <user>@<controller-address> sudo k0s kubeconfig admin > kubeconfig
chmod 600 kubeconfig
export KUBECONFIG=$PWD/kubeconfig
kubectl get nodes -L virtualization.baryon.io/host
```

The `server:` entry in the kubeconfig must be an address your workstation
reaches. Every node must be `Ready` and show `true` in the `HOST` column.
The controller node is listed only when k0s runs with `--enable-worker`
(see the known issue above).

## 5. Storage

Baryon does not install storage. Root disks are PersistentVolumeClaims, so
the cluster needs a CSI driver that meets the
[CSI requirements](compatibility.md#csi-requirements). Tested on the k0s
cluster:

- OpenEBS LVM LocalPV (lvm-driver 1.10.1): node-local volumes; a VM on it
  starts again only on its node.
- Ceph RBD through Rook (Rook v1.20.8, ceph-csi v3.17.1): a VM on it can
  start on another node.

Exact versions are in the
[support matrix](compatibility.md#v01-support-matrix). Install the driver
following its own documentation. On k0s the kubelet root directory is
`/var/lib/k0s/kubelet`, and CSI node plugins must be configured with it.

Baryon creates the root disk claim of a VM with an `http` source in the
StorageClass named by `rootDisk.storageClassName`, or in the default
StorageClass when that field is unset. To mark a StorageClass as the
default:

```bash
kubectl patch storageclass <name> -p '{"metadata":{"annotations":{"storageclass.kubernetes.io/is-default-class":"true"}}}'
```

## 6. Install Baryon

The release manifests contain everything `deploy/base` installs: the CRDs,
the `baryon-system` namespace, baryon-controller, the baryon-node-agent
DaemonSet, RBAC, the admission policies, and the roles for namespace users.
The component images are pinned by digest. The admission policies are part
of the supported installation and need Kubernetes 1.32 or later
([security.md](security.md#admission-policies)).

### From the release

```bash
kubectl apply --server-side -f https://github.com/wrkode/baryon/releases/download/v0.1.0-rc3/baryon.yaml
kubectl apply -f https://github.com/wrkode/baryon/releases/download/v0.1.0-rc3/baryon-classes.yaml
```

The nodes pull the images from `ghcr.io` by digest, without credentials.
To check the files before applying them, use
[Verify the release](#verify-the-release) instead.

`baryon-classes.yaml` holds the example VirtualMachineClasses
`general-small` (2 vCPUs, 2Gi) and `general-medium` (4 vCPUs, 8Gi). Classes
are immutable; write your own in the same shape ([api.md](api.md#virtualmachineclass-cluster-scoped-short-name-bvmclass)).

### Verify the release

The GitHub release (a prerelease for `-rc` tags) carries:

| File | Content |
|---|---|
| `baryon.yaml` | everything `deploy/base` installs, component images pinned by digest |
| `baryon-crds.yaml` | the CRDs alone, for the first step of an upgrade |
| `baryon-classes.yaml` | the example VirtualMachineClasses |
| `baryon-virtd-v0.1.0-rc3-x86_64-linux-musl` | the static baryon-virtd binary, identical to the one in the host image |
| `baryon-v0.1.0-rc3-source.tar.gz` | the source tree of the tag |
| `images.txt` | the controller, node agent, anchor, importer, and host image references with digests |
| `*.spdx.json` | SPDX SBOMs of the source tree, of the baryon-virtd binary, and of each image by digest, with the Rust crates of the Baryon binaries |
| `SHA256SUMS` | SHA-256 checksums of the files above |

Download the files and check the checksums (no account needed):

```bash
base=https://github.com/wrkode/baryon/releases/download/v0.1.0-rc3
for f in baryon.yaml baryon-crds.yaml baryon-classes.yaml images.txt SHA256SUMS; do
  curl -fsSLO "$base/$f"
done
sha256sum --check --ignore-missing SHA256SUMS
```

The checksums come from the same release, so they detect a corrupted
download, not a substituted release. Build provenance does: every file
and image has a GitHub artifact attestation that ties its digest to the
release workflow, the tag, and the commit of `wrkode/baryon`. Verify the
files and the images (the commit is the one the tag points to; the
commands are in [release.md](release.md#verifying-a-release), including a
way without a GitHub login):

First check the tag: its signature against the maintainer's key pinned
in [release.md](release.md#tags-and-releases), and that its commit is on
`main` (the commands are in
[release.md](release.md#verifying-a-release)). Then, with the commit of
the tag:

```bash
v=v0.1.0-rc3
c=<commit of the tag, from the check above>
opts=(--repo wrkode/baryon --signer-workflow wrkode/baryon/.github/workflows/release.yaml
      --source-ref "refs/tags/$v" --source-digest "$c" --deny-self-hosted-runners)
gh attestation verify SHA256SUMS "${opts[@]}"
gh attestation verify baryon.yaml "${opts[@]}"
for ref in $(sed 's/^[^:]*: //' images.txt); do
  gh attestation verify "oci://$ref" "${opts[@]}"
done
```

Then apply the verified files:

```bash
kubectl apply --server-side -f baryon.yaml
kubectl apply -f baryon-classes.yaml
```

Status: the release workflow (`.github/workflows/release.yaml`) and the
manifest renderer (`hack/release-manifests.sh`) are Implemented. An
installation from release files rendered by the same procedure, applied as
files, is Tested on the k0s cluster in the v0.1.0-rc1 release validation
([testing.md](testing.md#v010-rc1-release-validation)) and the v0.1.0-rc2
release qualification ([testing.md](testing.md#v010-rc2-release-qualification)).
The published release files and images of v0.1.0-rc1 and of v0.1.0-rc2,
rebuilt by the release workflow from their tags, passed a smoke test on
the k0s cluster after each tag
([v0.1.0-rc1](testing.md#published-artifacts-smoke-test),
[v0.1.0-rc2](testing.md#published-v010-rc2-artifacts-smoke-test)).

### Restricted registries and mirrors

The supported installation pulls the images from `ghcr.io` anonymously.
A cluster that cannot reach `ghcr.io`, or that must use a mirror, needs the
images under their digests: `baryon.yaml` refers to them by digest, and
baryon-controller creates Anchor Pods and import Pods in the VM's
namespace without `imagePullSecrets`. Two ways:

- A registry mirror configured in each node's containerd for `ghcr.io`,
  which serves the same manifests (the digests do not change when a
  registry copies an image with its manifest). Status: Not tested.
- Load the images onto every node under their digest references. An image
  saved by Docker's classic image store has a different manifest, and an
  image known only by a tag name is not found, so kubelet tries to pull.
  One way that keeps the digest, with the `oras` CLI and the k0s `ctr`;
  repeat for the other component images and for every node, with the
  references from `images.txt`:

```bash
ref=ghcr.io/wrkode/baryon-controller@sha256:<digest>
oras copy "$ref" --to-oci-layout controller:v0.1.0-rc3
(cd controller && tar cf ../controller.tar .)
ssh <user>@<node-address> sudo k0s ctr -n k8s.io images import \
  --base-name ghcr.io/wrkode/baryon-controller - < controller.tar
ssh <user>@<node-address> sudo k0s ctr -n k8s.io images tag \
  ghcr.io/wrkode/baryon-controller:v0.1.0-rc3 "$ref"
```

Status: loading the images onto the nodes is Tested (the smoke tests of
v0.1.0-rc1 and v0.1.0-rc2, published while the repository was private; in
the v0.1.0-rc2 smoke test, this procedure was run as written for the
importer image on one node). The host image for a Kairos Operator upgrade
can be loaded the same way under the tag that the `NodeOpUpgrade` names.

### From a checkout

Build the static binaries and the component images, and push them
to a registry the nodes can pull from:

```bash
hack/build-static.sh
for c in anchor importer node-agent controller; do
  docker build -f "packaging/images/$c.Dockerfile" -t "<registry>/baryon-$c:<tag>" .
  docker push "<registry>/baryon-$c:<tag>"
done
```

Render the manifests with your images and apply them:

```bash
CONTROLLER_IMAGE=<registry>/baryon-controller:<tag> \
NODE_AGENT_IMAGE=<registry>/baryon-node-agent:<tag> \
ANCHOR_IMAGE=<registry>/baryon-anchor:<tag> \
IMPORTER_IMAGE=<registry>/baryon-importer:<tag> \
  hack/release-manifests.sh _out
kubectl apply --server-side -f _out/baryon.yaml
kubectl apply -f _out/baryon-classes.yaml
```

`hack/release-manifests.sh` needs `kubectl` (for `kubectl kustomize`) and
refuses output that still contains a development image reference. Instead,
you can set the images in `deploy/base/controller/deployment.yaml` (the
controller image and the `BARYON_ANCHOR_IMAGE` and `BARYON_IMPORTER_IMAGE` values) and
`deploy/base/node-agent/daemonset.yaml` and run
`kubectl apply --server-side -k deploy/base`
([development.md](development.md#deploying-to-a-cluster-of-hadron-nodes)).
The nodes must run a host image built from the same release or later
([minimum host image](compatibility.md#which-combinations-work)).

## 7. Verify the installation

The controller and one node agent per Baryon node run:

```bash
kubectl -n baryon-system get pods -o wide
```

baryon-virtd runs on each node as a systemd service of the host image:

```bash
ssh <user>@<node-address> systemctl status baryon-virtd
```

Each Baryon node has a VirtualizationNode that is `Ready` with reason
`RuntimeReady`, and a recent heartbeat:

```
$ kubectl get virtualizationnodes
NAME    READY   REASON         VMS   RUNTIME     HYPERVISOR   HEARTBEAT   AGE
node1   True    RuntimeReady   0     0.1.0-rc2   v54.0        12s         12m
node2   True    RuntimeReady   0     0.1.0-rc2   v54.0        9s          12m
node3   True    RuntimeReady   0     0.1.0-rc2   v54.0        21s         12m
```

The v0.1.0-rc2 host image ships Cloud Hypervisor v54.0 (v0.1.0-rc1:
v53.0).

Each Baryon node advertises VM and import slots:

```bash
kubectl get nodes -o custom-columns='NAME:.metadata.name,VMS:.status.allocatable.virtualization\.baryon\.io/vm,IMPORTS:.status.allocatable.virtualization\.baryon\.io/import'
```

A node that is missing, not `Ready`, or advertises no slots:
[Is this node virtualization-capable?](troubleshooting.md#is-this-node-virtualization-capable)

## 8. First VM

Pin the guest image by its SHA-256, so that the importer refuses any other
content. The Debian 13 cloud image is the tested guest; its URL is a
`latest` path whose content changes when Debian publishes a new build, so
compute the digest right before creating the VM:

```bash
url=https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.raw
curl -fsSL "$url" | sha256sum
```

`examples/debian-vm.yaml` is the same VM without the digest. Save this
manifest, with the digest, as `debian-vm.yaml`:

```yaml
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata:
  name: debian
  namespace: default
spec:
  className: general-small
  rootDisk:
    size: 10Gi
    source:
      http:
        url: https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.raw
        sha256: <64 hex characters from sha256sum>
```

Apply it and wait for the guest to boot. The import Pod `debian-import`
downloads the image into the root disk claim first; then the Anchor Pod
`debian-anchor` starts the VM:

```bash
kubectl apply -f debian-vm.yaml
kubectl get virtualmachine debian -w
kubectl wait virtualmachine/debian --for=condition=Ready --timeout=15m
```

`Ready` True (`GuestBooted`) means the guest acquired its address over
DHCP or showed a console login prompt. Read its address, which is the
Anchor Pod's IP:

```bash
kubectl get virtualmachine debian -o jsonpath='{.status.addresses[0].address}{"\n"}'
```

Reach it from a pod. The Debian cloud image runs an SSH server, so its
banner shows that the guest answers on the pod network:

```bash
addr="$(kubectl get virtualmachine debian -o jsonpath='{.status.addresses[0].address}')"
kubectl run probe --rm -it --restart=Never --image=busybox:1.36 -- sh -c "echo | nc -w 3 $addr 22"
```

The output starts with `SSH-2.0-`. The v1alpha1 API has no field for guest
credentials or cloud-init user data, and the Debian cloud image has no
password login; to log in, use an image that contains your credentials.
Selecting VMs with a Service: [networking.md](networking.md#selecting-vms-by-their-labels).

To stop, start, and restart the VM: set `spec.runPolicy` to `Stopped` or
`Running`, or set the `virtualization.baryon.io/restartedAt` annotation
([api.md](api.md#restarting-a-vm)).

## 9. Uninstall

Status: Tested on the k0s cluster, both with `kubectl delete -k
deploy/base` for an installation from a checkout and with the release file
`baryon.yaml` in the v0.1.0-rc1 release validation, the VMs deleted first
each time: afterwards the `baryon-system` namespace and the CRDs were
gone, no Baryon pods were left, no runtime directories were left on the
nodes, and baryon-virtd was still active as a host service. A clean
install from the release files afterwards worked
([testing.md](testing.md#v010-rc1-release-validation)). Deleting a
VirtualMachine and what that removes is Tested
([testing.md](testing.md#what-deletion-removes)).

1. Delete every VirtualMachine while baryon-controller still runs. Each VM
   carries the finalizer `virtualization.baryon.io/anchor-cleanup`, which
   only baryon-controller removes after the VM's Anchor Pod is gone; with
   the controller deleted first, the VirtualMachines and the CRD deletion
   wait on it.

   ```bash
   kubectl delete virtualmachines --all --all-namespaces
   kubectl get virtualmachines --all-namespaces
   ```

   Continue when the list is empty.

2. Delete the Baryon components, with the file you installed:

   ```bash
   kubectl delete -f baryon.yaml
   ```

   (`kubectl delete -k deploy/base` for an installation from a checkout.)
   This removes the CRDs and with them the VirtualMachineClasses and
   VirtualizationNodes.

What remains:

- Claims named in `rootDisk.source.persistentVolumeClaim`: they belong to
  the user and are never deleted by Baryon. Root disk claims that Baryon
  created for `http` sources are deleted with their VM; whether the
  volume behind them is kept depends on the StorageClass reclaim policy.
- On each node: the per-VM log directories under `/var/log/baryon/vms/`
  (console output, Cloud Hypervisor and DHCP responder logs), removed by
  `systemd-tmpfiles-clean` 14 days after their last change
  ([hadron.md](hadron.md#logs-and-cleanup)). Console output can contain
  credentials.
- The host image: baryon-virtd, Cloud Hypervisor, and the firmware are part
  of the Baryon-enabled Hadron image and keep running without VMs. Removing
  them means moving the node to another image through Kairos.
- The node label `virtualization.baryon.io/host=true` and the CSI driver.
- On each Node object, the extended resources `virtualization.baryon.io/vm`
  and `virtualization.baryon.io/import`, at 0 once the device plugin has
  unregistered (observed on the k0s cluster). kubelet keeps them; they do
  not affect other pods.

## 10. Upgrading

Baryon has two parts that are upgraded separately
([version skew](compatibility.md#version-skew)):

- the cluster release: CRDs, baryon-controller, baryon-node-agent, the
  anchor and importer images, RBAC, admission policies; upgraded with
  `kubectl apply`
- the host image: the Baryon-enabled Hadron image with baryon-virtd and
  Cloud Hypervisor; upgraded through Kairos, one node at a time

Read the release notes and the
[API change log](api-stability.md#change-log-per-release) first. The
upgrade from 0.1.0-rc2 to 0.1.0-rc3, and from 0.1.0-rc1 to 0.1.0-rc2, is
the procedure below; the CRDs and classes do not change. From 0.1.0-rc1 to
0.1.0-rc3, go through 0.1.0-rc2; the direct upgrade is Not tested. For the
upgrade from an earlier commit to 0.1.0-rc1, follow
[Upgrade to 0.1.0-rc1](compatibility.md#upgrade-to-010-rc1).

Cluster release, in this order and close together:

```bash
base=https://github.com/wrkode/baryon/releases/download/<new-version>
kubectl apply --server-side -f "$base/baryon-crds.yaml"   # 1. CRDs
kubectl apply -f <your-classes>.yaml                       # 2. classes, only if the release changes their shape
kubectl apply --server-side -f "$base/baryon.yaml"         # 3. controller, node agent, policies
```

Host images: one Kairos Operator `NodeOpUpgrade` per node, with
`spec.image` set to the new host image (for example
`ghcr.io/wrkode/baryon-hadron:<new-version>-k0s`), starting the next node
only after the previous node and its VMs are back. The procedure is the
[runbook](kairos-integration.md#runbook-upgrade-one-node-with-kairos-operator);
what happens to VMs is in the
[lifecycle contract](kairos-integration.md#lifecycle-contract). VMs on the
node are stopped and start again where their storage allows; live
migration is not supported.

Order between the two: within a runtime protocol major version, either part
can go first, and mixed host versions are allowed during the rollout
([upgrade order](compatibility.md#upgrade-order)). The minimum host image of
each cluster release is in
[compatibility.md](compatibility.md#which-combinations-work); a host older
than that lacks host-side safety fixes, so finish the host rollout up to
it. For the 0.1.0-rc3 cluster release the minimum is the 0.1.0-rc2 host
image; moving hosts to the 0.1.0-rc3 host image is optional (it changes
only the build) and stops the VMs of each node in turn.

Status:

- The order CRDs, classes, deployment: Tested on the k0s cluster, from a
  checkout, and in the v0.1.0-rc1 release validation with release files,
  from a release-like build of 8db33bb to one of c3a3735; running VMs kept
  running ([testing.md](testing.md#v010-rc1-release-validation)).
- Host images with Kairos Operator v0.2.2, one `NodeOpUpgrade` per node and
  the runbook including its step 5 check: Tested on all three nodes of the
  k0s cluster in the release validation, with the upgrade image preloaded
  on each node and a new VM started while the hosts were mixed. Pulling
  the host image from `ghcr.io` during the upgrade: Not tested.

### Rollback

- Rolling the cluster release back from 0.1.0-rc1 to an earlier commit: Not
  supported. Earlier controllers cannot read the 0.1.0-rc1 class shape
  (`cpu.vcpus`) or the 0.1.0-rc1 VirtualizationNode status
  ([Upgrade to 0.1.0-rc1](compatibility.md#upgrade-to-010-rc1)). Observed
  in the release validation, with the earlier manifests applied over the
  0.1.0-rc1 CRDs and classes: the earlier controller's reconciles and
  watches failed, the earlier node agents could not publish, so every
  VirtualizationNode stayed `Ready` Unknown (`NodeAgentStopped`) with a
  stale heartbeat, and a VM created meanwhile got no status and no pods.
  Running VMs kept running with the same Anchor Pods. Recovery: apply the
  0.1.0-rc1 `baryon.yaml` again; every node was Ready again, the waiting
  VM started, and the Anchor Pods of running VMs were unchanged.
- Rolling the cluster release back from 0.1.0-rc2 to 0.1.0-rc1: Not
  tested. The CRDs and classes are the same; the 0.1.0-rc1 controller runs
  imports with the anchor image, and the policies of 0.1.0-rc2 that the
  0.1.0-rc1 `baryon.yaml` does not contain stay installed until they are
  deleted.
- Rolling a host image back through Kairos A/B within the same runtime
  protocol major version: any 1.x host image works with any 1.x cluster
  release (ADR 0012); the older image lacks the host-side safety fixes of
  newer releases. The one-shot fallback boot is Tested in the release
  validation: drain the node, select the fallback entry for the next
  boot (it sets `next_entry` in `/oem/grubenv`), reboot, and uncordon the
  node when it is Ready:

  ```bash
  kubectl drain <node> --ignore-daemonsets
  ssh <user>@<node-address> sudo kairos-agent bootentry --select fallback
  ssh <user>@<node-address> sudo systemctl reboot
  kubectl uncordon <node>
  ```

  The node
  booted its previous host image, its VMs came back, and a new VM started
  there under the 0.1.0-rc1 cluster release; the next reboot returned to
  the active image. An automatic fallback after a failed upgrade boot is
  Not tested ([lifecycle contract](kairos-integration.md#lifecycle-contract)).
