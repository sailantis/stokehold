//! The backend abstraction the dispatcher talks to.
//!
//! One enum covers every backend shape from the configuration, so the
//! dispatcher and both front-ends have exactly one thing to call.

use std::sync::Mutex;

use stokehold_core::config::BackendConfig;
use stokehold_fcgi::{Endpoint, Request, Response};

use crate::error::{Result, ServerError};
use crate::pool::CgiPool;
use crate::worker_pool::WorkerPool;

/// How many idle connections to an external backend to hold open.
///
/// Exactly one, which is not an arbitrary small number. `php-fpm` accepts a
/// connection *into a child* and then waits for the request on it, and a child
/// blocked in that read counts as **active** in php-fpm's own scoreboard — so
/// every connection parked there permanently removes one child from the set the
/// master is willing to dispatch to. Retaining more than one therefore costs
/// capacity in proportion to the cap, which is real and measurable: against a
/// 12-child pool, retaining 4 cost 7% of throughput at concurrency 32 (34,250 vs
/// 36,906 rps) and retaining 12 cost 26% (27,407 vs 36,906).
///
/// One is worth that single child, because the reconnect it saves is on the path
/// of *every* request: keeping a warm connection raised concurrency-1 throughput
/// from 2,986 to 3,598 rps, and cost nothing at concurrency 32 (36,497 vs 36,906,
/// inside the noise). More than one buys nothing further — 2 measured 3,578 at
/// concurrency 1, the same within noise as 1.
///
/// `nginx` retains *zero* upstream connections and reconnects for every request,
/// which is the correct default when the front-end is nginx and the connection is
/// a unix socket. stokehold's front-end is not nginx, and the reconnect is
/// measurable, so one is kept.
const MAX_IDLE_CONNECTIONS: usize = 1;

/// Connections to one external backend, checked out per request.
///
/// One FastCGI connection carries one request at a time — it is a conversation,
/// and the codec has no request-id demultiplexer, so multiplexing is not an
/// option. A single connection therefore caps the whole backend at one request
/// at a time no matter how large its `pm.max_children` is, which is what
/// `docs/plan.md` hazard 22 records. This type is the fix: as many connections as
/// there are requests in flight, each reused when it comes back.
///
/// Connections themselves are not limited, because the number in flight is
/// already bounded by the front-end's connection limit, and refusing to open a
/// connection would turn a busy backend into a 503. Only *retention* is capped,
/// at [`MAX_IDLE_CONNECTIONS`], which see for why the number is one.
///
/// Public only because it is a field of the public [`Backend`] enum; every method
/// below is private, so the only way to a connection is through the backend.
#[derive(Debug)]
pub struct ClientPool {
    endpoint: Endpoint,
    /// Warm connections, checked out by `acquire` and returned by `release`.
    idle: Mutex<Vec<stokehold_fcgi::Client>>,
}

impl ClientPool {
    fn new(endpoint: Endpoint) -> Self {
        ClientPool {
            endpoint,
            idle: Mutex::new(Vec::new()),
        }
    }

    fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Take a connection, reusing a warm one when there is one.
    ///
    /// A connection taken from the pool is not proven alive: the backend may have
    /// closed it since it was returned, and there is no way to tell without
    /// sending something. `request_external` recovers from exactly that, so the
    /// pooled connection is handed out as-is rather than probed — a probe would
    /// cost the same round trip as the request it is trying to avoid.
    fn acquire(&self, backend: &str) -> Result<stokehold_fcgi::Client> {
        let pooled = {
            let mut idle = self
                .idle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            idle.pop()
        };
        if let Some(client) = pooled {
            return Ok(client);
        }

        stokehold_fcgi::Client::connect(&self.endpoint, None).map_err(|source| {
            ServerError::Worker {
                backend: backend.to_string(),
                source,
            }
        })
    }

    /// Give a connection back, or drop it when it cannot serve another request.
    fn release(&self, client: stokehold_fcgi::Client) {
        if !client.is_reusable() {
            // Dropping closes the socket, which is the correct end for a
            // connection the backend refused to keep. Checked before taking the
            // lock so a busy pool is not locked for a connection it will not keep.
            return;
        }

        let mut idle = self
            .idle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if idle.len() < MAX_IDLE_CONNECTIONS {
            idle.push(client);
        }
    }

    /// Close every idle connection.
    fn clear(&mut self) {
        let idle = self
            .idle
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        idle.clear();
    }
}

