//! Derived values: Anchor Pod resources, VMM uid, guest MAC, object names.

use sha2::{Digest, Sha256};

/// Fixed VMM memory overhead added to guest memory for the pod limit.
pub(crate) const MEMORY_OVERHEAD_FIXED: u64 = 128 * 1024 * 1024;
/// Proportional overhead: 1/64 of guest memory (page tables, device state).
pub(crate) const MEMORY_OVERHEAD_DIVISOR: u64 = 64;

/// Guest shutdown grace before baryon-virtd stops the VMM.
pub(crate) const STOP_GRACE_SECONDS: u32 = 60;
/// Extra pod termination time on top of the guest grace: baryon-virtd's
/// escalation (two steps of 5 s) plus margin.
pub(crate) const TERMINATION_MARGIN_SECONDS: u32 = 30;

/// Pod memory limit for a guest of `guest_bytes`, rounded up to MiB.
pub(crate) fn pod_memory_bytes(guest_bytes: u64) -> u64 {
    let raw = guest_bytes + MEMORY_OVERHEAD_FIXED + guest_bytes / MEMORY_OVERHEAD_DIVISOR;
    raw.div_ceil(1 << 20) << 20
}

/// Stable guest MAC (shared with baryon-virtd).
pub(crate) fn vm_mac(vm_uid: &str) -> String {
    baryon_runtime_api::identity::guest_mac_string(vm_uid)
}

/// Name of a VM's Anchor Pod. One fixed name per VM means at most one Anchor
/// Pod object can exist for a VM at any time.
pub(crate) fn anchor_pod_name(vm: &str) -> String {
    bounded_name(vm, "-anchor", 253)
}

/// Name of the root disk claim Baryon creates for imported disks.
pub(crate) fn root_claim_name(vm: &str) -> String {
    bounded_name(vm, "-root", 253)
}

/// `<base><suffix>`, shortened with a hash when longer than `max`.
fn bounded_name(base: &str, suffix: &str, max: usize) -> String {
    let full = format!("{base}{suffix}");
    if full.len() <= max {
        return full;
    }
    let digest = Sha256::digest(base.as_bytes());
    let hash: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
    let keep = max - suffix.len() - hash.len() - 1;
    let trimmed = base[..keep].trim_end_matches(['-', '.']);
    format!("{trimmed}-{hash}{suffix}")
}

/// Hash of the inputs that require a new Anchor Pod when they change.
pub(crate) fn spec_hash(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0]);
    }
    h.finalize()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_overhead() {
        let gib = 1 << 30;
        assert_eq!(
            pod_memory_bytes(2 * gib),
            2 * gib + (128 << 20) + (32 << 20)
        );
        assert_eq!(pod_memory_bytes(64 << 20) % (1 << 20), 0);
        // Always above baryon-virtd's minimum headroom of 64 MiB.
        assert!(pod_memory_bytes(gib) - gib >= 64 << 20);
    }

    #[test]
    fn mac_comes_from_the_shared_derivation() {
        let uid = "6b7f4c0e-6a39-4b8e-9d0e-2f1f0c6c4a11";
        assert_eq!(vm_mac(uid), "02:1f:0c:6c:4a:11");
    }

    #[test]
    fn names_are_bounded() {
        assert_eq!(anchor_pod_name("web"), "web-anchor");
        let long = "a".repeat(260);
        let n = anchor_pod_name(&long);
        assert!(n.len() <= 253);
        assert!(n.ends_with("-anchor"));
        assert_eq!(n, anchor_pod_name(&long));
    }
}
