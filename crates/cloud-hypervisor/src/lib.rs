//! Internal Cloud Hypervisor integration for Baryon.
//!
//! This crate is deliberately narrow. It knows how to express a Baryon VM as
//! a Cloud Hypervisor command line, how to call the Cloud Hypervisor REST
//! API over its Unix socket, and how to read the event monitor stream. It is
//! not a VMM abstraction and has no notion of other hypervisors.
//!
//! Behavior is pinned to Cloud Hypervisor v54. Upstream facts this crate
//! relies on are cited in the module documentation and were checked against
//! v54.0.

pub mod api;
pub mod events;
pub mod launch;
pub mod version;

pub use api::{ApiClient, ApiError, VmInfo, VmState, VmmPing};
pub use launch::LaunchConfig;
pub use version::Version;
