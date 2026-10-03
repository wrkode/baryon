//! Basic PCI inventory from /sys/bus/pci/devices.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::root::HostRoot;

/// One PCI function.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PciDevice {
    /// Domain, bus, device, and function, for example `0000:00:1f.2`.
    pub address: String,
    /// Four hex digits without a prefix, for example `8086`.
    pub vendor_id: String,
    pub device_id: String,
    /// Six hex digits: class, subclass, programming interface.
    pub class: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subsystem_vendor_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subsystem_device_id: Option<String>,
    /// Bound kernel driver, for example `nvme` or `vfio-pci`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iommu_group: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub numa_node: Option<u32>,
    /// Maximum SR-IOV virtual functions, when the device supports SR-IOV.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sriov_total_vfs: Option<u32>,
}

impl PciDevice {
    /// The two-digit PCI base class, for example `02` for network controllers.
    pub fn base_class(&self) -> &str {
        self.class.get(..2).unwrap_or("")
    }
}

pub(crate) fn discover(root: &HostRoot) -> Result<Vec<PciDevice>> {
    let mut devices = Vec::new();
    for address in root.read_dir_names("/sys/bus/pci/devices")? {
        let base = format!("/sys/bus/pci/devices/{address}");
        let hex = |file: &str| -> Result<Option<String>> {
            root.read_optional(&format!("{base}/{file}"))
                .map(|v| v.map(|s| strip_hex(&s)))
        };
        let required = |file: &str| -> Result<String> {
            hex(file)?.ok_or_else(|| Error::parse(root.path(&format!("{base}/{file}")), "missing"))
        };

        let iommu_group = root
            .read_link_name(&format!("{base}/iommu_group"))?
            .map(|g| {
                g.parse::<u32>().map_err(|e| {
                    Error::parse(root.path(&format!("{base}/iommu_group")), e.to_string())
                })
            })
            .transpose()?;

        // The kernel reports -1 when the device has no NUMA affinity.
        let numa_node = root
            .read_optional(&format!("{base}/numa_node"))?
            .and_then(|s| s.trim().parse::<i64>().ok())
            .and_then(|n| u32::try_from(n).ok());

        let sriov_total_vfs = root
            .read_u64_optional(&format!("{base}/sriov_totalvfs"))?
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0);

        devices.push(PciDevice {
            vendor_id: required("vendor")?,
            device_id: required("device")?,
            class: required("class")?,
            subsystem_vendor_id: hex("subsystem_vendor")?,
            subsystem_device_id: hex("subsystem_device")?,
            driver: root.read_link_name(&format!("{base}/driver"))?,
            iommu_group,
            numa_node,
            sriov_total_vfs,
            address,
        });
    }
    Ok(devices)
}

fn strip_hex(s: &str) -> String {
    let s = s.trim();
    s.strip_prefix("0x").unwrap_or(s).to_ascii_lowercase()
}
