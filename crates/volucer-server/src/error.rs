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

    #[error("{binary} exited during startup with {status}{hint}")]
    StartupExited {
        binary: std::path::PathBuf,
        status: String,
        hint: Hint,
    },

    #[error("{binary} did not start listening on {address} within {timeout:?}{hint}")]
    StartupTimeout {
        binary: std::path::PathBuf,
        address: String,
        timeout: std::time::Duration,
        hint: Hint,
    },

    #[error("no worker is available for backend {backend:?}")]
    NoWorker { backend: String },

    #[error("all workers for backend {backend:?} are busy")]
    AllBusy { backend: String },

    #[error("a worker for backend {backend:?} failed: {source}")]
    Worker {
        backend: String,
        #[source]
        source: volucer_fcgi::FcgiError,
    },

    /// A failure reaching an HTTP origin, or coming back from one.
    ///
    /// Kept as the upstream crate's own type rather than flattened into `reason: String`,
    /// because the front-end has to choose between a 502 and a 504 and only this error knows
    /// which failure a deadline produces — the same reason [`ServerError::Worker`] keeps
    /// `FcgiError` instead of its message.
    #[error("backend {backend:?} could not reach its origin: {source}")]
    Upstream {
        backend: String,
        #[source]
        source: volucer_upstream::UpstreamError,
    },

    /// The named backend speaks HTTP, and this is not the HTTP front-end.
    ///
    /// Not a misconfiguration and not an unreachable origin: the request arrived on the wrong
    /// protocol for this backend kind. The FastCGI front-end has no way to hand an upstream a
    /// request at all, so the honest answer is a 500 that names the mistake rather than a 502
    /// from a connection that was never attempted.
    #[error(
        "backend {backend:?} is an HTTP upstream, which cannot be reached over FastCGI; \
         a request for it must arrive on the HTTP listener"
    )]
    WrongProtocol { backend: String },

    #[error("backend {backend:?} could not speak the worker protocol: {source}")]
    WorkerProtocol {
        backend: String,
        #[source]
        source: volucer_worker::WorkerError,
    },

    #[error("bad FastCGI protocol: {reason}")]
    BadProtocol { reason: String },

    #[error(transparent)]
    Config(#[from] volucer_core::Error),

    #[error("no listener is configured; set server.fastcgi or server.http")]
    NoListener,

    #[error("a listener could not start: {reason}")]
    Listener { reason: String },

    #[error(transparent)]
    Fcgi(#[from] volucer_fcgi::FcgiError),

    #[error("the peer sent a management record where a request was expected")]
    UnsupportedManagementRecord,

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

/// An explanation appended to a startup failure, when one is known.
///
/// A field rather than a separate error variant because the *failure* is the same whether or
/// not the cause is known; what changes is whether the operator is told what to do about it.
/// The one case it is used for is the most likely way a worker fails to start: the server was
/// built for the socket transport and the worker is a `Worker::create()` one, which waits on
/// stdin and dies when it is at end-of-file. That message alone — "exited during startup with
/// exit code 255" — sends a reader looking at PHP, where nothing is wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Hint(pub Option<&'static str>);

impl std::fmt::Display for Hint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(text) => write!(f, " — {text}"),
            None => Ok(()),
        }
    }
}

/// The hint for a worker that died while the server was waiting for a socket connection.
///
/// The socket transport hands the worker a *socket* and sets the worker's stdin to
/// end-of-file, so that a worker reading the wrong stream fails instead of blocking. A worker
/// still calling `Worker::create()` therefore reads EOF and exits during its handshake, and
/// that is the one cause of a socket startup failure that the exit status cannot express.
///
/// It is worded as a condition rather than a diagnosis on purpose. The server knows only that
/// the worker exited while a connection was awaited; a missing file or a syntax error looks
/// exactly the same from here, and the worker's own stderr says which it was. So this names
/// the check to make and both ways out, rather than claiming a cause it has not established.
pub const SOCKET_WORKER_HINT: &str =
    "if the worker calls Volucer\\Worker::create(), it must call Volucer\\Worker::connect() \
     instead: that is what honours this transport, and it falls back to stdio when no socket is \
     offered, so it is safe either way. To keep the worker as it is, set `transport = \"pipe\"` on \
     the backend. Anything the worker wrote to stderr is logged above and will say which it was.";

impl ServerError {
    /// Whether the backend was reached but did not answer in time.
    ///
    /// A front-end needs this to choose between a 504 and a 502, and the
    /// knowledge belongs here rather than in each front-end: only this crate knows
    /// which failure a deadline produces for each backend kind.
    pub fn is_timeout(&self) -> bool {
        match self {
            ServerError::WorkerProtocol { source, .. } => {
                matches!(source, volucer_worker::WorkerError::Timeout(_))
            }
            ServerError::Worker { source, .. } | ServerError::Fcgi(source) => source.is_timeout(),
            // An origin that was reached and stayed silent is the one time `UpstreamError` calls
            // itself a timeout; everything else it reports is an origin that could not be reached.
            ServerError::Upstream { source, .. } => source.is_timeout(),
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
