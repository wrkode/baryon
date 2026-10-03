//! Cloud Hypervisor command line construction.
//!
//! Baryon starts Cloud Hypervisor with the complete VM configuration on the
//! command line because only inherited file descriptors can be passed at
//! creation time: `vm.create` over the API drops file descriptors (Cloud
//! Hypervisor v53, `vmm/src/api/http/http_endpoint.rs`).
//!
//! Values are validated before they are formatted. Cloud Hypervisor option
//! strings are comma separated `key=value` lists, so paths and identifiers
//! containing `,` or `=` are rejected rather than escaped.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};

/// Errors in a launch configuration.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LaunchError {
    #[error(
        "{field}: value {value:?} contains a character that Cloud Hypervisor option syntax cannot carry"
    )]
    UnsafeValue { field: &'static str, value: String },
    #[error("{field}: file descriptor {fd} is reserved for stdio")]
    ReservedFd { field: &'static str, fd: RawFd },
    #[error("{field}: {reason}")]
    Invalid { field: &'static str, reason: String },
}

/// Where Cloud Hypervisor serves its REST API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiSocket {
    /// Cloud Hypervisor binds the socket at this path.
    Path(PathBuf),
    /// An already bound and listening socket inherited at this descriptor.
    Fd(RawFd),
}

/// Boot payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    /// UEFI or other firmware loaded in the architecture-specific way.
    Firmware(PathBuf),
    /// Direct kernel boot.
    Kernel {
        kernel: PathBuf,
        initramfs: Option<PathBuf>,
        cmdline: String,
    },
}

/// Location of a disk image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskSource {
    /// A path in Cloud Hypervisor's mount namespace.
    Path(PathBuf),
    /// An inherited descriptor, opened by Cloud Hypervisor through
    /// `/proc/self/fd/<fd>`. The descriptor may be `O_PATH`; Cloud Hypervisor
    /// reopens it with its own credentials.
    InheritedFd(RawFd),
}

/// Disk image format. Baryon uses raw images only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageType {
    Raw,
}

impl ImageType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
        }
    }
}

/// A virtio-blk disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disk {
    pub id: String,
    pub source: DiskSource,
    pub image_type: ImageType,
    pub readonly: bool,
    pub direct: bool,
    /// Guest-visible serial, exposed as /dev/disk/by-id/virtio-<serial>.
    pub serial: Option<String>,
}

/// A virtio-net device backed by pre-opened TAP descriptors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Net {
    pub id: String,
    /// One TAP queue descriptor per queue pair.
    pub tap_fds: Vec<RawFd>,
    /// Guest MAC address.
    pub mac: MacAddr,
}

/// A MAC address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacAddr(pub [u8; 6]);

impl std::fmt::Display for MacAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let b = self.0;
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5]
        )
    }
}

impl std::str::FromStr for MacAddr {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut out = [0u8; 6];
        let mut parts = s.split(':');
        for byte in &mut out {
            let part = parts.next().ok_or_else(|| format!("invalid MAC {s:?}"))?;
            if part.len() != 2 {
                return Err(format!("invalid MAC {s:?}"));
            }
            *byte = u8::from_str_radix(part, 16).map_err(|_| format!("invalid MAC {s:?}"))?;
        }
        if parts.next().is_some() {
            return Err(format!("invalid MAC {s:?}"));
        }
        Ok(Self(out))
    }
}

/// Serial port or virtio console output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleMode {
    Off,
    Null,
    File(PathBuf),
    /// Cloud Hypervisor listens on this Unix socket. Output produced while no
    /// client is connected is buffered (1 MiB, Cloud Hypervisor v53 and later).
    Socket(PathBuf),
}

/// Seccomp mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seccomp {
    /// Kill the process on a violation.
    Enforce,
    /// Log violations only. For debugging.
    Log,
}

/// SMBIOS identity presented to the guest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Platform {
    pub system_uuid: Option<String>,
    pub system_serial_number: Option<String>,
    pub system_product_name: Option<String>,
}

/// Full launch configuration for one VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchConfig {
    pub api_socket: ApiSocket,
    pub event_monitor: PathBuf,
    pub log_file: PathBuf,
    /// 0 logs warnings, 1 info, 2 debug.
    pub verbosity: u8,
    pub vcpus: u32,
    pub memory_mib: u64,
    pub payload: Payload,
    pub disks: Vec<Disk>,
    pub nets: Vec<Net>,
    pub serial: ConsoleMode,
    pub console: ConsoleMode,
    pub platform: Platform,
    pub seccomp: Seccomp,
    pub landlock: bool,
    /// Directories Cloud Hypervisor may create and remove files in once
    /// Landlock is enforced (`--landlock-rules path=<dir>,access=rw`). Its
    /// own rules cover the files named in the configuration, but not
    /// re-creating the serial socket when the guest reboots. Ignored without
    /// `landlock`.
    pub landlock_writable_dirs: Vec<PathBuf>,
}

