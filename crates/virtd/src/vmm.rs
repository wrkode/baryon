//! Starting and stopping confined child processes: Cloud Hypervisor and the
//! DHCP responder (`crate::dhcp_helper`).
//!
//! The VMM is started with:
//!
//! - the uid, gid, and supplementary groups of the Anchor process, plus the
//!   group that owns /dev/kvm
//! - no capabilities (a non-root uid after `setuid` has none), an empty
//!   capability bounding set, and `no_new_privs`
//! - a new, empty network namespace: the VMM reaches the guest network only
//!   through the inherited TAP descriptor (whose device lives in the pod
//!   network namespace) and has no route to host or cluster addresses
//! - membership in its cgroup (the Anchor container's, see `VmmPlacement`),
//!   entered before `execve` so that no VMM allocation is ever charged
//!   elsewhere
//! - its own session, so signals for baryon-virtd's process group do not
//!   reach it
//! - stdin on /dev/null and stdout and stderr on a file, so it never depends
//!   on baryon-virtd staying alive
//! - only the descriptors listed in the launch configuration
//!
//! baryon-virtd is the parent and reaps the process when it exits. If
//! baryon-virtd restarts, the VMM is reparented to the init process (or the
//! closest subreaper) and baryon-virtd watches it through a pidfd.

use std::ffi::OsString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rustix::process::{Pid, PidfdFlags, Signal, WaitIdOptions};

/// Credentials the VMM runs with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmmCredentials {
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
}

/// A request to start Cloud Hypervisor.
#[derive(Debug)]
pub struct SpawnRequest<'a> {
    pub binary: &'a Path,
    /// Process name (`argv[0]`), if it should differ from `binary`.
    pub arg0: Option<&'a str>,
    pub args: Vec<OsString>,
    /// Descriptors to inherit. Their numbers must match the ones used in
    /// `args`.
    pub inherit: Vec<BorrowedFd<'a>>,
    pub credentials: VmmCredentials,
    /// `cgroup.procs` of the VMM cgroup, opened for writing.
    pub cgroup_procs: BorrowedFd<'a>,
    /// File receiving standard error, and standard output unless it is
    /// piped. A file rather than a pipe, so the child never depends on
    /// baryon-virtd reading it.
    pub output: Option<std::fs::File>,
    /// Give the child a pipe as standard input and keep the write end. The
    /// child sees end of file when the write end is dropped, including when
    /// baryon-virtd exits. Otherwise stdin is /dev/null.
    pub stdin_pipe: bool,
    /// Read the child's standard output through a pipe.
    pub stdout_pipe: bool,
}

/// A started child and the pipe ends requested in its [`SpawnRequest`].
#[derive(Debug)]
pub struct Spawned {
    pub process: VmmProcess,
    pub stdin: Option<std::process::ChildStdin>,
    pub stdout: Option<std::process::ChildStdout>,
}

/// A running VMM.
#[derive(Debug)]
pub struct VmmProcess {
    pub pid: i32,
    pub pidfd: OwnedFd,
    /// Whether this process is our child, so that it must be reaped.
    pub is_child: bool,
}

/// Errors starting or controlling a VMM.
#[derive(Debug, thiserror::Error)]
pub enum VmmError {
    #[error("starting {binary}: {source}")]
    Spawn {
        binary: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("opening pidfd for VMM {pid}: {source}")]
    Pidfd {
        pid: i32,
        #[source]
        source: io::Error,
    },
    #[error("signalling VMM {pid}: {source}")]
    Signal {
        pid: i32,
        #[source]
        source: io::Error,
    },
}

/// How the VMM process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    Signal(i32),
    /// The process was not our child; its status is unknown.
    Unknown,
}

