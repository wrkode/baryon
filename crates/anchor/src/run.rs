//! `baryon-anchor run`.
//!
//! Connects to baryon-virtd, attaches the VM for this pod, and mirrors its
//! status into a readiness file. On SIGTERM it requests a final stop and
//! waits for it. If baryon-virtd restarts, the stream breaks and the anchor
//! reconnects; baryon-virtd keeps the VM running for a grace period and the
//! new attach finds it again.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use baryon_runtime_api::anchor_spec::{ANCHOR_STATE_DIR, AnchorSpec, SPEC_ENV};
use baryon_runtime_api::protocol::{self, Version};
use baryon_runtime_api::v1alpha1::anchor_service_client::AnchorServiceClient;
use baryon_runtime_api::v1alpha1::{
    AttachRequest, NegotiateRequest, NegotiateResponse, StopRequest, VmPhase, VmStatus,
};
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Status};

const TERMINATION_LOG: &str = "/dev/termination-log";
const MAX_BACKOFF: Duration = Duration::from_secs(10);
/// How long the anchor keeps reporting the last VM state while baryon-virtd
/// is unreachable (for example during its restart, which leaves the VM
/// running). After that the pod reports not ready.
const DISCONNECTED_GRACE: Duration = Duration::from_secs(30);

/// Result of protocol negotiation with baryon-virtd.
#[derive(Debug)]
enum Negotiation {
    /// baryon-virtd of this version can run the VM.
    Compatible(Version),
    /// It cannot; the message starts with the reason.
    Refused(String),
    /// Negotiation did not complete; reconnect.
    Retry(Status),
}

/// Decide from baryon-virtd's answer to `Negotiate` whether it can run a VM
/// that needs `required`. A baryon-virtd that predates negotiation answers
/// UNIMPLEMENTED and speaks 1.0 with the 1.0 features.
fn negotiation(answer: Result<NegotiateResponse, Status>, required: &[String]) -> Negotiation {
    let (server, available) = match answer {
        Ok(r) => (
            Version::of_peer(r.version.as_ref()),
            protocol::features_of_peer(r.version.as_ref(), &r.features),
        ),
        Err(s) if s.code() == Code::Unimplemented => (
            Version::of_peer(None),
            protocol::features_of_peer(None, &[]),
        ),
        Err(s) if s.code() == Code::PermissionDenied => {
            return Negotiation::Refused(refusal_message(s.message()));
        }
        Err(s) => return Negotiation::Retry(s),
    };
    match protocol::check_server(server, &available, required) {
        Ok(()) => Negotiation::Compatible(server),
        Err(e) => Negotiation::Refused(e.to_string()),
    }
}

/// Outcome of one attach session.
enum Session {
    /// The VM reached a terminal state; exit with this code.
    Done(ExitCode),
    /// The connection broke; reconnect.
    Reconnect,
    /// SIGTERM received and handled.
    Terminated,
}

pub(crate) async fn run(socket: PathBuf) -> ExitCode {
    let spec = match std::env::var(SPEC_ENV)
        .map_err(|_| baryon_runtime_api::anchor_spec::SpecError::Missing)
        .and_then(|s| AnchorSpec::from_json(&s))
    {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "invalid configuration");
            write_termination_message(&format!("InvalidSpec: {e}"));
            return ExitCode::FAILURE;
        }
    };
    let mut term = match crate::term::Term::new() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "cannot read SIGTERM");
            return ExitCode::FAILURE;
        }
    };
    write_status(None);

    let mut backoff = Duration::from_millis(500);
    let mut disconnected_since: Option<std::time::Instant> = None;
    loop {
        let client = tokio::select! {
            c = connect(&socket) => c,
            _ = term.recv() => {
                tracing::info!("terminating before baryon-virtd was reachable");
                return ExitCode::SUCCESS;
            }
        };
        let mut client = match client {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(socket = %socket.display(), error = %e, "baryon-virtd unreachable; retrying");
                mark_disconnected(&mut disconnected_since);
                tokio::select! {
                    () = tokio::time::sleep(backoff) => {}
                    _ = term.recv() => return ExitCode::SUCCESS,
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        backoff = Duration::from_millis(500);
        match session(&mut client, &spec, &mut term, &mut disconnected_since).await {
            Session::Done(code) => return code,
            Session::Terminated => return ExitCode::SUCCESS,
            Session::Reconnect => {
                mark_disconnected(&mut disconnected_since);
                tokio::select! {
                    () = tokio::time::sleep(backoff) => {}
                    _ = term.recv() => return ExitCode::SUCCESS,
                }
            }
        }
    }
}

/// Termination message for a refused attach. baryon-virtd prefixes refusals
/// that baryon-controller acts on with a reason (`IdentityConflict: ...`);
/// those are passed through, everything else is reported as
/// `AttachRefused`.
fn refusal_message(msg: &str) -> String {
    match msg.split_once(": ") {
        Some((reason, _))
            if !reason.is_empty()
                && reason.starts_with(|c: char| c.is_ascii_uppercase())
                && reason.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            msg.to_owned()
        }
        _ => format!("AttachRefused: {msg}"),
    }
}

