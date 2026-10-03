//! Discovery against synthetic sysfs, procfs, and /etc trees.

// Fixture helpers are test code; failing loudly is the intended behavior.
#![allow(clippy::unwrap_used, clippy::print_stdout)]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use baryon_host::cpu::VirtualizationExtension;
use baryon_host::{DiscoveryOptions, HostRoot, KvmProbe, discover};

/// Builder for a fake host filesystem.
struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn root(&self) -> HostRoot {
        HostRoot::at(self.dir.path())
    }

    fn p(&self, abs: &str) -> PathBuf {
        self.dir.path().join(abs.trim_start_matches('/'))
    }

    fn file(&self, abs: &str, contents: &str) -> &Self {
        let path = self.p(abs);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
        self
    }

    fn dir(&self, abs: &str) -> &Self {
        fs::create_dir_all(self.p(abs)).unwrap();
        self
    }

    fn link(&self, abs: &str, target: &str) -> &Self {
        let path = self.p(abs);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        symlink(Path::new(target), path).unwrap();
        self
    }

    fn hugepages(&self, base: &str, size_kib: u64, total: u64, free: u64) -> &Self {
        let d = format!("{base}/hugepages-{size_kib}kB");
        self.file(&format!("{d}/nr_hugepages"), &format!("{total}\n"))
            .file(&format!("{d}/free_hugepages"), &format!("{free}\n"))
            .file(&format!("{d}/surplus_hugepages"), "0\n");
        if base == "/sys/kernel/mm/hugepages" {
            self.file(&format!("{d}/resv_hugepages"), "0\n");
        }
        self
    }

    #[allow(clippy::too_many_arguments)]
    fn pci(
        &self,
        addr: &str,
        vendor: &str,
        device: &str,
        class: &str,
        group: Option<u32>,
        driver: Option<&str>,
        numa: &str,
    ) -> &Self {
        let d = format!("/sys/bus/pci/devices/{addr}");
        self.file(&format!("{d}/vendor"), &format!("0x{vendor}\n"))
            .file(&format!("{d}/device"), &format!("0x{device}\n"))
            .file(&format!("{d}/class"), &format!("0x{class}\n"))
            .file(&format!("{d}/subsystem_vendor"), &format!("0x{vendor}\n"))
            .file(&format!("{d}/subsystem_device"), "0x0000\n")
            .file(&format!("{d}/numa_node"), &format!("{numa}\n"));
        if let Some(g) = group {
            self.link(
                &format!("{d}/iommu_group"),
                &format!("../../../kernel/iommu_groups/{g}"),
            )
            .dir(&format!("/sys/kernel/iommu_groups/{g}/devices/{addr}"))
            .file(&format!("/sys/kernel/iommu_groups/{g}/type"), "DMA-FQ\n");
        }
        if let Some(drv) = driver {
            self.link(
                &format!("{d}/driver"),
                &format!("../../../bus/pci/drivers/{drv}"),
            )
            .dir(&format!("/sys/bus/pci/drivers/{drv}"));
        }
        self
    }

    /// A two-socket Intel host with an IOMMU, hugepages, and VFIO loaded.
    fn virtualization_host() -> Self {
        let f = Self::new();
        f.file("/proc/sys/kernel/arch", "x86_64\n")
            .file("/proc/sys/kernel/osrelease", "6.12.10-hadron\n")
            .file(
                "/etc/os-release",
                "NAME=\"Hadron\"\nID=hadron\nVERSION_ID=\"0.4.0\"\nPRETTY_NAME=\"Hadron Linux 0.4.0\"\n",
            )
            .file(
                "/etc/kairos-release",
                "KAIROS_VERSION=\"v4.1.2\"\nKAIROS_FAMILY=\"hadron\"\n",
            )
            .file(
                "/proc/cpuinfo",
                "processor\t: 0\nmodel name\t: Intel(R) Xeon(R) Gold\nflags\t\t: fpu vmx ept\n\nprocessor\t: 1\n",
            )
            .file("/sys/devices/system/cpu/online", "0-7\n")
            .file("/sys/module/kvm_intel/parameters/nested", "Y\n")
            .file(
                "/proc/meminfo",
                "MemTotal:       16384000 kB\nMemFree:  1000 kB\n",
            )
            .file(
                "/sys/kernel/mm/transparent_hugepage/enabled",
                "always [madvise] never\n",
            )
            .hugepages("/sys/kernel/mm/hugepages", 2048, 512, 500)
            .hugepages("/sys/kernel/mm/hugepages", 1_048_576, 0, 0)
            .file("/sys/devices/system/node/online", "0-1\n")
            .file("/sys/devices/system/node/node0/cpulist", "0-3\n")
            .file(
                "/sys/devices/system/node/node0/meminfo",
                "Node 0 MemTotal:        8192000 kB\n",
            )
            .file("/sys/devices/system/node/node0/distance", "10 21\n")
            .hugepages("/sys/devices/system/node/node0/hugepages", 2048, 256, 250)
            .file("/sys/devices/system/node/node1/cpulist", "4-7\n")
            .file(
                "/sys/devices/system/node/node1/meminfo",
                "Node 1 MemTotal:        8192000 kB\n",
            )
            .file("/sys/devices/system/node/node1/distance", "21 10\n")
            .hugepages("/sys/devices/system/node/node1/hugepages", 2048, 256, 250)
            .dir("/sys/devices/system/node/power")
            .dir("/sys/class/iommu/dmar0")
            .dir("/sys/class/iommu/dmar1")
            .file(
                "/proc/cmdline",
                "BOOT_IMAGE=/vmlinuz root=LABEL=COS_ACTIVE intel_iommu=on iommu=pt quiet\n",
            )
            .pci(
                "0000:00:00.0",
                "8086",
                "2020",
                "060000",
                Some(0),
                None,
                "0",
            )
            .pci(
                "0000:3b:00.0",
                "8086",
                "1572",
                "020000",
                Some(7),
                Some("i40e"),
                "0",
            )
            .file("/sys/bus/pci/devices/0000:3b:00.0/sriov_totalvfs", "64\n")
            .pci(
                "0000:af:00.0",
                "10de",
                "20b5",
                "030200",
                Some(12),
                Some("vfio-pci"),
                "-1",
            )
            .dir("/sys/module/vfio")
            .dir("/sys/module/vfio_pci")
            .dir("/sys/module/iommufd")
            .file("/sys/fs/cgroup/cgroup.controllers", "cpuset cpu io memory hugetlb pids\n");
        // Device nodes cannot be created without privileges; regular files
        // stand in for them and must be reported as not usable.
        f.file("/dev/kvm", "").file("/dev/net/tun", "");
        f
    }
}

