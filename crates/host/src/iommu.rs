//! IOMMU, IOMMUFD, and VFIO discovery.

use serde::{Deserialize, Serialize};

use crate::devices::{self, DeviceNode};
use crate::error::{Error, Result};
use crate::root::HostRoot;

/// One IOMMU group from /sys/kernel/iommu_groups.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IommuGroup {
    pub id: u32,
    /// Default domain type, for example `DMA`, `DMA-FQ`, or `identity`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain_type: Option<String>,
    /// PCI addresses or other device names in the group.
    pub devices: Vec<String>,
}

/// Observed IOMMU state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IommuInfo {
    /// An IOMMU is active: at least one unit is registered and devices are
    /// placed in groups.
    pub enabled: bool,
    /// Registered IOMMU units from /sys/class/iommu, for example `dmar0`.
    pub units: Vec<String>,
    pub groups: Vec<IommuGroup>,
    /// IOMMU-related parameters from the kernel command line, reported as
    /// found. They explain the state; they are not used to decide it.
    pub kernel_parameters: Vec<String>,
    /// The IOMMUFD character device, /dev/iommu.
    pub iommufd: DeviceNode,
}

/// Observed VFIO state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VfioInfo {
    /// The legacy VFIO container device, /dev/vfio/vfio.
    pub container: DeviceNode,
    /// The vfio-pci driver is registered with the PCI bus.
    pub vfio_pci_driver: bool,
    /// /dev/vfio/devices exists, which means VFIO device character devices
    /// (used with IOMMUFD) are enabled and at least one device is bound.
    pub device_cdev_dir: bool,
    /// Loaded kernel modules relevant to VFIO. Built-in code that registers
    /// a /sys/module entry is reported the same way.
    pub modules: Vec<String>,
}

const IOMMU_CMDLINE_KEYS: &[&str] = &["intel_iommu", "amd_iommu", "iommu", "iommu.passthrough"];
const VFIO_MODULES: &[&str] = &[
    "vfio",
    "vfio_pci",
    "vfio_pci_core",
    "vfio_iommu_type1",
    "iommufd",
];

pub(crate) fn discover_iommu(root: &HostRoot) -> Result<IommuInfo> {
    let units = root.read_dir_names("/sys/class/iommu")?;
    let mut groups = Vec::new();
    for name in root.read_dir_names("/sys/kernel/iommu_groups")? {
        let id: u32 = name.parse().map_err(|e: std::num::ParseIntError| {
            Error::parse(
                root.path(&format!("/sys/kernel/iommu_groups/{name}")),
                e.to_string(),
            )
        })?;
        let base = format!("/sys/kernel/iommu_groups/{name}");
        let domain_type = root
            .read_optional(&format!("{base}/type"))?
            .map(|s| s.trim().to_owned());
        let devices = root.read_dir_names(&format!("{base}/devices"))?;
        groups.push(IommuGroup {
            id,
            domain_type,
            devices,
        });
    }
    groups.sort_by_key(|g| g.id);

    let cmdline = root.read_optional("/proc/cmdline")?.unwrap_or_default();
    let kernel_parameters = cmdline
        .split_whitespace()
        .filter(|p| {
            let key = p.split_once('=').map_or(*p, |(k, _)| k);
            IOMMU_CMDLINE_KEYS.contains(&key)
        })
        .map(str::to_owned)
        .collect();

    Ok(IommuInfo {
        enabled: !units.is_empty() && !groups.is_empty(),
        units,
        groups,
        kernel_parameters,
        iommufd: devices::inspect(root, "/dev/iommu"),
    })
}

pub(crate) fn discover_vfio(root: &HostRoot) -> Result<VfioInfo> {
    let modules = VFIO_MODULES
        .iter()
        .filter(|m| root.exists(&format!("/sys/module/{m}")))
        .map(|m| (*m).to_owned())
        .collect();
    Ok(VfioInfo {
        container: devices::inspect(root, "/dev/vfio/vfio"),
        vfio_pci_driver: root.exists("/sys/bus/pci/drivers/vfio-pci"),
        device_cdev_dir: root.exists("/dev/vfio/devices"),
        modules,
    })
}
