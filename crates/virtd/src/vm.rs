//! VM supervision.
//!
//! Each VM has one supervisor task that owns every resource of the VM: the
//! VMM process, its runtime directory, the network handoff, the DHCP
//! responder process, and the serial capture task. All state changes go
//! through that task, so there is no shared mutable VM state. The manager
//! only indexes VMs by the UID of the pod that owns them.
//!
//! Lifetime rules (ADR 0005):
//!
//! - A VM is created by the first `Attach` from a pod and belongs to it.
//! - The VMM runs in the Anchor container's cgroup, so it never outlives the
//!   Anchor container: when the container stops or restarts, the container
//!   runtime stops the VMM with it, and a restarted anchor starts a new VM.
//! - The VMM does outlive baryon-virtd. After a restart, baryon-virtd
//!   re-adopts it from its runtime record, and the anchor reattaches.
//! - When the last session closes without a stop request (baryon-virtd
//!   restarted, or the anchor's connection broke), the VM keeps running for
//!   the reattach grace period, then it is stopped.
//! - A stop request with `final` set stops the VM and the VM is forgotten
//!   once the VMM has exited.

use std::collections::HashMap;
use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use prost::Message as _;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use baryon_cloud_hypervisor::ApiClient;
use baryon_cloud_hypervisor::launch::{
    ApiSocket, ConsoleMode, Disk, DiskSource, ImageType, LaunchConfig, MacAddr, Net, Payload,
    Platform,
};
use baryon_runtime_api::v1alpha1::{
    GuestStatus, NetworkState, NetworkStatus, VmPhase, VmSpec, VmStatus,
};

use crate::cgroup::{self, VmmCgroup};
use crate::config::Config;
use crate::dhcp_helper;
use crate::handoff::dhcp::{DhcpEvent, DnsConfig, LeaseConfig};
use crate::handoff::network::{self, PodInterface};
use crate::handoff::storage;
use crate::peer::AnchorPeer;
use crate::podcgroup::PodCgroup;
use crate::record::{self, VmRecord};
use crate::serial::{self, SerialEvent};
use crate::vmm::{self, Exit, VmmCredentials, VmmProcess};
use crate::vmm_files::{self, VmmDir};
use baryon_runtime_api::identity::VmIdentity;

/// Memory the VMM may use on top of guest memory before the pod limit is
/// considered too small. Cloud Hypervisor's own footprint is well below this.
pub const VMM_MEMORY_OVERHEAD: u64 = 64 * 1024 * 1024;

const STOP_ESCALATION_STEP: Duration = Duration::from_secs(5);

/// Interval at which the ACPI power button is pressed again during a stop.
const POWER_BUTTON_REPEAT: Duration = Duration::from_secs(5);

/// How often pod liveness is checked while no live anchor is attached.
const POD_LIVENESS_INTERVAL: Duration = Duration::from_millis(250);
/// Root-only subdirectory of a VM's runtime directory for baryon-virtd's own
/// sockets and logs.
const CONTROL_DIR: &str = "ctl";
/// Files in the VMM-owned `vmm` directory.
const SERIAL_SOCKET: &str = "serial.sock";
const VMM_LOG: &str = "cloud-hypervisor.log";
/// The VMM's stdout and stderr, in `ctl`.
const VMM_OUTPUT: &str = "cloud-hypervisor.out";
/// How much of a VMM log is read to explain a failure in status.
const STATUS_LOG_BYTES: u64 = 64 * 1024;
/// How much of each VMM log is kept under the log directory after a VM ends.
const PRESERVED_LOG_BYTES: u64 = 16 * 1024 * 1024;
/// Consecutive DHCP responder failures after which it is reported as
/// failing. baryon-virtd keeps restarting it while the VM runs: the
/// responder holds no state, and without it the guest loses its address.
const DHCP_FAILING_AFTER: u32 = 10;
/// A responder that ran this long resets the failure count.
const DHCP_STABLE: Duration = Duration::from_secs(300);
/// Upper bound of the restart backoff.
const DHCP_MAX_BACKOFF: Duration = Duration::from_secs(60);
/// How often a running responder is probed for liveness.
const DHCP_PROBE_INTERVAL: Duration = Duration::from_secs(10);
/// A responder that answered no probe for this long is hung: it is killed
/// and restarted. Two and a half probe intervals, so one late answer does
/// not count.
const DHCP_PROBE_TIMEOUT: Duration = Duration::from_secs(25);

/// Why the last responder stopped serving, for status messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DhcpFailure {
    StartFailed,
    Exited,
    Unresponsive,
    ProtocolViolation,
}

/// A reference to an anchor process, owned by the supervisor.
#[derive(Debug)]
pub struct AnchorRef {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
    pub pidfd: OwnedFd,
    pub cgroup: PodCgroup,
}

impl AnchorRef {
    pub fn from_peer(p: &AnchorPeer) -> io::Result<Self> {
        Ok(Self {
            pid: p.pid,
            uid: p.uid,
            gid: p.gid,
            groups: p.groups.clone(),
            pidfd: p.pidfd.try_clone()?,
            cgroup: p.cgroup.clone(),
        })
    }

    fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            pid: self.pid,
            uid: self.uid,
            gid: self.gid,
            groups: self.groups.clone(),
            pidfd: self.pidfd.try_clone()?,
            cgroup: self.cgroup.clone(),
        })
    }

    fn alive(&self) -> bool {
        crate::peer::pidfd_alive(self.pidfd.as_fd())
    }
}

#[derive(Debug)]
enum Command {
    Attach(AnchorRef),
    Detach,
    Stop {
        grace: Duration,
        final_: bool,
        reply: oneshot::Sender<VmStatus>,
    },
}

/// Errors returned to an attaching anchor.
#[derive(Debug, thiserror::Error)]
pub enum AttachError {
    #[error("invalid VM spec: {0}")]
    InvalidSpec(String),
    #[error("a different VM spec is already running for this pod; restart the pod to change it")]
    SpecMismatch,
    /// Another VM on this node already runs with the same host identity.
    /// The message starts with a machine-readable reason, which the anchor
    /// passes on and baryon-controller acts on (it places the VM elsewhere).
    #[error(
        "IdentityConflict: another VM on this node runs with the same host identity; this VM cannot run on this node"
    )]
    IdentityConflict,
    #[error("the caller (uid {uid}, gid {gid}) does not run as the host identity of VM {vm_uid}")]
    IdentityMismatch { uid: u32, gid: u32, vm_uid: String },
    #[error("internal error: {0}")]
    Internal(String),
}

/// One VM known to the manager.
#[derive(Debug)]
pub struct VmEntry {
    pub pod_uid: String,
    pub spec: VmSpec,
    pub vmm_uid: u32,
    /// Device numbers of the block devices the VM's VMM uses. Two VMs on a
    /// node never use the same block device: the VMM's file lock does not
    /// exclude another container's node for the same device.
    block_devices: Mutex<Vec<u64>>,
    status: watch::Sender<VmStatus>,
    cmd: mpsc::Sender<Command>,
}

/// Ends an anchor session when dropped.
#[derive(Debug)]
pub struct SessionGuard {
    cmd: mpsc::Sender<Command>,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        // A closed channel means the VM has already finished; nothing to do.
        if let Err(mpsc::error::TrySendError::Full(_)) = self.cmd.try_send(Command::Detach) {
            tracing::warn!("could not deliver session end to VM supervisor: queue full");
        }
    }
}

/// Index of VMs on this node.
#[derive(Debug)]
pub struct Manager {
    cfg: Arc<Config>,
    vms: Mutex<HashMap<String, Arc<VmEntry>>>,
    generation: watch::Sender<u64>,
    hypervisor_version: String,
}

impl Manager {
    pub fn new(cfg: Arc<Config>, hypervisor_version: String) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            vms: Mutex::new(HashMap::new()),
            generation: watch::channel(0).0,
            hypervisor_version,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<VmEntry>>> {
        // A poisoned lock means a panic while holding it; the map itself is
        // still consistent because every critical section is a single insert,
        // remove, or read.
        self.vms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn bump(&self) {
        self.generation.send_modify(|g| *g = g.wrapping_add(1));
    }

    /// Current status of every VM.
    pub fn list(&self) -> Vec<VmStatus> {
        let mut out: Vec<VmStatus> = self
            .lock()
            .values()
            .map(|e| e.status.borrow().clone())
            .collect();
        out.sort_by(|a, b| a.pod_uid.cmp(&b.pod_uid));
        out
    }

    /// Notified after every VM status change.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.generation.subscribe()
    }

    /// Attach an anchor session. Creates the VM if this pod has none.
    pub fn attach(
        self: &Arc<Self>,
        peer: &AnchorPeer,
        spec: VmSpec,
    ) -> Result<(watch::Receiver<VmStatus>, SessionGuard), AttachError> {
        validate_spec(&spec)?;
        // The controller runs each Anchor Pod as its VM's host identity.
        // Requiring the same identity here means a pod cannot drive a VM it
        // was not created for, and cannot take the identity of another VM.
        VmIdentity::for_vm(&spec.vm_uid)
            .and_then(|id| id.verify(peer.uid, peer.gid))
            .map_err(|_| AttachError::IdentityMismatch {
                uid: peer.uid,
                gid: peer.gid,
                vm_uid: spec.vm_uid.clone(),
            })?;
        let anchor =
            AnchorRef::from_peer(peer).map_err(|e| AttachError::Internal(e.to_string()))?;
        let pod_uid = anchor.cgroup.pod_uid.clone();

        let mut vms = self.lock();
        if let Some(entry) = vms.get(&pod_uid) {
            if entry.spec != spec {
                return Err(AttachError::SpecMismatch);
            }
            let rx = entry.status.subscribe();
            entry
                .cmd
                .try_send(Command::Attach(anchor))
                .map_err(|e| AttachError::Internal(e.to_string()))?;
            return Ok((
                rx,
                SessionGuard {
                    cmd: entry.cmd.clone(),
                },
            ));
        }
        // Two VMs must never share a host identity on a node: they could
        // signal, trace, and read each other. A collision is refused, never
        // tolerated.
        if let Some(other) = vms.values().find(|e| e.vmm_uid == anchor.uid) {
            tracing::warn!(pod = %pod_uid, other = %other.pod_uid, "VM host identity collision");
            return Err(AttachError::IdentityConflict);
        }

        let (status_tx, status_rx) = watch::channel(initial_status(&pod_uid, &spec, self));
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let entry = Arc::new(VmEntry {
            pod_uid: pod_uid.clone(),
            spec: spec.clone(),
            vmm_uid: anchor.uid,
            block_devices: Mutex::new(Vec::new()),
            status: status_tx,
            cmd: cmd_tx.clone(),
        });
        vms.insert(pod_uid.clone(), entry.clone());
        drop(vms);
        self.bump();

        let sup = Supervisor::new(self, entry, cmd_rx);
        tokio::spawn(sup.run_new(anchor));
        Ok((status_rx, SessionGuard { cmd: cmd_tx }))
    }

