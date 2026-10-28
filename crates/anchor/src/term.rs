//! Termination requests from the container runtime only (ADR 0015).
//!
//! SIGTERM makes the anchor stop its VM. kubelet asks for it through the
//! container runtime, which runs as root, so a SIGTERM from any other user
//! is ignored: processes with the VM's host identity (exec'd processes, and
//! the VMM and the DHCP responder) can send signals to the anchor, but must
//! not be able to stop the VM through it. The sender is read from a
//! signalfd; `tokio::signal` does not expose it.
//!
//! The sender's uid is trusted only when the kernel filled it in, that is
//! for signals sent with kill(2) or tgkill(2) (`SI_USER`, `SI_TKILL`). A
//! process may queue a signal to another process with siginfo of its own
//! choosing (rt_sigqueueinfo(2), pidfd_send_signal(2)); the kernel then only
//! forbids those two codes, and leaves the uid as given.

use std::io;
use std::mem::{MaybeUninit, size_of};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use tokio::io::unix::AsyncFd;

/// Block SIGTERM in the calling thread. Call it before any other thread
/// starts, so every thread inherits the mask and the signal is only ever
/// read from the signalfd.
#[allow(unsafe_code)]
pub(crate) fn block() -> io::Result<()> {
    let set = sigterm_set();
    // SAFETY: `set` is an initialized signal set; the old mask is not
    // requested (null).
    let rc =
        unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut()) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    Ok(())
}

#[allow(unsafe_code)]
fn sigterm_set() -> libc::sigset_t {
    let mut set = MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: sigemptyset initializes the set it is given; sigaddset then
    // adds a valid signal number to the initialized set. Both only fail for
    // invalid arguments, which these are not.
    unsafe {
        libc::sigemptyset(set.as_mut_ptr());
        libc::sigaddset(set.as_mut_ptr(), libc::SIGTERM);
        set.assume_init()
    }
}

/// SIGTERM requests, read from a signalfd.
pub(crate) struct Term {
    fd: AsyncFd<OwnedFd>,
    /// SIGTERMs ignored so far.
    ignored: u64,
}

