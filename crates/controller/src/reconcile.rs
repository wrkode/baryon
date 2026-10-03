//! VirtualMachine reconciliation.
//!
//! For each VirtualMachine the controller ensures, in order:
//!
//! 1. the class exists and is valid
//! 2. the root disk claim exists (created and owned by the VM for imported
//!    disks, referenced for existing claims)
//! 3. the Anchor Pod exists when the VM should run, and does not otherwise
//! 4. status reflects the Anchor Pod and claim
//!
//! A finalizer guarantees that the Anchor Pod, and therefore the VMM, is gone
//! before the VirtualMachine disappears. Claims created by Baryon are owned
//! by the VM and garbage collected after it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::core::v1::{
    PersistentVolumeClaim, PersistentVolumeClaimSpec, Pod, VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{OwnerReference, Time};
use k8s_openapi::jiff::Timestamp;
use kube::api::{Api, DeleteParams, ObjectMeta, Patch, PatchParams, PostParams, Preconditions};
use kube::runtime::controller::Action;
use kube::runtime::events::{Event, EventType, Recorder};
use kube::runtime::finalizer::{self, finalizer};
use kube::{Resource, ResourceExt};

use baryon_api::vm::{RunPolicy, VolumeMode};
use baryon_api::{VirtualMachine, VirtualMachineClass, keys, quantity};
use baryon_runtime_api::anchor_spec::DiskMode;

use crate::anchor::{self, AnchorInput, SPEC_HASH_ANNOTATION};
use crate::resources;
use crate::status::{self, Observed};

