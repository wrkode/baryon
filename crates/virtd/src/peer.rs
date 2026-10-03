//! Identification of processes connecting to baryon-virtd sockets.
//!
//! Every connection is identified once, at accept time, from kernel peer
//! credentials. The pidfd obtained through `SO_PEERPIDFD` refers to the exact
//! process that connected, so later operations on its namespaces cannot be
//! redirected to another process by PID reuse.

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

    #[tokio::test]
    async fn non_pod_process_is_rejected() {
        // The test process is not in a pod cgroup (or runs as root in CI).
        let (a, _b) = UnixStream::pair().unwrap();
        assert!(identify_anchor(&a).is_err());
    }
}
