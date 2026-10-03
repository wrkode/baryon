//! Derivation of the pod identity from a process's cgroup v2 path.
//!
//! kubelet creates one cgroup per pod. Its name encodes the pod UID:
//!
//! - systemd driver: `kubepods-burstable-pod<uid with _ for ->.slice`
//! - cgroupfs driver: `pod<uid>`
//!
//! Containers are children of that cgroup. The pod cgroup can appear at any
//! depth (QoS class, a kubelet cgroup root, a container host's own
//! hierarchy), so parsing looks for the last path component with a pod name
//! and does not assume a fixed prefix.

/// The pod cgroup of a container process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodCgroup {
    /// Pod UID in canonical form (lowercase, dash separated).
    pub pod_uid: String,
    /// Path of the pod cgroup relative to the cgroup2 mount, starting with `/`.
    pub pod_path: String,
    /// Path of the container cgroup the process is in.
    pub container_path: String,
}

impl PodCgroup {
    /// Whether the container cgroup is a systemd scope inside a systemd pod
    /// slice, which is what the kubelet systemd cgroup driver creates.
    pub fn systemd_managed(&self) -> bool {
        let leaf = |p: &str| p.rsplit('/').next().unwrap_or("").to_owned();
        leaf(&self.container_path).ends_with(".scope") && leaf(&self.pod_path).ends_with(".slice")
    }
}

/// Errors parsing a cgroup file.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CgroupError {
    #[error("process is not in a cgroup v2 hierarchy")]
    NotUnified,
    #[error("cgroup path {0:?} does not belong to a Kubernetes pod")]
    NotAPod(String),
    #[error("cgroup path {0:?} names the pod cgroup itself, not a container in it")]
    NotInContainer(String),
}

/// Parse the contents of `/proc/<pid>/cgroup`.
pub fn parse_proc_cgroup(contents: &str) -> Result<PodCgroup, CgroupError> {
    // On a unified hierarchy the only line is `0::<path>`.
    let path = contents
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .ok_or(CgroupError::NotUnified)?
        .trim();
    parse_path(path)
}

fn parse_path(path: &str) -> Result<PodCgroup, CgroupError> {
    let components: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    let (index, uid) = components
        .iter()
        .enumerate()
        .rev()
        .find_map(|(i, c)| pod_uid_from_component(c).map(|uid| (i, uid)))
        .ok_or_else(|| CgroupError::NotAPod(path.to_owned()))?;
    if index + 1 == components.len() {
        return Err(CgroupError::NotInContainer(path.to_owned()));
    }
    Ok(PodCgroup {
        pod_uid: uid,
        pod_path: format!("/{}", components[..=index].join("/")),
        container_path: format!("/{}", components.join("/")),
    })
}

fn pod_uid_from_component(c: &str) -> Option<String> {
    // systemd: kubepods-<qos>-pod<uid>.slice or kubepods-pod<uid>.slice
    if let Some(stem) = c.strip_suffix(".slice") {
        let last = stem.rsplit('-').next()?;
        let raw = last.strip_prefix("pod")?;
        return canonical_uid(&raw.replace('_', "-"));
    }
    // cgroupfs: pod<uid>
    canonical_uid(c.strip_prefix("pod")?)
}

fn canonical_uid(s: &str) -> Option<String> {
    let s = s.to_ascii_lowercase();
    let groups: Vec<&str> = s.split('-').collect();
    let lens = [8, 4, 4, 4, 12];
    if groups.len() != lens.len() {
        return None;
    }
    for (g, len) in groups.iter().zip(lens) {
        if g.len() != len || !g.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const UID: &str = "1b4e28ba-2fa1-11d2-883f-0016d3cca427";

    #[test]
    fn systemd_driver_burstable() {
        let s = "0::/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod1b4e28ba_2fa1_11d2_883f_0016d3cca427.slice/cri-containerd-abc123.scope\n";
        let p = parse_proc_cgroup(s).unwrap();
        assert_eq!(p.pod_uid, UID);
        assert_eq!(
            p.pod_path,
            "/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod1b4e28ba_2fa1_11d2_883f_0016d3cca427.slice"
        );
        assert!(p.container_path.ends_with("cri-containerd-abc123.scope"));
        assert!(p.systemd_managed());
    }

    #[test]
    fn systemd_driver_guaranteed() {
        let s = "0::/kubepods.slice/kubepods-pod1b4e28ba_2fa1_11d2_883f_0016d3cca427.slice/cri-containerd-x.scope";
        assert_eq!(parse_proc_cgroup(s).unwrap().pod_uid, UID);
    }

    #[test]
    fn cgroupfs_driver() {
        let s =
            "0::/kubepods/besteffort/pod1b4e28ba-2fa1-11d2-883f-0016d3cca427/0123456789abcdef\n";
        let p = parse_proc_cgroup(s).unwrap();
        assert_eq!(p.pod_uid, UID);
        assert_eq!(
            p.pod_path,
            "/kubepods/besteffort/pod1b4e28ba-2fa1-11d2-883f-0016d3cca427"
        );
        // Parsed, but not supported for VMs (see VmmCgroup).
        assert!(!p.systemd_managed());
    }

    #[test]
    fn nested_kubelet_root() {
        // kind runs kubelet with cgroupRoot=/kubelet inside the node's cgroup
        // namespace.
        let s = "0::/kubelet.slice/kubelet-kubepods.slice/kubelet-kubepods-besteffort.slice/kubelet-kubepods-besteffort-pod1b4e28ba_2fa1_11d2_883f_0016d3cca427.slice/cri-containerd-e.scope";
        let p = parse_proc_cgroup(s).unwrap();
        assert_eq!(p.pod_uid, UID);
        assert!(p.pod_path.starts_with("/kubelet.slice/"));
    }

    #[test]
    fn rejects_non_pod_and_v1() {
        assert_eq!(
            parse_proc_cgroup("0::/system.slice/sshd.service"),
            Err(CgroupError::NotAPod("/system.slice/sshd.service".into()))
        );
        assert_eq!(
            parse_proc_cgroup("12:memory:/kubepods/pod1\n"),
            Err(CgroupError::NotUnified)
        );
        // pod-like name but not a UUID
        assert!(matches!(
            parse_proc_cgroup("0::/kubepods/podabc/xyz"),
            Err(CgroupError::NotAPod(_))
        ));
    }

    #[test]
    fn rejects_process_in_pod_cgroup_itself() {
        let s = "0::/kubepods/pod1b4e28ba-2fa1-11d2-883f-0016d3cca427";
        assert!(matches!(
            parse_proc_cgroup(s),
            Err(CgroupError::NotInContainer(_))
        ));
    }
}
