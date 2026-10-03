//! Per-VM identities.
//!
//! Each Baryon VM runs as a distinct, unprivileged host identity: the Anchor
//! container, Cloud Hypervisor, and the DHCP responder of one VM share it,
//! and no two VMs on a node do. [`VmIdentity`] is the only way to obtain or
//! check that identity. How it is allocated is an implementation detail of
//! this module, shared by baryon-controller (which runs the Anchor container
//! with it) and baryon-virtd (which checks the anchor against it); it is not
//! part of the Kubernetes API, of documented storage formats, or of any
//! annotation.
//!
//! The guest's MAC address is a separate, guest-visible identity and is
//! also derived here so both sides agree on it.

use std::ops::RangeInclusive;

use sha2::{Digest, Sha256};

/// Host uids and gids reserved for VM identities. Hosts must not assign
/// these to other users or groups. This range is a host requirement and is
/// documented as such; which value a given VM receives is not.
pub const RESERVED_IDS: RangeInclusive<u32> = 1_000_000_000..=1_999_999_999;

/// The host identity of one VM's processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VmIdentity {
    uid: u32,
    gid: u32,
}

/// Why an identity check failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    #[error("the VM UID {0:?} is empty")]
    EmptyVmUid(String),
    #[error("the process runs as uid {uid} gid {gid}, not as this VM's identity")]
    Mismatch { uid: u32, gid: u32 },
}

impl VmIdentity {
    /// The identity of the VM with Kubernetes UID `vm_uid`. Deterministic:
    /// the same VM gets the same identity on every node and across
    /// restarts of any component, so imported disk images stay readable.
    pub fn for_vm(vm_uid: &str) -> Result<Self, IdentityError> {
        if vm_uid.is_empty() {
            return Err(IdentityError::EmptyVmUid(vm_uid.to_owned()));
        }
        let digest = Sha256::digest(vm_uid.as_bytes());
        let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
        let span = RESERVED_IDS.end() - RESERVED_IDS.start() + 1;
        let id = RESERVED_IDS.start() + n % span;
        Ok(Self { uid: id, gid: id })
    }

    pub fn uid(&self) -> u32 {
        self.uid
    }

    pub fn gid(&self) -> u32 {
        self.gid
    }

    /// Check that a process (from its peer credentials) runs as this
    /// identity.
    pub fn verify(&self, uid: u32, gid: u32) -> Result<(), IdentityError> {
        if uid == self.uid && gid == self.gid {
            Ok(())
        } else {
            Err(IdentityError::Mismatch { uid, gid })
        }
    }

    /// Whether `id` lies in the range reserved for VM identities.
    pub fn is_reserved(id: u32) -> bool {
        RESERVED_IDS.contains(&id)
    }
}

/// Stable guest MAC address: locally administered, unicast, the remaining
/// 40 bits taken from the end of the VirtualMachine UID. Non-hex characters
/// are ignored; missing digits are zero.
pub fn guest_mac(vm_uid: &str) -> [u8; 6] {
    let hex: String = vm_uid.chars().filter(char::is_ascii_hexdigit).collect();
    let tail = hex.get(hex.len().saturating_sub(10)..).unwrap_or("");
    let mut mac = [0x02, 0, 0, 0, 0, 0];
    for (i, byte) in mac.iter_mut().skip(1).enumerate() {
        *byte = tail
            .get(i * 2..i * 2 + 2)
            .and_then(|b| u8::from_str_radix(b, 16).ok())
            .unwrap_or(0);
    }
    mac
}

/// [`guest_mac`] in the usual colon-separated lowercase form.
pub fn guest_mac_string(vm_uid: &str) -> String {
    let m = guest_mac(vm_uid);
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[0], m[1], m[2], m[3], m[4], m[5]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "0b9d3a5e-2b9b-4f4e-8d0a-6c1f2e7a9b01";
    const B: &str = "0b9d3a5e-2b9b-4f4e-8d0a-6c1f2e7a9b02";

    #[test]
    fn allocation_is_deterministic() {
        // Restart consistency: every component and every restart computes
        // the same identity for the same VM.
        assert_eq!(VmIdentity::for_vm(A), VmIdentity::for_vm(A));
        assert_ne!(
            VmIdentity::for_vm(A).unwrap(),
            VmIdentity::for_vm(B).unwrap()
        );
    }

    #[test]
    fn identities_are_unprivileged_and_reserved() {
        for i in 0..2000 {
            let id = VmIdentity::for_vm(&format!("{i:08x}-0000-4000-8000-000000000000")).unwrap();
            assert!(VmIdentity::is_reserved(id.uid()));
            assert_eq!(id.uid(), id.gid());
            // Never root, never a regular or system user, never nobody.
            assert!(id.uid() >= 1_000_000_000);
        }
        assert!(!VmIdentity::is_reserved(0));
        assert!(!VmIdentity::is_reserved(65_534));
        assert!(!VmIdentity::is_reserved(u32::MAX));
    }

    #[test]
    fn verification() {
        let id = VmIdentity::for_vm(A).unwrap();
        assert!(id.verify(id.uid(), id.gid()).is_ok());
        assert_eq!(
            id.verify(id.uid() + 1, id.gid()),
            Err(IdentityError::Mismatch {
                uid: id.uid() + 1,
                gid: id.gid()
            })
        );
        assert!(id.verify(id.uid(), 0).is_err());
        assert!(id.verify(0, 0).is_err());
    }

    #[test]
    fn empty_vm_uid_is_refused() {
        assert_eq!(
            VmIdentity::for_vm(""),
            Err(IdentityError::EmptyVmUid(String::new()))
        );
    }

    #[test]
    fn mac_is_local_unicast_from_uid_tail() {
        let uid = "6b7f4c0e-6a39-4b8e-9d0e-2f1f0c6c4a11";
        assert_eq!(guest_mac(uid), [0x02, 0x1f, 0x0c, 0x6c, 0x4a, 0x11]);
        assert_eq!(guest_mac_string(uid), "02:1f:0c:6c:4a:11");
        assert_eq!(
            guest_mac_string("6B7F4C0E-6A39-4B8E-9D0E-2F1F0C6C4A11"),
            "02:1f:0c:6c:4a:11"
        );
        assert_eq!(guest_mac(""), [0x02, 0, 0, 0, 0, 0]);
    }
}
