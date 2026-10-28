//! baryon-importer: the process inside a VM's import Pod.
//!
//! It runs only in the import Pod, before the VM's first start, and has the
//! only code path in Baryon that writes an image onto a root disk volume.
//! The Anchor Pod's image does not contain it, so access to a running
//! Anchor Pod gives no way to re-import or overwrite the VM's disk.
//!
//! Subcommands:
//!
//! - `import`: write a raw disk image from an HTTP(S) URL into the root disk
//!   volume, once baryon-controller has approved the import for the claim.
//! - `probe`: readiness probe that always fails. An import Pod is never
//!   ready, so Services that select the VM's labels never send traffic to
//!   it.

mod import;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

const BUILD: baryon_version::BuildInfo = baryon_version::BuildInfo::new("baryon-importer");

const TERMINATION_LOG: &str = "/dev/termination-log";

#[derive(Debug, Parser)]
#[command(
    name = "baryon-importer",
    version,
    about = "Baryon disk image importer for import Pods"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
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
        /// Write to this raw block device instead of an image file in `dir`.
        #[arg(long)]
        device: Option<PathBuf>,
        /// Write nothing until this file holds the claim UID: baryon-controller
        /// approves the import once the claim is bound to a volume
        /// provisioned for it.
        #[arg(long)]
        approval_file: PathBuf,
        /// UID of the claim; the approval must name it.
        #[arg(long)]
        claim_uid: String,
    },
    /// Readiness probe of the import Pod; always fails.
    Probe,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let Command::Import {
        url,
        sha256,
        size_bytes,
        dir,
        device,
        approval_file,
        claim_uid,
    } = args.command
    else {
        return ExitCode::FAILURE;
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .flatten_event(true)
        .init();
    tracing::info!(build = %BUILD, "starting");
    // Only the import container's own process may import: one started next
    // to it (for example with kubectl exec) would find the same approval
    // and write the claim with a source the controller never checked. It
    // writes nothing, the termination message included.
    if std::process::id() != 1 {
        tracing::error!(
            pid = std::process::id(),
            "`import` must be the container's init process (PID 1)"
        );
        return ExitCode::FAILURE;
    }
    // Processes with the same uid (started with kubectl exec) can then
    // neither ptrace the importer nor read its /proc entries.
    if let Err(e) =
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
    {
        tracing::error!(error = %e, "cannot make the process non-dumpable");
        write_termination_message("ImportFailed: cannot make the importer non-dumpable");
        return ExitCode::FAILURE;
    }
    if claim_uid.trim().is_empty() {
        tracing::error!("the claim UID is empty");
        write_termination_message("ImportFailed: the claim UID is empty");
        return ExitCode::FAILURE;
    }
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
    let result = rt.block_on(async {
        import::wait_for_approval(&approval_file, claim_uid.trim()).await?;
        import::import(
            &url,
            sha256.as_deref(),
            size_bytes,
            match &device {
                Some(d) => import::Target::Device(d),
                None => import::Target::File(&dir),
            },
        )
        .await
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "import failed");
            write_termination_message(&format!("ImportFailed: {e:#}"));
            ExitCode::FAILURE
        }
    }
}

/// kubelet reports the content of this file in the container status.
fn write_termination_message(msg: &str) {
    let mut m = msg.to_owned();
    m.truncate(4000);
    let _ = std::fs::write(TERMINATION_LOG, m);
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_cli_is_consistent() {
        Args::command().debug_assert();
    }

    #[test]
    fn import_requires_the_approval_file_and_the_claim_uid() {
        let base = [
            "baryon-importer",
            "import",
            "--url=https://example.com/a.raw",
            "--size-bytes=1048576",
        ];
        assert!(Args::try_parse_from(base).is_err());
        let mut with_approval = base.to_vec();
        with_approval.push("--approval-file=/etc/baryon/import/approved");
        assert!(Args::try_parse_from(&with_approval).is_err());
        let mut full = with_approval.clone();
        full.push("--claim-uid=6b7f");
        assert!(Args::try_parse_from(&full).is_ok());
    }
}
