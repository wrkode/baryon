//! baryon-controller: reconciles VirtualMachines into Anchor Pods and root
//! disk claims. It runs unprivileged, mounts nothing from the host, and never
//! talks to baryon-virtd.

mod anchor;
mod reconcile;
mod resources;
mod status;

use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use kube::Api;
use kube::ResourceExt;
use kube::runtime::controller::{Config as ControllerConfig, Controller};
use kube::runtime::events::{Recorder, Reporter};
use kube::runtime::reflector::ObjectRef;
use kube::runtime::watcher;

use baryon_api::{VirtualMachine, VirtualMachineClass, keys};

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
    tracing::info!(build = %BUILD, anchor_image = %args.anchor_image, "starting");

    let client = kube::Client::try_default()
        .await
        .context("creating Kubernetes client")?;
    let reporter = Reporter {
        controller: "baryon-controller".to_owned(),
        instance: std::env::var("POD_NAME").ok(),
    };
    let ctx = Arc::new(reconcile::Context {
        client: client.clone(),
        anchor_image: args.anchor_image,
        recorder: Recorder::new(client.clone(), reporter),
    });

    let managed = format!("{}={}", keys::MANAGED_BY_LABEL, keys::MANAGED_BY);
    let vms: Api<VirtualMachine> = Api::all(client.clone());
    let controller = Controller::new(vms, watcher::Config::default());
    let store = controller.store();
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
        .with_config(ControllerConfig::default().concurrency(args.concurrency))
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