    /// Stop the VM of a pod and wait until it has stopped.
    pub async fn stop(&self, pod_uid: &str, grace: Duration, final_: bool) -> Option<VmStatus> {
        let entry = self.lock().get(pod_uid).cloned()?;
        let (reply, rx) = oneshot::channel();
        if entry
            .cmd
            .send(Command::Stop {
                grace,
                final_,
                reply,
            })
            .await
            .is_err()
        {
            return Some(entry.status.borrow().clone());
        }
        rx.await.ok()
    }

    /// Record that the VM of `pod_uid` uses the block devices `devs`, unless
    /// another VM on the node uses one of them.
    fn claim_block_devices(&self, pod_uid: &str, devs: &[u64]) -> Result<(), String> {
        let vms = self.lock();
        for e in vms.values().filter(|e| e.pod_uid != pod_uid) {
            let theirs = e
                .block_devices
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(d) = devs.iter().find(|d| theirs.contains(d)) {
                return Err(format!(
                    "block device {}:{} is used by the VM {}/{} on this node",
                    rustix::fs::major(*d),
                    rustix::fs::minor(*d),
                    e.spec.vm_namespace,
                    e.spec.vm_name
                ));
            }
        }
        if let Some(e) = vms.get(pod_uid) {
            *e.block_devices
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = devs.to_vec();
        }
        Ok(())
    }

    fn remove(&self, pod_uid: &str) {
        self.lock().remove(pod_uid);
        self.bump();
    }

    /// Re-adopt VMMs described by runtime records after a restart. Records
    /// whose process is gone are cleaned up.
    pub fn recover(self: &Arc<Self>) -> usize {
        let dir = self.cfg.vms_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return 0,
        };
        let mut recovered = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            let rec = match VmRecord::read(&path) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(dir = %path.display(), error = %e, "discarding unreadable runtime record");
                    vmm_files::remove_vm_dir(&path);
                    continue;
                }
            };
            match self.adopt(&rec) {
                Ok(()) => recovered += 1,
                Err(reason) => {
                    tracing::info!(pod = %rec.pod_uid, reason, "runtime record does not match a live VMM; cleaning up");
                    vmm_files::remove_vm_dir(&path);
                }
            }
        }
        recovered
    }

    fn adopt(self: &Arc<Self>, rec: &VmRecord) -> Result<(), String> {
        let start = vmm::start_time(rec.vmm_pid).map_err(|e| format!("process gone: {e}"))?;
        if start != rec.vmm_start_time {
            return Err("pid reused by another process".into());
        }
        let exe = std::fs::read_link(format!("/proc/{}/exe", rec.vmm_pid))
            .map_err(|e| format!("cannot read exe: {e}"))?;
        if exe != self.cfg.cloud_hypervisor {
            return Err(format!("pid runs {}, not Cloud Hypervisor", exe.display()));
        }
        let pidfd = vmm::open_pidfd(rec.vmm_pid).map_err(|e| e.to_string())?;
        // The pidfd was opened after the start time check; check again so the
        // pidfd is known to refer to the recorded process instance.
        if vmm::start_time(rec.vmm_pid).ok() != Some(rec.vmm_start_time) {
            return Err("process exited during recovery".into());
        }
        let spec_bytes = record::from_hex(&rec.spec_hex).ok_or("invalid spec encoding")?;
        let spec = VmSpec::decode(spec_bytes.as_slice()).map_err(|e| e.to_string())?;

        let mut status = initial_status(&rec.pod_uid, &spec, self);
        // Running means the guest showed boot evidence; that is known only
        // if the previous baryon-virtd recorded it.
        if rec.guest_booted_at_unix_ms > 0 {
            status.phase = VmPhase::Running as i32;
            status.reason = "Recovered".into();
            status.message = "VMM re-adopted after a baryon-virtd restart".into();
        } else {
            status.phase = VmPhase::Starting as i32;
            status.reason = "Booting".into();
            status.message =
                "VMM re-adopted after a baryon-virtd restart; waiting for guest boot evidence"
                    .into();
        }
        status.started_at_unix_ms = rec.started_at_unix_ms;
        status.hypervisor_version = rec.hypervisor_version.clone();
        if let Some(g) = status.guest.as_mut() {
            g.mac_address = rec.guest_mac.clone();
            g.booted_at_unix_ms = rec.guest_booted_at_unix_ms;
        }

        let (status_tx, _) = watch::channel(status);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let entry = Arc::new(VmEntry {
            pod_uid: rec.pod_uid.clone(),
            spec,
            vmm_uid: rec.vmm_uid,
            block_devices: Mutex::new(rec.block_devices.clone()),
            status: status_tx,
            cmd: cmd_tx,
        });
        self.lock().insert(rec.pod_uid.clone(), entry.clone());
        self.bump();

        let mut sup = Supervisor::new(self, entry, cmd_rx);
        sup.record = Some(rec.clone());
        sup.dhcp_last_ack = rec.dhcp_last_ack_unix_ms;
        sup.vmm = Some(VmmProcess {
            pid: rec.vmm_pid,
            pidfd,
            is_child: false,
        });
        sup.vmm_cgroup = Some(VmmCgroup {
            path: PathBuf::from(&rec.vmm_cgroup),
        });
        if !rec.pod_cgroup.is_empty() {
            sup.pod_cgroup = Some(PathBuf::from(&rec.pod_cgroup));
        }
        sup.pod_interface = rec.pod_interface.clone();
        sup.detach_deadline = Some(Instant::now() + self.cfg.recovery_grace);
        tracing::info!(pod = %rec.pod_uid, pid = rec.vmm_pid, "recovered running VM");
        tokio::spawn(sup.run_recovered());
        Ok(())
    }
}

fn validate_spec(spec: &VmSpec) -> Result<(), AttachError> {
    let bad = |m: &str| Err(AttachError::InvalidSpec(m.to_owned()));
    // These strings come from the anchor and are untrusted. They end up in
    // log paths, SMBIOS strings, and status, so they must have Kubernetes
    // object name syntax.
    if !is_uuid(&spec.vm_uid) {
        return bad("vm_uid must be a UUID");
    }
    if !is_dns_label(&spec.vm_namespace) {
        return bad("vm_namespace must be a DNS-1123 label");
    }
    // Also the guest hostname (DHCP option 12, NoCloud seed).
    if !is_dns_label(&spec.vm_name) {
        return bad("vm_name must be a DNS-1123 label");
    }
    if spec.vcpus == 0 || spec.vcpus > 256 {
        return bad("vcpus must be between 1 and 256");
    }
    if spec.memory_bytes < 64 * 1024 * 1024 || !spec.memory_bytes.is_multiple_of(1024 * 1024) {
        return bad("memory must be at least 64 MiB and a multiple of 1 MiB");
    }
    if spec.disks.is_empty() || spec.disks.len() > 8 {
        return bad("between 1 and 8 disks are required");
    }
    if spec.interfaces.len() != 1 {
        return bad("exactly one network interface is supported");
    }
    if spec
        .boot
        .as_ref()
        .and_then(|b| b.payload.as_ref())
        .is_none()
    {
        return bad("a boot payload is required");
    }
    Ok(())
}

fn is_uuid(s: &str) -> bool {
    let lens = [8, 4, 4, 4, 12];
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == lens.len()
        && groups
            .iter()
            .zip(lens)
            .all(|(g, l)| g.len() == l && g.chars().all(|c| c.is_ascii_hexdigit()))
}

fn is_dns_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// The reported network status for a responder `state` (and when it began),
/// `failures` since it last ran stably, and the last lease acknowledgement
/// (Unix ms, 0 if none). Messages hold no relative times: they stay in the
/// VM's condition until the state changes.
fn network_status(
    state: (NetworkState, i64),
    failures: u32,
    cause: DhcpFailure,
    last_ack: i64,
) -> NetworkStatus {
    let lease_expires = if last_ack > 0 {
        last_ack.saturating_add(i64::from(crate::handoff::dhcp::LEASE_SECONDS) * 1000)
    } else {
        0
    };
    let lease = if last_ack > 0 {
        "the guest's lease stays valid until it expires"
    } else {
        "the guest has not acquired an address"
    };
    let retry = format!(
        "baryon-virtd restarts it with backoff of at most {} s",
        DHCP_MAX_BACKOFF.as_secs()
    );
    let stopped = match cause {
        DhcpFailure::StartFailed => "the DHCP responder could not be started",
        DhcpFailure::Exited => "the DHCP responder stopped",
        DhcpFailure::Unresponsive => {
            "the DHCP responder stopped answering liveness probes and was stopped"
        }
        DhcpFailure::ProtocolViolation => {
            "the DHCP responder wrote output that breaks its protocol and was stopped"
        }
    };
    let (reason, message) = match state.0 {
        NetworkState::Ready => (
            "AddressServiceRunning",
            "the DHCP responder for the guest runs and answers liveness probes".to_owned(),
        ),
        NetworkState::Restarting => (
            "DhcpResponderRestarting",
            format!("{stopped}; {retry}; {lease}"),
        ),
        NetworkState::Failing => (
            "DhcpResponderFailing",
            format!(
                "the DHCP responder failed {failures} times in a row (last: {stopped}); {retry}; {lease}"
            ),
        ),
        NetworkState::Unspecified => ("Unknown", String::new()),
    };
    NetworkStatus {
        state: state.0 as i32,
        reason: reason.to_owned(),
        message,
        since_unix_ms: state.1,
        consecutive_failures: failures,
        last_ack_unix_ms: last_ack,
        lease_expires_unix_ms: lease_expires,
    }
}

