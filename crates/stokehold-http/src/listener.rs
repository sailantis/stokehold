//! The HTTP listener: accept connections, translate to CGI, dispatch, answer.
//!
//! Both front-ends do the same job — read a client protocol, produce the CGI
//! environment, hand it to the one dispatcher, translate the answer back. The
//! difference is the transport and the dialect. That is why this module's real
//! work is two conversions, [`to_cgi`] and [`from_cgi`], with the network
//! machinery around them being as thin as the protocols allow.
//!
//! # The async boundary
//!
//! Every backend is blocking: a worker slot is one process on a pipe serving one
//! request at a time, and `php-cgi`/`php-fpm` process requests sequentially. So
//! the dispatcher is reached through `tokio::task::spawn_blocking`, never
//! directly. This is the single line that keeps the two worlds apart, and the
//! rule not to break is: **no blocking call in an async context**.
//!
//! # Request body handling
//!
//! The body is read into memory before dispatch, up to
//! [`HttpLimits::max_body_bytes`]. That is a deliberate limit rather than a
//! shortcut: the CGI protocol carries a body as one `STDIN` stream preceded by
//! its length, and `php-cgi` needs `CONTENT_LENGTH` before it will read any of
//! it, so a streaming pass-through is not available. Multipart parsing is not
//! done here either — the bytes go to PHP, which owns `$_FILES` and
//! `is_uploaded_file()`.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue, CONTENT_LENGTH, CONTENT_TYPE, HOST};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};

use stokehold_core::sites::{ResolvedSite, Sites};
use stokehold_fcgi::{cgi, RequestInfo, Route, Routing};
use stokehold_server::{Dispatcher, ServerError};

use crate::error::{HttpError, Result};
use crate::static_files::{self, StaticFiles};

/// The body type every response uses.
///
/// A backend answer is a `Vec` that is already in memory, while a static file is
/// a stream, so the two have to meet somewhere. Boxing is that meeting point: one
/// allocation per response, against a static file that would otherwise have to be
/// buffered whole to be expressible.
pub type ResponseBody = BoxBody<Bytes, std::io::Error>;

/// A body made of bytes that are already in memory.
pub fn body_of(bytes: Bytes) -> ResponseBody {
    Full::new(bytes)
        .map_err(|never: std::convert::Infallible| match never {})
        .boxed()
}

/// How long an idle keep-alive connection may sit before it is closed.
///
/// The same fifteen seconds the FastCGI listener uses. Without it, an idle
/// connection holds one of the capped slots for ever.
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(15);

/// How long `accept` waits before the shutdown flag is checked again.
///
/// `hyper` and `tokio` between them need no runtime configuration, and adding a
/// signal stream just to interrupt an `accept` would be the only reason to pull
/// one in. Polling matches the FastCGI listener, and 200 ms is far below the time
/// an operator notices.
const ACCEPT_POLL: Duration = Duration::from_millis(200);

/// Limits on the HTTP listener.
#[derive(Debug, Clone, Copy)]
pub struct HttpLimits {
    /// Maximum concurrent connections. One accepted connection holds a slot for
    /// as long as it is open, keep-alive included.
    pub max_connections: usize,
    /// Maximum request body, in bytes.
    ///
    /// The body is buffered so its length can be declared to the backend. PHP's
    /// own `post_max_size` is a separate, later limit; this one exists so an
    /// unauthenticated client cannot make the server allocate without bound.
    pub max_body_bytes: usize,
}

impl Default for HttpLimits {
    fn default() -> Self {
        HttpLimits {
            max_connections: 512,
            max_body_bytes: 32 * 1024 * 1024,
        }
    }
}

/// What the listener reports about its own behaviour.
#[derive(Debug, Default)]
pub struct HttpStats {
    pub connections: AtomicUsize,
    pub requests: AtomicUsize,
    pub rejected: AtomicUsize,
    pub failed: AtomicUsize,
    /// Handshakes that completed. Zero on a deployment without TLS.
    pub handshakes: AtomicUsize,
    /// Handshakes that failed.
    ///
    /// Counted rather than only logged because a TLS misconfiguration is the one failure that
    /// is invisible from the client's side: the client sees a handshake error and never reaches
    /// the server's logs. A non-zero count on a port that should be idle is the signal.
    pub handshake_failures: AtomicUsize,
    /// Cleartext connections answered on a TLS port.
    ///
    /// Only possible with `[server.tls].plain`, which is off by default and logged at startup.
    /// Counted so a deployment can see it happening rather than inferring it.
    pub plaintext_on_tls_port: AtomicUsize,
}

impl HttpStats {
    /// The counters as a tuple, for logging at shutdown.
    pub fn snapshot(&self) -> (usize, usize, usize, usize) {
        (
            self.connections.load(Ordering::Relaxed),
            self.requests.load(Ordering::Relaxed),
            self.rejected.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
        )
    }

    /// The TLS counters, for logging at shutdown.
    pub fn tls_snapshot(&self) -> (usize, usize, usize) {
        (
            self.handshakes.load(Ordering::Relaxed),
            self.handshake_failures.load(Ordering::Relaxed),
            self.plaintext_on_tls_port.load(Ordering::Relaxed),
        )
    }
}

/// TLS settings for a listener, borrowed from the configuration.
#[derive(Clone, Copy)]
pub struct TlsSettings<'a> {
    /// The validated configuration, read for `[server.tls]` and the per-site tables.
    pub config: &'a stokehold_core::config::Config,
    /// Where a relative `certificate`/`key` resolves.
    pub config_dir: &'a std::path::Path,
}

/// Stops the listener from another thread.
///
/// A plain flag, exactly as the FastCGI listener uses, so the Ctrl-C handler
/// lives on an ordinary thread and no runtime handle has to be passed around.
#[derive(Clone)]
pub struct HttpShutdown {
    flag: Arc<AtomicBool>,
}

