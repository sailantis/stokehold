//! Request dispatch: choosing a backend and translating errors.
//!
//! Route-level selection is deliberately simple for now — a default backend plus
//! per-host or per-path overrides, which is what the design calls for. Anything
//! more elaborate belongs with the front-ends, where the request is understood.

use std::collections::BTreeMap;

use stokehold_core::config::{Config, RouteConfig};
use stokehold_fcgi::{cgi, Request, Response};

use crate::backend::Backend;
use crate::error::{Result, ServerError};

/// Routes requests to backends.
#[derive(Debug)]
pub struct Dispatcher {
    backends: BTreeMap<String, Backend>,
    default: String,
    routing: cgi::Routing,
    /// The route table from the configuration, in declaration order.
    ///
    /// Kept here rather than in each front-end so there is exactly one place a
    /// backend is chosen, which is what lets the FastCGI and HTTP front-ends
    /// behave identically for the same request.
    routes: Vec<RouteConfig>,
}

impl Dispatcher {
    /// Build every configured backend and prepare dispatch.
    ///
    /// Backends are started here rather than lazily, so a misconfiguration is
    /// reported at startup instead of on the first request that happens to hit
    /// the broken one.
    pub fn from_config(config: &Config, routing: cgi::Routing) -> Result<Self> {
        let mut backends = BTreeMap::new();
        for (name, backend_config) in &config.backends {
            backends.insert(name.clone(), Backend::from_config(name, backend_config)?);
        }

        // No backends at all is a legitimate deployment rather than a mistake: a
        // listener with nothing to dispatch to is how static files are served
        // without PHP, and with no backends there is also no route that could
        // select one, because `Config::validate` requires every route to name a
        // configured backend. It is allowed here so that `check` and `serve`
        // agree about the file — `check` reports it as a warning, because a server
        // that can answer nothing but static files is usually a configuration
        // still being written rather than a finished one.
        let default = match config.default_backend.clone() {
            Some(name) => name,
            None if backends.is_empty() => String::new(),
            None => {
                return Err(ServerError::Misconfigured {
                    backend: "<default>".to_string(),
                    reason: "no default_backend is configured".to_string(),
                })
            }
        };

        if !backends.contains_key(&default) && !backends.is_empty() {
            return Err(ServerError::Misconfigured {
                backend: default,
                reason: "default_backend does not match any configured backend".to_string(),
            });
        }

        Ok(Dispatcher {
            backends,
            default,
            routing,
            routes: config.routes.clone(),
        })
    }

    /// Whether any backend was configured.
    ///
    /// False is the static-only deployment, where a request that names no file
    /// has nowhere to go. The front-ends answer 500 for one, and the CLI warns at
    /// startup, so an operator is not left reading a bare "internal error".
    pub fn has_backends(&self) -> bool {
        !self.backends.is_empty()
    }

    /// The routing table in use, so a front-end can resolve paths for logging.
    pub fn routing(&self) -> &cgi::Routing {
        &self.routing
    }

    /// The default backend's name.
    pub fn default_backend(&self) -> &str {
        &self.default
    }

    /// Whether the named backend is a worker pool.
    ///
    /// A worker reads `script_filename` as metadata and routes on `REQUEST_URI`, so a
    /// path naming no script is not a file the router needs to resolve for it — which is
    /// what lets a route to a worker skip the `stat` entirely. An unknown name is not a
    /// worker: a name that selects nothing cannot vouch for the safety of anything.
    pub fn is_worker(&self, backend: &str) -> bool {
        self.backends.get(backend).is_some_and(Backend::is_worker)
    }

    /// Names of every backend, for diagnostics.
    pub fn backend_names(&self) -> Vec<&str> {
        self.backends.values().map(Backend::name).collect()
    }

