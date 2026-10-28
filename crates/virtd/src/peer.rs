//! Identification of processes connecting to baryon-virtd sockets.
//!
//! Every connection is identified once, at accept time, from kernel peer
//! credentials. The pidfd obtained through `SO_PEERPIDFD` refers to the exact
//! process that connected, so later operations on its namespaces cannot be
//! redirected to another process by PID reuse.
//!
//! Identity is not authority (ADR 0015). Every process of a pod that runs as
//! the VM's host identity passes the identity check, including processes
//! started with `kubectl exec`, and the VMM and the DHCP responder, which
//! baryon-virtd places in the Anchor container's cgroup. Lifecycle calls
//! additionally require the caller to be an Anchor supervisor
//! ([`check_supervisor`]): the init process of a container PID namespace, in
//! the initial user namespace, outside baryon-virtd's own namespaces.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UnixStream;

use crate::podcgroup::{self, PodCgroup};

/// `SO_PEERPIDFD` (Linux 6.5). The value is 77 on every architecture Baryon
/// supports (x86_64, aarch64); it differs only on alpha, mips, parisc, sparc.
const SO_PEERPIDFD: libc::c_int = 77;

/// Errors identifying a peer.
#[derive(Debug, thiserror::Error)]
pub enum PeerError {
    #[error("reading peer credentials: {0}")]
    Credentials(#[source] io::Error),
    #[error("obtaining peer pidfd (requires Linux 6.5 or later): {0}")]
    Pidfd(#[source] io::Error),
    #[error("peer pidfd does not match peer pid {pid}")]
    PidfdMismatch { pid: i32 },
    #[error("peer process {pid} exited during identification")]
    Exited { pid: i32 },
    #[error("reading /proc/{pid}/{file}: {source}")]
    Proc {
        pid: i32,
        file: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("peer process {pid} is not a pod container: {source}")]
    NotPod {
        pid: i32,
        #[source]
        source: podcgroup::CgroupError,
    },
    #[error("peer process {pid} runs as root; anchor processes must run as a non-root user")]
    Root { pid: i32 },
}

/// Identity of a namespace: the device and inode of its nsfs file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NsId {
    pub dev: u64,
    pub ino: u64,
}

/// The namespaces that the supervisor check compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Namespaces {
    pub user: NsId,
    pub pid: NsId,
    pub mnt: NsId,
    pub net: NsId,
}

impl Namespaces {
    /// Namespaces of the process behind `/proc/<pid>` (`self` for this one).
    pub fn of(pid: &str) -> io::Result<Self> {
        let id = |ns: &str| -> io::Result<NsId> {
            let st = rustix::fs::stat(format!("/proc/{pid}/ns/{ns}").as_str())?;
            Ok(NsId {
                dev: st.st_dev,
                ino: st.st_ino,
            })
        };
        Ok(Self {
            user: id("user")?,
            pid: id("pid")?,
            mnt: id("mnt")?,
            net: id("net")?,
        })
    }
}

/// baryon-virtd's own namespaces, read once.
pub fn own_namespaces() -> Result<Namespaces, String> {
    static OWN: std::sync::OnceLock<Result<Namespaces, String>> = std::sync::OnceLock::new();
    OWN.get_or_init(|| Namespaces::of("self").map_err(|e| e.to_string()))
        .clone()
}

/// Why a peer is not an Anchor supervisor. The text follows the reason
/// `NotAnchorSupervisor` in refusals.
pub fn check_supervisor(nspid: &[i32], peer: &Namespaces, own: &Namespaces) -> Result<(), String> {
    if peer.user != own.user {
        return Err("the caller is not in the initial user namespace".into());
    }
    if peer.pid == own.pid || nspid.len() < 2 {
        return Err("the caller runs in baryon-virtd's PID namespace, not in a container".into());
    }
    if nspid.last() != Some(&1) {
        return Err(
            "the caller is not the init process of its container (for example a process started with kubectl exec)"
                .into(),
        );
    }
    if peer.mnt == own.mnt || peer.net == own.net {
        return Err("the caller shares baryon-virtd's mount or network namespace".into());
    }
    Ok(())
}

