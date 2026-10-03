//! Construction of the VM Anchor Pod (ADR 0004, ADR 0005).
//!
//! The pod is the VM's Kubernetes envelope: scheduling, resources, the root
//! disk claim, and the pod network. It runs one unprivileged container,
//! baryon-anchor, and requests one `virtualization.baryon.io/vm` slot, which
//! grants /dev/kvm and the baryon-virtd anchor socket through the device
//! plugin. It needs no hostPath, no capabilities, and no API credentials, and
//! satisfies the Pod Security "restricted" level.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    Affinity, Capabilities, Container, EnvVar, ExecAction, NodeAffinity, NodeSelector,
    NodeSelectorRequirement, NodeSelectorTerm, PersistentVolumeClaimVolumeSource, Pod,
    PodSecurityContext, PodSpec, Probe, ResourceRequirements, SeccompProfile, SecurityContext,
    Volume, VolumeDevice, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};

use baryon_api::keys;
use baryon_runtime_api::anchor_spec::{
    AnchorDisk, AnchorSpec, DiskMode, ROOT_DISK_DEVICE, ROOT_DISK_DIR, SPEC_ENV,
};

use crate::resources;
use baryon_runtime_api::identity::VmIdentity;

/// Annotation on the Anchor Pod with the hash of its inputs.
pub(crate) const SPEC_HASH_ANNOTATION: &str = "virtualization.baryon.io/spec-hash";
/// Name of the anchor container in the Anchor Pod.
pub(crate) const ANCHOR_CONTAINER: &str = "anchor";

/// Everything needed to build an Anchor Pod.
#[derive(Debug, Clone)]
pub(crate) struct AnchorInput {
    pub vm_name: String,
    pub vm_namespace: String,
    pub vm_uid: String,
    pub owner: OwnerReference,
    pub vcpus: u32,
    pub guest_memory_bytes: u64,
    pub claim_name: String,
    pub disk_mode: DiskMode,
    /// Image import for a new claim: URL, optional SHA-256, disk size.
    pub import: Option<(String, Option<String>, u64)>,
    pub anchor_image: String,
    pub restarted_at: Option<String>,
    /// Nodes the Anchor Pod must not be scheduled to (host identity
    /// conflicts).
    pub avoid_nodes: Vec<String>,
}

impl AnchorInput {
    /// Hash of what the VM's spec and class determine. The anchor image is
    /// not part of it: a newer runtime is reported separately.
    /// Whether an import is still pending is not part of it: it changes when
    /// the import completes, which is not a spec change.
    pub(crate) fn spec_hash(&self) -> String {
        resources::spec_hash(&[
            &self.vcpus.to_string(),
            &self.guest_memory_bytes.to_string(),
            &self.claim_name,
            &format!("{:?}", self.disk_mode),
            self.restarted_at.as_deref().unwrap_or(""),
        ])
    }

    fn anchor_spec(&self) -> AnchorSpec {
        AnchorSpec {
            vm_uid: self.vm_uid.clone(),
            vm_namespace: self.vm_namespace.clone(),
            vm_name: self.vm_name.clone(),
            vcpus: self.vcpus,
            memory_bytes: self.guest_memory_bytes,
            root_disk: AnchorDisk {
                name: "root".to_owned(),
                mode: self.disk_mode,
                read_only: false,
            },
            pod_interface: "eth0".to_owned(),
            stop_grace_seconds: resources::STOP_GRACE_SECONDS,
        }
    }
}

fn q(s: impl Into<String>) -> Quantity {
    Quantity(s.into())
}

fn restricted_container_security() -> SecurityContext {
    SecurityContext {
        allow_privilege_escalation: Some(false),
        read_only_root_filesystem: Some(true),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".to_owned()]),
            add: None,
        }),
        ..Default::default()
    }
}