/// Exit code for a refused attach: the one baryon-controller acts on for a
/// host identity conflict, `FAILURE` for every other refusal.
fn refusal_exit_code(message: &str) -> ExitCode {
    if message.starts_with("IdentityConflict: ") {
        ExitCode::from(baryon_runtime_api::IDENTITY_CONFLICT_EXIT_CODE)
    } else {
        ExitCode::FAILURE
    }
}

/// Record that baryon-virtd is unreachable; report not ready once that has
/// lasted longer than `DISCONNECTED_GRACE`.
fn mark_disconnected(since: &mut Option<std::time::Instant>) {
    if since.get_or_insert_with(std::time::Instant::now).elapsed() >= DISCONNECTED_GRACE {
        write_status(None);
    }
}

async fn connect(socket: &Path) -> Result<AnchorServiceClient<Channel>, tonic::transport::Error> {
    let endpoint = Endpoint::try_from(format!("unix://{}", socket.display()))?
        .connect_timeout(Duration::from_secs(5));
    Ok(AnchorServiceClient::new(endpoint.connect().await?))
}

async fn session(
    client: &mut AnchorServiceClient<Channel>,
    spec: &AnchorSpec,
    term: &mut crate::term::Term,
    disconnected_since: &mut Option<std::time::Instant>,
) -> Session {
    let required = spec.required_features();
    let negotiated = client
        .negotiate(NegotiateRequest {
            version: Some(Version::CURRENT.to_proto()),
            required_features: required.clone(),
        })
        .await
        .map(tonic::Response::into_inner);
    match negotiation(negotiated, &required) {
        Negotiation::Compatible(server) => {
            tracing::debug!(server = %server, "runtime protocol negotiated");
        }
        Negotiation::Refused(message) => {
            tracing::error!(message = %message, "baryon-virtd cannot run this VM");
            write_termination_message(&message);
            return Session::Done(ExitCode::FAILURE);
        }
        Negotiation::Retry(status) => {
            tracing::warn!(code = ?status.code(), message = status.message(), "negotiation failed");
            return Session::Reconnect;
        }
    }
    let request = AttachRequest {
        spec: Some(spec.to_vm_spec()),
        protocol: baryon_runtime_api::PROTOCOL.to_owned(),
        version: Some(Version::CURRENT.to_proto()),
        required_features: required,
    };
    let mut stream = match client.attach(request).await {
        Ok(r) => r.into_inner(),
        Err(status) => {
            return match status.code() {
                Code::InvalidArgument | Code::FailedPrecondition | Code::PermissionDenied => {
                    tracing::error!(code = ?status.code(), message = status.message(), "attach refused");
                    let message = refusal_message(status.message());
                    write_termination_message(&message);
                    Session::Done(refusal_exit_code(&message))
                }
                _ => {
                    tracing::warn!(code = ?status.code(), message = status.message(), "attach failed");
                    Session::Reconnect
                }
            };
        }
    };
    loop {
        tokio::select! {
            msg = stream.message() => match msg {
                Ok(Some(status)) => {
                    *disconnected_since = None;
                    log_status(&status);
                    write_status(Some(&status));
                    if is_terminal(&status) {
                        write_termination_message(&format!("{}: {}", status.reason, status.message));
                        return Session::Done(if status.phase == VmPhase::Failed as i32 {
                            ExitCode::FAILURE
                        } else {
                            ExitCode::SUCCESS
                        });
                    }
                }
                Ok(None) => {
                    tracing::warn!("status stream ended; reconnecting");
                    return Session::Reconnect;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "status stream failed; reconnecting");
                    return Session::Reconnect;
                }
            },
            _ = term.recv() => {
                stop(client, spec).await;
                return Session::Terminated;
            }
        }
    }
}

