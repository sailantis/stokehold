//! The pooled client that reaches an origin, and the request/response pair it moves.
//!
//! # The pool is `hyper-util`'s, deliberately
//!
//! A hand-rolled pool was the alternative and it loses on one row: **upgrades**. `client-legacy`'s
//! `Poolable` hands the connection's parts back as an `UpgradeableConnection` once a handshake
//! succeeds, which is the mechanism a WebSocket proxy is built on — and a pool we own cannot give
//! the raw connection back, because it has already wrapped it in its own framing. Reconnecting to
//! upgrade is not equivalent. Keep-alive, HTTP/2 multiplexing, `Upgrade: h2c` and the idle timeout
//! come along with it for free.
//!
//! One plan hazard does **not** transfer here, and it is worth saying so: the FastCGI pool keeps
//! `MAX_IDLE_CONNECTIONS = 1` because `php-fpm` counts a parked connection as an *active child*, so
//! every idle connection permanently removes one worker from the set the master dispatches to. An
//! HTTP origin does not work that way — keep-alive is exactly what it expects — so the upstream pool
//! is allowed to be what `client-legacy` defaults to.
//!
//! # The connector is ours, and that is the only custom part
//!
//! `hyper-util`'s `HttpConnector` is hardcoded to TCP and refuses any scheme but `http`. The pool,
//! the state machines and the upgrade machinery are all still its own; a connector is only asked
//! "give me an IO for this URI", so it is the one seam where a unix socket and a TLS handshake fit
//! without a second copy of the rest.

mod connect;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::BodyExt;
use hyper::body::Body;
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

use crate::error::{Result, UpstreamError};
use crate::origin::Origin;

/// A request on its way to an origin.
///
/// The `http` crate's own types, which is not a coincidence worth hiding: an HTTP request *is* a
/// method, a URI, a version, headers and a body, and inventing a parallel representation of a type
/// already in the build would be the CGI flattening again in the other direction. `http` arrives
/// through `hyper`, so this adds no dependency.
///
/// The generic body is what makes the translation testable without a network: a test can drive
/// [`UpstreamRequest`] and [`translate_response`](crate::translate_response) with a body whose
/// frames arrive on a timer.
#[derive(Debug)]
pub struct UpstreamRequest<B> {
    /// The method, unchanged.
    pub method: hyper::Method,
    /// **Origin-form** — the path and query only. The origin is the backend's address, so it is not
    /// part of the request line; it is the pool's business instead.
    pub uri: hyper::Uri,
    /// The client's version as it arrived. `hyper` negotiates with the origin independently, so
    /// this is informational rather than the version that will be sent.
    pub version: hyper::Version,
    /// Multi-valued: `HeaderMap` is a multimap, so two headers with one name survive as two, which
    /// is the whole reason this path does not go through CGI.
    pub headers: hyper::HeaderMap,
    /// The body, still streaming.
    pub body: B,
}

/// What an origin answered.
#[derive(Debug)]
pub struct UpstreamResponse {
    pub status: hyper::StatusCode,
    pub headers: hyper::HeaderMap,
    /// **Not `BoxBody`.** `hyper`'s server bounds the response body as `B: Body + 'static` with no
    /// `Send` or `Sync`, while `hyper::body::Incoming` is not `Sync` — it can wrap an
    /// `h2::RecvStream`. `BoxBody` requires both, so boxing an origin's body into it does not
    /// compile, which makes this a one-word change with a verified reason rather than a preference.
    pub body: UpstreamBody,
    /// The version the origin answered with, not necessarily the one that was sent.
    pub version: hyper::Version,
}

/// The body type an origin's answer is boxed into.
///
/// The error is `io::Error` rather than `hyper::Error` so this can be handed to the same response
/// type the static-file and CGI paths use; `Incoming`'s error is mapped at the boundary, where the
/// only question anyone asks about it is whether the connection failed.
pub type UpstreamBody = UnsyncBoxBody<Bytes, std::io::Error>;

/// A pooled client for one origin.
///
/// Cloneable and cheap to clone: the connection pool, the connector and the configuration are all
/// behind `Arc` inside `client-legacy`'s `Client`, so a clone shares the pool rather than making a
/// second one.
#[derive(Debug)]
pub struct UpstreamClient<B> {
    origin: Arc<Origin>,
    client: Client<connect::Connector, B>,
    idle_timeout: Duration,
}

impl<B> Clone for UpstreamClient<B> {
    fn clone(&self) -> Self {
        Self {
            origin: self.origin.clone(),
            client: self.client.clone(),
            idle_timeout: self.idle_timeout,
        }
    }
}