    /// The backend that should serve this request.
    ///
    /// A route may override the default, and both front-ends reach the decision
    /// through `stokehold_core::config::select_backend`, so a request gets the same
    /// answer whichever protocol carried it and whichever front-end asked. That
    /// sharing is the point: the two used to have their own copy of the precedence
    /// rule, and the HTTP one had grown `path_suffix` and the exact-host tiebreak
    /// while this one had not.
    pub fn backend_for(&self, request: &Request) -> &str {
        self.backend_for_routes(&self.routes, request)
    }
    /// [`Dispatcher::backend_for`], against a site's own route table.
    ///
    /// A site carries its own routes, so a request from nginx must be routed by the table
    /// of the site its `Host` names rather than by the server's. Passing the table in keeps
    /// one rule for both front-ends; what differs behind nginx is only that the document root
    /// is already settled, because nginx resolved `SCRIPT_FILENAME` before forwarding.
    pub fn backend_for_routes<'a>(
        &'a self,
        routes: &'a [RouteConfig],
        request: &Request,
    ) -> &'a str {
        let host = param(request, "HTTP_HOST").or_else(|| param(request, "SERVER_NAME"));
        let path = param(request, "REQUEST_URI");
        let path = path
            .as_deref()
            .map(|uri| uri.split(['?', '#']).next().unwrap_or(uri))
            .unwrap_or("/");

        stokehold_core::config::select_backend(routes, host.as_deref(), path)
            // A route that matches nothing leaves the decision to `default_backend`, which is
            // the name this deployment's configuration gave — not necessarily the first route.
            .unwrap_or(self.default.as_str())
    }

    /// Send a request to a backend chosen from a given route table.
    ///
    /// The routes are a parameter rather than a field because a site carries its own: a request
    /// from nginx must be routed by the table of the site its `Host` names. One rule, two callers
    /// — which is what keeps the two front-ends interchangeable.
    pub fn dispatch_with_routes(
        &self,
        routes: &[RouteConfig],
        request: &Request,
    ) -> Result<Response> {
        let name = self.backend_for_routes(routes, request);
        self.dispatch_to(name, request)
    }

    /// Send a request to the backend its route selects.
    pub fn dispatch(&self, request: &Request) -> Result<Response> {
        // The name is borrowed rather than cloned into a `String`. `backend_for` and
        // `dispatch_to` both only read `self`, so the borrow it returns is still live
        // when `dispatch_to` is called; owning the name was an allocation per request
        // that bought nothing.
        let name = self.backend_for(request);
        self.dispatch_to(name, request)
    }

    /// Send a request to a named backend.
    pub fn dispatch_to(&self, name: &str, request: &Request) -> Result<Response> {
        let backend = self
            .backends
            .get(name)
            // A static-only configuration reaches here with no backends at all.
            // Reported as `NoWorker` rather than as a misconfiguration, because by
            // the time a request arrives the file is a decision rather than a
            // mistake — the CLI already warned about it at startup.
            .ok_or_else(|| ServerError::NoWorker {
                backend: name.to_string(),
            })?;
        backend.request(request)
    }

    /// Send a request to the default backend, ignoring the route table.
    ///
    /// An alias used by the listener, where "default" is the meaningful word.
    /// Returns `NoWorker` for a static-only configuration, which is the one case
    /// where a request can arrive with nowhere to go.
    pub fn dispatch_to_default(&self, request: &Request) -> Result<Response> {
        self.dispatch_to(&self.default, request)
    }

    /// Slot contention in every worker pool, for the caller to report.
    ///
    /// `&self`, so it is readable at any time. The CLI reads it once, after the
    /// listeners have stopped, so a load test's numbers are in the log next to the
    /// arm they belong to.
    pub fn contention(&self) -> Vec<crate::backend::Contention> {
        self.backends
            .values()
            .filter_map(Backend::worker_contention)
            .collect()
    }

    /// Stop every backend.
    ///
    /// `&mut self`, and only reachable by the owner: `Drop` is what reaps the
    /// processes, so this is not on the request path.
    pub fn shutdown(&mut self) {
        for backend in self.backends.values_mut() {
            backend.shutdown();
        }
    }
}

