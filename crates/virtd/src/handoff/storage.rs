//! Storage handoff: make a disk that is visible only inside the Anchor
//! container available to Cloud Hypervisor on the host.
//!
//! The disk path is resolved inside the Anchor process's root with
//! `openat2(RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS)`. Absolute symlinks and
//! `..` cannot leave the container root, and `/proc` magic links cannot be
//! used to jump elsewhere. The result is an `O_PATH` descriptor: it grants
//! no read or write access by itself. Cloud Hypervisor reopens it through
//! `/proc/self/fd/N` with its own credentials (the pod user), so ordinary
//! permission checks decide whether the VMM may use the disk. See ADR 0006.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::fs::{FileType, Mode, OFlags, ResolveFlags};

use crate::peer::pidfd_alive;

/// Kind of disk backing found in the container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskKind {
    /// A regular file on a filesystem-mode volume.
    File,
    /// A block device node of a block-mode volume.
    BlockDevice,
}

/// A resolved disk ready to be inherited by Cloud Hypervisor.
#[derive(Debug)]
pub struct ResolvedDisk {
    pub name: String,
    pub kind: DiskKind,
    /// `O_PATH` descriptor of the disk.
    pub fd: OwnedFd,
    pub size_bytes: u64,
    pub read_only: bool,
}

/// Errors resolving a disk.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("disk {name}: container path {path:?} must be absolute")]
    RelativePath { name: String, path: String },
    #[error("disk {name}: resolving {path:?} in the anchor container: {source}")]
    Resolve {
        name: String,
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("disk {name}: {path:?} is a {found}, expected a regular file or block device")]
    WrongType {
        name: String,
        path: String,
        found: &'static str,
    },
    #[error("disk {name}: {path:?} is empty")]
    Empty { name: String, path: String },
    #[error(
        "disk {name}: the block device (owner {owner}:{group}, mode {mode:o}) is not {access} for the VM's host identity; the container runtime must give block volume devices the pod's runAsUser and runAsGroup (containerd: device_ownership_from_security_context = true)"
    )]
    DeviceAccess {
        name: String,
        owner: u32,
        group: u32,
        mode: u32,
        access: &'static str,
    },
}

/// Check that the VM's identity can open a block device disk the way the VMM
/// will (read and write, or read only). Raw block devices are not files:
/// Baryon does not change their ownership or mode. Access must come from the
/// container runtime, which can own a block volume's device node by the
/// pod's runAsUser and runAsGroup; this check makes a missing runtime
/// setting a clear start failure instead of an EACCES inside the VMM.
pub fn check_device_access(
    disk: &ResolvedDisk,
    uid: u32,
    gid: u32,
    groups: &[u32],
) -> Result<(), StorageError> {
    let stat = rustix::fs::fstat(disk.fd.as_fd()).map_err(|e| StorageError::Resolve {
        name: disk.name.clone(),
        path: String::new(),
        source: e.into(),
    })?;
    let mode = stat.st_mode & 0o777;
    let bits = if stat.st_uid == uid {
        (mode >> 6) & 0o7
    } else if stat.st_gid == gid || groups.contains(&stat.st_gid) {
        (mode >> 3) & 0o7
    } else {
        mode & 0o7
    };
    let need = if disk.read_only { 0o4 } else { 0o6 };
    if bits & need == need {
        Ok(())
    } else {
        Err(StorageError::DeviceAccess {
            name: disk.name.clone(),
            owner: stat.st_uid,
            group: stat.st_gid,
            mode,
            access: if disk.read_only {
                "readable"
            } else {
                "readable and writable"
            },
        })
    }
}

/// Open the root directory of the process behind `pidfd`.
///
/// `/proc/<pid>/root` is opened by PID. The pidfd is checked afterwards: if
/// the process is still alive the PID cannot have been reused, so the
/// directory belongs to the intended process.
pub fn open_process_root(pid: i32, pidfd: BorrowedFd<'_>) -> io::Result<OwnedFd> {
    let root = rustix::fs::open(
        format!("/proc/{pid}/root"),
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    if !pidfd_alive(pidfd) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "process exited while its root was opened",
        ));
    }
    Ok(root)
}

