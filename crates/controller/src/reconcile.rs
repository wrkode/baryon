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
    ContainerStatus, PersistentVolume, PersistentVolumeClaim, PersistentVolumeClaimSpec, Pod,
    VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{OwnerReference, Time};
use k8s_openapi::jiff::Timestamp;
use kube::api::{
    Api, DeleteParams, ListParams, ObjectMeta, Patch, PatchParams, PostParams, Preconditions,
};
use kube::runtime::controller::Action;
use kube::runtime::events::{Event, EventType, Recorder};
use kube::runtime::finalizer::{self, finalizer};
use kube::{Resource, ResourceExt};

use baryon_api::vm::{RootDiskImportStatus, RunPolicy, VolumeMode};
use baryon_api::{VirtualMachine, VirtualMachineClass, keys, quantity};
use baryon_runtime_api::anchor_spec::DiskMode;

use crate::anchor::{self, AnchorInput, SPEC_HASH_ANNOTATION};
use crate::disk_import;
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
    pub importer_image: String,
    pub recorder: Recorder,
    pub nodes: Arc<crate::nodes::NodeView>,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("anchor_image", &self.anchor_image)
            .field("importer_image", &self.importer_image)
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
    if class.spec.cpu.vcpus == 0 {
        // Stored before cpu.cores became cpu.vcpus (0.1.0-rc1).
        return Ok(Err((
            "InvalidClass".to_owned(),
            format!(
                "VirtualMachineClass {} has no cpu.vcpus; apply it again with cpu.vcpus",
                vm.spec.class_name
            ),
        )));
    }
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
        vcpus: class.spec.cpu.vcpus,
        memory_bytes: memory,
    }))
}

struct Disk {
    claim_name: String,
    claim: Option<PersistentVolumeClaim>,
    mode: DiskMode,
    import: Option<(String, Option<String>, u64)>,
    /// The PersistentVolume bound to a claim Baryon created.
    pv: Option<PersistentVolume>,
    /// The importer may write: the bound volume was checked to be fresh.
    approved: bool,
}

/// The root disk claim and whether its disk must be imported. The second
/// value is the UID of the claim when it holds an imported or used disk,
/// for `status.rootDiskImport`, also when the disk is refused.
/// Bytes the root disk claim requests for a disk of `size_bytes`, rounded
/// up to MiB. A filesystem claim adds 64 MiB for the import marker and the
/// filesystem's own metadata; a block claim is the disk itself.
fn claim_request_bytes(size_bytes: u64, volume_mode: &str) -> u64 {
    let wanted = if volume_mode == "Block" {
        size_bytes
    } else {
        size_bytes.saturating_add(64 << 20)
    };
    wanted.div_ceil(1 << 20).saturating_mul(1 << 20)
}

