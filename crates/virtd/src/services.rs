//! gRPC services served by baryon-virtd.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::Stream;
use tokio::sync::watch;
use tonic::{Request, Response, Status};

use baryon_runtime_api::v1alpha1::anchor_service_server::AnchorService;
use baryon_runtime_api::v1alpha1::node_service_server::NodeService;
use baryon_runtime_api::v1alpha1::{
    AttachRequest, GetHostRequest, GetRuntimeInfoRequest, HostInfo, RuntimeInfo, StopRequest,
    StopResponse, VmList, VmPhase, VmStatus, WatchVmsRequest,
};

use crate::peer::{AnchorPeer, PeerInfo};
use crate::vm::{AttachError, Manager, SessionGuard};

/// Longest guest shutdown an anchor may request.
const MAX_GRACE: Duration = Duration::from_secs(600);

type StatusStream = Pin<Box<dyn Stream<Item = Result<VmStatus, Status>> + Send>>;
type VmListStream = Pin<Box<dyn Stream<Item = Result<VmList, Status>> + Send>>;

fn anchor_peer<T>(req: &Request<T>) -> Result<Arc<Result<AnchorPeer, String>>, Status> {
    let info = req
        .extensions()
        .get::<PeerInfo>()
        .ok_or_else(|| Status::unauthenticated("no peer information on this connection"))?;
    if let Err(e) = info.anchor.as_ref() {
        return Err(Status::permission_denied(format!(
            "caller pid {} uid {} is not an anchor process: {e}",
            info.pid, info.uid
        )));
    }
    Ok(info.anchor.clone())
}

