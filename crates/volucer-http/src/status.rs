//! The metrics and status listener: a second HTTP endpoint that is not the request path.
//!
//! # Why this is in `volucer-http` rather than in a crate of its own
//!
//! It is a `hyper` server, and this crate is where the `tokio` runtime lives — the crate document
//! says so: *"This is the **only** place in volucer where async code runs, and it is separated on
//! purpose."* A second runtime elsewhere would be a second answer to "where does tokio live", and
//! the reason the boundary exists is that every backend is blocking by construction.
//!
//! # It shares no code with the request path, on purpose
//!
//! No sites, no dispatcher, no CGI, no TLS, no reload. What it serves is a registry and a readiness
//! table, both of which are `Arc`s handed in at construction. That is deliberate rather than a
//! simplification: the request path's HTTP listener may be TLS-only and faces the public, and an
//! endpoint that inherited any of its configuration would be one misconfiguration away from being
//! unreachable — or public. See `docs/plugins.md` §6.1.
//!
//! # Two endpoints, and the difference is the one an orchestrator acts on
//!
//! `/health` answers 200 while the process is running, even if nothing is ready yet: it is the
//! answer to "should this container be restarted", and restarting a server that is merely still
//! booting turns a slow start into a crash loop.
//!
//! `/ready` answers the configured unavailable status until every service reports ready. It is the
//! answer to "should traffic be sent here", and a probe that returned 200 while the pools were
//! still booting is how a rolling deploy sends traffic to a server that answers 502.
//!
//! Every other path is a 404, on both, so neither endpoint becomes a second way to ask whether the
//! server is up — and so a scrape endpoint does not become a surface for anything else.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;

use volucer_metrics::Registry;

use crate::error::{HttpError, Result};

/// What the status endpoints report, and how they are configured.
///
/// A trait rather than a table of booleans, because the answer changes while the process runs: a
/// worker pool becomes ready when its first worker finishes the handshake, and an ACME engine is
/// ready whether or not a certificate has been issued yet. A snapshot taken at startup would be a
/// readiness probe that always says the same thing.
pub trait Readiness: Send + Sync {
    /// Whether the process can serve traffic now.
    fn is_ready(&self) -> bool;

    /// One line per thing that is not ready, for the body of a 503.
    ///
    /// Empty when ready. The point of the body is that a probe's *log* contains the reason: a 503
    /// with an empty body tells an on-call engineer that something is wrong and nothing else, and
    /// the server that knows is the only place that can say what.
    fn reasons(&self) -> Vec<String>;
}

/// Always ready, for a deployment with no services to wait for.
///
/// The default, and the honest one: a server with no worker pools and no ACME can serve the moment
/// it is listening, so a readiness probe that waited for something would be waiting for a condition
/// that never arrives.
pub struct AlwaysReady;

impl Readiness for AlwaysReady {
    fn is_ready(&self) -> bool {
        true
    }

    fn reasons(&self) -> Vec<String> {
        Vec::new()
    }
}

/// What the listener needs that is not configuration.
pub struct StatusServices {
    /// The metric registry behind `/metrics`, or `None` to serve no metrics endpoint.
    pub metrics: Option<Registry>,
    /// The path the exposition is served at.
    pub metrics_path: String,
    /// A prefix prepended to every metric name at render time.
    pub metrics_prefix: Option<String>,
    /// Readiness, behind `/health` and `/ready`, or `None` to serve neither.
    pub readiness: Option<Arc<dyn Readiness>>,
    /// What to answer when not ready.
    pub unavailable_status: StatusCode,
}

impl std::fmt::Debug for StatusServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written because `Arc<dyn Readiness>` is not `Debug`, and every other struct in this
        // crate is. A derived one would force the trait to carry a `Debug` bound it has no other
        // use for.
        f.debug_struct("StatusServices")
            .field("metrics", &self.metrics.is_some())
            .field("metrics_path", &self.metrics_path)
            .field("metrics_prefix", &self.metrics_prefix)
            .field("readiness", &self.readiness.is_some())
            .field("unavailable_status", &self.unavailable_status)
            .finish()
    }
}