async fn ensure_claim(
    ctx: &Context,
    vm: &VirtualMachine,
    ns: &str,
) -> Result<(Result<Disk, (String, String)>, Option<String>), Error> {
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(ctx.client.clone(), ns);
    let source = &vm.spec.root_disk.source;
    if let Some(existing) = &source.persistent_volume_claim {
        let claim = pvcs.get_opt(&existing.claim_name).await?;
        let Some(claim) = claim else {
            return Ok((
                Err((
                    "ClaimNotFound".to_owned(),
                    format!(
                        "PersistentVolumeClaim {} does not exist",
                        existing.claim_name
                    ),
                )),
                None,
            ));
        };
        let mode = match claim.spec.as_ref().and_then(|s| s.volume_mode.as_deref()) {
            Some("Block") => DiskMode::Block,
            _ => DiskMode::Filesystem,
        };
        return Ok((
            Ok(Disk {
                claim_name: existing.claim_name.clone(),
                claim: Some(claim),
                pv: None,
                mode,
                import: None,
                approved: false,
            }),
            None,
        ));
    }
    let Some(http) = &source.http else {
        return Ok((
            Err((
                "InvalidSpec".to_owned(),
                "rootDisk.source is empty".to_owned(),
            )),
            None,
        ));
    };
    let Some(size) = vm.spec.root_disk.size.as_ref() else {
        return Ok((
            Err((
                "InvalidSpec".to_owned(),
                "rootDisk.size is required".to_owned(),
            )),
            None,
        ));
    };
    let size_bytes = match quantity::to_bytes(&size.0) {
        Ok(b) => b,
        Err(e) => return Ok((Err(("InvalidSpec".to_owned(), e.to_string())), None)),
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
                return Ok((
                    Err((
                        "ClaimConflict".to_owned(),
                        format!("PersistentVolumeClaim {name} exists and is not owned by this VM"),
                    )),
                    None,
                ));
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
                            Quantity(claim_request_bytes(size_bytes, wanted_mode).to_string()),
                        )])),
                        limits: None,
                    }),
                    ..Default::default()
                }),
                status: None,
            };
            let created = match pvcs.create(&PostParams::default(), &pvc).await {
                Ok(c) => c,
                // The API server refused the claim (quota, admission):
                // report it on the VM.
                Err(kube::Error::Api(e)) if e.code == 422 || e.code == 403 => {
                    return Ok((
                        Err((
                            "ClaimRejected".to_owned(),
                            status::capped(e.message.clone()),
                        )),
                        None,
                    ));
                }
                Err(e) => return Err(e.into()),
            };
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
    let digest = anchor::source_digest(&http.url);
    let pv = match claim
        .spec
        .as_ref()
        .and_then(|s| s.volume_name.as_deref())
        .filter(|_| claim.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Bound"))
    {
        Some(pv_name) => {
            let pvs: Api<PersistentVolume> = Api::all(ctx.client.clone());
            pvs.get_opt(pv_name).await?
        }
        None => None,
    };
    let claim_uid = claim.metadata.uid.clone().unwrap_or_default();
    let imported_before = vm
        .status
        .as_ref()
        .and_then(|s| s.root_disk_import.as_ref())
        .is_some_and(|i| !claim_uid.is_empty() && i.claim_uid == claim_uid);
    let was_ready = imported_before || legacy_ready(vm, &claim);
    let facts = disk_import::Facts {
        claim: &claim,
        pv: pv.as_ref(),
        source: &digest,
        url: &http.url,
        legacy_claim_record: claim
            .annotations()
            .get(keys::IMPORTED_ANNOTATION)
            .map(String::as_str),
        was_ready,
        block_mode: mode == DiskMode::Block,
    };
    // A used disk stays marked as used while the claim is the same object,
    // also when it is refused.
    let used = was_ready.then(|| claim_uid.clone());
    let approved = match disk_import::decide(&facts) {
        disk_import::Decision::Imported => None,
        disk_import::Decision::RecordLegacy => {
            let written = match &pv {
                Some(pv) => record_import(ctx, pv, vm, &claim, &digest).await?,
                None => false,
            };
            if !written {
                // Not ready until recorded; look again shortly.
                return Ok((
                    Err((
                        "DiskBindingPending".to_owned(),
                        "waiting for the claim and its volume to be bound to each other".to_owned(),
                    )),
                    used,
                ));
            }
            None
        }
        disk_import::Decision::Import { approved } => Some(approved),
        disk_import::Decision::Refuse(reason, message) => {
            return Ok((Err((reason, message)), used));
        }
    };
    let imported = approved.is_none().then_some(claim_uid).or(used);
    Ok((
        Ok(Disk {
            claim_name: name,
            claim: Some(claim),
            pv,
            mode,
            import: approved.map(|_| (http.url.clone(), http.sha256.clone(), size_bytes)),
            approved: approved.unwrap_or(false),
        }),
        imported,
    ))
}

/// `status.rootDiskImport`: set once an import into the current claim is
/// complete and kept while the claim is the same object, so a missing
/// volume record cannot make that claim look new. A recreated claim (new
/// UID) starts over.
fn root_disk_import(
    vm: &VirtualMachine,
    imported_claim_uid: Option<String>,
    claim: Option<&PersistentVolumeClaim>,
) -> Option<RootDiskImportStatus> {
    if let Some(uid) = imported_claim_uid.filter(|u| !u.is_empty()) {
        return Some(RootDiskImportStatus { claim_uid: uid });
    }
    let previous = vm.status.as_ref()?.root_disk_import.clone()?;
    match claim.and_then(|c| c.metadata.uid.as_deref()) {
        Some(uid) if uid != previous.claim_uid => None,
        _ => Some(previous),
    }
}

/// Record a completed import on the claim's PersistentVolume (ADR 0006).
async fn record_import(
    ctx: &Context,
    pv: &PersistentVolume,
    vm: &VirtualMachine,
    claim: &PersistentVolumeClaim,
    digest: &str,
) -> Result<bool, Error> {
    if !disk_import::bound_to(pv, claim) {
        tracing::warn!(pv = %pv.name_any(), claim = %claim.name_any(), "not recording an import: the volume is not bound to the claim");
        return Ok(false);
    }
    let record = disk_import::ImportRecord {
        vm_uid: vm.meta().uid.clone().unwrap_or_default(),
        claim_uid: claim.metadata.uid.clone().unwrap_or_default(),
        source: digest.to_owned(),
    };
    let patch = serde_json::json!({
        "metadata": {"annotations": {disk_import::RECORD_ANNOTATION: record.to_json()}}
    });
    let pvs: Api<PersistentVolume> = Api::all(ctx.client.clone());
    pvs.patch(
        &pv.name_any(),
        &PatchParams::default(),
        &Patch::Merge(&patch),
    )
    .await?;
    Ok(true)
}

