//! The connector: how a URI becomes a connection.
//!
//! `hyper-util` ships one, and it does exactly one of the four shapes an origin can take. It is
//! hardcoded to TCP and, more decisively, **refuses any scheme that is not `http`** — `enforce_http`
//! defaults to `true` — so it cannot complete a TLS handshake and cannot reach a unix socket. That
//! is not a limitation to work around with a second client; it is the seam where a hand-written
//! `Service<Uri>` fits, because everything expensive — the pool, the HTTP/1.1 and HTTP/2 state
//! machines, the upgrade machinery — lives above this and none of it is reimplemented here.
//!
//! What the contract actually requires, read off `hyper-util`'s own `Connect` blanket impl:
//!
//! * `Service<Uri>` with `Response = T`, `T: Read + Write + Connection + Unpin + Send + 'static`
//! * `Error: Into<Box<dyn Error + Send + Sync>>`
//! * `Future: Unpin + Send`
//!
//! The `Unpin` bound is why the future is boxed: a future holding `async` blocks that borrow a
//! `TcpStream` and then a `TlsStream` is not `Unpin` on its own, and this project's dependency set
//! has no `pin-project` to hand. One boxed future per *new connection* — not per request, because
//! the pool reuses what it already has — is the honest price.
//!
//! # The `Connected` value is not bookkeeping
//!
//! `client-legacy` does `extra.set(res.extensions_mut())` from whatever the connector attached, and
//! `Connected::negotiated_h2()` is what tells the client to treat this connection as HTTP/2.
//! Without it, ALPN could negotiate `h2` on the wire and the client would still speak HTTP/1.1 over
//! it — a hang, not an error. So the ALPN result is read off the session after the handshake and
//! carried back.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use hyper::Uri;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::error::{Result, UpstreamError};
use crate::origin::{Origin, UpstreamTls};

/// How long a connection attempt may take, dial and handshake together.
///
/// The FastCGI path has the same shape and for the same reason: without it a black-holed origin
/// holds a request slot until the operating system gives up, which reads as "the server is down"
/// rather than "one origin is unreachable". Five seconds is the value the FastCGI callers already
/// pass to `Endpoint::connect`, so a deployment has one number to reason about.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Builds connections to one origin.
///
/// Cloned per connection attempt, so everything in it is `Arc` or `Copy`.
#[derive(Clone, Debug)]
pub struct Connector {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    origin: Origin,
    /// Resolved once, at startup: a CA path typo must not become a 502 on the first request.
    tls: Option<Arc<ClientConfig>>,
}

impl Connector {
    /// Prepare a connector for an origin, loading a CA if one was named.
    pub fn new(origin: &Origin) -> Result<Self> {
        let tls = match origin.tls() {
            Some(tls) => Some(client_config(origin, tls)?),
            None => None,
        };
        Ok(Self {
            inner: Arc::new(Inner {
                origin: origin.clone(),
                tls,
            }),
        })
    }
}

/// The rustls client configuration an `https:` origin is verified with.
fn client_config(origin: &Origin, tls: &UpstreamTls) -> Result<Arc<ClientConfig>> {
    // The provider must be installed before a `ClientConfig` exists. `volucer-http` does this for
    // the serving path, but this crate has to work when it is the only one linked — a unit test,
    // for instance. The install is idempotent and "already installed" is the state we want.
    install_crypto_provider();

    let mut roots = RootCertStore::empty();
    match &tls.ca {
        Some(path) => {
            let named = |reason: String| UpstreamError::Ca {
                path: path.display().to_string(),
                reason,
            };
            let file = std::fs::File::open(path).map_err(|source| named(source.to_string()))?;
            let mut reader = std::io::BufReader::new(file);
            let mut found = 0usize;
            for certificate in rustls_pemfile::certs(&mut reader) {
                let certificate = certificate.map_err(|source| named(source.to_string()))?;
                roots
                    .add(certificate)
                    .map_err(|source| named(source.to_string()))?;
                found += 1;
            }
            if found == 0 {
                return Err(named("the file contains no certificate".to_string()));
            }
        }
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }

    let mut config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    // `h2` first: an origin that can multiplex should, and one that cannot picks `http/1.1`. Both
    // are offered, because offering only `h2` makes an HTTP/1.1-only origin fail the handshake
    // instead of downgrading.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    // There is no `dangerous()` path here and no way to reach one: no verifier is ever installed,
    // so a chain that does not reach `roots` fails the handshake. Deliberate — see the crate docs.
    //
    // The verification *name* is checked here rather than at connect time: a typo in `sni` is a
    // configuration mistake, and a configuration mistake belongs at startup.
    if tls.sni.is_some() {
        hello_name(origin, tls)?;
    } else {
        let host = origin
            .authority()
            .map(|authority| authority.host().to_string())
            .unwrap_or_default();
        ServerName::try_from(host.clone()).map_err(|source| UpstreamError::InvalidOrigin {
            origin: host,
            reason: format!("the origin's host is not a name a certificate can carry: {source}"),
        })?;
    }
    Ok(Arc::new(config))
}