impl HttpShutdown {
    pub fn shutdown(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    pub fn is_shutting_down(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

/// Everything one connection needs, shared across tasks.
struct FrontEnd {
    /// Shared without a lock of its own: the dispatcher and its backends
    /// synchronise internally, per backend slot, so requests to different
    /// workers genuinely run at the same time.
    dispatcher: Arc<Dispatcher>,
    /// One prebuilt routing per site, by site name, plus the default under
    /// `"<default>"`.
    ///
    /// Prebuilt, not derived per request: a `Routing` is a document root and a front
    /// controller, and rebuilding one for every request would put a `PathBuf` clone on
    /// the hot path to answer a question the configuration already answered at startup.
    routings: Arc<BTreeMap<String, Routing>>,
    /// The static layer per site, for the same reason.
    statics: Arc<BTreeMap<String, StaticFiles>>,
    /// The site table, to turn a `Host` header into one of the entries above.
    sites: Arc<Sites>,
    limits: HttpLimits,
    stats: Arc<HttpStats>,
    shutdown: Arc<AtomicBool>,
    /// The port this listener is bound to, for `SERVER_PORT` when the client
    /// sends no `Host`.
    server_port: u16,
}

impl FrontEnd {
    /// The routing and static layer for one site.
    ///
    /// A site resolved by [`Sites`] is always in both maps — they are built from the
    /// same table in the same pass — so a missing entry would mean the two had
    /// diverged, and falling back to the default is the safe direction: it serves the
    /// deployment's own root rather than an empty path.
    fn for_site(&self, site: &ResolvedSite) -> (&Routing, &StaticFiles) {
        let routing = self
            .routings
            .get(&site.name)
            .or_else(|| self.routings.get(DEFAULT_SITE_KEY))
            .expect("the default site is always built");
        let statics = self
            .statics
            .get(&site.name)
            .or_else(|| self.statics.get(DEFAULT_SITE_KEY))
            .expect("the default site is always built");
        (routing, statics)
    }
}

/// The key the default site's routing is stored under.
///
/// A name that cannot collide with a site's own, because a site name comes from a TOML
/// table key and `<>` is not valid in a bare TOML key.
pub const DEFAULT_SITE_KEY: &str = "<default>";

/// A running HTTP listener.
pub struct HttpListener {
    listener: tokio::net::TcpListener,
    front_end: Arc<FrontEnd>,
    /// TLS, when `[server.tls]` is configured. `None` is a plaintext listener, and that is
    /// the whole of the cost when TLS is off: no accept wrapping, no peek, no TLS type built.
    tls: Option<Arc<tokio_rustls::TlsAcceptor>>,
    /// Also answer cleartext on the TLS port (`[server.tls].plain`).
    ///
    /// Only ever true with `tls` set. The first byte decides which of the two paths a
    /// connection takes, which is what nginx's `ssl_preread` does — a TLS record always begins
    /// `0x16`, a plaintext request begins an ASCII method letter.
    accept_plain: bool,
}

/// A running HTTP listener on a dedicated runtime thread.
///
/// This is how a blocking caller — `stokehold serve`, whose FastCGI listener owns
/// the main thread — runs an HTTP front-end: the runtime is created here, lives on
/// a thread that exists only for it, and is torn down by [`RunningHttp::stop`].
/// The alternative would be an async `main`, which drags the whole program into
/// the runtime instead of leaving it at this boundary.
///
/// The runtime is deliberately *not* stored: it is moved onto the listener's
/// thread and dropped there, which is what the runtime requires. The handle kept
/// here is the shutdown flag and the thread, which is all a caller needs.
pub struct RunningHttp {
    shutdown: HttpShutdown,
    thread: Option<std::thread::JoinHandle<()>>,
    address: SocketAddr,
    stats: Arc<HttpStats>,
}

impl RunningHttp {
    /// Bind a listener and start it on a dedicated runtime thread.
    ///
    /// Returns once the listener is bound and the thread is accepting, so a port
    /// conflict is reported to the caller rather than to a thread.
    pub fn start(
        address: &str,
        dispatcher: Arc<Dispatcher>,
        sites: Arc<Sites>,
        limits: HttpLimits,
    ) -> Result<Self> {
        Self::start_with_tls(address, dispatcher, sites, limits, None)
    }

    /// [`RunningHttp::start`], with TLS.
    pub fn start_with_tls(
        address: &str,
        dispatcher: Arc<Dispatcher>,
        sites: Arc<Sites>,
        limits: HttpLimits,
        tls: Option<TlsSettings<'_>>,
    ) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(HttpError::Runtime)?;

        let listener = runtime.block_on(HttpListener::bind_with_tls(
            address, dispatcher, sites, limits, tls,
        ))?;
        let bound = listener.local_addr()?;
        let shutdown = listener.shutdown_handle();
        let stats = listener.stats();

        let thread = std::thread::spawn(move || {
            // The runtime is owned by this thread and dropped when the listener
            // stops, which is the order `tokio` requires.
            if let Err(err) = runtime.block_on(listener.run()) {
                tracing::warn!(%err, "http listener stopped with an error");
            }
        });

        Ok(RunningHttp {
            shutdown,
            thread: Some(thread),
            address: bound,
            stats,
        })
    }

    /// The address actually bound.
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    /// Statistics, for logging at shutdown.
    pub fn stats(&self) -> Arc<HttpStats> {
        Arc::clone(&self.stats)
    }

    /// Stop the listener and wait for its thread to finish.
    ///
    /// Waiting matters: the dispatcher lives inside the listener, and the worker
    /// pool reaps its processes when it is dropped. Returning before that would
    /// leave PHP processes behind.
    pub fn stop(self) {
        drop(self);
    }

    fn stop_inner(&mut self) {
        self.shutdown.shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for RunningHttp {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

impl HttpListener {
    /// Bind a listener.
    ///
    /// An async function because a `tokio` socket can only be created inside a
    /// runtime — which is the visible tip of the boundary this crate exists to
    /// draw. Binding happens before `run` so a port conflict is reported before
    /// any backend is reached.
    pub async fn bind(
        address: &str,
        dispatcher: Arc<Dispatcher>,
        sites: Arc<Sites>,
        limits: HttpLimits,
    ) -> Result<Self> {
        Self::bind_with_tls(address, dispatcher, sites, limits, None).await
    }

    /// [`HttpListener::bind`], with the TLS settings and the directory certificate paths
    /// resolve against.
    ///
    /// The resolver is built here rather than per connection, and a certificate that cannot
    /// load fails this call — before the port is bound — so a misconfigured deployment is
    /// reported once rather than as a listener that accepts and then fails every handshake.
    pub async fn bind_with_tls(
        address: &str,
        dispatcher: Arc<Dispatcher>,
        sites: Arc<Sites>,
        limits: HttpLimits,
        tls: Option<TlsSettings<'_>>,
    ) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .map_err(|source| HttpError::Bind {
                address: address.to_string(),
                source,
            })?;
        let bound = listener.local_addr().map_err(|source| HttpError::Bind {
            address: address.to_string(),
            source,
        })?;

        // The acceptor is built before anything is served, and its failure is reported as a
        // configuration error: a certificate that cannot be read must not become a listener.
        let (acceptor, accept_plain) = match tls {
            Some(settings) => {
                let resolver = Arc::new(
                    crate::tls::SniResolver::from_config(settings.config, settings.config_dir)
                        .map_err(|source| HttpError::Tls {
                            reason: source.to_string(),
                        })?,
                );
                let server = crate::tls::server_config(
                    settings.config,
                    settings.config_dir,
                    Arc::clone(&resolver),
                )
                .map_err(|source| HttpError::Tls {
                    reason: source.to_string(),
                })?;
                tracing::info!(
                    named_certificates = resolver.named_certificates(),
                    alpn = ?server.alpn_protocols.iter()
                        .map(|p| String::from_utf8_lossy(p).into_owned())
                        .collect::<Vec<_>>(),
                    plaintext_accepted = settings.config.server.tls.as_ref()
                        .is_some_and(|t| t.plain),
                    "TLS enabled"
                );
                (
                    Some(Arc::new(tokio_rustls::TlsAcceptor::from(server))),
                    settings.config.server.tls.as_ref().is_some_and(|t| t.plain),
                )
            }
            None => (None, false),
        };

        // The bound port, which matters when port 0 was requested. The port is needed
        // here rather than by the caller because only the listener knows what it got.
        let port = bound.port();
        let mut routings = BTreeMap::new();
        let mut statics = BTreeMap::new();
        for site in sites
            .named()
            .values()
            .chain(std::iter::once(sites.default_site()))
        {
            routings.insert(
                site.name.clone(),
                Routing {
                    document_root: site.document_root.clone(),
                    front_controller: site.front_controller.clone(),
                    ..Routing::default()
                },
            );
            statics.insert(
                site.name.clone(),
                StaticFiles {
                    config: site.static_files.clone(),
                    document_root: site.document_root.clone(),
                    server_port: port,
                },
            );
        }

        Ok(HttpListener {
            listener,
            front_end: Arc::new(FrontEnd {
                dispatcher,
                routings: Arc::new(routings),
                statics: Arc::new(statics),
                sites,
                limits,
                stats: Arc::new(HttpStats::default()),
                shutdown: Arc::new(AtomicBool::new(false)),
                server_port: port,
            }),
            tls: acceptor,
            accept_plain,
        })
    }

    /// The address actually bound, which matters when port 0 was requested.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.listener
            .local_addr()
            .map_err(|source| HttpError::Bind {
                address: "<listener>".to_string(),
                source,
            })
    }

    /// A handle that stops the listener from another thread.
    pub fn shutdown_handle(&self) -> HttpShutdown {
        HttpShutdown {
            flag: Arc::clone(&self.front_end.shutdown),
        }
    }

    /// Statistics, for logging.
    pub fn stats(&self) -> Arc<HttpStats> {
        Arc::clone(&self.front_end.stats)
    }

    /// Accept and serve connections until the shutdown handle is used.
    ///
    /// A connection is a task rather than a thread, which is the whole reason
    /// this front-end is async: thousands of mostly-idle connections share a
    /// handful of threads, where the FastCGI listener's thread-per-connection
    /// model costs a kernel stack each. The number of connections that can be
    /// *doing work* is still the number of backend slots — async does not change
    /// that, and does not pretend to.
    pub async fn run(self) -> Result<()> {
        let HttpListener {
            listener,
            front_end,
            tls,
            accept_plain,
        } = self;

        while !front_end.shutdown.load(Ordering::Relaxed) {
            match tokio::time::timeout(ACCEPT_POLL, listener.accept()).await {
                Ok(Ok((stream, peer))) => {
                    let active = front_end.stats.connections.fetch_add(1, Ordering::Relaxed) + 1;
                    if active > front_end.limits.max_connections {
                        front_end.stats.rejected.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(%peer, active, "refusing an HTTP connection: at the limit");
                        drop(stream);
                        continue;
                    }

                    // Nagle's algorithm is disabled on the raw socket, which is the one place
                    // it can be: the stream is wrapped by TLS from here on, and a `TlsStream`
                    // has no `set_nodelay`.
                    let _ = stream.set_nodelay(true);
                    let front_end = Arc::clone(&front_end);
                    let tls = tls.clone();
                    tokio::spawn(async move {
                        serve_accepted(stream, tls, accept_plain, front_end.clone(), peer).await;
                        front_end.stats.connections.fetch_sub(1, Ordering::Relaxed);
                    });
                }
                Ok(Err(source)) => {
                    // A transient accept failure must not stop the listener,
                    // which is what the FastCGI listener does too.
                    tracing::warn!(%source, "http accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                // The poll elapsed with no connection: loop so the shutdown flag
                // is read again.
                Err(_elapsed) => {}
            }
        }

        let (connections, requests, rejected, failed) = front_end.stats.snapshot();
        tracing::info!(
            connections,
            requests,
            rejected,
            failed,
            "http listener stopped"
        );
        Ok(())
    }
}

/// The first byte of a TLS record header: `ContentType: handshake`.
///
/// A plaintext HTTP request cannot begin with this byte — it begins with an ASCII method letter
/// (`G` = `0x47`, `P` = `0x50`, …) — so one byte separates the two protocols unambiguously.
/// This is the same test nginx's `ssl_preread` applies.
const TLS_HANDSHAKE_BYTE: u8 = 0x16;

/// Decide the protocol for one accepted connection, then serve it.
///
/// With no TLS configured this is `serve_connection` on the raw stream — no peek, no branch,
/// no TLS value constructed, which is the whole of the claim that TLS costs nothing when it is
/// off.
///
/// With TLS configured and `plain` off, every connection is a TLS handshake: a client that
/// sends cleartext gets a handshake failure, which is what a TLS-only port should do.
///
/// With `plain` on, the first byte decides. That is a **detection, not a downgrade**: a
/// connection goes down one path or the other, and a client that offers TLS and fails the
/// handshake still fails. There is no retry as cleartext.
async fn serve_accepted(
    stream: tokio::net::TcpStream,
    tls: Option<Arc<tokio_rustls::TlsAcceptor>>,
    accept_plain: bool,
    front_end: Arc<FrontEnd>,
    peer: SocketAddr,
) {
    let Some(acceptor) = tls else {
        if let Err(err) = serve_connection(stream, front_end, peer).await {
            tracing::debug!(%peer, %err, "http connection ended");
        }
        return;
    };

    if accept_plain && !starts_with_tls_handshake(&stream).await {
        front_end
            .stats
            .plaintext_on_tls_port
            .fetch_add(1, Ordering::Relaxed);
        if let Err(err) = serve_connection(stream, front_end, peer).await {
            tracing::debug!(%peer, %err, "http connection ended");
        }
        return;
    }

    match acceptor.accept(stream).await {
        Ok(tls_stream) => {
            front_end
                .stats
                .handshakes
                .fetch_add(1, Ordering::Relaxed);
            if let Err(err) = serve_connection(tls_stream, front_end, peer).await {
                tracing::debug!(%peer, %err, "https connection ended");
            }
        }
        // A failed handshake is not an error worth propagating: it is a client that could not
        // negotiate, and it is counted so a port that nothing can connect to is visible rather
        // than silent.
        Err(err) => {
            front_end
                .stats
                .handshake_failures
                .fetch_add(1, Ordering::Relaxed);
            tracing::debug!(%peer, %err, "tls handshake failed");
        }
    }
}

/// Whether a connection begins with a TLS handshake.
///
/// `peek` reads without consuming, so the bytes are still there for whichever path runs next.
/// A read that fails or returns nothing is *not* a TLS handshake: there is no request to serve
/// either way, and treating a dead connection as TLS would only turn a clean close into a
/// handshake error.
async fn starts_with_tls_handshake(stream: &tokio::net::TcpStream) -> bool {
    let mut first = [0u8; 1];
    matches!(stream.peek(&mut first).await, Ok(1) if first[0] == TLS_HANDSHAKE_BYTE)
}

/// Serve every request on one connection until it closes.
///
/// Generic over the stream so that one code path serves both protocols. The TLS handshake is
/// the caller's business: by the time a stream arrives here it is already decrypted (or was
/// never encrypted), and everything below — the request loop, dispatch, site resolution, the
/// static layer — is identical. `hyper`'s auto-detection still picks HTTP/2 from the ALPN
/// result, which it reads off the `TlsStream` through the `TokioIo` wrapper.
async fn serve_connection<S>(
    stream: S,
    front_end: Arc<FrontEnd>,
    peer: SocketAddr,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let remote_addr = peer.ip().to_string();
    let service = service_fn(move |request: Request<Incoming>| {
        let front_end = Arc::clone(&front_end);
        let remote_addr = remote_addr.clone();
        // Infallible because a failure is turned into an error page, not
        // propagated: a 502 the client can read beats a dropped connection.
        async move { Ok::<_, Infallible>(handle(request, front_end, remote_addr).await) }
    });

    let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
    // A connection that sends a partial request and stops must not hold a slot
    // for ever. This is the timeout `hyper` offers for HTTP/1; a fully idle
    // keep-alive connection is bounded by `max_connections` instead, because
    // `hyper` has no per-connection idle timeout and inventing one would mean
    // wrapping the IO stream rather than configuring the server.
    //
    // The timer is not decoration: once any deadline is configured `hyper` panics
    // with "no timer" rather than silently not applying it, so the timer is what
    // makes the timeout real.
    builder
        .http1()
        .timer(TokioTimer::new())
        .keep_alive(true)
        .header_read_timeout(Some(KEEPALIVE_TIMEOUT));

    builder
        .serve_connection(TokioIo::new(stream), service)
        .await
        .map_err(|source| HttpError::BadGateway(format!("connection failed: {source}")))
}

/// Serve one request, mapping any failure to an error page.
async fn handle(
    request: Request<Incoming>,
    front_end: Arc<FrontEnd>,
    remote_addr: String,
) -> Response<ResponseBody> {
    match prepare(request, &front_end, remote_addr).await {
        Ok(response) => {
            front_end.stats.requests.fetch_add(1, Ordering::Relaxed);
            response
        }
        Err(err) => {
            front_end.stats.failed.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(status = err.status(), %err, "http request failed");
            error_response(&err)
        }
    }
}

/// Read the body, build the CGI environment, dispatch, and translate the answer.
async fn prepare(
    request: Request<Incoming>,
    front_end: &Arc<FrontEnd>,
    remote_addr: String,
) -> Result<Response<ResponseBody>> {
    let (parts, body) = request.into_parts();

    // The site is resolved first, because everything after this is per site: which
    // document root a path resolves under, which front controller serves it, and which
    // static layer answers it. A map probe on the runtime thread, not work.
    let host = parts
        .headers
        .get(HOST)
        .and_then(|value| value.to_str().ok());
    let site = front_end.sites.resolve(host);
    let (routing, statics) = front_end.for_site(site);

    // Static content is decided before the body is read: a `GET` for a file has
    // no body worth buffering, and refusing to buffer one it will never use is
    // the point of a limit.
    //
    // `serve_only` decides whether this happens at all. A site that serves only a
    // backend skips it, which is the difference between "static files first" and "no
    // static layer": the first still stats on every request to discover there is no
    // file, and for an application host there never is one.
    if site.serve_only.serves_files() {
        let static_request = static_files::Request {
            method: &parts.method,
            path: parts.uri.path(),
            headers: &parts.headers,
            host,
        };
        if let Some(answer) = static_files::respond(static_request, statics).await {
            return answer;
        }
    }

    // A site that serves only files has no backend to fall through to, and the honest
    // answer for a path naming no file is then 404 — not a dispatch, and not a 500 from
    // a backend the deployment never configured.
    if !site.serve_only.serves_backend() {
        return Err(HttpError::NotFound(parts.uri.path().to_string()));
    }

    let body = read_body(body, front_end.limits.max_body_bytes).await?;

    // A route that proves its pattern names a script may skip the router's own `stat`:
    // `php-cgi` checks existence itself, and the extension guard is already satisfied by
    // the pattern. See `RouteConfig::skip_stat` for why that is a static property of the
    // configuration rather than something to decide per request.
    let route = {
        let path = parts.uri.path();
        stokehold_core::config::select_route(&site.routes, host, path)
    };
    let skips_stat = route.is_some_and(|route| {
        stokehold_core::config::skip_stat(route, front_end.dispatcher.is_worker(&route.backend))
    });

    // Everything from here on blocks: routing stats the filesystem to resolve
    // SCRIPT_FILENAME, and the backend reads and writes pipes. Both must happen
    // off the runtime's threads.
    let front_end = Arc::clone(front_end);
    let routing = routing.clone();
    let blocking = tokio::task::spawn_blocking(move || {
        let cgi_request = if skips_stat {
            to_cgi_routed(&parts, body, &routing, front_end.server_port, &remote_addr)?
        } else {
            to_cgi(&parts, body, &routing, front_end.server_port, &remote_addr)?
        };
        front_end
            .dispatcher
            .dispatch(&cgi_request)
            .map_err(map_server_error)
    });

    let response = blocking
        .await
        .map_err(|err| HttpError::BadGateway(format!("the dispatch task failed: {err}")))??;

    if !response.stderr.is_empty() {
        // Where a developer looks first when PHP complains, exactly as in the
        // FastCGI front-end.
        tracing::debug!(
            stderr = %String::from_utf8_lossy(&response.stderr).trim_end(),
            "backend wrote to STDERR"
        );
    }

    from_cgi(response)
}

/// Read the request body, refusing anything over the limit.
///
/// The frames are counted as they arrive rather than checked afterwards, so an
/// oversized upload is refused without being buffered first.
async fn read_body(body: Incoming, limit: usize) -> Result<Vec<u8>> {
    let mut body = body;
    let mut out: Vec<u8> = Vec::new();

    while let Some(frame) = body.frame().await {
        let frame = frame
            .map_err(|err| HttpError::BadRequest(format!("reading the request body: {err}")))?;
        if let Some(data) = frame.data_ref() {
            if out.len().saturating_add(data.len()) > limit {
                return Err(HttpError::BodyTooLarge { limit });
            }
            out.extend_from_slice(data);
        }
    }

    Ok(out)
}

/// Build the CGI request a backend understands.
///
/// The request is expressed as the CGI environment plus a body, which is what
/// every backend in the configuration already speaks. Doing it here rather than
/// teaching each front-end its own dialect is what makes `dispatch` one path.
///
/// Public, with `Parts` in the signature, so that the allocation cost of translating one
/// request can be measured in a test binary of its own — a `#[global_allocator]` is
/// per-binary, and this is the only place the translation can be driven from outside.
#[doc(hidden)]
pub fn to_cgi(
    parts: &hyper::http::request::Parts,
    body: Vec<u8>,
    routing: &Routing,
    server_port: u16,
    remote_addr: &str,
) -> Result<stokehold_fcgi::Request> {
    to_cgi_with(parts, body, routing, server_port, remote_addr, false)
}

/// Build the CGI request for a path a route has already claimed.
///
/// Identical to [`to_cgi`] except that a path naming no script is handed to the backend
/// rather than answered 404 here — see [`cgi::route_or_dispatch`] for why that is safe.
/// Called only when a route's `skip_stat` says so, because skipping the check
/// unconditionally would send every unmatched path to a backend.
#[doc(hidden)]
pub fn to_cgi_routed(
    parts: &hyper::http::request::Parts,
    body: Vec<u8>,
    routing: &Routing,
    server_port: u16,
    remote_addr: &str,
) -> Result<stokehold_fcgi::Request> {
    to_cgi_with(parts, body, routing, server_port, remote_addr, true)
}

fn to_cgi_with(
    parts: &hyper::http::request::Parts,
    body: Vec<u8>,
    routing: &Routing,
    server_port: u16,
    remote_addr: &str,
    allow_no_script: bool,
) -> Result<stokehold_fcgi::Request> {
    let path = parts.uri.path();
    let target = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let query = parts.uri.query().unwrap_or("");

    // Resolved once, before building the environment, so a missing or forbidden path
    // can be told apart from a backend failure. `build_params` would report both as a
    // bad request, which is right for FastCGI and wrong for HTTP: a path that names no
    // script is a 404.
    //
    // The answer is then handed to `build_params_for`, which does not route again. Doing
    // it twice walked the path and called `is_file` on its candidates a second time on
    // every request, for a result already in hand.
    let route = match if allow_no_script {
        cgi::route_or_dispatch(path, routing)
    } else {
        cgi::route(path, routing)
    } {
        Route::NotFound => return Err(HttpError::NotFound(path.to_string())),
        Route::Forbidden => return Err(HttpError::Forbidden(path.to_string())),
        resolved => resolved,
    };

    let host = parts
        .headers
        .get(HOST)
        .and_then(|value| value.to_str().ok());
    let (server_name, server_port) = split_host(host, server_port);

    let body_len = body.len();
    let headers = parts
        .headers
        .iter()
        .filter(|(name, _)| !is_hop_by_hop(name.as_str()))
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();

    let info = RequestInfo {
        method: parts.method.as_str().to_string(),
        uri: target.to_string(),
        query_string: query.to_string(),
        protocol: protocol_name(parts.version).to_string(),
        headers,
        remote_addr: remote_addr.to_string(),
        server_name,
        server_port,
        body_len,
        // TLS is step 23; nothing can arrive here over HTTPS yet.
        https: false,
    };

    let params = cgi::build_params_for(&info, routing, &route).map_err(map_cgi_error)?;

    Ok(stokehold_fcgi::Request {
        params,
        body,
        // The front-end decides keep-alive from its own protocol, so the
        // backend is always asked to stay up.
        keep_conn: true,
    })
}

/// Turn a backend response into an HTTP response.
///
/// `Content-Length` is always set from the body that is actually sent, and an
/// application-supplied one is dropped. The two cannot both be right, and only
/// one of them can break framing: declaring more than is sent desynchronises the
/// connection, while declaring less is merely conservative. `HEAD` is handled by
/// `hyper`, which suppresses the body and keeps the length — which is exactly
/// what `HEAD` is for.
fn from_cgi(response: stokehold_fcgi::Response) -> Result<Response<ResponseBody>> {
    // One pass for status, headers and body. Asking for the headers and then the
    // body separately made each search the payload for the same blank line, and
    // `perf` attributed 13.6% of the server process's CPU to the pair.
    //
    // `hyper` needs `HeaderName`/`HeaderValue`, which own their bytes, so a copy is
    // unavoidable here — but only the headers `hyper` actually accepts are copied,
    // and only once. The parser itself hands back slices of `stdout` instead of a
    // `String` per header, so the wasted allocation happens nowhere at all.
    let (status, headers, body) = response.parse();
    let body = body.to_vec();

    let mut builder = Response::builder().status(status);
    for (name, value) in headers {
        // An unusable header from the application used to panic here, because
        // `Builder::header` panics on a name or value `hyper` rejects: the
        // connection was dropped with no response at all. Skipping is what a
        // gateway should do — the rest of the response is still sendable.
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        if is_hop_by_hop(name.as_str()) || name == CONTENT_LENGTH {
            continue;
        }
        let Ok(value) = HeaderValue::from_bytes(value.as_bytes()) else {
            continue;
        };
        builder = builder.header(name, value);
    }
    builder = builder.header(CONTENT_LENGTH, body.len());

    builder
        .body(body_of(Bytes::from(body)))
        .map_err(|err| HttpError::BadGateway(format!("the backend sent an unusable header: {err}")))
}

/// A plain-text error page for a failure that never reached the application.
fn error_response(err: &HttpError) -> Response<ResponseBody> {
    let body = format!("{}\n", err);
    Response::builder()
        .status(err.status())
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(CONTENT_LENGTH, body.len())
        .body(body_of(Bytes::from(body)))
        .expect("a plain-text error page is a valid response")
}

/// Which HTTP status a dispatch failure deserves.
///
/// The distinction a gateway exists to make: a backend that is unreachable is a
/// 502, one that is reachable but silent is a 504, and one that has no capacity
/// is a 503. Collapsing them into one status throws away the only information
/// the operator gets.
fn map_server_error(err: ServerError) -> HttpError {
    if err.is_timeout() {
        HttpError::GatewayTimeout(err.to_string())
    } else if err.is_busy() {
        HttpError::Overloaded
    } else {
        HttpError::BadGateway(err.to_string())
    }
}

/// A CGI-environment failure, in HTTP terms.
fn map_cgi_error(err: stokehold_fcgi::FcgiError) -> HttpError {
    match err {
        stokehold_fcgi::FcgiError::BadRequest(reason) => HttpError::BadRequest(reason),
        other => HttpError::BadGateway(other.to_string()),
    }
}

/// Split a `Host` header into a name and a port.
///
/// A `Host` without a port is the common case, and an IPv6 literal needs its
/// brackets stripped before the port can be found, so the naive `rsplit_once`
/// is not enough on its own.
fn split_host(host: Option<&str>, fallback_port: u16) -> (String, u16) {
    let Some(host) = host else {
        return ("localhost".to_string(), fallback_port);
    };
    let host = host.trim();
    if host.is_empty() {
        return ("localhost".to_string(), fallback_port);
    }

    if let Some(rest) = host.strip_prefix('[') {
        // `[::1]:8080` or `[::1]`
        if let Some((address, tail)) = rest.split_once(']') {
            let port = tail
                .strip_prefix(':')
                .and_then(|port| port.parse().ok())
                .unwrap_or(fallback_port);
            return (address.to_string(), port);
        }
        return (host.to_string(), fallback_port);
    }

    match host.rsplit_once(':') {
        Some((name, port)) if !name.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            (name.to_string(), port.parse().unwrap_or(fallback_port))
        }
        _ => (host.to_string(), fallback_port),
    }
}

/// The CGI name for an HTTP version.
fn protocol_name(version: hyper::Version) -> &'static str {
    match version {
        hyper::Version::HTTP_09 => "HTTP/0.9",
        hyper::Version::HTTP_10 => "HTTP/1.0",
        hyper::Version::HTTP_2 => "HTTP/2.0",
        hyper::Version::HTTP_3 => "HTTP/3.0",
        _ => "HTTP/1.1",
    }
}

/// Headers that belong to one hop and must not be forwarded.
///
/// `Host` is deliberately absent: it becomes `HTTP_HOST`, which is what PHP reads
/// and what the route table matches on. `Content-Type` is absent too — it becomes
/// `CONTENT_TYPE`, which `build_params` does.
fn is_hop_by_hop(name: &str) -> bool {
    const HOP_BY_HOP: [&str; 7] = [
        "connection",
        "keep-alive",
        "proxy-connection",
        "transfer-encoding",
        "te",
        "trailer",
        "upgrade",
    ];
    HOP_BY_HOP.iter().any(|hop| name.eq_ignore_ascii_case(hop))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn routing() -> Routing {
        Routing {
            document_root: PathBuf::from("."),
            front_controller: Some("index.php".to_string()),
            forbidden_extensions: vec![".htaccess".to_string()],
        }
    }

    /// `Parts` as a front-end would produce them, with no `FrontEnd` involved:
    /// none of these tests should start a backend.
    fn parts(method: &str, target: &str, host: Option<&str>) -> hyper::http::request::Parts {
        let mut builder = Request::builder().method(method).uri(target);
        if let Some(host) = host {
            builder = builder.header(HOST, host);
        }
        builder.body(()).expect("a valid request").into_parts().0
    }

    fn param<'a>(request: &'a stokehold_fcgi::Request, name: &str) -> &'a [u8] {
        request
            .params
            .iter()
            .find(|(key, _)| key.as_slice() == name.as_bytes())
            .map(|(_, value)| value.as_slice())
            .unwrap_or_else(|| panic!("{name} is missing from the environment"))
    }

    #[test]
    fn the_environment_carries_what_php_reads() {
        let request = to_cgi(
            &parts("GET", "/about?x=1", Some("example.test")),
            Vec::new(),
            &routing(),
            8080,
            "10.0.0.5",
        )
        .expect("a request that routes");

        assert_eq!(param(&request, "REQUEST_METHOD"), b"GET");
        assert_eq!(param(&request, "REQUEST_URI"), b"/about?x=1");
        assert_eq!(param(&request, "QUERY_STRING"), b"x=1");
        assert_eq!(param(&request, "SERVER_PROTOCOL"), b"HTTP/1.1");
        assert_eq!(param(&request, "REMOTE_ADDR"), b"10.0.0.5");
        assert_eq!(param(&request, "SERVER_NAME"), b"example.test");
        assert_eq!(param(&request, "SERVER_PORT"), b"8080");
        assert_eq!(param(&request, "HTTP_HOST"), b"example.test");
        assert_eq!(param(&request, "SCRIPT_NAME"), b"/index.php");
        assert_eq!(param(&request, "PATH_INFO"), b"/about");
        assert_eq!(param(&request, "GATEWAY_INTERFACE"), b"FastCGI/1.0");
        // The front controller is what serves an unrouted path, so PHP must be
        // told to run it rather than to 404 in the SAPI.
        assert_eq!(param(&request, "REDIRECT_STATUS"), b"200");
    }

    #[test]
    fn a_body_is_declared_to_the_backend() {
        let request = to_cgi(
            &parts("POST", "/submit", Some("example.test")),
            b"a=1&b=2".to_vec(),
            &routing(),
            80,
            "127.0.0.1",
        )
        .expect("a request that routes");

        // Without CONTENT_LENGTH PHP leaves php://input empty and never runs its
        // body parser, so this is the parameter that makes a POST work at all.
        assert_eq!(param(&request, "CONTENT_LENGTH"), b"7");
        assert_eq!(request.body, b"a=1&b=2");
    }

    #[test]
    fn a_request_without_a_host_falls_back_to_the_bound_port() {
        let request = to_cgi(
            &parts("GET", "/", None),
            Vec::new(),
            &routing(),
            9090,
            "127.0.0.1",
        )
        .expect("a request that routes");

        assert_eq!(param(&request, "SERVER_NAME"), b"localhost");
        assert_eq!(param(&request, "SERVER_PORT"), b"9090");
    }

    #[test]
    fn a_host_with_a_port_is_split_into_name_and_port() {
        assert_eq!(
            split_host(Some("example.test:8443"), 80),
            ("example.test".to_string(), 8443)
        );
        assert_eq!(
            split_host(Some("example.test"), 80),
            ("example.test".to_string(), 80)
        );
        assert_eq!(
            split_host(Some("[::1]:8443"), 80),
            ("::1".to_string(), 8443)
        );
        assert_eq!(split_host(Some("[::1]"), 80), ("::1".to_string(), 80));
        assert_eq!(split_host(None, 80), ("localhost".to_string(), 80));
        assert_eq!(split_host(Some(""), 80), ("localhost".to_string(), 80));
    }

    #[test]
    fn hop_by_hop_headers_are_not_forwarded() {
        let request = Request::builder()
            .method("POST")
            .uri("/submit")
            .header(HOST, "example.test")
            .header("Connection", "keep-alive")
            .header("Transfer-Encoding", "chunked")
            .header(CONTENT_TYPE, "application/json")
            .header("X-Custom", "yes")
            .body(())
            .expect("a valid request")
            .into_parts()
            .0;

        let built = to_cgi(&request, b"{}".to_vec(), &routing(), 80, "127.0.0.1")
            .expect("a request that routes");

        assert_eq!(param(&built, "HTTP_X_CUSTOM"), b"yes");
        // Content-Type keeps its CGI name; it is not a hop-by-hop header.
        assert_eq!(param(&built, "CONTENT_TYPE"), b"application/json");
        let names: Vec<&[u8]> = built.params.iter().map(|(key, _)| key.as_slice()).collect();
        assert!(
            !names.contains(&b"HTTP_CONNECTION".as_slice()),
            "Connection is hop-by-hop"
        );
        assert!(
            !names.contains(&b"HTTP_TRANSFER_ENCODING".as_slice()),
            "Transfer-Encoding is hop-by-hop"
        );
    }

    #[test]
    fn a_missing_script_is_reported_as_not_found() {
        let routing = Routing {
            document_root: PathBuf::from("a-directory-that-does-not-exist"),
            front_controller: None,
            forbidden_extensions: Vec::new(),
        };
        let err = to_cgi(
            &parts("GET", "/nope.php", Some("example.test")),
            Vec::new(),
            &routing,
            80,
            "127.0.0.1",
        )
        .expect_err("no script exists under that root");

        assert_eq!(err.status(), 404);
    }

    #[test]
    fn a_forbidden_path_is_reported_as_forbidden() {
        let routing = Routing {
            document_root: PathBuf::from("a-directory-that-does-not-exist"),
            front_controller: Some("index.php".to_string()),
            forbidden_extensions: vec![".htaccess".to_string()],
        };
        let err = to_cgi(
            &parts("GET", "/.htaccess", Some("example.test")),
            Vec::new(),
            &routing,
            80,
            "127.0.0.1",
        )
        .expect_err("the path names a forbidden extension");

        assert_eq!(err.status(), 403);
    }

    #[test]
    fn a_status_header_becomes_the_http_status_and_the_body_survives() {
        let response = stokehold_fcgi::Response {
            stdout: b"Status: 404 Not Found\r\nContent-Type: text/plain\r\n\r\nmissing\n".to_vec(),
            ..stokehold_fcgi::Response::default()
        };
        let built = from_cgi(response).expect("a well-formed response");

        assert_eq!(built.status(), 404);
        assert_eq!(
            built
                .headers()
                .get(CONTENT_TYPE)
                .expect("content type is kept"),
            "text/plain"
        );
    }

    #[test]
    fn the_declared_length_is_the_body_that_is_sent() {
        // An application that declares a length it does not send would
        // desynchronise the connection, so the front-end's own count wins.
        let response = stokehold_fcgi::Response {
            stdout: b"Content-Length: 9999\r\n\r\nshort".to_vec(),
            ..stokehold_fcgi::Response::default()
        };
        let built = from_cgi(response).expect("a well-formed response");

        assert_eq!(built.headers().get(CONTENT_LENGTH).expect("a length"), "5");
        assert_eq!(built.status(), 200, "no Status header means 200");
    }

    #[test]
    fn a_repeated_header_is_not_collapsed() {
        // `Set-Cookie` is the reason: two cookies are two headers, and folding
        // them into one corrupts both.
        let response = stokehold_fcgi::Response {
            stdout: b"Set-Cookie: a=1\r\nSet-Cookie: b=2\r\n\r\n".to_vec(),
            ..stokehold_fcgi::Response::default()
        };
        let built = from_cgi(response).expect("a well-formed response");

        let cookies: Vec<&str> = built
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|value| value.to_str().expect("valid UTF-8"))
            .collect();
        assert_eq!(cookies, vec!["a=1", "b=2"]);
    }