async fn apply(vm: &VirtualMachine, ctx: &Context, ns: &str) -> Result<Action, Error> {
    let pods: Api<Pod> = Api::namespaced(ctx.client.clone(), ns);
    let vm_name = vm.name_any();
    let vm_uid = vm.meta().uid.clone().unwrap_or_default();
    let pod_name = resources::anchor_pod_name(&vm_name);
    let mut pod = pods.get_opt(&pod_name).await?;
    let import_name = resources::import_pod_name(&vm_name);
    let mut import_pod = pods
        .get_opt(&import_name)
        .await?
        .filter(|p| owned_by(&p.metadata, &vm_uid));
    // Only a pod baryon-controller created can request the import resource
    // (admission policy baryon-vm-resource). Any other pod with the import
    // Pod's name is left alone, never trusted, and blocks the VM.
    let import_conflict = import_pod
        .as_ref()
        .is_some_and(|p| !requests(p, keys::IMPORT_RESOURCE))
        || pods
            .get_opt(&import_name)
            .await?
            .is_some_and(|p| !owned_by(&p.metadata, &vm_uid));
    if import_conflict {
        import_pod = None;
    }

    let mut obs_blocked = None;
    let mut restart_reason = None;
    let mut claim_name = None;
    let mut claim = None;
    let mut has_import = false;

    let compute = resolve_class(ctx, vm).await?;
    let (disk, used_claim_uid) = ensure_claim(ctx, vm, ns).await?;
    let mut imported_claim_uid = used_claim_uid;
    match (&compute, &disk) {
        (Err(b), _) | (_, Err(b)) => obs_blocked = Some(b.clone()),
        _ => {}
    }
    if import_conflict && obs_blocked.is_none() {
        obs_blocked = Some((
            "ImportPodConflict".to_owned(),
            format!("pod {import_name} exists and was not created by baryon-controller; delete it"),
        ));
    }
    // A refused import is a storage state: report it on StorageReady too.
    let storage_blocked = disk
        .as_ref()
        .err()
        .filter(|(reason, _)| disk_import::REFUSALS.contains(&reason.as_str()))
        .cloned();
    // A pod with the Anchor Pod's name that this VM does not control is left
    // alone and never reported as the VM's.
    // Only a pod baryon-controller created can request the vm resource
    // (admission policy baryon-vm-resource); any other pod with the Anchor
    // Pod's name is not the VM's.
    if let Some(p) = pod
        .as_ref()
        .filter(|p| !owned_by(&p.metadata, &vm_uid) || !requests(p, keys::VM_RESOURCE))
    {
        let message = format!(
            "pod {pod_name} exists and is not controlled by this VirtualMachine; delete or rename it"
        );
        tracing::warn!(vm = %vm_name, pod = ?p.metadata.uid, "{message}");
        let observed = Observed {
            blocked: Some(("AnchorPodConflict".to_owned(), message)),
            claim_name: disk.as_ref().ok().map(|d| d.claim_name.clone()),
            claim: disk.as_ref().ok().and_then(|d| d.claim.as_ref()),
            pod: None,
            import_pod: None,
            has_import: disk.as_ref().is_ok_and(|d| d.import.is_some()),
            restart_reason: None,
            mac_address: Some(resources::vm_mac(&vm_uid)),
            identity_conflict_nodes: recorded_conflict_nodes(vm, ctx, ns).await?,
            storage_blocked: None,
            disk_error: disk.as_ref().err().cloned(),
            root_disk_import: vm.status.as_ref().and_then(|s| s.root_disk_import.clone()),
            node: None,
            node_fresh: false,
        };
        write_status(vm, ctx, ns, &observed).await?;
        return Ok(Action::requeue(Duration::from_secs(60)));
    }
    if let Ok(d) = &disk {
        claim_name = Some(d.claim_name.clone());
        claim.clone_from(&d.claim);
        has_import = d.import.is_some();
        // A completed import is recorded on the claim's PersistentVolume
        // before any Anchor Pod uses the disk, so no later pod imports again.
        // Only the import Pod baryon-controller created for this claim and
        // URL counts (ADR 0006).
        if let (Some((url, _, _)), Some(c), Some(pv)) = (&d.import, d.claim.as_ref(), d.pv.as_ref())
        {
            let claim_uid = c.metadata.uid.as_deref().unwrap_or_default();
            let done = import_pod
                .as_ref()
                .is_some_and(|p| import_pod_succeeded(p, url, &d.claim_name, claim_uid));
            if done && record_import(ctx, pv, vm, c, &anchor::source_digest(url)).await? {
                has_import = false;
                imported_claim_uid = c.metadata.uid.clone();
                event(
                    ctx,
                    vm,
                    EventType::Normal,
                    "DiskImported",
                    format!("imported the disk image into claim {}", d.claim_name),
                )
                .await;
            }
        }
    }

    // An empty value requests nothing.
    let restarted_at = vm
        .annotations()
        .get(keys::RESTARTED_AT_ANNOTATION)
        .filter(|v| !v.is_empty())
        .cloned();
    let should_run = vm.spec.run_policy == RunPolicy::Running;
    // Nodes where another VM has the same host identity. Kept while the VM
    // should run; a stopped VM starts over.
    let mut conflict_nodes: Vec<String> = if should_run {
        recorded_conflict_nodes(vm, ctx, ns).await?
    } else {
        Vec::new()
    };

    // One running VM per claim: a pod of another VM that uses the claim
    // blocks this VM from starting (the VMM's file lock does not exclude a
    // second VMM on a block device, and a claim may be shared across nodes).
    if should_run
        && pod.is_none()
        && import_pod.is_none()
        && obs_blocked.is_none()
        && let Ok(d) = &disk
        && let Some(other) = claim_user(&pods, &d.claim_name, &vm_uid).await?
    {
        obs_blocked = Some((
            "ClaimInUse".to_owned(),
            format!(
                "claim {} is used by pod {other} of another VirtualMachine",
                d.claim_name
            ),
        ));
    }

    // The import Pod exists only while an import is pending and the VM
    // should run. One that finished, or imports another URL, is replaced.
    if let Some(ip) = &import_pod
        && ip.metadata.deletion_timestamp.is_none()
    {
        let wanted = disk
            .as_ref()
            .ok()
            .and_then(|d| d.import.as_ref())
            .map(|i| anchor::source_digest(&i.0));
        let current_claim = disk
            .as_ref()
            .ok()
            .and_then(|d| d.claim.as_ref())
            .and_then(|c| c.metadata.uid.as_deref());
        let stale = !should_run
            || !has_import
            || ip.annotations().get(anchor::IMPORT_SOURCE_ANNOTATION) != wanted.as_ref()
            || !current_claim.is_some_and(|uid| import_pod_for_claim(ip, uid))
            || ip.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Failed");
        if stale {
            // The Anchor Pod is created only once this pod is gone, so a
            // VMM never runs while an importer may still write.
            pods.delete(&import_name, &delete_this(ip)).await?;
        } else if let Ok(d) = &disk
            && d.approved
            && let Some(claim_uid) = d.claim.as_ref().and_then(|c| c.metadata.uid.as_deref())
            && ip
                .annotations()
                .get(anchor::IMPORT_APPROVED_ANNOTATION)
                .map(String::as_str)
                != Some(claim_uid)
        {
            // The claim is bound to a volume provisioned for it: let the
            // importer write.
            let patch = serde_json::json!({
                "metadata": {"annotations": {anchor::IMPORT_APPROVED_ANNOTATION: claim_uid}}
            });
            pods.patch(&import_name, &PatchParams::default(), &Patch::Merge(&patch))
                .await?;
        }
    } else if import_pod.is_some() {
        // Terminating; wait for it to go.
    } else if should_run
        && has_import
        && pod.is_none()
        && obs_blocked.is_none()
        && let (Ok(c), Ok(d)) = (&compute, &disk)
    {
        let input = anchor_input(vm, ctx, c, d, restarted_at.clone(), &conflict_nodes);
        match pods
            .create(&PostParams::default(), &anchor::build_import(&input)?)
            .await
        {
            Ok(created) => {
                event(
                    ctx,
                    vm,
                    EventType::Normal,
                    "Importing",
                    format!("created import Pod {import_name}"),
                )
                .await;
                import_pod = Some(created);
            }
            Err(kube::Error::Api(e)) if e.code == 409 => {}
            // The API server refused the pod (invalid placement, an unknown
            // PriorityClass, quota, admission): report it on the VM.
            Err(kube::Error::Api(e)) if e.code == 422 || e.code == 403 => {
                obs_blocked = Some(("PodRejected".to_owned(), status::capped(e.message.clone())));
            }
            Err(e) => return Err(e.into()),
        }
    }

    if let Some(p) = &pod {
        let terminating = p.metadata.deletion_timestamp.is_some();
        let pod_restart = p
            .annotations()
            .get(keys::RESTARTED_AT_ANNOTATION)
            .filter(|v| !v.is_empty())
            .cloned();
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
                let input = anchor_input(vm, ctx, c, d, restarted_at.clone(), &conflict_nodes);
                // The VM's labels follow it to the running Anchor Pod; they
                // select it for Services and for other VMs' placement.
                if let Err(e) = sync_labels(&pods, p, &input.labels).await {
                    tracing::warn!(vm = %vm_name, error = %e, "cannot copy the VM's labels to its Anchor Pod");
                }
                let pod_hash = p.annotations().get(SPEC_HASH_ANNOTATION);
                let current = pod_hash == Some(&input.spec_hash())
                    || pod_hash == Some(&input.spec_hash_with(pod_restart.as_deref()));
                restart_reason = if !current {
                    Some(status::RestartReason::Spec)
                } else if placement_changed(p, &input) {
                    Some(status::RestartReason::Placement)
                } else if anchor_image_of(p) != Some(ctx.anchor_image.as_str()) {
                    Some(status::RestartReason::AnchorImage)
                } else {
                    None
                };
            }
        }
        pod = pods.get_opt(&pod_name).await?;
    } else if should_run
        && !has_import
        && import_pod.is_none()
        && obs_blocked.is_none()
        && let (Ok(c), Ok(d)) = (&compute, &disk)
    {
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
            // The API server refused the pod (invalid placement, an unknown
            // PriorityClass, quota, admission): report it on the VM.
            Err(kube::Error::Api(e)) if e.code == 422 || e.code == 403 => {
                obs_blocked = Some(("PodRejected".to_owned(), status::capped(e.message.clone())));
            }
            Err(e) => return Err(e.into()),
        }
    }

    let node_name = pod
        .as_ref()
        .and_then(|p| p.spec.as_ref())
        .and_then(|s| s.node_name.clone());
    let (node, node_fresh) = node_name
        .as_deref()
        .map_or((None, false), |n| ctx.nodes.get(n));
    let node_settled = node_name.as_deref().is_none_or(|n| ctx.nodes.settled(n));
    let observed = Observed {
        node: node.as_deref(),
        node_fresh,
        blocked: obs_blocked,
        claim_name,
        claim: claim.as_ref(),
        pod: pod.as_ref(),
        import_pod: import_pod.as_ref(),
        has_import,
        restart_reason,
        mac_address: Some(resources::vm_mac(&vm_uid)),
        identity_conflict_nodes: conflict_nodes,
        storage_blocked,
        disk_error: disk.as_ref().err().cloned(),
        root_disk_import: root_disk_import(vm, imported_claim_uid, claim.as_ref()),
    };
    write_status(vm, ctx, ns, &observed).await?;
    // The controller does not watch PersistentVolumes: look again soon while
    // the disk is refused, so a corrected volume is picked up.
    let requeue = match observed.storage_blocked.as_ref().map(|(r, _)| r.as_str()) {
        Some("DiskBindingPending") => 30,
        // Refusals that wait for an administrator.
        Some(_) => 120,
        // A refused pod or claim may be accepted later (quota, a
        // PriorityClass or StorageClass created afterwards).
        None if observed
            .blocked
            .as_ref()
            .is_some_and(|(r, _)| r == "PodRejected" || r == "ClaimRejected") =>
        {
            60
        }
        // The node store lags the watch that triggered this reconcile.
        None if !node_settled => 5,
        None => 300,
    };
    Ok(Action::requeue(Duration::from_secs(requeue)))
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
        let net_before = vm.status.as_ref().and_then(|s| {
            baryon_api::conditions::find(&s.conditions, baryon_api::conditions::vm::NETWORK_READY)
                .map(|c| c.status.clone())
        });
        if let Some(net) = baryon_api::conditions::find(
            &new_status.conditions,
            baryon_api::conditions::vm::NETWORK_READY,
        ) && net_before.as_deref() != Some(net.status.as_str())
            // Warn on every loss; report True only as a recovery, not on
            // every start.
            && (net.status == baryon_api::conditions::FALSE
                || (net.status == baryon_api::conditions::TRUE
                    && net_before.as_deref() == Some(baryon_api::conditions::FALSE)))
        {
            let (t, reason) = if net.status == baryon_api::conditions::TRUE {
                (EventType::Normal, "NetworkReady")
            } else {
                (EventType::Warning, "NetworkNotReady")
            };
            event(
                ctx,
                vm,
                t,
                reason,
                status::capped(format!("{}: {}", net.reason, net.message)),
            )
            .await;
        }
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
        claim_uid: d
            .claim
            .as_ref()
            .and_then(|c| c.metadata.uid.clone())
            .unwrap_or_default(),
        import_approval: d
            .approved
            .then(|| d.claim.as_ref().and_then(|c| c.metadata.uid.clone()))
            .flatten(),
        anchor_image: ctx.anchor_image.clone(),
        importer_image: ctx.importer_image.clone(),
        restarted_at,
        avoid_nodes: avoid_nodes.to_vec(),
        // An empty placement places like none.
        placement: vm
            .spec
            .placement
            .clone()
            .filter(|p| *p != baryon_api::vm::Placement::default()),
        labels: anchor::vm_labels(&vm.labels().clone()),
    }
}

