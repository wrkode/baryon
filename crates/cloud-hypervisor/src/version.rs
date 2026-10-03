//! Cloud Hypervisor version handling.
//!
//! The authoritative version at runtime is `vmm.ping` from a running VMM.
//! Before any VMM runs, Baryon reads the version from the component manifest
//! shipped in the Baryon-enabled image. Parsing the banner of
//! `cloud-hypervisor --version` is a fallback for development hosts.

use std::fmt;

/// A Cloud Hypervisor release version, `MAJOR.POINT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u32,
    pub point: u32,
}

impl Version {
    /// The release Baryon is developed and tested against.
    pub const TESTED: Version = Version {
        major: 53,
        point: 0,
    };

    /// Parse `53.0`, `v53.0`, or the first line of `cloud-hypervisor
    /// --version` (`cloud-hypervisor v53.0`). A trailing build suffix after a
    /// dash is ignored.
    pub fn parse(s: &str) -> Result<Self, String> {
        let first_line = s.lines().next().unwrap_or("").trim();
        let token = first_line
            .split_whitespace()
            .last()
            .ok_or_else(|| format!("empty version string {s:?}"))?;
        let token = token.strip_prefix('v').unwrap_or(token);
        let token = token.split('-').next().unwrap_or(token);
        let mut parts = token.split('.');
        let major = parts
            .next()
            .and_then(|p| p.parse().ok())
            .ok_or_else(|| format!("invalid version {s:?}"))?;
        let point = match parts.next() {
            Some(p) => p.parse().map_err(|_| format!("invalid version {s:?}"))?,
            None => 0,
        };
        Ok(Self { major, point })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}.{}", self.major, self.point)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_forms() {
        let v = Version {
            major: 53,
            point: 0,
        };
        assert_eq!(Version::parse("53.0").unwrap(), v);
        assert_eq!(Version::parse("v53.0").unwrap(), v);
        assert_eq!(
            Version::parse("cloud-hypervisor v53.0\nMigration Protocol Versions: 0").unwrap(),
            v
        );
        assert_eq!(
            Version::parse("v51.1-12-gabcdef").unwrap(),
            Version {
                major: 51,
                point: 1
            }
        );
        assert_eq!(
            Version::parse("54").unwrap(),
            Version {
                major: 54,
                point: 0
            }
        );
        assert!(Version::parse("").is_err());
        assert!(Version::parse("cloud-hypervisor vX").is_err());
    }

    #[test]
    fn orders_and_displays() {
        assert!(Version::parse("52.0").unwrap() < Version::TESTED);
        assert_eq!(Version::TESTED.to_string(), "v53.0");
    }
}
