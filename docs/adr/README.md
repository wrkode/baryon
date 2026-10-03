# Architecture decision records

Each ADR records one significant decision with its context, consequences,
and the alternatives that were considered. ADRs are not edited to change a
decision; a new ADR supersedes the old one, and the old one is marked
Superseded with a link. Refinements that keep the decision (hardening,
corrections, validation results) are made in place and listed in the ADR's
Revisions section with their date.

ADRs are baselined at the first commit of the repository. Changes made
before it, including decisions that changed between drafts, are recorded in
the Revisions sections for context, but the rule above applies from the
first commit on.

Status values: Proposed, Accepted, Superseded, Rejected.

| ADR | Title | Status |
|---|---|---|
| [0001](0001-rust.md) | Implement Baryon in Rust | Accepted |
| [0002](0002-cloud-hypervisor-only.md) | Cloud Hypervisor is the only VMM | Accepted |
| [0003](0003-kairos-hadron-lifecycle.md) | Host lifecycle belongs to Hadron and Kairos | Accepted |
| [0004](0004-anchor-pod.md) | One VM Anchor Pod per running VM | Accepted |
| [0005](0005-node-runtime.md) | Host-owned Cloud Hypervisor confined to the Anchor container | Accepted |
| [0006](0006-storage-model.md) | PVC disks are resolved in the Anchor mount namespace | Accepted (filesystem mode) |
| [0007](0007-network-model.md) | TAP in the pod network namespace with L2 passthrough | Accepted |
| [0008](0008-kubernetes-api.md) | v1alpha1 API surface and status ownership | Accepted |
| [0009](0009-runtime-protocol.md) | gRPC over Unix sockets between node components and baryon-virtd | Accepted |
| [0010](0010-license.md) | License under Apache-2.0 | Accepted |
| [0011](0011-vm-host-identity.md) | Each VM runs as a distinct unprivileged host identity | Accepted |
| [0012](0012-protocol-versioning.md) | Runtime protocol versions, features, and negotiation | Proposed |

Template:

```
# NNNN: Title

Status: Proposed | Accepted | Superseded by NNNN
Date: YYYY-MM-DD

## Context
## Decision
## Consequences
## Alternatives considered
## Revisions
```
