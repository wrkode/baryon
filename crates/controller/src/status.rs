//! VirtualMachine status, derived from the Anchor Pod and the root claim.
//!
//! The Anchor Pod is the single runtime status channel (ADR 0008):
//! kubelet reports the anchor container's readiness (guest boot evidence),
//! its restarts (VM restarts), and its termination message (the reason the
//! VM stopped, written by baryon-anchor from baryon-virtd's status). The
//! controller is the only writer of VirtualMachine status.

use k8s_openapi::api::core::v1::{ContainerStatus, PersistentVolumeClaim, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;

use baryon_api::conditions::{self, ConditionUpdate, vm};
use baryon_api::vm::{RunPolicy, VmAddress};
use baryon_api::{VirtualMachine, VirtualMachineStatus};

/// What the controller observed for one VM.
#[derive(Debug, Default)]
pub(crate) struct Observed<'a> {
    /// Class or spec problem that prevents the VM from starting.
    pub blocked: Option<(String, String)>,
    pub claim_name: Option<String>,
    pub claim: Option<&'a PersistentVolumeClaim>,
    pub pod: Option<&'a Pod>,
    /// The import Pod, while an import is pending.
    pub import_pod: Option<&'a Pod>,
    pub has_import: bool,
    pub restart_reason: Option<RestartReason>,
    pub mac_address: Option<String>,
    /// Nodes to report in `identityConflictNodes`.
    pub identity_conflict_nodes: Vec<String>,
    /// The disk is not usable and will not be imported: (reason, message).
    pub storage_blocked: Option<(String, String)>,
    /// Why the root disk claim could not be resolved (`ClaimNotFound`,
    /// `ClaimConflict`, `ClaimRejected`, `InvalidSpec`), whatever else
    /// blocks the VM.
    pub disk_error: Option<(String, String)>,
    /// The VirtualizationNode of the node the Anchor Pod runs on.
    pub node: Option<&'a baryon_api::VirtualizationNode>,
    /// Whether the controller saw that node's heartbeat recently.
    pub node_fresh: bool,
    /// `status.rootDiskImport` to report.
    pub root_disk_import: Option<baryon_api::vm::RootDiskImportStatus>,
}

/// Why the running Anchor Pod differs from what the controller would create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestartReason {
    /// The VM spec or its class changed.
    Spec,
    /// baryon-controller now uses a different anchor image.
    AnchorImage,
    /// The VM's scheduling constraints changed.
    Placement,
}

fn container<'a>(
    statuses: Option<&'a Vec<ContainerStatus>>,
    name: &str,
) -> Option<&'a ContainerStatus> {
    statuses?.iter().find(|c| c.name == name)
}

/// Termination message and exit code of the last run of a container.
fn last_termination(c: &ContainerStatus) -> Option<(i32, String)> {
    let t = c
        .state
        .as_ref()
        .and_then(|s| s.terminated.as_ref())
        .or_else(|| c.last_state.as_ref().and_then(|s| s.terminated.as_ref()))?;
    Some((
        t.exit_code,
        t.message.clone().unwrap_or_default().trim().to_owned(),
    ))
}

/// `Reason: detail` messages written by baryon-anchor.
fn split_reason(msg: &str) -> (String, String) {
    match msg.split_once(": ") {
        Some((r, d)) if !r.is_empty() && r.chars().all(|c| c.is_ascii_alphanumeric()) => {
            (r.to_owned(), d.to_owned())
        }
        _ => ("VmStopped".to_owned(), msg.to_owned()),
    }
}

