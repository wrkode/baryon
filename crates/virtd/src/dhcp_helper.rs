//! The guest DHCP responder as a separate, unprivileged process.
//!
//! Guest DHCP frames are untrusted input, so baryon-virtd does not parse
//! them. It creates the packet socket on the TAP (which needs CAP_NET_RAW
//! and the pod network namespace) and hands it to a child process started
//! with the same confinement as Cloud Hypervisor (see `crate::vmm`): the
//! VM's uid and gid, no capabilities, `no_new_privs`, an empty network
//! namespace, and the Anchor container's cgroup. The child is this binary
//! (`/proc/self/exe`) with the hidden `dhcp-responder` subcommand and runs
//! [`crate::handoff::dhcp::serve`] on the inherited socket.
//!
//! Protocol between the two processes:
//!
//! - stdin: one line of JSON, the [`LeaseConfig`]. The parent keeps the pipe
//!   open afterwards. End of file means the parent stopped the responder or
//!   exited, and the responder exits. A responder therefore never outlives
//!   the baryon-virtd that started it; after a restart, baryon-virtd starts
//!   a new one when the anchor reattaches.
//! - stdout: one line of JSON per [`DhcpEvent`]. The parent treats these as
//!   untrusted: lines are length-limited and events that do not match the
//!   lease are ignored.
//! - stderr: log lines, appended to a root-only file.

use std::ffi::OsString;
use std::io::{self, BufRead, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, bail};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::handoff::dhcp::{self, DhcpEvent, LeaseConfig};
use crate::vmm::{self, SpawnRequest, VmmCredentials, VmmError, VmmProcess};

/// Hidden subcommand of baryon-virtd that runs the responder.
pub const SUBCOMMAND: &str = "dhcp-responder";

/// Name the responder process shows in `ps`.
const PROCESS_NAME: &str = "baryon-dhcp";

/// Largest lease configuration the responder accepts on stdin.
const MAX_CONFIG_BYTES: u64 = 64 * 1024;

/// Longest event line the parent accepts from the responder.
const MAX_EVENT_LINE: usize = 4096;

/// A running responder process, from the parent's side.
#[derive(Debug)]
pub struct Responder {
    pub process: VmmProcess,
    pub started: Instant,
    /// Write end of the responder's stdin. Dropping it stops the responder.
    stdin: Option<std::process::ChildStdin>,
    reader: tokio::task::JoinHandle<()>,
}

/// What [`start`] needs.
#[derive(Debug)]
pub struct StartRequest<'a> {
    /// Packet socket bound to the TAP. Moved into the responder.
    pub socket: OwnedFd,
    pub lease: &'a LeaseConfig,
    pub credentials: VmmCredentials,
    /// `cgroup.procs` of the cgroup the VMM runs in.
    pub cgroup_procs: BorrowedFd<'a>,
    /// Receives the responder's log.
    pub log: Option<std::fs::File>,
    /// For identification in process listings only.
    pub pod_uid: &'a str,
    pub events: mpsc::Sender<DhcpEvent>,
}