/// Look up a CGI parameter as a lossy UTF-8 string.
fn param(request: &Request, name: &str) -> Option<String> {
    request
        .params
        .iter()
        .find(|(key, _)| key.as_slice() == name.as_bytes())
        .map(|(_, value)| String::from_utf8_lossy(value).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use stokehold_core::config::{BackendConfig, Config, ExternalConfig, ServerConfig};

    /// A backend that can be *built* without starting anything, but cannot serve:
    /// an `external` backend pointing at a loopback port nothing listens on.
    ///
    /// Deliberately not a `worker` backend: the worker pool starts real processes
    /// in `from_config`, and these tests are about routing, not supervision.
    fn unreachable_backend() -> BackendConfig {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a probe port");
        let socket = listener.local_addr().expect("address").to_string();
        drop(listener);
        BackendConfig::External(ExternalConfig { socket })
    }

    fn config_with(backends: &[(&str, BackendConfig)], default: &str) -> Config {
        Config {
            server: ServerConfig {
                http: Some("127.0.0.1:8080".to_string()),
                ..Default::default()
            },
            backends: backends
                .iter()
                .map(|(name, config)| (name.to_string(), config.clone()))
                .collect(),
            default_backend: Some(default.to_string()),
            routes: Vec::new(),
            sites: BTreeMap::new(),
        }
    }

    /// A request as a front-end would build it: the CGI parameters the route
    /// table reads, and nothing else.
    fn request_for(host: &str, uri: &str) -> Request {
        let pair = |name: &str, value: &str| (name.as_bytes().to_vec(), value.as_bytes().to_vec());
        Request {
            params: vec![pair("HTTP_HOST", host), pair("REQUEST_URI", uri)],
            ..Request::default()
        }
    }

    #[test]
    fn builds_every_backend_and_dispatch_reaches_the_default() {
        let config = config_with(
            &[
                ("app", unreachable_backend()),
                ("other", unreachable_backend()),
            ],
            "app",
        );
        let dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();

        assert_eq!(dispatcher.default_backend(), "app");
        let mut names = dispatcher.backend_names();
        names.sort();
        assert_eq!(names, vec!["app", "other"]);

        // The backend cannot connect, but the routing is what is under test: the
        // failure names the backend we reached, which is the default one.
        let err = dispatcher.dispatch(&Request::default()).unwrap_err();
        assert!(err.to_string().contains("app"), "{err}");
    }

    #[test]
    fn dispatch_to_an_unknown_backend_reports_it_by_name() {
        let config = config_with(&[("app", unreachable_backend())], "app");
        let dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();

        let err = dispatcher
            .dispatch_to("nope", &Request::default())
            .unwrap_err();
        assert!(matches!(err, ServerError::NoWorker { .. }), "{err}");
        assert!(err.to_string().contains("nope"), "{err}");
    }

    #[test]
    fn rejects_a_default_backend_that_does_not_exist() {
        let config = config_with(&[("app", unreachable_backend())], "missing");
        let err = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap_err();
        assert!(err.to_string().contains("default_backend"), "{err}");
    }

    /// An external backend with an unusable socket must be reported now.
    #[test]
    fn startup_fails_on_a_misconfigured_backend_rather_than_on_first_request() {
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

    #[test]
    fn a_route_selects_the_backend_and_the_default_applies_otherwise() {
        let mut config = config_with(
            &[
                ("app", unreachable_backend()),
                ("legacy", unreachable_backend()),
            ],
            "app",
        );
        config.routes = vec![RouteConfig {
            path_prefix: Some("/api".to_string()),
            path_suffix: None,
            host: None,
            backend: "legacy".to_string(),
        }];
        let dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();

        assert_eq!(
            dispatcher.backend_for(&request_for("example.test", "/api/users")),
            "legacy"
        );
        assert_eq!(
            dispatcher.backend_for(&request_for("example.test", "/other")),
            "app"
        );
        // A query string is not part of the path, so it cannot change the route.
        assert_eq!(
            dispatcher.backend_for(&request_for("example.test", "/api?x=1")),
            "legacy"
        );
    }

    #[test]
    fn a_route_narrows_an_existing_one_rather_than_widening_it() {
        // Two routes match `/api/x`; the more specific one decides. This is the
        // property that makes the table predictable rather than order-dependent.
        let mut config = config_with(
            &[
                ("app", unreachable_backend()),
                ("api", unreachable_backend()),
                ("admin_api", unreachable_backend()),
            ],
            "app",
        );
        config.routes = vec![
            RouteConfig {
                path_prefix: Some("/api".to_string()),
                path_suffix: None,
                host: None,
                backend: "api".to_string(),
            },
            RouteConfig {
                path_prefix: Some("/".to_string()),
                path_suffix: None,
                host: Some("admin.test".to_string()),
                backend: "admin_api".to_string(),
            },
        ];
        let dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();

        assert_eq!(
            dispatcher.backend_for(&request_for("admin.test", "/api/x")),
            "admin_api"
        );
        assert_eq!(
            dispatcher.backend_for(&request_for("public.test", "/api/x")),
            "api"
        );
    }
}