/// Whether the VM's placement differs from the one its Anchor Pod was
/// created with. An Anchor Pod of an earlier release has no placement hash;
/// it matches a VM without placement.
fn placement_changed(pod: &Pod, input: &AnchorInput) -> bool {
    match pod.annotations().get(anchor::PLACEMENT_HASH_ANNOTATION) {
        // Pods created before an empty placement was treated as none carry
        // the hash of the empty placement.
        Some(h) => {
            h != &input.placement_hash()
                && !(input.placement.is_none() && *h == anchor::empty_placement_hash())
        }
        None => input.placement.is_some(),
    }
}

/// Make the VM labels on its Anchor Pod equal to `wanted`. Only labels
/// Baryon copied (listed in the pod's label-keys annotation) are removed;
/// labels that others set on the pod are left alone.
async fn sync_labels(
    pods: &Api<Pod>,
    pod: &Pod,
    wanted: &BTreeMap<String, String>,
) -> Result<(), Error> {
    let copied = anchor::copied_label_keys(pod);
    let current: BTreeMap<String, String> = pod
        .labels()
        .iter()
        .filter(|(k, _)| copied.contains(k.as_str()) || wanted.contains_key(*k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let wanted_keys = anchor::label_keys_value(wanted);
    let keys_current = pod
        .annotations()
        .get(anchor::LABEL_KEYS_ANNOTATION)
        .cloned()
        .unwrap_or_default();
    if &current == wanted && keys_current == wanted_keys {
        return Ok(());
    }
    let mut changes = serde_json::Map::new();
    for k in copied.iter().filter(|k| !wanted.contains_key(**k)) {
        changes.insert((*k).to_owned(), serde_json::Value::Null);
    }
    for (k, v) in wanted {
        changes.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    let patch = serde_json::json!({"metadata": {
        "labels": changes,
        "annotations": {anchor::LABEL_KEYS_ANNOTATION: wanted_keys},
    }});
    pods.patch(
        &pod.name_any(),
        &PatchParams::default(),
        &Patch::Merge(&patch),
    )
    .await?;
    Ok(())
}

/// A Baryon pod of another VM that mounts `claim`, if any.
async fn claim_user(pods: &Api<Pod>, claim: &str, vm_uid: &str) -> Result<Option<String>, Error> {
    let managed = format!("{}={}", keys::MANAGED_BY_LABEL, keys::MANAGED_BY);
    let list = pods.list(&ListParams::default().labels(&managed)).await?;
    Ok(list
        .items
        .iter()
        .find(|p| !owned_by(&p.metadata, vm_uid) && uses_claim(p, claim))
        .map(ResourceExt::name_any))
}

/// Whether `pod` has a volume backed by `claim`.
fn uses_claim(pod: &Pod, claim: &str) -> bool {
    pod.spec
        .as_ref()
        .and_then(|s| s.volumes.as_ref())
        .is_some_and(|vs| {
            vs.iter().any(|v| {
                v.persistent_volume_claim
                    .as_ref()
                    .is_some_and(|c| c.claim_name == claim)
            })
        })
}

/// Whether an importer container in `statuses` exited 0.
fn importer_exited_0(statuses: Option<&Vec<ContainerStatus>>) -> bool {
    statuses.is_some_and(|statuses| {
        statuses.iter().any(|c| {
            c.name == anchor::IMPORT_CONTAINER
                && c.state
                    .as_ref()
                    .and_then(|s| s.terminated.as_ref())
                    .is_some_and(|t| t.exit_code == 0)
        })
    })
}

/// Whether the import Pod `pod` imported `url` into the claim
/// `claim_name` with UID `claim_uid` successfully.
///
/// Only the pod's spec counts, which is immutable and, for a pod that
/// requests the `virtualization.baryon.io/import` resource, written by
/// baryon-controller (admission policy baryon-vm-resource): the resource,
/// the mounted claim, and the URL and claim UID in the importer's
/// arguments. Annotations, which namespace users can change, do not.
fn import_pod_succeeded(pod: &Pod, url: &str, claim_name: &str, claim_uid: &str) -> bool {
    let args_ok = pod.spec.as_ref().is_some_and(|s| {
        s.containers.iter().any(|c| {
            c.name == anchor::IMPORT_CONTAINER
                && c.args.as_ref().is_some_and(|a| {
                    a.contains(&format!("--url={url}"))
                        && a.contains(&format!("--claim-uid={claim_uid}"))
                })
        })
    });
    !claim_uid.is_empty()
        && requests(pod, keys::IMPORT_RESOURCE)
        && uses_claim(pod, claim_name)
        && args_ok
        && importer_exited_0(
            pod.status
                .as_ref()
                .and_then(|s| s.container_statuses.as_ref()),
        )
}

/// Whether the import Pod `pod` imports into the claim with UID `claim_uid`
/// (its importer's `--claim-uid` argument).
fn import_pod_for_claim(pod: &Pod, claim_uid: &str) -> bool {
    let arg = format!("--claim-uid={claim_uid}");
    pod.spec.as_ref().is_some_and(|s| {
        s.containers.iter().any(|c| {
            c.name == anchor::IMPORT_CONTAINER && c.args.as_ref().is_some_and(|a| a.contains(&arg))
        })
    })
}

/// Whether a container of `pod` requests the extended resource `name`.
fn requests(pod: &Pod, name: &str) -> bool {
    pod.spec.as_ref().is_some_and(|s| {
        s.containers.iter().any(|c| {
            c.resources
                .as_ref()
                .and_then(|r| r.limits.as_ref())
                .is_some_and(|l| l.contains_key(name))
        })
    })
}

/// Whether the VM's status shows, from an earlier release, a root disk that
/// became ready after this claim was created: such a disk may have been
/// used by a guest. A StorageReady=True older than the claim belongs to a
/// claim that no longer exists.
fn legacy_ready(vm: &VirtualMachine, claim: &PersistentVolumeClaim) -> bool {
    let Some(status) = vm.status.as_ref() else {
        return false;
    };
    let same_claim = status.root_disk_claim.as_deref() == claim.metadata.name.as_deref();
    let ready_since = baryon_api::conditions::find(
        &status.conditions,
        baryon_api::conditions::vm::STORAGE_READY,
    )
    .filter(|c| c.status == baryon_api::conditions::TRUE)
    .map(|c| c.last_transition_time.0);
    let created = claim.metadata.creation_timestamp.as_ref().map(|t| t.0);
    same_claim && matches!((ready_since, created), (Some(r), Some(c)) if r >= c)
}

/// Upper bound of `identityConflictNodes`.
const MAX_CONFLICT_NODES: usize = 16;

/// The nodes in the VM's `status.identityConflictNodes`, read from the API
/// server, not from the cache. The list accumulates across reconciles, and a
/// reconcile that a pod event triggers can run on a cached VirtualMachine
/// that does not have the last status write yet; deriving the status from
/// that copy would drop a node recorded a moment earlier.
async fn recorded_conflict_nodes(
    vm: &VirtualMachine,
    ctx: &Context,
    ns: &str,
) -> Result<Vec<String>, Error> {
    let api: Api<VirtualMachine> = Api::namespaced(ctx.client.clone(), ns);
    let current = api.get(&vm.name_any()).await?;
    // A VirtualMachine deleted and created again under the same name starts
    // over.
    if current.metadata.uid != vm.metadata.uid {
        return Ok(Vec::new());
    }
    Ok(current
        .status
        .map(|s| s.identity_conflict_nodes)
        .unwrap_or_default())
}

/// The node of `pod` if its anchor ended because baryon-virtd refused the
/// VM there for a host identity conflict: the anchor's exit code and its
/// termination message both say so, since other processes of the container
/// can write the message (ADR 0015).
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
        .filter_map(|s| s.terminated.as_ref())
        .filter(|t| t.exit_code == i32::from(baryon_runtime_api::IDENTITY_CONFLICT_EXIT_CODE))
        .filter_map(|t| t.message.as_deref())
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
        let pod = |code: i32, msg: &str| Pod {
            spec: Some(PodSpec {
                node_name: Some("node2".into()),
                ..Default::default()
            }),
            status: Some(PodStatus {
                container_statuses: Some(vec![ContainerStatus {
                    name: anchor::ANCHOR_CONTAINER.into(),
                    last_state: Some(ContainerState {
                        terminated: Some(ContainerStateTerminated {
                            exit_code: code,
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
        let conflict = i32::from(baryon_runtime_api::IDENTITY_CONFLICT_EXIT_CODE);
        assert_eq!(
            identity_conflict_node(&pod(conflict, "IdentityConflict: another VM on this node")),
            Some("node2".to_owned())
        );
        assert_eq!(
            identity_conflict_node(&pod(conflict, "VmmKilled: signal 9")),
            None
        );
        // The message alone, as any process of the container could write it.
        assert_eq!(
            identity_conflict_node(&pod(1, "IdentityConflict: another VM on this node")),
            None
        );
        assert_eq!(
            identity_conflict_node(&pod(137, "IdentityConflict: another VM on this node")),
            None
        );
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

    fn exited(code: i32) -> ContainerStatus {
        ContainerStatus {
            name: "import".into(),
            state: Some(k8s_openapi::api::core::v1::ContainerState {
                terminated: Some(k8s_openapi::api::core::v1::ContainerStateTerminated {
                    exit_code: code,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn an_import_counts_only_for_the_url_it_imported() {
        use k8s_openapi::api::core::v1::{
            Container, PersistentVolumeClaimVolumeSource, PodSpec, PodStatus, ResourceRequirements,
            Volume,
        };
        use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
        let pod = |slot: &str, claim: &str, url: &str, claim_uid: &str, code: i32| Pod {
            spec: Some(PodSpec {
                containers: vec![Container {
                    name: "import".into(),
                    args: Some(vec![
                        "import".into(),
                        format!("--url={url}"),
                        format!("--claim-uid={claim_uid}"),
                    ]),
                    resources: Some(ResourceRequirements {
                        limits: Some([(slot.to_owned(), Quantity("1".into()))].into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                volumes: Some(vec![Volume {
                    name: "root-disk".into(),
                    persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                        claim_name: claim.into(),
                        read_only: None,
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            status: Some(PodStatus {
                container_statuses: Some(vec![exited(code)]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let a = "https://a/d.raw";
        let ok = |p: &Pod| import_pod_succeeded(p, a, "web-root", "claim-1");
        assert!(ok(&pod(keys::IMPORT_RESOURCE, "web-root", a, "claim-1", 0)));
        // another URL, a failed importer, another claim UID, another claim
        assert!(!ok(&pod(
            keys::IMPORT_RESOURCE,
            "web-root",
            "https://b/d.raw",
            "claim-1",
            0
        )));
        assert!(!ok(&pod(
            keys::IMPORT_RESOURCE,
            "web-root",
            a,
            "claim-1",
            1
        )));
        assert!(!ok(&pod(
            keys::IMPORT_RESOURCE,
            "web-root",
            a,
            "claim-0",
            0
        )));
        assert!(!ok(&pod(keys::IMPORT_RESOURCE, "db-root", a, "claim-1", 0)));
        // a pod without the controller-only import slot (made by someone else)
        assert!(!ok(&pod("cpu", "web-root", a, "claim-1", 0)));
        // annotations play no part: an otherwise identical pod is the same
        let mut annotated = pod(keys::IMPORT_RESOURCE, "web-root", a, "claim-1", 0);
        annotated.metadata.annotations = Some(
            [(
                anchor::IMPORT_APPROVED_ANNOTATION.to_owned(),
                "claim-9".to_owned(),
            )]
            .into(),
        );
        assert!(ok(&annotated));
        assert!(requests(&annotated, keys::IMPORT_RESOURCE));
        assert!(!requests(&annotated, keys::VM_RESOURCE));
    }

    #[test]
    fn legacy_readiness_counts_only_for_the_current_claim() {
        let vm = |claim: &str, ready_at: i64| -> VirtualMachine {
            serde_json::from_value(serde_json::json!({
                "apiVersion": "virtualization.baryon.io/v1alpha1",
                "kind": "VirtualMachine",
                "metadata": {"name": "web", "namespace": "default", "uid": "vm-1"},
                "spec": {"className": "small", "rootDisk": {"size": "4Gi", "source": {"http": {"url": "https://a/d.raw"}}}},
                "status": {
                    "rootDiskClaim": claim,
                    "conditions": [{"type": "StorageReady", "status": "True", "reason": "DiskReady", "message": "",
                        "lastTransitionTime": k8s_openapi::jiff::Timestamp::from_second(ready_at).unwrap().to_string()}]
                }
            }))
            .unwrap()
        };
        let claim = PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some("web-root".into()),
                creation_timestamp: Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                    k8s_openapi::jiff::Timestamp::from_second(100).unwrap(),
                )),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(legacy_ready(&vm("web-root", 150), &claim));
        // ready before this claim existed: it was another claim
        assert!(!legacy_ready(&vm("web-root", 50), &claim));
        // status about another claim
        assert!(!legacy_ready(&vm("db-root", 150), &claim));
    }

    #[test]
    fn finds_pods_that_use_a_claim() {
        let pod = |claim: &str| Pod {
            spec: Some(k8s_openapi::api::core::v1::PodSpec {
                volumes: Some(vec![k8s_openapi::api::core::v1::Volume {
                    name: "root-disk".into(),
                    persistent_volume_claim: Some(
                        k8s_openapi::api::core::v1::PersistentVolumeClaimVolumeSource {
                            claim_name: claim.into(),
                            read_only: None,
                        },
                    ),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(uses_claim(&pod("web-root"), "web-root"));
        assert!(!uses_claim(&pod("db-root"), "web-root"));
        assert!(!uses_claim(&Pod::default(), "web-root"));
    }

    #[test]
    fn the_import_status_is_kept_for_the_same_claim_only() {
        let vm = |imported: Option<&str>| -> VirtualMachine {
            let mut v = serde_json::json!({
                "apiVersion": "virtualization.baryon.io/v1alpha1",
                "kind": "VirtualMachine",
                "metadata": {"name": "web", "namespace": "default", "uid": "vm-1"},
                "spec": {"className": "small", "rootDisk": {"size": "4Gi", "source": {"http": {"url": "https://a/d.raw"}}}}
            });
            if let Some(uid) = imported {
                v["status"] = serde_json::json!({"rootDiskImport": {"claimUID": uid}});
            }
            serde_json::from_value(v).unwrap()
        };
        let claim = |uid: &str| PersistentVolumeClaim {
            metadata: ObjectMeta {
                uid: Some(uid.into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let uid = |r: Option<RootDiskImportStatus>| r.map(|r| r.claim_uid);
        // A completed import sets it.
        assert_eq!(
            uid(root_disk_import(
                &vm(None),
                Some("c1".into()),
                Some(&claim("c1"))
            )),
            Some("c1".into())
        );
        // Kept while the claim is the same object, also when nothing else
        // proves the import.
        assert_eq!(
            uid(root_disk_import(&vm(Some("c1")), None, Some(&claim("c1")))),
            Some("c1".into())
        );
        assert_eq!(
            uid(root_disk_import(&vm(Some("c1")), None, None)),
            Some("c1".into())
        );
        // A recreated claim starts over.
        assert_eq!(
            uid(root_disk_import(&vm(Some("c1")), None, Some(&claim("c2")))),
            None
        );
    }

    #[test]
    fn an_import_pod_for_another_claim_is_stale() {
        let p = Pod {
            spec: Some(k8s_openapi::api::core::v1::PodSpec {
                containers: vec![k8s_openapi::api::core::v1::Container {
                    name: "import".into(),
                    args: Some(vec!["import".into(), "--claim-uid=claim-1".into()]),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(import_pod_for_claim(&p, "claim-1"));
        assert!(!import_pod_for_claim(&p, "claim-2"));
        assert!(!import_pod_for_claim(&Pod::default(), "claim-1"));
    }

    #[test]
    fn claim_requests_add_room_only_for_filesystem_claims() {
        let gib = 1u64 << 30;
        assert_eq!(claim_request_bytes(4 * gib, "Block"), 4 * gib);
        assert_eq!(
            claim_request_bytes(4 * gib, "Filesystem"),
            4 * gib + (64 << 20)
        );
        // Rounded up to MiB.
        assert_eq!(claim_request_bytes(gib + 1, "Block"), gib + (1 << 20));
    }
}
