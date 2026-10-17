//! The listener loop: accept connections, serve requests, dispatch to backends.
//!
//! Deliberately thread-per-connection on blocking sockets rather than an async
//! runtime. The reason is the backend, not the front-end: `php-cgi` processes
//! requests sequentially and a slot serves one request at a time, so the
//! concurrency ceiling is the number of slots. Threads map onto that directly,
//! keep the code readable, and avoid an executor in a program whose real work is
//! process supervision.
//!
//! The dispatcher is shared without a lock of its own: each backend slot
//! synchronises internally, so a request to one worker does not hold up a
//! request to another. That is what makes `workers` mean parallelism as well as
//! recycling, and it is the change this comment used to say would be needed here
//! once the worker pool landed.
//!
//! Thread-per-connection is deliberate. Threads map onto the number of backend
//! slots directly, keep the code readable, and avoid an executor in a program
//! whose real work is process supervision.

use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use volucer_core::sites::Sites;
use volucer_observe::access_log::Record;
use volucer_observe::{Observability, RequestContext};
use volucer_watch::{Registry, SharedState};

use crate::dispatch::Dispatcher;
use crate::error::{Result, ServerError};
use crate::frontend::fastcgi::{self, ServerErrorKind};

/// How long an idle keep-alive connection may sit before it is closed.
///
/// Without this a keep-alive connection from nginx would occupy a thread for
/// ever, and enough of them would exhaust the process's threads.
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(15);

/// How much of a request to read ahead of the codec.
///
/// A front-end sends the environment in records of up to `MAX_CONTENT_LEN`, but the common case
/// is a handful of small ones, and the codec asks for each record's header and content
/// separately. This is sized so an ordinary environment arrives in one fill; a larger one costs
/// one read per buffer rather than one per field.
const READ_BUFFER_LEN: usize = 8 * 1024;

/// Limits on concurrent connections, so a misbehaving client cannot make the
/// process allocate unboundedly many threads.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_connections: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_connections: 512,
        }
    }
}

/// What the listener reports about its own behaviour.
#[derive(Debug, Default)]
pub struct Stats {
    pub connections: AtomicUsize,
    pub requests: AtomicUsize,
    pub rejected: AtomicUsize,
    pub failed: AtomicUsize,
}

impl Stats {
    fn snapshot(&self) -> (usize, usize, usize, usize) {
        (
            self.connections.load(Ordering::Relaxed),
            self.requests.load(Ordering::Relaxed),
            self.rejected.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
        )
    }
}

/// A running FastCGI listener.
pub struct FastCgiListener {
    listener: TcpListener,
    dispatcher: Arc<Dispatcher>,
    /// The site table, shared with the HTTP front-end, so a reload moves both.
    ///
    /// Site selection behind nginx can only change the *backend*: nginx resolved
    /// `SCRIPT_FILENAME` before forwarding and the params are relayed verbatim, so the document
    /// root is already settled. That is still the difference between one pool and another — and
    /// it is why this must move with the HTTP listener rather than beside it. A reload that
    /// reached only one front-end took effect on one protocol, which reads as the other one being
    /// right and is invisible to any test that exercises a single front-end.
    sites: SharedState,
    /// The process's one registry, so this listener is reachable by a reload.
    ///
    /// Held rather than derived from `sites` because a reload has to rebuild the *derived* state of
    /// every front-end, and this listener has none of its own — it resolves per request. Keeping
    /// the registry here is what lets `serve` hold one place that a trigger applies through.
    registry: Arc<Registry>,
    shutdown: Arc<AtomicBool>,
    stats: Arc<Stats>,
    limits: Limits,
    /// Request identity and the access log, shared with the HTTP front-end.
    ///
    /// One `Arc` for the process rather than one per listener: a deployment behind nginx runs both
    /// front-ends, and two access logs would mean one application's requests appear in two files
    /// depending on which front-end received them. Defaults to disabled so every existing caller
    /// and test is unchanged — the "off by default costs nothing" rule, made structural.
    observe: Arc<volucer_observe::Observability>,
}

