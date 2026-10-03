//! Kubelet device plugins for the `virtualization.baryon.io/vm` and
//! `virtualization.baryon.io/import` resources.
//!
//! Every node with a healthy Baryon runtime advertises a fixed number of VM
//! slots. Anchor Pods request one slot. This gives Baryon three things
//! through one standard kubelet mechanism:
//!
//! - scheduling: Anchor Pods only fit on nodes whose runtime is healthy
//!   (kubelet sets allocatable to zero when the devices are unhealthy)
//! - /dev/kvm in the Anchor container's device list, so a VMM placed in the
//!   container cgroup may use it
//! - the baryon-virtd anchor socket mounted into the container without a
//!   hostPath volume in the pod spec
//!
//! The import resource has the same slots and health but grants nothing: it
//! only places the import Pod on a node that can run the VM, without
//! /dev/kvm or the anchor socket.
//!
//! Protocol: k8s.io/kubelet/pkg/apis/deviceplugin/v1beta1.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use futures::Stream;
use tokio::net::UnixListener;
use tokio::sync::watch;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Endpoint, Server};
use tonic::{Request, Response, Status};

/// Generated kubelet device plugin API.
#[allow(
    missing_debug_implementations,
    unreachable_pub,
    clippy::all,
    clippy::pedantic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]
pub mod api {
    tonic::include_proto!("v1beta1");
}

use api::device_plugin_server::{DevicePlugin, DevicePluginServer};
use api::registration_client::RegistrationClient;
use api::{
    AllocateRequest, AllocateResponse, ContainerAllocateResponse, Device, DevicePluginOptions,
    DeviceSpec, Empty, ListAndWatchResponse, Mount, PreStartContainerRequest,
    PreStartContainerResponse, PreferredAllocationRequest, PreferredAllocationResponse,
    RegisterRequest,
};

const API_VERSION: &str = "v1beta1";
const KUBELET_SOCKET: &str = "kubelet.sock";

/// Device plugin settings.
#[derive(Debug, Clone)]
pub(crate) struct PluginConfig {
    /// kubelet device plugin directory, for example
    /// /var/lib/kubelet/device-plugins. The kubelet uses this fixed path whatever
    /// its root directory is (k0s included).
    pub plugin_dir: PathBuf,
    /// Socket file name in `plugin_dir`.
    pub socket_name: String,
    pub resource_name: String,
    /// Whether an allocation grants /dev/kvm and the anchor socket.
    pub grants: bool,
    pub slots: u32,
    pub kvm_device: PathBuf,
    /// Host directory holding the baryon-virtd anchor socket.
    pub anchor_socket_dir: PathBuf,
    /// Where that directory appears inside the Anchor container.
    pub anchor_mount_path: String,
}

#[derive(Debug, Clone)]
struct Plugin {
    cfg: PluginConfig,
    health: watch::Receiver<bool>,
}

type DeviceStream = Pin<Box<dyn Stream<Item = Result<ListAndWatchResponse, Status>> + Send>>;

impl Plugin {
    fn devices(&self, healthy: bool) -> Vec<Device> {
        let health = if healthy { "Healthy" } else { "Unhealthy" };
        (0..self.cfg.slots)
            .map(|i| Device {
                id: format!("vm-{i:04}"),
                health: health.to_owned(),
                topology: None,
            })
            .collect()
    }
}

#[tonic::async_trait]
impl DevicePlugin for Plugin {
    type ListAndWatchStream = DeviceStream;

    async fn get_device_plugin_options(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<DevicePluginOptions>, Status> {
        Ok(Response::new(DevicePluginOptions::default()))
    }

    async fn list_and_watch(&self, _req: Request<Empty>) -> Result<Response<DeviceStream>, Status> {
        let plugin = self.clone();
        let mut health = self.health.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tokio::spawn(async move {
            loop {
                let healthy = *health.borrow_and_update();
                tracing::info!(healthy, "advertising VM slots to kubelet");
                let resp = ListAndWatchResponse {
                    devices: plugin.devices(healthy),
                };
                if tx.send(Ok(resp)).await.is_err() {
                    return;
                }
                tokio::select! {
                    r = health.changed() => if r.is_err() { return },
                    () = tx.closed() => return,
                }
            }
        });
        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        )))
    }

    async fn get_preferred_allocation(
        &self,
        _req: Request<PreferredAllocationRequest>,
    ) -> Result<Response<PreferredAllocationResponse>, Status> {
        Err(Status::unimplemented("no preferred allocation"))
    }

    async fn allocate(
        &self,
        req: Request<AllocateRequest>,
    ) -> Result<Response<AllocateResponse>, Status> {
        let n = req.into_inner().container_requests.len();
        if !self.cfg.grants {
            return Ok(Response::new(AllocateResponse {
                container_responses: vec![ContainerAllocateResponse::default(); n],
            }));
        }
        let one = ContainerAllocateResponse {
            devices: vec![DeviceSpec {
                container_path: "/dev/kvm".to_owned(),
                host_path: self.cfg.kvm_device.display().to_string(),
                permissions: "rw".to_owned(),
            }],
            mounts: vec![Mount {
                container_path: self.cfg.anchor_mount_path.clone(),
                host_path: self.cfg.anchor_socket_dir.display().to_string(),
                read_only: true,
            }],
            ..Default::default()
        };
        Ok(Response::new(AllocateResponse {
            container_responses: vec![one; n],
        }))
    }

    async fn pre_start_container(
        &self,
        _req: Request<PreStartContainerRequest>,
    ) -> Result<Response<PreStartContainerResponse>, Status> {
        Ok(Response::new(PreStartContainerResponse {}))
    }
}