/// How long an idle pooled connection is kept when the caller names no timeout.
///
/// The same ninety seconds `client-legacy` defaults to, restated here so the number is visible in
/// this codebase rather than implied by a dependency.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

impl<B> UpstreamClient<B>
where
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    /// Build a client for an origin.
    ///
    /// `idle_timeout` of `None` disables pooling altogether — every request dials. That is the
    /// right shape for an origin that closes connections itself and is confused by reuse, and it is
    /// offered because the alternative would be an operator discovering it by experiment.
    pub fn new(origin: Origin, idle_timeout: Option<Duration>) -> Result<Self> {
        let connector = connect::Connector::new(&origin)?;
        let mut builder = Client::builder(TokioExecutor::new());
        builder.pool_timer(hyper_util::rt::TokioTimer::new());
        // The `Host` header is kept explicitly rather than re-derived (see
        // `translate::prepare_request`): the origin is addressed separately from the name the
        // client asked for, and only the latter is semantic to the origin.
        builder.set_host(false);
        match idle_timeout {
            Some(idle_timeout) => {
                builder.pool_idle_timeout(idle_timeout);
            }
            None => {
                builder.pool_max_idle_per_host(0);
            }
        }
        let client = builder.build::<connect::Connector, B>(connector);
        Ok(Self {
            origin: Arc::new(origin),
            client,
            idle_timeout: idle_timeout.unwrap_or(Duration::ZERO),
        })
    }
}

impl<B> UpstreamClient<B> {
    /// The configured origin.
    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    /// How long an idle connection is kept. Zero when pooling is off.
    pub fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }
}

impl<B> UpstreamClient<B>
where
    B: Body<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    /// Send one request and hand back the origin's answer unread.
    ///
    /// The body of the answer is **not** read here: a caller that streams hands the `Incoming` on,
    /// and one that buffers collects it itself. Reading it here would decide that for both.
    pub async fn request(&self, request: UpstreamRequest<B>) -> Result<UpstreamResponse> {
        let uri = destination(&self.origin, &request.uri)?;
        let mut outgoing = Request::builder()
            .method(request.method)
            .uri(uri)
            .version(request.version);
        if let Some(headers) = outgoing.headers_mut() {
            *headers = request.headers;
        }
        let outgoing = outgoing
            .body(request.body)
            .map_err(|source| UpstreamError::BadRequest(source.to_string()))?;

        let response = self.client.request(outgoing).await?;
        let status = response.status();
        let version = response.version();
        let headers = response.headers().clone();
        Ok(UpstreamResponse {
            status,
            headers,
            body: box_body(response.into_body()),
            version,
        })
    }
}

/// Box an origin's body into the response body type.
///
/// `Incoming` carries `hyper::Error`, which the response type cannot name without making every
/// other response body depend on `hyper`. The mapping is lossless for the only thing a caller can
/// act on — `io::Error::other` keeps the display text and the source.
fn box_body(body: hyper::body::Incoming) -> UpstreamBody {
    BodyExt::map_err(body, std::io::Error::other).boxed_unsync()
}

/// Name the destination to the pool.
///
/// `client-legacy` derives both the pool key and the dialled address from this URI's scheme and
/// authority, and its own `HttpConnector` refuses a scheme it does not know — so the scheme here
/// has to be the origin's own. `client-legacy` rewrites the URI to origin-form before it goes on
/// the wire, so an absolute-form request is never actually sent.
///
/// A `unix:` origin has no authority to name, and the connector ignores the URI entirely, so the
/// placeholder is honest rather than load-bearing. The client's own `Host` header is what the
/// origin reads in that case.
fn destination(origin: &Origin, path_and_query: &hyper::Uri) -> Result<hyper::Uri> {
    let path = path_and_query
        .path_and_query()
        .map(|path| path.as_str())
        .unwrap_or("/");
    let path = if path.is_empty() { "/" } else { path };

    let builder = hyper::http::uri::Builder::new();
    // A socket path is refused at parse time on every platform that cannot dial one, so the
    // fallback is only reachable on unix — where it is a placeholder the connector ignores.
    let builder = builder.scheme(origin.scheme().unwrap_or("unix"));
    let builder = match origin.authority() {
        Some(authority) => builder.authority(authority.clone()),
        // Nothing dials this; the connector has already been given the socket path.
        None => builder.authority("localhost"),
    };
    builder.path_and_query(path).build().map_err(|source| {
        UpstreamError::BadRequest(format!("{path:?} cannot be sent to an origin: {source}"))
    })
}

