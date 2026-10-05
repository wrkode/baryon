//! Baryon's seccomp layer for the processes baryon-virtd starts as a VM's
//! identity: Cloud Hypervisor and the DHCP responder.
//!
//! Cloud Hypervisor installs its own per-thread seccomp filters, which allow
//! what any of its features needs, and none on its main thread. Its
//! configuration offers no way to narrow them. Seccomp filters stack and the
//! strictest result wins, so baryon-virtd installs this filter before exec:
//! it is the floor for every thread of both processes, and the only filter
//! on Cloud Hypervisor's main thread and in the DHCP responder. It denies
//! only what Baryon's supported feature set never needs (checked against
//! Cloud Hypervisor v53.0; see docs/security.md, "Baryon seccomp layer"):
//!
//! - `userfaultfd(2)`: used by Cloud Hypervisor only for demand-paged
//!   snapshot restore and post-copy live migration, neither supported by
//!   Baryon. A user-mode-only userfaultfd is available to unprivileged
//!   processes whatever `vm.unprivileged_userfaultfd` says.
//! - `ptrace(2)`, `process_vm_readv(2)`, `process_vm_writev(2)`,
//!   `pidfd_getfd(2)`: no Cloud Hypervisor thread allows them; denied so the
//!   VMM and the responder of a VM, which share a uid, cannot reach into
//!   each other through the threads Cloud Hypervisor leaves unfiltered.
//! - `socket(2)` for any family but `AF_UNIX`: Baryon passes TAP devices as
//!   file descriptors, and Cloud Hypervisor's TAP ioctls on that path use
//!   `AF_UNIX` sockets; inet sockets serve only TAP creation with an
//!   address and the migration transports.
//! - System calls of another ABI: another architecture (for example i386
//!   calls of an x86_64 process) kills the process; x32 calls fail.
//!
//! Denied calls fail with `EPERM`. Everything else is left to Cloud
//! Hypervisor's filters.

use std::io;

/// `AUDIT_ARCH_*` of the native ABI (linux/audit.h).
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xc000_00b7;

/// Offsets in `struct seccomp_data` (linux/seccomp.h): the system call
/// number, the architecture, and the low 32 bits of the first argument
/// (both supported architectures are little-endian).
const DATA_NR: u32 = 0;
const DATA_ARCH: u32 = 4;
const DATA_ARG0_LO: u32 = 16;

/// System call numbers at or above this are x32 calls on x86_64.
#[cfg(target_arch = "x86_64")]
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

const RET_ALLOW: u32 = libc::SECCOMP_RET_ALLOW;
const RET_EPERM: u32 = libc::SECCOMP_RET_ERRNO | (libc::EPERM as u32 & libc::SECCOMP_RET_DATA);
const RET_KILL: u32 = libc::SECCOMP_RET_KILL_PROCESS;

const LD_W_ABS: u32 = libc::BPF_LD | libc::BPF_W | libc::BPF_ABS;
const JEQ_K: u32 = libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K;
#[cfg(target_arch = "x86_64")]
const JGE_K: u32 = libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K;
const RET_K: u32 = libc::BPF_RET | libc::BPF_K;

/// System calls the filter denies outright.
const DENIED: [libc::c_long; 5] = [
    libc::SYS_userfaultfd,
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_pidfd_getfd,
];

/// Where a conditional jump goes.
#[derive(Clone, Copy)]
enum To {
    Next,
    Allow,
    Deny,
}

/// The BPF program, built before fork so that installing it allocates
/// nothing.
#[derive(Clone)]
pub struct Filter(Vec<libc::sock_filter>);

impl std::fmt::Debug for Filter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Filter")
            .field("len", &self.0.len())
            .finish()
    }
}

