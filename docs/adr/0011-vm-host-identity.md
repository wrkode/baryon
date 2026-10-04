# 0011: Each VM runs as a distinct unprivileged host identity

Status: Accepted
Date: 2026-10-03, revised 2026-10-03

## Context

Cloud Hypervisor runs on the host, started by baryon-virtd (ADR 0005). The
processes of different VMs on one node must not be able to signal, trace,
or read each other, and none of them may hold privileges. Disk images that
Baryon imports are files owned by the identity that wrote them, so a VM must
keep the same identity across Anchor Pod replacements, baryon-virtd
restarts, and node reboots.

Kubernetes user namespaces (`hostUsers: false`) offer per-pod id mapping,
but the VMM is a host process outside the pod's user namespace, and the
mapping would change the ownership model of every imported image.

## Decision

- Each Baryon VM executes as a distinct, unprivileged host uid and gid: the
  Anchor container, Cloud Hypervisor, and the DHCP responder of that VM.
  This is the externally visible contract.
- The identity is allocated deterministically from the VM by an internal
  function behind the `VmIdentity` type in `crates/runtime-api`, shared by
  baryon-controller (which runs the Anchor container as it) and
  baryon-virtd (which verifies the anchor's peer credentials against it).
  The allocation algorithm is not part of the Kubernetes API, of
  documented storage formats, or of any annotation, and may change with a
  runtime protocol version (ADR 0009).
- Hosts reserve the id range 1000000000 to 1999999999 for VM identities.
- baryon-virtd refuses an anchor whose uid or gid differs from its VM's
  identity (`IdentityMismatch`), and refuses a second VM with the same
  identity on a node (`IdentityConflict`): a collision is never tolerated.
- On `IdentityConflict`, baryon-controller records the node in the VM's
  `status.identityConflictNodes`, replaces the Anchor Pod, and keeps the
  new pod off the recorded nodes with required node affinity: a
  `matchFields` requirement `metadata.name NotIn <nodes>`, ANDed into every
  required node selector term of the VM's own placement (ADR 0004), so no
  user term can reach a recorded node. The list is cleared when the VM is
  stopped.
- Filesystem access: the pod's `fsGroup` is the VM's gid, and Baryon's
  importer writes images mode 0600 as the VM's uid. Raw block volumes are
  devices, not files: their access is governed by the device permissions
  the container runtime sets for the pod, not by Unix file ownership that
  Baryon would impose.
- Kubernetes user namespaces may be evaluated later as additional hardening
  for the Anchor container; they are not the VM identity contract.

## Consequences

- A collision is visible (`Ready=False`, reason `IdentityConflict`, an
  event) and resolves itself by placing the VM on another node. It cannot
  resolve on a single-node cluster, or when the VM's placement allows no
  other node; the VM then stays not ready (`Unschedulable` once every
  allowed node is recorded) until the conflicting VM is gone and the VM is
  stopped and started.
- Tolerations in the VM's placement do not weaken this exclusion or the
  fencing rule: the exclusion is a required node term, not a taint, and
  tolerating `NoExecute` not-ready or unreachable taints only delays the
  eviction of the VM's pod (ADR 0004).
- The probability of a collision among n VMs on one node is about
  n^2 / 2^31, roughly one in 200000 for 100 VMs.
- A VirtualMachine re-created under the same name has a new Kubernetes UID
  and therefore a new identity; an imported image it reuses through a
  retained claim is readable only if the storage applies `fsGroup`
  ownership.
- Changing the allocation is a protocol change: the cluster release and the
  host image must agree on it.
- Block-mode disks need the container runtime to give device nodes the
  pod's `runAsUser` and `runAsGroup`; kubelet does not apply `fsGroup` to
  raw block devices. With containerd this is
  `device_ownership_from_security_context = true`, which is node-wide: it
  applies to every device of every pod that sets `runAsUser`, including
  devices from device plugins. In Anchor containers /dev/kvm is then owned
  by the VM's host identity, so any process in the container can open it
  (the VMM, a host process, opens the host's /dev/kvm through the kvm
  group either way; the device cgroup allows /dev/kvm in both cases). Without the setting a block-mode
  VM fails with `BlockDeviceAccessDenied`. Verified on three Hadron nodes
  running k0s with runc.

## Alternatives considered

- Kubernetes user namespaces as the identity mechanism. Rejected as the
  contract: the VMM is a host process outside the pod's user namespace, and
  ownership of imported images would depend on the id mapping.
- One shared uid for all VMMs. Rejected: one compromised VMM could signal
  or trace every other.
- Node-local allocation by baryon-virtd. Rejected: the Anchor container's
  uid must be known when the pod is created, before it is scheduled.
- Re-allocating on collision with a different value. Rejected: the VM's
  imported image is owned by its identity.

## Revisions

- 2026-10-03: written when the identity moved behind `VmIdentity`, the
  algorithm was removed from the documentation, and collisions gained the
  `IdentityConflict` handling.
- 2026-10-03: the containerd `device_ownership_from_security_context`
  requirement for block-mode disks and its effect on /dev/kvm in Anchor
  containers were recorded.
- 2026-10-03: with VM placement (ADR 0004, ADR 0008) the identity-conflict
  requirement is ANDed into every user node selector term; a placement that
  allows only recorded nodes leaves the VM `Unschedulable`; tolerations do
  not affect the exclusion or fencing.