/// Start Cloud Hypervisor, or another confined child such as the DHCP
/// responder, with the confinement described in the module documentation.
#[allow(unsafe_code)]
pub fn spawn(req: SpawnRequest<'_>) -> Result<Spawned, VmmError> {
    let inherit: Vec<RawFd> = req.inherit.iter().map(AsRawFd::as_raw_fd).collect();
    let procs_fd = req.cgroup_procs.as_raw_fd();
    let groups: Vec<libc::gid_t> = req.credentials.groups.clone();
    let uid = req.credentials.uid;
    let gid = req.credentials.gid;
    let last_cap = last_capability().map_err(|source| VmmError::Spawn {
        binary: req.binary.to_owned(),
        source,
    })?;

    let spawn_err = |source| VmmError::Spawn {
        binary: req.binary.to_owned(),
        source,
    };
    let mut cmd = Command::new(req.binary);
    if let Some(name) = req.arg0 {
        cmd.arg0(name);
    }
    cmd.args(&req.args);
    cmd.stdin(if req.stdin_pipe {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    match &req.output {
        Some(f) => {
            cmd.stderr(Stdio::from(f.try_clone().map_err(spawn_err)?));
            if !req.stdout_pipe {
                cmd.stdout(Stdio::from(f.try_clone().map_err(spawn_err)?));
            }
        }
        None => {
            cmd.stderr(Stdio::null());
            if !req.stdout_pipe {
                cmd.stdout(Stdio::null());
            }
        }
    }
    if req.stdout_pipe {
        cmd.stdout(Stdio::piped());
    }

    let mut seccomp_filter = crate::seccomp::filter();
    // SAFETY: the closure runs in the child between fork and execve, where
    // only async-signal-safe operations are allowed. It makes only direct
    // system calls (write, fcntl, setsid, unshare, prctl, setgroups, setgid,
    // setuid). setgroups, unshare, and prctl are not on the POSIX
    // async-signal-safe list, but glibc and musl implement them as plain
    // system call wrappers without locks or allocation. The closure only
    // reads values computed before the fork (`inherit` and `groups` are
    // never modified, so they are not reallocated; the seccomp program is
    // built before the fork, moved into the closure, and only read) and
    // allocates nothing. Every
    // failure is reported as an io::Error, which std turns into a failed
    // spawn in the parent.
    unsafe {
        cmd.pre_exec(move || {
            // Move into the VMM cgroup first, while still privileged, so the
            // VMM never runs outside the pod cgroup.
            if libc::write(procs_fd, b"0".as_ptr().cast(), 1) != 1 {
                return Err(io::Error::last_os_error());
            }
            for &fd in &inherit {
                if libc::fcntl(fd, libc::F_SETFD, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            // An empty network namespace (loopback only, down). Descriptors
            // keep working across namespaces, so the inherited TAP and API
            // listener are unaffected.
            if libc::unshare(libc::CLONE_NEWNET) != 0 {
                return Err(io::Error::last_os_error());
            }
            // Empty the bounding set while CAP_SETPCAP is still held. With a
            // non-root uid and no_new_privs it is already inert; this makes
            // the VMM's capability state empty everywhere.
            for cap in 0..=last_cap {
                if libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if libc::setgroups(groups.len(), groups.as_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::setgid(gid) != 0 {
                return Err(io::Error::last_os_error());
            }
            // After setuid to a non-zero uid the permitted, effective, and
            // ambient capability sets are cleared by the kernel.
            if libc::setuid(uid) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            // Last: Baryon's seccomp layer (see `crate::seccomp`), under the
            // filters Cloud Hypervisor installs itself.
            crate::seccomp::install(&mut seccomp_filter)?;
            Ok(())
        });
    }

    let mut child = cmd.spawn().map_err(spawn_err)?;
    let pid = i32::try_from(child.id()).unwrap_or(i32::MAX);
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    // The child is reaped through its pidfd (see `reap`), not through
    // `Child::wait`, so the handle is intentionally dropped here.
    drop(child);
    let pidfd = open_pidfd(pid)?;
    Ok(Spawned {
        process: VmmProcess {
            pid,
            pidfd,
            is_child: true,
        },
        stdin,
        stdout,
    })
}

/// Highest capability number the running kernel knows.
fn last_capability() -> io::Result<libc::c_ulong> {
    let s = std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")?;
    s.trim()
        .parse()
        .map_err(|_| io::Error::other("unparseable /proc/sys/kernel/cap_last_cap"))
}

/// Open a pidfd for an existing process.
pub fn open_pidfd(pid: i32) -> Result<OwnedFd, VmmError> {
    let p = Pid::from_raw(pid).ok_or_else(|| VmmError::Pidfd {
        pid,
        source: io::Error::new(io::ErrorKind::InvalidInput, "invalid pid"),
    })?;
    rustix::process::pidfd_open(p, PidfdFlags::empty()).map_err(|e| VmmError::Pidfd {
        pid,
        source: e.into(),
    })
}

/// Send a signal through the pidfd. Fails with ESRCH once the process has
/// exited, never reaching an unrelated process.
pub fn signal(pidfd: BorrowedFd<'_>, pid: i32, sig: Signal) -> Result<(), VmmError> {
    rustix::process::pidfd_send_signal(pidfd, sig).map_err(|e| VmmError::Signal {
        pid,
        source: e.into(),
    })
}

/// Wait until the process behind `pidfd` exits.
#[allow(unsafe_code)]
pub async fn wait_exit(pidfd: &OwnedFd) -> io::Result<()> {
    let dup = pidfd.try_clone()?;
    // SAFETY: `dup` is a fresh `OwnedFd` (a duplicate of the pidfd) moved
    // into the `AsyncFd`, which owns it until it is dropped at the end of
    // this function. Nothing else holds or can close that descriptor, and
    // `as_raw_fd` of an `OwnedFd` always returns the same descriptor.
    let async_fd = unsafe {
        tokio::io::unix::AsyncFd::register_with_interest(dup, tokio::io::Interest::READABLE)
    }?;
    let _guard = async_fd.readable().await?;
    Ok(())
}

/// Collect the exit status of an exited child.
pub fn reap(pidfd: BorrowedFd<'_>, is_child: bool) -> Exit {
    if !is_child {
        return Exit::Unknown;
    }
    match rustix::process::waitid(
        rustix::process::WaitId::PidFd(pidfd.as_fd()),
        WaitIdOptions::EXITED | WaitIdOptions::NOHANG,
    ) {
        Ok(Some(status)) => {
            if let Some(code) = status.exit_status() {
                Exit::Code(code)
            } else if let Some(sig) = status.terminating_signal() {
                Exit::Signal(sig)
            } else {
                Exit::Unknown
            }
        }
        _ => Exit::Unknown,
    }
}

/// Process start time in clock ticks since boot (`/proc/<pid>/stat` field
/// 22). Together with the PID it identifies a process instance across
/// baryon-virtd restarts.
pub fn start_time(pid: i32) -> io::Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    parse_start_time(&stat).ok_or_else(|| io::Error::other("unparseable /proc/<pid>/stat"))
}

fn parse_start_time(stat: &str) -> Option<u64> {
    // The command name (field 2) is in parentheses and may contain spaces;
    // fields are counted after the last closing parenthesis.
    let rest = &stat[stat.rfind(')')? + 1..];
    // rest starts at field 3; field 22 is index 19.
    rest.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_stat_start_time() {
        let stat = "4242 (cloud hyper (x)) S 1 4242 4242 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 9 0 123456 1000 10 18446744073709551615";
        assert_eq!(parse_start_time(stat), Some(123_456));
        assert!(start_time(std::process::id() as i32).unwrap() > 0);
    }

    #[tokio::test]
    async fn pidfd_tracks_child_exit() {
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 3")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        drop(child);
        let pidfd = open_pidfd(pid).unwrap();
        wait_exit(&pidfd).await.unwrap();
        assert_eq!(reap(pidfd.as_fd(), true), Exit::Code(3));
    }
}