/// Box an inbound request body for the client, without reading any of it.
///
/// `Incoming` carries `hyper::Error`, and the client wants one body type for every request so that
/// the pool and the protocol state machines stay non-generic — so the error is mapped, which for a
/// caller is the same question it always asks about a body: did the connection fail.
///
/// Deliberately **not** `BodyExt::boxed`: that produces `BoxBody<Bytes, hyper::Error>`, a different
/// type from [`UpstreamBody`], and `BoxBody` requires `Sync` — which the body a `h2` connection
/// hands out is not.
pub fn incoming_body(body: hyper::body::Incoming) -> UpstreamBody {
    BodyExt::map_err(body, std::io::Error::other).boxed_unsync()
}

/// Read a whole body into memory.
///
/// Offered for tests and for a caller that has decided to buffer: it is *not* what the streaming
/// path uses, and the difference is what this step's streaming assertions are about.
pub async fn collect(body: UpstreamBody) -> Result<Bytes> {
    Ok(body
        .collect()
        .await
        .map_err(|source| UpstreamError::Unreachable(source.to_string()))?
        .to_bytes())
}

/// Whether a header name must not be forwarded across a hop.
///
/// The RFC 9110 §7.6.1 set, plus `Proxy-Authenticate` and `Proxy-Authorization`, which the same
/// section calls hop-by-hop in practice. `Host` is deliberately absent: it is the vhost identity the
/// origin needs. `Content-Length` and `Transfer-Encoding` are absent too — `hyper` re-derives
/// framing from the body, and forwarding a stale length is how a proxy desynchronises a connection.
pub fn is_hop_by_hop(name: &str) -> bool {
    const HOP_BY_HOP: [&str; 9] = [
        "connection",
        "keep-alive",
        "proxy-connection",
        "proxy-authenticate",
        "proxy-authorization",
        "transfer-encoding",
        "te",
        "trailer",
        "upgrade",
    ];
    HOP_BY_HOP.iter().any(|hop| name.eq_ignore_ascii_case(hop))
}

/// The header names a `Connection:` value says are hop-by-hop *for this message*.
///
/// The half of RFC 9110 §7.6.1 that is easy to skip and expensive to get wrong: `Connection:
/// X-Private` means `X-Private` is hop-local, so forwarding it leaks whatever the previous hop
/// wanted kept between itself and us.
pub fn connection_named(headers: &hyper::HeaderMap) -> Vec<hyper::header::HeaderName> {
    let mut named = Vec::new();
    for value in headers.get_all(hyper::header::CONNECTION).iter() {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for token in value.split(',') {
            let token = token.trim();
            // `Connection: close` names a directive, not a header.
            if token.is_empty() || token.eq_ignore_ascii_case("close") {
                continue;
            }
            if let Ok(name) = hyper::header::HeaderName::from_bytes(token.as_bytes()) {
                named.push(name);
            }
        }
    }
    named
}

/// The body a streamed inbound request carries, re-exported so a caller can name it without
/// depending on `hyper` directly.
pub type IncomingBody = hyper::body::Incoming;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ipv6_origin_keeps_its_brackets_in_the_destination() {
        // `Uri`'s authority renders `[::1]:3000`, and a client that stripped the brackets would
        // send a request line naming a malformed host.
        let origin = Origin::parse("http://[::1]:3000").expect("parses");
        let uri = destination(&origin, &hyper::Uri::from_static("/api")).expect("builds");
        assert_eq!(uri.to_string(), "http://[::1]:3000/api");
    }

    #[test]
    fn the_query_survives_the_destination() {
        let origin = Origin::parse("http://127.0.0.1:3000").expect("parses");
        let uri = destination(&origin, &hyper::Uri::from_static("/api?x=1&y=2")).expect("builds");
        assert_eq!(uri.to_string(), "http://127.0.0.1:3000/api?x=1&y=2");
    }

    #[test]
    fn a_trailing_slash_is_preserved_rather_than_normalised() {
        // `/api/` and `/api` are different routes to many frameworks, so normalising one into the
        // other here would silently change which handler the origin picks.
        let origin = Origin::parse("http://127.0.0.1:3000").expect("parses");
        let uri = destination(&origin, &hyper::Uri::from_static("/api/")).expect("builds");
        assert_eq!(uri.to_string(), "http://127.0.0.1:3000/api/");
    }

    #[test]
    fn pooling_off_reports_no_idle_timeout() {
        // The assertion that makes `None` mean "do not reuse" rather than "use the default": a
        // caller reading `idle_timeout()` must not see ninety seconds when it asked for none.
        let origin = Origin::parse("http://127.0.0.1:3000").expect("parses");
        let client: UpstreamClient<http_body_util::Empty<Bytes>> =
            UpstreamClient::new(origin, None).expect("builds");
        assert_eq!(client.idle_timeout(), Duration::ZERO);
    }
}
