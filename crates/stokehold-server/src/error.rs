//! Errors from supervision and dispatch.

/// A supervisor or dispatch failure.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("backend {backend:?} is misconfigured: {reason}")]
    Misconfigured { backend: String, reason: String },

    #[error("could not start {binary}: {source}")]
    Spawn {
        binary: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{binary} exited during startup with {status}")]
    StartupExited {
        binary: std::path::PathBuf,
        status: String,
    },

    #[error("{binary} did not start listening on {address} within {timeout:?}")]
    StartupTimeout {
        binary: std::path::PathBuf,
        address: String,
        timeout: std::time::Duration,
    },

    #[error("no worker is available for backend {backend:?}")]
    NoWorker { backend: String },

    #[error("all workers for backend {backend:?} are busy")]
    AllBusy { backend: String },

    #[error("a worker for backend {backend:?} failed: {source}")]
    Worker {
        backend: String,
        #[source]
        source: stokehold_fcgi::FcgiError,
    },

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, ServerError>;
