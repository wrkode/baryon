//! Whether the root disk of a VM with an HTTP image source must be imported
//! (ADR 0006).
//!
//! An import overwrites the disk, so it must happen exactly once, before any
//! guest runs on it. The authoritative record of a completed import is an
//! annotation on the bound PersistentVolume: the PV has the lifetime of the
//! data, it is bound to one claim UID, and namespace users cannot write
//! PersistentVolumes. Claim metadata, which namespace users can edit, is not
//! trusted for this decision.
//!
//! Without a record, an import is allowed only into a volume that is
//! provably fresh: dynamically provisioned for this claim after the claim
//! was created. Anything else (a static or pre-bound PV, a restored or
//! cloned claim, a disk that was ready before but lost its record) is
//! refused, never overwritten.

use k8s_openapi::api::core::v1::{PersistentVolume, PersistentVolumeClaim};
use kube::ResourceExt;
use serde::{Deserialize, Serialize};

/// Annotation on the PersistentVolume recording a completed import.
pub(crate) const RECORD_ANNOTATION: &str = "virtualization.baryon.io/disk-import";

/// Annotation that dynamic provisioners set on the PersistentVolumes they
/// create (Kubernetes `pv.kubernetes.io/provisioned-by`).
const PROVISIONED_BY: &str = "pv.kubernetes.io/provisioned-by";

/// Reasons of [`Decision::Refuse`], and of a legacy record that cannot be
/// written yet: the disk is not ready and is looked at again soon.
pub(crate) const REFUSALS: &[&str] = &[
    "DiskNotProvisionedForClaim",
    "DiskInitializedForAnotherClaim",
    "DiskSourceMismatch",
    "DiskImportRecordInvalid",
    "ImportStateUnknown",
    "DiskBindingPending",
];

/// The import record on a PersistentVolume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportRecord {
    /// UID of the VirtualMachine the disk was imported for.
    #[serde(rename = "vmUID")]
    pub vm_uid: String,
    /// UID of the claim the PV was bound to at import.
    #[serde(rename = "claimUID")]
    pub claim_uid: String,
    /// `sha256:` digest of the image source URL.
    pub source: String,
}

impl ImportRecord {
    pub(crate) fn to_json(&self) -> String {
        // Serializing three strings cannot fail.
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// What to do with the root disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// The PV records this import; the disk is ready.
    Imported,
    /// The import was completed by an earlier release, which recorded it on
    /// the claim; write the PV record, then treat it as imported.
    RecordLegacy,
    /// Import. `approved`: the bound PV is fresh, so the importer may write;
    /// when false the claim is not bound yet and the PV cannot be checked.
    Import { approved: bool },
    /// Do not import and do not start: (reason, message).
    Refuse(String, String),
}

/// Inputs for [`decide`].
#[derive(Debug)]
pub(crate) struct Facts<'a> {
    pub claim: &'a PersistentVolumeClaim,
    /// The bound PersistentVolume, if the claim is bound.
    pub pv: Option<&'a PersistentVolume>,
    /// `sha256:` digest of the current source URL.
    pub source: &'a str,
    pub url: &'a str,
    /// The claim annotation an earlier release wrote, if any.
    pub legacy_claim_record: Option<&'a str>,
    /// The VM's controller-written status records a completed import into
    /// this claim (`status.rootDiskImport`), or, for VMs of earlier
    /// releases, shows StorageReady=True. Either means a guest may have
    /// used the disk.
    pub was_ready: bool,
    pub block_mode: bool,
}

const ADOPT_HINT: &str = "to use existing data, reference the claim with rootDisk.source.persistentVolumeClaim in a new VirtualMachine";

