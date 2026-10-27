# 0002: Cloud Hypervisor is the only VMM

Status: Accepted
Date: 2026-10-02, revised 2026-10-04

## Context

A VMM abstraction layer costs design effort, testing effort, and API
compromises (lowest common denominator). Baryon controls the host image, so
it can choose one VMM and ship one tested version.

Cloud Hypervisor (v53.0, 2026-07-12) provides what Baryon needs: a REST API
over a Unix socket, UEFI and direct kernel boot, virtio-blk and virtio-net
with pre-opened TAP file descriptors, serial socket output, hybrid vsock,
VFIO with iommufd, hugepages through memfd, snapshot and restore, live
migration, seccomp, and Landlock. It publishes static musl binaries for
x86_64 and aarch64. Its REST API and CLI have a two-release deprecation
policy; snapshot and migration compatibility is limited to specific version
ranges.

## Decision

Cloud Hypervisor is the only VMM backend. Baryon has one internal crate,
`baryon-cloud-hypervisor`, that builds Cloud Hypervisor configuration and
talks to its API. That crate is not a public abstraction and has no trait
for alternative VMMs.

The API (VirtualMachine) describes intent. Cloud Hypervisor arguments are
never part of it.

Each Baryon release pins one Cloud Hypervisor version, delivered in the
Baryon-enabled Hadron image and recorded in the version manifest.

Moving to another Cloud Hypervisor version includes comparing the defaults
of every option Baryon passes or relies on between the two versions, in
the code (`vmm/src/config.rs`) and not only the release notes. Where a
default changed, Baryon writes the option explicitly, so the guest's
device configuration is a Baryon decision and not the pinned version's
default. Baryon's seccomp deviation table is checked again
([security.md](../security.md#baryon-seccomp-layer)).

## Consequences

- Features that Cloud Hypervisor lacks are not available in Baryon, and
  Baryon does not work around them with another VMM.
- Cloud Hypervisor upgrades arrive with OS image upgrades, which interacts
  with migration compatibility (starting with v54.0, released 2026-10-14,
  Cloud Hypervisor accepts migrations from the two previous releases and
  does not support migrating back to an older one). Live migration is Not
  supported (ADR 0013);
  a future migration design must check versions before attempting a
  migration, and ADR 0013 lists that version gate as an entry criterion.
- Test nodes that are themselves VMs run under another hypervisor with
  nested virtualization; that hypervisor is not a Baryon backend.

## Alternatives considered

- A pluggable VMM interface with Cloud Hypervisor as the first
  implementation. Rejected: no second implementation is planned, and an
  interface designed without one encodes Cloud Hypervisor assumptions anyway.
- A general-purpose VMM with broad device emulation. Rejected: larger device
  model and attack surface, and the project thesis is a Cloud Hypervisor
  platform.

## Revisions

- 2026-10-04: the migration compatibility consequence now links ADR 0013
  (live migration Not supported; the version check is one of its entry
  criteria) and says that v54 is not released. The decision is unchanged.
- 2026-10-27: Baryon moves to Cloud Hypervisor v54.0; the migration
  compatibility consequence now states v54's rule instead of the pending
  release, and the Decision records the upgrade check of option defaults
  (v54.0 changed the defaults of nested virtualization and the virtio-net
  offloads). The Context describes v53.0, the version evaluated for this
  ADR. The decision to use Cloud Hypervisor as the only VMM, one pinned
  version per release, is unchanged.