const FIELD_MANAGER: &str = "baryon-controller";

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("Kubernetes API: {0}")]
    Kube(#[from] kube::Error),
    #[error("finalizer: {0}")]
    Finalizer(#[source] Box<finalizer::Error<Error>>),
    #[error("object has no namespace")]
    NoNamespace,
    #[error("encoding: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("Anchor Pod {0} is still terminating")]
    AnchorTerminating(String),
    #[error("building the Anchor Pod: {0}")]
    AnchorBuild(#[from] anchor::BuildError),
}

/// Shared controller state.
pub(crate) struct Context {
    pub client: kube::Client,
    pub anchor_image: String,
    pub recorder: Recorder,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("anchor_image", &self.anchor_image)
            .finish_non_exhaustive()
    }
}

pub(crate) async fn reconcile(vm: Arc<VirtualMachine>, ctx: Arc<Context>) -> Result<Action, Error> {
    let ns = vm.namespace().ok_or(Error::NoNamespace)?;
    let api: Api<VirtualMachine> = Api::namespaced(ctx.client.clone(), &ns);
    finalizer(&api, keys::VM_FINALIZER, vm, |event| async {
        match event {
            finalizer::Event::Apply(vm) => apply(&vm, &ctx, &ns).await,
            finalizer::Event::Cleanup(vm) => cleanup(&vm, &ctx, &ns).await,
        }
    })
    .await
    .map_err(|e| Error::Finalizer(Box::new(e)))
}

pub(crate) fn error_policy(vm: Arc<VirtualMachine>, err: &Error, _ctx: Arc<Context>) -> Action {
    match err {
        Error::Finalizer(inner)
            if matches!(
                **inner,
                finalizer::Error::CleanupFailed(Error::AnchorTerminating(_))
            ) =>
        {
            tracing::debug!(vm = %vm.name_any(), "waiting for the Anchor Pod to terminate");
            Action::requeue(Duration::from_secs(2))
        }
        _ => {
            tracing::warn!(vm = %vm.name_any(), error = %err, "reconcile failed");
            Action::requeue(Duration::from_secs(10))
        }
    }
}

/// Whether `meta` names this VirtualMachine (by UID) as its controller.
fn owned_by(meta: &ObjectMeta, vm_uid: &str) -> bool {
    !vm_uid.is_empty()
        && meta.owner_references.as_ref().is_some_and(|refs| {
            refs.iter().any(|r| {
                r.uid == vm_uid && r.controller == Some(true) && r.kind == "VirtualMachine"
            })
        })
}

/// Delete exactly this pod: a pod re-created under the same name in the
/// meantime is not touched.
fn delete_this(pod: &Pod) -> DeleteParams {
    DeleteParams {
        preconditions: Some(Preconditions {
            uid: pod.metadata.uid.clone(),
            resource_version: None,
        }),
        ..DeleteParams::default()
    }
}

/// The anchor container image of `pod`.
fn anchor_image_of(pod: &Pod) -> Option<&str> {
    pod.spec
        .as_ref()?
        .containers
        .iter()
        .find(|c| c.name == anchor::ANCHOR_CONTAINER)?
        .image
        .as_deref()
}

fn owner(vm: &VirtualMachine) -> OwnerReference {
    OwnerReference {
        api_version: "virtualization.baryon.io/v1alpha1".to_owned(),
        kind: "VirtualMachine".to_owned(),
        name: vm.name_any(),
        uid: vm.meta().uid.clone().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    }
}

fn labels(vm: &VirtualMachine) -> BTreeMap<String, String> {
    BTreeMap::from([
        (keys::VM_NAME_LABEL.to_owned(), vm.name_any()),
        (
            keys::MANAGED_BY_LABEL.to_owned(),
            keys::MANAGED_BY.to_owned(),
        ),
    ])
}

async fn event(ctx: &Context, vm: &VirtualMachine, type_: EventType, reason: &str, note: String) {
    let ev = Event {
        type_,
        reason: reason.to_owned(),
        note: Some(note),
        action: reason.to_owned(),
        secondary: None,
    };
    if let Err(e) = ctx.recorder.publish(&ev, &vm.object_ref(&())).await {
        tracing::debug!(error = %e, "publishing event failed");
    }
}

struct Compute {
    vcpus: u32,
    memory_bytes: u64,
}

async fn resolve_class(
    ctx: &Context,
    vm: &VirtualMachine,
) -> Result<Result<Compute, (String, String)>, Error> {
    let classes: Api<VirtualMachineClass> = Api::all(ctx.client.clone());
    let Some(class) = classes.get_opt(&vm.spec.class_name).await? else {
        return Ok(Err((
            "ClassNotFound".to_owned(),
            format!("VirtualMachineClass {} does not exist", vm.spec.class_name),
        )));
    };
    let memory = match quantity::to_bytes(&class.spec.memory.size.0) {
        Ok(b) if b >= 64 << 20 && b.is_multiple_of(1 << 20) => b,
        Ok(_) | Err(_) => {
            return Ok(Err((
                "InvalidClass".to_owned(),
                format!(
                    "VirtualMachineClass {}: memory must be at least 64Mi and a whole number of Mi",
                    vm.spec.class_name
                ),
            )));
        }
    };
    Ok(Ok(Compute {
        vcpus: class.spec.cpu.cores,
        memory_bytes: memory,
    }))
}

struct Disk {
    claim_name: String,
    claim: Option<PersistentVolumeClaim>,
    mode: DiskMode,
    import: Option<(String, Option<String>, u64)>,
}

async fn ensure_claim(
    ctx: &Context,
    vm: &VirtualMachine,
    ns: &str,
) -> Result<Result<Disk, (String, String)>, Error> {
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), ns);
    let source = &vm.spec.root_disk.source;
    if let Some(existing) = &source.persistent_volume_claim {
        let claim = pvcs.get_opt(&existing.claim_name).await?;
        let Some(claim) = claim else {
            return Ok(Err((
                "ClaimNotFound".to_owned(),
                format!(
                    "PersistentVolumeClaim {} does not exist",
                    existing.claim_name
                ),
            )));
        };
        let mode = match claim.spec.as_ref().and_then(|s| s.volume_mode.as_deref()) {
            Some("Block") => DiskMode::Block,
            _ => DiskMode::Filesystem,
        };
        return Ok(Ok(Disk {
            claim_name: existing.claim_name.clone(),
            claim: Some(claim),
            mode,
            import: None,
        }));
    }
    let Some(http) = &source.http else {
        return Ok(Err((
            "InvalidSpec".to_owned(),
            "rootDisk.source is empty".to_owned(),
        )));
    };
    let Some(size) = vm.spec.root_disk.size.as_ref() else {
        return Ok(Err((
            "InvalidSpec".to_owned(),
            "rootDisk.size is required".to_owned(),
        )));
    };
    let size_bytes = match quantity::to_bytes(&size.0) {
        Ok(b) => b,
        Err(e) => return Ok(Err(("InvalidSpec".to_owned(), e.to_string()))),
    };
    let name = resources::root_claim_name(&vm.name_any());
    let wanted_mode = match vm.spec.root_disk.volume_mode.unwrap_or_default() {
        VolumeMode::Filesystem => "Filesystem",
        VolumeMode::Block => "Block",
    };
    let claim = match pvcs.get_opt(&name).await? {
        Some(c) => {
            let ours = c
                .owner_references()
                .iter()
                .any(|o| o.uid == vm.meta().uid.clone().unwrap_or_default());
            if !ours {
                return Ok(Err((
                    "ClaimConflict".to_owned(),
                    format!("PersistentVolumeClaim {name} exists and is not owned by this VM"),
                )));
            }
            c
        }
        None => {
            let pvc = PersistentVolumeClaim {
                metadata: ObjectMeta {
                    name: Some(name.clone()),
                    namespace: Some(ns.to_owned()),
                    labels: Some(labels(vm)),
                    owner_references: Some(vec![owner(vm)]),
                    ..Default::default()
                },
                spec: Some(PersistentVolumeClaimSpec {
                    access_modes: Some(vec!["ReadWriteOnce".to_owned()]),
                    volume_mode: Some(wanted_mode.to_owned()),
                    storage_class_name: vm.spec.root_disk.storage_class_name.clone(),
                    resources: Some(VolumeResourceRequirements {
                        requests: Some(BTreeMap::from([(
                            "storage".to_owned(),
                            // Room for the import marker and filesystem metadata.
                            Quantity(
                                ((size_bytes + (64 << 20)).div_ceil(1 << 20) << 20).to_string(),
                            ),
                        )])),
                        limits: None,
                    }),
                    ..Default::default()
                }),
                status: None,
            };
            let created = pvcs.create(&PostParams::default(), &pvc).await?;
            event(
                ctx,
                vm,
                EventType::Normal,
                "ClaimCreated",
                format!("created root disk claim {name}"),
            )
            .await;
            created
        }
    };
    // The claim's own mode decides (volumeMode is immutable on a claim).
    let mode = match claim.spec.as_ref().and_then(|s| s.volume_mode.as_deref()) {
        Some("Block") => DiskMode::Block,
        _ => DiskMode::Filesystem,
    };
    // Import until the claim records a completed import.
    let imported = claim
        .annotations()
        .get(keys::IMPORTED_ANNOTATION)
        .is_some_and(|u| u == &http.url);
    Ok(Ok(Disk {
        claim_name: name,
        claim: Some(claim),
        mode,
        import: (!imported).then(|| (http.url.clone(), http.sha256.clone(), size_bytes)),
    }))
}

