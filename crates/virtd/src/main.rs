//! baryon-virtd: the Baryon host virtualization runtime.
//!
//! Runs as a systemd service on Baryon-enabled Hadron hosts. Serves the
//! anchor and node gRPC services on Unix sockets, supervises Cloud
//! Hypervisor processes, and keeps running VMs alive across its own
//! restarts. It is not a Kubernetes client.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};
use futures::StreamExt;
use tokio::net::UnixListener;
use tokio_util::sync::CancellationToken;

use baryon_cloud_hypervisor::launch::Seccomp;
use baryon_runtime_api::v1alpha1::RuntimeInfo;
use baryon_runtime_api::v1alpha1::anchor_service_server::AnchorServiceServer;
use baryon_runtime_api::v1alpha1::node_service_server::NodeServiceServer;

use baryon_virtd::config::Config;
use baryon_virtd::{peer, services, vm};

const BUILD: baryon_version::BuildInfo = baryon_version::BuildInfo::new("baryon-virtd");

/// Time open client streams get to finish after a shutdown signal.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LogFormat {
    Json,
    Text,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SeccompMode {
    Enforce,
    Log,
}

#[derive(Debug, Parser)]
#[command(
    name = "baryon-virtd",
    version,
    about = "Baryon host virtualization runtime"
)]
struct Args {
    #[arg(long, env = "BARYON_RUNTIME_DIR", default_value = "/run/baryon")]
    runtime_dir: PathBuf,
    #[arg(long, env = "BARYON_LOG_DIR", default_value = "/var/log/baryon")]
    log_dir: PathBuf,
    #[arg(long, default_value = "/usr/bin/cloud-hypervisor")]
    cloud_hypervisor: PathBuf,
    #[arg(long, default_value = "/usr/share/cloud-hypervisor/CLOUDHV.fd")]
    firmware: PathBuf,
    #[arg(long, default_value = "/sys/fs/cgroup")]
    cgroup_root: PathBuf,
    #[arg(long, default_value = "/dev/kvm")]
    kvm_device: PathBuf,
    #[arg(long, default_value = "/usr/share/baryon/components.json")]
    components_manifest: PathBuf,
    /// Seconds a VM keeps running after its anchor disconnected unexpectedly.
    #[arg(long, default_value_t = 30)]
    reattach_grace_seconds: u64,
    /// Seconds a recovered VM waits for its anchor after a restart.
    #[arg(long, default_value_t = 120)]
    recovery_grace_seconds: u64,
    /// Guest shutdown time when no grace period is requested.
    #[arg(long, default_value_t = 60)]
    default_stop_grace_seconds: u64,
    #[arg(long, value_enum, default_value = "enforce")]
    seccomp: SeccompMode,
    /// Disable Landlock in Cloud Hypervisor. For diagnosis only.
    #[arg(long)]
    no_landlock: bool,
    /// Cloud Hypervisor log verbosity (0 warn, 1 info, 2 debug).
    #[arg(long, default_value_t = 1)]
    vmm_verbosity: u8,
    #[arg(long, value_enum, default_value = "json")]
    log_format: LogFormat,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Serve guest DHCP on an inherited packet socket. Started by
    /// baryon-virtd for each VM; not for direct use.
    #[command(name = "dhcp-responder", hide = true)]
    DhcpResponder {
        #[arg(long)]
        socket_fd: i32,
        /// Pod UID, for identification in logs and process listings.
        #[arg(long)]
        pod_uid: String,
    },
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Some(Cmd::DhcpResponder { socket_fd, pod_uid }) = &args.command {
        // stdout carries events to the parent; logs go to stderr.
        init_tracing(args.log_format, true);
        let _span = tracing::info_span!("dhcp_responder", pod = %pod_uid).entered();
        return baryon_virtd::dhcp_helper::run_child(*socket_fd);
    }
    init_tracing(args.log_format, false);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("baryon-virtd")
        .build()
        .context("building tokio runtime")?;
    let result = rt.block_on(run(args));
    // Do not wait for blocking network setup threads of VMs being started.
    rt.shutdown_timeout(Duration::from_secs(1));
    result
}

fn init_tracing(format: LogFormat, stderr: bool) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match (format, stderr) {
        (LogFormat::Json, false) => builder.json().flatten_event(true).init(),
        (LogFormat::Json, true) => builder
            .json()
            .flatten_event(true)
            .with_writer(std::io::stderr)
            .init(),
        (LogFormat::Text, false) => builder.init(),
        (LogFormat::Text, true) => builder.with_writer(std::io::stderr).init(),
    }
}

