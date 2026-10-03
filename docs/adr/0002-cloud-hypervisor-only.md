# 0002: Cloud Hypervisor is the only VMM

Status: Accepted
Date: 2026-10-02

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

## Consequences

- Features that Cloud Hypervisor lacks are not available in Baryon, and
  Baryon does not work around them with another VMM.
- Cloud Hypervisor upgrades arrive with OS image upgrades, which interacts
  with migration compatibility (Cloud Hypervisor guarantees migration from at
  most the two previous major versions starting with v54). Baryon must check
  versions before attempting migration.
- Test nodes that are themselves VMs run under another hypervisor with
  nested virtualization; that hypervisor is not a Baryon backend.

## Alternatives considered

- A pluggable VMM interface with Cloud Hypervisor as the first
  implementation. Rejected: no second implementation is planned, and an
  interface designed without one encodes Cloud Hypervisor assumptions anyway.
- A general-purpose VMM with broad device emulation. Rejected: larger device
  model and attack surface, and the project thesis is a Cloud Hypervisor
  platform.
