//! baryon-controller: reconciles VirtualMachines into Anchor Pods and root
//! disk claims. It runs unprivileged, mounts nothing from the host, and never
//! talks to baryon-virtd.

mod anchor;
mod disk_import;
mod nodes;
mod reconcile;
mod resources;
mod status;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use clap::Parser;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use kube::Api;
use kube::ResourceExt;
use kube::runtime::controller::{Config as ControllerConfig, Controller};
use kube::runtime::events::{Recorder, Reporter};
use kube::runtime::reflector::{self, ObjectRef};
use kube::runtime::utils::Backoff;
use kube::runtime::{WatchStreamExt as _, watcher};

use baryon_api::{VirtualMachine, VirtualMachineClass, VirtualizationNode, keys};

const BUILD: baryon_version::BuildInfo = baryon_version::BuildInfo::new("baryon-controller");

#[derive(Debug, Parser)]
#[command(
    name = "baryon-controller",
    version,
    about = "Baryon VirtualMachine controller"
)]
struct Args {
    /// Image of the Anchor Pod container (baryon-anchor).
    #[arg(long, env = "BARYON_ANCHOR_IMAGE")]
    anchor_image: String,
    /// Image of the import Pod container (baryon-importer). It is never
    /// used for Anchor Pods.
    #[arg(long, env = "BARYON_IMPORTER_IMAGE")]
    importer_image: String,
    /// Concurrent reconciles.
    #[arg(long, default_value_t = 8)]
    concurrency: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .flatten_event(true)
        .init();
    tracing::info!(build = %BUILD, anchor_image = %args.anchor_image, importer_image = %args.importer_image, "starting");

    let client = kube::Client::try_default()
        .await
        .context("creating Kubernetes client")?;
    let reporter = Reporter {
        controller: "baryon-controller".to_owned(),
        instance: std::env::var("POD_NAME").ok(),
    };
    // VirtualizationNodes, for NetworkReady: a reflector the reconciler
    // reads, and the time each node's heartbeat was last seen.
    let (node_reader, node_writer) = reflector::store();
    let node_view = Arc::new(nodes::NodeView::new(node_reader.clone()));
    let view = node_view.clone();
    tokio::spawn(
        watcher(
            Api::<VirtualizationNode>::all(client.clone()),
            watcher::Config::default(),
        )
        .backoff(TriggerBackoff::default())
        .reflect(node_writer)
        .applied_objects()
        .for_each(move |r| {
            match r {
                Ok(n) => view.saw(&n),
                Err(e) => tracing::warn!(error = %e, "VirtualizationNode watch error"),
            }
            futures::future::ready(())
        }),
    );
    // Without the nodes, every running VM would briefly report NetworkReady
    // Unknown after a controller restart.
    if tokio::time::timeout(Duration::from_secs(30), node_reader.wait_until_ready())
        .await
        .is_err()
    {
        tracing::warn!("VirtualizationNodes not listed after 30 s; starting without them");
    }
    // A node whose report goes stale or fresh changes nothing the trigger
    // sees: check periodically and reconcile all VMs when that happens.
    let (stale_tx, stale_rx) = tokio::sync::mpsc::channel::<()>(1);
    let view = node_view.clone();
    tokio::spawn(async move {
        let mut fresh = view.fresh_nodes();
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        loop {
            tick.tick().await;
            let now = view.fresh_nodes();
            // Both directions: a node that stops reporting, and one that
            // reports again (its heartbeat alone triggers nothing).
            if now != fresh {
                tracing::info!(
                    "a VirtualizationNode stopped or resumed reporting; reconciling VMs"
                );
                let _ = stale_tx.try_send(());
            }
            fresh = now;
        }
    });
    let stale = futures::stream::unfold(stale_rx, |mut rx| async move {
        rx.recv().await.map(|()| ((), rx))
    });

    let ctx = Arc::new(reconcile::Context {
        client: client.clone(),
        anchor_image: args.anchor_image,
        importer_image: args.importer_image,
        recorder: Recorder::new(client.clone(), reporter),
        nodes: node_view.clone(),
    });