/// Serve the plugin and keep it registered with kubelet. kubelet removes
/// plugin sockets when it restarts; the plugin then serves and registers
/// again.
pub(crate) async fn run(
    cfg: PluginConfig,
    health: watch::Receiver<bool>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let socket = cfg.plugin_dir.join(&cfg.socket_name);
    while !cancel.is_cancelled() {
        let _ = std::fs::remove_file(&socket);
        let listener = match UnixListener::bind(&socket) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(socket = %socket.display(), error = %e, "cannot bind device plugin socket");
                if sleep_or_cancel(Duration::from_secs(5), &cancel).await {
                    return Ok(());
                }
                continue;
            }
        };
        let serving = CancellationToken::new();
        let server = Server::builder()
            .add_service(DevicePluginServer::new(Plugin {
                cfg: cfg.clone(),
                health: health.clone(),
            }))
            .serve_with_incoming_shutdown(
                UnixListenerStream::new(listener),
                serving.clone().cancelled_owned(),
            );
        let server = tokio::spawn(server);

        match register(&cfg).await {
            Ok(()) => {
                tracing::info!(resource = %cfg.resource_name, slots = cfg.slots, "registered with kubelet")
            }
            Err(e) => tracing::warn!(error = %e, "kubelet registration failed; retrying"),
        }

        // Re-register when kubelet removed the socket (kubelet restart) or the
        // server stopped.
        loop {
            if sleep_or_cancel(Duration::from_secs(5), &cancel).await {
                serving.cancel();
                let _ = server.await;
                return Ok(());
            }
            if !socket.exists() || server.is_finished() {
                tracing::info!("device plugin socket gone; registering again");
                break;
            }
        }
        serving.cancel();
        let _ = server.await;
    }
    Ok(())
}

async fn register(cfg: &PluginConfig) -> anyhow::Result<()> {
    let kubelet = cfg.plugin_dir.join(KUBELET_SOCKET);
    let channel = Endpoint::try_from(format!("unix://{}", kubelet.display()))?
        .connect_timeout(Duration::from_secs(5))
        .connect()
        .await?;
    RegistrationClient::new(channel)
        .register(RegisterRequest {
            version: API_VERSION.to_owned(),
            endpoint: cfg.socket_name.clone(),
            resource_name: cfg.resource_name.clone(),
            options: Some(DevicePluginOptions::default()),
        })
        .await?;
    Ok(())
}

/// Returns true when cancelled.
async fn sleep_or_cancel(d: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        () = tokio::time::sleep(d) => false,
        () = cancel.cancelled() => true,
    }
}

/// The plugin directory must exist; kubelet creates it.
pub(crate) fn check_plugin_dir(dir: &Path) -> anyhow::Result<()> {
    if !dir.join(KUBELET_SOCKET).exists() {
        anyhow::bail!(
            "{} has no kubelet.sock; is the kubelet device plugin directory mounted?",
            dir.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(healthy: bool) -> Plugin {
        Plugin {
            cfg: PluginConfig {
                plugin_dir: "/var/lib/kubelet/device-plugins".into(),
                socket_name: "baryon-vm.sock".into(),
                resource_name: "virtualization.baryon.io/vm".into(),
                grants: true,
                slots: 3,
                kvm_device: "/dev/kvm".into(),
                anchor_socket_dir: "/run/baryon/anchor".into(),
                anchor_mount_path: "/run/baryon/anchor".into(),
            },
            health: watch::channel(healthy).1,
        }
    }

    #[test]
    fn advertises_slots_with_health() {
        let d = plugin(true).devices(true);
        assert_eq!(d.len(), 3);
        assert_eq!(d[2].id, "vm-0002");
        assert!(
            plugin(false)
                .devices(false)
                .iter()
                .all(|d| d.health == "Unhealthy")
        );
    }

    #[tokio::test]
    async fn allocate_grants_kvm_and_socket() {
        let resp = plugin(true)
            .allocate(Request::new(AllocateRequest {
                container_requests: vec![Default::default()],
            }))
            .await
            .unwrap()
            .into_inner();
        let c = &resp.container_responses[0];
        assert_eq!(c.devices[0].container_path, "/dev/kvm");
        assert_eq!(c.devices[0].permissions, "rw");
        assert_eq!(c.mounts[0].host_path, "/run/baryon/anchor");
        assert!(c.mounts[0].read_only);
    }

    #[tokio::test]
    async fn the_import_resource_grants_nothing() {
        let mut p = plugin(true);
        p.cfg.grants = false;
        let resp = p
            .allocate(Request::new(AllocateRequest {
                container_requests: vec![Default::default(), Default::default()],
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.container_responses.len(), 2);
        for c in &resp.container_responses {
            assert!(c.devices.is_empty() && c.mounts.is_empty() && c.envs.is_empty());
        }
    }
}