fn initial_status(pod_uid: &str, spec: &VmSpec, m: &Manager) -> VmStatus {
    VmStatus {
        pod_uid: pod_uid.to_owned(),
        vm_uid: spec.vm_uid.clone(),
        vm_namespace: spec.vm_namespace.clone(),
        vm_name: spec.vm_name.clone(),
        phase: VmPhase::Preparing as i32,
        reason: "Preparing".into(),
        message: String::new(),
        started_at_unix_ms: 0,
        stopped_at_unix_ms: 0,
        guest: Some(GuestStatus::default()),
        hypervisor_version: m.hypervisor_version.clone(),
        network: None,
    }
}

#[derive(Debug, Clone)]
struct StopState {
    reason: &'static str,
    requested: bool,
}

/// Failure while preparing or starting a VM, with a status reason.
#[derive(Debug)]
struct StartFailure {
    reason: &'static str,
    message: String,
}

impl StartFailure {
    fn new(reason: &'static str, e: impl std::fmt::Display) -> Self {
        Self {
            reason,
            message: e.to_string(),
        }
    }
}

struct Supervisor {
    cfg: Arc<Config>,
    manager: Weak<Manager>,
    entry: Arc<VmEntry>,
    /// The runtime record as last written, so it can be updated.
    record: Option<VmRecord>,
    /// Shared BSD lock on the VM's log directory while the VM runs.
    log_dir_lock: Option<OwnedFd>,
    cmd_rx: mpsc::Receiver<Command>,
    anchor: Option<AnchorRef>,
    sessions: usize,
    vmm: Option<VmmProcess>,
    vmm_cgroup: Option<VmmCgroup>,
    /// Pod cgroup directory, used to tell an anchor restart from pod teardown.
    pod_cgroup: Option<PathBuf>,
    /// The current anchor process has exited and no new one attached yet.
    anchor_exited: bool,
    pod_interface: String,
    lease: Option<LeaseConfig>,
    helpers: CancellationToken,
    /// The DHCP responder process, while it runs.
    dhcp: Option<dhcp_helper::Responder>,
    dhcp_failures: u32,
    dhcp_last_failure: DhcpFailure,
    /// Liveness probes: the last probe number sent, the last one sent
    /// before the current responder started, when the current responder
    /// last answered (or started), and whether it has been reported Ready.
    dhcp_probe_seq: u64,
    dhcp_probe_base: u64,
    /// The last probe the current responder answered.
    dhcp_probe_answered: u64,
    dhcp_alive_at: Option<std::time::Instant>,
    dhcp_answered: bool,
    /// Reported state of the responder and when it last changed.
    dhcp_state: (NetworkState, i64),
    /// Last DHCP acknowledgement to the guest, Unix ms, 0 if none.
    dhcp_last_ack: i64,
    /// When an acknowledgement was last processed. The responder already
    /// reports at most one per interval; this bounds the work here even if
    /// a responder reports more.
    dhcp_ack_processed: Option<Instant>,
    dhcp_restart_at: Option<Instant>,
    dhcp_tx: mpsc::Sender<DhcpEvent>,
    dhcp_rx: mpsc::Receiver<DhcpEvent>,
    serial_tx: mpsc::Sender<SerialEvent>,
    serial_rx: mpsc::Receiver<SerialEvent>,
    detach_deadline: Option<Instant>,
    stopping: Option<StopState>,
    escalation: Option<tokio::task::JoinHandle<()>>,
    stop_replies: Vec<oneshot::Sender<VmStatus>>,
    final_stop: bool,
}

impl Supervisor {
    fn new(manager: &Arc<Manager>, entry: Arc<VmEntry>, cmd_rx: mpsc::Receiver<Command>) -> Self {
        let (dhcp_tx, dhcp_rx) = mpsc::channel(8);
        let (serial_tx, serial_rx) = mpsc::channel(8);
        let pod_interface = entry
            .spec
            .interfaces
            .first()
            .map(|i| i.pod_interface.clone())
            .unwrap_or_else(|| "eth0".into());
        Self {
            cfg: manager.cfg.clone(),
            record: None,
            log_dir_lock: None,
            manager: Arc::downgrade(manager),
            entry,
            cmd_rx,
            anchor: None,
            sessions: 0,
            vmm: None,
            vmm_cgroup: None,
            pod_cgroup: None,
            anchor_exited: false,
            pod_interface,
            lease: None,
            helpers: CancellationToken::new(),
            dhcp: None,
            dhcp_failures: 0,
            dhcp_last_failure: DhcpFailure::Exited,
            dhcp_probe_seq: 0,
            dhcp_probe_base: 0,
            dhcp_probe_answered: 0,
            dhcp_alive_at: None,
            dhcp_answered: false,
            dhcp_state: (NetworkState::Unspecified, 0),
            dhcp_last_ack: 0,
            dhcp_ack_processed: None,
            dhcp_restart_at: None,
            dhcp_tx,
            dhcp_rx,
            serial_tx,
            serial_rx,
            detach_deadline: None,
            stopping: None,
            escalation: None,
            stop_replies: Vec::new(),
            final_stop: false,
        }
    }

    fn pod_uid(&self) -> &str {
        &self.entry.pod_uid
    }

    fn vm_dir(&self) -> PathBuf {
        self.cfg.vm_dir(&self.entry.pod_uid)
    }