/// The filter described in the module documentation.
pub fn filter() -> Filter {
    // (instruction code, k, where to go if true, where to go if false)
    let mut body: Vec<(u32, u32, To, To)> = vec![(LD_W_ABS, DATA_NR, To::Next, To::Next)];
    #[cfg(target_arch = "x86_64")]
    body.push((JGE_K, X32_SYSCALL_BIT, To::Deny, To::Next));
    for nr in DENIED {
        // System call numbers are small non-negative constants.
        body.push((JEQ_K, nr as u32, To::Deny, To::Next));
    }
    body.push((JEQ_K, libc::SYS_socket as u32, To::Next, To::Allow));
    body.push((LD_W_ABS, DATA_ARG0_LO, To::Next, To::Next));
    body.push((JEQ_K, libc::AF_UNIX as u32, To::Allow, To::Deny));

    // Prologue: kill on a foreign architecture. Epilogue: the two returns.
    let mut prog = vec![
        stmt(LD_W_ABS, DATA_ARCH),
        jump(JEQ_K, AUDIT_ARCH, 1, 0),
        stmt(RET_K, RET_KILL),
    ];
    let allow = prog.len() + body.len();
    let deny = allow + 1;
    for (code, k, jt, jf) in body {
        let here = prog.len();
        if code == LD_W_ABS {
            prog.push(stmt(code, k));
            continue;
        }
        let off = |to: To| -> u8 {
            let target = match to {
                To::Next => here + 1,
                To::Allow => allow,
                To::Deny => deny,
            };
            // Forward jumps within a program of a few dozen instructions.
            u8::try_from(target - here - 1).unwrap_or(u8::MAX)
        };
        prog.push(jump(code, k, off(jt), off(jf)));
    }
    prog.push(stmt(RET_K, RET_ALLOW));
    prog.push(stmt(RET_K, RET_EPERM));
    Filter(prog)
}

const fn stmt(code: u32, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code: code as u16,
        jt: 0,
        jf: 0,
        k,
    }
}

const fn jump(code: u32, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter {
        code: code as u16,
        jt,
        jf,
        k,
    }
}