async fn apply(vm: &VirtualMachine, ctx: &Context, ns: &str) -> Result<Action, Error> {
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), ns);
    let vm_name = vm.name_any();
    let vm_uid = vm.meta().uid.clone().unwrap_or_default();
    let pod_name = resources::anchor_pod_name(&vm_name);
    let mut pod = pods.get_opt(&pod_name).await?;

    let mut obs_blocked = None;
    let mut restart_reason = None;
    let mut claim_name = None;
    let mut claim = None;
    let mut has_import = false;

    let compute = resolve_class(ctx, vm).await?;
    let disk = ensure_claim(ctx, vm, ns).await?;
    match (&compute, &disk) {
        (Err(b), _) | (_, Err(b)) => obs_blocked = Some(b.clone()),
        _ => {}
    }
    // A pod with the Anchor Pod's name that this VM does not control is left
    // alone and never reported as the VM's.
    if let Some(p) = pod.as_ref().filter(|p| !owned_by(&p.metadata, &vm_uid)) {
        let message = format!(
            "pod {pod_name} exists and is not controlled by this VirtualMachine; delete or rename it"
        );
        tracing::warn!(vm = %vm_name, pod = ?p.metadata.uid, "{message}");
        let observed = Observed {
            blocked: Some(("AnchorPodConflict".to_owned(), message)),
            claim_name: disk.as_ref().ok().map(|d| d.claim_name.clone()),
            claim: disk.as_ref().ok().and_then(|d| d.claim.as_ref()),
            pod: None,
            has_import: disk.as_ref().is_ok_and(|d| d.import.is_some()),
            restart_reason: None,
            mac_address: Some(resources::vm_mac(&vm_uid)),
            identity_conflict_nodes: vm
                .status
                .as_ref()
                .map(|s| s.identity_conflict_nodes.clone())
                .unwrap_or_default(),
        };
        write_status(vm, ctx, ns, &observed).await?;
        return Ok(Action::requeue(Duration::from_secs(60)));
    }
    if let Ok(d) = &disk {
        claim_name = Some(d.claim_name.clone());
        claim.clone_from(&d.claim);
        has_import = d.import.is_some();
        // A completed import is recorded on the claim, so later Anchor Pods
        // start without importing again (a raw block volume has nowhere else
        // to keep that state).
        if let (Some((url, _, _)), Some(p)) = (&d.import, pod.as_ref())
            && import_succeeded(p)
        {
            let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), ns);
            let patch = serde_json::json!({
                "metadata": {"annotations": {keys::IMPORTED_ANNOTATION: url}}
            });
            pvcs.patch(
                &d.claim_name,
                &PatchParams::default(),
                &Patch::Merge(&patch),
            )
            .await?;
            has_import = false;
        }
    }

    let restarted_at = vm.annotations().get(keys::RESTARTED_AT_ANNOTATION).cloned();
    let should_run = vm.spec.run_policy == RunPolicy::Running;
    // Nodes where another VM has the same host identity. Kept while the VM
    // should run; a stopped VM starts over.
    let mut conflict_nodes: Vec<String> = if should_run {
        vm.status
            .as_ref()
            .map(|s| s.identity_conflict_nodes.clone())
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    if let Some(p) = &pod {
        let terminating = p.metadata.deletion_timestamp.is_some();
        let pod_restart = p.annotations().get(keys::RESTARTED_AT_ANNOTATION).cloned();
        let finished = matches!(
            p.status.as_ref().and_then(|s| s.phase.as_deref()),
            Some("Succeeded" | "Failed")
        );
        if !terminating {
            if !should_run {
                pods.delete(&pod_name, &delete_this(p)).await?;
                event(
                    ctx,
                    vm,
                    EventType::Normal,
                    "Stopping",
                    "runPolicy is Stopped; stopping the VM".to_owned(),
                )
                .await;
            } else if restarted_at.is_some() && restarted_at != pod_restart {
                pods.delete(&pod_name, &delete_this(p)).await?;
                event(
                    ctx,
                    vm,
                    EventType::Normal,
                    "Restarting",
                    "restart requested".to_owned(),
                )
                .await;
            } else if let Some(node) = identity_conflict_node(p) {
                // baryon-virtd refused the VM on this node: another VM there
                // has the same host identity. Replace the pod away from it.
                if !conflict_nodes.contains(&node) && conflict_nodes.len() < MAX_CONFLICT_NODES {
                    conflict_nodes.push(node.clone());
                }
                pods.delete(&pod_name, &delete_this(p)).await?;
                event(
                    ctx,
                    vm,
                    EventType::Warning,
                    "IdentityConflict",
                    format!("another VM on node {node} has the same host identity; placing the VM on another node"),
                )
                .await;
            } else if finished {
                // An evicted or failed pod does not restart by itself.
                pods.delete(&pod_name, &delete_this(p)).await?;
                event(
                    ctx,
                    vm,
                    EventType::Warning,
                    "AnchorPodFinished",
                    "Anchor Pod finished; replacing it".to_owned(),
                )
                .await;
            } else if let (Ok(c), Ok(d)) = (&compute, &disk) {
                let want =
                    anchor_input(vm, ctx, c, d, restarted_at.clone(), &conflict_nodes).spec_hash();
                restart_reason = if p.annotations().get(SPEC_HASH_ANNOTATION) != Some(&want) {
                    Some(status::RestartReason::SpecChanged)
                } else if anchor_image_of(p) != Some(ctx.anchor_image.as_str()) {
                    Some(status::RestartReason::AnchorImageChanged)
                } else {
                    None
                };
            }
        }
        pod = pods.get_opt(&pod_name).await?;
    } else if should_run && let (Ok(c), Ok(d)) = (&compute, &disk) {
        let input = anchor_input(vm, ctx, c, d, restarted_at.clone(), &conflict_nodes);
        let p = anchor::build(&input)?;
        match pods.create(&PostParams::default(), &p).await {
            Ok(created) => {
                event(
                    ctx,
                    vm,
                    EventType::Normal,
                    "Starting",
                    format!(
                        "created Anchor Pod {pod_name} ({} vCPUs, {} MiB)",
                        c.vcpus,
                        c.memory_bytes >> 20
                    ),
                )
                .await;
                pod = Some(created);
            }
            // Another reconcile or a terminating predecessor holds the name.
            Err(kube::Error::Api(e)) if e.code == 409 => {}
            Err(e) => return Err(e.into()),
        }
    }

    let observed = Observed {
        blocked: obs_blocked,
        claim_name,
        claim: claim.as_ref(),
        pod: pod.as_ref(),
        has_import,
        restart_reason,
        mac_address: Some(resources::vm_mac(&vm_uid)),
        identity_conflict_nodes: conflict_nodes,
    };
    write_status(vm, ctx, ns, &observed).await?;
    Ok(Action::requeue(Duration::from_secs(300)))
}

