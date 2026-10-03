# 0010: License under Apache-2.0

Status: Accepted
Date: 2026-10-03

## Context

Baryon is published as an open source project. It is built on and next to
projects that use Apache-2.0: Kubernetes, Kairos, Hadron, and Cloud
Hypervisor (Apache-2.0 or BSD-3-Clause). It vendors one Kubernetes protobuf
definition (the kubelet device plugin API) and depends on Rust crates under
permissive licenses. Contributions are accepted under the Developer
Certificate of Origin.

## Decision

- Baryon is licensed under the Apache License, Version 2.0. The full text
  is in `LICENSE` at the repository root.
- Every crate declares `license = "Apache-2.0"` through the workspace
  (`license.workspace = true`).
- Source files do not carry per-file license headers; the repository
  `LICENSE` applies. Vendored files keep their upstream origin note.
- Every commit carries a DCO `Signed-off-by` trailer created with
  `git commit -s`.

## Consequences

- The license matches the ecosystem Baryon integrates with, so code and
  manifests can move between these projects without relicensing.
- Apache-2.0 includes an explicit patent grant.
- Dependencies must stay compatible with Apache-2.0; a dependency license
  check in CI is Planned.

## Alternatives considered

- MIT or BSD-3-Clause. Rejected: no explicit patent grant, and a different
  license from the projects Baryon is part of.
- Dual licensing (Apache-2.0 or MIT), common for Rust crates. Rejected: the
  crates are not published for reuse (`publish = false`), so the second
  license adds nothing.
- A contributor license agreement instead of DCO. Rejected: DCO is lighter
  and common in the Kubernetes ecosystem.