    /// The end of a log file the VMM wrote, read without trusting the
    /// VMM-owned directory (see `crate::vmm_files`).
    fn vmm_log_tail_max(&self, name: &str, max: u64) -> Option<Vec<u8>> {
        let dir = VmmDir::open(&self.vm_dir().join("vmm"), self.entry.vmm_uid).ok()?;
        match dir.read_tail(name, max) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => {
                tracing::warn!(pod = %self.pod_uid(), file = name, error = %e, "not reading VMM file");
                None
            }
        }
    }

    fn vmm_log_tail(&self, name: &str) -> Option<Vec<u8>> {
        self.vmm_log_tail_max(name, STATUS_LOG_BYTES)
    }

    /// The end of the VMM's stdout and stderr, which baryon-virtd writes to a
    /// root-owned file.
    fn vmm_output_tail(&self) -> Option<Vec<u8>> {
        read_tail_plain(
            &self.vm_dir().join(CONTROL_DIR).join(VMM_OUTPUT),
            STATUS_LOG_BYTES,
        )
        .ok()
    }

    fn api_socket(&self) -> PathBuf {
        self.vm_dir().join(CONTROL_DIR).join("api.sock")
    }

    fn publish(&self, f: impl FnOnce(&mut VmStatus)) {
        self.entry.status.send_modify(f);
        if let Some(m) = self.manager.upgrade() {
            m.bump();
        }
    }

    fn set_phase(&self, phase: VmPhase, reason: &str, message: impl Into<String>) {
        let message = message.into();
        tracing::info!(pod = %self.entry.pod_uid, vm = %self.entry.spec.vm_name, ?phase, reason, %message, "VM status");
        self.publish(|s| {
            s.phase = phase as i32;
            s.reason = reason.to_owned();
            s.message = message;
        });
    }

    async fn run_new(mut self, anchor: AnchorRef) {
        self.sessions = 1;
        match self.prepare_and_start(&anchor).await {
            Ok(()) => {
                self.anchor = Some(anchor);
                self.supervise().await;
                self.finish().await;
            }
            Err(f) => {
                tracing::warn!(pod = %self.pod_uid(), reason = f.reason, message = %f.message, "VM failed to start");
                self.anchor = Some(anchor);
                self.cleanup().await;
                self.publish(|s| {
                    s.phase = VmPhase::Failed as i32;
                    s.reason = f.reason.to_owned();
                    s.message = f.message;
                    s.stopped_at_unix_ms = now_ms();
                });
                self.finish().await;
            }
        }
    }

    async fn run_recovered(mut self) {
        self.lock_log_dir();
        self.start_serial();
        self.supervise().await;
        self.finish().await;
    }

    async fn prepare_and_start(&mut self, anchor: &AnchorRef) -> Result<(), StartFailure> {
        // Refuse before touching anything: the VMM will join the Anchor
        // container's cgroup, and stopping that systemd scope is what stops
        // the VMM before kubelet tears down the pod's volumes and network,
        // also while baryon-virtd is down (ADR 0005). Under the cgroupfs
        // driver nothing outside baryon-virtd does that.
        if !anchor.cgroup.systemd_managed() {
            return Err(StartFailure::new(
                "CgroupDriverUnsupported",
                format!(
                    "the Anchor container cgroup {} is not a systemd scope; Baryon requires the kubelet systemd cgroup driver",
                    anchor.cgroup.container_path
                ),
            ));
        }
        let spec = self.entry.spec.clone();
        let dir = self.vm_dir();
        let vmm_dir = dir.join("vmm");

        // Runtime directory: root-owned, traversable; the vmm subdirectory is
        // a tmpfs that belongs to the VMM user, which creates its sockets and
        // logs there.
        if dir.exists() {
            vmm_files::remove_vm_dir(&dir);
        }
        create_dir_mode(&dir, 0o711).map_err(|e| StartFailure::new("RuntimeDirFailed", e))?;
        create_dir_mode(&vmm_dir, 0o700).map_err(|e| StartFailure::new("RuntimeDirFailed", e))?;
        // A size-limited tmpfs of the VMM's own, owned by the VM's uid.
        vmm_files::mount_vmm_dir(&vmm_dir, anchor.uid, anchor.gid)
            .map_err(|e| StartFailure::new("RuntimeDirFailed", e))?;
        // The Cloud Hypervisor API has no authentication, so its socket lives
        // in a root-only directory. The VMM uses the inherited listener and
        // never needs the path.
        create_dir_mode(&dir.join(CONTROL_DIR), 0o700)
            .map_err(|e| StartFailure::new("RuntimeDirFailed", e))?;

        // Storage: resolve every disk inside the anchor container root.
        let root = storage::open_process_root(anchor.pid, anchor.pidfd.as_fd())
            .map_err(|e| StartFailure::new("StorageFailed", e))?;
        let mut disks = Vec::new();
        for d in &spec.disks {
            let resolved =
                storage::resolve_disk(root.as_fd(), &d.name, &d.container_path, d.read_only)
                    .map_err(|e| StartFailure::new("StorageFailed", e))?;
            if resolved.kind == storage::DiskKind::BlockDevice {
                storage::check_device_access(&resolved, anchor.uid, anchor.gid, &anchor.groups)
                    .map_err(|e| StartFailure::new("BlockDeviceAccessDenied", e))?;
            }
            tracing::info!(pod = %self.entry.pod_uid, disk = %d.name, kind = ?resolved.kind, size = resolved.size_bytes, "disk resolved in anchor mount namespace");
            disks.push(resolved);
        }
        let block_devices: Vec<u64> = disks
            .iter()
            .filter(|d| d.kind == storage::DiskKind::BlockDevice)
            .map(|d| d.rdev)
            .collect();
        if let Some(m) = self.manager.upgrade() {
            m.claim_block_devices(&self.entry.pod_uid, &block_devices)
                .map_err(|e| StartFailure::new("DiskInUse", e))?;
        }
        let resolv = read_container_file(root.as_fd(), "/etc/resolv.conf").unwrap_or_default();

        // Resources: the pod limits bound the VMM; refuse a spec that cannot fit.
        let limits = cgroup::pod_limits(&self.cfg.cgroup_root, &anchor.cgroup.pod_path)
            .map_err(|e| StartFailure::new("ResourcesInvalid", e))?;
        // Without a pod memory limit the VMM would run unaccounted; this
        // happens when any container in the pod (including init containers)
        // has no memory limit.
        let Some(mem) = limits.memory_bytes else {
            return Err(StartFailure::new(
                "ResourcesUnbounded",
                "the pod has no memory limit; every container of an Anchor Pod must set one",
            ));
        };
        if spec.memory_bytes + VMM_MEMORY_OVERHEAD > mem {
            return Err(StartFailure::new(
                "ResourcesExceedPodLimits",
                format!(
                    "guest memory {} plus VMM overhead {} exceeds pod memory limit {}",
                    spec.memory_bytes, VMM_MEMORY_OVERHEAD, mem
                ),
            ));
        }
        if let Some(cpu) = limits.cpu_millis
            && u64::from(spec.vcpus) * 1000 > cpu
        {
            return Err(StartFailure::new(
                "ResourcesExceedPodLimits",
                format!(
                    "{} vCPUs exceed pod CPU limit of {} millicores",
                    spec.vcpus, cpu
                ),
            ));
        }

        // Network: TAP and passthrough inside the pod network namespace.
        let pidfd = anchor
            .pidfd
            .try_clone()
            .map_err(|e| StartFailure::new("NetworkFailed", e))?;
        let pod_if = self.pod_interface.clone();
        let guest_mac = vm_mac(&spec.vm_uid);
        let handoff = tokio::task::spawn_blocking(move || {
            network::prepare(pidfd.as_fd(), &pod_if, guest_mac)
        })
        .await
        .map_err(|e| StartFailure::new("NetworkFailed", e))?
        .map_err(|e| StartFailure::new("NetworkFailed", e))?;
        let pod = handoff.pod_interface.clone();

        // cgroup: the Anchor container's (checked above).
        let vmm_cgroup = VmmCgroup::existing(&self.cfg.cgroup_root, &anchor.cgroup.container_path)
            .map_err(|e| StartFailure::new("CgroupFailed", e))?;
        self.pod_cgroup = Some(
            self.cfg
                .cgroup_root
                .join(anchor.cgroup.pod_path.trim_start_matches('/')),
        );
        let procs = vmm_cgroup
            .open_procs()
            .map_err(|e| StartFailure::new("CgroupFailed", e))?;
        self.vmm_cgroup = Some(vmm_cgroup.clone());

        // API socket: bound here, inherited by the VMM.
        let api_path = self.api_socket();
        let api_listener = std::os::unix::net::UnixListener::bind(&api_path)
            .map_err(|e| StartFailure::new("VmmStartFailed", e))?;
        let api_fd: OwnedFd = api_listener.into();

        let serial_path = vmm_dir.join(SERIAL_SOCKET);
        let _ = std::fs::remove_file(&serial_path);

        let disk_cfgs: Vec<Disk> = disks
            .iter()
            .enumerate()
            .map(|(i, d)| Disk {
                id: format!("disk{i}"),
                source: DiskSource::InheritedFd(d.fd.as_raw_fd()),
                image_type: ImageType::Raw,
                readonly: d.read_only,
                direct: false,
                serial: Some(format!("baryon-{}", sanitize_ident(&d.name))),
            })
            .collect();
        let launch = LaunchConfig {
            api_socket: ApiSocket::Fd(api_fd.as_raw_fd()),
            event_monitor: vmm_dir.join("events.json"),
            log_file: vmm_dir.join("cloud-hypervisor.log"),
            verbosity: self.cfg.vmm_verbosity,
            vcpus: spec.vcpus,
            memory_mib: spec.memory_bytes / (1024 * 1024),
            payload: Payload::Firmware(self.cfg.firmware.clone()),
            disks: disk_cfgs,
            nets: vec![Net {
                id: "net0".into(),
                tap_fds: vec![handoff.tap_fd.as_raw_fd()],
                mac: pod.mac,
            }],
            serial: ConsoleMode::Socket(serial_path),
            console: ConsoleMode::Off,
            platform: Platform {
                system_uuid: Some(spec.vm_uid.clone()),
                // cloud-init NoCloud reads instance-id and hostname from the
                // SMBIOS serial number. With no network-config it applies its
                // fallback (DHCP on the first NIC), which the DHCP responder
                // answers. No metadata server is involved.
                system_serial_number: Some(nocloud_serial(&spec.vm_uid, &spec.vm_name)),
                system_product_name: Some("Baryon VirtualMachine".into()),
            },
            seccomp: self.cfg.seccomp,
            landlock: self.cfg.landlock,
            // The VMM's own directory (logs, events, serial socket), so a
            // guest reboot can re-create the serial socket.
            landlock_writable_dirs: vec![vmm_dir.clone()],
        };
        let args = launch
            .to_args()
            .map_err(|e| StartFailure::new("VmmStartFailed", e))?;

        let kvm_gid = std::fs::metadata(&self.cfg.kvm_device)
            .map(|m| std::os::unix::fs::MetadataExt::gid(&m))
            .map_err(|e| StartFailure::new("KvmUnavailable", e))?;
        let mut groups = anchor.groups.clone();
        if !groups.contains(&kvm_gid) {
            groups.push(kvm_gid);
        }

        let mut inherit: Vec<std::os::fd::BorrowedFd<'_>> =
            vec![api_fd.as_fd(), handoff.tap_fd.as_fd()];
        inherit.extend(disks.iter().map(|d| d.fd.as_fd()));

        let process = vmm::spawn(vmm::SpawnRequest {
            binary: &self.cfg.cloud_hypervisor,
            arg0: None,
            args,
            inherit,
            credentials: VmmCredentials {
                uid: anchor.uid,
                gid: anchor.gid,
                groups,
            },
            cgroup_procs: procs.as_fd(),
            output: std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(dir.join(CONTROL_DIR).join(VMM_OUTPUT))
                .ok(),
            stdin_pipe: false,
            stdout_pipe: false,
        })
        .map_err(|e| StartFailure::new("VmmStartFailed", e))?
        .process;
        // The VMM holds its own copies of the descriptors now.
        drop(api_fd);
        drop(disks);
        let tap_fd = handoff.tap_fd;
        drop(tap_fd);

        let started = now_ms();
        let start_time = vmm::start_time(process.pid).unwrap_or(0);
        let record = VmRecord {
            version: record::RECORD_VERSION,
            pod_uid: self.entry.pod_uid.clone(),
            vm_uid: spec.vm_uid.clone(),
            vm_namespace: spec.vm_namespace.clone(),
            vm_name: spec.vm_name.clone(),
            spec_hex: record::to_hex(&spec.encode_to_vec()),
            vmm_pid: process.pid,
            vmm_start_time: start_time,
            vmm_uid: anchor.uid,
            pod_interface: self.pod_interface.clone(),
            vmm_cgroup: vmm_cgroup.path.display().to_string(),
            pod_cgroup: self
                .pod_cgroup
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            hypervisor_version: self.entry.status.borrow().hypervisor_version.clone(),
            started_at_unix_ms: started,
            guest_mac: pod.mac.to_string(),
            guest_booted_at_unix_ms: 0,
            dhcp_last_ack_unix_ms: 0,
            block_devices,
        };
        if let Err(e) = record.write(&dir) {
            tracing::warn!(error = %e, "cannot write runtime record; recovery after restart will not find this VM");
        }
        self.record = Some(record);
        tracing::info!(pod = %self.pod_uid(), pid = process.pid, uid = anchor.uid, mac = %pod.mac, ip = %pod.ipv4.address, "VMM started");
        self.vmm = Some(process);

        self.lease = Some(LeaseConfig {
            client_mac: pod.mac,
            ipv4: pod.ipv4.clone(),
            mtu: pod.mtu,
            dns: DnsConfig::parse_resolv_conf(&resolv),
            hostname: spec.vm_name.clone(),
        });
        self.start_dhcp(handoff.dhcp_socket, anchor);
        self.lock_log_dir();
        self.start_serial();
        self.publish(|s| {
            s.phase = VmPhase::Starting as i32;
            s.reason = "Booting".into();
            s.message = format!("VMM started, guest address {}", pod.ipv4.address);
            s.started_at_unix_ms = started;
            if let Some(g) = s.guest.as_mut() {
                g.mac_address = pod.mac.to_string();
                g.ip_address = pod.ipv4.address.to_string();
            }
        });
        Ok(())
    }

    /// Start the DHCP responder process on `socket`, with the anchor's
    /// identity and in the VMM's cgroup (see `crate::dhcp_helper`).
    fn start_dhcp(&mut self, socket: OwnedFd, anchor: &AnchorRef) {
        let Some(lease) = self.lease.clone() else {
            return;
        };
        let procs = match self.vmm_cgroup.as_ref().map(VmmCgroup::open_procs) {
            Some(Ok(p)) => p,
            Some(Err(e)) => {
                tracing::warn!(pod = %self.pod_uid(), error = %e, "cannot open the VMM cgroup for the DHCP responder");
                self.schedule_dhcp_restart(DhcpFailure::StartFailed);
                return;
            }
            None => return,
        };
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.vm_dir().join(CONTROL_DIR).join("dhcp.log"))
            .ok();
        let started = dhcp_helper::start(dhcp_helper::StartRequest {
            socket,
            lease: &lease,
            credentials: VmmCredentials {
                uid: anchor.uid,
                gid: anchor.gid,
                groups: anchor.groups.clone(),
            },
            cgroup_procs: procs.as_fd(),
            log,
            pod_uid: &self.entry.pod_uid,
            events: self.dhcp_tx.clone(),
        });
        match started {
            Ok(r) => {
                tracing::info!(pod = %self.pod_uid(), pid = r.process.pid, "DHCP responder started");
                self.dhcp = Some(r);
                // Ready only once it answers a probe: a running process is
                // not evidence that it serves.
                self.dhcp_probe_base = self.dhcp_probe_seq;
                self.dhcp_probe_answered = self.dhcp_probe_seq;
                self.dhcp_alive_at = Some(std::time::Instant::now());
                self.dhcp_answered = false;
                self.probe_dhcp();
            }
            Err(e) => {
                tracing::warn!(pod = %self.pod_uid(), error = %e, "cannot start the DHCP responder");
                self.schedule_dhcp_restart(DhcpFailure::StartFailed);
            }
        }
    }

    /// The responder exited while the VM runs: restart it with backoff for as
    /// long as the VM runs.
    fn on_dhcp_exit(&mut self) {
        let Some(r) = self.dhcp.take() else {
            return;
        };
        let ran = r.started.elapsed();
        let exit = r.reap();
        if self.stopping.is_some() {
            return;
        }
        tracing::warn!(pod = %self.pod_uid(), ?exit, ran_s = ran.as_secs(), "DHCP responder exited");
        if ran >= DHCP_STABLE {
            self.dhcp_failures = 0;
        }
        self.schedule_dhcp_restart(DhcpFailure::Exited);
    }

    /// Probe the running responder, or restart it if it answered no probe
    /// for [`DHCP_PROBE_TIMEOUT`].
    fn probe_dhcp(&mut self) {
        // While the VM stops, the responder is stopped with it.
        if self.stopping.is_some() {
            return;
        }
        let Some(r) = self.dhcp.as_mut() else {
            return;
        };
        let alive_at = self.dhcp_alive_at.unwrap_or(r.started);
        if alive_at.elapsed() >= DHCP_PROBE_TIMEOUT {
            let Some(r) = self.dhcp.take() else {
                return;
            };
            tracing::warn!(
                pod = %self.pod_uid(),
                pid = r.process.pid,
                silent_s = alive_at.elapsed().as_secs(),
                "DHCP responder stopped answering liveness probes; restarting it"
            );
            // Count only the time it served, not the time it hung.
            if alive_at.saturating_duration_since(r.started) >= DHCP_STABLE {
                self.dhcp_failures = 0;
            }
            r.stop();
            self.schedule_dhcp_restart(DhcpFailure::Unresponsive);
            return;
        }
        self.dhcp_probe_seq += 1;
        if let Err(e) = r.probe(self.dhcp_probe_seq) {
            // Counted as a missing answer.
            tracing::debug!(pod = %self.pod_uid(), error = %e, "cannot probe the DHCP responder");
        }
    }

    /// An answer to probe `probe`. Answers to probes sent before the current
    /// responder started are ignored; an answer to a probe not sent, or a
    /// second answer, is a protocol violation.
    fn on_dhcp_alive(&mut self, probe: u64) {
        if self.dhcp.is_none() || probe <= self.dhcp_probe_base {
            return;
        }
        if probe > self.dhcp_probe_seq || probe <= self.dhcp_probe_answered {
            self.on_dhcp_violation("an answer to a probe it was not sent");
            return;
        }
        self.dhcp_probe_answered = probe;
        self.dhcp_alive_at = Some(std::time::Instant::now());
        // After a failure, one answer is not enough: a responder that answers
        // once and then hangs again would show Ready between its failures.
        // It must keep answering for a full probe timeout first.
        let proven = self.dhcp_failures == 0
            || self
                .dhcp
                .as_ref()
                .is_some_and(|r| r.started.elapsed() >= DHCP_PROBE_TIMEOUT);
        if !self.dhcp_answered && proven {
            self.dhcp_answered = true;
            self.publish_network(NetworkState::Ready);
        }
    }

    fn schedule_dhcp_restart(&mut self, cause: DhcpFailure) {
        self.dhcp_last_failure = cause;
        self.dhcp_failures = self.dhcp_failures.saturating_add(1);
        let backoff =
            Duration::from_secs(1u64 << (self.dhcp_failures - 1).min(6)).min(DHCP_MAX_BACKOFF);
        self.dhcp_restart_at = Some(Instant::now() + backoff);
        if self.dhcp_failures == DHCP_FAILING_AFTER + 1 {
            tracing::error!(pod = %self.pod_uid(), failures = DHCP_FAILING_AFTER, "DHCP responder keeps failing; restarting it every {} s", DHCP_MAX_BACKOFF.as_secs());
        }
        let state = if self.dhcp_failures > DHCP_FAILING_AFTER {
            NetworkState::Failing
        } else {
            NetworkState::Restarting
        };
        self.publish_network(state);
    }

    /// The current responder broke its protocol: kill and restart it, with
    /// one log line per restart whatever it wrote.
    fn on_dhcp_violation(&mut self, what: &str) {
        let Some(r) = self.dhcp.take() else {
            return;
        };
        tracing::warn!(pod = %self.pod_uid(), pid = r.process.pid, what, "DHCP responder broke its protocol; restarting it");
        r.stop();
        if self.stopping.is_some() {
            return;
        }
        self.schedule_dhcp_restart(DhcpFailure::ProtocolViolation);
    }

    /// Report the responder state to the anchor and node agent.
    fn publish_network(&mut self, state: NetworkState) {
        if self.dhcp_state.0 != state {
            self.dhcp_state = (state, now_ms());
        }
        let n = network_status(
            self.dhcp_state,
            self.dhcp_failures,
            self.dhcp_last_failure,
            self.dhcp_last_ack,
        );
        self.publish(|s| s.network = Some(n));
    }

    /// Open a new DHCP socket in the pod network namespace and start a
    /// responder on it. Needs a live anchor for the namespace.
    async fn restart_dhcp(&mut self) {
        self.dhcp_restart_at = None;
        if self.dhcp.is_some() || self.stopping.is_some() {
            return;
        }
        let Some(anchor) = self.anchor.as_ref().filter(|a| a.alive()) else {
            // Retried when an anchor attaches.
            return;
        };
        let Ok(anchor) = AnchorRef::try_clone(anchor) else {
            self.schedule_dhcp_restart(DhcpFailure::StartFailed);
            return;
        };
        match self.reopen_dhcp(&anchor).await {
            Ok(socket) => self.start_dhcp(socket, &anchor),
            Err(e) => {
                tracing::warn!(pod = %self.pod_uid(), error = %e, "cannot reopen the DHCP socket");
                self.schedule_dhcp_restart(DhcpFailure::StartFailed);
            }
        }
    }

    /// Reopen the DHCP packet socket on the TAP and refresh the lease from
    /// the pod's current network state and resolv.conf.
    async fn reopen_dhcp(&mut self, anchor: &AnchorRef) -> Result<OwnedFd, String> {
        let fd = anchor.pidfd.try_clone().map_err(|e| e.to_string())?;
        let pod_if = self.pod_interface.clone();
        let (socket, pod) =
            tokio::task::spawn_blocking(move || network::reopen_dhcp_socket(fd.as_fd(), &pod_if))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
        let root = storage::open_process_root(anchor.pid, anchor.pidfd.as_fd()).ok();
        let resolv = root
            .and_then(|r| read_container_file(r.as_fd(), "/etc/resolv.conf"))
            .unwrap_or_default();
        self.lease = Some(lease_from(&pod, &resolv, &self.entry.spec.vm_name));
        Ok(socket)
    }

    /// Stop the DHCP responder and wait briefly for it to exit, so it is
    /// gone before its cgroup is removed.
    async fn stop_dhcp(&mut self) {
        self.dhcp_restart_at = None;
        // The VM no longer runs: nothing to report about its address service.
        self.dhcp_state = (NetworkState::Unspecified, 0);
        self.publish(|s| s.network = None);
        if let Some(r) = self.dhcp.take() {
            let fd = r.process.pidfd.try_clone().ok();
            r.stop();
            if let Some(fd) = fd {
                let _ = tokio::time::timeout(Duration::from_secs(2), vmm::wait_exit(&fd)).await;
            }
        }
    }

    /// Create the VM's log directory and hold a shared BSD lock on it for as
    /// long as the VM runs: systemd-tmpfiles skips locked directories when
    /// it ages /var/log/baryon/vms, so the logs of a running VM are never
    /// removed, however quiet its console is. The lock goes away with the
    /// descriptor (cleanup, or baryon-virtd exiting) and is taken again after
    /// a restart.
    fn lock_log_dir(&mut self) {
        let dir = self.console_log_dir();
        let locked = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .and_then(|()| {
                rustix::fs::open(
                    &dir,
                    rustix::fs::OFlags::DIRECTORY
                        | rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::CLOEXEC,
                    rustix::fs::Mode::empty(),
                )
                .map_err(io::Error::from)
            })
            .and_then(|fd| {
                rustix::fs::flock(&fd, rustix::fs::FlockOperation::NonBlockingLockShared)
                    .map_err(io::Error::from)
                    .map(|()| fd)
            });
        match locked {
            Ok(fd) => self.log_dir_lock = Some(fd),
            Err(e) => {
                tracing::warn!(pod = %self.pod_uid(), dir = %dir.display(), error = %e, "cannot lock the VM log directory; log retention may remove it while the VM runs")
            }
        }
    }

    fn start_serial(&self) {
        // The socket is in the VMM-owned directory: connect only through
        // `VmmDir`, never by following the path as root.
        let vmm_dir = self.vm_dir().join("vmm");
        let owner = self.entry.vmm_uid;
        let connect = move || {
            let vmm_dir = vmm_dir.clone();
            async move { VmmDir::open(&vmm_dir, owner)?.connect(SERIAL_SOCKET).await }
        };
        let log = self.console_log_dir().join("console.log");
        tokio::spawn(serial::capture(
            connect,
            log,
            self.serial_tx.clone(),
            self.helpers.child_token(),
        ));
    }

    /// Main loop while the VMM runs.
    async fn supervise(&mut self) {
        let Some(vmm) = self.vmm.as_ref() else {
            return;
        };
        let pidfd = match vmm.pidfd.try_clone() {
            Ok(fd) => fd,
            Err(e) => {
                tracing::error!(error = %e, "cannot duplicate VMM pidfd");
                return;
            }
        };
        let exit = vmm::wait_exit(&pidfd);
        tokio::pin!(exit);
        let mut liveness = tokio::time::interval(POD_LIVENESS_INTERVAL);
        liveness.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut dhcp_probes = tokio::time::interval(DHCP_PROBE_INTERVAL);
        dhcp_probes.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            let deadline = self.detach_deadline;
            let anchor_fd = self
                .anchor
                .as_ref()
                .filter(|_| !self.anchor_exited)
                .and_then(|a| a.pidfd.try_clone().ok());
            // Poll pod liveness whenever no live anchor is attached: after an
            // anchor exit and after recovery, until an anchor attaches.
            let check_pod = self.anchor_exited || self.anchor.is_none();
            let dhcp_fd = self
                .dhcp
                .as_ref()
                .and_then(|d| d.process.pidfd.try_clone().ok());
            let dhcp_restart = self.dhcp_restart_at;
            tokio::select! {
                () = wait_process(anchor_fd) => self.on_anchor_exit(),
                () = wait_process(dhcp_fd) => self.on_dhcp_exit(),
                () = sleep_until(dhcp_restart) => self.restart_dhcp().await,
                _ = liveness.tick(), if check_pod => self.check_pod_alive(),
                _ = dhcp_probes.tick(), if self.dhcp.is_some() => self.probe_dhcp(),
                r = &mut exit => {
                    if let Err(e) = r {
                        tracing::warn!(error = %e, "waiting on VMM pidfd failed; treating the VMM as exited");
                    }
                    break;
                }
                cmd = self.cmd_rx.recv() => match cmd {
                    Some(Command::Attach(anchor)) => self.on_attach(anchor).await,
                    Some(Command::Detach) => self.on_detach(),
                    Some(Command::Stop { grace, final_, reply }) => {
                        self.stop_replies.push(reply);
                        self.final_stop |= final_;
                        self.begin_stop(grace, "Stopped", true);
                    }
                    None => {}
                },
                Some(ev) = self.dhcp_rx.recv() => match ev {
                    DhcpEvent::Acknowledged { ip, mac } => {
                        let due = self.dhcp_ack_processed.is_none_or(|t| {
                            t.elapsed() >= crate::handoff::dhcp::ACK_REPORT_INTERVAL
                        });
                        if due {
                            self.dhcp_ack_processed = Some(Instant::now());
                            self.on_dhcp_ack();
                            self.on_guest_evidence(Some((ip, mac)), false);
                        }
                    }
                    DhcpEvent::Alive { probe } => self.on_dhcp_alive(probe),
                    DhcpEvent::ProtocolViolation { what } => self.on_dhcp_violation(&what),
                },
                Some(SerialEvent::LoginPrompt) = self.serial_rx.recv() => self.on_guest_evidence(None, true),
                () = sleep_until(deadline) => {
                    self.detach_deadline = None;
                    if self.sessions == 0 {
                        self.final_stop = true;
                        self.begin_stop(self.cfg.default_stop_grace, "AnchorGone", false);
                    }
                }
            }
        }
        self.on_exit().await;
    }

    /// The anchor process exited. If the pod is being torn down (a final
    /// stop is in progress, or no other process is left in the pod), the
    /// VMM must be gone before kubelet tears down volumes: kill it now.
    /// Otherwise the anchor container is restarting and may reattach.
    fn on_anchor_exit(&mut self) {
        self.anchor_exited = true;
        if self.final_stop {
            tracing::warn!(pod = %self.pod_uid(), "anchor exited during a final stop; killing the VMM");
            self.kill_now("AnchorTerminated");
        } else {
            tracing::info!(pod = %self.pod_uid(), "anchor exited; checking whether the pod is terminating");
            self.check_pod_alive();
        }
    }

    /// Kill the VMM when no process other than the VMM remains in the pod
    /// cgroup: the pod sandbox is gone and volume teardown is next.
    fn check_pod_alive(&mut self) {
        let (Some(pod), Some(vmm)) = (self.pod_cgroup.as_ref(), self.vmm.as_ref()) else {
            return;
        };
        // The DHCP responder runs in the same cgroup and does not count. Its
        // pid is excluded only while its pidfd shows it alive, before and
        // after the scan, so a reused pid is never mistaken for it.
        let responder_alive = |s: &Self| {
            s.dhcp
                .as_ref()
                .filter(|d| crate::peer::pidfd_alive(d.process.pidfd.as_fd()))
                .map(|d| d.process.pid)
        };
        let responder = responder_alive(self);
        let mut ours = vec![vmm.pid];
        ours.extend(responder);
        let others = cgroup::tree_pids(pod, &ours);
        if responder.is_some() && responder_alive(self) != responder {
            return;
        }
        if !pod.exists() || others.is_empty() {
            tracing::warn!(pod = %self.pod_uid(), "pod has no processes left besides the VMM; killing the VMM");
            self.kill_now("PodTerminated");
        }
    }

    fn kill_now(&mut self, reason: &'static str) {
        if let Some(h) = self.escalation.take() {
            h.abort();
        }
        if self.stopping.is_none() {
            self.stopping = Some(StopState {
                reason,
                requested: false,
            });
            self.set_phase(VmPhase::Stopping, reason, "killing the VMM");
        }
        if let Some(vmm) = self.vmm.as_ref() {
            let _ = vmm::signal(vmm.pidfd.as_fd(), vmm.pid, rustix::process::Signal::KILL);
        }
    }

    async fn on_attach(&mut self, anchor: AnchorRef) {
        self.sessions += 1;
        self.detach_deadline = None;
        self.anchor_exited = false;
        if self.dhcp.is_none() && self.stopping.is_none() {
            // Recovered VM: the responder exited with the previous
            // baryon-virtd (its stdin closed). Start a new one.
            match self.reopen_dhcp(&anchor).await {
                // The failure count is not reset here: it only resets after
                // a responder ran for DHCP_STABLE, so a flapping anchor
                // cannot hide a responder that keeps failing.
                Ok(socket) => self.start_dhcp(socket, &anchor),
                Err(e) => {
                    tracing::warn!(pod = %self.pod_uid(), error = %e, "cannot restore the DHCP responder");
                    self.schedule_dhcp_restart(DhcpFailure::StartFailed);
                }
            }
        }
        tracing::info!(pod = %self.pod_uid(), sessions = self.sessions, "anchor attached");
        self.anchor = Some(anchor);
    }

    fn on_detach(&mut self) {
        self.sessions = self.sessions.saturating_sub(1);
        tracing::info!(pod = %self.pod_uid(), sessions = self.sessions, "anchor detached");
        if self.sessions == 0 && self.stopping.is_none() {
            self.detach_deadline = Some(Instant::now() + self.cfg.reattach_grace);
        }
    }

    /// The responder acknowledged a lease: remember when, also in the
    /// runtime record, so the lease countdown survives a baryon-virtd
    /// restart.
    fn on_dhcp_ack(&mut self) {
        self.dhcp_last_ack = now_ms();
        if let Some(rec) = self.record.as_mut() {
            rec.dhcp_last_ack_unix_ms = self.dhcp_last_ack;
            if let Err(e) = rec.write(&self.cfg.vm_dir(&self.entry.pod_uid)) {
                tracing::warn!(pod = %self.entry.pod_uid, error = %e, "cannot update runtime record");
            }
        }
        let state = self.dhcp_state.0;
        self.publish_network(state);
    }

    fn on_guest_evidence(&mut self, dhcp: Option<(Ipv4Addr, MacAddr)>, login: bool) {
        let first = {
            let s = self.entry.status.borrow();
            s.guest.as_ref().is_none_or(|g| g.booted_at_unix_ms == 0)
        };
        if first && let Some(rec) = self.record.as_mut() {
            rec.guest_booted_at_unix_ms = now_ms();
            if let Err(e) = rec.write(&self.cfg.vm_dir(&self.entry.pod_uid)) {
                tracing::warn!(pod = %self.entry.pod_uid, error = %e, "cannot update runtime record");
            }
        }
        self.publish(|s| {
            let g = s.guest.get_or_insert_with(GuestStatus::default);
            if let Some((ip, mac)) = dhcp {
                g.dhcp_lease_acknowledged = true;
                g.ip_address = ip.to_string();
                g.mac_address = mac.to_string();
            }
            if login {
                g.console_login_prompt = true;
            }
            if g.booted_at_unix_ms == 0 {
                g.booted_at_unix_ms = now_ms();
            }
            if s.phase == VmPhase::Starting as i32
                || (s.phase == VmPhase::Running as i32 && s.reason == "Recovered")
            {
                s.phase = VmPhase::Running as i32;
                s.reason = "GuestBooted".into();
                s.message = if dhcp.is_some() {
                    "guest acquired its address over DHCP".into()
                } else {
                    "guest console shows a login prompt".into()
                };
            }
        });
        if first {
            tracing::info!(pod = %self.pod_uid(), vm = %self.entry.spec.vm_name, dhcp = dhcp.is_some(), login, "guest boot evidence");
        }
    }

    fn begin_stop(&mut self, grace: Duration, reason: &'static str, requested: bool) {
        if self.stopping.is_some() {
            return;
        }
        self.detach_deadline = None;
        self.stopping = Some(StopState { reason, requested });
        self.set_phase(
            VmPhase::Stopping,
            reason,
            format!("shutting down guest, grace period {}s", grace.as_secs()),
        );
        let Some(vmm) = self.vmm.as_ref() else { return };
        let Ok(pidfd) = vmm.pidfd.try_clone() else {
            return;
        };
        let pid = vmm.pid;
        let api = ApiClient::new(self.api_socket()).with_timeout(Duration::from_secs(5));
        self.escalation = Some(tokio::spawn(async move {
            // A guest early in boot (before its ACPI event handling runs)
            // ignores the power button, so it is pressed again until the
            // grace period ends.
            let deadline = tokio::time::Instant::now() + grace;
            loop {
                if let Err(e) = api.power_button().await {
                    tracing::info!(pid, error = %e, "ACPI power button failed");
                }
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    break;
                }
                tokio::time::sleep((deadline - now).min(POWER_BUTTON_REPEAT)).await;
            }
            tracing::info!(
                pid,
                "guest did not shut down within the grace period; stopping the VMM"
            );
            let _ = api.shutdown_vmm().await;
            tokio::time::sleep(STOP_ESCALATION_STEP).await;
            let _ = vmm::signal(pidfd.as_fd(), pid, rustix::process::Signal::TERM);
            tokio::time::sleep(STOP_ESCALATION_STEP).await;
            tracing::warn!(pid, "VMM ignored SIGTERM; sending SIGKILL");
            let _ = vmm::signal(pidfd.as_fd(), pid, rustix::process::Signal::KILL);
        }));
    }

    async fn on_exit(&mut self) {
        if let Some(h) = self.escalation.take() {
            h.abort();
        }
        let exit = self
            .vmm
            .as_ref()
            .map(|v| vmm::reap(v.pidfd.as_fd(), v.is_child))
            .unwrap_or(Exit::Unknown);
        let oom = self
            .vmm_cgroup
            .as_ref()
            .map(|c| oom_killed(&c.path))
            .unwrap_or(false);
        let log_tail = [self.vmm_output_tail(), self.vmm_log_tail(VMM_LOG)]
            .into_iter()
            .flatten()
            .map(|b| last_lines(&String::from_utf8_lossy(&b), 5))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" | ");

        let (phase, reason, message) = match (&self.stopping, exit) {
            (Some(s), _) => (VmPhase::Stopped, s.reason, format!("VMM exited ({exit:?})")),
            (None, _) if oom => (
                VmPhase::Failed,
                "OutOfMemory",
                "the VMM was killed by the kernel OOM killer in the pod cgroup".to_owned(),
            ),
            (None, Exit::Code(0)) if log_tail.contains("VCPU generated error") => {
                (VmPhase::Failed, "VmmFailed", log_tail.clone())
            }
            (None, Exit::Code(0)) => (
                VmPhase::Stopped,
                "GuestPoweredOff",
                "the guest shut down".to_owned(),
            ),
            (None, Exit::Code(c)) => (
                VmPhase::Failed,
                "VmmFailed",
                format!("VMM exited with code {c}: {log_tail}"),
            ),
            (None, Exit::Signal(s)) => (
                VmPhase::Failed,
                "VmmKilled",
                format!("VMM was killed by signal {s}"),
            ),
            (None, Exit::Unknown) => (
                VmPhase::Failed,
                "VmmExited",
                "VMM exited while baryon-virtd was not its parent; exit status unknown".to_owned(),
            ),
        };
        if let Some(s) = &self.stopping
            && !s.requested
        {
            tracing::info!(pod = %self.pod_uid(), "VM stopped because its anchor did not return");
        }
        self.cleanup().await;
        self.publish(|s| {
            s.phase = phase as i32;
            s.reason = reason.to_owned();
            s.message = message;
            s.stopped_at_unix_ms = now_ms();
        });
        tracing::info!(pod = %self.pod_uid(), ?phase, reason, ?exit, "VMM exited");
    }

    /// Release every resource. Safe to call with partially prepared state.
    async fn cleanup(&mut self) {
        self.helpers.cancel();
        self.stop_dhcp().await;
        if let Some(anchor) = self.anchor.as_ref().filter(|a| a.alive())
            && let Ok(fd) = anchor.pidfd.try_clone()
        {
            let pod_if = self.pod_interface.clone();
            let r =
                tokio::task::spawn_blocking(move || network::teardown(fd.as_fd(), &pod_if)).await;
            if let Ok(Err(e)) = r {
                tracing::warn!(pod = %self.pod_uid(), error = %e, "network teardown failed");
            }
        }
        // The container cgroup belongs to the container runtime.
        self.vmm_cgroup = None;
        self.preserve_vmm_logs();
        self.log_dir_lock = None;
        vmm_files::remove_vm_dir(&self.vm_dir());
    }

    /// Copy the VMM log and output to the persistent log directory before
    /// the runtime directory is removed.
    fn preserve_vmm_logs(&self) {
        use std::os::unix::fs::DirBuilderExt;
        let dest = self.console_log_dir();
        if std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dest)
            .is_err()
        {
            return;
        }
        let ctl_dir = self.vm_dir().join(CONTROL_DIR);
        let mut saved: Vec<(&str, Vec<u8>)> = Vec::new();
        // Written by the VMM: read through VmmDir, bounded.
        if let Some(b) = self.vmm_log_tail_max(VMM_LOG, PRESERVED_LOG_BYTES) {
            saved.push((VMM_LOG, b));
        }
        // Root-owned files in ctl/.
        for f in [VMM_OUTPUT, "dhcp.log"] {
            if let Ok(b) = read_tail_plain(&ctl_dir.join(f), PRESERVED_LOG_BYTES) {
                saved.push((f, b));
            }
        }
        for (f, bytes) in saved {
            let written = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(dest.join(f))
                .and_then(|mut out| std::io::Write::write_all(&mut out, &bytes));
            if let Err(e) = written {
                tracing::warn!(pod = %self.pod_uid(), file = f, error = %e, "cannot preserve VMM log");
            }
        }
    }

    fn console_log_dir(&self) -> PathBuf {
        let spec = &self.entry.spec;
        self.cfg.log_dir.join("vms").join(format!(
            "{}_{}_{}",
            spec.vm_namespace, spec.vm_name, self.entry.pod_uid
        ))
    }

    async fn finish(mut self) {
        let status = self.entry.status.borrow().clone();
        for r in self.stop_replies.drain(..) {
            let _ = r.send(status.clone());
        }
        if let Some(m) = self.manager.upgrade() {
            m.remove(&self.entry.pod_uid);
        }
        // Answer stop requests that raced with the exit.
        self.cmd_rx.close();
        while let Some(cmd) = self.cmd_rx.recv().await {
            if let Command::Stop { reply, .. } = cmd {
                let _ = reply.send(status.clone());
            }
        }
    }
}

