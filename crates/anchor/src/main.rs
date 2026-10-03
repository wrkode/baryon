//! baryon-anchor: the process inside a VM Anchor Pod.
//!
//! Subcommands:
//!
//! - `run`: attach this pod to baryon-virtd, report readiness, and stop the
//!   VM when the container is asked to stop. The VM's lifetime is bound to
//!   this process (ADR 0005).
//! - `probe`: exec readiness probe. The pod IP belongs to the guest, so
//!   network probes would reach the guest instead of this process.
//! - `import`: init container that writes a raw disk image from an HTTP(S)
//!   URL into the root disk volume.

mod import;
mod run;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

const BUILD: baryon_version::BuildInfo = baryon_version::BuildInfo::new("baryon-anchor");

#[derive(Debug, Parser)]
#[command(
    name = "baryon-anchor",
    version,
    about = "Baryon VM Anchor Pod process"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the VM for this pod until the container stops.
    Run {
        #[arg(long, env = "BARYON_VIRTD_SOCKET", default_value = baryon_runtime_api::ANCHOR_SOCKET)]
        socket: PathBuf,
    },
    /// Exit 0 when the VM is running.
    Probe,
    /// Import a raw disk image into the root disk volume.
    Import {
        #[arg(long)]
        url: String,
        /// Expected SHA-256 of the image, hex encoded.
        #[arg(long)]
        sha256: Option<String>,
        /// Size of the virtual disk in bytes. The image is extended to it.
        #[arg(long)]
        size_bytes: u64,
        #[arg(long, default_value = "/var/lib/baryon/disks/root")]
        dir: PathBuf,
    },
}

fn main() -> ExitCode {
    let args = Args::parse();
    if matches!(args.command, Command::Probe) {
        return run::probe();
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .flatten_event(true)
        .init();
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "cannot start runtime");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(build = %BUILD, "starting");
    match args.command {
        Command::Run { socket } => rt.block_on(run::run(socket)),
        Command::Import {
            url,
            sha256,
            size_bytes,
            dir,
        } => match rt.block_on(import::import(&url, sha256.as_deref(), size_bytes, &dir)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "import failed");
                run::write_termination_message(&format!("ImportFailed: {e:#}"));
                ExitCode::FAILURE
            }
        },
        Command::Probe => ExitCode::SUCCESS,
    }
}
