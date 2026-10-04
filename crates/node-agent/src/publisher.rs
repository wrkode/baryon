//! Publishes the VirtualizationNode of this node from baryon-virtd's host
//! report and runtime information.
//!
//! The node agent is the only writer of VirtualizationNode objects. It also
//! drives the device plugin's health: the node accepts VMs only while
//! baryon-virtd answers, speaks a compatible runtime protocol, and KVM is
//! usable.

use std::path::{Path, PathBuf};
use std::time::Duration;

use k8s_openapi::api::core::v1::Node;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{OwnerReference, Time};
use k8s_openapi::jiff::Timestamp;
use kube::api::{Api, ObjectMeta, Patch, PatchParams, PostParams};
use kube::{Client, Resource};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tonic::transport::Endpoint;

use baryon_api::conditions::{self, ConditionUpdate};
use baryon_api::node::{
    CpuStatus, HugepageStatus, IommuStatus, KvmStatus, MAX_NODE_VMS, NodeVmNetwork, NodeVmStatus,
    NumaNodeStatus, PciDeviceStatus, RuntimeStatus, VfioStatus,
};
use baryon_api::{VirtualizationNode, VirtualizationNodeSpec, VirtualizationNodeStatus};
use baryon_host::HostReport;
use baryon_runtime_api::protocol;
use baryon_runtime_api::v1alpha1::node_service_client::NodeServiceClient;
use baryon_runtime_api::v1alpha1::{
    GetHostRequest, NetworkState, RuntimeInfo, VmStatus, WatchVmsRequest,
};

const FIELD_MANAGER: &str = "baryon-node-agent";

#[derive(Debug, Clone)]
pub(crate) struct PublisherConfig {
    pub node_name: String,
    pub virtd_socket: PathBuf,
    pub interval: Duration,
}

/// What baryon-virtd reported, or why it could not be asked.
#[derive(Debug)]
pub(crate) struct Observation {
    pub host: Option<HostReport>,
    pub runtime: Option<RuntimeInfo>,
    pub vm_count: Option<u32>,
    /// Status of every VM on the node, as baryon-virtd reports it.
    pub vms: Vec<VmStatus>,
    pub error: Option<String>,
}

pub(crate) async fn run(
    client: Client,
    cfg: PublisherConfig,
    health: watch::Sender<bool>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let api: Api<VirtualizationNode> = Api::all(client.clone());
    let nodes: Api<Node> = Api::all(client);
    let mut ticker = tokio::time::interval(cfg.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            _ = ticker.tick() => {}
        }
        let obs = observe(&cfg.virtd_socket).await;
        let status_ok = obs.error.is_none();
        let kvm_ok = obs.host.as_ref().is_some_and(|h| h.kvm.usable);
        let protocol_ok = obs
            .runtime
            .as_ref()
            .is_some_and(|r| runtime_compatible(r).is_ok());
        health.send_replace(status_ok && kvm_ok && protocol_ok);
        if let Err(e) = publish(&api, &nodes, &cfg.node_name, &obs).await {
            tracing::warn!(error = %e, "publishing VirtualizationNode failed");
        }
    }
}

/// Whether this node agent's cluster release can run VMs with the
/// baryon-virtd that reported `r` (same protocol package and major version).
/// Features are checked per VM by the anchor.
fn runtime_compatible(r: &RuntimeInfo) -> Result<(), String> {
    if r.protocol != baryon_runtime_api::PROTOCOL {
        return Err(format!(
            "baryon-virtd speaks runtime protocol {:?}, the cluster release {:?}",
            r.protocol,
            baryon_runtime_api::PROTOCOL
        ));
    }
    protocol::check_major(protocol::Version::of_peer(r.version.as_ref())).map_err(|e| {
        e.to_string()
            .split_once(": ")
            .map_or_else(|| e.to_string(), |(_, m)| m.to_owned())
    })
}

