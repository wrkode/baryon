//! Baryon host virtualization runtime.
//!
//! The binary in `main.rs` wires these modules together. They are exposed
//! as a library so that integration tests can exercise the handoff and
//! supervision code directly.

pub mod cgroup;
pub mod config;
pub mod dhcp_helper;
pub mod handoff;
pub mod peer;
pub mod podcgroup;
pub mod record;
pub mod seccomp;
pub mod serial;
pub mod services;
pub mod vm;
pub mod vmm;
pub mod vmm_files;