fn node_only() -> DiscoveryOptions {
    DiscoveryOptions {
        kvm_probe: KvmProbe::NodeOnly,
    }
}

#[test]
fn discovers_virtualization_host() {
    let f = Fixture::virtualization_host();
    let r = discover(&f.root(), node_only());

    assert!(
        r.problems.is_empty(),
        "unexpected problems: {:?}",
        r.problems
    );
    assert_eq!(r.architecture.as_deref(), Some("x86_64"));

    let os = r.os.as_ref().unwrap();
    assert_eq!(os.kernel_release, "6.12.10-hadron");
    assert_eq!(os.os_release.id.as_deref(), Some("hadron"));
    assert_eq!(
        os.kairos_release
            .as_ref()
            .unwrap()
            .get("KAIROS_VERSION")
            .map(String::as_str),
        Some("v4.1.2")
    );

    let cpu = r.cpu.as_ref().unwrap();
    assert_eq!(cpu.online, "0-7");
    assert_eq!(cpu.online_count, 8);
    assert_eq!(
        cpu.virtualization_extension,
        Some(VirtualizationExtension::Vmx)
    );
    assert_eq!(cpu.nested_virtualization, Some(true));
    assert_eq!(cpu.model_name.as_deref(), Some("Intel(R) Xeon(R) Gold"));

    let mem = r.memory.as_ref().unwrap();
    assert_eq!(mem.total_bytes, 16_384_000 * 1024);
    assert_eq!(mem.transparent_hugepage.as_deref(), Some("madvise"));
    assert_eq!(mem.hugepages.len(), 2);
    assert_eq!(mem.hugepages[0].page_size_bytes, 2 * 1024 * 1024);
    assert_eq!(mem.hugepages[0].total, 512);
    assert_eq!(mem.hugepages[0].free, 500);
    assert_eq!(mem.hugepages[0].reserved, Some(0));
    assert_eq!(mem.hugepages[1].page_size_bytes, 1024 * 1024 * 1024);

    let numa = r.numa.as_ref().unwrap();
    assert_eq!(numa.len(), 2);
    assert_eq!(numa[1].id, 1);
    assert_eq!(numa[1].cpus, "4-7");
    assert_eq!(numa[1].cpu_count, 4);
    assert_eq!(numa[1].distances, vec![21, 10]);
    assert_eq!(numa[0].hugepages[0].total, 256);
    assert_eq!(numa[0].hugepages[0].reserved, None);

    let iommu = r.iommu.as_ref().unwrap();
    assert!(iommu.enabled);
    assert_eq!(iommu.units, vec!["dmar0", "dmar1"]);
    assert_eq!(
        iommu.groups.iter().map(|g| g.id).collect::<Vec<_>>(),
        vec![0, 7, 12]
    );
    assert_eq!(iommu.groups[1].devices, vec!["0000:3b:00.0"]);
    assert_eq!(iommu.groups[1].domain_type.as_deref(), Some("DMA-FQ"));
    assert_eq!(iommu.kernel_parameters, vec!["intel_iommu=on", "iommu=pt"]);
    assert!(!iommu.iommufd.present);

    let vfio = r.vfio.as_ref().unwrap();
    assert!(vfio.vfio_pci_driver);
    assert_eq!(vfio.modules, vec!["vfio", "vfio_pci", "iommufd"]);
    assert!(!vfio.container.present);

    let pci = r.pci_devices.as_ref().unwrap();
    assert_eq!(pci.len(), 3);
    let nic = pci.iter().find(|d| d.address == "0000:3b:00.0").unwrap();
    assert_eq!(nic.vendor_id, "8086");
    assert_eq!(nic.base_class(), "02");
    assert_eq!(nic.driver.as_deref(), Some("i40e"));
    assert_eq!(nic.iommu_group, Some(7));
    assert_eq!(nic.numa_node, Some(0));
    assert_eq!(nic.sriov_total_vfs, Some(64));
    let gpu = pci.iter().find(|d| d.address == "0000:af:00.0").unwrap();
    assert_eq!(gpu.driver.as_deref(), Some("vfio-pci"));
    assert_eq!(gpu.numa_node, None, "-1 means no NUMA affinity");
    let bridge = pci.iter().find(|d| d.address == "0000:00:00.0").unwrap();
    assert_eq!(bridge.driver, None);

    let cg = r.cgroup.as_ref().unwrap();
    assert!(cg.unified);
    assert!(cg.controllers.contains(&"hugetlb".to_owned()));

    // Regular files are not device nodes.
    assert!(r.kvm.device.present);
    assert!(!r.kvm.usable);
    assert_eq!(
        r.kvm.reason.as_deref(),
        Some("/dev/kvm is not a character device")
    );
    assert!(r.network_devices.tun.present);
    assert!(!r.network_devices.tun.available());
    assert!(!r.network_devices.vhost_net.present);
}

