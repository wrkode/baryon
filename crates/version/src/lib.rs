//! Build and version information shared by Baryon binaries.
//!
//! The git commit is injected at build time through the `BARYON_GIT_COMMIT`
//! environment variable (set by CI and the image build). Local builds report
//! `unknown`.

use serde::Serialize;

/// Version of the Baryon release this binary belongs to.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Git commit the binary was built from, or `unknown`.
pub const GIT_COMMIT: &str = match option_env!("BARYON_GIT_COMMIT") {
    Some(c) => c,
    None => "unknown",
};

/// Build information reported by every Baryon binary.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildInfo {
    pub component: &'static str,
    pub version: &'static str,
    pub git_commit: &'static str,
}

impl BuildInfo {
    pub const fn new(component: &'static str) -> Self {
        Self {
            component,
            version: VERSION,
            git_commit: GIT_COMMIT,
        }
    }
}

impl std::fmt::Display for BuildInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} ({})",
            self.component, self.version, self.git_commit
        )
    }
}
