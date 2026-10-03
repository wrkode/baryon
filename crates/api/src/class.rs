//! VirtualMachineClass: a reusable compute shape.

use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::{CustomResource, KubeSchema};
use serde::{Deserialize, Serialize};

/// vCPUs and memory shared by VMs that reference this class.
#[derive(CustomResource, KubeSchema, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[kube(
    group = "virtualization.baryon.io",
    version = "v1alpha1",
    kind = "VirtualMachineClass",
    plural = "virtualmachineclasses",
    singular = "virtualmachineclass",
    shortname = "bvmclass",
    category = "baryon",
    printcolumn = r#"{"name":"Cores","type":"integer","jsonPath":".spec.cpu.cores"}"#,
    printcolumn = r#"{"name":"Memory","type":"string","jsonPath":".spec.memory.size"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct VirtualMachineClassSpec {
    pub cpu: CpuSpec,
    pub memory: MemorySpec,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, KubeSchema)]
#[serde(rename_all = "camelCase")]
pub struct CpuSpec {
    /// Number of vCPUs.
    #[x_kube(validation = kube::core::Rule::new("self >= 1 && self <= 256").message("cores must be between 1 and 256"))]
    pub cores: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemorySpec {
    /// Guest memory, for example 4Gi. Must be a whole number of MiB and at
    /// least 64Mi.
    pub size: Quantity,
}