    let managed = format!("{}={}", keys::MANAGED_BY_LABEL, keys::MANAGED_BY);
    let vms: Api<VirtualMachine> = Api::all(client.clone());
    let controller = Controller::new(vms, watcher::Config::default());
    let store = controller.store();
    let node_store = controller.store();
    controller
        .owns(
            Api::<Pod>::all(client.clone()),
            watcher::Config::default().labels(&managed),
        )
        .owns(
            Api::<PersistentVolumeClaim>::all(client.clone()),
            watcher::Config::default().labels(&managed),
        )
        .watches(
            Api::<VirtualMachineClass>::all(client.clone()),
            watcher::Config::default(),
            move |class| {
                let name = class.name_any();
                store
                    .state()
                    .into_iter()
                    .filter(|vm| vm.spec.class_name == name)
                    .map(|vm| ObjectRef::from_obj(&*vm))
                    .collect::<Vec<_>>()
            },
        )
        // A node's VirtualizationNode carries the host-side state of its
        // VMs (NetworkReady). Heartbeat-only updates trigger nothing.
        .watches(
            Api::<VirtualizationNode>::all(client.clone()),
            watcher::Config::default(),
            move |node| {
                if !node_view.changed(&node) {
                    return Vec::new();
                }
                let name = node.name_any();
                node_store
                    .state()
                    .into_iter()
                    .filter(|vm| {
                        vm.status.as_ref().and_then(|s| s.node_name.as_deref())
                            == Some(name.as_str())
                    })
                    .map(|vm| ObjectRef::from_obj(&*vm))
                    .collect::<Vec<_>>()
            },
        )
        .reconcile_all_on(stale)
        .with_config(ControllerConfig::default().concurrency(args.concurrency))
        .trigger_backoff(TriggerBackoff::default())
        .shutdown_on_signal()
        .run(reconcile::reconcile, reconcile::error_policy, ctx)
        .for_each(|result| async move {
            match result {
                Ok((obj, _)) => tracing::debug!(vm = %obj.name, ns = ?obj.namespace, "reconciled"),
                Err(kube::runtime::controller::Error::ReconcilerFailed(e, obj)) => {
                    tracing::debug!(vm = %obj.name, error = %e, "reconcile error");
                }
                Err(e) => tracing::warn!(error = %e, "controller error"),
            }
        })
        .await;
    tracing::info!("stopped");
    Ok(())
}

/// Backoff of the controller's merged watch streams.
///
/// kube-runtime backs off the merged stream of all of a controller's watches
/// after an error in any one of them, and no watch makes progress while it
/// waits. After an API server restart every watch fails once with 410
/// Expired, and with kube-runtime's default (up to 30 s, reset only after
/// 120 s without errors) those failures add up to minutes in which no VM is
/// reconciled. Short delays bound the pause per error; each watch still
/// makes at most one request per delay while the API server is down.
#[derive(Debug, Clone)]
struct TriggerBackoff {
    next: Duration,
}

impl TriggerBackoff {
    const MIN: Duration = Duration::from_millis(500);
    const MAX: Duration = Duration::from_secs(5);
}

impl Default for TriggerBackoff {
    fn default() -> Self {
        Self { next: Self::MIN }
    }
}

impl Iterator for TriggerBackoff {
    type Item = Duration;

    fn next(&mut self) -> Option<Duration> {
        let d = self.next;
        self.next = (d * 2).min(Self::MAX);
        Some(d)
    }
}

impl Backoff for TriggerBackoff {
    fn reset(&mut self) {
        self.next = Self::MIN;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_backoff_is_short_bounded_and_resets() {
        let mut b = TriggerBackoff::default();
        let delays: Vec<Duration> = b.by_ref().take(6).collect();
        assert_eq!(delays[0], Duration::from_millis(500));
        assert_eq!(delays[1], Duration::from_secs(1));
        assert!(delays.iter().all(|d| *d <= TriggerBackoff::MAX));
        assert_eq!(delays[5], TriggerBackoff::MAX);
        b.reset();
        assert_eq!(b.next(), Some(TriggerBackoff::MIN));
    }
}
