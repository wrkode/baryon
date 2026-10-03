//! Placement of Cloud Hypervisor in the Anchor container's cgroup.
//!
//! The VMM joins the cgroup of the Anchor container (a systemd scope created
//! by the container runtime), so the pod's CPU and memory limits, any
//! per-container cpuset, and the container's accounting apply to it, and
//! stopping the container scope stops the VMM (ADR 0005). baryon-virtd never
//! creates or removes cgroups.

use std::io;
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags};
use std::os::fd::OwnedFd;

/// Errors handling cgroups.
#[derive(Debug, thiserror::Error)]
pub enum CgroupError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path}: unexpected content {content:?}")]
    Parse { path: PathBuf, content: String },
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> CgroupError + '_ {
    move |source| CgroupError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Resource limits of a pod cgroup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PodLimits {
    /// `memory.max` in bytes, `None` if unlimited.
    pub memory_bytes: Option<u64>,
    /// CPU limit in milli-CPUs from `cpu.max`, `None` if unlimited.
    pub cpu_millis: Option<u64>,
}

/// The cgroup a VM's VMM runs in: the Anchor container's. It belongs to the
/// container runtime.
#[derive(Debug, Clone)]
pub struct VmmCgroup {
    pub path: PathBuf,
}

impl VmmCgroup {
    /// Use an existing cgroup (the Anchor container's) without taking
    /// ownership of it.
    pub fn existing(root: &Path, path: &str) -> Result<Self, CgroupError> {
        let path = root.join(path.trim_start_matches('/'));
        if !path.join("cgroup.procs").exists() {
            return Err(CgroupError::Io {
                path,
                source: io::Error::new(io::ErrorKind::NotFound, "cgroup does not exist"),
            });
        }
        Ok(Self { path })
    }

    /// Open `cgroup.procs` for writing. The child process writes `0` to it
    /// after fork to move itself before it executes Cloud Hypervisor.
    pub fn open_procs(&self) -> Result<OwnedFd, CgroupError> {
        let procs = self.path.join("cgroup.procs");
        rustix::fs::open(&procs, OFlags::WRONLY | OFlags::CLOEXEC, Mode::empty())
            .map_err(|e| io_err(&procs)(e.into()))
    }

    /// Process IDs currently in the cgroup.
    pub fn pids(&self) -> Result<Vec<i32>, CgroupError> {
        let procs = self.path.join("cgroup.procs");
        let content = match std::fs::read_to_string(&procs) {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err(&procs)(e)),
        };
        Ok(content
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect())
    }
}

/// Process IDs in a cgroup and all of its descendants, excluding `skip`.
pub fn tree_pids(dir: &Path, skip: &[i32]) -> Vec<i32> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        if let Ok(c) = std::fs::read_to_string(d.join("cgroup.procs")) {
            out.extend(
                c.lines()
                    .filter_map(|l| l.trim().parse::<i32>().ok())
                    .filter(|p| !skip.contains(p)),
            );
        }
        if let Ok(entries) = std::fs::read_dir(&d) {
            for e in entries.flatten() {
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    stack.push(e.path());
                }
            }
        }
    }
    out
}

/// Read the limits of a pod cgroup.
pub fn pod_limits(root: &Path, pod_path: &str) -> Result<PodLimits, CgroupError> {
    let pod = root.join(pod_path.trim_start_matches('/'));
    let memory_bytes = match read_optional(&pod.join("memory.max"))? {
        None => None,
        Some(s) => parse_memory_max(&s).ok_or_else(|| CgroupError::Parse {
            path: pod.join("memory.max"),
            content: s.clone(),
        })?,
    };
    let cpu_millis = match read_optional(&pod.join("cpu.max"))? {
        None => None,
        Some(s) => parse_cpu_max(&s).ok_or_else(|| CgroupError::Parse {
            path: pod.join("cpu.max"),
            content: s.clone(),
        })?,
    };
    Ok(PodLimits {
        memory_bytes,
        cpu_millis,
    })
}

fn read_optional(path: &Path) -> Result<Option<String>, CgroupError> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(path)(e)),
    }
}

/// `max` or a byte count. The outer `Option` is the parse result.
fn parse_memory_max(s: &str) -> Option<Option<u64>> {
    match s.trim() {
        "max" => Some(None),
        v => v.parse().ok().map(Some),
    }
}

/// `max 100000` or `200000 100000` (quota and period in microseconds).
fn parse_cpu_max(s: &str) -> Option<Option<u64>> {
    let mut parts = s.split_whitespace();
    let quota = parts.next()?;
    let period: u64 = parts.next().unwrap_or("100000").parse().ok()?;
    if quota == "max" {
        return Some(None);
    }
    let quota: u64 = quota.parse().ok()?;
    if period == 0 {
        return None;
    }
    Some(Some(quota * 1000 / period))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_limits() {
        assert_eq!(parse_memory_max("max\n"), Some(None));
        assert_eq!(parse_memory_max("2147483648\n"), Some(Some(2_147_483_648)));
        assert_eq!(parse_memory_max("x"), None);
        assert_eq!(parse_cpu_max("max 100000\n"), Some(None));
        assert_eq!(parse_cpu_max("200000 100000\n"), Some(Some(2000)));
        assert_eq!(parse_cpu_max("150000 100000"), Some(Some(1500)));
        assert_eq!(parse_cpu_max("1 0"), None);
    }

    #[test]
    fn limits_pids_and_tree_walk() {
        let root = tempfile::tempdir().unwrap();
        let pod = "kubepods/podx";
        std::fs::create_dir_all(root.path().join(pod).join("ctr")).unwrap();
        std::fs::write(root.path().join(pod).join("memory.max"), "1073741824\n").unwrap();
        std::fs::write(root.path().join(pod).join("cpu.max"), "100000 100000\n").unwrap();
        // On a real cgroupfs the kernel provides cgroup.procs.
        std::fs::write(root.path().join(pod).join("ctr/cgroup.procs"), "42\n").unwrap();

        let cg = VmmCgroup::existing(root.path(), &format!("/{pod}/ctr")).unwrap();
        assert_eq!(cg.pids().unwrap(), vec![42]);
        assert!(VmmCgroup::existing(root.path(), "kubepods/missing").is_err());

        let limits = pod_limits(root.path(), pod).unwrap();
        assert_eq!(limits.memory_bytes, Some(1 << 30));
        assert_eq!(limits.cpu_millis, Some(1000));

        // Tree walk sees nested cgroups and skips the given pids.
        std::fs::create_dir_all(root.path().join(pod).join("sandbox")).unwrap();
        std::fs::write(root.path().join(pod).join("sandbox/cgroup.procs"), "7\n").unwrap();
        let mut pids = tree_pids(&root.path().join(pod), &[42]);
        pids.sort_unstable();
        assert_eq!(pids, vec![7]);
    }
}