async fn stop(client: &mut AnchorServiceClient<Channel>, spec: &AnchorSpec) {
    tracing::info!(
        grace_s = spec.stop_grace_seconds,
        "SIGTERM: shutting down the VM"
    );
    write_status(None);
    let req = StopRequest {
        grace_period_seconds: spec.stop_grace_seconds,
        r#final: true,
    };
    // baryon-virtd escalates to a hard stop after the grace period; allow
    // for that before giving up. kubelet sends SIGKILL at the pod's
    // termination grace period regardless.
    let limit = Duration::from_secs(u64::from(spec.stop_grace_seconds) + 20);
    match tokio::time::timeout(limit, client.stop(req)).await {
        Ok(Ok(resp)) => {
            if let Some(status) = resp.into_inner().status {
                log_status(&status);
                write_termination_message(&format!("{}: {}", status.reason, status.message));
            }
        }
        Ok(Err(e)) => tracing::error!(error = %e, "stop request failed"),
        Err(_) => tracing::error!("stop request timed out"),
    }
}

fn is_terminal(s: &VmStatus) -> bool {
    s.phase == VmPhase::Stopped as i32 || s.phase == VmPhase::Failed as i32
}

fn log_status(s: &VmStatus) {
    let phase = VmPhase::try_from(s.phase).unwrap_or(VmPhase::Unspecified);
    tracing::info!(?phase, reason = %s.reason, message = %s.message, "VM status");
}

fn status_path() -> PathBuf {
    Path::new(ANCHOR_STATE_DIR).join("status")
}

/// Record the VM phase for the readiness probe. `None` means not ready.
fn write_status(status: Option<&VmStatus>) {
    let line = match status {
        Some(s) => {
            let phase = VmPhase::try_from(s.phase).unwrap_or(VmPhase::Unspecified);
            format!("{} {}\n", phase.as_str_name(), s.reason)
        }
        None => "VM_PHASE_UNSPECIFIED Disconnected\n".to_owned(),
    };
    if let Err(e) = write_status_in(Path::new(ANCHOR_STATE_DIR), &line) {
        tracing::warn!(error = %e, "cannot write readiness status");
    }
}