/// A process inside a pod that connected to the anchor socket.
#[derive(Debug)]
pub struct AnchorPeer {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
    /// Supplementary groups of the process.
    pub groups: Vec<u32>,
    pub pidfd: OwnedFd,
    pub cgroup: PodCgroup,
    /// The process's PID in each nested PID namespace, outermost first, from
    /// the pidfd's fdinfo (`NSpid`), so PID reuse cannot change it.
    pub nspid: Vec<i32>,
    pub namespaces: Namespaces,
}

impl AnchorPeer {
    /// Whether the peer may call lifecycle RPCs (ADR 0015); the error says
    /// why not.
    pub fn supervisor(&self) -> Result<(), String> {
        let own = own_namespaces()?;
        check_supervisor(&self.nspid, &self.namespaces, &own)
    }

    /// Whether the peer runs outside baryon-virtd's PID namespace, the
    /// condition for any AnchorService call.
    pub fn outside_host_pid_namespace(&self) -> Result<(), String> {
        let own = own_namespaces()?;
        if self.namespaces.pid == own.pid || self.nspid.len() < 2 {
            return Err(
                "the caller runs in baryon-virtd's PID namespace (for example a VMM or a DHCP responder)"
                    .into(),
            );
        }
        Ok(())
    }
}

/// Connection metadata attached to every request by tonic.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub pid: i32,
    pub uid: u32,
    /// Identification result for anchor connections. Errors are carried to
    /// the request handler so that the caller receives a precise reason.
    pub anchor: Arc<Result<AnchorPeer, String>>,
}

/// Whether the process behind a pidfd is still running.
pub fn pidfd_alive(pidfd: BorrowedFd<'_>) -> bool {
    let mut fds = [rustix::event::PollFd::new(
        &pidfd,
        rustix::event::PollFlags::IN,
    )];
    let zero = rustix::event::Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    match rustix::event::poll(&mut fds, Some(&zero)) {
        Ok(0) => true,
        Ok(_) => !fds[0].revents().contains(rustix::event::PollFlags::IN),
        Err(_) => false,
    }
}

/// Identify a pod process from a connected stream.
pub fn identify_anchor(stream: &UnixStream) -> Result<AnchorPeer, PeerError> {
    let cred = rustix::net::sockopt::socket_peercred(stream)
        .map_err(|e| PeerError::Credentials(e.into()))?;
    let pid = cred.pid.as_raw_nonzero().get();
    let uid = cred.uid.as_raw();
    let gid = cred.gid.as_raw();
    if uid == 0 {
        return Err(PeerError::Root { pid });
    }

    let pidfd = peer_pidfd(stream).map_err(PeerError::Pidfd)?;
    if pidfd_pid(pidfd.as_fd()) != Some(pid) {
        return Err(PeerError::PidfdMismatch { pid });
    }

    let cgroup_raw = read_proc(pid, "cgroup")?;
    let status_raw = read_proc(pid, "status")?;
    let namespaces = Namespaces::of(&pid.to_string()).map_err(|source| PeerError::Proc {
        pid,
        file: "ns",
        source,
    })?;
    let nspid = pidfd_nspid(pidfd.as_fd()).ok_or(PeerError::Exited { pid })?;
    // The files above could belong to a new process if the peer exited and
    // its PID was reused while they were read. The pidfd still refers to the
    // original process; if it is alive, the PID was not reused.
    if !pidfd_alive(pidfd.as_fd()) {
        return Err(PeerError::Exited { pid });
    }

    let cgroup = podcgroup::parse_proc_cgroup(&cgroup_raw)
        .map_err(|source| PeerError::NotPod { pid, source })?;
    let groups = parse_groups(&status_raw);

    Ok(AnchorPeer {
        pid,
        uid,
        gid,
        groups,
        pidfd,
        cgroup,
        nspid,
        namespaces,
    })
}

