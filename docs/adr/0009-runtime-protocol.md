# 0009: gRPC over Unix sockets between node components and baryon-virtd

Status: Accepted
Date: 2026-10-02, revised 2026-10-03

## Context

baryon-virtd has two kinds of local clients: baryon-anchor (one per VM,
inside an Anchor Pod in the VM's namespace) and baryon-node-agent (one per node,
in `baryon-system`). The controller has no path to baryon-virtd. The protocol
must authenticate callers, support a long-lived per-VM session with
server-sent state, tolerate version skew (the host image and the cluster
components are upgraded independently), and never listen on TCP.

No file descriptors cross this protocol: baryon-virtd derives namespaces and
disks from the caller's process (ADR 0005, ADR 0006).

## Decision

- gRPC (tonic 0.14, prost) over Unix domain sockets. Protobuf package
  `baryon.runtime.v1alpha1`, defined in `crates/runtime-api/proto/`, compiled
  with protox so no `protoc` is needed.
- Two sockets with separate services:
  - `/run/baryon/anchor/virtd.sock`: `AnchorService` (`Attach` server stream,
    `Stop`). A caller can only act on the VM of its own pod, identified from
    peer credentials; requests from processes that are not in a pod cgroup or
    run as root are refused.
  - `/run/baryon/node/virtd.sock`: `NodeService` (`GetRuntimeInfo`,
    `GetHost`, `WatchVms`). Read-only.
- Both report `RuntimeInfo` (baryon-virtd version, protocol, Cloud Hypervisor
  version) so clients can detect skew. `AttachRequest` carries the protocol
  the anchor speaks; baryon-virtd refuses any other than its own with
  `FAILED_PRECONDITION`, and the anchor ends with `AttachRefused`.
- The per-VM host identity (`VmIdentity` in `crates/runtime-api`, ADR 0011)
  and the guest MAC derivation are part of the protocol: the controller and
  baryon-virtd must compute the same values, so changing either is a
  protocol change. The allocation of the host identity is internal to that
  type and is not documented as a format.
- The host report (`HostInfo.report_json`) is versioned by
  `report_version`, which changes only for incompatible changes. Compatible
  changes add optional fields; readers ignore unknown fields.
- The anchor socket directory reaches Anchor containers through the
  `virtualization.baryon.io/vm` device plugin (a kubelet-injected mount), not
  through a hostPath volume in the pod spec. The node socket directory is a
  hostPath volume of the node agent DaemonSet in `baryon-system`.
- The controller passes the VM runtime spec to baryon-anchor in the
  `BARYON_VM_SPEC` environment variable (`AnchorSpec`, JSON, in
  `crates/runtime-api/src/anchor_spec.rs`). baryon-virtd treats it as
  untrusted: identity strings are validated, resource use is bounded by the
  pod cgroup, and disks are resolved inside the pod.

## Consequences

- Peer credentials are taken once per connection, together with a pidfd, so
  PID reuse cannot redirect later operations.
- Any container that is allocated the `virtualization.baryon.io/vm` resource
  can reach the anchor socket. The ValidatingAdmissionPolicy in
  `deploy/base/policy/vm-resource.yaml` lets only baryon-controller create pods
  that request it, and baryon-virtd requires the caller to run as the host
  identity of the VM it names (see [security.md](../security.md)).
- The node socket is owner-only (0600 in a 0700 directory) and NodeService
  additionally requires peer uid 0, which baryon-node-agent has (without
  capabilities).
- Schema evolution follows protobuf rules.
- Kairos rolling upgrades mix host images in one cluster. Within one
  protocol version, components must tolerate each other's additive changes;
  the supported skew between cluster release and host image is not yet
  written down (Planned in [compatibility.md](../compatibility.md)).
- `grpcurl` can be used against the sockets for debugging.

## Alternatives considered

- hostPath volume for the anchor socket in each Anchor Pod (the first
  prototype). Rejected: requires every VM namespace to allow hostPath under
  Pod Security Admission.
- A CSI ephemeral volume to deliver the socket. Viable; the device plugin
  also serves as the scheduling gate and grants /dev/kvm, so one mechanism
  covers all three.
- JSON over HTTP on a Unix socket. Viable; gRPC gives streaming, deadlines,
  status codes, and schema evolution without a hand-written framing layer.
- A custom framed protocol with SCM_RIGHTS. Not needed while no fds cross the
  protocol.
- TCP with mTLS from the controller. Rejected: gives the controller a network
  path to root daemons on every node.

## Revisions

- 2026-10-03: the node socket became owner-only with a uid 0 check, the
  anchor identity check and the admission policy were added, and after the
  architecture gate the protocol field in `AttachRequest`, the identity
  derivation as part of the protocol, and the host report versioning rule.
- 2026-10-03: the host identity is referred to through `VmIdentity` (ADR
  0011) instead of a described derivation.