/// Derive the status and write it with server-side apply when it changed.
/// Emits an event when Ready changes.
async fn write_status(
    vm: &VirtualMachine,
    ctx: &Context,
    ns: &str,
    observed: &Observed<'_>,
) -> Result<(), Error> {
    let vm_name = vm.name_any();
    let new_status = status::derive(vm, observed, Time(Timestamp::now()));
    if status::changed(vm.status.as_ref(), &new_status) {
        let ready_before = vm
            .status
            .as_ref()
            .and_then(|s| {
                baryon_api::conditions::find(&s.conditions, baryon_api::conditions::vm::READY)
            })
            .map(|c| c.status.clone());
        let ready_after =
            baryon_api::conditions::find(&new_status.conditions, baryon_api::conditions::vm::READY);
        let api: Api<VirtualMachine> = Api::namespaced(ctx.client.clone(), ns);
        let patch = serde_json::json!({
            "apiVersion": "virtualization.baryon.io/v1alpha1",
            "kind": "VirtualMachine",
            "status": new_status,
        });
        api.patch_status(
            &vm_name,
            &PatchParams::apply(FIELD_MANAGER).force(),
            &Patch::Apply(&patch),
        )
        .await?;
        if let Some(after) = ready_after
            && ready_before.as_deref() != Some(after.status.as_str())
        {
            let (t, reason) = if after.status == "True" {
                (EventType::Normal, "Ready")
            } else {
                (EventType::Normal, "NotReady")
            };
            event(
                ctx,
                vm,
                t,
                reason,
                format!("{}: {}", after.reason, after.message),
            )
            .await;
        }
    }
    Ok(())
}