fn read_proc(pid: i32, file: &'static str) -> Result<String, PeerError> {
    std::fs::read_to_string(format!("/proc/{pid}/{file}")).map_err(|source| PeerError::Proc {
        pid,
        file,
        source,
    })
}

#[allow(unsafe_code)]
fn peer_pidfd(stream: &UnixStream) -> io::Result<OwnedFd> {
    let mut fd: libc::c_int = -1;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `stream` owns a valid socket descriptor for the duration of the
    // call. `fd` and `len` are valid, properly aligned out-parameters and
    // `len` holds the size of `fd`. SO_PEERPIDFD writes one int.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            SO_PEERPIDFD,
            (&raw mut fd).cast(),
            &raw mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if fd < 0 {
        return Err(io::Error::other("kernel returned an invalid pidfd"));
    }
    // SAFETY: on success the kernel installed a new descriptor `fd` in this
    // process (with O_CLOEXEC) that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// PID that a pidfd refers to, from `/proc/self/fdinfo`. `None` if the
/// process has exited or the descriptor is not a pidfd.
pub fn pidfd_pid(pidfd: BorrowedFd<'_>) -> Option<i32> {
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd())).ok()?;
    info.lines()
        .find_map(|l| l.strip_prefix("Pid:"))
        .and_then(|v| v.trim().parse::<i32>().ok())
        .filter(|p| *p > 0)
}

/// `NSpid` of a pidfd from `/proc/self/fdinfo`: the PID in each nested PID
/// namespace, outermost first. `None` if the process has exited.
pub fn pidfd_nspid(pidfd: BorrowedFd<'_>) -> Option<Vec<i32>> {
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd())).ok()?;
    parse_nspid(&info)
}

fn parse_nspid(fdinfo: &str) -> Option<Vec<i32>> {
    let v: Vec<i32> = fdinfo
        .lines()
        .find_map(|l| l.strip_prefix("NSpid:"))?
        .split_whitespace()
        .map(|p| p.parse::<i32>().ok())
        .collect::<Option<_>>()?;
    // An exited process shows -1; a process outside the procfs instance's
    // PID namespace shows 0.
    (!v.is_empty() && v.iter().all(|p| *p > 0)).then_some(v)
}