/// Resolve one disk inside a container root.
pub fn resolve_disk(
    root: BorrowedFd<'_>,
    name: &str,
    container_path: &str,
    read_only: bool,
) -> Result<ResolvedDisk, StorageError> {
    if !container_path.starts_with('/') {
        return Err(StorageError::RelativePath {
            name: name.to_owned(),
            path: container_path.to_owned(),
        });
    }
    let resolve_err = |source: io::Error| StorageError::Resolve {
        name: name.to_owned(),
        path: container_path.to_owned(),
        source,
    };
    let fd = rustix::fs::openat2(
        root,
        container_path,
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::IN_ROOT | ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(|e| resolve_err(e.into()))?;

    let stat = rustix::fs::fstat(fd.as_fd()).map_err(|e| resolve_err(e.into()))?;
    let (kind, size_bytes) = match FileType::from_raw_mode(stat.st_mode) {
        FileType::RegularFile => (DiskKind::File, u64::try_from(stat.st_size).unwrap_or(0)),
        FileType::BlockDevice => (DiskKind::BlockDevice, 0),
        other => {
            return Err(StorageError::WrongType {
                name: name.to_owned(),
                path: container_path.to_owned(),
                found: file_type_name(other),
            });
        }
    };
    if kind == DiskKind::File && size_bytes == 0 {
        return Err(StorageError::Empty {
            name: name.to_owned(),
            path: container_path.to_owned(),
        });
    }
    Ok(ResolvedDisk {
        name: name.to_owned(),
        kind,
        fd,
        size_bytes,
        read_only,
    })
}

fn file_type_name(t: FileType) -> &'static str {
    match t {
        FileType::RegularFile => "regular file",
        FileType::Directory => "directory",
        FileType::Symlink => "symlink",
        FileType::Fifo => "fifo",
        FileType::Socket => "socket",
        FileType::CharacterDevice => "character device",
        FileType::BlockDevice => "block device",
        FileType::Unknown => "unknown file type",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn root(dir: &std::path::Path) -> OwnedFd {
        rustix::fs::open(
            dir,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap()
    }

    fn setup() -> (tempfile::TempDir, OwnedFd) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("var/lib/baryon/disks/root")).unwrap();
        std::fs::write(
            dir.path().join("var/lib/baryon/disks/root/disk.img"),
            vec![0u8; 4096],
        )
        .unwrap();
        let r = root(dir.path());
        (dir, r)
    }

    #[test]
    fn device_access_follows_owner_group_and_mode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // The check looks only at ownership and mode bits, so a regular file
        // stands in for the device node.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dev");
        std::fs::write(&path, b"x").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let (me, my_gid) = (meta.uid(), meta.gid());
        let disk = |ro: bool| ResolvedDisk {
            name: "root".into(),
            kind: DiskKind::BlockDevice,
            fd: rustix::fs::open(&path, OFlags::PATH | OFlags::CLOEXEC, Mode::empty()).unwrap(),
            size_bytes: 0,
            read_only: ro,
        };
        let chmod =
            |m: u32| std::fs::set_permissions(&path, std::fs::Permissions::from_mode(m)).unwrap();

        chmod(0o600);
        assert!(check_device_access(&disk(false), me, my_gid, &[]).is_ok());
        // Another identity, no group access: refused.
        assert!(matches!(
            check_device_access(&disk(false), me + 1, my_gid + 1, &[]),
            Err(StorageError::DeviceAccess { .. })
        ));
        // Group access through a supplementary group.
        chmod(0o660);
        assert!(check_device_access(&disk(false), me + 1, my_gid + 1, &[my_gid]).is_ok());
        // Read-only disks need only read access.
        chmod(0o640);
        assert!(check_device_access(&disk(true), me + 1, my_gid, &[]).is_ok());
        assert!(check_device_access(&disk(false), me + 1, my_gid, &[]).is_err());
    }

    #[test]
    fn resolves_regular_file() {
        let (_dir, r) = setup();
        let d = resolve_disk(
            r.as_fd(),
            "root",
            "/var/lib/baryon/disks/root/disk.img",
            false,
        )
        .unwrap();
        assert_eq!(d.kind, DiskKind::File);
        assert_eq!(d.size_bytes, 4096);
    }

    #[test]
    fn absolute_symlink_stays_inside_root() {
        let (dir, r) = setup();
        // A host file that must not be reachable.
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"secret").unwrap();
        symlink(
            outside.path(),
            dir.path().join("var/lib/baryon/disks/root/escape.img"),
        )
        .unwrap();
        let err = resolve_disk(
            r.as_fd(),
            "root",
            "/var/lib/baryon/disks/root/escape.img",
            false,
        )
        .unwrap_err();
        // Resolution happens inside the root, where the target does not exist.
        assert!(matches!(err, StorageError::Resolve { .. }), "{err}");
    }

    #[test]
    fn dotdot_cannot_escape() {
        let (_dir, r) = setup();
        let err = resolve_disk(r.as_fd(), "root", "/../../../../etc/passwd", false).unwrap_err();
        assert!(matches!(err, StorageError::Resolve { .. }), "{err}");
    }

    #[test]
    fn relative_symlink_inside_root_is_followed() {
        let (dir, r) = setup();
        symlink(
            "disk.img",
            dir.path().join("var/lib/baryon/disks/root/link.img"),
        )
        .unwrap();
        let d = resolve_disk(
            r.as_fd(),
            "root",
            "/var/lib/baryon/disks/root/link.img",
            false,
        )
        .unwrap();
        assert_eq!(d.kind, DiskKind::File);
    }

    #[test]
    fn rejects_directories_empty_files_and_relative_paths() {
        let (dir, r) = setup();
        assert!(matches!(
            resolve_disk(r.as_fd(), "root", "/var/lib/baryon/disks/root", false),
            Err(StorageError::WrongType {
                found: "directory",
                ..
            })
        ));
        std::fs::write(dir.path().join("empty.img"), b"").unwrap();
        assert!(matches!(
            resolve_disk(r.as_fd(), "root", "/empty.img", false),
            Err(StorageError::Empty { .. })
        ));
        assert!(matches!(
            resolve_disk(r.as_fd(), "root", "var/lib/disk.img", false),
            Err(StorageError::RelativePath { .. })
        ));
    }

    #[test]
    fn reopen_through_proc_self_fd() {
        // The mechanism Cloud Hypervisor relies on: an O_PATH descriptor can
        // be reopened for read and write through /proc/self/fd.
        let (_dir, r) = setup();
        let d = resolve_disk(
            r.as_fd(),
            "root",
            "/var/lib/baryon/disks/root/disk.img",
            false,
        )
        .unwrap();
        let path = format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&d.fd));
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        assert_eq!(f.metadata().unwrap().len(), 4096);
    }
}
