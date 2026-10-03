//! Access to the files the VMM owns.
//!
//! `vms/<pod-uid>/vmm/` belongs to the per-VM uid, and Cloud Hypervisor may
//! create, remove, and link entries in it (it needs that to re-create its
//! serial socket when the guest reboots). baryon-virtd runs as root, so it
//! must not trust anything there: a compromised VMM could replace its log
//! with a symlink or hard link to a host file, or with a device that never
//! ends, and have root read it into a VM's status. Every access in this
//! module is relative to a directory descriptor, refuses symlinks, requires
//! the expected file type, the VM's uid as owner, and a single link, and
//! reads a bounded amount.

use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::Path;

use rustix::fs::{FileType, Mode, OFlags};

/// Size limit of the tmpfs mounted on each VMM directory. It holds the
/// Cloud Hypervisor log, the event file, and the serial socket; without a
/// limit of its own, a VMM could fill the host's /run.
pub const VMM_DIR_SIZE_BYTES: u64 = 32 * 1024 * 1024;

/// Mount a private tmpfs on `path` (an empty directory), owned by `uid` and
/// `gid`, mode 0700, size-limited, without devices, setuid, or execution.
pub fn mount_vmm_dir(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    use rustix::mount::MountFlags;
    let data = std::ffi::CString::new(format!(
        "size={VMM_DIR_SIZE_BYTES},nr_inodes=256,mode=0700,uid={uid},gid={gid}"
    ))
    .map_err(io::Error::other)?;
    rustix::mount::mount(
        "baryon-vmm",
        path,
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
        Some(data.as_c_str()),
    )?;
    Ok(())
}

/// Remove a VM's runtime directory, unmounting the VMM tmpfs first. Missing
/// parts are not an error.
pub fn remove_vm_dir(dir: &Path) {
    let vmm = dir.join("vmm");
    // EINVAL: not a mount point (never mounted, or already unmounted).
    match rustix::mount::unmount(&vmm, rustix::mount::UnmountFlags::DETACH) {
        Ok(()) | Err(rustix::io::Errno::INVAL | rustix::io::Errno::NOENT) => {}
        Err(e) => {
            tracing::warn!(path = %vmm.display(), error = %e, "cannot unmount the VMM directory")
        }
    }
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(path = %dir.display(), error = %e, "cannot remove the VM runtime directory")
        }
    }
}

/// The VMM's directory, opened without following links.
#[derive(Debug)]
pub struct VmmDir {
    fd: OwnedFd,
    owner: u32,
}

impl VmmDir {
    /// Open `path`, which must be a directory owned by `owner`.
    pub fn open(path: &Path, owner: u32) -> io::Result<Self> {
        let fd = rustix::fs::open(
            path,
            OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let st = rustix::fs::fstat(&fd)?;
        if FileType::from_raw_mode(st.st_mode) != FileType::Directory || st.st_uid != owner {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is not a directory owned by uid {owner}", path.display()),
            ));
        }
        Ok(Self { fd, owner })
    }

    /// Open `name` without following links and check its type and owner.
    fn open_checked(&self, name: &str, flags: OFlags, expected: FileType) -> io::Result<OwnedFd> {
        if name.contains('/') || name == "." || name == ".." {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{name:?} is not a plain file name"),
            ));
        }
        let fd = rustix::fs::openat(
            &self.fd,
            name,
            flags | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let st = rustix::fs::fstat(&fd)?;
        let kind = FileType::from_raw_mode(st.st_mode);
        if kind != expected || st.st_uid != self.owner || st.st_nlink != 1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{name} is not a {expected:?} owned by uid {} with one link",
                    self.owner
                ),
            ));
        }
        Ok(fd)
    }

    /// The last `max` bytes of the regular file `name`.
    pub fn read_tail(&self, name: &str, max: u64) -> io::Result<Vec<u8>> {
        let fd = self.open_checked(
            name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY,
            FileType::RegularFile,
        )?;
        let mut file = std::fs::File::from(fd);
        let len = file.metadata()?.len();
        file.seek(SeekFrom::Start(len.saturating_sub(max)))?;
        let mut out = Vec::new();
        file.take(max).read_to_end(&mut out)?;
        Ok(out)
    }

    /// Connect to the Unix socket `name`. The socket is opened as an
    /// `O_PATH` descriptor and checked first; the connection goes through
    /// that descriptor's `/proc/self/fd` link, so the entry cannot be swapped
    /// between the check and the connect.
    pub async fn connect(&self, name: &str) -> io::Result<tokio::net::UnixStream> {
        let fd = self.open_checked(name, OFlags::PATH, FileType::Socket)?;
        let path = format!("/proc/self/fd/{}", fd.as_fd().as_raw_fd());
        let stream = tokio::net::UnixStream::connect(&path).await;
        drop(fd);
        stream
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn dir() -> (tempfile::TempDir, VmmDir) {
        let t = tempfile::tempdir().unwrap();
        let uid = std::fs::metadata(t.path()).unwrap().uid();
        let d = VmmDir::open(t.path(), uid).unwrap();
        (t, d)
    }

    #[test]
    fn reads_a_bounded_tail() {
        let (t, d) = dir();
        std::fs::write(t.path().join("log"), b"0123456789").unwrap();
        assert_eq!(d.read_tail("log", 4).unwrap(), b"6789");
        assert_eq!(d.read_tail("log", 100).unwrap(), b"0123456789");
    }

    #[test]
    fn refuses_symlinks_hard_links_and_other_types() {
        let (t, d) = dir();
        std::fs::write(t.path().join("real"), b"secret").unwrap();
        std::os::unix::fs::symlink(t.path().join("real"), t.path().join("link")).unwrap();
        assert!(d.read_tail("link", 100).is_err());
        std::fs::hard_link(t.path().join("real"), t.path().join("hard")).unwrap();
        assert!(d.read_tail("hard", 100).is_err());
        assert!(d.read_tail("real", 100).is_err(), "two links now");
        std::fs::create_dir(t.path().join("sub")).unwrap();
        assert!(d.read_tail("sub", 100).is_err());
        assert!(d.read_tail("../x", 100).is_err());
        assert!(d.read_tail("missing", 100).is_err());
    }

    #[test]
    fn refuses_a_directory_with_another_owner() {
        let t = tempfile::tempdir().unwrap();
        let uid = std::fs::metadata(t.path()).unwrap().uid();
        assert!(VmmDir::open(t.path(), uid.wrapping_add(1)).is_err());
    }

    #[tokio::test]
    async fn connects_through_the_checked_descriptor() {
        let (t, d) = dir();
        let listener = tokio::net::UnixListener::bind(t.path().join("serial.sock")).unwrap();
        let accept = tokio::spawn(async move { listener.accept().await.map(|_| ()) });
        d.connect("serial.sock").await.unwrap();
        accept.await.unwrap().unwrap();

        std::os::unix::fs::symlink(t.path().join("serial.sock"), t.path().join("alias")).unwrap();
        assert!(d.connect("alias").await.is_err());
        std::fs::write(t.path().join("file"), b"x").unwrap();
        assert!(d.connect("file").await.is_err());
    }
}