impl FastCgiListener {
    /// Bind a listener.
    ///
    /// Binding happens here rather than inside `run`, so a port conflict is
    /// reported before any backend is started.
    ///
    /// The dispatcher arrives as an `Arc` because it is shared: the HTTP
    /// front-end serves the same backends, so there is one set of processes
    /// behind one routing decision, whichever protocol a request arrived on. It
    /// needs no lock because each backend synchronises internally.
    ///
    /// A listener on its own gets a registry of its own, so that a reload still
    /// has something to apply through.
    pub fn bind(
        address: &str,
        dispatcher: Arc<Dispatcher>,
        sites: Arc<Sites>,
        limits: Limits,
    ) -> Result<Self> {
        let registry = Arc::new(Registry::new(SharedState::new((*sites).clone())));
        Self::bind_registered(address, dispatcher, registry, limits)
    }

    /// [`FastCgiListener::bind`], around the process's one registry.
    ///
    /// The two-listener case, and the only way `serve` can give both front-ends one table. A
    /// listener that built its own would be the one still serving the old configuration after a
    /// reload, which reads as the *other* front-end being right and is invisible to any test that
    /// exercises one protocol — so this parameter prevents the defect rather than documenting it.
    ///
    /// The registry is taken rather than a bare table because a FastCGI listener needs the table
    /// *and* the reload path that moves it; passing only the table is how a reload came to reach
    /// this listener's data but not its configuration.
    pub fn bind_registered(
        address: &str,
        dispatcher: Arc<Dispatcher>,
        registry: Arc<Registry>,
        limits: Limits,
    ) -> Result<Self> {
        let listener = TcpListener::bind(address).map_err(|source| ServerError::Misconfigured {
            backend: "<listener>".to_string(),
            reason: format!("cannot bind {address}: {source}"),
        })?;
        listener.set_nonblocking(false).map_err(ServerError::Io)?;

        Ok(FastCgiListener {
            listener,
            dispatcher,
            sites: registry.shared().clone(),
            registry,
            shutdown: Arc::new(AtomicBool::new(false)),
            stats: Arc::new(Stats::default()),
            limits,
            observe: Arc::new(volucer_observe::Observability::disabled()),
        })
    }

    /// The same listener with request identity and an access log.
    ///
    /// A builder rather than another argument on [`FastCgiListener::bind_registered`], because the
    /// observability handle is a process-wide thing that only `serve` has: every existing caller
    /// and every test constructs a listener without one, and those must keep compiling and keep
    /// costing nothing.
    #[must_use]
    pub fn with_observability(mut self, observe: Arc<volucer_observe::Observability>) -> Self {
        self.observe = observe;
        self
    }

    /// The site table this listener serves from, shared with every other front-end.
    ///
    /// A clone of the handle rather than of the table, so replacing through it moves all of them.
    pub fn shared_state(&self) -> SharedState {
        self.sites.clone()
    }

    /// The process's one registry, which a reload applies through.
    pub fn registry(&self) -> Arc<Registry> {
        Arc::clone(&self.registry)
    }

    /// A handle that replaces what this listener is serving, without a restart.
    ///
    /// The process's registry, and that is the point: both front-ends hold one, so replacing
    /// through it moves both. There is deliberately no per-listener reload path, because
    /// per-listener is the bug.
    pub fn reload_handle(&self) -> Arc<Registry> {
        Arc::clone(&self.registry)
    }

    /// The address actually bound, which matters when port 0 was requested.
    pub fn local_addr(&self) -> Result<std::net::SocketAddr> {
        self.listener.local_addr().map_err(ServerError::Io)
    }