impl Term {
    /// Must be called within the tokio runtime, after [`block`].
    #[allow(unsafe_code)]
    pub(crate) fn new() -> io::Result<Self> {
        let set = sigterm_set();
        // SAFETY: `set` is an initialized signal set; -1 asks for a new
        // descriptor.
        let raw =
            unsafe { libc::signalfd(-1, &raw const set, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: signalfd returned a new descriptor that nothing else owns.
        let owned = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: `owned` is moved into the AsyncFd, which owns it until the
        // Term is dropped; nothing else can close it, and `as_raw_fd` of an
        // `OwnedFd` always returns the same descriptor.
        let fd = unsafe { AsyncFd::register(owned) }?;
        Ok(Self { fd, ignored: 0 })
    }

    /// Wait for a SIGTERM from uid 0. SIGTERMs from other users are ignored
    /// and logged with a bounded volume: processes with the VM's uid can
    /// send them in a loop.
    pub(crate) async fn recv(&mut self) {
        loop {
            let Ok(mut guard) = self.fd.readable().await else {
                // The reactor failed; nothing can be read any more.
                return std::future::pending().await;
            };
            match guard.try_io(|fd| read_siginfo(fd.get_ref())) {
                Ok(Ok(info)) if from_runtime(&info) => return,
                Ok(Ok(info)) => {
                    self.ignored = self.ignored.saturating_add(1);
                    if logged(self.ignored) {
                        tracing::warn!(
                            sender_pid = info.ssi_pid,
                            sender_uid = info.ssi_uid,
                            code = info.ssi_code,
                            ignored = self.ignored,
                            "ignoring SIGTERM: only the container runtime (uid 0, kill or tgkill) may stop the VM"
                        );
                    }
                }
                Ok(Err(e)) => tracing::warn!(error = %e, "reading the signalfd failed"),
                Err(_would_block) => {}
            }
        }
    }
}

/// Whether the `n`th ignored SIGTERM is logged: the first, then each power
/// of two, so a sender in a loop adds one line per doubling.
fn logged(n: u64) -> bool {
    n.is_power_of_two()
}

/// Whether a SIGTERM comes from root, with a sender the kernel filled in.
fn from_runtime(info: &libc::signalfd_siginfo) -> bool {
    info.ssi_uid == 0 && matches!(info.ssi_code, libc::SI_USER | libc::SI_TKILL)
}

#[allow(unsafe_code)]
fn read_siginfo(fd: &OwnedFd) -> io::Result<libc::signalfd_siginfo> {
    let mut info = MaybeUninit::<libc::signalfd_siginfo>::uninit();
    let len = size_of::<libc::signalfd_siginfo>();
    // SAFETY: `info` is a writable buffer of exactly `len` bytes, and `fd`
    // is a valid open signalfd for the duration of the call.
    let n = unsafe { libc::read(fd.as_raw_fd(), info.as_mut_ptr().cast(), len) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if usize::try_from(n).ok() != Some(len) {
        return Err(io::Error::other("short read from the signalfd"));
    }
    // SAFETY: the kernel wrote a complete `signalfd_siginfo` (n == len).
    Ok(unsafe { info.assume_init() })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A SIGTERM from the test's own user (not root, or root in CI) is
    /// delivered to the signalfd with the sender's uid.
    #[tokio::test(flavor = "current_thread")]
    async fn reads_the_sender_of_sigterm() {
        block().unwrap();
        let term = Term::new().unwrap();
        // Directed at this thread, where SIGTERM is blocked, so it is queued
        // for the signalfd instead of terminating the test binary.
        #[allow(unsafe_code)]
        // SAFETY: tgkill with this process's pid, this thread's tid, and a
        // valid signal number; it has no memory arguments.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_tgkill,
                libc::getpid(),
                libc::gettid(),
                libc::SIGTERM,
            )
        };
        assert_eq!(rc, 0);
        let mut guard = term.fd.readable().await.unwrap();
        let info = guard
            .try_io(|fd| read_siginfo(fd.get_ref()))
            .unwrap()
            .unwrap();
        assert_eq!(info.ssi_signo, libc::SIGTERM as u32);
        assert_eq!(info.ssi_uid, rustix::process::getuid().as_raw());
        assert_eq!(info.ssi_code, libc::SI_TKILL);
    }

    #[allow(unsafe_code)]
    fn siginfo(uid: u32, code: i32) -> libc::signalfd_siginfo {
        // SAFETY: signalfd_siginfo is plain integers and padding, for which
        // all zero bytes are a valid value.
        let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
        info.ssi_signo = libc::SIGTERM as u32;
        info.ssi_uid = uid;
        info.ssi_code = code;
        info
    }

    /// Only root, through kill or tgkill, stops the VM; a uid in queued
    /// siginfo is chosen by the sender and never trusted.
    #[test]
    fn only_kernel_attributed_root_signals_count() {
        assert!(from_runtime(&siginfo(0, libc::SI_USER)));
        assert!(from_runtime(&siginfo(0, libc::SI_TKILL)));
        assert!(!from_runtime(&siginfo(0, libc::SI_QUEUE)));
        assert!(!from_runtime(&siginfo(0, -60)));
        assert!(!from_runtime(&siginfo(1000, libc::SI_USER)));
        assert!(!from_runtime(&siginfo(1000, libc::SI_TKILL)));
    }

    #[test]
    fn ignored_signals_are_logged_per_doubling() {
        let logged_upto = |n: u64| (1..=n).filter(|&i| logged(i)).count();
        assert!(logged(1) && logged(2) && !logged(3) && logged(4));
        assert_eq!(logged_upto(1_000_000), 20);
    }
}
