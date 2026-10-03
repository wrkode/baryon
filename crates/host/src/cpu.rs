//! CPU and architecture discovery.

use serde::{Deserialize, Serialize};

use crate::cpulist;
use crate::error::{Error, Result};
use crate::root::HostRoot;

/// Hardware virtualization extension advertised by the CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VirtualizationExtension {
    /// Intel VT-x.
    Vmx,
    /// AMD-V.
    Svm,
}

/// Observed CPU state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CpuInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    /// Online CPUs in kernel list format, for example `0-19`.
    pub online: String,
    pub online_count: u32,
    /// x86 virtualization extension from the CPU flags. Always `None` on
    /// architectures that do not advertise one in /proc/cpuinfo.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub virtualization_extension: Option<VirtualizationExtension>,
    /// Whether the loaded KVM vendor module allows nested virtualization.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nested_virtualization: Option<bool>,
}

/// Kernel machine name, for example `x86_64` or `aarch64`.
pub(crate) fn architecture(root: &HostRoot) -> Result<String> {
    if let Some(arch) = root.read_optional("/proc/sys/kernel/arch")? {
        return Ok(arch.trim().to_owned());
    }
    if root.is_system() {
        let uname = rustix::system::uname();
        return Ok(uname.machine().to_string_lossy().into_owned());
    }
    Err(Error::parse(
        root.path("/proc/sys/kernel/arch"),
        "architecture not available",
    ))
}

pub(crate) fn discover(root: &HostRoot) -> Result<CpuInfo> {
    let online_raw = root.read_trimmed("/sys/devices/system/cpu/online")?;
    let online_ids = cpulist::parse_cpu_list(&online_raw)
        .map_err(|e| Error::parse(root.path("/sys/devices/system/cpu/online"), e))?;

    let cpuinfo = root.read_optional("/proc/cpuinfo")?.unwrap_or_default();
    let (model_name, virtualization_extension) = parse_cpuinfo(&cpuinfo);

    let nested_virtualization = match virtualization_extension {
        Some(VirtualizationExtension::Vmx) => {
            read_bool_param(root, "/sys/module/kvm_intel/parameters/nested")?
        }
        Some(VirtualizationExtension::Svm) => {
            read_bool_param(root, "/sys/module/kvm_amd/parameters/nested")?
        }
        None => None,
    };

    Ok(CpuInfo {
        model_name,
        online: cpulist::format_cpu_list(&online_ids),
        online_count: u32::try_from(online_ids.len()).unwrap_or(u32::MAX),
        virtualization_extension,
        nested_virtualization,
    })
}

/// Extract the model name and virtualization flag from the first processor
/// block of /proc/cpuinfo.
fn parse_cpuinfo(cpuinfo: &str) -> (Option<String>, Option<VirtualizationExtension>) {
    let mut model = None;
    let mut ext = None;
    for line in cpuinfo.lines() {
        let Some((key, value)) = line.split_once(':') else {
            if model.is_some() || ext.is_some() {
                // A blank line ends the first processor block.
                break;
            }
            continue;
        };
        match key.trim() {
            "model name" if model.is_none() => model = Some(value.trim().to_owned()),
            "flags" if ext.is_none() => {
                let flags: Vec<&str> = value.split_whitespace().collect();
                ext = if flags.contains(&"vmx") {
                    Some(VirtualizationExtension::Vmx)
                } else if flags.contains(&"svm") {
                    Some(VirtualizationExtension::Svm)
                } else {
                    None
                };
            }
            _ => {}
        }
    }
    (model, ext)
}

/// Module parameters report booleans as `Y`/`N` or `1`/`0`.
fn read_bool_param(root: &HostRoot, abs: &str) -> Result<Option<bool>> {
    match root.read_optional(abs)? {
        None => Ok(None),
        Some(v) => match v.trim() {
            "Y" | "y" | "1" => Ok(Some(true)),
            "N" | "n" | "0" => Ok(Some(false)),
            other => Err(Error::parse(
                root.path(abs),
                format!("unexpected boolean parameter value {other:?}"),
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_intel_cpuinfo() {
        let s = "processor\t: 0\nvendor_id\t: GenuineIntel\nmodel name\t: Intel(R) Xeon(R)\nflags\t\t: fpu vme vmx ept\n\nprocessor\t: 1\nmodel name\t: other\n";
        let (model, ext) = parse_cpuinfo(s);
        assert_eq!(model.as_deref(), Some("Intel(R) Xeon(R)"));
        assert_eq!(ext, Some(VirtualizationExtension::Vmx));
    }

    #[test]
    fn parses_amd_cpuinfo() {
        let s = "processor\t: 0\nmodel name\t: AMD EPYC\nflags\t\t: fpu svm npt\n";
        assert_eq!(parse_cpuinfo(s).1, Some(VirtualizationExtension::Svm));
    }

    #[test]
    fn arm_cpuinfo_has_no_x86_flag() {
        let s =
            "processor\t: 0\nBogoMIPS\t: 50.00\nFeatures\t: fp asimd\nCPU implementer\t: 0x41\n";
        assert_eq!(parse_cpuinfo(s), (None, None));
    }
}
