//! KVM availability.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::devices::{self, DeviceNode};
use crate::root::HostRoot;

/// The stable KVM API version. The kernel has reported 12 since Linux 2.6.22
/// and userspace is expected to refuse any other value.
pub const KVM_API_VERSION: i32 = 12;

/// `_IO(KVMIO, 0x00)` with `KVMIO = 0xAE`.
const KVM_GET_API_VERSION: u64 = 0xAE00;

/// Observed KVM state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KvmInfo {
    pub device: DeviceNode,
    /// Result of `KVM_GET_API_VERSION`, when probed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_version: Option<i32>,
    /// KVM can be used to run guests from this host.
    pub usable: bool,
    /// Why KVM is not usable, or why usability was not fully verified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// How far KVM discovery may go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KvmProbe {
    /// Open /dev/kvm and issue `KVM_GET_API_VERSION`. Requires read and write
    /// access to the device.
    Ioctl,
    /// Only inspect the device node. Used for fixture-based tests.
    NodeOnly,
}

pub(crate) fn discover(root: &HostRoot, probe: KvmProbe) -> KvmInfo {
    let device = devices::inspect(root, "/dev/kvm");
    if !device.present {
        return KvmInfo {
            device,
            api_version: None,
            usable: false,
            reason: Some("/dev/kvm does not exist".into()),
        };
    }
    if !device.char_device {
        return KvmInfo {
            device,
            api_version: None,
            usable: false,
            reason: Some("/dev/kvm is not a character device".into()),
        };
    }
    match probe {
        KvmProbe::NodeOnly => KvmInfo {
            device,
            api_version: None,
            usable: true,
            reason: Some("API version not probed".into()),
        },
        KvmProbe::Ioctl => match probe_api_version(&root.path("/dev/kvm")) {
            Ok(version) if version == KVM_API_VERSION => KvmInfo {
                device,
                api_version: Some(version),
                usable: true,
                reason: None,
            },
            Ok(version) => KvmInfo {
                device,
                api_version: Some(version),
                usable: false,
                reason: Some(format!(
                    "unexpected KVM API version {version}, expected {KVM_API_VERSION}"
                )),
            },
            Err(e) => KvmInfo {
                device,
                api_version: None,
                usable: false,
                reason: Some(format!("cannot query /dev/kvm: {e}")),
            },
        },
    }
}

/// Open the KVM device and return the result of `KVM_GET_API_VERSION`.
#[allow(unsafe_code)]
pub fn probe_api_version(path: &Path) -> io::Result<i32> {
    // std opens files with O_CLOEXEC.
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    // SAFETY: KVM_GET_API_VERSION does not read or write user memory; it
    // only returns an integer. The kernel rejects a non-zero argument with
    // EINVAL (virt/kvm/kvm_main.c, kvm_dev_ioctl), so the variadic argument is
    // passed explicitly as 0 rather than left to whatever the register holds.
    // The descriptor is valid for the duration of the call because `file` is
    // alive until the end of this function. The request type differs between
    // libc implementations, hence the inferred cast.
    let ret = unsafe { libc::ioctl(file.as_raw_fd(), KVM_GET_API_VERSION as _, 0usize) };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}