fn anchor_input(
    vm: &VirtualMachine,
    ctx: &Context,
    c: &Compute,
    d: &Disk,
    restarted_at: Option<String>,
    avoid_nodes: &[String],
) -> AnchorInput {
    AnchorInput {
        vm_name: vm.name_any(),
        vm_namespace: vm.namespace().unwrap_or_default(),
        vm_uid: vm.meta().uid.clone().unwrap_or_default(),
        owner: owner(vm),
        vcpus: c.vcpus,
        guest_memory_bytes: c.memory_bytes,
        claim_name: d.claim_name.clone(),
        disk_mode: d.mode,
        import: d.import.clone(),
        anchor_image: ctx.anchor_image.clone(),
        restarted_at,
        avoid_nodes: avoid_nodes.to_vec(),
    }
}

/// Whether the import init container of `pod` finished successfully.
fn import_succeeded(pod: &Pod) -> bool {
    pod.status
        .as_ref()
        .and_then(|s| s.init_container_statuses.as_ref())
        .is_some_and(|statuses| {
            statuses.iter().any(|c| {
                c.name == "import"
                    && c.state
                        .as_ref()
                        .and_then(|s| s.terminated.as_ref())
                        .is_some_and(|t| t.exit_code == 0)
            })
        })
}

/// Upper bound of `identityConflictNodes`.
const MAX_CONFLICT_NODES: usize = 16;

