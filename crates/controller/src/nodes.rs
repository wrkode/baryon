//! baryon-controller's view of VirtualizationNodes, read for the
//! NetworkReady condition of the VMs on each node.
//!
//! A node's report is fresh while the controller has seen its heartbeat
//! change within [`STALE`]. Freshness uses the controller's own clock, so
//! clock skew between nodes and the controller does not matter. The only
//! use of the node's own clock is at the first sighting after a controller
//! start: a heartbeat already older than [`STALE`] counts as stale.
//!
//! The controller watches VirtualizationNodes twice: once through this
//! reflector, which the reconciler reads, and once as a trigger for the
//! VMs on a node. The two watches are not ordered; [`NodeView::settled`]
//! tells the reconciler to look again when the store lags the trigger.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use k8s_openapi::jiff::Timestamp;
use kube::ResourceExt;
use kube::runtime::reflector::{ObjectRef, Store};

use baryon_api::{VirtualizationNode, conditions};

/// How long a node's report is trusted after the controller last saw its
/// heartbeat change: three node agent intervals (at most 30 s each).
pub(crate) const STALE: Duration = Duration::from_secs(STALE_SECONDS as u64);
const STALE_SECONDS: i64 = 90;

pub(crate) struct NodeView {
    store: Store<VirtualizationNode>,
    /// Per node: the last heartbeat seen and when it was seen.
    seen: Mutex<HashMap<String, (Option<String>, Instant)>>,
    /// Per node: digest of what NetworkReady depends on, to trigger VMs
    /// only when it changes.
    digests: Mutex<HashMap<String, u64>>,
}

impl NodeView {
    pub(crate) fn new(store: Store<VirtualizationNode>) -> Self {
        Self {
            store,
            seen: Mutex::new(HashMap::new()),
            digests: Mutex::new(HashMap::new()),
        }
    }

    /// Record an applied VirtualizationNode from the reflector stream.
    pub(crate) fn saw(&self, node: &VirtualizationNode) {
        let hb = heartbeat(node);
        let Ok(mut seen) = self.seen.lock() else {
            return;
        };
        let name = node.name_any();
        match seen.get(&name) {
            Some((h, _)) if *h == hb => {}
            Some(_) => {
                seen.insert(name, (hb, Instant::now()));
            }
            None => {
                // First sighting: trust a heartbeat only if the node itself
                // says it is recent.
                let age = node
                    .status
                    .as_ref()
                    .and_then(|s| s.heartbeat_time.as_ref())
                    .map(|t| Timestamp::now().as_second() - t.0.as_second());
                let at = if age.is_some_and(|a| (0..=STALE_SECONDS).contains(&a)) {
                    Instant::now()
                } else {
                    Instant::now()
                        .checked_sub(STALE + Duration::from_secs(1))
                        .unwrap_or_else(Instant::now)
                };
                seen.insert(name, (hb, at));
            }
        }
    }

    /// Whether the store holds what the trigger last saw for node `name`;
    /// false while the reflector lags the trigger watch.
    pub(crate) fn settled(&self, name: &str) -> bool {
        let Some(node) = self.store.get(&ObjectRef::new(name)) else {
            return true;
        };
        self.digests
            .lock()
            .ok()
            .and_then(|d| d.get(name).copied())
            .is_none_or(|d| d == digest(&node))
    }

    /// The node `name` and whether its report is fresh.
    pub(crate) fn get(&self, name: &str) -> (Option<std::sync::Arc<VirtualizationNode>>, bool) {
        let node = self.store.get(&ObjectRef::new(name));
        let fresh = node.is_some() && self.fresh(name, Instant::now());
        (node, fresh)
    }

    fn fresh(&self, name: &str, now: Instant) -> bool {
        self.seen
            .lock()
            .ok()
            .and_then(|s| s.get(name).map(|(_, at)| now.duration_since(*at) <= STALE))
            .unwrap_or(false)
    }

    /// Names of the nodes whose report is fresh now.
    pub(crate) fn fresh_nodes(&self) -> HashSet<String> {
        let now = Instant::now();
        self.store
            .state()
            .iter()
            .map(|n| n.name_any())
            .filter(|n| self.fresh(n, now))
            .collect()
    }

    /// Whether what NetworkReady depends on changed on `node` since the
    /// last call for it. Heartbeat-only updates do not count.
    pub(crate) fn changed(&self, node: &VirtualizationNode) -> bool {
        let d = digest(node);
        let Ok(mut digests) = self.digests.lock() else {
            return true;
        };
        digests.insert(node.name_any(), d) != Some(d)
    }
}

fn heartbeat(node: &VirtualizationNode) -> Option<String> {
    node.status
        .as_ref()
        .and_then(|s| s.heartbeat_time.as_ref())
        .map(|t| t.0.to_string())
}

/// Digest of the parts of a VirtualizationNode that NetworkReady reads,
/// other than the heartbeat.
fn digest(node: &VirtualizationNode) -> u64 {
    let mut h = DefaultHasher::new();
    if let Some(s) = node.status.as_ref() {
        if let Some(c) = conditions::find(&s.conditions, conditions::node::RUNTIME_HEALTHY) {
            (&c.status, &c.reason).hash(&mut h);
        }
        s.runtime
            .as_ref()
            .and_then(|r| r.protocol_version.as_ref())
            .hash(&mut h);
        serde_json::to_string(&s.vms)
            .unwrap_or_default()
            .hash(&mut h);
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use baryon_api::VirtualizationNodeSpec;
    use baryon_api::node::{NodeVmStatus, VirtualizationNodeStatus};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
    use kube::runtime::reflector;

    fn node(heartbeat: i64, pod: &str) -> VirtualizationNode {
        let mut n = VirtualizationNode::new("node1", VirtualizationNodeSpec {});
        n.status = Some(VirtualizationNodeStatus {
            heartbeat_time: Some(Time(Timestamp::from_second(heartbeat).unwrap())),
            vms: vec![NodeVmStatus {
                anchor_pod_uid: pod.into(),
                network: None,
            }],
            ..Default::default()
        });
        n
    }

    #[test]
    fn heartbeats_alone_do_not_trigger_vms() {
        let (store, _writer) = reflector::store();
        let view = NodeView::new(store);
        assert!(view.changed(&node(1, "a")));
        assert!(!view.changed(&node(2, "a")));
        assert!(view.changed(&node(3, "b")));
    }

    #[test]
    fn a_node_is_fresh_only_after_its_heartbeat_was_seen() {
        let (store, _writer) = reflector::store();
        let view = NodeView::new(store);
        let now = Instant::now();
        let recent = Timestamp::now().as_second() - 5;
        assert!(!view.fresh("node1", now));
        view.saw(&node(recent, "a"));
        assert!(view.fresh("node1", Instant::now()));
        assert!(!view.fresh("node1", Instant::now() + STALE + Duration::from_secs(1)));
        // A changed heartbeat is fresh whatever the node's clock says.
        view.saw(&node(1, "a"));
        assert!(view.fresh("node1", Instant::now()));
    }

    #[test]
    fn an_old_heartbeat_at_first_sighting_is_stale() {
        let (store, _writer) = reflector::store();
        let view = NodeView::new(store);
        view.saw(&node(1, "a"));
        assert!(!view.fresh("node1", Instant::now()));
    }
}