pub(crate) fn decide(f: &Facts<'_>) -> Decision {
    let claim_uid = f.claim.metadata.uid.as_deref().unwrap_or_default();
    let spec = f.claim.spec.as_ref();
    if spec.is_some_and(|s| s.data_source.is_some() || s.data_source_ref.is_some()) {
        return Decision::Refuse(
            "DiskNotProvisionedForClaim".to_owned(),
            format!(
                "claim {} has a data source; Baryon imports only into empty volumes it provisioned; {ADOPT_HINT}",
                f.claim.name_any()
            ),
        );
    }
    let Some(pv) = f.pv else {
        return Decision::Import { approved: false };
    };
    match record_of(pv) {
        Some(Ok(r)) if r.claim_uid != claim_uid => {
            return Decision::Refuse(
                "DiskInitializedForAnotherClaim".to_owned(),
                format!(
                    "volume {} holds an image imported for another claim; it is not imported again; {ADOPT_HINT}",
                    pv.name_any()
                ),
            );
        }
        Some(Ok(r)) if r.source != f.source => {
            return Decision::Refuse(
                "DiskSourceMismatch".to_owned(),
                format!(
                    "volume {} holds an image from another source; it is not imported again",
                    pv.name_any()
                ),
            );
        }
        Some(Ok(_)) => return Decision::Imported,
        Some(Err(e)) => {
            return Decision::Refuse(
                "DiskImportRecordInvalid".to_owned(),
                format!("volume {}: unreadable import record: {e}", pv.name_any()),
            );
        }
        None => {}
    }
    // No record on the PV.
    let legacy_match = f
        .legacy_claim_record
        .is_some_and(|v| v == f.source || v == f.url);
    if f.was_ready && legacy_match {
        return Decision::RecordLegacy;
    }
    if f.was_ready && f.block_mode {
        // A block disk that was ready has been used by a guest; without a
        // record nothing proves the import, so nothing may overwrite it. A
        // filesystem disk is still protected by the importer's marker file.
        return Decision::Refuse(
            "ImportStateUnknown".to_owned(),
            format!(
                "the disk was ready before but volume {} has no import record; it is not imported again; {ADOPT_HINT}",
                pv.name_any()
            ),
        );
    }
    if fresh_for(pv, f.claim) {
        Decision::Import { approved: true }
    } else {
        Decision::Refuse(
            "DiskNotProvisionedForClaim".to_owned(),
            format!(
                "volume {} was not dynamically provisioned for claim {}; Baryon imports only into volumes provisioned for the claim it created; {ADOPT_HINT}",
                pv.name_any(),
                f.claim.name_any()
            ),
        )
    }
}

/// Whether `pv` and `claim` are bound to each other: the claim is Bound to
/// this volume, and the volume's claimRef names this claim (namespace,
/// name, and UID) and is Bound. An import record is written only then.
pub(crate) fn bound_to(pv: &PersistentVolume, claim: &PersistentVolumeClaim) -> bool {
    let claim_bound = claim.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Bound")
        && claim.spec.as_ref().and_then(|s| s.volume_name.as_deref())
            == pv.metadata.name.as_deref();
    let pv_bound = pv.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Bound");
    let reference = pv.spec.as_ref().and_then(|s| s.claim_ref.as_ref());
    let names_claim = reference.is_some_and(|r| {
        r.uid.is_some()
            && r.uid == claim.metadata.uid
            && r.name == claim.metadata.name
            && r.namespace == claim.metadata.namespace
    });
    claim_bound && pv_bound && names_claim
}

/// The import record of `pv`: None without one, Err if unreadable.
fn record_of(pv: &PersistentVolume) -> Option<Result<ImportRecord, String>> {
    pv.annotations()
        .get(RECORD_ANNOTATION)
        .map(|v| serde_json::from_str(v).map_err(|e| e.to_string()))
}