impl LaunchConfig {
    /// Command line arguments, not including the program name.
    pub fn to_args(&self) -> Result<Vec<OsString>, LaunchError> {
        let mut args: Vec<OsString> = Vec::new();
        let mut push = |flag: &str, value: String| {
            args.push(flag.into());
            args.push(value.into());
        };

        push(
            "--api-socket",
            match &self.api_socket {
                ApiSocket::Path(p) => format!("path={}", path_value("api_socket", p)?),
                ApiSocket::Fd(fd) => format!("fd={}", fd_value("api_socket", *fd)?),
            },
        );
        push(
            "--event-monitor",
            format!("path={}", path_value("event_monitor", &self.event_monitor)?),
        );
        push(
            "--log-file",
            path_value("log_file", &self.log_file)?.to_owned(),
        );

        if self.vcpus == 0 {
            return Err(LaunchError::Invalid {
                field: "vcpus",
                reason: "must be at least 1".into(),
            });
        }
        push("--cpus", format!("boot={}", self.vcpus));

        if self.memory_mib == 0 {
            return Err(LaunchError::Invalid {
                field: "memory_mib",
                reason: "must be at least 1".into(),
            });
        }
        push("--memory", format!("size={}M", self.memory_mib));

        match &self.payload {
            Payload::Firmware(fw) => push("--firmware", path_value("firmware", fw)?.to_owned()),
            Payload::Kernel {
                kernel,
                initramfs,
                cmdline,
            } => {
                push("--kernel", path_value("kernel", kernel)?.to_owned());
                if let Some(initramfs) = initramfs {
                    push(
                        "--initramfs",
                        path_value("initramfs", initramfs)?.to_owned(),
                    );
                }
                // The command line is a single argument, not an option list.
                push("--cmdline", cmdline.clone());
            }
        }

        for disk in &self.disks {
            push("--disk", disk_value(disk)?);
        }
        for net in &self.nets {
            push("--net", net_value(net)?);
        }

        push("--serial", console_value("serial", &self.serial)?);
        push("--console", console_value("console", &self.console)?);

        let platform = platform_value(&self.platform)?;
        if !platform.is_empty() {
            push("--platform", platform);
        }

        push(
            "--seccomp",
            match self.seccomp {
                Seccomp::Enforce => "true",
                Seccomp::Log => "log",
            }
            .to_owned(),
        );

        if self.landlock {
            args.push("--landlock".into());
            for dir in &self.landlock_writable_dirs {
                args.push("--landlock-rules".into());
                args.push(format!("path={},access=rw", path_value("landlock_rules", dir)?).into());
            }
        }
        for _ in 0..self.verbosity.min(3) {
            args.push("-v".into());
        }
        Ok(args)
    }

    /// Descriptors that must be inherited by the Cloud Hypervisor process.
    pub fn inherited_fds(&self) -> Vec<RawFd> {
        let mut fds = Vec::new();
        if let ApiSocket::Fd(fd) = self.api_socket {
            fds.push(fd);
        }
        for disk in &self.disks {
            if let DiskSource::InheritedFd(fd) = disk.source {
                fds.push(fd);
            }
        }
        for net in &self.nets {
            fds.extend(&net.tap_fds);
        }
        fds
    }
}

fn disk_value(disk: &Disk) -> Result<String, LaunchError> {
    let mut s = String::new();
    match &disk.source {
        DiskSource::Path(p) => {
            let _ = write!(s, "path={}", path_value("disk.path", p)?);
        }
        DiskSource::InheritedFd(fd) => {
            let _ = write!(s, "path=/proc/self/fd/{}", fd_value("disk.fd", *fd)?);
        }
    }
    let _ = write!(
        s,
        ",image_type={},readonly={},direct={},id={}",
        disk.image_type.as_str(),
        on_off(disk.readonly),
        on_off(disk.direct),
        ident("disk.id", &disk.id)?
    );
    if let Some(serial) = &disk.serial {
        let _ = write!(s, ",serial={}", ident("disk.serial", serial)?);
    }
    Ok(s)
}

fn net_value(net: &Net) -> Result<String, LaunchError> {
    if net.tap_fds.is_empty() {
        return Err(LaunchError::Invalid {
            field: "net.tap_fds",
            reason: "at least one TAP descriptor is required".into(),
        });
    }
    let fds = net
        .tap_fds
        .iter()
        .map(|fd| fd_value("net.tap_fds", *fd).map(|v| v.to_string()))
        .collect::<Result<Vec<_>, _>>()?
        .join(",");
    // Cloud Hypervisor requires two virtqueues (rx and tx) per TAP queue.
    Ok(format!(
        "fd=[{fds}],num_queues={},mac={},id={}",
        net.tap_fds.len() * 2,
        net.mac,
        ident("net.id", &net.id)?
    ))
}