/// Install `filter` on the calling thread. Async-signal-safe: one `prctl`
/// on memory the caller owns, no allocation.
///
/// # Safety
///
/// Call only in a single-threaded child between fork and exec, after
/// `PR_SET_NO_NEW_PRIVS`: the filter applies to the calling thread only
/// and cannot be removed, so in a multi-threaded process the other threads
/// would stay unfiltered.
#[allow(unsafe_code)]
pub unsafe fn install(filter: &mut Filter) -> io::Result<()> {
    let prog = libc::sock_fprog {
        // A few dozen instructions, far below BPF_MAXINSNS.
        len: u16::try_from(filter.0.len()).unwrap_or(u16::MAX),
        filter: filter.0.as_mut_ptr(),
    };
    // SAFETY: `prog` points to `len` initialized `sock_filter` entries owned
    // by the caller for the duration of the call; the kernel copies the
    // program and keeps no reference to it.
    let r = unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER as libc::c_ulong,
            &prog as *const libc::sock_fprog,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `UFFD_USER_MODE_ONLY` (linux/userfaultfd.h): allowed to unprivileged
    /// processes regardless of `vm.unprivileged_userfaultfd`.
    const UFFD_USER_MODE_ONLY: libc::c_long = 1;

    /// Run `check` in a forked child, with the filter installed if
    /// `filtered`; return the raw wait status.
    #[allow(unsafe_code)]
    fn in_child(filtered: bool, check: fn() -> i32) -> i32 {
        let mut f = filter();
        // SAFETY: the child calls only async-signal-safe functions (prctl,
        // syscall, close, _exit) before exiting; the parent waits for it.
        unsafe {
            let pid = libc::fork();
            assert!(pid >= 0, "fork failed");
            if pid == 0 {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    libc::_exit(90);
                }
                if filtered && install(&mut f).is_err() {
                    libc::_exit(91);
                }
                libc::_exit(check());
            }
            let mut status = 0;
            assert_eq!(libc::waitpid(pid, &mut status, 0), pid);
            status
        }
    }

    fn exit_code(status: i32) -> i32 {
        assert!(libc::WIFEXITED(status), "child did not exit: {status:#x}");
        libc::WEXITSTATUS(status)
    }

    /// 0 if the call failed with EPERM, otherwise `code`.
    #[allow(unsafe_code)]
    fn eperm(r: libc::c_long, code: i32) -> i32 {
        // SAFETY: reads this thread's errno.
        if r < 0 && unsafe { *libc::__errno_location() } == libc::EPERM {
            0
        } else {
            code
        }
    }

    #[allow(unsafe_code)]
    fn user_mode_userfaultfd() -> i32 {
        // SAFETY: plain system calls without pointer arguments.
        unsafe {
            let fd = libc::syscall(
                libc::SYS_userfaultfd,
                libc::O_CLOEXEC as libc::c_long | UFFD_USER_MODE_ONLY,
            );
            if fd >= 0 {
                libc::close(fd as i32);
                return 0;
            }
            *libc::__errno_location()
        }
    }

    #[test]
    fn user_mode_userfaultfd_is_denied() {
        // Control: without the filter the call must work, or the test
        // would pass on a host that refuses it anyway.
        let control = exit_code(in_child(false, user_mode_userfaultfd));
        if control == libc::ENOSYS {
            eprintln!("kernel without userfaultfd; skipping");
            return;
        }
        assert_eq!(
            control, 0,
            "unfiltered user-mode userfaultfd failed: errno {control}"
        );
        assert_eq!(
            exit_code(in_child(true, user_mode_userfaultfd)),
            libc::EPERM
        );
    }

    #[test]
    fn process_access_calls_are_denied_and_others_work() {
        #[allow(unsafe_code)]
        fn check() -> i32 {
            // SAFETY: system calls with null or invalid pointers are refused
            // by the filter before the kernel reads them; getpid takes none.
            unsafe {
                if libc::syscall(libc::SYS_getpid) <= 0 {
                    return 1;
                }
                let r = eperm(
                    libc::syscall(libc::SYS_ptrace, libc::PTRACE_TRACEME, 0, 0, 0),
                    2,
                );
                if r != 0 {
                    return r;
                }
                let pid = libc::syscall(libc::SYS_getpid);
                let r = eperm(
                    libc::syscall(libc::SYS_process_vm_readv, pid, 0, 0, 0, 0, 0),
                    3,
                );
                if r != 0 {
                    return r;
                }
                let r = eperm(
                    libc::syscall(libc::SYS_process_vm_writev, pid, 0, 0, 0, 0, 0),
                    4,
                );
                if r != 0 {
                    return r;
                }
                eperm(libc::syscall(libc::SYS_pidfd_getfd, -1, 0, 0), 5)
            }
        }
        assert_eq!(exit_code(in_child(true, check)), 0);
    }

    #[test]
    fn only_unix_sockets_can_be_created() {
        #[allow(unsafe_code)]
        fn check() -> i32 {
            // SAFETY: socket and close on descriptors this child owns.
            unsafe {
                let s = libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
                if s < 0 {
                    return 10;
                }
                libc::close(s);
                for (family, code) in [
                    (libc::AF_INET, 11),
                    (libc::AF_INET6, 12),
                    (libc::AF_NETLINK, 13),
                    (libc::AF_PACKET, 14),
                ] {
                    let s = libc::socket(family, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
                    let r = eperm(s.into(), code);
                    if r != 0 {
                        return r;
                    }
                }
            }
            0
        }
        assert_eq!(exit_code(in_child(true, check)), 0);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x32_calls_are_denied() {
        #[allow(unsafe_code)]
        fn check() -> i32 {
            // SAFETY: x32 getpid, no arguments.
            eperm(
                unsafe { libc::syscall(libc::SYS_getpid | X32_SYSCALL_BIT as libc::c_long) },
                20,
            )
        }
        assert_eq!(exit_code(in_child(true, check)), 0);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn foreign_architecture_calls_kill_the_process() {
        #[allow(unsafe_code)]
        fn check() -> i32 {
            // SAFETY: i386 getpid (number 20) through the legacy gate; it
            // takes no arguments and touches no memory.
            unsafe {
                std::arch::asm!("int 0x80", inlateout("eax") 20 => _, options(nostack));
            }
            30
        }
        let status = in_child(true, check);
        if libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGSEGV {
            eprintln!("IA32 emulation disabled; skipping");
            return;
        }
        assert!(
            libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGSYS,
            "want SIGSYS, got status {status:#x}"
        );
    }

    #[test]
    fn jumps_stay_inside_the_program() {
        let f = filter();
        let len = f.0.len();
        for (i, ins) in f.0.iter().enumerate() {
            if u32::from(ins.code) & 0x07 == libc::BPF_JMP {
                assert!(i + 1 + usize::from(ins.jt) < len);
                assert!(i + 1 + usize::from(ins.jf) < len);
            }
        }
        assert_eq!(u32::from(f.0[len - 1].code), RET_K);
    }
}