/// Whether `pv` was dynamically provisioned for `claim`: it carries the
/// provisioner annotation, is bound to this claim's UID, and was created
/// no earlier than the claim.
fn fresh_for(pv: &PersistentVolume, claim: &PersistentVolumeClaim) -> bool {
    let provisioned = pv.annotations().contains_key(PROVISIONED_BY);
    let bound_to = pv
        .spec
        .as_ref()
        .and_then(|s| s.claim_ref.as_ref())
        .and_then(|r| r.uid.as_deref());
    let created_after = match (
        pv.metadata.creation_timestamp.as_ref(),
        claim.metadata.creation_timestamp.as_ref(),
    ) {
        (Some(p), Some(c)) => p.0 >= c.0,
        _ => false,
    };
    provisioned && bound_to.is_some() && bound_to == claim.metadata.uid.as_deref() && created_after
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{
        ObjectReference, PersistentVolumeClaimSpec, PersistentVolumeSpec, TypedLocalObjectReference,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
    use k8s_openapi::jiff::Timestamp;
    use kube::api::ObjectMeta;

    const URL: &str = "https://example.org/d.raw";

    fn t(s: i64) -> Time {
        Time(Timestamp::from_second(s).unwrap())
    }

    fn claim() -> PersistentVolumeClaim {
        PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some("web-root".into()),
                uid: Some("claim-1".into()),
                creation_timestamp: Some(t(100)),
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec::default()),
            ..Default::default()
        }
    }

    fn pv(
        claim_uid: &str,
        created: i64,
        provisioned: bool,
        record: Option<&ImportRecord>,
    ) -> PersistentVolume {
        let mut ann = std::collections::BTreeMap::new();
        if provisioned {
            ann.insert(PROVISIONED_BY.to_owned(), "csi.example".to_owned());
        }
        if let Some(r) = record {
            ann.insert(RECORD_ANNOTATION.to_owned(), r.to_json());
        }
        PersistentVolume {
            metadata: ObjectMeta {
                name: Some("pvc-1".into()),
                creation_timestamp: Some(t(created)),
                annotations: Some(ann),
                ..Default::default()
            },
            spec: Some(PersistentVolumeSpec {
                claim_ref: Some(ObjectReference {
                    uid: Some(claim_uid.into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn record(claim_uid: &str, source: &str) -> ImportRecord {
        ImportRecord {
            vm_uid: "vm-1".into(),
            claim_uid: claim_uid.into(),
            source: source.into(),
        }
    }

    fn facts<'a>(
        c: &'a PersistentVolumeClaim,
        p: Option<&'a PersistentVolume>,
        src: &'a str,
    ) -> Facts<'a> {
        Facts {
            claim: c,
            pv: p,
            source: src,
            url: URL,
            legacy_claim_record: None,
            was_ready: false,
            block_mode: true,
        }
    }

    fn reason(d: Decision) -> String {
        match d {
            Decision::Refuse(r, _) => r,
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn a_fresh_volume_is_imported_once() {
        let c = claim();
        assert_eq!(
            decide(&facts(&c, None, "sha256:a")),
            Decision::Import { approved: false }
        );
        let fresh = pv("claim-1", 101, true, None);
        assert_eq!(
            decide(&facts(&c, Some(&fresh), "sha256:a")),
            Decision::Import { approved: true }
        );
        let done = pv("claim-1", 101, true, Some(&record("claim-1", "sha256:a")));
        assert_eq!(
            decide(&facts(&c, Some(&done), "sha256:a")),
            Decision::Imported
        );
    }

    #[test]
    fn claim_metadata_does_not_decide() {
        // A completed import stays completed whatever the claim says, and a
        // claim annotation alone never skips or repeats an import.
        let c = claim();
        let done = pv("claim-1", 101, true, Some(&record("claim-1", "sha256:a")));
        let mut f = facts(&c, Some(&done), "sha256:a");
        f.legacy_claim_record = None;
        assert_eq!(decide(&f), Decision::Imported);
        let fresh = pv("claim-1", 101, true, None);
        let mut f = facts(&c, Some(&fresh), "sha256:a");
        f.legacy_claim_record = Some("sha256:a"); // set by a tenant, VM never ready
        assert_eq!(decide(&f), Decision::Import { approved: true });
    }

    #[test]
    fn records_for_another_claim_or_source_refuse() {
        let c = claim();
        let other = pv("claim-1", 101, true, Some(&record("claim-0", "sha256:a")));
        assert_eq!(
            reason(decide(&facts(&c, Some(&other), "sha256:a"))),
            "DiskInitializedForAnotherClaim"
        );
        let src = pv("claim-1", 101, true, Some(&record("claim-1", "sha256:b")));
        assert_eq!(
            reason(decide(&facts(&c, Some(&src), "sha256:a"))),
            "DiskSourceMismatch"
        );
        let mut bad = pv("claim-1", 101, true, None);
        bad.metadata
            .annotations
            .get_or_insert_default()
            .insert(RECORD_ANNOTATION.into(), "{".into());
        assert_eq!(
            reason(decide(&facts(&c, Some(&bad), "sha256:a"))),
            "DiskImportRecordInvalid"
        );
    }

    #[test]
    fn volumes_not_provisioned_for_the_claim_refuse() {
        let c = claim();
        // static PV (no provisioner annotation)
        let stat = pv("claim-1", 101, false, None);
        assert_eq!(
            reason(decide(&facts(&c, Some(&stat), "sha256:a"))),
            "DiskNotProvisionedForClaim"
        );
        // older than the claim (pre-bound)
        let old = pv("claim-1", 50, true, None);
        assert_eq!(
            reason(decide(&facts(&c, Some(&old), "sha256:a"))),
            "DiskNotProvisionedForClaim"
        );
        // bound to another claim UID
        let elsewhere = pv("claim-9", 101, true, None);
        assert_eq!(
            reason(decide(&facts(&c, Some(&elsewhere), "sha256:a"))),
            "DiskNotProvisionedForClaim"
        );
        // claim with a data source (clone, snapshot, populator)
        let mut cloned = claim();
        if let Some(s) = cloned.spec.as_mut() {
            s.data_source = Some(TypedLocalObjectReference {
                kind: "PersistentVolumeClaim".into(),
                name: "src".into(),
                api_group: None,
            });
        }
        assert_eq!(
            reason(decide(&facts(&cloned, None, "sha256:a"))),
            "DiskNotProvisionedForClaim"
        );
    }

    #[test]
    fn legacy_records_migrate_and_fail_closed() {
        let c = claim();
        let fresh = pv("claim-1", 101, true, None);
        // An earlier release recorded the import on the claim and the VM was
        // ready: record it on the PV.
        for legacy in ["sha256:a", URL] {
            let mut f = facts(&c, Some(&fresh), "sha256:a");
            f.legacy_claim_record = Some(legacy);
            f.was_ready = true;
            assert_eq!(decide(&f), Decision::RecordLegacy);
        }
        // The VM was ready but the record is gone: a block disk is refused.
        let mut f = facts(&c, Some(&fresh), "sha256:a");
        f.was_ready = true;
        assert_eq!(reason(decide(&f)), "ImportStateUnknown");
        // A filesystem disk is protected by the marker file: import Pod runs.
        f.block_mode = false;
        assert_eq!(decide(&f), Decision::Import { approved: true });
    }

    #[test]
    fn record_json_uses_kubernetes_field_names() {
        let json = record("claim-1", "sha256:a").to_json();
        assert_eq!(
            json,
            r#"{"vmUID":"vm-1","claimUID":"claim-1","source":"sha256:a"}"#
        );
        let back: ImportRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, record("claim-1", "sha256:a"));
    }

    #[test]
    fn a_record_needs_a_mutual_binding() {
        use k8s_openapi::api::core::v1::{PersistentVolumeClaimStatus, PersistentVolumeStatus};
        let mut c = claim();
        c.metadata.namespace = Some("default".into());
        c.spec.get_or_insert_default().volume_name = Some("pvc-1".into());
        c.status = Some(PersistentVolumeClaimStatus {
            phase: Some("Bound".into()),
            ..Default::default()
        });
        let mut p = pv("claim-1", 101, true, None);
        if let Some(r) = p.spec.as_mut().and_then(|s| s.claim_ref.as_mut()) {
            r.name = Some("web-root".into());
            r.namespace = Some("default".into());
        }
        p.status = Some(PersistentVolumeStatus {
            phase: Some("Bound".into()),
            ..Default::default()
        });
        assert!(bound_to(&p, &c));
        // A claim pointing at a volume that does not point back.
        let mut other = p.clone();
        if let Some(r) = other.spec.as_mut().and_then(|s| s.claim_ref.as_mut()) {
            r.namespace = Some("victim".into());
        }
        assert!(!bound_to(&other, &c));
        // A claim that is not Bound (spec.volumeName set by a user).
        let mut pending = c.clone();
        pending.status = None;
        assert!(!bound_to(&p, &pending));
        // A volume that is not Bound.
        let mut released = p.clone();
        released.status = None;
        assert!(!bound_to(&released, &c));
    }
}