/// Required node affinity that keeps the pod off `nodes` (matched by node
/// name, not by label, so it holds whatever labels the nodes carry).
fn avoid_nodes_affinity(nodes: &[String]) -> Option<Affinity> {
    if nodes.is_empty() {
        return None;
    }
    Some(Affinity {
        node_affinity: Some(NodeAffinity {
            required_during_scheduling_ignored_during_execution: Some(NodeSelector {
                node_selector_terms: vec![NodeSelectorTerm {
                    match_fields: Some(vec![NodeSelectorRequirement {
                        key: "metadata.name".to_owned(),
                        operator: "NotIn".to_owned(),
                        values: Some(nodes.to_vec()),
                    }]),
                    ..Default::default()
                }],
            }),
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// Errors building an Anchor Pod.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BuildError {
    #[error("encoding the anchor spec: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("VM identity: {0}")]
    Identity(#[from] baryon_runtime_api::identity::IdentityError),
}

/// Build the Anchor Pod.
pub(crate) fn build(input: &AnchorInput) -> Result<Pod, BuildError> {
    // The Anchor container, and with it the VMM, runs as the VM's identity.
    // The pod-level fsGroup gives filesystem volumes that support ownership
    // management to the same group.
    let identity = VmIdentity::for_vm(&input.vm_uid)?;
    let uid = i64::from(identity.uid());
    let gid = i64::from(identity.gid());
    let memory = resources::pod_memory_bytes(input.guest_memory_bytes).to_string();
    let cpu = input.vcpus.to_string();

    let mut labels = BTreeMap::new();
    labels.insert(keys::VM_NAME_LABEL.to_owned(), input.vm_name.clone());
    labels.insert(keys::VM_UID_LABEL.to_owned(), input.vm_uid.clone());
    labels.insert(
        keys::MANAGED_BY_LABEL.to_owned(),
        keys::MANAGED_BY.to_owned(),
    );

    let mut annotations = BTreeMap::new();
    annotations.insert(SPEC_HASH_ANNOTATION.to_owned(), input.spec_hash());
    if let Some(r) = &input.restarted_at {
        annotations.insert(keys::RESTARTED_AT_ANNOTATION.to_owned(), r.clone());
    }

    let resources = ResourceRequirements {
        requests: Some(BTreeMap::from([
            ("cpu".to_owned(), q(cpu.clone())),
            ("memory".to_owned(), q(memory.clone())),
            (keys::VM_RESOURCE.to_owned(), q("1")),
        ])),
        limits: Some(BTreeMap::from([
            ("cpu".to_owned(), q(cpu)),
            ("memory".to_owned(), q(memory)),
            (keys::VM_RESOURCE.to_owned(), q("1")),
        ])),
        ..Default::default()
    };

    let (disk_mounts, disk_devices) = match input.disk_mode {
        DiskMode::Filesystem => (
            vec![VolumeMount {
                name: "root-disk".to_owned(),
                mount_path: ROOT_DISK_DIR.to_owned(),
                ..Default::default()
            }],
            None,
        ),
        DiskMode::Block => (
            Vec::new(),
            Some(vec![VolumeDevice {
                name: "root-disk".to_owned(),
                device_path: ROOT_DISK_DEVICE.to_owned(),
            }]),
        ),
    };

    let anchor_mounts = disk_mounts.clone();

    let anchor = Container {
        name: ANCHOR_CONTAINER.to_owned(),
        image: Some(input.anchor_image.clone()),
        args: Some(vec!["run".to_owned()]),
        env: Some(vec![EnvVar {
            name: SPEC_ENV.to_owned(),
            value: Some(input.anchor_spec().to_json()?),
            value_from: None,
        }]),
        resources: Some(resources),
        readiness_probe: Some(Probe {
            exec: Some(ExecAction {
                command: Some(vec!["/baryon-anchor".to_owned(), "probe".to_owned()]),
            }),
            period_seconds: Some(2),
            ..Default::default()
        }),
        volume_mounts: Some(anchor_mounts),
        volume_devices: disk_devices.clone(),
        security_context: Some(restricted_container_security()),
        termination_message_policy: Some("File".to_owned()),
        ..Default::default()
    };

    let init_containers = input.import.as_ref().map(|(url, sha, size)| {
        let mut args = vec![
            "import".to_owned(),
            format!("--url={url}"),
            format!("--size-bytes={size}"),
        ];
        if let Some(s) = sha {
            args.push(format!("--sha256={s}"));
        }
        if input.disk_mode == DiskMode::Block {
            args.push(format!("--device={ROOT_DISK_DEVICE}"));
        }
        vec![Container {
            name: "import".to_owned(),
            image: Some(input.anchor_image.clone()),
            args: Some(args),
            // Every container needs limits, or the pod has no pod-level memory
            // limit and the VMM would run unbounded. Init container requests
            // do not add to the pod's scheduled resources when they are below
            // the main container's.
            resources: Some(ResourceRequirements {
                requests: Some(BTreeMap::from([
                    ("cpu".to_owned(), q("1")),
                    ("memory".to_owned(), q("256Mi")),
                ])),
                limits: Some(BTreeMap::from([
                    ("cpu".to_owned(), q("1")),
                    ("memory".to_owned(), q("256Mi")),
                ])),
                ..Default::default()
            }),
            volume_mounts: Some(disk_mounts.clone()),
            volume_devices: disk_devices.clone(),
            security_context: Some(restricted_container_security()),
            termination_message_policy: Some("File".to_owned()),
            ..Default::default()
        }]
    });

    let mut volumes = vec![Volume {
        name: "root-disk".to_owned(),
        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
            claim_name: input.claim_name.clone(),
            read_only: None,
        }),
        ..Default::default()
    }];
    volumes.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Pod {
        metadata: ObjectMeta {
            name: Some(resources::anchor_pod_name(&input.vm_name)),
            namespace: Some(input.vm_namespace.clone()),
            labels: Some(labels),
            annotations: Some(annotations),
            owner_references: Some(vec![input.owner.clone()]),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![anchor],
            init_containers,
            volumes: Some(volumes),
            affinity: avoid_nodes_affinity(&input.avoid_nodes),
            restart_policy: Some("Always".to_owned()),
            termination_grace_period_seconds: Some(i64::from(
                resources::STOP_GRACE_SECONDS + resources::TERMINATION_MARGIN_SECONDS,
            )),
            automount_service_account_token: Some(false),
            enable_service_links: Some(false),
            security_context: Some(PodSecurityContext {
                run_as_non_root: Some(true),
                run_as_user: Some(uid),
                run_as_group: Some(gid),
                fs_group: Some(gid),
                seccomp_profile: Some(SeccompProfile {
                    type_: "RuntimeDefault".to_owned(),
                    localhost_profile: None,
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        status: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(import: bool) -> AnchorInput {
        AnchorInput {
            vm_name: "web".into(),
            vm_namespace: "default".into(),
            vm_uid: "0b9d3a5e-2b9b-4f4e-8d0a-6c1f2e7a9b01".into(),
            owner: OwnerReference {
                api_version: "virtualization.baryon.io/v1alpha1".into(),
                kind: "VirtualMachine".into(),
                name: "web".into(),
                uid: "0b9d3a5e-2b9b-4f4e-8d0a-6c1f2e7a9b01".into(),
                controller: Some(true),
                block_owner_deletion: Some(true),
            },
            vcpus: 2,
            guest_memory_bytes: 2 << 30,
            claim_name: "web-root".into(),
            disk_mode: DiskMode::Filesystem,
            import: import.then(|| ("https://example.org/d.raw".into(), None, 10 << 30)),
            anchor_image: "baryon/anchor:dev".into(),
            restarted_at: None,
            avoid_nodes: Vec::new(),
        }
    }

    #[test]
    fn pod_is_guaranteed_restricted_and_has_no_host_access() {
        let pod = build(&input(true)).unwrap();
        let spec = pod.spec.as_ref().unwrap();
        let json = serde_json::to_string(&pod).unwrap();
        assert!(!json.contains("hostPath"));
        assert!(!json.contains("privileged"));
        assert_eq!(spec.automount_service_account_token, Some(false));
        for c in spec
            .containers
            .iter()
            .chain(spec.init_containers.iter().flatten())
        {
            let r = c.resources.as_ref().unwrap();
            assert_eq!(
                r.requests, r.limits,
                "container {} must be Guaranteed",
                c.name
            );
            let sc = c.security_context.as_ref().unwrap();
            assert_eq!(sc.allow_privilege_escalation, Some(false));
            assert_eq!(
                sc.capabilities.as_ref().unwrap().drop.as_ref().unwrap(),
                &["ALL"]
            );
        }
        let anchor = &spec.containers[0];
        let limits = anchor.resources.as_ref().unwrap().limits.as_ref().unwrap();
        assert_eq!(limits["virtualization.baryon.io/vm"].0, "1");
        assert_eq!(limits["cpu"].0, "2");
        assert_eq!(
            limits["memory"].0,
            resources::pod_memory_bytes(2 << 30).to_string()
        );
        let sc = spec.security_context.as_ref().unwrap();
        assert_eq!(sc.run_as_non_root, Some(true));
        let id = VmIdentity::for_vm(&input(true).vm_uid).unwrap();
        assert_eq!(sc.run_as_user, Some(i64::from(id.uid())));
        assert_eq!(sc.run_as_group, Some(i64::from(id.gid())));
        assert_eq!(sc.fs_group, Some(i64::from(id.gid())));
        assert_eq!(pod.metadata.name.as_deref(), Some("web-anchor"));
        assert!(spec.affinity.is_none());
    }

    #[test]
    fn avoids_identity_conflict_nodes() {
        let mut i = input(false);
        i.avoid_nodes = vec!["node2".into()];
        let pod = build(&i).unwrap();
        let term = &pod
            .spec
            .unwrap()
            .affinity
            .unwrap()
            .node_affinity
            .unwrap()
            .required_during_scheduling_ignored_during_execution
            .unwrap()
            .node_selector_terms[0];
        let req = &term.match_fields.as_ref().unwrap()[0];
        assert_eq!(req.key, "metadata.name");
        assert_eq!(req.operator, "NotIn");
        assert_eq!(req.values.as_deref(), Some(&["node2".to_owned()][..]));
    }

    #[test]
    fn import_only_for_http_sources_and_block_mode_uses_devices() {
        assert!(
            build(&input(false))
                .unwrap()
                .spec
                .unwrap()
                .init_containers
                .is_none()
        );
        let mut i = input(false);
        i.disk_mode = DiskMode::Block;
        let pod = build(&i).unwrap();
        let c = &pod.spec.unwrap().containers[0];
        assert_eq!(
            c.volume_devices.as_ref().unwrap()[0].device_path,
            "/dev/baryon/root"
        );
        assert!(
            c.volume_mounts
                .as_ref()
                .unwrap()
                .iter()
                .all(|m| m.name != "root-disk")
        );
    }

    #[test]
    fn block_import_writes_the_device() {
        let mut i = input(true);
        i.disk_mode = DiskMode::Block;
        let pod = build(&i).unwrap();
        let init = &pod.spec.unwrap().init_containers.unwrap()[0];
        assert!(
            init.args
                .as_ref()
                .unwrap()
                .contains(&format!("--device={ROOT_DISK_DEVICE}"))
        );
        assert_eq!(
            init.volume_devices.as_ref().unwrap()[0].device_path,
            ROOT_DISK_DEVICE
        );
        assert!(init.volume_mounts.as_ref().unwrap().is_empty());
    }

    #[test]
    fn hash_ignores_a_pending_import() {
        // The import completing must not look like a spec change.
        assert_eq!(input(true).spec_hash(), input(false).spec_hash());
    }

    #[test]
    fn spec_env_round_trips() {
        let pod = build(&input(true)).unwrap();
        let env = &pod.spec.unwrap().containers[0].env.clone().unwrap()[0];
        let spec = AnchorSpec::from_json(env.value.as_ref().unwrap()).unwrap();
        assert_eq!(spec.vcpus, 2);
        assert_eq!(spec.root_disk.mode, DiskMode::Filesystem);
    }

    #[test]
    fn hash_changes_with_restart() {
        let a = input(true).spec_hash();
        let mut i = input(true);
        i.restarted_at = Some("2026-10-03T00:00:00Z".into());
        assert_ne!(a, i.spec_hash());
    }
}
