//! Host virtualization capability discovery for Baryon nodes.
//!
//! Discovery reads structured kernel interfaces (sysfs, procfs, device
//! nodes, os-release files) and never parses the output of command-line
//! tools. It has no Kubernetes dependency. Every reader takes a
//! [`HostRoot`] so that tests can run against fixture trees.
//!
//! The only operation that goes beyond reading files is the optional
//! `KVM_GET_API_VERSION` ioctl, selected with [`KvmProbe::Ioctl`].

pub mod cgroup;
pub mod cpu;
pub mod cpulist;
pub mod devices;
pub mod error;
pub mod iommu;
pub mod kvm;
pub mod memory;
pub mod numa;
pub mod os;
pub mod pci;
pub mod report;
pub mod root;

pub use error::{Error, Result};
pub use kvm::KvmProbe;
pub use report::{DiscoveryOptions, HostReport, Problem, discover};
pub use root::HostRoot;