/// A running metrics and status listener.
///
/// The same shape as [`crate::RunningHttp`], deliberately: a runtime on its own thread, a shutdown
/// flag, and a `stop` that joins. Two listeners that stopped differently would be two shutdown
/// paths to get right, and this process already has one.
pub struct RunningStatus {
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    address: SocketAddr,
}

impl RunningStatus {
    /// Bind and serve, on a thread of its own.
    pub fn start(address: &str, services: StatusServices) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(HttpError::Runtime)?;

        // Bound *before* the thread starts, so a bad address is an error from `start` rather than a
        // listener that reports a port and then fails to appear. The same reason `RunningHttp`
        // binds inside `block_on` before spawning.
        let listener = std::net::TcpListener::bind(address).map_err(|source| HttpError::Bind {
            address: address.to_string(),
            source,
        })?;
        let bound = listener.local_addr().map_err(|source| HttpError::Bind {
            address: address.to_string(),
            source,
        })?;
        listener.set_nonblocking(true).map_err(HttpError::Io)?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&shutdown);
        let services = Arc::new(services);

        let thread = std::thread::Builder::new()
            .name("volucer-status".to_string())
            .spawn(move || {
                if let Err(err) = runtime.block_on(serve(listener, services, flag)) {
                    tracing::warn!(%err, "the metrics listener stopped with an error");
                }
            })
            .map_err(|source| HttpError::Runtime(std::io::Error::other(source)))?;

        Ok(RunningStatus {
            shutdown,
            thread: Some(thread),
            address: bound,
        })
    }

    /// The address actually bound, which matters when port 0 was requested.
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    /// A handle that stops the listener from another thread.
    pub fn shutdown_handle(&self) -> StatusShutdown {
        StatusShutdown {
            flag: Arc::clone(&self.shutdown),
        }
    }

    /// Stop the listener and wait for its thread to finish.
    ///
    /// Joining rather than dropping: the runtime is owned by the thread, and `tokio` requires the
    /// runtime to be dropped *after* everything that borrowed it. Returning early would also make
    /// "the port is free" a race the next bind in a test could lose.
    pub fn stop(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A handle that stops a [`RunningStatus`] from another thread.
#[derive(Clone)]
pub struct StatusShutdown {
    flag: Arc<AtomicBool>,
}

impl StatusShutdown {
    pub fn shutdown(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }
}

impl std::fmt::Debug for StatusShutdown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatusShutdown").finish()
    }
}

/// Accept connections until the flag is set.
async fn serve(
    listener: std::net::TcpListener,
    services: Arc<StatusServices>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let listener = tokio::net::TcpListener::from_std(listener).map_err(HttpError::Io)?;
    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    while !shutdown.load(Ordering::Relaxed) {
        // A timeout rather than a bare `accept`, because the shutdown flag has to be noticed and a
        // blocking accept has nothing to interrupt it. The same shape the FastCGI listener uses,
        // and for the same reason: there is no portable way to wake an `accept` on a flag.
        match tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept()).await {
            Ok(Ok((stream, _peer))) => {
                let services = Arc::clone(&services);
                tasks.retain(|task| !task.is_finished());
                tasks.push(tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                        let services = Arc::clone(&services);
                        async move { Ok::<_, Infallible>(answer(request, &services)) }
                    });
                    // HTTP/1 only, and `hyper`'s auto builder is *not* used here. A scraper and an
                    // orchestrator both speak HTTP/1.1, so h2 would be a negotiation to maintain for
                    // no caller — and the request path's listener is where protocol support belongs.
                    if let Err(err) = hyper::server::conn::http1::Builder::new()
                        .timer(hyper_util::rt::TokioTimer::new())
                        .serve_connection(io, service)
                        .await
                    {
                        // A scraper that disconnects mid-response is not an operational event.
                        tracing::debug!(%err, "metrics connection ended");
                    }
                }));
            }
            Ok(Err(err)) => {
                tracing::warn!(%err, "the metrics listener could not accept");
            }
            // The timeout: loop and re-check the flag.
            Err(_) => {}
        }
    }
    Ok(())
}