/// Ask baryon-virtd for the host report, runtime information, and VM count.
pub(crate) async fn observe(socket: &Path) -> Observation {
    match observe_inner(socket).await {
        Ok((host, runtime, vms)) => Observation {
            host: Some(host),
            runtime: Some(runtime),
            vm_count: Some(u32::try_from(vms.len()).unwrap_or(u32::MAX)),
            vms,
            error: None,
        },
        Err(e) => Observation {
            host: None,
            runtime: None,
            vm_count: None,
            vms: Vec::new(),
            error: Some(format!("{e:#}")),
        },
    }
}

async fn observe_inner(socket: &Path) -> anyhow::Result<(HostReport, RuntimeInfo, Vec<VmStatus>)> {
    let channel = Endpoint::try_from(format!("unix://{}", socket.display()))?
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .connect()
        .await?;
    let mut client = NodeServiceClient::new(channel);
    let info = client.get_host(GetHostRequest {}).await?.into_inner();
    if info.report_version != baryon_runtime_api::HOST_REPORT_VERSION {
        anyhow::bail!(
            "baryon-virtd reports host schema version {}, expected {}",
            info.report_version,
            baryon_runtime_api::HOST_REPORT_VERSION
        );
    }
    let host: HostReport = serde_json::from_str(&info.report_json)?;
    let runtime = info.runtime.unwrap_or_default();
    let mut vms = client.watch_vms(WatchVmsRequest {}).await?.into_inner();
    let list = vms.message().await?.map(|l| l.vms).unwrap_or_default();
    Ok((host, runtime, list))
}

async fn publish(
    api: &Api<VirtualizationNode>,
    nodes: &Api<Node>,
    node_name: &str,
    obs: &Observation,
) -> anyhow::Result<()> {
    let existing = match api.get_opt(node_name).await? {
        Some(vn) => vn,
        None => {
            let node = nodes.get(node_name).await?;
            let owner = OwnerReference {
                api_version: "v1".to_owned(),
                kind: "Node".to_owned(),
                name: node_name.to_owned(),
                uid: node.meta().uid.clone().unwrap_or_default(),
                ..Default::default()
            };
            let vn = VirtualizationNode {
                metadata: ObjectMeta {
                    name: Some(node_name.to_owned()),
                    owner_references: Some(vec![owner]),
                    labels: Some(
                        [(
                            baryon_api::keys::MANAGED_BY_LABEL.to_owned(),
                            baryon_api::keys::MANAGED_BY.to_owned(),
                        )]
                        .into(),
                    ),
                    ..Default::default()
                },
                spec: VirtualizationNodeSpec {},
                status: None,
            };
            api.create(&PostParams::default(), &vn).await?
        }
    };
    let now = Time(Timestamp::now());
    let previous = existing.status.unwrap_or_default();
    let status = build_status(obs, &previous, now);
    let patch = serde_json::json!({
        "apiVersion": "virtualization.baryon.io/v1alpha1",
        "kind": "VirtualizationNode",
        "status": status,
    });
    api.patch_status(
        node_name,
        &PatchParams::apply(FIELD_MANAGER).force(),
        &Patch::Apply(&patch),
    )
    .await?;
    Ok(())
}