/// Start a responder for `lease`.
pub fn start(req: StartRequest<'_>) -> Result<Responder, VmmError> {
    let exe = Path::new("/proc/self/exe");
    let fd = req.socket.as_raw_fd();
    let args: Vec<OsString> = [
        SUBCOMMAND.to_owned(),
        "--socket-fd".to_owned(),
        fd.to_string(),
        "--pod-uid".to_owned(),
        req.pod_uid.to_owned(),
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    let spawn_err = |source| VmmError::Spawn {
        binary: exe.to_owned(),
        source,
    };
    let spawned = vmm::spawn(SpawnRequest {
        binary: exe,
        arg0: Some(PROCESS_NAME),
        args,
        inherit: vec![req.socket.as_fd()],
        credentials: req.credentials,
        cgroup_procs: req.cgroup_procs,
        output: req.log,
        stdin_pipe: true,
        stdout_pipe: true,
    })?;
    // The responder holds its own copy of the socket now.
    drop(req.socket);

    let mut responder = Responder {
        process: spawned.process,
        started: Instant::now(),
        stdin: spawned.stdin,
        reader: tokio::spawn(async {}),
    };
    let config = serde_json::to_string(req.lease).map_err(|e| spawn_err(io::Error::other(e)))?;
    // A few hundred bytes: the write completes into the pipe buffer without
    // waiting for the child.
    let written = match responder.stdin.as_mut() {
        Some(stdin) => stdin
            .write_all(config.as_bytes())
            .and_then(|()| stdin.write_all(b"\n")),
        None => Err(io::Error::other("responder stdin is not a pipe")),
    };
    let stdout = match spawned.stdout {
        Some(out) => tokio::process::ChildStdout::from_std(out),
        None => Err(io::Error::other("responder stdout is not a pipe")),
    };
    let stdout = match written.and(stdout) {
        Ok(out) => out,
        Err(e) => {
            responder.stop();
            return Err(spawn_err(e));
        }
    };
    responder.reader = tokio::spawn(forward_events(stdout, req.lease.clone(), req.events));
    Ok(responder)
}

impl Responder {
    /// Stop the responder: close its stdin, kill it, and reap it in the
    /// background.
    pub fn stop(mut self) {
        self.stdin = None;
        self.reader.abort();
        let _ = vmm::signal(
            self.process.pidfd.as_fd(),
            self.process.pid,
            rustix::process::Signal::KILL,
        );
        let process = self.process;
        tokio::spawn(async move {
            let _ = vmm::wait_exit(&process.pidfd).await;
            vmm::reap(process.pidfd.as_fd(), process.is_child);
        });
    }

    /// Collect the exit status after the process has exited.
    pub fn reap(self) -> vmm::Exit {
        self.reader.abort();
        vmm::reap(self.process.pidfd.as_fd(), self.process.is_child)
    }
}

/// Read event lines from the responder and pass on the ones that match the
/// lease.
async fn forward_events<R: tokio::io::AsyncRead + Unpin>(
    stdout: R,
    lease: LeaseConfig,
    events: mpsc::Sender<DhcpEvent>,
) {
    let mut reader = tokio::io::BufReader::new(stdout);
    let mut line = Vec::with_capacity(256);
    loop {
        line.clear();
        let limit = u64::try_from(MAX_EVENT_LINE).unwrap_or(u64::MAX) + 1;
        match (&mut reader).take(limit).read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if line.last() != Some(&b'\n') {
            tracing::warn!("DHCP responder wrote an overlong line; ignoring its output");
            return;
        }
        match serde_json::from_slice::<DhcpEvent>(&line) {
            Ok(ev @ DhcpEvent::Acknowledged { ip, mac })
                if ip == lease.ipv4.address && mac == lease.client_mac =>
            {
                if events.send(ev).await.is_err() {
                    return;
                }
            }
            Ok(other) => {
                tracing::warn!(event = ?other, "DHCP responder reported an event that does not match the lease");
            }
            Err(e) => tracing::warn!(error = %e, "unparseable DHCP responder event"),
        }
    }
}

/// Entry point of the responder process (`baryon-virtd dhcp-responder`).
pub fn run_child(socket_fd: RawFd) -> anyhow::Result<()> {
    if rustix::process::geteuid().is_root() {
        bail!("the DHCP responder must not run as root");
    }
    let socket = take_socket(socket_fd)?;

    let mut line = String::new();
    io::stdin()
        .lock()
        .take(MAX_CONFIG_BYTES)
        .read_line(&mut line)
        .context("reading the lease configuration")?;
    let lease: LeaseConfig =
        serde_json::from_str(&line).context("parsing the lease configuration")?;
    tracing::info!(ip = %lease.ipv4.address, mac = %lease.client_mac, "DHCP responder starting");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;
    rt.block_on(async move {
        let cancel = CancellationToken::new();
        // End of file on stdin: the parent stopped us or is gone.
        let on_eof = cancel.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 256];
            let mut stdin = io::stdin().lock();
            while matches!(stdin.read(&mut buf), Ok(n) if n > 0) {}
            on_eof.cancel();
        });
        let (tx, mut rx) = mpsc::channel(8);
        let writer = tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                let Ok(json) = serde_json::to_string(&ev) else {
                    continue;
                };
                let mut out = io::stdout().lock();
                if writeln!(out, "{json}").and_then(|()| out.flush()).is_err() {
                    return;
                }
            }
        });
        let result = dhcp::serve(socket, lease, tx, cancel).await;
        writer.abort();
        result
    })
    .context("serving DHCP")?;
    tracing::info!("DHCP responder stopped");
    Ok(())
}