fn console_value(field: &'static str, mode: &ConsoleMode) -> Result<String, LaunchError> {
    Ok(match mode {
        ConsoleMode::Off => "off".into(),
        ConsoleMode::Null => "null".into(),
        ConsoleMode::File(p) => format!("file={}", path_value(field, p)?),
        ConsoleMode::Socket(p) => format!("socket={}", path_value(field, p)?),
    })
}

fn platform_value(p: &Platform) -> Result<String, LaunchError> {
    let mut parts = Vec::new();
    if let Some(v) = &p.system_uuid {
        parts.push(format!("system_uuid={}", ident("platform.system_uuid", v)?));
    }
    if let Some(v) = &p.system_serial_number {
        parts.push(format!(
            "system_serial_number={}",
            smbios("platform.system_serial_number", v)?
        ));
    }
    if let Some(v) = &p.system_product_name {
        parts.push(format!(
            "system_product_name={}",
            smbios("platform.system_product_name", v)?
        ));
    }
    Ok(parts.join(","))
}

/// SMBIOS strings may contain `=` and `;` (cloud-init reads
/// `ds=nocloud;i=...;h=...` from the system serial number). Cloud Hypervisor
/// splits an option on its first `=` only, so `=` inside the value is safe;
/// commas, brackets, and quotes are not.
fn smbios<'a>(field: &'static str, s: &'a str) -> Result<&'a str, LaunchError> {
    if s.is_empty() {
        return Err(LaunchError::Invalid {
            field,
            reason: "must not be empty".into(),
        });
    }
    let bad = |c: char| matches!(c, ',' | '[' | ']' | '"') || c.is_control() || !c.is_ascii();
    if s.chars().any(bad) {
        return Err(LaunchError::UnsafeValue {
            field,
            value: s.to_owned(),
        });
    }
    Ok(s)
}

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

fn fd_value(field: &'static str, fd: RawFd) -> Result<RawFd, LaunchError> {
    if fd <= 2 {
        return Err(LaunchError::ReservedFd { field, fd });
    }
    Ok(fd)
}

fn path_value<'a>(field: &'static str, p: &'a Path) -> Result<&'a str, LaunchError> {
    let s = p.to_str().ok_or_else(|| LaunchError::UnsafeValue {
        field,
        value: p.to_string_lossy().into_owned(),
    })?;
    if !p.is_absolute() {
        return Err(LaunchError::Invalid {
            field,
            reason: format!("path {s:?} must be absolute"),
        });
    }
    check_chars(field, s)?;
    Ok(s)
}

/// Identifiers and SMBIOS strings: printable ASCII without option delimiters.
fn ident<'a>(field: &'static str, s: &'a str) -> Result<&'a str, LaunchError> {
    if s.is_empty() {
        return Err(LaunchError::Invalid {
            field,
            reason: "must not be empty".into(),
        });
    }
    check_chars(field, s)?;
    Ok(s)
}

