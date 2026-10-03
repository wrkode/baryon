//! Construction of the VM Anchor Pod (ADR 0004, ADR 0005).
//!
//! The pod is the VM's Kubernetes envelope: scheduling, resources, the root
//! disk claim, and the pod network. It runs one unprivileged container,
//! baryon-anchor, and requests one `virtualization.baryon.io/vm` slot, which
//! grants /dev/kvm and the baryon-virtd anchor socket through the device
//! plugin. It needs no hostPath, no capabilities, and no API credentials, and
//! satisfies the Pod Security "restricted" level.
//!
//! An imported disk is written by a separate import Pod before the first
//! Anchor Pod exists (ADR 0006), so a guest never runs on a disk whose
//! import baryon-controller has not recorded.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    Affinity, Capabilities, Container, DownwardAPIVolumeFile, DownwardAPIVolumeSource, EnvVar,
    ExecAction, NodeAffinity, NodeSelector, NodeSelectorRequirement, NodeSelectorTerm,
    ObjectFieldSelector, PersistentVolumeClaimVolumeSource, Pod, PodSecurityContext, PodSpec,
    Probe, ResourceRequirements, SeccompProfile, SecurityContext, Volume, VolumeDevice,
    VolumeMount,
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
/// Name of the importer container, in the import Pod and in Anchor Pods
/// created by earlier releases (as an init container).
pub(crate) const IMPORT_CONTAINER: &str = "import";
/// Annotation on the import Pod with the source it imports, as
/// [`source_digest`].
pub(crate) const IMPORT_SOURCE_ANNOTATION: &str = "virtualization.baryon.io/import-source";

/// `sha256:<hex>` of an image URL: identifies the source in annotations
/// without copying the URL, which may carry a token.
pub(crate) fn source_digest(url: &str) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(url.as_bytes());
    let hex: String = d.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}
/// Annotation on the import Pod, set by baryon-controller to the claim UID
/// once the claim is bound to a volume provisioned for it. The importer
/// reads it through a downward API file and writes nothing before it is
/// set.
pub(crate) const IMPORT_APPROVED_ANNOTATION: &str = "virtualization.baryon.io/import-approved";
/// Where the importer reads the approval.
const APPROVAL_DIR: &str = "/etc/baryon/import";
/// Label on the import Pod with the VM's UID.
pub(crate) const IMPORT_FOR_LABEL: &str = "virtualization.baryon.io/import-for";

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
    /// Image import into a new claim: URL, optional SHA-256, disk size.
    /// Used by the import Pod only.
    pub import: Option<(String, Option<String>, u64)>,
    /// UID of the root disk claim. The import Pod carries it in the
    /// importer's arguments, so completion is tied to this claim.
    pub claim_uid: String,
    /// The claim UID, when the claim is bound to a volume provisioned for
    /// it, so the import Pod is created already approved. Used by the
    /// import Pod only.
    pub import_approval: Option<String>,
    pub anchor_image: String,
    pub restarted_at: Option<String>,
    /// Nodes the Anchor Pod must not be scheduled to (host identity
    /// conflicts).
    pub avoid_nodes: Vec<String>,
}

impl AnchorInput {
    /// Hash of what the VM's spec and class determine. The anchor image is
    /// not part of it: a newer runtime is reported separately.
    /// The import is not part of it: it runs in its own pod.
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
    #[error("the VM has no image to import, or its claim has no UID")]
    NoImport,
}

/// One slot of `slot_resource`, and the CPU and memory of the VM, as
/// requests equal to limits.
fn vm_resources(input: &AnchorInput, slot_resource: &str) -> ResourceRequirements {
    let memory = resources::pod_memory_bytes(input.guest_memory_bytes).to_string();
    let cpu = input.vcpus.to_string();
    let list = BTreeMap::from([
        ("cpu".to_owned(), q(cpu)),
        ("memory".to_owned(), q(memory)),
        (slot_resource.to_owned(), q("1")),
    ]);
    ResourceRequirements {
        requests: Some(list.clone()),
        limits: Some(list),
        ..Default::default()
    }
}

/// How a container sees the root disk: a mount or a device.
fn disk_access(mode: DiskMode) -> (Vec<VolumeMount>, Option<Vec<VolumeDevice>>) {
    match mode {
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
    }
}

/// The pod-level part shared by the Anchor Pod and the import Pod: the
/// claim volume, the VM's identity, placement constraints.
fn vm_pod_spec(
    input: &AnchorInput,
    containers: Vec<Container>,
    restart: &str,
) -> Result<PodSpec, BuildError> {
    // Containers run as the VM's identity. The pod-level fsGroup gives
    // filesystem volumes that support ownership management to the same
    // group.
    let identity = VmIdentity::for_vm(&input.vm_uid)?;
    let uid = i64::from(identity.uid());
    let gid = i64::from(identity.gid());
    Ok(PodSpec {
        containers,
        volumes: Some(vec![Volume {
            name: "root-disk".to_owned(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: input.claim_name.clone(),
                read_only: None,
            }),
            ..Default::default()
        }]),
        affinity: avoid_nodes_affinity(&input.avoid_nodes),
        restart_policy: Some(restart.to_owned()),
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
    })
}

