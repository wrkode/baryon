//! VirtualMachine: user intent for one VM.

use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::{CustomResource, KubeSchema};
use serde::{Deserialize, Serialize};

/// Desired state of a VirtualMachine.
#[derive(CustomResource, KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[kube(
    group = "virtualization.baryon.io",
    version = "v1alpha1",
    kind = "VirtualMachine",
    plural = "virtualmachines",
    singular = "virtualmachine",
    namespaced,
    status = "VirtualMachineStatus",
    shortname = "bvm",
    category = "baryon",
    // The name is also the guest hostname and a label value on the objects
    // Baryon creates, so it must be a DNS-1123 label.
    validation = kube::core::Rule::new("self.metadata.name.size() <= 63 && !self.metadata.name.contains('.')").message("VirtualMachine names must be DNS-1123 labels: at most 63 characters, no dots"),
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].status"}"#,
    printcolumn = r#"{"name":"Reason","type":"string","jsonPath":".status.conditions[?(@.type==\"Ready\")].reason"}"#,
    printcolumn = r#"{"name":"Node","type":"string","jsonPath":".status.nodeName"}"#,
    printcolumn = r#"{"name":"Address","type":"string","jsonPath":".status.addresses[0].address"}"#,
    printcolumn = r#"{"name":"Class","type":"string","jsonPath":".spec.className","priority":1}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct VirtualMachineSpec {
    /// Name of the VirtualMachineClass that sets vCPUs and memory.
    #[x_kube(validation = kube::core::Rule::new("self.size() > 0").message("className is required"))]
    pub class_name: String,

    /// Whether the VM should be running. Changing it starts or stops the VM.
    #[serde(default)]
    pub run_policy: RunPolicy,

    /// The disk the VM boots from. Immutable: the disk's content and claim
    /// are bound to it; create a new VirtualMachine for another disk.
    #[x_kube(validation = kube::core::Rule::new("self == oldSelf").message("rootDisk is immutable; create a new VirtualMachine to use another disk"))]
    pub root_disk: RootDisk,

    /// Networks of the VM. v1alpha1 supports exactly one network, the pod
    /// network of the VM's Anchor Pod. Defaults to that network.
    #[serde(default = "default_networks")]
    #[x_kube(
        validation = kube::core::Rule::new("self.size() == 1 && has(self[0].pod)").message("exactly one network with source pod is supported"),
        merge_strategy = kube::core::ListMerge::Map(vec!["name".into()])
    )]
    #[schemars(length(max = 8))]
    pub networks: Vec<Network>,

    /// Kubernetes scheduling constraints for the VM. Baryon copies them to
    /// the pods that run and import the VM; the Kubernetes scheduler places
    /// them. Changing them takes effect at the next start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<Placement>,
}

/// Kubernetes scheduling constraints, with the meaning they have on a pod.
/// Baryon adds its own requirements (a node that runs the Baryon runtime,
/// nodes to avoid) to them.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, KubeSchema)]
#[serde(rename_all = "camelCase")]
pub struct Placement {
    /// Node labels the node must have.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub node_selector: std::collections::BTreeMap<String, String>,
    /// Node affinity and pod (anti-)affinity. Pod affinity terms see the
    /// VM's labels, which Baryon copies to the VM's pods. Pod affinity terms
    /// apply within the VM's namespace only: `namespaces` and
    /// `namespaceSelector` are refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[x_kube(validation = kube::core::Rule::new("!has(self.podAffinity) || ((!has(self.podAffinity.requiredDuringSchedulingIgnoredDuringExecution) || self.podAffinity.requiredDuringSchedulingIgnoredDuringExecution.all(t, !has(t.namespaces) && !has(t.namespaceSelector))) && (!has(self.podAffinity.preferredDuringSchedulingIgnoredDuringExecution) || self.podAffinity.preferredDuringSchedulingIgnoredDuringExecution.all(w, !has(w.podAffinityTerm.namespaces) && !has(w.podAffinityTerm.namespaceSelector))))").message("pod affinity terms may not set namespaces or namespaceSelector; they apply within the VM's namespace"))]
    #[x_kube(validation = kube::core::Rule::new("!has(self.podAntiAffinity) || ((!has(self.podAntiAffinity.requiredDuringSchedulingIgnoredDuringExecution) || self.podAntiAffinity.requiredDuringSchedulingIgnoredDuringExecution.all(t, !has(t.namespaces) && !has(t.namespaceSelector))) && (!has(self.podAntiAffinity.preferredDuringSchedulingIgnoredDuringExecution) || self.podAntiAffinity.preferredDuringSchedulingIgnoredDuringExecution.all(w, !has(w.podAffinityTerm.namespaces) && !has(w.podAffinityTerm.namespaceSelector))))").message("pod affinity terms may not set namespaces or namespaceSelector; they apply within the VM's namespace"))]
    pub affinity: Option<k8s_openapi::api::core::v1::Affinity>,
    /// Tolerations of node taints. A toleration of every taint, of a cordon
    /// (node.kubernetes.io/unschedulable), of control-plane nodes, or of a
    /// node shut down out of service is refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[x_kube(validation = kube::core::Rule::new("self.all(t, has(t.key) && t.key != '' && !(t.key in ['node.kubernetes.io/unschedulable', 'node-role.kubernetes.io/control-plane', 'node.kubernetes.io/out-of-service']))").message("tolerations must name a taint key and must not tolerate node.kubernetes.io/unschedulable, node-role.kubernetes.io/control-plane, or node.kubernetes.io/out-of-service"))]
    #[schemars(length(max = 32))]
    pub tolerations: Vec<k8s_openapi::api::core::v1::Toleration>,
    /// How VMs spread across topology domains; label selectors see the VM's
    /// labels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 8))]
    pub topology_spread_constraints: Vec<k8s_openapi::api::core::v1::TopologySpreadConstraint>,
    /// PriorityClass of the VM's pods. A preempted Anchor Pod stops its VM.
    /// The system classes (system-*) are refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[x_kube(validation = kube::core::Rule::new("!self.startsWith('system-')").message("system PriorityClasses are not allowed for VMs"))]
    #[schemars(length(max = 253))]
    pub priority_class_name: Option<String>,
}

