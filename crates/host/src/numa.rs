//! NUMA topology discovery.

use serde::{Deserialize, Serialize};

use crate::cpulist;
use crate::error::{Error, Result};
use crate::memory::{self, HugepagePool};
use crate::root::HostRoot;

/// One NUMA node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NumaNode {
    pub id: u32,
    /// CPUs in kernel list format.
    pub cpus: String,
    pub cpu_count: u32,
    pub memory_total_bytes: u64,
    /// Distances to every node, indexed by node id order as reported by the
    /// kernel.
    pub distances: Vec<u32>,
    pub hugepages: Vec<HugepagePool>,
}

pub(crate) fn discover(root: &HostRoot) -> Result<Vec<NumaNode>> {
    let mut nodes = Vec::new();
    for name in root.read_dir_names("/sys/devices/system/node")? {
        let Some(id) = name
            .strip_prefix("node")
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let base = format!("/sys/devices/system/node/{name}");

        let cpus_raw = root.read_trimmed(&format!("{base}/cpulist"))?;
        let cpu_ids = cpulist::parse_cpu_list(&cpus_raw)
            .map_err(|e| Error::parse(root.path(&format!("{base}/cpulist")), e))?;

        let meminfo = root.read_trimmed(&format!("{base}/meminfo"))?;
        let memory_total_bytes = memory::meminfo_kib(&meminfo, "MemTotal")
            .map(|kib| kib * 1024)
            .ok_or_else(|| {
                Error::parse(root.path(&format!("{base}/meminfo")), "MemTotal not found")
            })?;

        let distances = match root.read_optional(&format!("{base}/distance"))? {
            None => Vec::new(),
            Some(s) => s
                .split_whitespace()
                .map(|d| d.parse::<u32>())
                .collect::<std::result::Result<_, _>>()
                .map_err(|e| Error::parse(root.path(&format!("{base}/distance")), e.to_string()))?,
        };

        let hugepages = memory::hugepage_pools(root, &format!("{base}/hugepages"), false)?;

        nodes.push(NumaNode {
            id,
            cpus: cpulist::format_cpu_list(&cpu_ids),
            cpu_count: u32::try_from(cpu_ids.len()).unwrap_or(u32::MAX),
            memory_total_bytes,
            distances,
            hugepages,
        });
    }
    nodes.sort_by_key(|n| n.id);
    Ok(nodes)
}
