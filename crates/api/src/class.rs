//! VirtualMachineClass: a reusable compute shape. Immutable: to change the
//! shape of VMs, create a new class and change their `className`.

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
    printcolumn = r#"{"name":"vCPUs","type":"integer","jsonPath":".spec.cpu.vcpus"}"#,
    printcolumn = r#"{"name":"Memory","type":"string","jsonPath":".spec.memory.size"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
// A class stored before cpu.vcpus existed (cpu.cores, pruned on read) has no
// vcpus and may be updated once to the current shape.
#[x_kube(validation = kube::core::Rule::new("self == oldSelf || !has(oldSelf.cpu.vcpus)").message("VirtualMachineClass spec is immutable; create a new class and change the VMs' className"))]
pub struct VirtualMachineClassSpec {
    pub cpu: CpuSpec,
    pub memory: MemorySpec,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, KubeSchema)]
#[serde(rename_all = "camelCase")]
#[x_kube(validation = kube::core::Rule::new("has(self.vcpus)").message("cpu.vcpus is required"))]
pub struct CpuSpec {
    /// Number of virtual CPUs, 1 to 256. Says nothing about the guest's
    /// CPU topology.
    // Defaulted when reading, so a class stored in the earlier shape is
    // reported as invalid instead of failing every read; the schema still
    // requires it.
    #[serde(default)]
    #[schemars(range(min = 1, max = 256), transform = no_default)]
    pub vcpus: u32,
}

/// Remove the default schemars derives from `serde(default)`: the API server
/// would apply it, also to stored objects when they are read.
fn no_default(schema: &mut schemars::Schema) {
    if let Some(o) = schema.as_object_mut() {
        o.remove("default");
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, KubeSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemorySpec {
    /// Guest memory, for example 4Gi: a whole number of MiB from 64Mi to
    /// 16Ti.
    #[x_kube(validation = kube::core::Rule::new("isQuantity(string(self)) && quantity(string(self)).isInteger() && quantity(string(self)).compareTo(quantity('64Mi')) >= 0 && quantity(string(self)).compareTo(quantity('16Ti')) <= 0 && quantity(string(self)).asInteger() % 1048576 == 0").message("memory.size must be a whole number of Mi between 64Mi and 16Ti"))]
    pub size: Quantity,
}
