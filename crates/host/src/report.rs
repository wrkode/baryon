//! Aggregated host discovery.

use serde::{Deserialize, Serialize};

use crate::cgroup::{self, CgroupInfo};
use crate::cpu::{self, CpuInfo};
use crate::devices::{self, NetworkDevices};
use crate::iommu::{self, IommuInfo, VfioInfo};
use crate::kvm::{self, KvmInfo, KvmProbe};
use crate::memory::{self, MemoryInfo};
use crate::numa::{self, NumaNode};
use crate::os::{self, OsInfo};
use crate::pci::{self, PciDevice};
use crate::root::HostRoot;

/// A section that could not be discovered. Discovery continues past
/// failures so that one unreadable file does not hide every other result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Problem {
    pub section: String,
    pub message: String,
}

/// Everything Baryon knows about the host's virtualization capabilities.
///
/// Sections that failed are `None` and have a matching entry in
/// [`HostReport::problems`]. A `None` section means unknown, never absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<OsInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<CpuInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryInfo>,
    pub kvm: KvmInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iommu: Option<IommuInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vfio: Option<VfioInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub numa: Option<Vec<NumaNode>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_devices: Option<Vec<PciDevice>>,
    pub network_devices: NetworkDevices,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cgroup: Option<CgroupInfo>,
    pub problems: Vec<Problem>,
}

/// Options that control how intrusive discovery may be.
#[derive(Debug, Clone, Copy)]
pub struct DiscoveryOptions {
    pub kvm_probe: KvmProbe,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            kvm_probe: KvmProbe::Ioctl,
        }
    }
}

/// Discover host capabilities under `root`.
pub fn discover(root: &HostRoot, options: DiscoveryOptions) -> HostReport {
    let mut problems = Vec::new();
    let p = &mut problems;

    let architecture = section(p, "architecture", cpu::architecture(root));
    let os = section(p, "os", os::discover(root));
    let cpu = section(p, "cpu", cpu::discover(root));
    let memory = section(p, "memory", memory::discover(root));
    let iommu = section(p, "iommu", iommu::discover_iommu(root));
    let vfio = section(p, "vfio", iommu::discover_vfio(root));
    let numa = section(p, "numa", numa::discover(root));
    let pci_devices = section(p, "pci", pci::discover(root));
    let cgroup = section(p, "cgroup", cgroup::discover(root));

    HostReport {
        architecture,
        os,
        cpu,
        memory,
        kvm: kvm::discover(root, options.kvm_probe),
        iommu,
        vfio,
        numa,
        pci_devices,
        network_devices: devices::network_devices(root),
        cgroup,
        problems,
    }
}

fn section<T>(problems: &mut Vec<Problem>, name: &str, result: crate::Result<T>) -> Option<T> {
    match result {
        Ok(v) => Some(v),
        Err(e) => {
            problems.push(Problem {
                section: name.to_owned(),
                message: e.to_string(),
            });
            None
        }
    }
}