fn lease_from(pod: &PodInterface, resolv: &str, hostname: &str) -> LeaseConfig {
    LeaseConfig {
        client_mac: pod.mac,
        ipv4: pod.ipv4.clone(),
        mtu: pod.mtu,
        dns: DnsConfig::parse_resolv_conf(resolv),
        hostname: hostname.to_owned(),
    }
}

/// Resolves when the process behind `pidfd` exits; never resolves for `None`.
async fn wait_process(pidfd: Option<OwnedFd>) {
    match pidfd {
        Some(fd) => {
            let _ = vmm::wait_exit(&fd).await;
        }
        None => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// The last `max` bytes of a root-owned file.
fn read_tail_plain(path: &std::path::Path, max: u64) -> io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(max)))?;
    let mut out = Vec::new();
    f.take(max).read_to_end(&mut out)?;
    Ok(out)
}

fn create_dir_mode(path: &std::path::Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Read a small file inside a container root without following links out of
/// it.
fn read_container_file(root: std::os::fd::BorrowedFd<'_>, path: &str) -> Option<String> {
    use rustix::fs::{Mode, OFlags, ResolveFlags};
    let fd = rustix::fs::openat2(
        root,
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
        ResolveFlags::IN_ROOT | ResolveFlags::NO_MAGICLINKS,
    )
    .ok()?;
    let mut f = std::fs::File::from(fd);
    let mut s = String::new();
    use std::io::Read;
    (&mut f).take(64 * 1024).read_to_string(&mut s).ok()?;
    Some(s)
}

/// Stable guest MAC address for a VM (see
/// `baryon_runtime_api::identity::guest_mac`). The UID is validated as a UUID
/// before this is called.
fn vm_mac(vm_uid: &str) -> MacAddr {
    MacAddr(baryon_runtime_api::identity::guest_mac(vm_uid))
}

/// `ds=nocloud;i=<instance-id>;h=<hostname>` for cloud-init. The instance id
/// is the VirtualMachine UID, so first-boot actions run once per VM, not once
/// per pod.
fn nocloud_serial(vm_uid: &str, vm_name: &str) -> String {
    format!("ds=nocloud;i={vm_uid};h={vm_name}")
}

fn sanitize_ident(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(20)
        .collect()
}

fn oom_killed(cgroup: &std::path::Path) -> bool {
    std::fs::read_to_string(cgroup.join("memory.events"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("oom_kill "))
                .and_then(|v| v.trim().parse::<u64>().ok())
        })
        .is_some_and(|n| n > 0)
}

