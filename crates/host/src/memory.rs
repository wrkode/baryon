//! Memory and hugepage discovery.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::root::HostRoot;

/// A hugepage pool, either system-wide or for one NUMA node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HugepagePool {
    pub page_size_bytes: u64,
    pub total: u64,
    pub free: u64,
    /// Reserved pages. Only reported for system-wide pools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reserved: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surplus: Option<u64>,
}

/// Observed memory state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryInfo {
    pub total_bytes: u64,
    pub hugepages: Vec<HugepagePool>,
    /// Active transparent hugepage mode: `always`, `madvise`, or `never`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transparent_hugepage: Option<String>,
}

pub(crate) fn discover(root: &HostRoot) -> Result<MemoryInfo> {
    let meminfo = root.read_trimmed("/proc/meminfo")?;
    let total_bytes = meminfo_kib(&meminfo, "MemTotal")
        .map(|kib| kib * 1024)
        .ok_or_else(|| Error::parse(root.path("/proc/meminfo"), "MemTotal not found"))?;

    let hugepages = hugepage_pools(root, "/sys/kernel/mm/hugepages", true)?;

    let transparent_hugepage = root
        .read_optional("/sys/kernel/mm/transparent_hugepage/enabled")?
        .and_then(|s| selected_option(&s));

    Ok(MemoryInfo {
        total_bytes,
        hugepages,
        transparent_hugepage,
    })
}

/// Read hugepage pools from a directory containing `hugepages-<size>kB`
/// entries. `system_wide` selects whether the reserved count is available.
pub(crate) fn hugepage_pools(
    root: &HostRoot,
    dir: &str,
    system_wide: bool,
) -> Result<Vec<HugepagePool>> {
    let mut pools = Vec::new();
    for name in root.read_dir_names(dir)? {
        let Some(size_kib) = parse_pool_dir_name(&name) else {
            continue;
        };
        let base = format!("{dir}/{name}");
        let required = |file: &str| -> Result<u64> {
            let path = format!("{base}/{file}");
            root.read_u64_optional(&path)?
                .ok_or_else(|| Error::parse(root.path(&path), "missing hugepage counter"))
        };
        pools.push(HugepagePool {
            page_size_bytes: size_kib * 1024,
            total: required("nr_hugepages")?,
            free: required("free_hugepages")?,
            reserved: if system_wide {
                root.read_u64_optional(&format!("{base}/resv_hugepages"))?
            } else {
                None
            },
            surplus: root.read_u64_optional(&format!("{base}/surplus_hugepages"))?,
        });
    }
    pools.sort_by_key(|p| p.page_size_bytes);
    Ok(pools)
}

/// `hugepages-2048kB` yields 2048.
fn parse_pool_dir_name(name: &str) -> Option<u64> {
    name.strip_prefix("hugepages-")?
        .strip_suffix("kB")?
        .parse()
        .ok()
}

/// Value in KiB of a /proc/meminfo style line such as `MemTotal:  1024 kB`.
/// Also accepts the per-node format `Node 0 MemTotal:  1024 kB`.
pub(crate) fn meminfo_kib(meminfo: &str, key: &str) -> Option<u64> {
    meminfo.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        if k.split_whitespace().last()? != key {
            return None;
        }
        let mut parts = v.split_whitespace();
        let value: u64 = parts.next()?.parse().ok()?;
        match parts.next() {
            Some("kB") | None => Some(value),
            Some(_) => None,
        }
    })
}

/// The bracketed option in a sysfs selector such as `always [madvise] never`.
fn selected_option(s: &str) -> Option<String> {
    s.split_whitespace()
        .find_map(|w| w.strip_prefix('[')?.strip_suffix(']').map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_meminfo() {
        let s = "MemTotal:       131608648 kB\nMemFree:         5890844 kB\n";
        assert_eq!(meminfo_kib(s, "MemTotal"), Some(131_608_648));
        assert_eq!(meminfo_kib(s, "MemFree"), Some(5_890_844));
        assert_eq!(meminfo_kib(s, "Missing"), None);
    }

    #[test]
    fn parses_node_meminfo() {
        let s = "Node 0 MemTotal:       131608648 kB\nNode 0 MemFree:  1 kB\n";
        assert_eq!(meminfo_kib(s, "MemTotal"), Some(131_608_648));
    }

    #[test]
    fn parses_pool_names() {
        assert_eq!(parse_pool_dir_name("hugepages-2048kB"), Some(2048));
        assert_eq!(parse_pool_dir_name("hugepages-1048576kB"), Some(1_048_576));
        assert_eq!(parse_pool_dir_name("other"), None);
    }

    #[test]
    fn parses_selector() {
        assert_eq!(
            selected_option("always [madvise] never").as_deref(),
            Some("madvise")
        );
        assert_eq!(selected_option("always madvise never"), None);
    }
}