async fn run(args: Args) -> anyhow::Result<()> {
    tracing::info!(build = %BUILD, "starting");
    let cfg = Arc::new(Config {
        runtime_dir: args.runtime_dir,
        log_dir: args.log_dir,
        cloud_hypervisor: args.cloud_hypervisor,
        firmware: args.firmware,
        cgroup_root: args.cgroup_root,
        kvm_device: args.kvm_device,
        components_manifest: args.components_manifest,
        reattach_grace: Duration::from_secs(args.reattach_grace_seconds),
        recovery_grace: Duration::from_secs(args.recovery_grace_seconds),
        default_stop_grace: Duration::from_secs(args.default_stop_grace_seconds),
        seccomp: match args.seccomp {
            SeccompMode::Enforce => Seccomp::Enforce,
            SeccompMode::Log => Seccomp::Log,
        },
        landlock: !args.no_landlock,
        vmm_verbosity: args.vmm_verbosity,
    });

    preflight(&cfg)?;
    let components = std::fs::read_to_string(&cfg.components_manifest).ok();
    let hypervisor_version = hypervisor_version(&cfg, components.as_deref()).await;
    tracing::info!(%hypervisor_version, binary = %cfg.cloud_hypervisor.display(), "Cloud Hypervisor");

    for (dir, mode) in [
        (cfg.runtime_dir.clone(), 0o755),
        // Traversable, not listable: each VMM user must reach its own
        // vms/<pod-uid>/vmm directory.
        (cfg.vms_dir(), 0o711),
        (cfg.runtime_dir.join("anchor"), 0o755),
        // Only baryon-node-agent (uid 0 without capabilities, as the owner)
        // uses the node socket.
        (cfg.runtime_dir.join("node"), 0o700),
    ] {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("setting mode on {}", dir.display()))?;
    }

    let manager = vm::Manager::new(cfg.clone(), hypervisor_version.clone());
    let recovered = manager.recover();
    tracing::info!(recovered, "runtime recovery complete");

    let runtime = RuntimeInfo {
        virtd_version: BUILD.version.into(),
        virtd_git_commit: BUILD.git_commit.into(),
        protocol: baryon_runtime_api::PROTOCOL.into(),
        hypervisor: "cloud-hypervisor".into(),
        hypervisor_version,
        components_json: components.unwrap_or_default(),
    };

    let shutdown = CancellationToken::new();
    // Anchors run as per-VM uids, so the anchor socket is open to all users;
    // the peer checks on each connection decide what a caller may do. The
    // node socket is owner-only.
    let anchor_listener = bind(&cfg.anchor_socket(), 0o666)?;
    let node_listener = bind(&cfg.node_socket(), 0o600)?;

    let anchor_server = tonic::transport::Server::builder()
        .add_service(AnchorServiceServer::new(services::Anchor {
            manager: manager.clone(),
        }))
        .serve_with_incoming_shutdown(
            incoming(anchor_listener, true),
            shutdown.clone().cancelled_owned(),
        );
    let node_server = tonic::transport::Server::builder()
        .add_service(NodeServiceServer::new(services::Node {
            manager: manager.clone(),
            runtime,
        }))
        .serve_with_incoming_shutdown(
            incoming(node_listener, false),
            shutdown.clone().cancelled_owned(),
        );

    let signals = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            wait_for_signal().await;
            tracing::info!("shutting down; running VMs are left running");
            shutdown.cancel();
        })
    };

    tracing::info!(anchor = %cfg.anchor_socket().display(), node = %cfg.node_socket().display(), "serving");
    let servers = async { tokio::join!(anchor_server, node_server) };
    // Graceful shutdown waits for open connections, but anchor sessions and
    // node agent watches stay open for as long as their clients run. VMs
    // keep running and clients reconnect, so exit after a short grace.
    let grace = async {
        shutdown.cancelled().await;
        tokio::time::sleep(SHUTDOWN_GRACE).await;
    };
    tokio::select! {
        (a, n) = servers => {
            signals.abort();
            a.context("anchor service")?;
            n.context("node service")?;
        }
        () = grace => tracing::info!("closing remaining client connections"),
    }
    Ok(())
}

fn preflight(cfg: &Config) -> anyhow::Result<()> {
    if !cfg.cgroup_root.join("cgroup.controllers").exists() {
        bail!(
            "{} is not a cgroup v2 mount; Baryon requires the unified hierarchy",
            cfg.cgroup_root.display()
        );
    }
    if !cfg.cloud_hypervisor.exists() {
        bail!(
            "Cloud Hypervisor binary {} not found",
            cfg.cloud_hypervisor.display()
        );
    }
    if !cfg.firmware.exists() {
        bail!("firmware {} not found", cfg.firmware.display());
    }
    if !cfg.kvm_device.exists() {
        tracing::warn!(device = %cfg.kvm_device.display(), "KVM device missing; VMs cannot start on this host");
    }
    Ok(())
}

/// Version from the image component manifest, or from the binary banner on
/// hosts without a manifest.
async fn hypervisor_version(cfg: &Config, components: Option<&str>) -> String {
    if let Some(v) = components
        .and_then(|c| serde_json::from_str::<serde_json::Value>(c).ok())
        .and_then(|j| {
            j.pointer("/cloudHypervisor/version")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })
    {
        return v;
    }
    match tokio::process::Command::new(&cfg.cloud_hypervisor)
        .arg("--version")
        .output()
        .await
    {
        Ok(out) => baryon_cloud_hypervisor::Version::parse(&String::from_utf8_lossy(&out.stdout))
            .map(|v| v.to_string())
            .unwrap_or_else(|_| "unknown".into()),
        Err(_) => "unknown".into(),
    }
}

fn bind(path: &Path, mode: u32) -> anyhow::Result<UnixListener> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("removing stale {}", path.display())),
    }
    let l = UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting mode on {}", path.display()))?;
    Ok(l)
}

fn incoming(
    listener: UnixListener,
    anchor: bool,
) -> impl futures::Stream<Item = Result<peer::PeerStream, std::io::Error>> {
    futures::stream::unfold(listener, move |l| async move {
        loop {
            match l.accept().await {
                Ok((s, _)) => match peer::PeerStream::new(s, anchor) {
                    Ok(ps) => return Some((Ok(ps), l)),
                    Err(e) => {
                        tracing::warn!(error = %e, "rejecting connection without peer credentials")
                    }
                },
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    })
    .boxed()
}

async fn wait_for_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        std::future::pending::<()>().await;
        return;
    };
    tokio::select! {
        _ = term.recv() => {},
        _ = int.recv() => {},
    }
}
