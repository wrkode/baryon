//! Baryon Kubernetes API, group `virtualization.baryon.io`, version
//! `v1alpha1`. See ADR 0008.

// The KubeSchema derive re-emits each type inside a private module to build
// its schema, which trips this lint on every public type.
#![allow(unreachable_pub)]

pub mod class;
pub mod conditions;
pub mod node;
pub mod quantity;
pub mod vm;

pub use class::{VirtualMachineClass, VirtualMachineClassSpec};
pub use node::{VirtualizationNode, VirtualizationNodeSpec, VirtualizationNodeStatus};
pub use vm::{VirtualMachine, VirtualMachineSpec, VirtualMachineStatus};

use kube::CustomResourceExt;

/// API group.
pub const GROUP: &str = "virtualization.baryon.io";

/// Labels, annotations, and resource names used by Baryon.
pub mod keys {
    /// Label on Anchor Pods and claims: the VM they belong to.
    pub const VM_NAME_LABEL: &str = "virtualization.baryon.io/vm-name";
    /// Label on Anchor Pods: the VM UID.
    pub const VM_UID_LABEL: &str = "virtualization.baryon.io/vm-uid";
    /// Label on objects created by Baryon.
    pub const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";
    pub const MANAGED_BY: &str = "baryon";
    /// Annotation on a VirtualMachine requesting a restart (any new value).
    pub const RESTARTED_AT_ANNOTATION: &str = "virtualization.baryon.io/restartedAt";
    /// Extended resource advertised by baryon-node-agent: one per VM slot on a
    /// node with a healthy runtime. Requesting it grants /dev/kvm and the
    /// baryon-virtd anchor socket to the container.
    pub const VM_RESOURCE: &str = "virtualization.baryon.io/vm";
    /// Extended resource that only places a pod on a node with a healthy
    /// runtime, without granting anything. The import Pod requests it.
    pub const IMPORT_RESOURCE: &str = "virtualization.baryon.io/import";
    /// Annotation on a root disk claim with which earlier releases recorded
    /// a completed import (`sha256:` digest of the source URL, or the URL).
    /// Read only to migrate their VMs; the record now lives on the claim's
    /// PersistentVolume (ADR 0006). Not trusted on its own: namespace users
    /// can edit claims.
    pub const IMPORTED_ANNOTATION: &str = "virtualization.baryon.io/imported-from";
    /// Finalizer held by baryon-controller on VirtualMachines.
    pub const VM_FINALIZER: &str = "virtualization.baryon.io/anchor-cleanup";
}

/// All CRDs as one multi-document YAML stream.
pub fn crds_yaml() -> Result<String, Box<dyn std::error::Error>> {
    let mut out = String::new();
    for crd in [
        VirtualMachine::crd(),
        VirtualMachineClass::crd(),
        VirtualizationNode::crd(),
    ] {
        let mut value = serde_json::to_value(&crd)?;
        strip_numeric_formats(&mut value);
        out.push_str("---\n");
        out.push_str(&serde_saphyr::to_string(&value)?);
    }
    Ok(out)
}

/// schemars annotates integers with formats such as `uint32` that the
/// Kubernetes API server does not recognize and warns about on every apply.
/// The type and value constraints are unaffected by removing them.
fn strip_numeric_formats(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            let numeric = matches!(
                map.get("format").and_then(|f| f.as_str()),
                Some(
                    "int32"
                        | "int64"
                        | "uint8"
                        | "uint16"
                        | "uint32"
                        | "uint64"
                        | "float"
                        | "double"
                )
            );
            if numeric {
                map.remove("format");
            }
            map.values_mut().for_each(strip_numeric_formats);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(strip_numeric_formats),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crds_carry_validation_and_list_maps() {
        let vm = serde_json::to_string(&VirtualMachine::crd()).unwrap();
        assert!(vm.contains("exactly one of http or persistentVolumeClaim must be set"));
        assert!(vm.contains("size is required when the source is http"));
        assert!(vm.contains(r#""x-kubernetes-list-type":"map""#));
        assert!(vm.contains(r#""shortNames":["bvm"]"#));
        assert!(vm.contains(r#""status":{}"#), "status subresource");
        let node = serde_json::to_string(&VirtualizationNode::crd()).unwrap();
        assert!(node.contains(r#""scope":"Cluster""#));
        let class = serde_json::to_string(&VirtualMachineClass::crd()).unwrap();
        assert!(class.contains(r#""scope":"Cluster""#));
        assert!(class.contains("cores must be between 1 and 256"));
        assert!(
            crds_yaml()
                .unwrap()
                .matches("kind: CustomResourceDefinition")
                .count()
                == 3
        );
    }

    #[test]
    fn spec_defaults_and_round_trip() {
        let vm: VirtualMachineSpec = serde_json::from_value(serde_json::json!({
            "className": "small",
            "rootDisk": {"source": {"http": {"url": "https://example.org/disk.raw"}}, "size": "10Gi"}
        }))
        .unwrap();
        assert_eq!(vm.run_policy, vm::RunPolicy::Running);
        assert_eq!(vm.networks.len(), 1);
        assert!(vm.networks[0].pod.is_some());
        let back = serde_json::to_value(&vm).unwrap();
        assert_eq!(back["runPolicy"], "Running");
        assert_eq!(back["rootDisk"]["size"], "10Gi");
    }
}