/// Slot contention in a worker pool, gathered for the shutdown report.
///
/// Every worker pool accumulates what its slot mutexes cost, but a library has no
/// business deciding where that goes, so it is surfaced here and the *program*
/// prints it. A server that logs its own diagnostics is a server whose embedder
/// cannot; the CLI has one place to say it and this is the data for that place.
#[derive(Debug, Clone)]
pub struct Contention {
    pub backend: String,
    pub workers: usize,
    /// Requests this pool served. A request replayed on a second slot counts twice.
    pub served: u64,
    /// Mean time a request waited for a slot, in microseconds.
    pub mean_us: u64,
    /// The wait only one request in a hundred exceeded, and the median, in microseconds.
    ///
    /// The pair is the point: a mean says how much waiting there was, and these say how it
    /// was spread. A pool whose p99 is far above its p50 is queueing unfairly - most
    /// requests are served at once and a few wait for whole requests - which a mean cannot
    /// show and which is exactly the shape of a latency tail.
    pub p50_us: u64,
    pub p99_us: u64,
    /// The slot that took the most wait, and how much.
    pub busiest: usize,
    pub busiest_us: u64,
}

/// A backend, ready to serve requests.
#[derive(Debug)]
pub enum Backend {
    /// An already-running `php-fpm`, reached over its socket.
    ///
    /// stokehold does not manage this pool: systemd and php-fpm do, including
    /// `pm.max_requests` and per-pool users. We only speak FastCGI to it.
    External {
        name: String,
        /// Shared behind its own lock: one backend serves both front-ends, so a
        /// `&mut` here would make the whole dispatcher exclusive again.
        pool: ClientPool,
    },
    /// A pool of `php-cgi` processes that stokehold starts and supervises.
    SpawnCgi { name: String, pool: CgiPool },
    /// A pool of persistent PHP workers speaking stokehold's own protocol.
    ///
    /// The application boots once and stays resident, so PHP does *not* reset
    /// state between requests — the application clears its own request scope.
    Worker { name: String, pool: WorkerPool },
}

impl Backend {
    /// Build a backend from its configuration.
    pub fn from_config(name: &str, config: &BackendConfig) -> Result<Self> {
        match config {
            BackendConfig::External(external) => {
                let endpoint = Endpoint::parse(&external.socket).map_err(|source| {
                    ServerError::Misconfigured {
                        backend: name.to_string(),
                        reason: format!("{} is not a usable socket: {source}", external.socket),
                    }
                })?;
                Ok(Backend::External {
                    name: name.to_string(),
                    pool: ClientPool::new(endpoint),
                })
            }
            BackendConfig::SpawnCgi(cgi) => Ok(Backend::SpawnCgi {
                name: name.to_string(),
                pool: CgiPool::start(name, cgi)?,
            }),
            BackendConfig::Worker(worker) => Ok(Backend::Worker {
                name: name.to_string(),
                pool: WorkerPool::start(name, worker)?,
            }),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Backend::External { name, .. }
            | Backend::SpawnCgi { name, .. }
            | Backend::Worker { name, .. } => name,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Backend::External { .. } => "external",
            Backend::SpawnCgi { .. } => "spawn_cgi",
            Backend::Worker { .. } => "worker",
        }
    }

    /// Whether PHP's own SAPI resets state between requests for this backend.
    pub fn resets_per_request(&self) -> bool {
        !matches!(self, Backend::Worker { .. })
    }

    /// Send a request to this backend.
    ///
    /// `&self`: a backend is shared between the two front-ends and across
    /// request threads, so it must not need exclusive access. Each pool does its
    /// own synchronisation, and only around the work that actually needs it.
    pub fn request(&self, request: &Request) -> Result<Response> {
        match self {
            Backend::External { .. } => self.request_external(request),
            Backend::SpawnCgi { pool, .. } => pool.request(request),
            Backend::Worker { pool, .. } => pool.request(request),
        }
    }

    /// Serve a request from an externally managed FastCGI server.
    ///
    /// A connection is taken from the pool for the duration of one request, so
    /// the backend serves as many requests at once as it has workers rather than
    /// as many as it has *sockets* — see `docs/plan.md` hazard 22.
    ///
    /// The retry is not belt-and-braces: `FCGI_KEEP_CONN` is only a *request*,
    /// and `php-cgi` in particular closes the connection after each response
    /// even when keep-alive was asked for. Verified against php-cgi 8.3.31: the
    /// second request on a reused connection fails with a connection reset. So a
    /// reused connection that fails at the transport level is re-established
    /// once, which is indistinguishable from the client's point of view and
    /// immune to the backend's choice.
    fn request_external(&self, request: &Request) -> Result<Response> {
        let Backend::External { name, pool } = self else {
            unreachable!("only called for the external backend")
        };

        let mut client = pool.acquire(name)?;
        let reused = client.is_reusable();

        for attempt in 0..2 {
            match client.request(request) {
                Ok(response) => {
                    pool.release(client);
                    return Ok(response);
                }
                Err(source) if reused && attempt == 0 && is_transport_error(&source) => {
                    // The backend closed a connection it had agreed to keep; ask
                    // for a fresh one. The failed client is dropped here, so a
                    // connection that cannot be trusted never goes back in.
                    tracing::debug!(
                        backend = %name,
                        "the backend closed a reused connection; reconnecting"
                    );
                    client = stokehold_fcgi::Client::connect(pool.endpoint(), None).map_err(
                        |source| ServerError::Worker {
                            backend: name.clone(),
                            source,
                        },
                    )?;
                }
                Err(source) => {
                    return Err(ServerError::Worker {
                        backend: name.clone(),
                        source,
                    })
                }
            }
        }

        Err(ServerError::Worker {
            backend: name.clone(),
            source: stokehold_fcgi::FcgiError::IncompleteResponse {
                request_id: stokehold_fcgi::REQUEST_ID,
                complete: false,
            },
        })
    }