/// Write `line` to `<dir>/status` through a new temporary file, without
/// following symbolic links: other processes of the container can write the
/// directory (ADR 0015).
fn write_status_in(dir: &Path, line: &str) -> std::io::Result<()> {
    use rustix::fs::{AtFlags, Mode, OFlags};
    match std::fs::create_dir(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let dirfd = rustix::fs::open(
        dir,
        OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    match rustix::fs::unlinkat(&dirfd, "status.tmp", AtFlags::empty()) {
        Err(rustix::io::Errno::NOENT) | Ok(()) => {}
        Err(e) => return Err(e.into()),
    }
    let fd = rustix::fs::openat(
        &dirfd,
        "status.tmp",
        OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::WRONLY | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o644),
    )?;
    std::io::Write::write_all(&mut std::fs::File::from(fd), line.as_bytes())?;
    rustix::fs::renameat(&dirfd, "status.tmp", &dirfd, "status")?;
    Ok(())
}

/// Readiness: the VM is running and the guest produced boot evidence.
pub(crate) fn probe() -> ExitCode {
    match std::fs::read_to_string(status_path()) {
        Ok(s) if s.starts_with(VmPhase::Running.as_str_name()) => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}

/// kubelet reports the content of this file in the container status.
pub(crate) fn write_termination_message(msg: &str) {
    let mut m = msg.to_owned();
    m.truncate(4000);
    let _ = std::fs::write(TERMINATION_LOG, m);
}

#[cfg(test)]
mod tests {
    use super::*;
    use baryon_runtime_api::v1alpha1::ProtocolVersion;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|&x| x.to_owned()).collect()
    }

    #[test]
    fn a_1_0_virtd_runs_vms_that_need_only_1_0_features() {
        let unimplemented = Err(Status::unimplemented("unknown method Negotiate"));
        match negotiation(unimplemented, &strings(protocol::BASE_FEATURES)) {
            Negotiation::Compatible(v) => assert_eq!(v, Version { major: 1, minor: 0 }),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_1_0_virtd_refuses_a_newer_feature() {
        let unimplemented = Err(Status::unimplemented("unknown method Negotiate"));
        match negotiation(unimplemented, &strings(&["disk.block", "disk.future"])) {
            Negotiation::Refused(m) => {
                assert!(m.starts_with("UnsupportedFeature: "), "{m}");
                assert!(m.contains("disk.future"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_newer_virtd_of_the_same_major_is_compatible() {
        let answer = Ok(NegotiateResponse {
            version: Some(ProtocolVersion { major: 1, minor: 9 }),
            features: strings(&["boot.firmware", "disk.file", "network.pod", "x.new"]),
        });
        match negotiation(
            answer,
            &strings(&["boot.firmware", "disk.file", "network.pod"]),
        ) {
            Negotiation::Compatible(v) => assert_eq!(v, Version { major: 1, minor: 9 }),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn another_major_is_refused() {
        let answer = Ok(NegotiateResponse {
            version: Some(ProtocolVersion { major: 2, minor: 0 }),
            features: strings(protocol::BASE_FEATURES),
        });
        match negotiation(answer, &[]) {
            Negotiation::Refused(m) => assert!(m.starts_with("ProtocolIncompatible: "), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn transport_errors_retry() {
        let answer = Err(Status::unavailable("connection refused"));
        assert!(matches!(negotiation(answer, &[]), Negotiation::Retry(_)));
    }

    #[test]
    fn status_is_written_without_following_links() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        write_status_in(&state, "VM_PHASE_RUNNING GuestBooted\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(state.join("status")).unwrap(),
            "VM_PHASE_RUNNING GuestBooted\n"
        );
        // A link planted as the temporary file is removed, not followed.
        let outside = dir.path().join("outside");
        std::fs::write(&outside, "unchanged").unwrap();
        std::os::unix::fs::symlink(&outside, state.join("status.tmp")).unwrap();
        write_status_in(&state, "VM_PHASE_UNSPECIFIED Disconnected\n").unwrap();
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "unchanged");
        // A link as the directory itself is refused.
        let linked = dir.path().join("linked");
        std::os::unix::fs::symlink(&state, &linked).unwrap();
        assert!(write_status_in(&linked, "x\n").is_err());
    }

    #[test]
    fn refusals_keep_their_reason() {
        assert_eq!(
            refusal_message("IdentityConflict: another VM on this node"),
            "IdentityConflict: another VM on this node"
        );
        assert_eq!(
            refusal_message("invalid VM spec: vcpus must be between 1 and 256"),
            "AttachRefused: invalid VM spec: vcpus must be between 1 and 256"
        );
        assert_eq!(refusal_message("x"), "AttachRefused: x");
    }

    #[test]
    fn only_an_identity_conflict_has_its_exit_code() {
        let code = |m: &str| format!("{:?}", refusal_exit_code(m));
        let conflict = format!(
            "{:?}",
            ExitCode::from(baryon_runtime_api::IDENTITY_CONFLICT_EXIT_CODE)
        );
        assert_eq!(code("IdentityConflict: another VM on this node"), conflict);
        assert_ne!(code("AttachRefused: x"), conflict);
        assert_ne!(code("NotAnchorSupervisor: x"), conflict);
    }
}
