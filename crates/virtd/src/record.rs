//! Per-VM runtime records.
//!
//! A record is an index over kernel state, written when a VMM starts. After
//! a restart baryon-virtd uses it to find the VMM again and verifies every
//! field against the kernel before trusting it. Records live under the
//! runtime directory on tmpfs, so they disappear on reboot together with
//! the processes they describe.

use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Current record schema version.
pub const RECORD_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmRecord {
    pub version: u32,
    pub pod_uid: String,
    pub vm_uid: String,
    pub vm_namespace: String,
    pub vm_name: String,
    /// Protobuf encoding of the VmSpec, base16, so that a reattach can be
    /// checked for an identical spec.
    pub spec_hex: String,
    pub vmm_pid: i32,
    /// Process start time in clock ticks, from /proc/<pid>/stat.
    pub vmm_start_time: u64,
    pub vmm_uid: u32,
    pub pod_interface: String,
    pub vmm_cgroup: String,
    /// Absolute path of the pod cgroup, used to detect pod teardown.
    #[serde(default)]
    pub pod_cgroup: String,
    pub hypervisor_version: String,
    pub started_at_unix_ms: i64,
    pub guest_mac: String,
    /// When the guest first showed boot evidence (DHCP or a login prompt);
    /// 0 if it had not yet. A recovered VM is reported booted only if set.
    #[serde(default)]
    pub guest_booted_at_unix_ms: i64,
    /// Device numbers of the VM's block disks, so a restarted baryon-virtd
    /// keeps refusing them to other VMs.
    #[serde(default)]
    pub block_devices: Vec<u64>,
}

impl VmRecord {
    pub fn write(&self, dir: &Path) -> io::Result<()> {
        let tmp = dir.join("record.json.tmp");
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        // Root-only: the record names the VMM process and its cgroups.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&json)?;
        drop(f);
        std::fs::rename(tmp, dir.join("record.json"))
    }

    pub fn read(dir: &Path) -> io::Result<Self> {
        let bytes = std::fs::read(dir.join("record.json"))?;
        let rec: Self = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if rec.version != RECORD_VERSION {
            return Err(io::Error::other(format!(
                "unsupported record version {}",
                rec.version
            )));
        }
        Ok(rec)
    }
}

pub fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let rec = VmRecord {
            version: RECORD_VERSION,
            pod_uid: "p".into(),
            vm_uid: "v".into(),
            vm_namespace: "ns".into(),
            vm_name: "n".into(),
            spec_hex: to_hex(&[1, 2, 255]),
            vmm_pid: 42,
            vmm_start_time: 7,
            vmm_uid: 1_000_000_123,
            pod_interface: "eth0".into(),
            vmm_cgroup: "/sys/fs/cgroup/x".into(),
            pod_cgroup: "/sys/fs/cgroup".into(),
            hypervisor_version: "v53.0".into(),
            started_at_unix_ms: 1,
            guest_mac: "02:00:00:00:00:01".into(),
            guest_booted_at_unix_ms: 0,
            block_devices: vec![rustix::fs::makedev(251, 0)],
        };
        rec.write(dir.path()).unwrap();
        assert_eq!(VmRecord::read(dir.path()).unwrap(), rec);
        assert_eq!(from_hex(&rec.spec_hex).unwrap(), vec![1, 2, 255]);
        assert!(from_hex("abc").is_none());
        assert!(from_hex("zz").is_none());
    }
}