fn last_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use baryon_runtime_api::v1alpha1::{Boot, FirmwareBoot, NetworkInterface, boot};

    fn spec() -> VmSpec {
        VmSpec {
            vm_uid: "6b7f4c0e-6a39-4b8e-9d0e-2f1f0c6c4a11".into(),
            vm_namespace: "default".into(),
            vm_name: "vm1".into(),
            vcpus: 2,
            memory_bytes: 2 << 30,
            boot: Some(Boot {
                payload: Some(boot::Payload::Firmware(FirmwareBoot {})),
            }),
            disks: vec![baryon_runtime_api::v1alpha1::Disk {
                name: "root".into(),
                container_path: "/var/lib/baryon/disks/root/disk.img".into(),
                read_only: false,
            }],
            interfaces: vec![NetworkInterface {
                name: "default".into(),
                pod_interface: "eth0".into(),
            }],
        }
    }

    fn test_manager(dir: &std::path::Path) -> Arc<Manager> {
        Manager::new(
            Arc::new(Config {
                runtime_dir: dir.join("run"),
                log_dir: dir.join("log"),
                cloud_hypervisor: dir.join("cloud-hypervisor"),
                firmware: dir.join("CLOUDHV.fd"),
                cgroup_root: dir.join("cgroup"),
                kvm_device: dir.join("kvm"),
                components_manifest: dir.join("components.json"),
                reattach_grace: Duration::from_secs(1),
                recovery_grace: Duration::from_secs(1),
                default_stop_grace: Duration::from_secs(1),
                seccomp: baryon_cloud_hypervisor::launch::Seccomp::Enforce,
                landlock: true,
                vmm_verbosity: 0,
            }),
            "v53.0".into(),
        )
    }

    fn peer(uid: u32, gid: u32, pod_uid: &str) -> AnchorPeer {
        let me = rustix::process::getpid();
        AnchorPeer {
            pid: me.as_raw_nonzero().get(),
            uid,
            gid,
            groups: vec![gid],
            pidfd: rustix::process::pidfd_open(me, rustix::process::PidfdFlags::empty()).unwrap(),
            cgroup: PodCgroup {
                pod_uid: pod_uid.into(),
                pod_path: "/kubepods.slice/kubepods-podx.slice".into(),
                container_path: "/kubepods.slice/kubepods-podx.slice/cri-containerd-x.scope".into(),
            },
        }
    }

    #[test]
    fn attach_requires_the_vms_identity() {
        let dir = tempfile::tempdir().unwrap();
        let m = test_manager(dir.path());
        let id = VmIdentity::for_vm(&spec().vm_uid).unwrap();
        for (uid, gid) in [
            (id.uid() + 1, id.gid()),
            (id.uid(), id.gid() + 1),
            (1000, 1000),
        ] {
            match m.attach(&peer(uid, gid, "pod-a"), spec()) {
                Err(AttachError::IdentityMismatch { .. }) => {}
                other => panic!("expected IdentityMismatch, got {other:?}"),
            }
        }
        assert!(m.list().is_empty(), "a refused attach must not create a VM");
    }

    #[test]
    fn a_block_device_serves_one_vm_per_node() {
        let dir = tempfile::tempdir().unwrap();
        let m = test_manager(dir.path());
        let entry = |pod: &str, devs: Vec<u64>| {
            let (status, _) = watch::channel(VmStatus::default());
            let (cmd, _cmd_rx) = mpsc::channel(1);
            Arc::new(VmEntry {
                pod_uid: pod.into(),
                spec: spec(),
                vmm_uid: 1,
                block_devices: Mutex::new(devs),
                status,
                cmd,
            })
        };
        let dev = rustix::fs::makedev(251, 0);
        m.lock().insert("pod-a".into(), entry("pod-a", vec![dev]));
        m.lock().insert("pod-b".into(), entry("pod-b", Vec::new()));
        let err = m.claim_block_devices("pod-b", &[dev]).unwrap_err();
        assert!(err.contains("251:0"), "{err}");
        // Its own devices, and other devices, are fine.
        assert!(m.claim_block_devices("pod-a", &[dev]).is_ok());
        assert!(
            m.claim_block_devices("pod-b", &[rustix::fs::makedev(251, 16)])
                .is_ok()
        );
        // Released when the VM is gone.
        m.remove("pod-a");
        assert!(m.claim_block_devices("pod-b", &[dev]).is_ok());
    }

    #[test]
    fn identity_collision_on_a_node_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let m = test_manager(dir.path());
        let id = VmIdentity::for_vm(&spec().vm_uid).unwrap();
        // Another pod's VM already runs with the same host identity (a
        // collision of the allocation).
        let (status, _) = watch::channel(VmStatus::default());
        let (cmd, _cmd_rx) = mpsc::channel(1);
        let mut other = spec();
        other.vm_uid = "11111111-2222-4333-8444-555555555555".into();
        m.lock().insert(
            "pod-b".into(),
            Arc::new(VmEntry {
                pod_uid: "pod-b".into(),
                spec: other,
                vmm_uid: id.uid(),
                block_devices: Mutex::new(Vec::new()),
                status,
                cmd,
            }),
        );
        let err = m
            .attach(&peer(id.uid(), id.gid(), "pod-a"), spec())
            .unwrap_err();
        assert!(matches!(err, AttachError::IdentityConflict));
        // The reason prefix is what baryon-anchor and baryon-controller act on.
        assert!(err.to_string().starts_with("IdentityConflict: "));
        assert_eq!(m.list().len(), 1, "the conflicting VM must not be created");
    }

    #[test]
    fn validates_specs() {
        assert!(validate_spec(&spec()).is_ok());
        let mut s = spec();
        s.vcpus = 0;
        assert!(validate_spec(&s).is_err());
        let mut s = spec();
        s.memory_bytes = 100;
        assert!(validate_spec(&s).is_err());
        let mut s = spec();
        s.interfaces.clear();
        assert!(validate_spec(&s).is_err());
        let mut s = spec();
        s.boot = None;
        assert!(validate_spec(&s).is_err());
        let mut s = spec();
        s.vm_namespace = "../../etc".into();
        assert!(validate_spec(&s).is_err());
        let mut s = spec();
        s.vm_name = "a/b".into();
        assert!(validate_spec(&s).is_err());
        let mut s = spec();
        s.vm_uid = "not-a-uuid".into();
        assert!(validate_spec(&s).is_err());
        let mut s = spec();
        s.vm_name = "web-prod-1".into();
        assert!(validate_spec(&s).is_ok());
        // The name is the guest hostname: a DNS-1123 label, no dots.
        let mut s = spec();
        s.vm_name = "web.prod-1".into();
        assert!(validate_spec(&s).is_err());
        let mut s = spec();
        s.vm_name = "a".repeat(64);
        assert!(validate_spec(&s).is_err());
    }

    #[test]
    fn helpers() {
        assert_eq!(sanitize_ident("root disk/1"), "root-disk-1");
        assert_eq!(nocloud_serial("u-1", "vm1"), "ds=nocloud;i=u-1;h=vm1");
        assert_eq!(
            vm_mac("6b7f4c0e-6a39-4b8e-9d0e-2f1f0c6c4a11").to_string(),
            "02:1f:0c:6c:4a:11"
        );
        assert_eq!(last_lines("a\nb\nc", 2), "b | c");
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("memory.events"),
            "low 0\nhigh 0\nmax 3\noom 1\noom_kill 1\n",
        )
        .unwrap();
        assert!(oom_killed(dir.path()));
        std::fs::write(dir.path().join("memory.events"), "oom_kill 0\n").unwrap();
        assert!(!oom_killed(dir.path()));
    }

    #[test]
    fn network_status_reports_the_responder_and_the_lease() {
        let ready = network_status((NetworkState::Ready, 5), 0, DhcpFailure::Exited, 0);
        assert_eq!(ready.state, NetworkState::Ready as i32);
        assert_eq!(ready.reason, "AddressServiceRunning");
        assert_eq!((ready.since_unix_ms, ready.lease_expires_unix_ms), (5, 0));

        let restarting =
            network_status((NetworkState::Restarting, 7), 2, DhcpFailure::Exited, 1_000);
        assert_eq!(restarting.reason, "DhcpResponderRestarting");
        assert!(
            restarting
                .message
                .starts_with("the DHCP responder stopped;")
        );
        assert!(restarting.message.contains("at most 60 s"));
        assert!(restarting.message.contains("lease stays valid"));
        assert_eq!(
            restarting.lease_expires_unix_ms,
            1_000 + i64::from(crate::handoff::dhcp::LEASE_SECONDS) * 1000
        );

        let failing = network_status((NetworkState::Failing, 9), 11, DhcpFailure::Unresponsive, 0);
        assert_eq!(failing.reason, "DhcpResponderFailing");
        assert!(
            failing
                .message
                .contains("stopped answering liveness probes")
        );
        assert_eq!(failing.consecutive_failures, 11);
        assert!(failing.message.contains("has not acquired an address"));
        assert!(failing.message.contains("failed 11 times"));
    }
}