    #[test]
    fn a_backend_timeout_is_a_504_and_an_unreachable_backend_a_502() {
        let timeout = ServerError::WorkerProtocol {
            backend: "app".to_string(),
            source: stokehold_worker::WorkerError::Timeout(Duration::from_secs(30)),
        };
        assert_eq!(map_server_error(timeout).status(), 504);

        let unreachable = ServerError::Misconfigured {
            backend: "fpm".to_string(),
            reason: "cannot connect".to_string(),
        };
        assert_eq!(map_server_error(unreachable).status(), 502);

        let busy = ServerError::NoWorker {
            backend: "app".to_string(),
        };
        assert_eq!(map_server_error(busy).status(), 503);
    }

    #[test]
    fn an_error_page_names_its_own_status() {
        let response = error_response(&HttpError::BadGateway("worker died".to_string()));
        assert_eq!(response.status(), 502);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).expect("content type"),
            "text/plain; charset=utf-8"
        );
    }

    #[test]
    fn the_protocol_name_matches_the_cgi_spelling() {
        assert_eq!(protocol_name(hyper::Version::HTTP_10), "HTTP/1.0");
        assert_eq!(protocol_name(hyper::Version::HTTP_11), "HTTP/1.1");
        assert_eq!(protocol_name(hyper::Version::HTTP_2), "HTTP/2.0");
    }
}