/// Build the import Pod: it writes the image into the root disk claim and
/// exits. It requests the VM's CPU and memory and one
/// `virtualization.baryon.io/import` slot, which has the health of the
/// node's VM slots but grants no device or socket, so a claim that binds on
/// first use (WaitForFirstConsumer) binds on a node that can run the VM.
pub(crate) fn build_import(input: &AnchorInput) -> Result<Pod, BuildError> {
    let (url, sha, size) = input.import.as_ref().ok_or(BuildError::NoImport)?;
    if input.claim_uid.is_empty() {
        return Err(BuildError::NoImport);
    }
    let mut args = vec![
        "import".to_owned(),
        format!("--url={url}"),
        format!("--size-bytes={size}"),
        format!("--claim-uid={}", input.claim_uid),
    ];
    if let Some(s) = sha {
        args.push(format!("--sha256={s}"));
    }
    if input.disk_mode == DiskMode::Block {
        args.push(format!("--device={ROOT_DISK_DEVICE}"));
    }
    args.push(format!("--approval-file={APPROVAL_DIR}/approved"));
    let (mut mounts, devices) = disk_access(input.disk_mode);
    mounts.push(VolumeMount {
        name: "import-approval".to_owned(),
        mount_path: APPROVAL_DIR.to_owned(),
        read_only: Some(true),
        ..Default::default()
    });
    let importer = Container {
        name: IMPORT_CONTAINER.to_owned(),
        image: Some(input.anchor_image.clone()),
        args: Some(args),
        resources: Some(vm_resources(input, keys::IMPORT_RESOURCE)),
        volume_mounts: Some(mounts),
        volume_devices: devices,
        security_context: Some(restricted_container_security()),
        termination_message_policy: Some("File".to_owned()),
        ..Default::default()
    };
    let labels = BTreeMap::from([
        (IMPORT_FOR_LABEL.to_owned(), input.vm_uid.clone()),
        (
            keys::MANAGED_BY_LABEL.to_owned(),
            keys::MANAGED_BY.to_owned(),
        ),
    ]);
    let mut annotations =
        BTreeMap::from([(IMPORT_SOURCE_ANNOTATION.to_owned(), source_digest(url))]);
    if let Some(uid) = &input.import_approval {
        annotations.insert(IMPORT_APPROVED_ANNOTATION.to_owned(), uid.clone());
    }
    let mut spec = vm_pod_spec(input, vec![importer], "OnFailure")?;
    // The approval reaches the importer as a file that kubelet updates when
    // the annotation changes.
    spec.volumes.get_or_insert_default().push(Volume {
        name: "import-approval".to_owned(),
        downward_api: Some(DownwardAPIVolumeSource {
            items: Some(vec![DownwardAPIVolumeFile {
                path: "approved".to_owned(),
                field_ref: Some(ObjectFieldSelector {
                    field_path: format!("metadata.annotations['{IMPORT_APPROVED_ANNOTATION}']"),
                    api_version: None,
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    });
    Ok(Pod {
        metadata: ObjectMeta {
            name: Some(resources::import_pod_name(&input.vm_name)),
            namespace: Some(input.vm_namespace.clone()),
            labels: Some(labels),
            annotations: Some(annotations),
            owner_references: Some(vec![input.owner.clone()]),
            ..Default::default()
        },
        spec: Some(spec),
        status: None,
    })
}

/// Build the Anchor Pod.
pub(crate) fn build(input: &AnchorInput) -> Result<Pod, BuildError> {
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

    let (mounts, devices) = disk_access(input.disk_mode);
    let anchor = Container {
        name: ANCHOR_CONTAINER.to_owned(),
        image: Some(input.anchor_image.clone()),
        args: Some(vec!["run".to_owned()]),
        env: Some(vec![EnvVar {
            name: SPEC_ENV.to_owned(),
            value: Some(input.anchor_spec().to_json()?),
            value_from: None,
        }]),
        resources: Some(vm_resources(input, keys::VM_RESOURCE)),
        readiness_probe: Some(Probe {
            exec: Some(ExecAction {
                command: Some(vec!["/baryon-anchor".to_owned(), "probe".to_owned()]),
            }),
            period_seconds: Some(2),
            ..Default::default()
        }),
        volume_mounts: Some(mounts),
        volume_devices: devices,
        security_context: Some(restricted_container_security()),
        termination_message_policy: Some("File".to_owned()),
        ..Default::default()
    };

    let mut spec = vm_pod_spec(input, vec![anchor], "Always")?;
    spec.termination_grace_period_seconds = Some(i64::from(
        resources::STOP_GRACE_SECONDS + resources::TERMINATION_MARGIN_SECONDS,
    ));
    Ok(Pod {
        metadata: ObjectMeta {
            name: Some(resources::anchor_pod_name(&input.vm_name)),
            namespace: Some(input.vm_namespace.clone()),
            labels: Some(labels),
            annotations: Some(annotations),
            owner_references: Some(vec![input.owner.clone()]),
            ..Default::default()
        },
        spec: Some(spec),
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
            claim_uid: "claim-1".into(),
            import_approval: None,
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
        for c in &spec.containers {
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
    fn anchor_pod_never_imports_and_block_mode_uses_devices() {
        assert!(
            build(&input(true))
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
    fn import_pod_runs_the_importer_like_the_anchor() {
        assert!(matches!(
            build_import(&input(false)),
            Err(BuildError::NoImport)
        ));
        let mut i = input(true);
        i.avoid_nodes = vec!["node2".into()];
        let pod = build_import(&i).unwrap();
        assert_eq!(pod.metadata.name.as_deref(), Some("web-import"));
        let labels = pod.metadata.labels.as_ref().unwrap();
        // Services select VMs by name; the import Pod must not match them.
        assert!(!labels.contains_key(keys::VM_NAME_LABEL));
        assert_eq!(labels[IMPORT_FOR_LABEL], i.vm_uid);
        let source = &pod.metadata.annotations.as_ref().unwrap()[IMPORT_SOURCE_ANNOTATION];
        assert_eq!(source, &source_digest("https://example.org/d.raw"));
        assert!(source.starts_with("sha256:") && !source.contains("example.org"));
        let spec = pod.spec.as_ref().unwrap();
        let anchor = build(&i).unwrap().spec.unwrap();
        assert_eq!(spec.restart_policy.as_deref(), Some("OnFailure"));
        assert_eq!(spec.security_context, anchor.security_context);
        assert_eq!(spec.affinity, anchor.affinity);
        // The claim volume of the Anchor Pod, plus the approval file.
        let vols = spec.volumes.as_ref().unwrap();
        assert_eq!(vols[0], anchor.volumes.as_ref().unwrap()[0]);
        let approval = vols.iter().find(|v| v.name == "import-approval").unwrap();
        let file = &approval
            .downward_api
            .as_ref()
            .unwrap()
            .items
            .as_ref()
            .unwrap()[0];
        assert_eq!(
            file.field_ref.as_ref().unwrap().field_path,
            format!("metadata.annotations['{IMPORT_APPROVED_ANNOTATION}']")
        );
        // Not approved until the controller has checked the bound volume.
        assert!(
            !pod.metadata
                .annotations
                .as_ref()
                .unwrap()
                .contains_key(IMPORT_APPROVED_ANNOTATION)
        );
        assert_eq!(spec.automount_service_account_token, Some(false));
        let c = &spec.containers[0];
        // The VM's CPU and memory, but the placement-only slot: no /dev/kvm,
        // no anchor socket.
        let limits = c.resources.as_ref().unwrap().limits.as_ref().unwrap();
        let anchor_limits = anchor.containers[0]
            .resources
            .as_ref()
            .unwrap()
            .limits
            .as_ref()
            .unwrap();
        assert_eq!(limits["cpu"], anchor_limits["cpu"]);
        assert_eq!(limits["memory"], anchor_limits["memory"]);
        assert_eq!(limits[keys::IMPORT_RESOURCE].0, "1");
        assert!(!limits.contains_key(keys::VM_RESOURCE));
        assert_eq!(
            c.resources.as_ref().unwrap().requests.as_ref(),
            Some(limits)
        );
        let args = c.args.as_ref().unwrap();
        assert_eq!(args[0], "import");
        assert!(args.contains(&"--url=https://example.org/d.raw".to_owned()));
        assert!(!args.iter().any(|a| a.starts_with("--device")));
    }

    #[test]
    fn block_import_writes_the_device() {
        let mut i = input(true);
        i.disk_mode = DiskMode::Block;
        let pod = build_import(&i).unwrap();
        let c = &pod.spec.unwrap().containers[0];
        assert!(
            c.args
                .as_ref()
                .unwrap()
                .contains(&format!("--device={ROOT_DISK_DEVICE}"))
        );
        assert_eq!(
            c.volume_devices.as_ref().unwrap()[0].device_path,
            ROOT_DISK_DEVICE
        );
        // Only the approval file is mounted; the disk is a device.
        let mounts = c.volume_mounts.as_ref().unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].name, "import-approval");
        assert_eq!(mounts[0].read_only, Some(true));
        assert!(
            c.args
                .as_ref()
                .unwrap()
                .contains(&format!("--approval-file={APPROVAL_DIR}/approved"))
        );
    }

    #[test]
    fn an_approved_import_pod_carries_its_approval() {
        let mut i = input(true);
        i.import_approval = Some("claim-1".into());
        let pod = build_import(&i).unwrap();
        assert_eq!(
            pod.metadata.annotations.as_ref().unwrap()[IMPORT_APPROVED_ANNOTATION],
            "claim-1"
        );
        // The approval does not change the Anchor Pod.
        assert_eq!(i.spec_hash(), input(true).spec_hash());
    }

    #[test]
    fn hash_ignores_the_import() {
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