/// NetworkReady of a running VM whose Anchor Pod `pod_uid` runs on the node
/// `node`: the host-side guest network service state that baryon-virtd
/// reports through the node agent. Unknown when the node's report is
/// missing, stale (`fresh` false), or from a runtime that does not report
/// it. The reason comes from the reported state, not from the node; the
/// node's message is length-capped.
pub(crate) fn network_condition(
    node: Option<&baryon_api::VirtualizationNode>,
    fresh: bool,
    pod_uid: &str,
) -> ConditionUpdate {
    let unknown = |reason: &str, message: String| {
        ConditionUpdate::unknown(vm::NETWORK_READY, reason, message)
    };
    let Some(status) = node.and_then(|n| n.status.as_ref()) else {
        return unknown(
            "RuntimeUnreachable",
            "the node reports no host state".to_owned(),
        );
    };
    if !fresh {
        return unknown(
            "RuntimeUnreachable",
            "the node's Baryon runtime has not reported for 90 s; the state of the guest network service is unknown".to_owned(),
        );
    }
    match conditions::find(&status.conditions, conditions::node::RUNTIME_HEALTHY) {
        Some(c) if c.status == conditions::TRUE => {}
        c => {
            return unknown(
                "RuntimeUnreachable",
                capped(format!(
                    "the node's Baryon runtime is not healthy ({}); the state of the guest network service is unknown",
                    c.map_or("not reported", |c| c.reason.as_str())
                )),
            );
        }
    }
    let reports = status
        .runtime
        .as_ref()
        .and_then(|r| r.protocol_version.as_deref())
        .and_then(|v| v.split_once('.'))
        .and_then(|(major, minor)| Some((major.parse::<u32>().ok()?, minor.parse::<u32>().ok()?)))
        .is_some_and(|v| v >= (1, 2));
    if !reports {
        return unknown(
            "RuntimeDoesNotReport",
            "the node's baryon-virtd does not report the guest network service (runtime protocol before 1.2)".to_owned(),
        );
    }
    let Some(net) = status
        .vms
        .iter()
        .find(|v| v.anchor_pod_uid == pod_uid)
        .and_then(|v| v.network.as_ref())
    else {
        return unknown(
            "NotReported",
            "the node has not reported this VM yet".to_owned(),
        );
    };
    let mut message = net.message.clone();
    if let Some(t) = &net.lease_expires_time
        && net.state != "Ready"
    {
        message = format!("{message} (guest lease valid until {})", t.0);
    }
    let message = capped(message);
    match net.state.as_str() {
        "Ready" => ConditionUpdate::new(vm::NETWORK_READY, true, "AddressServiceRunning", message),
        "Restarting" => {
            ConditionUpdate::new(vm::NETWORK_READY, false, "DhcpResponderRestarting", message)
        }
        "Failing" => {
            ConditionUpdate::new(vm::NETWORK_READY, false, "DhcpResponderFailing", message)
        }
        _ => unknown(
            "NotReported",
            "the node reports an unknown state".to_owned(),
        ),
    }
}

/// Longest scheduler message copied into a condition; the full message is
/// on the pod.
const SCHEDULER_MESSAGE_MAX: usize = 1024;

/// `msg` cut to at most [`SCHEDULER_MESSAGE_MAX`] bytes, including the
/// " ..." marker, at a character boundary. Also the limit of an event note.
pub(crate) fn capped(msg: String) -> String {
    if msg.len() <= SCHEDULER_MESSAGE_MAX {
        return msg;
    }
    let mut end = SCHEDULER_MESSAGE_MAX - 4;
    while !msg.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} ...", &msg[..end])
}

/// The PodScheduled reason and message of a pod that is not bound to a
/// node, if the scheduler reported one.
fn unscheduled(pod: &Pod) -> Option<(String, String)> {
    if pod
        .spec
        .as_ref()
        .and_then(|s| s.node_name.as_ref())
        .is_some()
    {
        return None;
    }
    let c = pod
        .status
        .as_ref()?
        .conditions
        .as_ref()?
        .iter()
        .find(|c| c.type_ == "PodScheduled" && c.status == "False")?;
    Some((
        c.reason.clone().unwrap_or_else(|| "Pending".to_owned()),
        capped(c.message.clone().unwrap_or_default()),
    ))
}

