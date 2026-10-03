# 0004: One VM Anchor Pod per running VM

Status: Accepted
Date: 2026-10-02, revised 2026-10-03

## Context

A VM needs a node, CPU and memory reservations, storage attached through CSI,
a network attachment through CNI, and placement policy (affinity, taints,
priority). Kubernetes provides all of these for pods. Rebuilding them for VMs
would duplicate the scheduler, the volume manager, and CNI integration.

## Decision

Every running VM has exactly one VM Anchor Pod, created by baryon-controller
in the VM's namespace and owned by the VirtualMachine.

- The pod has a fixed name per VM (`<vm>-anchor`). The API server's name
  uniqueness therefore guarantees that at most one Anchor Pod object, and so
  at most one VMM, exists per VM. A replacement is created only after the
  previous pod object is gone. The controller acts only on a pod of that
  name that it controls (an ownerReference with the VM UID), deletes it with
  a UID precondition, and otherwise reports `AnchorPodConflict` and leaves
  the pod alone.
- Every container sets requests equal to limits, so the pod is Guaranteed
  QoS and its cgroup has a memory limit. Memory is
  guest memory plus 128 MiB plus 1/64 of guest memory for the VMM; CPU is the
  vCPU count.
- The root disk claim is a volume of the pod (filesystem mount or raw block
  device).
- The pod network is the VM network.
- The pod requests one `virtualization.baryon.io/vm` extended resource,
  served by the baryon-node-agent device plugin. Nodes advertise it only while
  baryon-virtd is healthy and KVM is usable. This is the scheduling gate; no
  Node labels are written.
- The pod runs one unprivileged container, `baryon-anchor`, without
  capabilities; the pod has no service account token and no hostPath
  volumes, and the container has a read-only root filesystem. It satisfies
  the Pod Security "restricted" level (Tested on a historical development
  cluster in which a kind node container acted as the host, with
  `enforce=restricted`).
- An imported root disk is written by a separate import Pod, `<vm>-import`,
  before the first Anchor Pod exists. It runs `baryon-anchor import` as the
  VM's host identity with the same restricted security context, claim
  volume, and node affinity as the Anchor Pod, `restartPolicy: OnFailure`,
  and requests the VM's CPU and memory plus one
  `virtualization.baryon.io/import` slot. That resource has the same slot
  count and health as `virtualization.baryon.io/vm` but grants nothing, so
  the import Pod lands on a node that can run the VM (a claim that binds on
  first use binds there) without /dev/kvm or the anchor socket. Only
  baryon-controller may create pods that request either resource
  (admission policy `baryon-vm-resource`).
- Exclusion rule: the Anchor Pod is created only when no import is pending
  for the VM and no import Pod object for it exists; an import Pod that is
  being deleted must be gone first. The exclusion relies on a single
  baryon-controller replica (no leader election) and must be revisited
  before the controller runs highly available.

Placement is completely determined by Kubernetes scheduling of this pod.
The controller adds no placement fields except a required node affinity
that keeps the pod off nodes where the VM hit a host identity conflict
(ADR 0011).
Baryon never picks a node.

## Consequences

- Scheduling, quotas, LimitRanges, and PodDisruptionBudgets apply to the
  Anchor Pod like to any pod. The VirtualMachine API does not yet expose pod
  placement (node selectors, tolerations, affinity, topology spread, priority
  class) or Multus annotations, and the controller sets none; a curated
  subset in the VM API is Planned.
- A stopped VM has no pod and holds no node resources. Its claim remains.
- Stopping and starting a VM creates a new pod, so the VM may land on a
  different node and get a different pod IP. Its MAC address does not change
  (ADR 0007).
- If a node becomes unreachable, the Anchor Pod object stays until it is
  force-deleted or the node is marked out of service, exactly like a
  StatefulSet pod. The VM is not started elsewhere until then; this is the
  fencing rule.
- `kubectl delete pod` on an Anchor Pod stops the VM. The controller creates
  a new one if the VM should run.
- Capacity window: the import Pod does not reserve a
  `virtualization.baryon.io/vm` slot. If a node's capacity is taken between
  the import and the Anchor Pod, an Anchor Pod for a claim bound to that
  node stays Pending until capacity is free. An import Pod that cannot be
  scheduled is reported through the VM's `Scheduled` and `Ready` reasons
  (for example `Unschedulable`).
- Anything that deletes pods during node maintenance (drain, Kairos Operator
  upgrades) stops VMs with a guest shutdown; the controller then starts them
  again through a new Anchor Pod wherever their storage allows. That stop
  and restart is the only evacuation. Moving running VMs off a node
  (migration-based evacuation) is Not supported (see
  [kairos-integration.md](../kairos-integration.md#cooperation-with-upgrades)). The pod has no
  emptyDir volume (the anchor keeps its readiness file in `/dev/shm`), so
  `kubectl drain` does not need `--delete-emptydir-data` (inferred from the
  kubectl drain filters, not tested).
- The fixed name and the at-most-one rule exclude a second VMM for a VM,
  which live migration needs (a source and a target at the same time).
  Migration would need a different fencing scheme.

## Alternatives considered

- Baryon-native scheduling of VMs to nodes. Rejected: duplicates the
  scheduler and loses quota, priority, and DRA integration.
- A long-lived per-node pod that hosts many VMs. Rejected: no per-VM resource
  accounting, volume attachment, or network attachment.
- A Node label maintained by the controller as the scheduling gate (the
  first draft of this ADR). Rejected after review: `nodes/patch` cannot be
  limited to labels, so the controller could change taints and
  schedulability of every node; and a label has no liveness. The device
  plugin's health is maintained by kubelet.

## Revisions

- 2026-10-02 (draft, before the first commit): the scheduling gate changed
  from a controller-maintained Node label to the device plugin resource
  after review (see Alternatives).
- 2026-10-03: the import init container is part of the pod; the controller
  acts only on Anchor Pods it controls and deletes with a UID precondition;
  the readiness file moved from an emptyDir to `/dev/shm`; the placement
  claim in Consequences was corrected (no placement fields in the API yet);
  the migration constraint was recorded.
- 2026-10-03: the import moved out of the Anchor Pod into a separate import
  Pod with the placement-only `virtualization.baryon.io/import` resource,
  so the importer receives neither /dev/kvm nor the anchor socket, and the
  import is recorded before any Anchor Pod uses the disk. The exclusion
  rule between the two pods and the capacity window were recorded, and the
  text describing the import init container was replaced. The controller
  still recognizes Anchor Pods of the previous release with the init
  container (legacy path, ADR 0012). The evacuation consequence was
  corrected after the evacuation design was removed: maintenance stops and
  restarts VMs; migration-based evacuation is Not supported.
