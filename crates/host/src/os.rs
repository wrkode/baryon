//! Operating system identification from os-release files and procfs.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::root::HostRoot;

/// Fields from os-release(5).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OsRelease {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pretty_name: Option<String>,
}

/// Observed operating system state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OsInfo {
    pub kernel_release: String,
    pub os_release: OsRelease,
    /// Key and value pairs from /etc/kairos-release when the host is a
    /// Kairos system. Reported verbatim; Baryon does not interpret them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kairos_release: Option<BTreeMap<String, String>>,
}

pub(crate) fn discover(root: &HostRoot) -> Result<OsInfo> {
    let kernel_release = root.read_trimmed("/proc/sys/kernel/osrelease")?;

    let os_release_raw = match root.read_optional("/etc/os-release")? {
        Some(s) => Some(s),
        None => root.read_optional("/usr/lib/os-release")?,
    };
    let os_fields = os_release_raw
        .as_deref()
        .map(parse_env_file)
        .unwrap_or_default();
    let os_release = OsRelease {
        id: os_fields.get("ID").cloned(),
        version_id: os_fields.get("VERSION_ID").cloned(),
        pretty_name: os_fields.get("PRETTY_NAME").cloned(),
    };

    let kairos_release = root
        .read_optional("/etc/kairos-release")?
        .map(|s| parse_env_file(&s));

    Ok(OsInfo {
        kernel_release,
        os_release,
        kairos_release,
    })
}

/// Parse `KEY=value` lines with optional single or double quotes, as used by
/// os-release(5). Comments and malformed lines are skipped.
pub(crate) fn parse_env_file(s: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        out.insert(key.to_owned(), unquote(value.trim()));
    }
    out
}

fn unquote(v: &str) -> String {
    let bytes = v.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            let inner = &v[1..v.len() - 1];
            if first == b'"' {
                return inner
                    .replace("\\\"", "\"")
                    .replace("\\$", "$")
                    .replace("\\`", "`")
                    .replace("\\\\", "\\");
            }
            return inner.to_owned();
        }
    }
    v.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_os_release() {
        let s = "PRETTY_NAME=\"Hadron Linux\"\nID=hadron\n# comment\nVERSION_ID='1.0'\nBAD LINE\n";
        let m = parse_env_file(s);
        assert_eq!(
            m.get("PRETTY_NAME").map(String::as_str),
            Some("Hadron Linux")
        );
        assert_eq!(m.get("ID").map(String::as_str), Some("hadron"));
        assert_eq!(m.get("VERSION_ID").map(String::as_str), Some("1.0"));
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn unescapes_double_quotes() {
        assert_eq!(unquote(r#""a \"b\" c""#), r#"a "b" c"#);
        assert_eq!(unquote("plain"), "plain");
        assert_eq!(unquote("\""), "\"");
    }
}
