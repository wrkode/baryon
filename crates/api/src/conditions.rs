//! Condition types and helpers.
//!
//! Conditions follow the Kubernetes API conventions (`metav1.Condition`):
//! the transition time changes only when the status changes.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};

/// VirtualMachine condition types.
pub mod vm {
    /// The Anchor Pod is bound to a node.
    pub const SCHEDULED: &str = "Scheduled";
    /// The root disk claim is bound and its content is in place.
    pub const STORAGE_READY: &str = "StorageReady";
    /// The VMM process is running for the current Anchor Pod.
    pub const RUNNING: &str = "Running";
    /// The guest produced boot evidence (DHCP lease or console login prompt).
    pub const READY: &str = "Ready";
    /// The spec changed in a way that takes effect only after a restart.
    pub const RESTART_REQUIRED: &str = "RestartRequired";
}

/// VirtualizationNode condition types.
pub mod node {
    /// The node can run VMs: runtime healthy and KVM usable.
    pub const READY: &str = "Ready";
    /// baryon-virtd answers and host discovery succeeded.
    pub const RUNTIME_HEALTHY: &str = "RuntimeHealthy";
}

pub const TRUE: &str = "True";
pub const FALSE: &str = "False";
pub const UNKNOWN: &str = "Unknown";

/// Desired state of one condition, without timestamps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionUpdate {
    pub type_: &'static str,
    pub status: &'static str,
    pub reason: String,
    pub message: String,
}

impl ConditionUpdate {
    pub fn new(
        type_: &'static str,
        status: bool,
        reason: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            type_,
            status: if status { TRUE } else { FALSE },
            reason: reason.into(),
            message: message.into(),
        }
    }

    pub fn unknown(
        type_: &'static str,
        reason: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            type_,
            status: UNKNOWN,
            reason: reason.into(),
            message: message.into(),
        }
    }
}

/// Merge updates into existing conditions. Conditions not mentioned are kept.
/// `lastTransitionTime` is preserved unless the status changed.
pub fn merge(
    existing: &[Condition],
    updates: &[ConditionUpdate],
    observed_generation: Option<i64>,
    now: Time,
) -> Vec<Condition> {
    let mut out: Vec<Condition> = existing.to_vec();
    for u in updates {
        let reason = if u.reason.is_empty() {
            "Unknown".to_owned()
        } else {
            u.reason.clone()
        };
        match out.iter_mut().find(|c| c.type_ == u.type_) {
            Some(c) => {
                if c.status != u.status {
                    c.last_transition_time = now.clone();
                }
                c.status = u.status.to_owned();
                c.reason = reason;
                c.message = u.message.clone();
                c.observed_generation = observed_generation;
            }
            None => out.push(Condition {
                type_: u.type_.to_owned(),
                status: u.status.to_owned(),
                reason,
                message: u.message.clone(),
                last_transition_time: now.clone(),
                observed_generation,
            }),
        }
    }
    out
}

/// Find a condition by type.
pub fn find<'a>(conditions: &'a [Condition], type_: &str) -> Option<&'a Condition> {
    conditions.iter().find(|c| c.type_ == type_)
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::jiff::Timestamp;

    fn t(secs: i64) -> Time {
        Time(Timestamp::from_second(secs).unwrap())
    }

    #[test]
    fn transition_time_changes_only_with_status() {
        let c = merge(
            &[],
            &[ConditionUpdate::new(vm::READY, false, "Booting", "")],
            Some(1),
            t(10),
        );
        assert_eq!(c[0].last_transition_time, t(10));

        let c = merge(
            &c,
            &[ConditionUpdate::new(vm::READY, false, "Booting", "still")],
            Some(1),
            t(20),
        );
        assert_eq!(c[0].last_transition_time, t(10));
        assert_eq!(c[0].message, "still");

        let c = merge(
            &c,
            &[ConditionUpdate::new(vm::READY, true, "GuestBooted", "")],
            Some(2),
            t(30),
        );
        assert_eq!(c[0].last_transition_time, t(30));
        assert_eq!(c[0].status, TRUE);
        assert_eq!(c[0].observed_generation, Some(2));
    }

    #[test]
    fn keeps_unmentioned_and_fills_reason() {
        let c = merge(
            &[],
            &[
                ConditionUpdate::new(vm::SCHEDULED, true, "", ""),
                ConditionUpdate::unknown(vm::READY, "Pending", ""),
            ],
            None,
            t(1),
        );
        let c = merge(
            &c,
            &[ConditionUpdate::new(vm::READY, true, "GuestBooted", "")],
            None,
            t(2),
        );
        assert_eq!(c.len(), 2);
        assert_eq!(find(&c, vm::SCHEDULED).unwrap().reason, "Unknown");
    }
}
