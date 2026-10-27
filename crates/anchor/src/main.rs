//! baryon-anchor: the process inside a VM Anchor Pod.
//!
//! Subcommands:
//!
//! - `run`: attach this pod to baryon-virtd, report readiness, and stop the
//!   VM when the container is asked to stop. The VM's lifetime is bound to
//!   this process (ADR 0005).
//! - `probe`: exec readiness probe. The pod IP belongs to the guest, so
//!   network probes would reach the guest instead of this process.
//!
//! There is deliberately no command that writes to the VM's disk: images
//! are imported by `baryon-importer` in the import Pod, before the VM's
//! first start, and that binary is not part of the Anchor image.

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
        Command::Probe => ExitCode::SUCCESS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_is_consistent() {
        Args::command().debug_assert();
    }

    /// The Anchor runtime has no disk import authority (finding M2 of
    /// 2026-10-27): no subcommand imports or writes a disk image.
    #[test]
    fn there_is_no_import_subcommand() {
        let names: Vec<String> = Args::command()
            .get_subcommands()
            .map(|c| c.get_name().to_owned())
            .collect();
        assert_eq!(names, ["run", "probe"]);
        assert!(
            Args::try_parse_from([
                "baryon-anchor",
                "import",
                "--url=https://example.com/a.raw",
                "--size-bytes=1048576",
                "--device=/dev/baryon/root",
            ])
            .is_err()
        );
    }
}
