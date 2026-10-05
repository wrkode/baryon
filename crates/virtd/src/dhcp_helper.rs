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
//! - stdin: one line of JSON, the [`LeaseConfig`], then one line per
//!   liveness probe, `{"probe":<n>}`. End of file means the parent stopped
//!   the responder or exited, and the responder exits. A responder
//!   therefore never outlives the baryon-virtd that started it; after a
//!   restart, baryon-virtd starts a new one when the anchor reattaches.
//! - stdout: one line of JSON per [`DhcpEvent`]. The DHCP serving loop
//!   answers each probe with [`DhcpEvent::Alive`], so a responder whose loop
//!   is stuck stops answering although its process exists. The parent
//!   treats all events as untrusted. A legitimate responder writes only
//!   well-formed events for its lease, at most one answer per probe; any
//!   other output (an overlong or unparseable line, an acknowledgement for
//!   another address, an answer to a probe not sent) is a protocol
//!   violation: the parent stops reading, and the supervisor kills and
//!   restarts the responder. Its log output is therefore bounded by the
//!   restart backoff, whatever a compromised responder writes.
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

/// How long a stopped responder is waited for after SIGKILL.
const REAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Longest probe line the responder reads from the parent.
const MAX_PROBE_LINE: u64 = 256;

/// A liveness probe line on the responder's stdin.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Probe {
    probe: u64,
}

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
    // Probes are written later from the VM supervisor's task: never block
    // it on a responder that does not read. A probe line is shorter than
    // PIPE_BUF, so a write either completes or fails with EAGAIN.
    let written = written.and_then(|()| match responder.stdin.as_ref() {
        Some(stdin) => {
            let flags = rustix::fs::fcntl_getfl(stdin)?;
            rustix::fs::fcntl_setfl(stdin, flags | rustix::fs::OFlags::NONBLOCK)
                .map_err(io::Error::from)
        }
        None => Ok(()),
    });
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
    /// Send liveness probe `probe`. An error (including a full pipe) means
    /// the responder does not read its probes.
    pub fn probe(&mut self, probe: u64) -> io::Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(io::Error::other("responder stdin is closed"));
        };
        let line = serde_json::to_string(&Probe { probe }).map_err(io::Error::other)?;
        stdin.write_all(format!("{line}\n").as_bytes())
    }

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
            // SIGKILL ends it unless it is stuck in the kernel; do not hold
            // a task and a descriptor for it forever.
            match tokio::time::timeout(REAP_TIMEOUT, vmm::wait_exit(&process.pidfd)).await {
                Ok(_) => {
                    vmm::reap(process.pidfd.as_fd(), process.is_child);
                }
                Err(_) => tracing::warn!(
                    pid = process.pid,
                    "a killed DHCP responder has not exited; no longer waiting for it"
                ),
            }
        });
    }

    /// Collect the exit status after the process has exited.
    pub fn reap(self) -> vmm::Exit {
        self.reader.abort();
        vmm::reap(self.process.pidfd.as_fd(), self.process.is_child)
    }
}