/// Install rustls' default crypto provider, once.
///
/// Deliberately similar to `volucer-http`'s equivalent rather than shared: this crate must not
/// depend on the listener, and the function is five lines of `OnceLock`. A second call with a
/// different provider is an error rustls reports, and this ignores it — the first install already
/// won, and re-reporting it would turn a working deployment into a failing one.
fn install_crypto_provider() {
    use std::sync::OnceLock;
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// An IO the pool can use, whatever it was built from.
///
/// Boxed rather than generic: a connector's `Response` type is fixed, and the three shapes here are
/// three different concrete types. Boxing costs one indirection per read and write on a path that
/// does a syscall per read and write anyway.
pub struct ConnIo(Box<dyn Io>);

trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Connection + Unpin + Send + 'static {}

impl<T> Io for T where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Connection + Unpin + Send + 'static
{
}

impl std::fmt::Debug for ConnIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConnIo")
    }
}

impl tokio::io::AsyncRead for ConnIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.0).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for ConnIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut *self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.0).poll_shutdown(cx)
    }
}

impl Connection for ConnIo {
    fn connected(&self) -> Connected {
        self.0.connected()
    }
}

impl tower_service::Service<Uri> for Connector {
    type Response = TokioIo<ConnIo>;
    type Error = UpstreamError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Uri) -> Self::Future {
        // Nothing is read off the URI: a connector belongs to exactly one origin, which is what
        // makes the TLS name and the socket path stable across every connection it makes. The pool
        // asks with the same URI every time (`client.rs`'s `domain_as_uri`), so there is nothing to
        // vary on anyway.
        let inner = self.inner.clone();
        Box::pin(async move {
            // `timeout` rather than `select!`: the same bound, without needing `tokio`'s `macros`
            // feature for a join of exactly two futures.
            match tokio::time::timeout(CONNECT_TIMEOUT, dial(&inner)).await {
                Ok(connected) => connected.map(TokioIo::new),
                Err(_) => Err(UpstreamError::Timeout),
            }
        })
    }
}

/// Dial the origin, handshaking when it is `https:`.
async fn dial(inner: &Inner) -> Result<ConnIo> {
    match &inner.origin {
        Origin::Network { base, tls: None } => {
            let stream = TcpStream::connect(host_port(base)).await?;
            // Set once per *connection*, not per request: the pool reuses this for many requests,
            // and a small request behind Nagle's algorithm is a latency bug.
            let _ = stream.set_nodelay(true);
            Ok(ConnIo(Box::new(stream)))
        }
        Origin::Network { base, tls: Some(_) } => {
            let config = inner
                .tls
                .clone()
                .expect("a TLS origin was built with a config");
            let stream = TcpStream::connect(host_port(base)).await?;
            let _ = stream.set_nodelay(true);

            let settings = inner.origin.tls().expect("a TLS origin has settings");
            let hello = hello_name(&inner.origin, settings)?;
            let stream = TlsConnector::from(config)
                .connect(hello, stream)
                .await
                .map_err(|source| UpstreamError::Unreachable(format!("TLS handshake: {source}")))?;

            // Read *after* the handshake: this is what makes an ALPN-negotiated HTTP/2 connection
            // actually be spoken as HTTP/2. Missing it is a hang rather than an error.
            let negotiated_h2 = {
                let (_, session) = stream.get_ref();
                session.alpn_protocol() == Some(b"h2".as_ref())
            };
            Ok(ConnIo(Box::new(TlsIo {
                stream,
                negotiated_h2,
            })))
        }
        #[cfg(unix)]
        Origin::Socket(path) => {
            let stream = tokio::net::UnixStream::connect(path).await?;
            Ok(ConnIo(Box::new(UnixIo(stream))))
        }
    }
}

fn host_port(base: &Uri) -> String {
    let host = base.host().unwrap_or("127.0.0.1");
    match base.port_u16() {
        Some(port) => format!("{host}:{port}"),
        None => {
            let port = if base.scheme_str() == Some("https") {
                443
            } else {
                80
            };
            format!("{host}:{port}")
        }
    }
}

/// The name the origin's certificate is verified against.
fn hello_name(origin: &Origin, tls: &UpstreamTls) -> Result<ServerName<'static>> {
    let name = match &tls.sni {
        Some(sni) => sni.clone(),
        None => origin
            .authority()
            .map(|authority| authority.host().to_string())
            .unwrap_or_default(),
    };
    if name.is_empty() {
        return Err(UpstreamError::InvalidOrigin {
            origin: origin
                .authority()
                .map(|authority| authority.as_str().to_string())
                .unwrap_or_default(),
            reason: "there is no name to verify the origin's certificate against".to_string(),
        });
    }
    ServerName::try_from(name.clone()).map_err(|source| UpstreamError::InvalidOrigin {
        origin: name,
        reason: format!("it is not a name a certificate can carry: {source}"),
    })
}

/// A TLS stream, remembering what ALPN agreed.
struct TlsIo {
    stream: tokio_rustls::client::TlsStream<TcpStream>,
    negotiated_h2: bool,
}

impl tokio::io::AsyncRead for TlsIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for TlsIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

impl Connection for TlsIo {
    fn connected(&self) -> Connected {
        let connected = Connected::new();
        if self.negotiated_h2 {
            connected.negotiated_h2()
        } else {
            connected
        }
    }
}

/// A unix socket, with the `Connected` value `hyper-util` gives that type itself.
#[cfg(unix)]
struct UnixIo(tokio::net::UnixStream);

#[cfg(unix)]
impl tokio::io::AsyncRead for UnixIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

#[cfg(unix)]
impl tokio::io::AsyncWrite for UnixIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

#[cfg(unix)]
impl Connection for UnixIo {
    fn connected(&self) -> Connected {
        // `hyper-util`'s own impl for `tokio::net::UnixStream` returns an empty `Connected`; this
        // is the same value, spelled out because the impl reached through `ConnIo` would not be.
        Connected::new()
    }
}