fn default_networks() -> Vec<Network> {
    vec![Network {
        name: "default".to_owned(),
        pod: Some(PodNetwork {}),
    }]
}

/// Desired run state.
#[derive(
    Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, schemars::JsonSchema,
)]
pub enum RunPolicy {
    /// The VM runs. If the guest powers off or the VMM fails, it is started
    /// again.
    #[default]
    Running,
    /// The VM is stopped. Its disk is kept.
    Stopped,
}

/// Volume mode of a claim Baryon creates.
#[derive(
    Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, schemars::JsonSchema,
)]
pub enum VolumeMode {
    /// A filesystem volume; the disk is an image file on it.
    #[default]
    Filesystem,
    /// A raw block volume; the disk is the device.
    Block,
}

/// The boot disk.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, KubeSchema)]
#[serde(rename_all = "camelCase")]
#[x_kube(validation = kube::core::Rule::new("!has(self.source.http) || has(self.size)").message("size is required when the source is http"))]
#[x_kube(validation = kube::core::Rule::new("!has(self.volumeMode) || has(self.source.http)").message("volumeMode applies to claims Baryon creates; an existing claim is used with its own volumeMode"))]
pub struct RootDisk {
    pub source: DiskSource,
    /// Virtual disk size. Required for imported disks; the claim requests
    /// this size and the image is extended to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<Quantity>,
    /// StorageClass for claims created by Baryon. Defaults to the cluster
    /// default StorageClass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_class_name: Option<String>,
    /// Volume mode of the claim Baryon creates for an imported disk.
    /// Defaults to Filesystem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_mode: Option<VolumeMode>,
}

/// Where the disk content comes from. Exactly one field must be set.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, KubeSchema)]
#[serde(rename_all = "camelCase")]
#[x_kube(validation = kube::core::Rule::new("(has(self.http) ? 1 : 0) + (has(self.persistentVolumeClaim) ? 1 : 0) == 1").message("exactly one of http or persistentVolumeClaim must be set"))]
pub struct DiskSource {
    /// Import a raw disk image from an HTTP or HTTPS URL into a filesystem
    /// mode claim created and owned by the VM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpSource>,
    /// Use an existing claim that already contains a raw disk. In filesystem
    /// mode the image must be `disk.img` at the root of the volume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistent_volume_claim: Option<ClaimSource>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, KubeSchema)]
#[serde(rename_all = "camelCase")]
#[x_kube(validation = kube::core::Rule::new("self.url.startsWith('https://') || has(self.sha256)").message("sha256 is required when the url is plain http"))]
pub struct HttpSource {
    #[x_kube(validation = kube::core::Rule::new("self.startsWith('https://') || self.startsWith('http://')").message("url must be http or https"))]
    #[schemars(length(max = 2048))]
    pub url: String,
    /// Expected SHA-256 of the image, hex encoded. Required for plain HTTP;
    /// strongly recommended for HTTPS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[x_kube(validation = kube::core::Rule::new("self.matches('^[0-9a-fA-F]{64}$')").message("sha256 must be 64 hex characters"))]
    #[schemars(length(min = 64, max = 64))]
    pub sha256: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClaimSource {
    pub claim_name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, KubeSchema)]
#[serde(rename_all = "camelCase")]
pub struct Network {
    /// Name of the network within the VM, a DNS-1123 label.
    #[x_kube(validation = kube::core::Rule::new("self.matches('^[a-z0-9]([-a-z0-9]*[a-z0-9])?$')").message("name must be a DNS-1123 label"))]
    #[schemars(length(min = 1, max = 63))]
    pub name: String,
    /// The pod network of the VM's Anchor Pod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod: Option<PodNetwork>,
}

/// The pod network. The guest takes over the pod's IP address.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, schemars::JsonSchema)]
pub struct PodNetwork {}

/// Observed state of a VirtualMachine. Written only by baryon-controller.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, KubeSchema)]
#[serde(rename_all = "camelCase")]
pub struct VirtualMachineStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[x_kube(merge_strategy = kube::core::ListMerge::Map(vec!["type".into()]))]
    pub conditions: Vec<Condition>,
    /// Node the VM runs on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,
    /// Name of the current Anchor Pod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_pod: Option<String>,
    /// Guest addresses. On the pod network these are the Anchor Pod IPs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<VmAddress>,
    /// MAC address of the guest NIC. Stable for the life of the VM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac_address: Option<String>,
    /// Claim holding the root disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_disk_claim: Option<String>,
    /// Set when the root disk claim held an imported or used disk. It only
    /// prevents another import into the same claim; the record on the
    /// claim's PersistentVolume is the authority (ADR 0006).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_disk_import: Option<RootDiskImportStatus>,
    /// Number of times the VM was restarted in the current Anchor Pod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_count: Option<i32>,
    /// Nodes where this VM cannot run because another VM there runs with
    /// the same host identity. New Anchor Pods avoid them. Cleared when the
    /// VM is stopped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 16))]
    pub identity_conflict_nodes: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VmAddress {
    /// InternalIP for pod network addresses.
    #[serde(rename = "type")]
    pub type_: String,
    pub address: String,
}

/// A completed import into the root disk claim.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RootDiskImportStatus {
    /// UID of the claim the image was imported into.
    #[serde(rename = "claimUID")]
    pub claim_uid: String,
}