/// The VirtualizationNode entry of one VM: its Anchor Pod UID and the
/// host-side network state, when baryon-virtd reports it (1.2 and later).
/// `s` cut to at most `max` bytes at a character boundary.
fn truncated(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

fn node_vm_status(vm: &VmStatus) -> NodeVmStatus {
    let time = |ms: i64| {
        (ms > 0)
            .then(|| Timestamp::from_millisecond(ms).ok().map(Time))
            .flatten()
    };
    let network = vm.network.as_ref().and_then(|n| {
        let state = match NetworkState::try_from(n.state).ok()? {
            NetworkState::Ready => "Ready",
            NetworkState::Restarting => "Restarting",
            NetworkState::Failing => "Failing",
            NetworkState::Unspecified => return None,
        };
        Some(NodeVmNetwork {
            state: state.to_owned(),
            reason: truncated(&n.reason, 64),
            message: truncated(&n.message, 1024),
            last_transition_time: time(n.since_unix_ms),
            lease_expires_time: time(n.lease_expires_unix_ms),
        })
    });
    NodeVmStatus {
        anchor_pod_uid: vm.pod_uid.clone(),
        network,
    }
}

/// Translate an observation into status. Pure, for tests.
pub(crate) fn build_status(
    obs: &Observation,
    previous: &VirtualizationNodeStatus,
    now: Time,
) -> VirtualizationNodeStatus {
    let mut s = VirtualizationNodeStatus {
        heartbeat_time: Some(now.clone()),
        ..Default::default()
    };
    let mut updates = Vec::new();
    match (&obs.host, &obs.error) {
        (Some(h), None) => {
            s.architecture = h.architecture.clone();
            if let Some(os) = &h.os {
                s.kernel_release = Some(os.kernel_release.clone());
                s.os_image = os.os_release.pretty_name.clone();
                s.kairos_version = os
                    .kairos_release
                    .as_ref()
                    .and_then(|k| k.get("KAIROS_VERSION").cloned());
            }
            s.cpu = h.cpu.as_ref().map(|c| CpuStatus {
                model: c.model_name.clone(),
                online: c.online_count,
                virtualization_extension: c.virtualization_extension.map(|e| {
                    serde_json::to_value(e)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default()
                }),
                nested_virtualization: c.nested_virtualization,
            });
            s.memory_bytes = h.memory.as_ref().map(|m| m.total_bytes);
            s.hugepages = h
                .memory
                .as_ref()
                .map(|m| {
                    m.hugepages
                        .iter()
                        .map(|p| HugepageStatus {
                            page_size_bytes: p.page_size_bytes,
                            total: p.total,
                            free: p.free,
                        })
                        .collect()
                })
                .unwrap_or_default();
            s.kvm = Some(KvmStatus {
                available: h.kvm.usable,
                api_version: h.kvm.api_version,
                reason: h.kvm.reason.clone(),
            });
            s.iommu = h.iommu.as_ref().map(|i| IommuStatus {
                enabled: i.enabled,
                groups: u32::try_from(i.groups.len()).unwrap_or(u32::MAX),
                iommufd: i.iommufd.available(),
            });
            s.vfio = h.vfio.as_ref().map(|v| VfioStatus {
                container_device: v.container.available(),
                pci_driver: v.vfio_pci_driver,
            });
            s.numa_nodes = h
                .numa
                .as_ref()
                .map(|nodes| {
                    nodes
                        .iter()
                        .map(|n| NumaNodeStatus {
                            id: n.id,
                            cpus: n.cpus.clone(),
                            memory_bytes: n.memory_total_bytes,
                        })
                        .collect()
                })
                .unwrap_or_default();
            s.pci_devices = h
                .pci_devices
                .as_ref()
                .map(|devs| {
                    devs.iter()
                        // Bridges are not useful inventory for VM placement.
                        .filter(|d| d.base_class() != "06")
                        .map(|d| PciDeviceStatus {
                            address: d.address.clone(),
                            vendor_id: d.vendor_id.clone(),
                            device_id: d.device_id.clone(),
                            class: d.class.clone(),
                            driver: d.driver.clone(),
                            iommu_group: d.iommu_group,
                            numa_node: d.numa_node,
                        })
                        .collect()
                })
                .unwrap_or_default();
            s.runtime = obs.runtime.as_ref().map(|r| RuntimeStatus {
                virtd_version: r.virtd_version.clone(),
                protocol: r.protocol.clone(),
                protocol_version: Some(protocol::Version::of_peer(r.version.as_ref()).to_string()),
                features: protocol::features_of_peer(r.version.as_ref(), &r.features),
                hypervisor: r.hypervisor.clone(),
                hypervisor_version: r.hypervisor_version.clone(),
            });
            s.vm_count = obs.vm_count;
            if obs.vms.len() > MAX_NODE_VMS {
                tracing::warn!(
                    vms = obs.vms.len(),
                    max = MAX_NODE_VMS,
                    "more VMs than a VirtualizationNode can list; publishing the first"
                );
            }
            s.vms = obs
                .vms
                .iter()
                .take(MAX_NODE_VMS)
                .map(node_vm_status)
                .collect();

            let compatible = obs.runtime.as_ref().map_or(
                Err("baryon-virtd sent no runtime information".to_owned()),
                runtime_compatible,
            );
            match &compatible {
                Ok(()) => updates.push(ConditionUpdate::new(
                    conditions::node::RUNTIME_HEALTHY,
                    true,
                    "RuntimeReachable",
                    "baryon-virtd answered and host discovery succeeded",
                )),
                Err(m) => updates.push(ConditionUpdate::new(
                    conditions::node::RUNTIME_HEALTHY,
                    false,
                    "ProtocolIncompatible",
                    m.clone(),
                )),
            }
            let (ready, reason, message) = if let Err(m) = compatible {
                (false, "ProtocolIncompatible", m)
            } else if h.kvm.usable {
                (true, "KvmAvailable", "the node can run VMs".to_owned())
            } else {
                (
                    false,
                    "KvmUnavailable",
                    h.kvm
                        .reason
                        .clone()
                        .unwrap_or_else(|| "KVM is not usable".to_owned()),
                )
            };
            updates.push(ConditionUpdate::new(
                conditions::node::READY,
                ready,
                reason,
                message,
            ));
        }
        (_, err) => {
            let message = err.clone().unwrap_or_else(|| "no host report".to_owned());
            updates.push(ConditionUpdate::new(
                conditions::node::RUNTIME_HEALTHY,
                false,
                "RuntimeUnreachable",
                message.clone(),
            ));
            updates.push(ConditionUpdate::new(
                conditions::node::READY,
                false,
                "RuntimeUnreachable",
                message,
            ));
        }
    }
    s.conditions = conditions::merge(&previous.conditions, &updates, None, now);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Time {
        Time(Timestamp::from_second(1_000).unwrap_or(Timestamp::UNIX_EPOCH))
    }

    #[test]
    fn unreachable_runtime_is_not_ready() {
        let obs = Observation {
            host: None,
            runtime: None,
            vm_count: None,
            vms: Vec::new(),
            error: Some("connection refused".into()),
        };
        let s = build_status(&obs, &VirtualizationNodeStatus::default(), now());
        let ready = conditions::find(&s.conditions, conditions::node::READY).unwrap();
        assert_eq!(ready.status, "False");
        assert_eq!(ready.reason, "RuntimeUnreachable");
        assert!(s.heartbeat_time.is_some());
        assert!(s.kvm.is_none());
    }

    #[test]
    fn reports_host_capabilities() {
        let host = baryon_host::discover(
            &baryon_host::HostRoot::system(),
            baryon_host::DiscoveryOptions {
                kvm_probe: baryon_host::KvmProbe::NodeOnly,
            },
        );
        let obs = Observation {
            host: Some(host),
            runtime: Some(RuntimeInfo {
                protocol: baryon_runtime_api::PROTOCOL.into(),
                hypervisor_version: "v53.0".into(),
                ..Default::default()
            }),
            vm_count: Some(2),
            vms: Vec::new(),
            error: None,
        };
        let s = build_status(&obs, &VirtualizationNodeStatus::default(), now());
        assert_eq!(s.vm_count, Some(2));
        assert_eq!(s.runtime.unwrap().hypervisor_version, "v53.0");
        assert!(s.architecture.is_some());
        assert!(
            conditions::find(&s.conditions, conditions::node::RUNTIME_HEALTHY)
                .is_some_and(|c| c.status == "True")
        );
        assert!(s.pci_devices.iter().all(|d| !d.class.starts_with("06")));
    }

    fn runtime(protocol: &str, version: Option<(u32, u32)>) -> RuntimeInfo {
        RuntimeInfo {
            protocol: protocol.into(),
            version: version.map(
                |(major, minor)| baryon_runtime_api::v1alpha1::ProtocolVersion { major, minor },
            ),
            features: vec!["disk.file".into()],
            ..Default::default()
        }
    }

    #[test]
    fn runtime_protocol_compatibility() {
        let p = baryon_runtime_api::PROTOCOL;
        // A 1.0 baryon-virtd reports no version.
        assert!(runtime_compatible(&runtime(p, None)).is_ok());
        assert!(runtime_compatible(&runtime(p, Some((1, 0)))).is_ok());
        assert!(runtime_compatible(&runtime(p, Some((1, 42)))).is_ok());
        assert!(runtime_compatible(&runtime(p, Some((2, 0)))).is_err());
        assert!(runtime_compatible(&runtime("baryon.runtime.v2", Some((2, 0)))).is_err());
    }

    #[test]
    fn an_incompatible_runtime_is_not_ready() {
        let host = baryon_host::discover(
            &baryon_host::HostRoot::system(),
            baryon_host::DiscoveryOptions {
                kvm_probe: baryon_host::KvmProbe::NodeOnly,
            },
        );
        let obs = Observation {
            host: Some(host),
            runtime: Some(runtime(baryon_runtime_api::PROTOCOL, Some((2, 0)))),
            vm_count: Some(0),
            vms: Vec::new(),
            error: None,
        };
        let s = build_status(&obs, &VirtualizationNodeStatus::default(), now());
        for t in [conditions::node::RUNTIME_HEALTHY, conditions::node::READY] {
            let c = conditions::find(&s.conditions, t).unwrap();
            assert_eq!(
                (c.status.as_str(), c.reason.as_str()),
                ("False", "ProtocolIncompatible")
            );
        }
        let r = s.runtime.unwrap();
        assert_eq!(r.protocol_version.as_deref(), Some("2.0"));
        assert_eq!(r.features, ["disk.file"]);
    }

    #[test]
    fn a_1_0_runtime_is_reported_with_base_features() {
        let host = baryon_host::discover(
            &baryon_host::HostRoot::system(),
            baryon_host::DiscoveryOptions {
                kvm_probe: baryon_host::KvmProbe::NodeOnly,
            },
        );
        let obs = Observation {
            host: Some(host),
            runtime: Some(runtime(baryon_runtime_api::PROTOCOL, None)),
            vm_count: Some(0),
            vms: Vec::new(),
            error: None,
        };
        let s = build_status(&obs, &VirtualizationNodeStatus::default(), now());
        let r = s.runtime.unwrap();
        assert_eq!(r.protocol_version.as_deref(), Some("1.0"));
        assert_eq!(r.features, protocol::BASE_FEATURES);
        assert!(
            conditions::find(&s.conditions, conditions::node::RUNTIME_HEALTHY)
                .is_some_and(|c| c.status == "True")
        );
    }

    #[test]
    fn vm_network_state_is_published_by_anchor_pod_uid() {
        use baryon_runtime_api::v1alpha1::NetworkStatus;
        let vm = |uid: &str, network: Option<NetworkStatus>| VmStatus {
            pod_uid: uid.into(),
            vm_name: "secret-name".into(),
            vm_namespace: "secret-ns".into(),
            network,
            ..Default::default()
        };
        let failing = node_vm_status(&vm(
            "pod-1",
            Some(NetworkStatus {
                state: NetworkState::Failing as i32,
                reason: "DhcpResponderFailing".into(),
                message: "m".into(),
                since_unix_ms: 1_000_000,
                consecutive_failures: 11,
                last_ack_unix_ms: 0,
                lease_expires_unix_ms: 0,
            }),
        ));
        assert_eq!(failing.anchor_pod_uid, "pod-1");
        let n = failing.network.as_ref().unwrap();
        assert_eq!(
            (n.state.as_str(), n.reason.as_str()),
            ("Failing", "DhcpResponderFailing")
        );
        assert!(n.last_transition_time.is_some());
        assert!(n.lease_expires_time.is_none());
        // A 1.1 baryon-virtd sends no network status.
        assert!(node_vm_status(&vm("pod-2", None)).network.is_none());
        // The entry carries no names or namespaces.
        let json = serde_json::to_string(&failing).unwrap();
        assert!(!json.contains("secret"));
    }
}