/// NodeService is for baryon-node-agent, which runs as uid 0 (without
/// capabilities). The socket mode already limits it to that uid; the check
/// keeps a misconfigured mode from exposing host inventory and the VM list.
fn require_node_agent<T>(req: &Request<T>) -> Result<(), Status> {
    let info = req
        .extensions()
        .get::<PeerInfo>()
        .ok_or_else(|| Status::unauthenticated("no peer information on this connection"))?;
    if info.uid != 0 {
        return Err(Status::permission_denied(format!(
            "NodeService is restricted to uid 0; caller pid {} uid {}",
            info.pid, info.uid
        )));
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Anchor {
    pub manager: Arc<Manager>,
}

#[tonic::async_trait]
impl AnchorService for Anchor {
    type AttachStream = StatusStream;

    async fn attach(&self, req: Request<AttachRequest>) -> Result<Response<StatusStream>, Status> {
        let peer = anchor_peer(&req)?;
        let Ok(peer) = peer.as_ref() else {
            return Err(Status::internal("peer check inconsistent"));
        };
        let req = req.into_inner();
        if req.protocol != baryon_runtime_api::PROTOCOL {
            return Err(Status::failed_precondition(format!(
                "anchor speaks protocol {:?}, this baryon-virtd speaks {:?}; the cluster release and the host image are incompatible",
                req.protocol,
                baryon_runtime_api::PROTOCOL
            )));
        }
        let spec = req
            .spec
            .ok_or_else(|| Status::invalid_argument("spec is required"))?;
        tracing::info!(pod = %peer.cgroup.pod_uid, vm = %spec.vm_name, ns = %spec.vm_namespace, pid = peer.pid, "attach");
        let (rx, guard) = self.manager.attach(peer, spec).map_err(|e| match e {
            AttachError::InvalidSpec(_) => Status::invalid_argument(e.to_string()),
            AttachError::SpecMismatch | AttachError::IdentityConflict => {
                Status::failed_precondition(e.to_string())
            }
            AttachError::IdentityMismatch { .. } => Status::permission_denied(e.to_string()),
            AttachError::Internal(_) => Status::internal(e.to_string()),
        })?;
        Ok(Response::new(Box::pin(status_stream(rx, guard))))
    }

    async fn stop(&self, req: Request<StopRequest>) -> Result<Response<StopResponse>, Status> {
        let peer = anchor_peer(&req)?;
        let Ok(peer) = peer.as_ref() else {
            return Err(Status::internal("peer check inconsistent"));
        };
        let pod_uid = peer.cgroup.pod_uid.clone();
        let r = req.into_inner();
        let grace = Duration::from_secs(u64::from(r.grace_period_seconds)).min(MAX_GRACE);
        tracing::info!(pod = %pod_uid, grace_s = grace.as_secs(), final_ = r.r#final, "stop");
        let status = self
            .manager
            .stop(&pod_uid, grace, r.r#final)
            .await
            .unwrap_or_else(|| VmStatus {
                pod_uid,
                phase: VmPhase::Stopped as i32,
                reason: "NotRunning".into(),
                message: "no VM is running for this pod".into(),
                ..Default::default()
            });
        Ok(Response::new(StopResponse {
            status: Some(status),
        }))
    }
}

/// Stream status updates until the VM has stopped. Dropping the stream (the
/// anchor disconnected) ends the session.
fn status_stream(
    mut rx: watch::Receiver<VmStatus>,
    guard: SessionGuard,
) -> impl Stream<Item = Result<VmStatus, Status>> + Send {
    channel_stream(move |tx| async move {
        let _guard = guard;
        loop {
            let status = rx.borrow_and_update().clone();
            let terminal =
                status.phase == VmPhase::Stopped as i32 || status.phase == VmPhase::Failed as i32;
            if tx.send(Ok(status)).await.is_err() {
                return;
            }
            if terminal {
                return;
            }
            tokio::select! {
                r = rx.changed() => if r.is_err() { return },
                () = tx.closed() => return,
            }
        }
    })
}

/// Run a producer task that sends into a bounded channel and expose the
/// receiver as a stream. The producer stops when the consumer is dropped.
fn channel_stream<T, F, Fut>(f: F) -> impl Stream<Item = T> + Send
where
    T: Send + 'static,
    F: FnOnce(tokio::sync::mpsc::Sender<T>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tokio::spawn(f(tx));
    tokio_stream::wrappers::ReceiverStream::new(rx)
}

#[derive(Debug, Clone)]
pub struct Node {
    pub manager: Arc<Manager>,
    pub runtime: RuntimeInfo,
}

#[tonic::async_trait]
impl NodeService for Node {
    type WatchVmsStream = VmListStream;

    async fn get_runtime_info(
        &self,
        req: Request<GetRuntimeInfoRequest>,
    ) -> Result<Response<RuntimeInfo>, Status> {
        require_node_agent(&req)?;
        Ok(Response::new(self.runtime.clone()))
    }

    async fn get_host(&self, req: Request<GetHostRequest>) -> Result<Response<HostInfo>, Status> {
        require_node_agent(&req)?;
        let report = tokio::task::spawn_blocking(|| {
            baryon_host::discover(
                &baryon_host::HostRoot::system(),
                baryon_host::DiscoveryOptions::default(),
            )
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
        let json = serde_json::to_string(&report).map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(HostInfo {
            report_version: baryon_runtime_api::HOST_REPORT_VERSION,
            report_json: json,
            runtime: Some(self.runtime.clone()),
        }))
    }

    async fn watch_vms(
        &self,
        req: Request<WatchVmsRequest>,
    ) -> Result<Response<VmListStream>, Status> {
        require_node_agent(&req)?;
        let manager = self.manager.clone();
        let mut changes = manager.changes();
        let stream = channel_stream(move |tx| async move {
            loop {
                changes.borrow_and_update();
                let list = VmList {
                    vms: manager.list(),
                };
                if tx.send(Ok(list)).await.is_err() {
                    return;
                }
                tokio::select! {
                    r = changes.changed() => if r.is_err() { return },
                    () = tx.closed() => return,
                }
            }
        });
        Ok(Response::new(Box::pin(stream)))
    }
}