fn check_chars(field: &'static str, s: &str) -> Result<(), LaunchError> {
    let bad = |c: char| matches!(c, ',' | '=' | '[' | ']' | '"') || c.is_control() || !c.is_ascii();
    if s.chars().any(bad) {
        return Err(LaunchError::UnsafeValue {
            field,
            value: s.to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> LaunchConfig {
        LaunchConfig {
            api_socket: ApiSocket::Fd(3),
            event_monitor: "/run/baryon/vms/u/vmm/events.json".into(),
            log_file: "/run/baryon/vms/u/vmm/ch.log".into(),
            verbosity: 1,
            vcpus: 2,
            memory_mib: 2048,
            payload: Payload::Firmware("/usr/share/cloud-hypervisor/CLOUDHV.fd".into()),
            disks: vec![Disk {
                id: "root".into(),
                source: DiskSource::InheritedFd(4),
                image_type: ImageType::Raw,
                readonly: false,
                direct: false,
                serial: Some("baryon-root".into()),
            }],
            nets: vec![Net {
                id: "net0".into(),
                tap_fds: vec![5],
                mac: "02:42:0a:f4:01:05".parse().unwrap(),
            }],
            serial: ConsoleMode::Socket("/run/baryon/vms/u/vmm/serial.sock".into()),
            console: ConsoleMode::Off,
            platform: Platform {
                system_uuid: Some("6b7f4c0e-6a39-4b8e-9d0e-2f1f0c6c4a11".into()),
                system_serial_number: Some("ds=nocloud;i=6b7f;h=vm1".into()),
                system_product_name: Some("Baryon".into()),
            },
            seccomp: Seccomp::Enforce,
            landlock: true,
            landlock_writable_dirs: vec!["/run/baryon/vms/u/vmm".into()],
        }
    }

    fn joined(cfg: &LaunchConfig) -> Vec<String> {
        cfg.to_args()
            .unwrap()
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect()
    }

    #[test]
    fn builds_firmware_boot_command_line() {
        let args = joined(&base());
        let expected: Vec<&str> = vec![
            "--api-socket",
            "fd=3",
            "--event-monitor",
            "path=/run/baryon/vms/u/vmm/events.json",
            "--log-file",
            "/run/baryon/vms/u/vmm/ch.log",
            "--cpus",
            "boot=2",
            "--memory",
            "size=2048M",
            "--firmware",
            "/usr/share/cloud-hypervisor/CLOUDHV.fd",
            "--disk",
            "path=/proc/self/fd/4,image_type=raw,readonly=off,direct=off,id=root,serial=baryon-root",
            "--net",
            "fd=[5],num_queues=2,mac=02:42:0a:f4:01:05,id=net0",
            "--serial",
            "socket=/run/baryon/vms/u/vmm/serial.sock",
            "--console",
            "off",
            "--platform",
            "system_uuid=6b7f4c0e-6a39-4b8e-9d0e-2f1f0c6c4a11,system_serial_number=ds=nocloud;i=6b7f;h=vm1,system_product_name=Baryon",
            "--seccomp",
            "true",
            "--landlock",
            "--landlock-rules",
            "path=/run/baryon/vms/u/vmm,access=rw",
            "-v",
        ];
        assert_eq!(args, expected);
    }

    #[test]
    fn lists_inherited_fds() {
        assert_eq!(base().inherited_fds(), vec![3, 4, 5]);
    }

    #[test]
    fn kernel_boot() {
        let mut cfg = base();
        cfg.payload = Payload::Kernel {
            kernel: "/k/vmlinux".into(),
            initramfs: Some("/k/initrd".into()),
            cmdline: "console=ttyS0 root=/dev/vda1 rw".into(),
        };
        let args = joined(&cfg);
        let i = args.iter().position(|a| a == "--kernel").unwrap();
        assert_eq!(
            &args[i..i + 6],
            &[
                "--kernel",
                "/k/vmlinux",
                "--initramfs",
                "/k/initrd",
                "--cmdline",
                "console=ttyS0 root=/dev/vda1 rw"
            ]
        );
    }

    #[test]
    fn rejects_option_injection() {
        let mut cfg = base();
        cfg.disks[0].source = DiskSource::Path("/a,readonly=on".into());
        assert!(matches!(
            cfg.to_args(),
            Err(LaunchError::UnsafeValue {
                field: "disk.path",
                ..
            })
        ));

        let mut cfg = base();
        cfg.disks[0].id = "root,path=/etc/shadow".into();
        assert!(matches!(
            cfg.to_args(),
            Err(LaunchError::UnsafeValue {
                field: "disk.id",
                ..
            })
        ));
    }

    #[test]
    fn smbios_rejects_option_delimiters() {
        let mut cfg = base();
        cfg.platform.system_serial_number = Some("a,iommufd=on".into());
        assert!(matches!(
            cfg.to_args(),
            Err(LaunchError::UnsafeValue {
                field: "platform.system_serial_number",
                ..
            })
        ));
    }

    #[test]
    fn rejects_stdio_fds_and_relative_paths() {
        let mut cfg = base();
        cfg.nets[0].tap_fds = vec![1];
        assert_eq!(
            cfg.to_args(),
            Err(LaunchError::ReservedFd {
                field: "net.tap_fds",
                fd: 1
            })
        );

        let mut cfg = base();
        cfg.log_file = "relative.log".into();
        assert!(matches!(
            cfg.to_args(),
            Err(LaunchError::Invalid {
                field: "log_file",
                ..
            })
        ));
    }

    #[test]
    fn multiqueue_net() {
        let mut cfg = base();
        cfg.nets[0].tap_fds = vec![5, 6];
        let args = joined(&cfg);
        assert!(args.contains(&"fd=[5,6],num_queues=4,mac=02:42:0a:f4:01:05,id=net0".to_owned()));
    }

    #[test]
    fn mac_round_trip() {
        let mac: MacAddr = "AA:bb:0c:00:ff:10".parse().unwrap();
        assert_eq!(mac.to_string(), "aa:bb:0c:00:ff:10");
        assert!("aa:bb:cc".parse::<MacAddr>().is_err());
        assert!("aa:bb:cc:dd:ee:ff:00".parse::<MacAddr>().is_err());
        assert!("aa:bb:cc:dd:ee:fg".parse::<MacAddr>().is_err());
    }
}