#[test]
fn host_without_virtualization() {
    let f = Fixture::new();
    f.file("/proc/sys/kernel/arch", "aarch64\n")
        .file("/proc/sys/kernel/osrelease", "6.8.0\n")
        .file(
            "/proc/cpuinfo",
            "processor\t: 0\nFeatures\t: fp asimd\nCPU implementer\t: 0x41\n",
        )
        .file("/sys/devices/system/cpu/online", "0-1\n")
        .file("/proc/meminfo", "MemTotal: 2048000 kB\n")
        .file("/proc/cmdline", "root=/dev/vda1\n");
    let r = discover(&f.root(), node_only());

    assert!(
        r.problems.is_empty(),
        "unexpected problems: {:?}",
        r.problems
    );
    assert_eq!(r.architecture.as_deref(), Some("aarch64"));
    assert!(!r.kvm.usable);
    assert_eq!(r.kvm.reason.as_deref(), Some("/dev/kvm does not exist"));
    let cpu = r.cpu.as_ref().unwrap();
    assert_eq!(cpu.virtualization_extension, None);
    assert_eq!(cpu.nested_virtualization, None);
    let iommu = r.iommu.as_ref().unwrap();
    assert!(!iommu.enabled);
    assert!(iommu.groups.is_empty());
    assert!(r.numa.as_ref().unwrap().is_empty());
    assert!(r.pci_devices.as_ref().unwrap().is_empty());
    assert!(!r.cgroup.as_ref().unwrap().unified);
    assert!(r.memory.as_ref().unwrap().hugepages.is_empty());
    assert!(r.os.as_ref().unwrap().kairos_release.is_none());
}

#[test]
fn malformed_input_is_reported_per_section() {
    let f = Fixture::virtualization_host();
    f.file("/sys/devices/system/cpu/online", "7-0\n")
        .file("/sys/devices/system/node/node1/meminfo", "garbage\n");
    let r = discover(&f.root(), node_only());

    let sections: Vec<&str> = r.problems.iter().map(|p| p.section.as_str()).collect();
    assert_eq!(sections, vec!["cpu", "numa"]);
    assert!(r.cpu.is_none());
    assert!(r.numa.is_none());
    // Other sections are still discovered.
    assert!(r.memory.is_some());
    assert!(r.iommu.as_ref().unwrap().enabled);
}

#[test]
fn report_serializes_to_camel_case_json() {
    let f = Fixture::virtualization_host();
    let r = discover(&f.root(), node_only());
    let json = serde_json::to_value(&r).unwrap();
    assert_eq!(json["cpu"]["onlineCount"], 8);
    assert_eq!(json["iommu"]["enabled"], true);
    assert_eq!(json["memory"]["hugepages"][0]["pageSizeBytes"], 2_097_152);
    assert_eq!(json["kvm"]["usable"], false);
    let back: baryon_host::HostReport = serde_json::from_value(json).unwrap();
    assert_eq!(back, r);
}

/// Runs against the real host. Skipped unless BARYON_TEST_REAL_HOST=1 so that
/// CI results do not depend on the runner's hardware.
#[test]
fn real_host_smoke() {
    if std::env::var("BARYON_TEST_REAL_HOST").as_deref() != Ok("1") {
        eprintln!("skipped: set BARYON_TEST_REAL_HOST=1 to run against this host");
        return;
    }
    let r = discover(&HostRoot::system(), DiscoveryOptions::default());
    println!("{}", serde_json::to_string_pretty(&r).unwrap());
    assert!(r.architecture.is_some());
    assert!(r.cpu.is_some());
}
