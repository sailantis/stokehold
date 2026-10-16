//! Request dispatch: choosing a backend and translating errors.
//!
//! Route-level selection is deliberately simple for now — a default backend plus
//! per-host or per-path overrides, which is what the design calls for. Anything
//! more elaborate belongs with the front-ends, where the request is understood.

use std::collections::BTreeMap;

use volucer_core::config::{Config, RouteConfig};
use volucer_fcgi::{cgi, Request, Response};

use crate::backend::{Backend, UpstreamBody};
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

    /// Whether the named backend reads the path as a file to execute.
    ///
    /// The question a route's `skip_stat` turns on. Not the negation of [`Self::is_worker`]: an
    /// upstream does not interpret the path either, but for its own reason, and a route to one must
    /// skip the `stat` just as a worker's does. An unknown name answers `true` — a backend that is
    /// not there cannot vouch for skipping a check.
    pub fn interprets_path(&self, backend: &str) -> bool {
        self.backends
            .get(backend)
            .is_none_or(Backend::interprets_path)
    }

    /// Names of every backend, for diagnostics.
    pub fn backend_names(&self) -> Vec<&str> {
        self.backends.values().map(Backend::name).collect()
    }

    /// The backend that should serve this request.
    ///
    /// A route may override the default, and both front-ends reach the decision
    /// through `volucer_core::config::select_backend`, so a request gets the same
    /// answer whichever protocol carried it and whichever front-end asked. That
    /// sharing is the point: the two used to have their own copy of the precedence
    /// rule, and the HTTP one had grown `path_suffix` and the exact-host tiebreak
    /// while this one had not.
    pub fn backend_for(&self, request: &Request) -> &str {
        self.backend_for_routes(&self.routes, request)
    }

    /// The backend that should serve an HTTP request, asked without a CGI environment.
    ///
    /// The same route rule [`Self::backend_for`] uses, reached from the other front-end. It exists
    /// because the HTTP front-end has `Parts`, not a CGI environment, and building one just to ask
    /// this question would be a second, lossy representation of a request that already exists —
    /// the flattening this whole step is about avoiding.
    ///
    /// `host` is the `Host` header as sent. `path` is the request path *without* its query or
    /// fragment, and a caller that forgets is a routing difference between the two front-ends for
    /// `/api?x=1`, so the rule lives here rather than in each caller.
    pub fn backend_for_target(&self, host: Option<&str>, path: &str) -> &str {
        let path = strip_query(path);
        volucer_core::config::select_backend(&self.routes, host, path)
            .unwrap_or(self.default.as_str())
    }

    /// [`Self::backend_for_target`], against a site's own route table.
    pub fn backend_for_target_routes<'a>(
        &'a self,
        routes: &'a [RouteConfig],
        host: Option<&str>,
        path: &str,
    ) -> &'a str {
        let path = strip_query(path);
        volucer_core::config::select_backend(routes, host, path).unwrap_or(self.default.as_str())
    }

    /// Whether the named backend is an HTTP upstream.
    ///
    /// The front-end's branch, and it has to be at the front-end: every other backend is a blocking
    /// process on a pipe, so making the dispatcher's *type* async to accommodate the one that is
    /// not would drag `spawn_blocking` into the type system of all four. An unknown name answers
    /// `false`, so a name that selects nothing keeps the CGI path's behaviour.
    pub fn is_upstream(&self, backend: &str) -> bool {
        matches!(self.backends.get(backend), Some(Backend::Upstream { .. }))
    }

    /// The client for a named upstream backend.
    ///
    /// `None` for the three CGI kinds, so a caller that asks for the wrong one cannot accidentally
    /// reach an origin — and the client is borrowed rather than cloned, because it shares its pool
    /// through `&self` and cloning it per request would be an `Arc` bump for nothing.
    pub fn upstream(
        &self,
        backend: &str,
    ) -> Option<&volucer_upstream::UpstreamClient<UpstreamBody>> {
        match self.backends.get(backend) {
            Some(Backend::Upstream { client, .. }) => Some(client),
            _ => None,
        }
    }

    /// Send an HTTP request to a named upstream backend.
    ///
    /// The one place the two request paths meet, and the reason it is a method here rather than in
    /// the front-end: choosing the backend, knowing whether it is an upstream, and holding the
    /// client are all the dispatcher's business, so the listener has exactly one call to make for
    /// either path.
    pub async fn dispatch_upstream(
        &self,
        backend: &str,
        request: volucer_upstream::UpstreamRequest<UpstreamBody>,
    ) -> Result<volucer_upstream::UpstreamResponse> {
        let Some(client) = self.upstream(backend) else {
            // Reached only if a caller asked for the wrong kind, which the front-end's own branch
            // makes unreachable in practice. Naming it rather than panicking keeps a future caller's
            // mistake a 500 with a reason instead of a crashed server.
            return Err(match self.backends.get(backend) {
                Some(_) => ServerError::WrongProtocol {
                    backend: backend.to_string(),
                },
                None => ServerError::NoWorker {
                    backend: backend.to_string(),
                },
            });
        };
        client
            .request(request)
            .await
            .map_err(|source| ServerError::Upstream {
                backend: backend.to_string(),
                source,
            })
    }

    /// Whether this deployment configures an upstream backend at all.
    ///
    /// The capability check the CLI makes before refusing a whole configuration: without it,
    /// `serve` would have to reject every deployment, including the ones that never mention an
    /// upstream. It answers for the *deployment*, not for a route, because the thing that is
    /// missing is a request path rather than a routing decision.
    pub fn has_upstream(&self) -> bool {
        self.backends
            .values()
            .any(|backend| matches!(backend, Backend::Upstream { .. }))
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
        let path = path.as_deref().map(strip_query).unwrap_or("/");

        volucer_core::config::select_backend(routes, host.as_deref(), path)
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

/// A path with its query and fragment removed.
///
/// One function for both front-ends, because two copies of this rule is how `/api?x=1` comes to
/// route differently depending on which protocol carried it. A `REQUEST_URI` from a CGI client
/// carries its query the same way a request line does, so there is genuinely one rule here and not
/// two that happen to agree.
fn strip_query(path: &str) -> &str {
    path.split(['?', '#']).next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use volucer_core::config::{BackendConfig, Config, ExternalConfig, ServerConfig};

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
                BackendConfig::External(volucer_core::config::ExternalConfig {
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

    // -----------------------------------------------------------------------
    // The upstream arm: reachable from Rust, and the two routing sources must agree.
    // -----------------------------------------------------------------------

    /// An upstream config, with the origin pointed at a port nothing listens on.
    ///
    /// Deliberately unreachable: every test here is about *which* backend is chosen and whether the
    /// client exists, not about what an origin answers — that needs the listener, and lives with
    /// the front-end tests.
    fn unreachable_upstream() -> BackendConfig {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a probe port");
        let port = listener.local_addr().expect("address").port();
        drop(listener);
        BackendConfig::Upstream(volucer_core::config::UpstreamConfig {
            origin: format!("http://127.0.0.1:{port}"),
            tls: None,
        })
    }

    #[test]
    fn an_upstream_backend_builds_a_client_and_is_not_reached_over_cgi() {
        let config = config_with(
            &[
                ("node", unreachable_upstream()),
                ("cgi", unreachable_backend()),
            ],
            "node",
        );
        let dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();

        assert!(dispatcher.is_upstream("node"));
        assert!(!dispatcher.is_upstream("cgi"));
        assert!(dispatcher.upstream("node").is_some());
        assert!(dispatcher.upstream("cgi").is_none());
        // And the FastCGI path refuses it by *name* rather than trying a connection, so the error
        // says "wrong protocol" instead of "unreachable origin" — two different operator actions.
        let err = dispatcher
            .dispatch_to("node", &Request::default())
            .unwrap_err();
        assert!(matches!(err, ServerError::WrongProtocol { .. }), "{err}");
        assert!(err.to_string().contains("FastCGI"), "{err}");
    }

    #[test]
    fn an_upstream_whose_origin_cannot_be_built_is_a_misconfiguration() {
        // `Backend::from_config` is reachable without `Config::validate`, so the origin is parsed
        // here too: a backend built from an unvalidated config must fail at startup with the key
        // named, not 502 on every request.
        let config = config_with(
            &[(
                "node",
                BackendConfig::Upstream(volucer_core::config::UpstreamConfig {
                    origin: "not-an-origin".to_string(),
                    tls: None,
                }),
            )],
            "node",
        );
        let err = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap_err();
        assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
        assert!(err.to_string().contains("node"), "{err}");
        assert!(err.to_string().contains("no scheme"), "{err}");
    }

    #[test]
    fn a_ca_that_cannot_be_read_fails_at_startup_not_on_the_first_request() {
        // The rule the other backends already follow: a mistyped path is a startup failure. Without
        // it, a deployment serves happily and 502s every request for one backend.
        let config = config_with(
            &[(
                "node",
                BackendConfig::Upstream(volucer_core::config::UpstreamConfig {
                    origin: "https://node.internal:8443".to_string(),
                    tls: Some(volucer_core::config::UpstreamTlsConfig {
                        ca: Some(std::path::PathBuf::from(
                            "/definitely/not/a/ca-certificate.pem",
                        )),
                        sni: None,
                    }),
                }),
            )],
            "node",
        );
        let err = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap_err();
        assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
        assert!(err.to_string().contains("ca-certificate"), "{err}");
    }

    /// The two routing sources must give the same answer, including about a query string.
    ///
    /// This is the assertion that makes `backend_for_target` a *reuse* of the route rule rather
    /// than a second copy of it. `/api?x=1` is the case that matters: a path with its query left on
    /// matches a `path_prefix` differently from one without, so a front-end that forgot to strip it
    /// would route the same request differently depending on the protocol.
    #[test]
    fn the_cgi_and_parts_routing_agree_including_about_a_query() {
        let config = config_with(
            &[
                ("api", unreachable_backend()),
                ("app", unreachable_backend()),
                ("fallback", unreachable_backend()),
            ],
            "app",
        );
        let mut config = config;
        // Two routes, and the *default* is a third. That matters: several of the assertions below
        // compare against the default, so a test whose default was one of the routes would pass
        // even if a query-bearing path failed to match a route at all.
        config.default_backend = Some("fallback".to_string());
        config.routes = vec![
            RouteConfig {
                host: None,
                path_prefix: Some("/api".to_string()),
                path_suffix: None,
                backend: "api".to_string(),
            },
            // A *suffix* route, deliberately, because it is the one that makes the stripping
            // load-bearing: `/api` as a prefix still matches `/api?x=1` with the query left on,
            // whereas `/index.php?x=1` does not end with `.php`.
            RouteConfig {
                host: None,
                path_prefix: None,
                path_suffix: Some(".php".to_string()),
                backend: "app".to_string(),
            },
        ];
        let dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();

        let fallback = config
            .default_backend
            .clone()
            .expect("the fixture states one");
        for (host, uri, expected) in [
            ("public.test", "/api", Some("api")),
            ("public.test", "/api/x", Some("api")),
            ("public.test", "/api?x=1&y=2", Some("api")),
            ("public.test", "/api/x?trace=1", Some("api")),
            ("public.test", "/api#frag", Some("api")),
            ("public.test", "/index.php", Some("app")),
            // The case a front-end that forwarded the query would get wrong: without stripping,
            // `.php` does not match and this falls through to the default.
            ("public.test", "/index.php?x=1", Some("app")),
            ("public.test", "/index.php?x=1#top", Some("app")),
            ("public.test", "/index.php#top", Some("app")),
            ("public.test", "/other", None),
            ("public.test", "/nothing/here", None),
        ] {
            let from_cgi = dispatcher.backend_for(&request_for(host, uri));
            // `parts.uri.path()` never carries a query, and a caller that passes a whole URI needs
            // the rule — so the front-end hands over a path and the helper strips one if a caller
            // forgot. Both sources therefore agree without the front-end knowing the rule.
            let path = uri.split(['?', '#']).next().unwrap_or(uri);
            let from_parts = dispatcher.backend_for_target(Some(host), path);

            let expected = expected.unwrap_or(fallback.as_str());
            assert_eq!(from_cgi, expected, "{uri} from a CGI client");
            assert_eq!(from_parts, expected, "{uri} from an HTTP client");
        }
    }

    #[test]
    fn an_unknown_backend_is_not_an_upstream_and_keeps_the_cgi_behaviour() {
        // A name that selects nothing must not be treated as an upstream: it cannot vouch for
        // anything, so it keeps the CGI path's answer rather than sending a request to an origin
        // that was never configured.
        let config = config_with(&[("app", unreachable_backend())], "app");
        let dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();
        assert!(!dispatcher.is_upstream("nope"));
        assert!(dispatcher.upstream("nope").is_none());
        let err = dispatcher
            .dispatch_to("nope", &Request::default())
            .unwrap_err();
        assert!(matches!(err, ServerError::NoWorker { .. }), "{err}");
    }

    #[test]
    fn has_upstream_answers_for_the_deployment_rather_than_a_route() {
        // The capability check `serve` makes. A deployment with no upstream must not be refused,
        // which is why this is not "does any route name one".
        let plain = config_with(&[("app", unreachable_backend())], "app");
        let dispatcher = Dispatcher::from_config(&plain, cgi::Routing::default()).unwrap();
        assert!(!dispatcher.has_upstream());

        let with_upstream = config_with(
            &[
                ("app", unreachable_backend()),
                ("node", unreachable_upstream()),
            ],
            "app",
        );
        let dispatcher = Dispatcher::from_config(&with_upstream, cgi::Routing::default()).unwrap();
        assert!(dispatcher.has_upstream());
    }
}
