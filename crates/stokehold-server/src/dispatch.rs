//! Request dispatch: choosing a backend and translating errors.
//!
//! Route-level selection is deliberately simple for now — a default backend plus
//! per-host or per-path overrides, which is what the design calls for. Anything
//! more elaborate belongs with the front-ends, where the request is understood.

use std::collections::BTreeMap;

use stokehold_core::config::Config;
use stokehold_fcgi::{cgi, Request, Response};

use crate::backend::Backend;
use crate::error::{Result, ServerError};

/// Routes requests to backends.
#[derive(Debug)]
pub struct Dispatcher {
    backends: BTreeMap<String, Backend>,
    default: String,
    routing: cgi::Routing,
}

impl Dispatcher {
    /// Build every configured backend and prepare dispatch.
    ///
    /// Backends are started here rather than lazily, so a misconfiguration is
    /// reported at startup instead of on the first request that happens to hit
    /// the broken one.
    pub fn from_config(config: &Config, routing: cgi::Routing) -> Result<Self> {
        let default = config
            .default_backend
            .clone()
            .ok_or_else(|| ServerError::Misconfigured {
                backend: "<default>".to_string(),
                reason: "no default_backend is configured".to_string(),
            })?;

        let mut backends = BTreeMap::new();
        for (name, backend_config) in &config.backends {
            backends.insert(name.clone(), Backend::from_config(name, backend_config)?);
        }

        if !backends.contains_key(&default) {
            return Err(ServerError::Misconfigured {
                backend: default,
                reason: "default_backend does not match any configured backend".to_string(),
            });
        }

        Ok(Dispatcher {
            backends,
            default,
            routing,
        })
    }

    /// The routing table in use, so a front-end can resolve paths for logging.
    pub fn routing(&self) -> &cgi::Routing {
        &self.routing
    }

    /// The default backend's name.
    pub fn default_backend(&self) -> &str {
        &self.default
    }

    /// Names of every backend, for diagnostics.
    pub fn backend_names(&self) -> Vec<&str> {
        self.backends.values().map(Backend::name).collect()
    }

    /// Send a request to the default backend.
    pub fn dispatch(&mut self, request: &Request) -> Result<Response> {
        let name = self.default.clone();
        self.dispatch_to(&name, request)
    }

    /// Send a request to a named backend.
    pub fn dispatch_to(&mut self, name: &str, request: &Request) -> Result<Response> {
        let backend = self
            .backends
            .get_mut(name)
            .ok_or_else(|| ServerError::NoWorker {
                backend: name.to_string(),
            })?;
        backend.request(request)
    }

    /// Stop every backend.
    pub fn shutdown(&mut self) {
        for backend in self.backends.values_mut() {
            backend.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stokehold_core::config::{BackendConfig, Config, ServerConfig, WorkerConfig};

    fn worker_config(command: &str) -> BackendConfig {
        BackendConfig::Worker(WorkerConfig {
            command: command.to_string(),
            workers: 1,
            memory_limit_mb: None,
            max_requests: None,
            working_dir: None,
            env: Default::default(),
        })
    }

    fn config_with(backends: &[(&str, BackendConfig)], default: &str) -> Config {
        Config {
            server: ServerConfig {
                http: Some("127.0.0.1:8080".to_string()),
                fastcgi: None,
            },
            backends: backends
                .iter()
                .map(|(name, config)| (name.to_string(), config.clone()))
                .collect(),
            default_backend: Some(default.to_string()),
        }
    }

    #[test]
    fn builds_every_backend_and_dispatch_reaches_the_default() {
        let config = config_with(
            &[
                ("app", worker_config("php worker.php")),
                ("other", worker_config("php o.php")),
            ],
            "app",
        );
        let mut dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();

        assert_eq!(dispatcher.default_backend(), "app");
        let mut names = dispatcher.backend_names();
        names.sort();
        assert_eq!(names, vec!["app", "other"]);

        // The worker protocol is unimplemented, but the routing itself works:
        // the failure names the backend we reached.
        let err = dispatcher.dispatch(&Request::default()).unwrap_err();
        assert!(err.to_string().contains("app"), "{err}");
    }

    #[test]
    fn dispatch_to_an_unknown_backend_reports_it_by_name() {
        let config = config_with(&[("app", worker_config("php worker.php"))], "app");
        let mut dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();

        let err = dispatcher
            .dispatch_to("nope", &Request::default())
            .unwrap_err();
        assert!(matches!(err, ServerError::NoWorker { .. }), "{err}");
        assert!(err.to_string().contains("nope"), "{err}");
    }

    #[test]
    fn rejects_a_default_backend_that_does_not_exist() {
        let config = config_with(&[("app", worker_config("php worker.php"))], "missing");
        let err = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap_err();
        assert!(err.to_string().contains("default_backend"), "{err}");
    }

    #[test]
    fn startup_fails_on_a_misconfigured_backend_rather_than_on_first_request() {
        // An external backend with an unusable socket must be reported now.
        #[cfg(not(unix))]
        let socket = "unix:/run/php.sock";
        #[cfg(unix)]
        let socket = "unix:";

        let config = config_with(
            &[(
                "fpm",
                BackendConfig::External(stokehold_core::config::ExternalConfig {
                    socket: socket.to_string(),
                }),
            )],
            "fpm",
        );
        assert!(Dispatcher::from_config(&config, cgi::Routing::default()).is_err());
    }
}
