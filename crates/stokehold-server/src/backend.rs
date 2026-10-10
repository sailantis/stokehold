//! The backend abstraction the dispatcher talks to.
//!
//! One enum covers every backend shape from the configuration, so the
//! dispatcher and both front-ends have exactly one thing to call.

use stokehold_core::config::BackendConfig;
use stokehold_fcgi::{Endpoint, Request, Response};

use crate::error::{Result, ServerError};
use crate::pool::CgiPool;

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
    /// A persistent PHP worker speaking stokehold's own protocol.
    ///
    /// Not implemented yet: it needs the protocol from Phase 5. Recognised here
    /// so configuration can be validated and reported before it works.
    Worker { name: String },
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
            BackendConfig::Worker(_) => Ok(Backend::Worker {
                name: name.to_string(),
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
            | Backend::Worker { name } => name,
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
            Backend::External {
                name,
                endpoint,
                client,
            } => {
                // Reconnect when the previous request declined keep-alive, or on
                // first use. `php-fpm` normally honours keep-alive, but the code
                // must not depend on that.
                if client.as_ref().map(|c| !c.is_reusable()).unwrap_or(true) {
                    *client = Some(stokehold_fcgi::Client::connect(endpoint, None).map_err(
                        |source| ServerError::Worker {
                            backend: name.clone(),
                            source,
                        },
                    )?);
                }
                let connection = client.as_mut().expect("connected above");
                connection
                    .request(request)
                    .map_err(|source| ServerError::Worker {
                        backend: name.clone(),
                        source,
                    })
            }
            Backend::SpawnCgi { pool, .. } => pool.request(request),
            Backend::Worker { name } => Err(ServerError::Misconfigured {
                backend: name.clone(),
                reason: "the worker backend needs the Phase 5 protocol, which is not implemented"
                    .to_string(),
            }),
        }
    }

    /// Stop the backend and everything it started.
    pub fn shutdown(&mut self) {
        if let Backend::SpawnCgi { pool, .. } = self {
            pool.shutdown();
        }
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
    fn a_worker_backend_is_recognised_but_not_serving_yet() {
        let mut backend = Backend::from_config(
            "app",
            &BackendConfig::Worker(WorkerConfig {
                command: "php worker.php".to_string(),
                workers: 1,
                memory_limit_mb: None,
                max_requests: None,
                working_dir: None,
                env: Default::default(),
            }),
        )
        .unwrap();

        assert_eq!(backend.kind(), "worker");
        // Worker mode is exactly the case where PHP does not reset for us.
        assert!(!backend.resets_per_request());
        let err = backend.request(&Request::default()).unwrap_err();
        assert!(err.to_string().contains("Phase 5"), "{err}");
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
