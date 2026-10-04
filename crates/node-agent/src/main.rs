//! baryon-node-agent: the Kubernetes-facing Baryon component on each node.
//!
//! It runs as a DaemonSet, talks to baryon-virtd over the node socket, and:
//!
//! - publishes this node's VirtualizationNode
//! - serves a kubelet device plugin for `virtualization.baryon.io/vm`
//!
//! It holds no host privileges beyond the sockets it is given. baryon-virtd
//! itself never talks to Kubernetes.

mod deviceplugin;
mod publisher;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use tokio_util::sync::CancellationToken;

const BUILD: baryon_version::BuildInfo = baryon_version::BuildInfo::new("baryon-node-agent");

#[derive(Debug, Parser)]
#[command(name = "baryon-node-agent", version, about = "Baryon node agent")]
struct Args {
    /// Name of this node (from the downward API).
    #[arg(long, env = "NODE_NAME")]
    node_name: String,
    #[arg(long, env = "BARYON_VIRTD_NODE_SOCKET", default_value = baryon_runtime_api::NODE_SOCKET)]
    virtd_socket: PathBuf,
    /// kubelet device plugin directory as mounted into this container.
    #[arg(long, default_value = "/var/lib/kubelet/device-plugins")]
    device_plugin_dir: PathBuf,
    /// VM slots advertised per node; at most the number of VM entries a
    /// VirtualizationNode can hold.
    #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u32).range(1..=baryon_api::node::MAX_NODE_VMS as i64))]
    vm_slots: u32,
    /// /dev/kvm on the host.
    #[arg(long, default_value = "/dev/kvm")]
    kvm_device: PathBuf,
    /// Host directory with the baryon-virtd anchor socket.
    #[arg(long, default_value = "/run/baryon/anchor")]
    anchor_socket_dir: PathBuf,
    /// Seconds between VirtualizationNode refreshes. baryon-controller treats
    /// a node it has not seen refreshed for 90 s as not reporting, so the
    /// interval is at most 30 s.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(5..=30))]
    status_interval_seconds: u64,
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
    tracing::info!(build = %BUILD, node = %args.node_name, "starting");

    let client = kube::Client::try_default()
        .await
        .context("creating Kubernetes client")?;
    let cancel = CancellationToken::new();
    let (health_tx, health_rx) = tokio::sync::watch::channel(false);

    deviceplugin::check_plugin_dir(&args.device_plugin_dir)?;
    let vm_plugin = deviceplugin::PluginConfig {
        plugin_dir: args.device_plugin_dir.clone(),
        socket_name: "baryon-vm.sock".to_owned(),
        resource_name: baryon_api::keys::VM_RESOURCE.to_owned(),
        grants: true,
        slots: args.vm_slots,
        kvm_device: args.kvm_device.clone(),
        anchor_socket_dir: args.anchor_socket_dir.clone(),
        anchor_mount_path: "/run/baryon/anchor".to_owned(),
    };
    let import_plugin = deviceplugin::PluginConfig {
        socket_name: "baryon-import.sock".to_owned(),
        resource_name: baryon_api::keys::IMPORT_RESOURCE.to_owned(),
        grants: false,
        ..vm_plugin.clone()
    };
    let plugin = tokio::spawn(deviceplugin::run(
        vm_plugin,
        health_rx.clone(),
        cancel.clone(),
    ));
    let import = tokio::spawn(deviceplugin::run(import_plugin, health_rx, cancel.clone()));
    let publisher = tokio::spawn(publisher::run(
        client,
        publisher::PublisherConfig {
            node_name: args.node_name.clone(),
            virtd_socket: args.virtd_socket.clone(),
            interval: Duration::from_secs(args.status_interval_seconds),
        },
        health_tx,
        cancel.clone(),
    ));

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = term.recv() => tracing::info!("terminating"),
        _ = tokio::signal::ctrl_c() => tracing::info!("interrupted"),
    }
    cancel.cancel();
    let _ = plugin.await;
    let _ = import.await;
    let _ = publisher.await;
    Ok(())
}
