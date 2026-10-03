use std::path::PathBuf;

/// Errors raised while reading host interfaces.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {reason}")]
    Parse { path: PathBuf, reason: String },
}

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    pub(crate) fn parse(path: impl Into<PathBuf>, reason: impl Into<String>) -> Self {
        Self::Parse {
            path: path.into(),
            reason: reason.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
