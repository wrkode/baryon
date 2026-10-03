//! Contract between baryon-controller and baryon-anchor.
//!
//! The controller renders an [`AnchorSpec`] as JSON into the
//! `BARYON_VM_SPEC` environment variable of the Anchor container. The anchor
//! converts it into a [`VmSpec`] for baryon-virtd. Everything here is
//! derived from the VirtualMachine and its class; it carries no host paths.
//!
//! baryon-virtd treats the resulting spec as untrusted input: anything the
//! anchor asks for is bounded by the pod's own cgroup limits and by what the
//! pod user can open.

use serde::{Deserialize, Serialize};

use crate::v1alpha1::{self, Boot, FirmwareBoot, NetworkInterface, VmSpec, boot};

/// Environment variable carrying the JSON spec.
pub const SPEC_ENV: &str = "BARYON_VM_SPEC";

/// Mount path of the root disk volume in the Anchor container
/// (filesystem mode). The disk image is `disk.img` inside it.
pub const ROOT_DISK_DIR: &str = "/var/lib/baryon/disks/root";

/// File name of the disk image inside a filesystem-mode volume.
pub const DISK_IMAGE_NAME: &str = "disk.img";

/// Device path of the root disk in the Anchor container (block mode).
pub const ROOT_DISK_DEVICE: &str = "/dev/baryon/root";

/// Directory for anchor runtime files (readiness). In the container's
/// /dev/shm, a tmpfs every container has and can write with a read-only root
/// filesystem, so the Anchor Pod needs no emptyDir volume (which would make
/// `kubectl drain` require `--delete-emptydir-data`).
pub const ANCHOR_STATE_DIR: &str = "/dev/shm/baryon-anchor";

/// Volume mode of a disk as seen by the anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiskMode {
    Filesystem,
    Block,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnchorDisk {
    pub name: String,
    pub mode: DiskMode,
    #[serde(default)]
    pub read_only: bool,
}

impl AnchorDisk {
    /// Path of the disk inside the Anchor container.
    pub fn container_path(&self) -> String {
        match self.mode {
            DiskMode::Filesystem => format!("{ROOT_DISK_DIR}/{DISK_IMAGE_NAME}"),
            DiskMode::Block => ROOT_DISK_DEVICE.to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnchorSpec {
    pub vm_uid: String,
    pub vm_namespace: String,
    pub vm_name: String,
    pub vcpus: u32,
    pub memory_bytes: u64,
    /// Only the root disk is supported in v1alpha1.
    pub root_disk: AnchorDisk,
    /// Pod interface the guest takes over.
    pub pod_interface: String,
    /// Seconds the guest gets to shut down when the pod terminates.
    pub stop_grace_seconds: u32,
}

/// Errors decoding a spec.
#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    #[error("{SPEC_ENV} is not set")]
    Missing,
    #[error("{SPEC_ENV} is not valid: {0}")]
    Invalid(#[from] serde_json::Error),
}

impl AnchorSpec {
    pub fn from_json(s: &str) -> Result<Self, SpecError> {
        Ok(serde_json::from_str(s)?)
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// The request sent to baryon-virtd.
    pub fn to_vm_spec(&self) -> VmSpec {
        VmSpec {
            vm_uid: self.vm_uid.clone(),
            vm_namespace: self.vm_namespace.clone(),
            vm_name: self.vm_name.clone(),
            vcpus: self.vcpus,
            memory_bytes: self.memory_bytes,
            boot: Some(Boot {
                payload: Some(boot::Payload::Firmware(FirmwareBoot {})),
            }),
            disks: vec![v1alpha1::Disk {
                name: self.root_disk.name.clone(),
                container_path: self.root_disk.container_path(),
                read_only: self.root_disk.read_only,
            }],
            interfaces: vec![NetworkInterface {
                name: "default".to_owned(),
                pod_interface: self.pod_interface.clone(),
            }],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_conversion() {
        let spec = AnchorSpec {
            vm_uid: "6b7f4c0e-6a39-4b8e-9d0e-2f1f0c6c4a11".into(),
            vm_namespace: "default".into(),
            vm_name: "vm1".into(),
            vcpus: 2,
            memory_bytes: 2 << 30,
            root_disk: AnchorDisk {
                name: "root".into(),
                mode: DiskMode::Filesystem,
                read_only: false,
            },
            pod_interface: "eth0".into(),
            stop_grace_seconds: 60,
        };
        let json = spec.to_json().unwrap();
        assert!(json.contains("\"rootDisk\""));
        assert_eq!(AnchorSpec::from_json(&json).unwrap(), spec);
        let vm = spec.to_vm_spec();
        assert_eq!(
            vm.disks[0].container_path,
            "/var/lib/baryon/disks/root/disk.img"
        );
        assert_eq!(vm.interfaces[0].pod_interface, "eth0");

        let block = AnchorDisk {
            name: "root".into(),
            mode: DiskMode::Block,
            read_only: true,
        };
        assert_eq!(block.container_path(), "/dev/baryon/root");
    }
}