    /// Whether this backend is a worker pool whose slot contention should be
    /// reported when the server stops. `None` for every other shape.
    pub fn worker_contention(&self) -> Option<Contention> {
        let Backend::Worker { name, pool } = self else {
            return None;
        };
        let (busiest, busiest_us) = pool.busiest_slot_wait_us();
        Some(Contention {
            backend: name.clone(),
            workers: pool.len(),
            served: pool.requests_served(),
            mean_us: pool.mean_slot_wait_us(),
            p50_us: pool.slot_wait_percentile_us(0.50),
            p99_us: pool.slot_wait_percentile_us(0.99),
            busiest,
            busiest_us,
        })
    }

    /// Stop the backend and everything it started.
    ///
    /// `&mut self`, and only reachable by the owner: `Drop` is what actually
    /// reaps the processes, so nothing on the request path calls this.
    pub fn shutdown(&mut self) {
        match self {
            Backend::SpawnCgi { pool, .. } => pool.shutdown(),
            Backend::Worker { pool, .. } => pool.shutdown(),
            Backend::External { pool, .. } => {
                // Nothing was started, so there is nothing to stop; closing the
                // connections is the whole cleanup. `get_mut` because the pool is
                // already exclusively borrowed here — taking the lock would be
                // pointless.
                pool.clear();
            }
        }
    }
}

/// Whether a failure means the connection is gone rather than the request being
/// refused.
///
/// Shared so both backends treat a dropped connection the same way.
pub fn is_transport_error(err: &stokehold_fcgi::FcgiError) -> bool {
    match err {
        stokehold_fcgi::FcgiError::Io(io) => matches!(
            io.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::WouldBlock
        ),
        stokehold_fcgi::FcgiError::IncompleteResponse { .. } => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stokehold_core::config::{ExternalConfig, WorkerConfig};

    #[test]
    fn an_external_backend_parses_its_socket() {
        let backend = Backend::from_config(
            "fpm",
            &BackendConfig::External(ExternalConfig {
                socket: "127.0.0.1:9000".to_string(),
            }),
        )
        .unwrap();
        assert_eq!(backend.kind(), "external");
        assert!(backend.resets_per_request());
        assert_eq!(backend.name(), "fpm");
    }

    #[test]
    fn a_bad_socket_is_a_misconfiguration() {
        #[cfg(not(unix))]
        let socket = "unix:/run/php.sock";
        #[cfg(unix)]
        let socket = "unix:";

        let err = Backend::from_config(
            "fpm",
            &BackendConfig::External(ExternalConfig {
                socket: socket.to_string(),
            }),
        )
        .unwrap_err();
        assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
    }

    #[test]
    fn a_worker_backend_without_a_real_command_is_reported_at_startup() {
        // The pool boots its workers in `from_config`, so a command that cannot
        // start is a startup error rather than a surprise on the first request.
        let err = Backend::from_config(
            "app",
            &BackendConfig::Worker(WorkerConfig {
                command: "definitely-not-a-php-binary worker.php".to_string(),
                workers: 1,
                memory_limit_mb: None,
                max_requests: None,
                working_dir: None,
                env: Default::default(),
                checksum: Default::default(),
                transport: Default::default(),
            }),
        )
        .unwrap_err();
        assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn spawn_fpm_is_no_longer_a_backend_shape() {
        // It was accepted by the parser and then refused by `Backend::from_config`, so the
        // failure arrived at startup rather than where the file was read. Now that the
        // variant is gone the file cannot be written at all, which is the point of a
        // removed key: it should be noticed by whoever wrote it.
        let text = r#"
default_backend = "own"
[server]
http = "127.0.0.1:8080"
[backends.own]
kind = "spawn_fpm"
binary = "/usr/sbin/php-fpm"
"#;
        assert!(
            stokehold_core::config::Config::from_toml(text).is_err(),
            "spawn_fpm must not parse"
        );
    }
}