/// Read event lines from the responder and pass on the ones that match the
/// lease. Stops at the first protocol violation and reports it.
async fn forward_events<R: tokio::io::AsyncRead + Unpin>(
    stdout: R,
    lease: LeaseConfig,
    events: mpsc::Sender<DhcpEvent>,
) {
    let mut reader = tokio::io::BufReader::new(stdout);
    let mut line = Vec::with_capacity(256);
    let violation = |what: &str| DhcpEvent::ProtocolViolation {
        what: what.to_owned(),
    };
    loop {
        line.clear();
        let limit = u64::try_from(MAX_EVENT_LINE).unwrap_or(u64::MAX) + 1;
        match (&mut reader).take(limit).read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if line.last() != Some(&b'\n') {
            let _ = events.send(violation("an overlong line")).await;
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
            // The supervisor accepts only answers to probes it sent to this
            // responder.
            Ok(ev @ DhcpEvent::Alive { .. }) => {
                if events.send(ev).await.is_err() {
                    return;
                }
            }
            Ok(DhcpEvent::Acknowledged { .. }) => {
                let _ = events
                    .send(violation("an acknowledgement for another address"))
                    .await;
                return;
            }
            // Not deserializable; listed for completeness.
            Ok(DhcpEvent::ProtocolViolation { .. }) | Err(_) => {
                let _ = events.send(violation("an unparseable line")).await;
                return;
            }
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
        // Probes from the parent, answered by the serving loop. End of file
        // on stdin: the parent stopped us or is gone.
        let (probe_tx, probe_rx) = mpsc::channel(4);
        let on_eof = cancel.clone();
        std::thread::spawn(move || {
            let mut stdin = io::stdin().lock();
            let mut line = String::new();
            loop {
                line.clear();
                match (&mut stdin).take(MAX_PROBE_LINE).read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                // An unreadable line is ignored; a missing answer is what the
                // parent acts on.
                if let Ok(p) = serde_json::from_str::<Probe>(&line) {
                    // A full channel means the serving loop is not taking
                    // probes; dropping more of them changes nothing.
                    let _ = probe_tx.try_send(p.probe);
                }
            }
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
        let result = dhcp::serve(socket, lease, tx, probe_rx, cancel).await;
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
    async fn forwards_events_and_stops_at_a_protocol_violation() {
        let good = DhcpEvent::Acknowledged {
            ip: Ipv4Addr::new(10, 42, 0, 13),
            mac: lease().client_mac,
        };
        let wrong_ip = DhcpEvent::Acknowledged {
            ip: Ipv4Addr::new(10, 42, 0, 99),
            mac: lease().client_mac,
        };
        let good_line = serde_json::to_string(&good).unwrap();
        // Everything after the first violation is ignored.
        let input = format!(
            "{good_line}\n{}\n{good_line}\n",
            serde_json::to_string(&wrong_ip).unwrap()
        );
        assert_eq!(
            forwarded(input.as_bytes()).await,
            vec![
                good.clone(),
                DhcpEvent::ProtocolViolation {
                    what: "an acknowledgement for another address".into()
                }
            ]
        );
        let input = format!("not json\n{good_line}\n");
        assert_eq!(
            forwarded(input.as_bytes()).await,
            vec![DhcpEvent::ProtocolViolation {
                what: "an unparseable line".into()
            }]
        );
        // The supervisor's own event cannot be sent by the responder.
        let input = "{\"protocolViolation\":{\"what\":\"x\"}}\n";
        assert_eq!(
            forwarded(input.as_bytes()).await,
            vec![DhcpEvent::ProtocolViolation {
                what: "an unparseable line".into()
            }]
        );
    }

    #[tokio::test]
    async fn forwards_probe_answers() {
        let alive = DhcpEvent::Alive { probe: 7 };
        let input = format!("{}\n", serde_json::to_string(&alive).unwrap());
        assert_eq!(forwarded(input.as_bytes()).await, vec![alive]);
    }

    #[test]
    fn probe_lines_are_short_json() {
        let line = serde_json::to_string(&Probe { probe: u64::MAX }).unwrap();
        assert_eq!(line, format!("{{\"probe\":{}}}", u64::MAX));
        assert!((line.len() as u64) < MAX_PROBE_LINE);
        assert!(serde_json::from_str::<Probe>("{\"probe\":3}\n").is_ok());
    }

    #[tokio::test]
    async fn stops_reading_at_an_overlong_line() {
        let good = serde_json::to_string(&DhcpEvent::Acknowledged {
            ip: Ipv4Addr::new(10, 42, 0, 13),
            mac: lease().client_mac,
        })
        .unwrap();
        let input = format!("{}\n{}\n", "x".repeat(MAX_EVENT_LINE + 10), good);
        assert_eq!(
            forwarded(input.as_bytes()).await,
            vec![DhcpEvent::ProtocolViolation {
                what: "an overlong line".into()
            }]
        );
    }
}