/// The node of `pod` if its anchor ended because baryon-virtd refused the
/// VM there for a host identity conflict.
fn identity_conflict_node(pod: &Pod) -> Option<String> {
    let node = pod.spec.as_ref()?.node_name.clone()?;
    let refused = pod
        .status
        .as_ref()?
        .container_statuses
        .as_ref()?
        .iter()
        .filter(|c| c.name == anchor::ANCHOR_CONTAINER)
        .flat_map(|c| [c.state.as_ref(), c.last_state.as_ref()])
        .flatten()
        .filter_map(|s| s.terminated.as_ref()?.message.as_deref())
        .any(|m| m.trim_start().starts_with("IdentityConflict:"));
    refused.then_some(node)
}

async fn cleanup(vm: &VirtualMachine, ctx: &Context, ns: &str) -> Result<Action, Error> {
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), ns);
    let pod_name = resources::anchor_pod_name(&vm.name_any());
    match pods.get_opt(&pod_name).await? {
        None => {
            event(
                ctx,
                vm,
                EventType::Normal,
                "Deleted",
                "the VM is stopped and its Anchor Pod is gone".to_owned(),
            )
            .await;
            Ok(Action::await_change())
        }
        Some(p) if !owned_by(&p.metadata, vm.meta().uid.as_deref().unwrap_or("")) => {
            // Not this VM's pod: nothing of ours to wait for.
            tracing::warn!(vm = %vm.name_any(), pod = %pod_name, "a pod with the Anchor Pod name is not controlled by this VM; not deleting it");
            Ok(Action::await_change())
        }
        Some(p) => {
            if p.metadata.deletion_timestamp.is_none() {
                pods.delete(&pod_name, &delete_this(&p)).await?;
                event(
                    ctx,
                    vm,
                    EventType::Normal,
                    "Stopping",
                    "VM deleted; shutting down the guest".to_owned(),
                )
                .await;
            }
            // Keep the finalizer until the pod is gone: the VMM stops before
            // the pod's volumes and network are torn down.
            Err(Error::AnchorTerminating(pod_name))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(owner_uid: &str, kind: &str, controller: bool) -> ObjectMeta {
        ObjectMeta {
            owner_references: Some(vec![OwnerReference {
                api_version: "virtualization.baryon.io/v1alpha1".into(),
                kind: kind.into(),
                name: "web".into(),
                uid: owner_uid.into(),
                controller: Some(controller),
                block_owner_deletion: Some(true),
            }]),
            ..Default::default()
        }
    }

    #[test]
    fn only_the_controlling_vm_owns_a_pod() {
        let uid = "6b7f4c0e-6a39-4b8e-9d0e-2f1f0c6c4a11";
        assert!(owned_by(&meta(uid, "VirtualMachine", true), uid));
        assert!(!owned_by(&meta(uid, "VirtualMachine", false), uid));
        assert!(!owned_by(&meta(uid, "ReplicaSet", true), uid));
        assert!(!owned_by(&meta("other", "VirtualMachine", true), uid));
        assert!(!owned_by(&ObjectMeta::default(), uid));
        assert!(!owned_by(&meta("", "VirtualMachine", true), ""));
    }

    #[test]
    fn detects_identity_conflicts_from_the_anchor_termination() {
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateTerminated, ContainerStatus, PodSpec, PodStatus,
        };
        let pod = |msg: &str| Pod {
            spec: Some(PodSpec {
                node_name: Some("node2".into()),
                ..Default::default()
            }),
            status: Some(PodStatus {
                container_statuses: Some(vec![ContainerStatus {
                    name: anchor::ANCHOR_CONTAINER.into(),
                    last_state: Some(ContainerState {
                        terminated: Some(ContainerStateTerminated {
                            exit_code: 1,
                            message: Some(msg.into()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            identity_conflict_node(&pod("IdentityConflict: another VM on this node")),
            Some("node2".to_owned())
        );
        assert_eq!(identity_conflict_node(&pod("VmmKilled: signal 9")), None);
        assert_eq!(identity_conflict_node(&Pod::default()), None);
    }

    #[test]
    fn deletes_carry_a_uid_precondition() {
        let pod = Pod {
            metadata: ObjectMeta {
                uid: Some("abc".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let p = delete_this(&pod);
        assert_eq!(p.preconditions.and_then(|c| c.uid).as_deref(), Some("abc"));
    }
}
