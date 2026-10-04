//! Version and features of the runtime protocol (ADR 0012).
//!
//! The protobuf package is the major version: `baryon.runtime.v1alpha1` is
//! major 1, and a new major version is a new package. Within a major
//! version, minor versions only add, so peers of the same major version
//! interoperate. What a VM needs from the host beyond the protocol itself is
//! expressed as named features: the anchor lists the features its VM needs,
//! baryon-virtd lists the features it implements, and an attach that needs a
//! missing feature is refused before the VM starts.
//!
//! A peer that sends no version predates negotiation and speaks 1.0 with
//! exactly [`BASE_FEATURES`].

use std::fmt;

use crate::v1alpha1::ProtocolVersion;

/// Major version of this package.
pub const MAJOR: u32 = 1;

/// Minor version this build speaks.
///
/// - 1.0: the initial protocol, without negotiation.
/// - 1.1: `ProtocolVersion`, `AnchorService.Negotiate`, required features in
///   `AttachRequest`, version and features in `RuntimeInfo`.
/// - 1.2: `NetworkStatus` in `VmStatus` (host-side guest network service).
///   Not a feature: a VM does not need it to run.
pub const MINOR: u32 = 2;

/// Feature names. A name, once released, keeps its meaning.
pub mod feature {
    /// UEFI firmware boot from the first disk.
    pub const FIRMWARE_BOOT: &str = "boot.firmware";
    /// A disk that is a regular file on a filesystem volume.
    pub const FILE_DISK: &str = "disk.file";
    /// A disk that is a block device node.
    pub const BLOCK_DISK: &str = "disk.block";
    /// The guest takes over an interface of the pod network namespace.
    pub const POD_NETWORK: &str = "network.pod";
}

/// Features of protocol 1.0, implied for a peer that sends no version.
pub const BASE_FEATURES: &[&str] = &[
    feature::FIRMWARE_BOOT,
    feature::FILE_DISK,
    feature::BLOCK_DISK,
    feature::POD_NETWORK,
];

/// Features baryon-virtd of this build implements.
pub const FEATURES: &[&str] = BASE_FEATURES;

/// A protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
}

impl Version {
    /// The version this build speaks.
    pub const CURRENT: Self = Self {
        major: MAJOR,
        minor: MINOR,
    };

    /// The version of a peer from its message; 1.0 when it sent none.
    pub fn of_peer(v: Option<&ProtocolVersion>) -> Self {
        v.map_or(Self { major: 1, minor: 0 }, |v| Self {
            major: v.major,
            minor: v.minor,
        })
    }

    /// The wire form.
    pub fn to_proto(self) -> ProtocolVersion {
        ProtocolVersion {
            major: self.major,
            minor: self.minor,
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Why two peers cannot run a VM together. The `Display` form starts with a
/// CamelCase reason, which the anchor reports as the VM's reason.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Incompatible {
    #[error(
        "ProtocolIncompatible: this side speaks runtime protocol {ours}, the other side {theirs}; the cluster release and the host image must have the same major version"
    )]
    Major { ours: Version, theirs: Version },
    #[error(
        "UnsupportedFeature: baryon-virtd (protocol {server}) does not implement {}; upgrade the host image of this node", .missing.join(", ")
    )]
    MissingFeatures {
        server: Version,
        missing: Vec<String>,
    },
}

/// The features a peer implements: its list, or [`BASE_FEATURES`] when it
/// predates negotiation (sent no version).
pub fn features_of_peer(version: Option<&ProtocolVersion>, features: &[String]) -> Vec<String> {
    if version.is_none() {
        BASE_FEATURES.iter().map(|&f| f.to_owned()).collect()
    } else {
        features.to_vec()
    }
}

/// Whether this build can talk to a peer of version `theirs`.
pub fn check_major(theirs: Version) -> Result<(), Incompatible> {
    if theirs.major == MAJOR {
        Ok(())
    } else {
        Err(Incompatible::Major {
            ours: Version::CURRENT,
            theirs,
        })
    }
}

/// Whether a baryon-virtd of version `server` implementing `available` can
/// run a VM that needs `required`.
pub fn check_server(
    server: Version,
    available: &[String],
    required: &[String],
) -> Result<(), Incompatible> {
    check_major(server)?;
    let missing: Vec<String> = required
        .iter()
        .filter(|r| !available.contains(r))
        .cloned()
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(Incompatible::MissingFeatures { server, missing })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|&x| x.to_owned()).collect()
    }

    #[test]
    fn a_peer_without_version_is_1_0_with_base_features() {
        assert_eq!(Version::of_peer(None), Version { major: 1, minor: 0 });
        assert_eq!(features_of_peer(None, &[]), s(BASE_FEATURES));
        // A 1.0 peer cannot claim features it does not know about.
        assert_eq!(
            features_of_peer(None, &s(&["disk.future"])),
            s(BASE_FEATURES)
        );
    }

    #[test]
    fn a_versioned_peer_reports_its_own_features() {
        let v = ProtocolVersion { major: 1, minor: 7 };
        assert_eq!(Version::of_peer(Some(&v)), Version { major: 1, minor: 7 });
        assert_eq!(features_of_peer(Some(&v), &s(&["a"])), s(&["a"]));
    }

    #[test]
    fn same_major_interoperates_in_both_directions() {
        for minor in [0, MINOR, MINOR + 5] {
            let peer = Version {
                major: MAJOR,
                minor,
            };
            assert!(check_major(peer).is_ok());
            assert!(check_server(peer, &s(BASE_FEATURES), &s(BASE_FEATURES)).is_ok());
        }
    }

    #[test]
    fn another_major_is_refused() {
        let err = check_server(Version { major: 2, minor: 0 }, &s(FEATURES), &[]).unwrap_err();
        assert!(matches!(err, Incompatible::Major { .. }));
        assert!(err.to_string().starts_with("ProtocolIncompatible: "));
        assert!(check_major(Version { major: 0, minor: 9 }).is_err());
    }

    #[test]
    fn a_missing_feature_is_named() {
        let err = check_server(
            Version { major: 1, minor: 0 },
            &s(BASE_FEATURES),
            &s(&["disk.block", "disk.future", "net.future"]),
        )
        .unwrap_err();
        assert_eq!(
            err,
            Incompatible::MissingFeatures {
                server: Version { major: 1, minor: 0 },
                missing: s(&["disk.future", "net.future"]),
            }
        );
        assert!(
            err.to_string()
                .starts_with("UnsupportedFeature: baryon-virtd (protocol 1.0) does not implement disk.future, net.future")
        );
    }

    #[test]
    fn this_build_implements_the_base_features() {
        for f in BASE_FEATURES {
            assert!(FEATURES.contains(f), "{f}");
        }
    }
}
