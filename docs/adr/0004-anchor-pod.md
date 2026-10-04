# 0004: One VM Anchor Pod per running VM

Status: Accepted
Date: 2026-10-02, revised 2026-10-03 and 2026-10-04

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
  name that it controls (an ownerReference with the VM UID) and that
  requests the `virtualization.baryon.io/vm` resource, which only
  baryon-controller can give a pod (admission policy `baryon-vm-resource`).
  It deletes such a pod with a UID precondition. Any other pod of that name
  is left alone and reported as `AnchorPodConflict`.
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
  volume, and scheduling fields as the Anchor Pod, `restartPolicy: OnFailure`,
  and requests the VM's CPU and memory plus one
  `virtualization.baryon.io/import` slot. That resource has the same slot
  count and health as `virtualization.baryon.io/vm` but grants nothing, so
  the import Pod lands on a node that can run the VM (a claim that binds on
  first use binds there) without /dev/kvm or the anchor socket. Only
  baryon-controller may create pods that request either resource
  (admission policy `baryon-vm-resource`).
- The import Pod also mounts a downward API file of its own annotation
  `virtualization.baryon.io/import-approved`. The importer writes nothing
  until the file equals the claim UID, which baryon-controller sets only
  after it has checked the claim's bound volume; only baryon-controller may
  set that annotation (admission policy `baryon-import-approval`, ADR
  0006). A pod named `<vm>-import` that the VM does not control or that
  does not request the import resource is never used and blocks the VM
  (`ImportPodConflict`).
- Exclusion rule: the Anchor Pod is created only when no import is pending
  for the VM and no import Pod object for it exists; an import Pod that is
  being deleted must be gone first. The exclusion relies on a single
  baryon-controller replica (no leader election) and must be revisited
  before the controller runs highly available.

Placement is completely determined by Kubernetes scheduling of this pod
and of the import Pod. The VirtualMachine's `spec.placement` carries
Kubernetes scheduling constraints (`nodeSelector`, `affinity`,
`tolerations`, `topologySpreadConstraints`, `priorityClassName`; ADR 0008),
which the controller copies to both pods unchanged. The only requirement
the controller adds is its own required node term, which keeps the pods off
nodes where the VM hit a host identity conflict (ADR 0011); it is ANDed
into every user node selector term. No `nodeName`, `schedulerName`, or
other pod field is exposed. Baryon never picks a node and computes no node
eligibility beside the scheduler: a pod that cannot be placed is reported
with the scheduler's reason and message, and a pod the API server refuses
at creation with `PodRejected`. Runtime health keeps gating placement
through the device plugin resources.

A placement change does not move a running VM. The Anchor Pod records a
hash of the placement it was created with
(`virtualization.baryon.io/placement-hash`); a difference is reported as
`RestartRequired` with reason `PlacementChanged` and applies at the next
start.

## Consequences

- Scheduling, quotas, LimitRanges, and PodDisruptionBudgets apply to the
  Anchor Pod like to any pod. Namespace-scoped admission (PodNodeSelector,
  PodTolerationRestriction, ResourceQuota scopes for PriorityClass and
  cross-namespace pod affinity) judges the VM's pods by their namespace,
  which is how administrators bound what VM placement may request. The
  VirtualMachine API exposes no Multus annotations.
- A preempted Anchor Pod stops its VM; the controller creates a new one,
  which is scheduled like any pod.
- A VM on a node-local volume stays on that volume's node whatever its
  placement says; a placement that excludes the node keeps the VM
  `Unschedulable`.
- The import Pod carries the VM's labels and is never Ready by design. A
  PodDisruptionBudget that selects VM labels counts it as unhealthy during
  an import, which can block evictions of the other pods the budget
  covers (for example `kubectl drain`). Deletions that bypass the Eviction
  API, such as the Kairos Operator drain, are unaffected.
- A stopped VM has no pod and holds no node resources. Its claim remains.
- Stopping and starting a VM creates a new pod, so the VM may land on a
  different node and get a different pod IP. Its MAC address does not change
  (ADR 0007).
- If a node becomes unreachable, the Anchor Pod object stays until it is
  force-deleted or the node is marked out of service, exactly like a
  StatefulSet pod. The VM is not started elsewhere until then; this is the
  fencing rule. A VM that tolerates the `NoExecute` taints
  `node.kubernetes.io/not-ready` or `node.kubernetes.io/unreachable` only
  delays the eviction of its pod, and with it its own replacement; the
  rule is unchanged. Tolerations of `node.kubernetes.io/out-of-service`
  are refused, so the out-of-service taint applies to every VM pod.
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
  Together with `ReadWriteOnce` claims they are what prevents a second
  copy of a VM today: `ClaimInUse` ignores pods of the same VM, and
  `DiskInUse` is checked on one node only. Live migration is Not supported
  (ADR 0013); a migration design would need a new fencing scheme with one
  Kubernetes-held authority for the active copy, enforced on the data
  path.

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
- A controller-computed eligibility check with a `NoCompatibleNode` reason.
  Rejected: a second eligibility computation beside the scheduler would
  drift from it (taints, capacity, volume topology); the scheduler's own
  message says which constraint excluded which nodes.

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
- 2026-10-03: the import Pod waits for an approval annotation that only
  baryon-controller sets (ADR 0006); a pod named like the import Pod or the
  Anchor Pod counts only if the VM controls it and it requests the import
  or vm resource (`ImportPodConflict`, `AnchorPodConflict`). The importer
  init container of Anchor Pods of earlier releases is no longer read as
  import evidence.
- 2026-10-03: VM placement. The VirtualMachine gained `spec.placement`
  (ADR 0008), which the controller copies to the import Pod and the Anchor
  Pod; its identity-conflict term is ANDed into every user node selector
  term; placement changes are `RestartRequired` (`PlacementChanged`), not
  applied to a running VM; unschedulable pods report the scheduler's
  reason, refused pods `PodRejected`. The Consequences no longer call
  placement in the API Planned and record preemption, node-local volumes,
  namespace admission, and the fencing effect of tolerations. After the
  architecture review: a PodDisruptionBudget over VM labels counts the
  never-Ready import Pod as unhealthy.
- 2026-10-04: the migration consequence now links ADR 0013 and states that
  the fixed name, the at-most-one rule, and `ReadWriteOnce` claims are what
  prevents a second copy of a VM today. The decision is unchanged.
