//! VirtualizationNode: observed virtualization capabilities of one node.
//!
//! One object per Kubernetes Node, named after it and owned by it. Written
//! only by baryon-node-agent on that node. It only reports capabilities that
//! were verified on the host; an absent field means unknown.

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
    printcolumn = r#"{"name":"Arch","type":"string","jsonPath":".status.architecture"}"#,
    printcolumn = r#"{"name":"KVM","type":"boolean","jsonPath":".status.kvm.available"}"#,
    printcolumn = r#"{"name":"IOMMU","type":"boolean","jsonPath":".status.iommu.enabled"}"#,
    printcolumn = r#"{"name":"Hypervisor","type":"string","jsonPath":".status.runtime.hypervisorVersion"}"#,
    printcolumn = r#"{"name":"VMs","type":"integer","jsonPath":".status.vmCount"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct VirtualizationNodeSpec {}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, KubeSchema)]
#[serde(rename_all = "camelCase")]
pub struct VirtualizationNodeStatus {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[x_kube(merge_strategy = kube::core::ListMerge::Map(vec!["type".into()]))]
    pub conditions: Vec<Condition>,
    /// Last time the node agent refreshed this status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_time: Option<Time>,
    /// Kernel machine name, for example x86_64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_release: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_image: Option<String>,
    /// Kairos release of the host, when it is a Kairos system.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kairos_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<CpuStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kvm: Option<KvmStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iommu: Option<IommuStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vfio: Option<VfioStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub numa_nodes: Vec<NumaNodeStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hugepages: Vec<HugepageStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pci_devices: Vec<PciDeviceStatus>,
    /// VMs currently known to the host runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm_count: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStatus {
    pub virtd_version: String,
    pub protocol: String,
    pub hypervisor: String,
    pub hypervisor_version: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CpuStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub online: u32,
    /// vmx or svm on x86_64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtualization_extension: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nested_virtualization: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KvmStatus {
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_version: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IommuStatus {
    pub enabled: bool,
    pub groups: u32,
    /// /dev/iommu is present.
    pub iommufd: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VfioStatus {
    /// /dev/vfio/vfio is present.
    pub container_device: bool,
    /// The vfio-pci driver is registered.
    pub pci_driver: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NumaNodeStatus {
    pub id: u32,
    pub cpus: String,
    pub memory_bytes: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HugepageStatus {
    pub page_size_bytes: u64,
    pub total: u64,
    pub free: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PciDeviceStatus {
    pub address: String,
    pub vendor_id: String,
    pub device_id: String,
    pub class: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iommu_group: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub numa_node: Option<u32>,
}
