# Development

## Prerequisites

- Rust: the toolchain is pinned in `rust-toolchain.toml` (1.99.0); rustup
  installs it on first use.
- Docker, for static musl builds (`rust:<toolchain>-alpine`), container
  images, and the Hadron image.
- kubectl and access to a cluster of Baryon-enabled Hadron nodes for the
  E2E tests ([testing.md](testing.md#environments)).

Building and the unit tests need no KVM. No `protoc` is needed; protobuf
files are compiled with protox.

## Repository layout

| Path | Content |
|---|---|
| `crates/` | Rust workspace members (see [architecture.md](architecture.md)) |
| `deploy/` | CRDs, namespace, controller and node agent manifests |
| `deploy/base/policy/` | ValidatingAdmissionPolicies: `vm-resource.yaml`, `node-agent.yaml`, `controller-writes.yaml`, `import-approval.yaml` (Kubernetes 1.30 or later; 1.32 or later for the node agent policy) |
| `deploy/base/tenant/` | ClusterRoles for namespace users, aggregated into `edit`, `admin`, and `view` |
| `packaging/images/` | container images for anchor, node agent, controller |
| `packaging/hadron/` | Baryon-enabled Hadron image |
| `packaging/versions.env` | pinned host components and checksums |
| `hack/` | build and check scripts |
| `hack/ci/` | CI API server checks against a single-node k0s container |
| `tests/e2e/` | end-to-end tests against a running cluster |
| `examples/` | example VirtualMachineClass and VirtualMachine objects |
| `docs/` | documentation and ADRs |

## Build and check

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
hack/check-text-style.sh
hack/verify-crds.sh
```

### CRDs

After changing API types, regenerate the CRDs:

```bash
cargo run -p baryon-api --bin baryon-crdgen > deploy/base/crds/virtualization.baryon.io.yaml
```

`hack/verify-crds.sh` fails when the file is stale. To run the API server
checks locally the way CI does (needs Docker, no KVM):

```bash
hack/ci/k0s.sh start
hack/ci/apiserver-checks.sh
hack/ci/k0s.sh stop
```

### Static binaries

Static binaries for images and hosts (Hadron is musl-only):

```bash
hack/build-static.sh                 # all binaries
hack/build-static.sh baryon-virtd    # one package
```

Output: `target/static/x86_64-unknown-linux-musl/release/`.

### Container images

The anchor, node agent, and controller images are built from the
repository root:

```bash
for c in anchor node-agent controller; do
  docker build -f "packaging/images/$c.Dockerfile" -t "<registry>/baryon/$c:<tag>" .
done
```

Push them to a registry that the cluster nodes can pull from.

### Baryon-enabled Hadron image

```bash
hack/build-hadron-image.sh <registry>/baryon/hadron:<tag>
```

The script reads the pinned versions from `packaging/versions.env` and
builds `packaging/hadron/Dockerfile`. `KUBERNETES_DISTRO` selects the
Kubernetes provider that kairos-init installs: `k0s` (default, the tested
distribution, version from `K0S_VERSION`), `k3s` (version from
`K3S_VERSION`; not supported in v0.1), or an empty value for a core image
without Kubernetes. `KUBERNETES_VERSION` overrides the pinned version:

```bash
KUBERNETES_DISTRO=k3s hack/build-hadron-image.sh <registry>/baryon/hadron:<tag>
```

Turning the image into bootable media with AuroraBoot, node configuration,
and upgrades are described in [hadron.md](hadron.md) and
[kairos-integration.md](kairos-integration.md).

## Deploying to a cluster of Hadron nodes

Prerequisites: every Baryon node runs the Baryon-enabled Hadron image, has
the label `virtualization.baryon.io/host=true`, uses the kubelet systemd
cgroup driver, and has graceful node shutdown configured; for block-mode
root disks, containerd also needs `device_ownership_from_security_context =
true` ([compatibility.md](compatibility.md); k0s:
[compatibility.md](compatibility.md#k0s-node-configuration)).

1. Set the image references in `deploy/base/controller/deployment.yaml`
   (the controller image and the `BARYON_ANCHOR_IMAGE` value) and in
   `deploy/base/node-agent/daemonset.yaml` to the images you pushed (or use
   a kustomize overlay with an `images:` entry).
2. Apply the manifests (CRDs, namespace, node agent, controller, admission
   policies, roles for namespace users) and the example classes:

   ```bash
   kubectl apply --server-side -k deploy/base
   kubectl apply -f examples/classes.yaml
   ```

3. Check that every Baryon node reports a VirtualizationNode and
   advertises the `virtualization.baryon.io/vm` and
   `virtualization.baryon.io/import` resources:

   ```bash
   kubectl get virtualizationnodes
   kubectl get nodes -o custom-columns='NAME:.metadata.name,VMS:.status.allocatable.virtualization\.baryon\.io/vm,IMPORTS:.status.allocatable.virtualization\.baryon\.io/import'
   ```

The admission policies in `deploy/base/policy/` are part of the supported
installation ([security.md](security.md#admission-policies)). With
`baryon-vm-resource` installed, only baryon-controller can create pods that
request `virtualization.baryon.io/vm` or `virtualization.baryon.io/import`;
`baryon-controller-writes` limits baryon-controller's PersistentVolume and
pod updates to the import record and the import approval; and
`baryon-import-approval` lets only baryon-controller approve an import.

baryon-virtd ships in the host image and is updated with a Kairos A/B
upgrade, not by copying binaries onto a node. Running VMs keep running
across a baryon-virtd restart. baryon-virtd exits 2 s after SIGTERM
(`SHUTDOWN_GRACE` in `crates/virtd/src/main.rs`) even while anchor sessions
and the node agent watch are open; the anchors reconnect.

## Places on a Baryon node

| Path | Content |
|---|---|
| `journalctl -u baryon-virtd` | host runtime log (JSON) |
| `/usr/share/baryon/components.json` | versions and checksums of the host components, and the Baryon `gitCommit` the image was built from |
| `/run/baryon/vms/<pod-uid>/record.json` | runtime record of each VM (mode 0600) |
| `/run/baryon/vms/<pod-uid>/ctl/` | root-only (0700): `api.sock` (Cloud Hypervisor API), `cloud-hypervisor.out` (VMM stdout and stderr), `dhcp.log` (DHCP responder log: a line per responder start and per reported lease acknowledgement, repeats within 60 s at debug level only; not size-capped while the VM runs) |
| `/run/baryon/vms/<pod-uid>/vmm/` | a tmpfs of its own (32 MiB, 256 inodes, mode 0700, owned by the VM's identity, `nosuid,nodev,noexec`), unmounted when the VM ends: serial socket, event file, `cloud-hypervisor.log` |
| `/var/log/baryon/vms/<ns>_<vm>_<pod-uid>/console.log` | guest serial console (mode 0600, directory 0700) |
| `/var/log/baryon/vms/<ns>_<vm>_<pod-uid>/` | also the last 16 MiB of `cloud-hypervisor.log`, `cloud-hypervisor.out`, and `dhcp.log`, copied when the VM stops (mode 0600); on Hadron removed after 14 days untouched |

These paths are internal to baryon-virtd and are not part of the API.
The VMM owns `vmm/` and can replace files there; baryon-virtd never follows
symlinks in it and reads it only with bounded reads
(`crates/virtd/src/vmm_files.rs`).

## Writing rules

Code comments, documentation, ADRs, and commit messages use ASCII only: no
emojis, no em dashes, no section signs. `hack/check-text-style.sh` enforces
this in CI. Feature statements use the status labels defined in
[architecture.md](architecture.md).

## Contributing

Baryon is licensed under Apache-2.0 (ADR 0010). Every commit carries a DCO
`Signed-off-by` trailer; create it with `git commit -s`.
