//! baryon-virtd configuration.

use std::path::PathBuf;
use std::time::Duration;

use baryon_cloud_hypervisor::launch::Seccomp;

/// Runtime configuration, assembled from the command line.
#[derive(Debug, Clone)]
pub struct Config {
    /// Volatile runtime state: sockets and per-VM records. Must be a tmpfs
    /// (`/run/baryon`) so that nothing survives a reboot.
    pub runtime_dir: PathBuf,
    /// Persistent logs (`/var/log/baryon`).
    pub log_dir: PathBuf,
    pub cloud_hypervisor: PathBuf,
    pub firmware: PathBuf,
    /// cgroup2 mount point.
    pub cgroup_root: PathBuf,
    pub kvm_device: PathBuf,
    /// Component manifest written at image build time.
    pub components_manifest: PathBuf,
    /// How long a running VM waits for its anchor to reconnect after the
    /// anchor connection closed without a stop request.
    pub reattach_grace: Duration,
    /// How long a VM recovered after a baryon-virtd restart waits for its
    /// anchor to reconnect.
    pub recovery_grace: Duration,
    /// Guest shutdown time when the VM is stopped without an explicit grace
    /// period (for example after the reattach grace expired).
    pub default_stop_grace: Duration,
    pub seccomp: Seccomp,
    pub landlock: bool,
    /// Cloud Hypervisor log verbosity (number of `-v`).
    pub vmm_verbosity: u8,
}

impl Config {
    pub fn vms_dir(&self) -> PathBuf {
        self.runtime_dir.join("vms")
    }

    pub fn vm_dir(&self, pod_uid: &str) -> PathBuf {
        self.vms_dir().join(pod_uid)
    }

    pub fn anchor_socket(&self) -> PathBuf {
        self.runtime_dir.join("anchor").join("virtd.sock")
    }

    pub fn node_socket(&self) -> PathBuf {
        self.runtime_dir.join("node").join("virtd.sock")
    }
}
