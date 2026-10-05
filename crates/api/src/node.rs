//! VirtualizationNode: whether one node can run Baryon VMs, its runtime
//! versions, and its runtime health.
//!
//! One object per Kubernetes Node, named after it and owned by it. Written
//! only by baryon-node-agent on that node. Host inventory (CPU, memory,
//! devices) is not published here: Kubernetes already publishes what the
//! scheduler uses on the Node, and the node agent logs the rest.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::{CustomResource, KubeSchema};
use serde::{Deserialize, Serialize};

/// VirtualizationNode has no desired state in v1alpha1.
#[derive(CustomResource, KubeSchema, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[kube(
    group = "virtualization.baryon.io",
    version = "v1alpha1",
    kind = "VirtualizationNode",
    plural = "virtualizationnodes",
    singular = "virtualizationnode",
    status = "VirtualizationNodeStatus",
    shortname = "vnode",
    category = "baryon",
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
    printcolumn = r#"{"name":"Reason","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].reason"}"#,
    printcolumn = r#"{"name":"VMs","type":"integer","jsonPath":".status.vmCount"}"#,
    printcolumn = r#"{"name":"Runtime","type":"string","jsonPath":".status.runtime.virtdVersion"}"#,
    printcolumn = r#"{"name":"Hypervisor","type":"string","jsonPath":".status.runtime.hypervisorVersion"}"#,
    printcolumn = r#"{"name":"Heartbeat","type":"date","jsonPath":".status.heartbeatTime"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct VirtualizationNodeSpec {}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, KubeSchema)]
#[serde(rename_all = "camelCase")]
pub struct VirtualizationNodeStatus {
    /// `Ready` and `RuntimeHealthy`. Valid only while `heartbeatTime` is
    /// recent (see docs/api.md): a node agent that stops writes them
    /// Unknown, but one that crashes leaves them as they were.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[x_kube(merge_strategy = kube::core::ListMerge::Map(vec!["type".into()]))]
    pub conditions: Vec<Condition>,
    /// Last time the node agent refreshed this status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_time: Option<Time>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeStatus>,
    /// VMs currently known to the host runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm_count: Option<u32>,
    /// Host-side state of each VM on the node, by Anchor Pod UID, read by
    /// baryon-controller for the VM's `NetworkReady` condition. Internal:
    /// users read `NetworkReady` on the VM. Carries no VM names,
    /// namespaces, or addresses. Entries have no `network` when baryon-virtd
    /// does not report it (runtime protocol before 1.2). At most
    /// [`MAX_NODE_VMS`] entries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[x_kube(merge_strategy = kube::core::ListMerge::Map(vec!["anchorPodUID".into()]))]
    #[schemars(length(max = 256))]
    pub vms: Vec<NodeVmStatus>,
}

/// Most entries in `status.vms`.
pub const MAX_NODE_VMS: usize = 256;

/// Host-side state of one VM.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeVmStatus {
    /// UID of the VM's Anchor Pod.
    #[serde(rename = "anchorPodUID")]
    pub anchor_pod_uid: String,
    /// The guest network service on the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NodeVmNetwork>,
}

/// State of the host-side guest network service (the DHCP responder).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NodeVmNetwork {
    /// Ready, Restarting, or Failing.
    #[schemars(extend("enum" = ["Ready", "Restarting", "Failing"]))]
    pub state: String,
    #[schemars(length(max = 64))]
    pub reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    #[schemars(length(max = 1024))]
    pub message: String,
    /// When the state last changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::Time>,
    /// When the guest's lease expires without a renewal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_time: Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::Time>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, KubeSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStatus {
    /// Version of baryon-virtd on the node.
    pub virtd_version: String,
    /// Git commit baryon-virtd was built from; `-dirty` marks a build from
    /// a tree with uncommitted changes, `unknown` a build without the
    /// information.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtd_git_commit: Option<String>,
    /// Version of Cloud Hypervisor on the node.
    pub hypervisor_version: String,
    /// Runtime protocol version of baryon-virtd, `<major>.<minor>`; 1.0 for
    /// a baryon-virtd that does not report one. Internal: compatibility is
    /// reported by the `RuntimeHealthy` condition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<String>,
    /// Runtime protocol features baryon-virtd implements. Internal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[x_kube(merge_strategy = kube::core::ListMerge::Set)]
    pub features: Vec<String>,
}
