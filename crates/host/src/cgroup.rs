//! Control group hierarchy discovery.

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::root::HostRoot;

/// Observed cgroup state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CgroupInfo {
    /// The unified cgroup v2 hierarchy is mounted at /sys/fs/cgroup.
    pub unified: bool,
    /// Controllers available at the root of the unified hierarchy.
    pub controllers: Vec<String>,
}

pub(crate) fn discover(root: &HostRoot) -> Result<CgroupInfo> {
    match root.read_optional("/sys/fs/cgroup/cgroup.controllers")? {
        Some(s) => Ok(CgroupInfo {
            unified: true,
            controllers: s.split_whitespace().map(str::to_owned).collect(),
        }),
        None => Ok(CgroupInfo {
            unified: false,
            controllers: Vec::new(),
        }),
    }
}
