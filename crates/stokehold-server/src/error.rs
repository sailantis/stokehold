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

    #[error("backend {backend:?} could not speak the worker protocol: {source}")]
    WorkerProtocol {
        backend: String,
        #[source]
        source: stokehold_worker::WorkerError,
    },

    #[error("bad FastCGI protocol: {reason}")]
    BadProtocol { reason: String },

    #[error(transparent)]
    Config(#[from] stokehold_core::Error),

    #[error("no listener is configured; set server.fastcgi (server.http is not implemented yet)")]
    NoListener,

    #[error(transparent)]
    Fcgi(#[from] stokehold_fcgi::FcgiError),

    #[error("the peer sent a management record where a request was expected")]
    UnsupportedManagementRecord,

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

impl ServerError {
    /// Whether the backend was reached but did not answer in time.
    ///
    /// A front-end needs this to choose between a 504 and a 502, and the
    /// knowledge belongs here rather than in each front-end: only this crate knows
    /// which failure a deadline produces for each backend kind.
    pub fn is_timeout(&self) -> bool {
        match self {
            ServerError::WorkerProtocol { source, .. } => {
                matches!(source, stokehold_worker::WorkerError::Timeout(_))
            }
            ServerError::Worker { source, .. } | ServerError::Fcgi(source) => source.is_timeout(),
            ServerError::Io(io) => io.kind() == std::io::ErrorKind::TimedOut,
            _ => false,
        }
    }

    /// Whether no backend could take the request at all.
    ///
    /// Distinct from a backend that failed: nothing was attempted, so the honest
    /// answer is a 503 rather than a 502.
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            ServerError::NoWorker { .. } | ServerError::AllBusy { .. }
        )
    }
}

pub type Result<T> = std::result::Result<T, ServerError>;