/// Take ownership of the inherited packet socket.
#[allow(unsafe_code)]
fn take_socket(fd: RawFd) -> anyhow::Result<OwnedFd> {
    if fd <= 2 {
        bail!("--socket-fd {fd} is a standard stream");
    }
    // SAFETY: F_GETFD only inspects the descriptor table entry.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        bail!("--socket-fd {fd} is not open");
    }
    // SAFETY: the descriptor is open (checked above). It was inherited from
    // the parent for this process; nothing else in this process refers to
    // it, so taking ownership cannot double-close.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    rustix::io::fcntl_setfd(&socket, rustix::io::FdFlags::CLOEXEC)
        .context("setting FD_CLOEXEC on the socket")?;
    let domain =
        rustix::net::sockopt::socket_domain(&socket).context("--socket-fd is not a socket")?;
    if domain != rustix::net::AddressFamily::PACKET {
        bail!("--socket-fd {fd} is not a packet socket");
    }
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handoff::dhcp::DnsConfig;
    use crate::handoff::network::{PodIpv4, PodRoute};
    use baryon_cloud_hypervisor::launch::MacAddr;
    use std::net::Ipv4Addr;
    use tokio::io::AsyncWriteExt;

    fn lease() -> LeaseConfig {
        LeaseConfig {
            client_mac: MacAddr([0x02, 0x1f, 0x0c, 0x6c, 0x4a, 0x11]),
            ipv4: PodIpv4 {
                address: Ipv4Addr::new(10, 42, 0, 13),
                prefix_len: 24,
                gateway: Some(Ipv4Addr::new(10, 42, 0, 1)),
                routes: vec![PodRoute {
                    destination: Ipv4Addr::new(10, 42, 0, 0),
                    prefix_len: 16,
                    gateway: Some(Ipv4Addr::new(10, 42, 0, 1)),
                }],
            },
            mtu: 1450,
            dns: DnsConfig {
                nameservers: vec![Ipv4Addr::new(10, 43, 0, 10)],
                search: vec!["default.svc.cluster.local".into()],
            },
            hostname: "debian".into(),
        }
    }

    #[test]
    fn lease_round_trips_as_json() {
        let l = lease();
        let json = serde_json::to_string(&l).unwrap();
        assert!(json.contains("\"02:1f:0c:6c:4a:11\""));
        let back: LeaseConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, l);
    }

    async fn forwarded(input: &[u8]) -> Vec<DhcpEvent> {
        let (mut w, r) = tokio::io::duplex(64 * 1024);
        let (tx, mut rx) = mpsc::channel(8);
        let task = tokio::spawn(forward_events(r, lease(), tx));
        w.write_all(input).await.unwrap();
        drop(w);
        task.await.unwrap();
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    #[tokio::test]
    async fn forwards_only_events_matching_the_lease() {
        let good = DhcpEvent::Acknowledged {
            ip: Ipv4Addr::new(10, 42, 0, 13),
            mac: lease().client_mac,
        };
        let wrong_ip = DhcpEvent::Acknowledged {
            ip: Ipv4Addr::new(10, 42, 0, 99),
            mac: lease().client_mac,
        };
        let input = format!(
            "{}\n{}\nnot json\n{}\n",
            serde_json::to_string(&wrong_ip).unwrap(),
            serde_json::to_string(&good).unwrap(),
            serde_json::to_string(&good).unwrap(),
        );
        assert_eq!(forwarded(input.as_bytes()).await, vec![good.clone(), good]);
    }

    #[tokio::test]
    async fn stops_reading_at_an_overlong_line() {
        let good = serde_json::to_string(&DhcpEvent::Acknowledged {
            ip: Ipv4Addr::new(10, 42, 0, 13),
            mac: lease().client_mac,
        })
        .unwrap();
        let input = format!("{}\n{}\n", "x".repeat(MAX_EVENT_LINE + 10), good);
        assert!(forwarded(input.as_bytes()).await.is_empty());
    }
}
