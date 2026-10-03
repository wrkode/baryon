use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Filesystem root that discovery reads from.
///
/// Production code uses [`HostRoot::system`]. Tests point the root at a
/// directory laid out like sysfs, procfs, /etc, and /dev so that discovery
/// can be exercised without real hardware.
#[derive(Debug, Clone)]
pub struct HostRoot {
    root: PathBuf,
}

impl HostRoot {
    /// The real host filesystem.
    pub fn system() -> Self {
        Self {
            root: PathBuf::from("/"),
        }
    }

    /// A host filesystem rooted at `root`, used for fixtures.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Whether this root is the real host filesystem.
    pub fn is_system(&self) -> bool {
        self.root == Path::new("/")
    }

    /// Resolve an absolute host path such as `/sys/class/iommu` under this root.
    pub fn path(&self, abs: &str) -> PathBuf {
        self.root.join(abs.trim_start_matches('/'))
    }

    pub(crate) fn exists(&self, abs: &str) -> bool {
        self.path(abs).exists()
    }

    pub(crate) fn read_trimmed(&self, abs: &str) -> Result<String> {
        let path = self.path(abs);
        fs::read_to_string(&path)
            .map(|s| s.trim().to_owned())
            .map_err(|e| Error::io(path, e))
    }

    /// Read a file, returning `None` when it does not exist.
    pub(crate) fn read_optional(&self, abs: &str) -> Result<Option<String>> {
        let path = self.path(abs);
        match fs::read_to_string(&path) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::io(path, e)),
        }
    }

    /// Read a file and parse it as an unsigned integer, returning `None` when
    /// the file does not exist.
    pub(crate) fn read_u64_optional(&self, abs: &str) -> Result<Option<u64>> {
        match self.read_optional(abs)? {
            None => Ok(None),
            Some(s) => s
                .trim()
                .parse::<u64>()
                .map(Some)
                .map_err(|e| Error::parse(self.path(abs), e.to_string())),
        }
    }

    /// Sorted entry names of a directory. A missing directory yields an empty list.
    pub(crate) fn read_dir_names(&self, abs: &str) -> Result<Vec<String>> {
        let path = self.path(abs);
        let entries = match fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::io(path, e)),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| Error::io(&path, e))?;
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        Ok(names)
    }

    /// Final path component of a symlink target, for sysfs links such as
    /// `driver` and `iommu_group`. A missing link yields `None`.
    pub(crate) fn read_link_name(&self, abs: &str) -> Result<Option<String>> {
        let path = self.path(abs);
        match fs::read_link(&path) {
            Ok(target) => Ok(target.file_name().map(|n| n.to_string_lossy().into_owned())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::io(path, e)),
        }
    }
}