/// Supplementary groups from `/proc/<pid>/status`.
fn parse_groups(status: &str) -> Vec<u32> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("Groups:"))
        .map(|v| {
            v.split_whitespace()
                .filter_map(|g| g.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// A Unix stream with the identity of its peer, for tonic.
#[derive(Debug)]
pub struct PeerStream {
    inner: UnixStream,
    info: PeerInfo,
}

impl PeerStream {
    /// Accept-time identification. `anchor` selects whether the connection
    /// must come from a pod container.
    pub fn new(inner: UnixStream, anchor: bool) -> io::Result<Self> {
        let cred = rustix::net::sockopt::socket_peercred(&inner).map_err(io::Error::from)?;
        let identified = if anchor {
            identify_anchor(&inner).map_err(|e| e.to_string())
        } else {
            Err("not an anchor connection".to_owned())
        };
        Ok(Self {
            info: PeerInfo {
                pid: cred.pid.as_raw_nonzero().get(),
                uid: cred.uid.as_raw(),
                anchor: Arc::new(identified),
            },
            inner,
        })
    }
}

impl tonic::transport::server::Connected for PeerStream {
    type ConnectInfo = PeerInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.info.clone()
    }
}

impl AsyncRead for PeerStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for PeerStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_groups() {
        let s =
            "Name:\tbaryon-anchor\nUid:\t1000\t1000\t1000\t1000\nGroups:\t10 994 1000 \nNgid:\t0\n";
        assert_eq!(parse_groups(s), vec![10, 994, 1000]);
        assert!(parse_groups("Name:\tx\n").is_empty());
    }

    #[tokio::test]
    async fn peer_pidfd_refers_to_self() {
        let (a, _b) = UnixStream::pair().unwrap();
        let pidfd = peer_pidfd(&a).unwrap();
        assert_eq!(pidfd_pid(pidfd.as_fd()), Some(std::process::id() as i32));
        assert!(pidfd_alive(pidfd.as_fd()));
    }

    #[test]
    fn parses_nspid_and_fails_closed() {
        assert_eq!(
            parse_nspid("Pid:\t4242\nNSpid:\t4242\t1\n"),
            Some(vec![4242, 1])
        );
        assert_eq!(parse_nspid("Pid:\t7\nNSpid:\t7\n"), Some(vec![7]));
        // Exited process, process outside the PID namespace, garbage, absent.
        assert_eq!(parse_nspid("Pid:\t-1\nNSpid:\t-1\n"), None);
        assert_eq!(parse_nspid("Pid:\t0\nNSpid:\t0\n"), None);
        assert_eq!(parse_nspid("NSpid:\t12\tx\n"), None);
        assert_eq!(parse_nspid("NSpid:\n"), None);
        assert_eq!(parse_nspid("Pid:\t12\n"), None);
    }

    fn ns(n: u64) -> NsId {
        NsId { dev: 4, ino: n }
    }

    fn host() -> Namespaces {
        Namespaces {
            user: ns(1),
            pid: ns(2),
            mnt: ns(3),
            net: ns(4),
        }
    }

    fn container() -> Namespaces {
        Namespaces {
            user: ns(1),
            pid: ns(12),
            mnt: ns(13),
            net: ns(14),
        }
    }

    /// Finding M1 of 2026-10-27: the VM's identity is not lifecycle
    /// authority. Only the init process of a container PID namespace in the
    /// initial user namespace passes.
    #[test]
    fn only_a_container_init_is_a_supervisor() {
        let own = host();
        // The Anchor container's init process (baryon-anchor run).
        assert!(check_supervisor(&[4242, 1], &container(), &own).is_ok());
        // Deeper nesting by the node runtime is accepted as long as the
        // caller is its namespace's init and in the initial user namespace.
        assert!(check_supervisor(&[4242, 9, 1], &container(), &own).is_ok());

        // The VMM or the DHCP responder: baryon-virtd's PID, mount, and
        // network namespaces (the VMM's network namespace is its own empty
        // one, but its PID namespace is the host's).
        let mut vmm = host();
        vmm.net = ns(40);
        let e = check_supervisor(&[5000], &vmm, &own).unwrap_err();
        assert!(e.contains("PID namespace"), "{e}");

        // A process started with kubectl exec in the Anchor container.
        let e = check_supervisor(&[5001, 27], &container(), &own).unwrap_err();
        assert!(e.contains("init process"), "{e}");

        // A same-uid process that created a user namespace and, in it, a PID
        // namespace whose init it is (from the host or from a container).
        let mut nested = container();
        nested.user = ns(77);
        let e = check_supervisor(&[5002, 1], &nested, &own).unwrap_err();
        assert!(e.contains("user namespace"), "{e}");
        let e = check_supervisor(&[5003, 27, 1], &nested, &own).unwrap_err();
        assert!(e.contains("user namespace"), "{e}");

        // A PID namespace of its own but the host's mount or network
        // namespace.
        let mut odd = container();
        odd.mnt = own.mnt;
        assert!(check_supervisor(&[5004, 1], &odd, &own).is_err());
        let mut odd = container();
        odd.net = own.net;
        assert!(check_supervisor(&[5005, 1], &odd, &own).is_err());

        // Malformed: no NSpid entries, or the host PID namespace with depth.
        assert!(check_supervisor(&[], &container(), &own).is_err());
        let mut hostpid = container();
        hostpid.pid = own.pid;
        assert!(check_supervisor(&[5006, 1], &hostpid, &own).is_err());
    }

    #[test]
    fn own_namespaces_are_readable() {
        let a = own_namespaces().unwrap();
        assert_eq!(a, Namespaces::of("self").unwrap());
    }

    #[tokio::test]
    async fn non_pod_process_is_rejected() {
        // The test process is not in a pod cgroup (or runs as root in CI).
        let (a, _b) = UnixStream::pair().unwrap();
        assert!(identify_anchor(&a).is_err());
    }
}