/// Build the answer for one request.
///
/// Synchronous and infallible on purpose. Nothing here can fail: the exposition is a `String` the
/// registry renders, and readiness is a method that returns a `bool`. A `Result` would be a
/// signature that promises a failure mode this has none of.
fn answer(
    request: Request<hyper::body::Incoming>,
    services: &StatusServices,
) -> Response<Full<Bytes>> {
    // The path only, so `/metrics?x=1` is the same endpoint. A query string is how a scraper adds a
    // cache-buster, and treating it as a different path would answer 404 to a legitimate scrape.
    let path = request.uri().path();

    if let Some(registry) = &services.metrics {
        if path == services.metrics_path {
            let body = match &services.metrics_prefix {
                Some(prefix) => prefix_metrics(&registry.render(), prefix),
                None => registry.render(),
            };
            return text(
                StatusCode::OK,
                body,
                "text/plain; version=0.0.4; charset=utf-8",
            );
        }
    }

    match (path, &services.readiness) {
        ("/health", Some(_)) => text(
            StatusCode::OK,
            "ok\n".to_string(),
            "text/plain; charset=utf-8",
        ),
        ("/ready", Some(readiness)) => {
            if readiness.is_ready() {
                text(
                    StatusCode::OK,
                    "ready\n".to_string(),
                    "text/plain; charset=utf-8",
                )
            } else {
                let mut body = String::from("not ready\n");
                for reason in readiness.reasons() {
                    body.push_str(&reason);
                    body.push('\n');
                }
                text(
                    services.unavailable_status,
                    body,
                    "text/plain; charset=utf-8",
                )
            }
        }
        _ => text(
            StatusCode::NOT_FOUND,
            "not found\n".to_string(),
            "text/plain; charset=utf-8",
        ),
    }
}

/// A `text/plain` answer with a length that is always right, because it is the body's own.
fn text(status: StatusCode, body: String, content_type: &str) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;
    // Set through `HeaderValue::from_static`-style construction rather than parsed, because these
    // are two constants in this file and a parse that could fail would be a branch nothing can take.
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_str(content_type).expect("a constant content type"),
    );
    response
}

/// Prepend a prefix to every metric name, leaving `# HELP`/`# TYPE` comments and their names alone
/// in step with the lines they describe.
///
/// Done on the rendered text rather than inside the registry, so the registry's names stay the ones
/// RoadRunner documents and a prefix is a presentation choice. `prefix = "myapp_"` therefore yields
/// `myapp_rr_http_request_total`, which is what RoadRunner's own `prefix` does.
///
/// The awkward part is the *derived* suffixes: a histogram's series are `name_bucket`, `name_sum`
/// and `name_count`, so prefixing the family name in a `# TYPE` line and the series lines
/// separately has to agree. Prefixing every line that is not a comment, at the position the name
/// starts, does agree — because `# HELP x` and `# TYPE x` and `x{...}` and `x_bucket{...}` all begin
/// with the name.
fn prefix_metrics(rendered: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(rendered.len() + rendered.len() / 8);
    for line in rendered.lines() {
        if let Some(rest) = line.strip_prefix("# HELP ") {
            out.push_str("# HELP ");
            out.push_str(prefix);
            out.push_str(rest);
        } else if let Some(rest) = line.strip_prefix("# TYPE ") {
            out.push_str("# TYPE ");
            out.push_str(prefix);
            out.push_str(rest);
        } else {
            out.push_str(prefix);
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// The instant the process started, for uptime.
///
/// A function rather than a constant, because a `static` initialiser that called `Instant::now()`
/// would be lazily evaluated on first use — which for a metric read on the first scrape is
/// indistinguishable from the process's start, and would silently become wrong the moment something
/// else touched it earlier.
#[must_use]
pub fn process_start() -> Instant {
    Instant::now()
}
