//! Inspection of device nodes such as /dev/kvm and /dev/net/tun.

use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};

use serde::{Deserialize, Serialize};

use crate::root::HostRoot;

/// Observed state of a device node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceNode {
    /// Absolute host path, for example `/dev/kvm`.
    pub path: String,
    /// The path exists.
    pub present: bool,
    /// The path is a character device.
    pub char_device: bool,
    /// Major and minor numbers when the path is a character device.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_number: Option<(u32, u32)>,
}

impl DeviceNode {
    /// Whether the node exists and is a character device.
    pub fn available(&self) -> bool {
        self.present && self.char_device
    }
}

/// Inspect a device node without opening it.
pub fn inspect(root: &HostRoot, abs: &str) -> DeviceNode {
    let path = root.path(abs);
    match fs::metadata(&path) {
        Ok(meta) => {
            let char_device = meta.file_type().is_char_device();
            let device_number = char_device.then(|| {
                let dev = meta.rdev();
                (rustix::fs::major(dev), rustix::fs::minor(dev))
            });
            DeviceNode {
                path: abs.to_owned(),
                present: true,
                char_device,
                device_number,
            }
        }
        Err(_) => DeviceNode {
            path: abs.to_owned(),
            present: false,
            char_device: false,
            device_number: None,
        },
    }
}

/// Device nodes relevant to the Baryon runtime other than KVM, IOMMU, and VFIO.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkDevices {
    pub tun: DeviceNode,
    pub vhost_net: DeviceNode,
    pub vhost_vsock: DeviceNode,
}

pub(crate) fn network_devices(root: &HostRoot) -> NetworkDevices {
    NetworkDevices {
        tun: inspect(root, "/dev/net/tun"),
        vhost_net: inspect(root, "/dev/vhost-net"),
        vhost_vsock: inspect(root, "/dev/vhost-vsock"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_real_char_device() {
        let node = inspect(&HostRoot::system(), "/dev/null");
        assert!(node.present);
        assert!(node.char_device);
        assert_eq!(node.device_number, Some((1, 3)));
        assert!(node.available());
    }

    #[test]
    fn regular_file_is_not_a_device() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("dev")).unwrap();
        std::fs::write(dir.path().join("dev/kvm"), b"").unwrap();
        let node = inspect(&HostRoot::at(dir.path()), "/dev/kvm");
        assert!(node.present);
        assert!(!node.char_device);
        assert!(!node.available());
    }

    #[test]
    fn missing_node() {
        let dir = tempfile::tempdir().unwrap();
        let node = inspect(&HostRoot::at(dir.path()), "/dev/kvm");
        assert!(!node.present);
        assert!(!node.available());
    }
}