pub(crate) fn derive(vm: &VirtualMachine, obs: &Observed<'_>, now: Time) -> VirtualMachineStatus {
    let previous = vm.status.clone().unwrap_or_default();
    let generation = vm.metadata.generation;
    let mut updates: Vec<ConditionUpdate> = Vec::new();
    let mut status = VirtualMachineStatus {
        observed_generation: generation,
        root_disk_claim: obs.claim_name.clone(),
        mac_address: obs.mac_address.clone(),
        identity_conflict_nodes: obs.identity_conflict_nodes.clone(),
        root_disk_import: obs.root_disk_import.clone(),
        ..Default::default()
    };

    // Storage.
    let claim_bound = obs
        .claim
        .and_then(|c| c.status.as_ref())
        .and_then(|s| s.phase.as_deref())
        == Some("Bound");
    // Progress of the importer in the import Pod baryon-controller created.
    // A completed import is reported once it is recorded (has_import is
    // then false); an exit code alone never makes the disk ready.
    let import = obs.import_pod.and_then(|p| {
        container(
            p.status
                .as_ref()
                .and_then(|s| s.container_statuses.as_ref()),
            "import",
        )
    });
    let storage = if let Some((reason, message)) = &obs.storage_blocked {
        ConditionUpdate::new(vm::STORAGE_READY, false, reason.clone(), message.clone())
    } else if obs.claim.is_none() {
        // A claim problem found while resolving the disk (refused, owned by
        // something else, missing) is the storage state too.
        match obs.disk_error.as_ref().filter(|(r, _)| r != "InvalidSpec") {
            Some((reason, message)) => {
                ConditionUpdate::new(vm::STORAGE_READY, false, reason.clone(), message.clone())
            }
            None => ConditionUpdate::new(
                vm::STORAGE_READY,
                false,
                "ClaimMissing",
                "the root disk claim does not exist",
            ),
        }
    } else if !claim_bound {
        ConditionUpdate::new(
            vm::STORAGE_READY,
            false,
            "ClaimPending",
            "the root disk claim is not bound yet (WaitForFirstConsumer claims bind when the import or Anchor Pod is scheduled)",
        )
    } else if obs.has_import {
        match import {
            Some(c)
                if c.state
                    .as_ref()
                    .and_then(|s| s.terminated.as_ref())
                    .is_some_and(|t| t.exit_code == 0) =>
            {
                ConditionUpdate::new(
                    vm::STORAGE_READY,
                    false,
                    "ImportingDisk",
                    "the importer finished; recording the import",
                )
            }
            Some(c) => match last_termination(c) {
                // Killed without a message (for example out of memory).
                Some((code, msg)) if code != 0 && msg.is_empty() => ConditionUpdate::new(
                    vm::STORAGE_READY,
                    false,
                    "ImportFailed",
                    format!("the importer exited with code {code} without a message"),
                ),
                Some((code, msg)) if code != 0 => {
                    let (reason, detail) = split_reason(&msg);
                    ConditionUpdate::new(vm::STORAGE_READY, false, reason, detail)
                }
                _ => ConditionUpdate::new(
                    vm::STORAGE_READY,
                    false,
                    "ImportingDisk",
                    "importing the disk image",
                ),
            },
            None => ConditionUpdate::new(
                vm::STORAGE_READY,
                false,
                "ImportPending",
                "waiting for the import to start",
            ),
        }
    } else {
        ConditionUpdate::new(
            vm::STORAGE_READY,
            true,
            "DiskReady",
            "root disk claim is bound",
        )
    };
    // While the disk is not ready, it is the most useful reason for Ready.
    let storage_blocker = (storage.status != conditions::TRUE)
        .then(|| (storage.reason.clone(), storage.message.clone()));
    updates.push(storage);

    updates.push(match obs.restart_reason {
        Some(RestartReason::Spec) => ConditionUpdate::new(
            vm::RESTART_REQUIRED,
            true,
            "SpecChanged",
            "the spec or class changed; restart the VM to apply it",
        ),
        Some(RestartReason::AnchorImage) => ConditionUpdate::new(
            vm::RESTART_REQUIRED,
            true,
            "AnchorImageChanged",
            "baryon-controller uses a different anchor image; restart the VM to use it",
        ),
        Some(RestartReason::Placement) => ConditionUpdate::new(
            vm::RESTART_REQUIRED,
            true,
            "PlacementChanged",
            "the scheduling constraints changed; they apply when the VM is restarted (a VM on a node-local volume stays on that volume's node)",
        ),
        None => ConditionUpdate::new(vm::RESTART_REQUIRED, false, "UpToDate", ""),
    });

    let not_ready = |reason: &str, message: &str| {
        [
            ConditionUpdate::new(vm::RUNNING, false, reason, message),
            ConditionUpdate::new(vm::READY, false, reason, message),
        ]
    };

    // An existing Anchor Pod decides the VM's state; a class or spec problem
    // only matters for the next start (it is reported through
    // RestartRequired or once the pod is gone).
    if let Some(pod) = obs.pod {
        let pod_status = pod.status.as_ref();
        status.anchor_pod = pod.metadata.name.clone();
        status.node_name = pod.spec.as_ref().and_then(|s| s.node_name.clone());
        status.addresses = pod_status
            .and_then(|s| s.pod_ips.as_ref())
            .map(|ips| {
                ips.iter()
                    .map(|ip| VmAddress {
                        type_: "InternalIP".to_owned(),
                        address: ip.ip.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Scheduling.
        if let Some(node) = &status.node_name {
            updates.push(ConditionUpdate::new(
                vm::SCHEDULED,
                true,
                "Scheduled",
                format!("Anchor Pod bound to {node}"),
            ));
        } else {
            let sched = pod_status
                .and_then(|s| s.conditions.as_ref())
                .and_then(|c| c.iter().find(|c| c.type_ == "PodScheduled"));
            let reason = sched
                .and_then(|c| c.reason.clone())
                .unwrap_or_else(|| "Pending".to_owned());
            let message = capped(sched.and_then(|c| c.message.clone()).unwrap_or_default());
            updates.push(ConditionUpdate::new(vm::SCHEDULED, false, reason, message));
        }

        let anchor = container(
            pod_status.and_then(|s| s.container_statuses.as_ref()),
            "anchor",
        );
        status.restart_count = anchor.map(|c| c.restart_count);
        // The node lifecycle controller marks the pods of a node that stopped
        // reporting not ready (PodReady False) but leaves kubelet's last
        // container status in place. A ready container under a not-ready
        // pod is that case: the VM's state is unknown.
        let pod_ready = pod_status
            .and_then(|s| s.conditions.as_ref())
            .and_then(|c| c.iter().find(|c| c.type_ == "Ready"));
        // kubelet itself sets Ready False on a pod in a terminal phase, which
        // is not a lost node.
        let terminal = matches!(
            pod_status.and_then(|s| s.phase.as_deref()),
            Some("Succeeded" | "Failed")
        );
        let node_lost = !terminal
            && anchor.is_some_and(|c| c.ready)
            && pod_ready.is_some_and(|c| c.status == conditions::FALSE);

        if node_lost {
            let message = pod_ready
                .and_then(|c| c.message.clone())
                .filter(|m| !m.is_empty())
                .map(capped)
                .unwrap_or_else(|| {
                    "the Anchor Pod's node stopped reporting; the VM's state is unknown".to_owned()
                });
            updates.push(ConditionUpdate::unknown(
                vm::RUNNING,
                "NodeUnreachable",
                message.clone(),
            ));
            updates.push(ConditionUpdate::unknown(
                vm::READY,
                "NodeUnreachable",
                message,
            ));
        } else if pod.metadata.deletion_timestamp.is_some() {
            updates.extend(not_ready("Stopping", "the guest is shutting down"));
        } else if status.node_name.is_none() {
            let reason = updates
                .iter()
                .rev()
                .find(|u| u.type_ == vm::SCHEDULED)
                .map(|u| u.reason.clone())
                .unwrap_or_else(|| "Pending".to_owned());
            updates.extend(not_ready(
                &reason,
                "waiting for the Anchor Pod to be scheduled",
            ));
        } else {
            match anchor {
                Some(c) if c.ready => {
                    updates.push(ConditionUpdate::new(
                        vm::RUNNING,
                        true,
                        "AnchorRunning",
                        "the Anchor container is running; it runs or starts the VMM",
                    ));
                    updates.push(ConditionUpdate::new(
                        vm::READY,
                        true,
                        "GuestBooted",
                        "the guest acquired its address or showed a login prompt",
                    ));
                }
                Some(c) if c.state.as_ref().and_then(|s| s.running.as_ref()).is_some() => {
                    updates.push(ConditionUpdate::new(
                        vm::RUNNING,
                        true,
                        "AnchorRunning",
                        "the Anchor container is running; it runs or starts the VMM",
                    ));
                    let message = match (c.restart_count, last_termination(c)) {
                        (n, Some((_, msg))) if n > 0 && !msg.is_empty() => {
                            format!("booting; the previous run ended with {msg}")
                        }
                        _ => "waiting for guest boot evidence".to_owned(),
                    };
                    updates.push(ConditionUpdate::new(vm::READY, false, "Booting", message));
                }
                Some(_) if storage_blocker.is_some() => {
                    let (reason, message) = storage_blocker.clone().unwrap_or_default();
                    updates.extend(not_ready(&reason, &message));
                }
                Some(c) => {
                    let waiting = c.state.as_ref().and_then(|s| s.waiting.as_ref());
                    let (reason, detail) = match last_termination(c) {
                        Some((_, msg)) if !msg.is_empty() => split_reason(&msg),
                        _ => (
                            waiting
                                .and_then(|w| w.reason.clone())
                                .unwrap_or_else(|| "Starting".to_owned()),
                            waiting.and_then(|w| w.message.clone()).unwrap_or_default(),
                        ),
                    };
                    let backoff =
                        waiting.and_then(|w| w.reason.as_deref()) == Some("CrashLoopBackOff");
                    let detail = if backoff {
                        format!("{detail} (restart back-off)")
                    } else {
                        detail
                    };
                    updates.extend(not_ready(&reason, &detail));
                }
                None => updates.extend(not_ready(
                    "Starting",
                    "the anchor container has not started",
                )),
            }
        }
    } else if vm.spec.run_policy == RunPolicy::Stopped {
        updates.push(ConditionUpdate::new(
            vm::SCHEDULED,
            false,
            "Stopped",
            "the VM is stopped",
        ));
        updates.extend(not_ready("Stopped", "runPolicy is Stopped"));
    } else if let Some((reason, message)) = &obs.blocked {
        updates.push(ConditionUpdate::new(
            vm::SCHEDULED,
            false,
            reason.clone(),
            message.clone(),
        ));
        updates.extend(not_ready(reason, message));
    } else if let Some((reason, message)) = obs.import_pod.and_then(unscheduled) {
        // The import Pod requests the VM's resources; if it cannot be
        // placed, neither can the VM.
        updates.push(ConditionUpdate::new(
            vm::SCHEDULED,
            false,
            reason.clone(),
            message.clone(),
        ));
        updates.extend(not_ready(&reason, &message));
    } else if let Some((reason, message)) = &storage_blocker {
        // The Anchor Pod is created once the disk is ready.
        updates.push(ConditionUpdate::new(
            vm::SCHEDULED,
            false,
            reason.clone(),
            message.clone(),
        ));
        updates.extend(not_ready(reason, message));
    } else {
        updates.push(ConditionUpdate::new(
            vm::SCHEDULED,
            false,
            "Pending",
            "creating the Anchor Pod",
        ));
        updates.extend(not_ready("Pending", "creating the Anchor Pod"));
    }

    // NetworkReady: the host-side guest network service of the running VM.
    let running = updates
        .iter()
        .rev()
        .find(|u| u.type_ == vm::RUNNING)
        .is_some_and(|u| u.status == conditions::TRUE);
    let running_on = obs
        .pod
        .filter(|_| running)
        .filter(|p| p.metadata.deletion_timestamp.is_none())
        .and_then(|p| {
            Some((
                p.metadata.uid.as_deref()?,
                p.spec.as_ref()?.node_name.as_ref()?,
            ))
        });
    let node_lost = updates
        .iter()
        .any(|u| u.type_ == vm::RUNNING && u.reason == "NodeUnreachable");
    updates.push(match running_on {
        _ if node_lost => ConditionUpdate::unknown(
            vm::NETWORK_READY,
            "NodeUnreachable",
            "the Anchor Pod's node stopped reporting",
        ),
        Some((uid, _)) => network_condition(obs.node, obs.node_fresh, uid),
        None => ConditionUpdate::unknown(
            vm::NETWORK_READY,
            "NotRunning",
            "the VM is not running on a node",
        ),
    });

    status.conditions = conditions::merge(&previous.conditions, &updates, generation, now);
    status
}

/// Whether two statuses differ in anything but condition timestamps, to
/// avoid writing unchanged status.
pub(crate) fn changed(old: Option<&VirtualMachineStatus>, new: &VirtualMachineStatus) -> bool {
    let Some(old) = old else { return true };
    let strip = |s: &VirtualMachineStatus| {
        let mut s = s.clone();
        for c in &mut s.conditions {
            c.last_transition_time = Time(k8s_openapi::jiff::Timestamp::UNIX_EPOCH);
        }
        s
    };
    strip(old) != strip(new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{
        ContainerState, ContainerStateRunning, ContainerStateTerminated, ContainerStateWaiting,
        PersistentVolumeClaimStatus, PodIP, PodSpec, PodStatus,
    };
    use k8s_openapi::jiff::Timestamp;

    fn vm() -> VirtualMachine {
        serde_json::from_value(serde_json::json!({
            "apiVersion": "virtualization.baryon.io/v1alpha1",
            "kind": "VirtualMachine",
            "metadata": {"name": "web", "namespace": "default", "generation": 3},
            "spec": {"className": "small", "rootDisk": {"source": {"http": {"url": "https://x/d.raw"}}, "size": "4Gi"}}
        }))
        .unwrap()
    }

    fn now() -> Time {
        Time(Timestamp::from_second(100).unwrap())
    }

    fn bound_claim() -> PersistentVolumeClaim {
        PersistentVolumeClaim {
            status: Some(PersistentVolumeClaimStatus {
                phase: Some("Bound".into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn pod(anchor: ContainerStatus, import_done: bool) -> Pod {
        let import = ContainerStatus {
            name: "import".into(),
            state: Some(ContainerState {
                terminated: import_done.then(|| ContainerStateTerminated {
                    exit_code: 0,
                    ..Default::default()
                }),
                running: (!import_done).then(ContainerStateRunning::default),
                ..Default::default()
            }),
            ..Default::default()
        };
        Pod {
            metadata: kube::api::ObjectMeta {
                name: Some("web-anchor".into()),
                uid: Some("pod-uid-1".into()),
                ..Default::default()
            },
            spec: Some(PodSpec {
                node_name: Some("worker-1".into()),
                ..Default::default()
            }),
            status: Some(PodStatus {
                pod_ips: Some(vec![PodIP {
                    ip: "10.244.1.5".into(),
                }]),
                container_statuses: Some(vec![anchor]),
                init_container_statuses: Some(vec![import]),
                ..Default::default()
            }),
        }
    }

    fn cond<'a>(
        s: &'a VirtualMachineStatus,
        t: &str,
    ) -> &'a k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition {
        conditions::find(&s.conditions, t).unwrap()
    }

    #[test]
    fn ready_vm() {
        let claim = bound_claim();
        let p = pod(
            ContainerStatus {
                name: "anchor".into(),
                ready: true,
                state: Some(ContainerState {
                    running: Some(ContainerStateRunning::default()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            true,
        );
        let s = derive(
            &vm(),
            &Observed {
                claim: Some(&claim),
                claim_name: Some("web-root".into()),
                pod: Some(&p),
                has_import: false,
                ..Default::default()
            },
            now(),
        );
        assert_eq!(cond(&s, vm::READY).status, "True");
        assert_eq!(cond(&s, vm::STORAGE_READY).status, "True");
        assert_eq!(cond(&s, vm::SCHEDULED).status, "True");
        assert_eq!(s.node_name.as_deref(), Some("worker-1"));
        assert_eq!(s.addresses[0].address, "10.244.1.5");
        assert_eq!(s.observed_generation, Some(3));
        // A running VM on a node without a report: Unknown, not NotRunning.
        assert_eq!(cond(&s, vm::NETWORK_READY).reason, "RuntimeUnreachable");
    }

    #[test]
    fn failed_vm_reports_anchor_termination_message() {
        let claim = bound_claim();
        let p = pod(
            ContainerStatus {
                name: "anchor".into(),
                restart_count: 2,
                state: Some(ContainerState {
                    waiting: Some(ContainerStateWaiting {
                        reason: Some("CrashLoopBackOff".into()),
                        message: None,
                    }),
                    ..Default::default()
                }),
                last_state: Some(ContainerState {
                    terminated: Some(ContainerStateTerminated {
                        exit_code: 1,
                        message: Some("VmmKilled: VMM was killed by signal 9".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            true,
        );
        let s = derive(
            &vm(),
            &Observed {
                claim: Some(&claim),
                pod: Some(&p),
                has_import: false,
                ..Default::default()
            },
            now(),
        );
        let ready = cond(&s, vm::READY);
        assert_eq!(ready.status, "False");
        assert_eq!(ready.reason, "VmmKilled");
        assert!(ready.message.contains("signal 9"));
        assert!(ready.message.contains("back-off"));
        assert_eq!(s.restart_count, Some(2));
        // The Anchor Pod exists but the VM does not run.
        assert_eq!(cond(&s, vm::NETWORK_READY).reason, "NotRunning");
    }

    #[test]
    fn importing_and_stopped_and_blocked() {
        let claim = bound_claim();
        let p = pod(
            ContainerStatus {
                name: "anchor".into(),
                state: Some(ContainerState {
                    waiting: Some(ContainerStateWaiting {
                        reason: Some("PodInitializing".into()),
                        message: None,
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            false,
        );
        let s = derive(
            &vm(),
            &Observed {
                claim: Some(&claim),
                pod: Some(&p),
                has_import: true,
                ..Default::default()
            },
            now(),
        );
        // An init container of an Anchor-named pod is no evidence of an
        // import: only the import Pod reports progress.
        assert_eq!(cond(&s, vm::STORAGE_READY).reason, "ImportPending");

        let mut stopped = vm();
        stopped.spec.run_policy = RunPolicy::Stopped;
        let s = derive(
            &stopped,
            &Observed {
                claim: Some(&claim),
                ..Default::default()
            },
            now(),
        );
        assert_eq!(cond(&s, vm::READY).reason, "Stopped");

        let s = derive(
            &vm(),
            &Observed {
                blocked: Some(("ClassNotFound".into(), "no class small".into())),
                ..Default::default()
            },
            now(),
        );
        assert_eq!(cond(&s, vm::READY).reason, "ClassNotFound");
    }

    #[test]
    fn running_pod_wins_over_a_blocked_class_and_restart_reasons_differ() {
        let claim = bound_claim();
        let p = pod(
            ContainerStatus {
                name: "anchor".into(),
                ready: true,
                state: Some(ContainerState {
                    running: Some(ContainerStateRunning::default()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            true,
        );
        let s = derive(
            &vm(),
            &Observed {
                blocked: Some(("ClassNotFound".into(), "no class small".into())),
                claim: Some(&claim),
                pod: Some(&p),
                has_import: true,
                restart_reason: Some(RestartReason::AnchorImage),
                ..Default::default()
            },
            now(),
        );
        assert_eq!(cond(&s, vm::READY).status, "True");
        assert_eq!(cond(&s, vm::RUNNING).reason, "AnchorRunning");
        let restart = cond(&s, vm::RESTART_REQUIRED);
        assert_eq!(restart.status, "True");
        assert_eq!(restart.reason, "AnchorImageChanged");

        let s = derive(
            &vm(),
            &Observed {
                claim: Some(&claim),
                pod: Some(&p),
                restart_reason: Some(RestartReason::Spec),
                ..Default::default()
            },
            now(),
        );
        assert_eq!(cond(&s, vm::RESTART_REQUIRED).reason, "SpecChanged");
    }

    #[test]
    fn unchanged_status_is_detected() {
        let s = derive(&vm(), &Observed::default(), now());
        let again = derive(
            &vm(),
            &Observed::default(),
            Time(Timestamp::from_second(200).unwrap()),
        );
        assert!(!changed(Some(&s), &again));
        assert!(changed(None, &s));
    }

    fn import_pod(state: ContainerState, last: Option<ContainerState>) -> Pod {
        Pod {
            metadata: kube::api::ObjectMeta {
                name: Some("web-import".into()),
                ..Default::default()
            },
            spec: Some(PodSpec::default()),
            status: Some(PodStatus {
                container_statuses: Some(vec![ContainerStatus {
                    name: "import".into(),
                    state: Some(state),
                    last_state: last,
                    ..Default::default()
                }]),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn import_pod_progress_is_the_vm_state() {
        let claim = bound_claim();
        let running = import_pod(
            ContainerState {
                running: Some(ContainerStateRunning::default()),
                ..Default::default()
            },
            None,
        );
        let s = derive(
            &vm(),
            &Observed {
                claim: Some(&claim),
                claim_name: Some("web-root".into()),
                import_pod: Some(&running),
                has_import: true,
                ..Default::default()
            },
            now(),
        );
        for t in [vm::STORAGE_READY, vm::SCHEDULED, vm::READY] {
            assert_eq!(cond(&s, t).status, "False", "{t}");
            assert_eq!(cond(&s, t).reason, "ImportingDisk", "{t}");
        }
        assert!(s.anchor_pod.is_none());

        let failed = import_pod(
            ContainerState::default(),
            Some(ContainerState {
                terminated: Some(ContainerStateTerminated {
                    exit_code: 1,
                    message: Some("ImportFailed: SHA-256 mismatch".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        );
        let s = derive(
            &vm(),
            &Observed {
                claim: Some(&claim),
                claim_name: Some("web-root".into()),
                import_pod: Some(&failed),
                has_import: true,
                ..Default::default()
            },
            now(),
        );
        assert_eq!(cond(&s, vm::READY).reason, "ImportFailed");
        assert_eq!(cond(&s, vm::READY).message, "SHA-256 mismatch");
    }

    #[test]
    fn a_pending_import_without_pods_is_not_ready() {
        let claim = bound_claim();
        let s = derive(
            &vm(),
            &Observed {
                claim: Some(&claim),
                claim_name: Some("web-root".into()),
                has_import: true,
                ..Default::default()
            },
            now(),
        );
        assert_eq!(cond(&s, vm::STORAGE_READY).reason, "ImportPending");
        assert_eq!(cond(&s, vm::READY).reason, "ImportPending");
    }

    #[test]
    fn an_unschedulable_import_pod_is_reported() {
        let claim = PersistentVolumeClaim::default(); // WaitForFirstConsumer: not bound yet
        let mut p = import_pod(ContainerState::default(), None);
        p.status = Some(PodStatus {
            conditions: Some(vec![k8s_openapi::api::core::v1::PodCondition {
                type_: "PodScheduled".into(),
                status: "False".into(),
                reason: Some("Unschedulable".into()),
                message: Some("0/3 nodes are available: 3 Insufficient memory.".into()),
                ..Default::default()
            }]),
            ..Default::default()
        });
        let s = derive(
            &vm(),
            &Observed {
                claim: Some(&claim),
                claim_name: Some("web-root".into()),
                import_pod: Some(&p),
                has_import: true,
                ..Default::default()
            },
            now(),
        );
        assert_eq!(cond(&s, vm::SCHEDULED).reason, "Unschedulable");
        assert_eq!(cond(&s, vm::READY).reason, "Unschedulable");
        assert!(cond(&s, vm::READY).message.contains("Insufficient memory"));
    }

    #[test]
    fn an_exited_importer_alone_does_not_make_the_disk_ready() {
        let claim = bound_claim();
        let done = import_pod(
            ContainerState {
                terminated: Some(ContainerStateTerminated {
                    exit_code: 0,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        );
        let s = derive(
            &vm(),
            &Observed {
                claim: Some(&claim),
                claim_name: Some("web-root".into()),
                import_pod: Some(&done),
                has_import: true,
                ..Default::default()
            },
            now(),
        );
        assert_eq!(cond(&s, vm::STORAGE_READY).status, "False");
        assert_eq!(cond(&s, vm::STORAGE_READY).reason, "ImportingDisk");
    }

    #[test]
    fn long_scheduler_messages_are_capped() {
        assert_eq!(capped("short".into()), "short");
        let long = "x".repeat(5000);
        let c = capped(long);
        assert!(c.len() <= SCHEDULER_MESSAGE_MAX);
        assert!(c.ends_with(" ..."));
    }

    #[test]
    fn network_ready_comes_from_the_nodes_report() {
        use baryon_api::node::{NodeVmNetwork, NodeVmStatus, RuntimeStatus};
        use baryon_api::{VirtualizationNode, VirtualizationNodeSpec, VirtualizationNodeStatus};
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
        let t = |s: i64| Time(Timestamp::from_second(s).unwrap());
        let node = |heartbeat: i64, healthy: bool, version: &str, state: Option<&str>| {
            let mut n = VirtualizationNode::new("node1", VirtualizationNodeSpec {});
            n.status = Some(VirtualizationNodeStatus {
                heartbeat_time: Some(t(heartbeat)),
                conditions: vec![Condition {
                    type_: "RuntimeHealthy".into(),
                    status: if healthy { "True" } else { "False" }.into(),
                    reason: "R".into(),
                    message: String::new(),
                    last_transition_time: t(0),
                    observed_generation: None,
                }],
                runtime: Some(RuntimeStatus {
                    protocol_version: Some(version.into()),
                    ..Default::default()
                }),
                vms: state
                    .map(|st| {
                        vec![NodeVmStatus {
                            anchor_pod_uid: "pod-1".into(),
                            network: Some(NodeVmNetwork {
                                state: st.into(),
                                reason: if st == "Ready" {
                                    "AddressServiceRunning"
                                } else {
                                    "DhcpResponderFailing"
                                }
                                .into(),
                                message: "m".into(),
                                last_transition_time: Some(t(500)),
                                lease_expires_time: Some(t(90_000)),
                            }),
                        }]
                    })
                    .unwrap_or_default(),
                ..Default::default()
            });
            n
        };
        // The heartbeat argument stands for freshness as the controller saw it.
        let c = |n: &VirtualizationNode| {
            let fresh = n.status.as_ref().and_then(|s| s.heartbeat_time.as_ref()) != Some(&t(800));
            network_condition(Some(n), fresh, "pod-1")
        };

        let ready = c(&node(990, true, "1.2", Some("Ready")));
        assert_eq!(
            (ready.status, ready.reason.as_str()),
            ("True", "AddressServiceRunning")
        );
        let failing = c(&node(990, true, "1.2", Some("Failing")));
        assert_eq!(
            (failing.status, failing.reason.as_str()),
            ("False", "DhcpResponderFailing")
        );
        assert!(failing.message.contains("lease valid until"));
        // Stale or unhealthy report, old runtime, no entry for this pod.
        assert_eq!(
            c(&node(800, true, "1.2", Some("Ready"))).reason,
            "RuntimeUnreachable"
        );
        assert_eq!(
            c(&node(990, false, "1.2", Some("Ready"))).reason,
            "RuntimeUnreachable"
        );
        assert_eq!(
            c(&node(990, true, "1.1", None)).reason,
            "RuntimeDoesNotReport"
        );
        assert_eq!(c(&node(990, true, "1.2", None)).reason, "NotReported");
        for u in [
            c(&node(800, true, "1.2", Some("Ready"))),
            c(&node(990, true, "1.1", None)),
        ] {
            assert_eq!(u.status, "Unknown");
        }
        assert_eq!(network_condition(None, true, "pod-1").status, "Unknown");
        // The reason comes from the state; the message is capped.
        let mut forged = node(990, true, "1.2", Some("Failing"));
        if let Some(v) = forged.status.as_mut().and_then(|s| s.vms.first_mut()) {
            let n = v.network.as_mut().unwrap();
            n.reason = "SomethingElse".into();
            n.message = "x".repeat(5000);
        }
        let f = c(&forged);
        assert_eq!(f.reason, "DhcpResponderFailing");
        assert!(f.message.len() <= 1024);
    }

    #[test]
    fn storage_ready_names_the_claim_problem_even_behind_a_class_problem() {
        let s = derive(
            &vm(),
            &Observed {
                blocked: Some(("ClassNotFound".into(), "no class".into())),
                disk_error: Some(("ClaimNotFound".into(), "no claim disk".into())),
                ..Default::default()
            },
            now(),
        );
        assert_eq!(cond(&s, vm::READY).reason, "ClassNotFound");
        assert_eq!(cond(&s, vm::STORAGE_READY).reason, "ClaimNotFound");
        let s = derive(&vm(), &Observed::default(), now());
        assert_eq!(cond(&s, vm::STORAGE_READY).reason, "ClaimMissing");
    }

    #[test]
    fn a_ready_vm_on_a_node_that_stopped_reporting_is_unknown() {
        use k8s_openapi::api::core::v1::PodCondition;
        let claim = bound_claim();
        let mut p = pod(
            ContainerStatus {
                name: "anchor".into(),
                ready: true,
                state: Some(ContainerState {
                    running: Some(ContainerStateRunning::default()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            true,
        );
        // What the node lifecycle controller leaves: PodReady False, the
        // container status as kubelet last reported it.
        p.status.as_mut().unwrap().conditions = Some(vec![PodCondition {
            type_: "Ready".into(),
            status: "False".into(),
            ..Default::default()
        }]);
        let obs = Observed {
            claim: Some(&claim),
            pod: Some(&p),
            ..Default::default()
        };
        let s = derive(&vm(), &obs, now());
        for t in [vm::RUNNING, vm::READY, vm::NETWORK_READY] {
            let c = cond(&s, t);
            assert_eq!(
                (c.status.as_str(), c.reason.as_str()),
                ("Unknown", "NodeUnreachable"),
                "{t}"
            );
        }
        // Evicted from the lost node (Terminating): still unknown, not
        // "the guest is shutting down".
        p.metadata.deletion_timestamp = Some(now());
        let obs = Observed {
            claim: Some(&claim),
            pod: Some(&p),
            ..Default::default()
        };
        let s = derive(&vm(), &obs, now());
        assert_eq!(cond(&s, vm::READY).reason, "NodeUnreachable");
        // A terminal pod with Ready False (kubelet sets it) is not a lost
        // node.
        p.metadata.deletion_timestamp = None;
        p.status.as_mut().unwrap().phase = Some("Failed".into());
        let obs = Observed {
            claim: Some(&claim),
            pod: Some(&p),
            ..Default::default()
        };
        let s = derive(&vm(), &obs, now());
        assert_ne!(cond(&s, vm::READY).reason, "NodeUnreachable");
    }
}