    /// A handle that stops the listener from another thread.
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            flag: Arc::clone(&self.shutdown),
        }
    }

    /// Statistics, for logging.
    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }

    /// Accept and serve connections until [`ShutdownHandle::shutdown`] is called.
    pub fn run(self) -> Result<()> {
        // A read timeout on the listener would break accept; instead the flag is
        // checked between accepts, and the loop uses a short poll interval.
        self.listener
            .set_nonblocking(true)
            .map_err(ServerError::Io)?;

        let mut handlers = Vec::new();
        while !self.shutdown.load(Ordering::Relaxed) {
            match self.listener.accept() {
                Ok((stream, peer)) => {
                    let active = self.stats.connections.fetch_add(1, Ordering::Relaxed) + 1;
                    if active > self.limits.max_connections {
                        self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(%peer, active, "refusing a connection: at the limit");
                        drop(stream);
                        continue;
                    }

                    let dispatcher = Arc::clone(&self.dispatcher);
                    let sites = self.sites.clone();
                    let stats = Arc::clone(&self.stats);
                    let shutdown = Arc::clone(&self.shutdown);
                    let max_connections = self.limits.max_connections;
                    let observe = Arc::clone(&self.observe);
                    handlers.retain(|handle: &std::thread::JoinHandle<()>| !handle.is_finished());
                    handlers.push(std::thread::spawn(move || {
                        if let Err(err) = serve_connection(
                            stream,
                            dispatcher,
                            sites,
                            stats,
                            shutdown,
                            max_connections,
                            observe,
                        ) {
                            tracing::debug!(%peer, %err, "connection ended");
                        }
                    }));
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(source) => {
                    // A transient accept failure should not kill the listener.
                    tracing::warn!(%source, "accept failed");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }

        tracing::info!("listener stopping");
        for handle in handlers {
            let _ = handle.join();
        }
        let (connections, requests, rejected, failed) = self.stats.snapshot();
        tracing::info!(connections, requests, rejected, failed, "listener stopped");
        Ok(())
    }
}

/// Stops a listener from another thread.
#[derive(Clone)]
pub struct ShutdownHandle {
    flag: Arc<AtomicBool>,
}

impl ShutdownHandle {
    pub fn shutdown(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    pub fn is_shutting_down(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

/// A request's CGI parameter, as text, if it is there and is valid UTF-8.
///
/// The environment is bytes, and a header value with an invalid byte is not a reason to refuse the
/// request — it just cannot be a request id or a trace context, so `None` is the honest answer and
/// the caller mints a fresh one.
fn param<'a>(params: &'a [(Vec<u8>, Vec<u8>)], name: &[u8]) -> Option<&'a str> {
    let (_, value) = params.iter().find(|(key, _)| key.as_slice() == name)?;
    std::str::from_utf8(value).ok()
}

/// The status the client will see for a failed dispatch.
///
/// Read off the same classification the error response uses, so the log and the answer cannot
/// disagree. `AllBusy` is a 503 because a full pool is temporary; the rest are a 502, which is what
/// a gateway says when the thing behind it failed.
fn server_error_status(err: &ServerError) -> u16 {
    match err {
        ServerError::AllBusy { .. } => 503,
        _ => 502,
    }
}

/// Write one access-log record for a request that arrived over FastCGI.
///
/// A free function rather than a method: it needs the request environment, the context and the
/// elapsed time, none of which belong to the listener.
///
/// **`REMOTE_ADDR` here is nginx's address, not the client's**, and that is the correct thing to
/// log: this listener's peer really is nginx, and nginx's own access log carries the real client.
/// Writing the header nginx put in `X-Forwarded-For` instead would make this log assert a peer it
/// never saw, which is the one thing a log must not do.
fn record_fastcgi(
    observe: &Observability,
    context: &RequestContext,
    request: &volucer_fcgi::Request,
    status: u16,
    bytes_sent: u64,
    elapsed: std::time::Duration,
) {
    let params = request.params.as_slice();
    let id = context.id.as_str();
    observe.access_log().record(&Record {
        id: &id,
        method: param(params, b"REQUEST_METHOD").unwrap_or("-"),
        target: param(params, b"REQUEST_URI").unwrap_or("-"),
        protocol: param(params, b"SERVER_PROTOCOL").unwrap_or("-"),
        peer: param(params, b"REMOTE_ADDR").and_then(|value| value.parse().ok()),
        host: param(params, b"HTTP_HOST").or_else(|| param(params, b"SERVER_NAME")),
        user_agent: param(params, b"HTTP_USER_AGENT"),
        referer: param(params, b"HTTP_REFERER"),
        status,
        total_seconds: elapsed.as_secs_f64(),
        // FastCGI has no separately-timed origin: the backend *is* the origin, and PHP's own
        // duration arrives in the worker protocol's `END` frame rather than as a header here.
        origin_seconds: None,
        bytes_sent,
    });
}

/// Serve requests on one connection until the peer closes or keep-alive ends.
fn serve_connection(
    stream: std::net::TcpStream,
    dispatcher: Arc<Dispatcher>,
    sites: SharedState,
    stats: Arc<Stats>,
    shutdown: Arc<AtomicBool>,
    max_connections: usize,
    observe: Arc<volucer_observe::Observability>,
) -> Result<()> {
    // On Windows an accepted socket inherits the listener's non-blocking mode,
    // so this must be reset explicitly. Without it every read returns
    // WSAEWOULDBLOCK immediately and the connection is dropped right after the
    // first record — which is exactly what happened before this line existed.
    stream.set_nonblocking(false).map_err(ServerError::Io)?;

    // The keep-alive timeout is what stops an idle connection holding a thread.
    stream
        .set_read_timeout(Some(KEEPALIVE_TIMEOUT))
        .map_err(ServerError::Io)?;
    let _ = stream.set_nodelay(true);

    // Buffered, because every record the front-end sends is a header followed by a content
    // payload and the codec asks for the two separately: unbuffered, a request whose environment
    // arrives as a dozen small records costs about three `read(2)`s each, and every one of them
    // is a syscall the previous one could have answered. Measured: 40 reads for a 12-record
    // request, 7 for the same request in one record. Writes go through `get_mut`, so a response
    // still leaves in its records rather than being held back, and the read half stays buffered.
    let mut stream = std::io::BufReader::with_capacity(READ_BUFFER_LEN, stream);
    let max_connections_string = max_connections.to_string();
    let mut requests_string = String::new();

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }

        let next = match fastcgi::read_next(&mut stream) {
            Ok(Some(next)) => next,
            // Clean end of stream: the peer closed between requests.
            Ok(None) => return Ok(()),
            Err(err) => return Err(err),
        };

        // A `GET_VALUES` probe is not a request: it must be answered with a
        // result record, never an END_REQUEST. nginx uses it to decide whether
        // this server is usable.
        let incoming = match next {
            fastcgi::NextRequest::GetValues(names) => {
                tracing::debug!(count = names.len(), "answering a GET_VALUES probe");
                let values = [
                    ("max_connections", max_connections_string.as_str()),
                    ("requests", requests_string.as_str()),
                ];
                fastcgi::write_get_values(stream.get_mut(), &values)?;
                continue;
            }
            fastcgi::NextRequest::Request(incoming) => incoming,
        };

        if incoming.aborted {
            // The client gave up before we produced anything, so there is
            // nothing to write.
            tracing::debug!("a request was aborted before it was served");
            continue;
        }

        let keep_conn = incoming.keep_conn;
        let request_id = volucer_fcgi::REQUEST_ID;

        // Routed, not `dispatch_to_default`. A request arriving from nginx still has a route
        // table, and a route naming a host or a path is the operator's way of saying which pool
        // serves it; bypassing the table meant the two front-ends chose differently for the same
        // request. The site is resolved from `HTTP_HOST` first, then `SERVER_NAME`, because a
        // client that sent no `Host` must not lose its site to nginx's `fastcgi_param`.
        //
        // What site selection *cannot* change behind nginx is the document root: nginx resolved
        // `SCRIPT_FILENAME` before forwarding and the params are relayed verbatim. So this
        // reaches the backend and nothing else, which is still the difference between one pool
        // and another.
        let request = incoming.into_backend_request();
        let host = request
            .params
            .iter()
            .find(|(key, _)| key.as_slice() == b"HTTP_HOST")
            .or_else(|| {
                request
                    .params
                    .iter()
                    .find(|(key, _)| key.as_slice() == b"SERVER_NAME")
            })
            .map(|(_, value)| String::from_utf8_lossy(value).into_owned());
        // One snapshot for the request, so the site is resolved against one generation of the
        // table even if a reload lands here — the same rule the HTTP front-end follows. Taken
        // *per request* rather than per connection: nginx keeps its upstream alive, so the request
        // that arrives after a reload very often arrives on a connection accepted before it, and a
        // per-connection snapshot would serve the old configuration for the life of that
        // connection. That is what this line was written to fix, and the test pins it.
        let snapshot = sites.sites();
        let site = snapshot.resolve(host.as_deref());
        // **Identity is resolved per request, here, on the request path.** nginx keeps its upstream
        // alive, so the next request very often arrives on a connection accepted long before it; a
        // per-connection id would give two requests the same id, which is worse than no id at all
        // because it looks like correlation and is not.
        let context = observe.begin(
            param(request.params.as_slice(), b"HTTP_X_REQUEST_ID"),
            param(request.params.as_slice(), b"HTTP_TRACEPARENT"),
        );
        let started = Instant::now();
        let response = dispatcher.dispatch_with_routes(&site.routes, &request);
        match response {
            Ok(response) => {
                stats.requests.fetch_add(1, Ordering::Relaxed);
                requests_string = stats.requests.load(Ordering::Relaxed).to_string();
                if !response.stderr.is_empty() {
                    // nginx puts this in its error log, which is where a
                    // developer looks first when PHP complains.
                    tracing::debug!(
                        stderr = %String::from_utf8_lossy(&response.stderr).trim_end(),
                        "backend wrote to STDERR"
                    );
                }
                fastcgi::write_response(
                    stream.get_mut(),
                    request_id,
                    &response.stdout,
                    &response.stderr,
                    keep_conn,
                )?;
                // **Logged after the answer is written, and that order is a decision.** A record
                // written before the bytes leave would claim a delivery that a failed write then
                // contradicts, and the write is the part that can fail. The status is the one the
                // backend actually sent — the same `parse` this front-end relies on, so the log
                // cannot disagree with what the client was told.
                if observe.is_enabled() {
                    let (status, _, body) = response.parse();
                    record_fastcgi(
                        &observe,
                        &context,
                        &request,
                        status,
                        body.len() as u64,
                        started.elapsed(),
                    );
                }
            }
            Err(err) => {
                stats.failed.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(%err, "dispatch failed");
                // A failed request is logged too, and with the status the *client* will see. An
                // access log that only records successes is the one an operator needs least: the
                // 502s are why they are reading it.
                if observe.is_enabled() {
                    record_fastcgi(
                        &observe,
                        &context,
                        &request,
                        server_error_status(&err),
                        0,
                        started.elapsed(),
                    );
                }
                fastcgi::write_error(
                    stream.get_mut(),
                    request_id,
                    &err.to_string(),
                    ServerErrorKind::Backend,
                )?;
            }
        }

        if !keep_conn {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn limits_have_a_sane_default() {
        assert!(Limits::default().max_connections > 0);
    }

    #[test]
    fn a_shutdown_handle_stops_the_listener() {
        let flag = Arc::new(AtomicBool::new(false));
        let handle = ShutdownHandle {
            flag: Arc::clone(&flag),
        };
        assert!(!handle.is_shutting_down());
        handle.shutdown();
        assert!(handle.is_shutting_down());
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn binding_a_used_port_is_a_misconfiguration_not_a_panic() {
        let held = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = held.local_addr().expect("address").to_string();

        let config = volucer_core::config::Config {
            server: volucer_core::config::ServerConfig {
                fastcgi: Some(address.clone()),
                ..Default::default()
            },
            // Nothing here has to start: the dispatcher is built but never used,
            // because binding fails first. An `external` backend pointing at a
            // port nothing listens on costs nothing and starts no process.
            backends: [(
                "fpm".to_string(),
                volucer_core::config::BackendConfig::External(
                    volucer_core::config::ExternalConfig {
                        socket: "127.0.0.1:1".to_string(),
                    },
                ),
            )]
            .into_iter()
            .collect(),
            default_backend: Some("fpm".to_string()),
            routes: Vec::new(),
            sites: BTreeMap::new(),
            ..volucer_core::config::Config::default()
        };
        let dispatcher =
            Dispatcher::from_config(&config, volucer_fcgi::Routing::default()).expect("dispatcher");

        let err = FastCgiListener::bind(
            &address,
            std::sync::Arc::new(dispatcher),
            std::sync::Arc::new(Sites::from_config(&config, std::path::Path::new("."))),
            Limits::default(),
        )
        .err()
        .unwrap_or_else(|| panic!("binding {address} should have failed"));
        assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
        assert!(err.to_string().contains("cannot bind"), "{err}");
    }

    #[test]
    fn stats_start_at_zero() {
        let stats = Stats::default();
        assert_eq!(stats.snapshot(), (0, 0, 0, 0));
    }
}
