//! The backend abstraction the dispatcher talks to.
//!
//! One enum covers every backend shape from the configuration, so the
//! dispatcher and both front-ends have exactly one thing to call.

use stokehold_core::config::BackendConfig;
use stokehold_fcgi::{Endpoint, Request, Response};

use crate::error::{Result, ServerError};
use crate::pool::CgiPool;
use crate::worker_pool::WorkerPool;

/// A backend, ready to serve requests.
#[derive(Debug)]
pub enum Backend {
    /// An already-running `php-fpm`, reached over its socket.
    ///
    /// stokehold does not manage this pool: systemd and php-fpm do, including
    /// `pm.max_requests` and per-pool users. We only speak FastCGI to it.
    External {
        name: String,
        endpoint: Endpoint,
        client: Option<stokehold_fcgi::Client>,
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
                    endpoint,
                    client: None,
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
            BackendConfig::SpawnFpm(fpm) => Err(ServerError::Misconfigured {
                backend: name.to_string(),
                reason: format!(
                    "spawn_fpm is not implemented yet (binary {}); point at a running php-fpm with \
                     kind = \"external\" instead",
                    fpm.binary.display()
                ),
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
    pub fn request(&mut self, request: &Request) -> Result<Response> {
        match self {
            Backend::External { .. } => self.request_external(request),
            Backend::SpawnCgi { pool, .. } => pool.request(request),
            Backend::Worker { pool, .. } => pool.request(request),
        }
    }

    /// Serve a request from an externally managed FastCGI server.
    ///
    /// The retry is not belt-and-braces: `FCGI_KEEP_CONN` is only a *request*,
    /// and `php-cgi` in particular closes the connection after each response
    /// even when keep-alive was asked for. Verified against php-cgi 8.3.31: the
    /// second request on a reused connection fails with a connection reset. So a
    /// reused connection that fails at the transport level is re-established
    /// once, which is indistinguishable from the client's point of view and
    /// immune to the backend's choice.
    fn request_external(&mut self, request: &Request) -> Result<Response> {
        let Backend::External {
            name,
            endpoint,
            client,
        } = self
        else {
            unreachable!("only called for the external backend")
        };

        for attempt in 0..2 {
            let reused = client.is_some();
            if client.as_ref().map(|c| !c.is_reusable()).unwrap_or(true) {
                let connection =
                    stokehold_fcgi::Client::connect(endpoint, None).map_err(|source| {
                        ServerError::Worker {
                            backend: name.clone(),
                            source,
                        }
                    })?;
                *client = Some(connection);
            }

            let connection = client.as_mut().expect("connected above");
            match connection.request(request) {
                Ok(response) => return Ok(response),
                Err(source) if reused && attempt == 0 && is_transport_error(&source) => {
                    tracing::debug!(
                        backend = %name,
                        "the backend closed a reused connection; reconnecting"
                    );
                    *client = None;
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

    /// Stop the backend and everything it started.
    pub fn shutdown(&mut self) {
        match self {
            Backend::SpawnCgi { pool, .. } => pool.shutdown(),
            Backend::Worker { pool, .. } => pool.shutdown(),
            Backend::External { client, .. } => {
                // Nothing was started, so there is nothing to stop; dropping the
                // connection is the whole cleanup.
                *client = None;
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
            }),
        )
        .unwrap_err();
        assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn spawn_fpm_explains_what_to_do_instead() {
        let err = Backend::from_config(
            "own",
            &BackendConfig::SpawnFpm(stokehold_core::config::SpawnFpmConfig {
                binary: std::path::PathBuf::from("/usr/sbin/php-fpm"),
                config: None,
                socket: None,
            }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("external"), "{err}");
    }
}
