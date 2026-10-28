//! gRPC protocol between Baryon node components and baryon-virtd.
//!
//! See `proto/baryon/runtime/v1alpha1/runtime.proto` for the contract and
//! ADR 0009 for the transport decision.

/// Generated code for `baryon.runtime.v1alpha1`.
#[allow(
    missing_debug_implementations,
    unreachable_pub,
    clippy::all,
    clippy::pedantic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]
pub mod v1alpha1 {
    tonic::include_proto!("baryon.runtime.v1alpha1");
}

/// Protocol identifier reported in `RuntimeInfo.protocol`.
pub const PROTOCOL: &str = "baryon.runtime.v1alpha1";

/// Version of the host report JSON schema carried in `HostInfo`.
pub const HOST_REPORT_VERSION: u32 = 1;

/// Default socket for `AnchorService`.
pub const ANCHOR_SOCKET: &str = "/run/baryon/anchor/virtd.sock";

/// Default socket for `NodeService`.
pub const NODE_SOCKET: &str = "/run/baryon/node/virtd.sock";

/// Exit code of the anchor when baryon-virtd refused its VM for a host
/// identity conflict. baryon-controller acts on the conflict only with this
/// code and the `IdentityConflict:` termination message together: other
/// processes of the Anchor container can write the message, never the
/// exit code of its init process (ADR 0015).
pub const IDENTITY_CONFLICT_EXIT_CODE: u8 = 3;

pub mod anchor_spec;
pub mod identity;
pub mod protocol;
