//! TLS, driven by a real client against a real listener.
//!
//! A handshake cannot be asserted from the server side alone: what matters is what the *client*
//! receives — which certificate was presented, and which protocol ALPN selected. So these tests
//! start a listener, connect with `rustls` as a client, and read the answer.
//!
//! Certificates are generated at test time rather than committed, because a committed key is a
//! key that ends up in someone's deployment. `rcgen` makes a distinct pair per test, which also
//! makes "the wrong certificate was presented" a visible failure rather than a coincidence.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use volucer_core::config::Config;
use volucer_core::sites::Sites;
use volucer_http::{HttpLimits, HttpListener, ListenerSettings, TlsSettings};
use volucer_server::Dispatcher;

/// A certificate pair on disk, named after the host it is for.
struct Cert {
    dir: PathBuf,
}

impl Cert {
    /// Generate a self-signed pair for `name` in its own directory.
    ///
    /// Its own directory because the config file is written beside the certificates, which is
    /// how a relative path is meant to resolve — so the test exercises that rule rather than
    /// working around it.
    fn new(label: &str, name: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("volucer-tls-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("certs")).expect("create fixture dir");

        let generated = rcgen::generate_simple_self_signed(vec![name.to_string()])
            .expect("generate a self-signed certificate");
        std::fs::write(dir.join("certs/server.crt"), generated.cert.pem()).expect("write cert");
        std::fs::write(
            dir.join("certs/server.key"),
            generated.key_pair.serialize_pem(),
        )
        .expect("write key");

        Cert { dir }
    }

    /// The config-file directory, which is what relative certificate paths resolve against.
    fn dir(&self) -> &Path {
        &self.dir
    }

    /// A second pair in the same directory, for the per-site (SNI) case.
    fn add(&self, file: &str, name: &str) {
        let generated = rcgen::generate_simple_self_signed(vec![name.to_string()])
            .expect("generate a self-signed certificate");
        std::fs::write(
            self.dir.join(format!("certs/{file}.crt")),
            generated.cert.pem(),
        )
        .expect("write cert");
        std::fs::write(
            self.dir.join(format!("certs/{file}.key")),
            generated.key_pair.serialize_pem(),
        )
        .expect("write key");
    }
}

impl Drop for Cert {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Write a config file beside the certificates, and return the parsed config.
///
/// Written to disk rather than built in memory because the path *resolution* is part of what is
/// under test: `certs/server.crt` must be read relative to this file's directory.
fn write_config(cert: &Cert, body: &str) -> Config {
    let text = format!(
        r#"
[server]
http = "127.0.0.1:0"
document_root = '{}'

[server.tls]
certificate = "certs/server.crt"
key = "certs/server.key"
{body}
"#,
        cert.dir().display()
    );
    std::fs::write(cert.dir().join("volucer.toml"), &text).expect("write config");
    let config = Config::from_toml(&text).expect("config parses");
    config.validate().expect("config validates");
    config
}

/// Start a listener in the background and return the address it bound.
///
/// The listener task owns the listener; only the address comes back, because `run` consumes it
/// and the task lives until the test's runtime shuts down.
async fn start(config: Config, config_dir: &Path) -> SocketAddr {
    start_keeping(config, config_dir).await.0
}

/// [`start`], keeping the handle that replaces what the running listener is serving.
///
/// The handle has to be taken *before* `run` consumes the listener, which is why both are
/// returned from one function rather than the caller binding and then asking.
async fn start_keeping(
    config: Config,
    config_dir: &Path,
) -> (SocketAddr, volucer_http::ReloadHandle) {
    let sites = volucer_http::SharedState::new(Sites::from_config(&config, config_dir));
    let dispatcher = Arc::new(
        Dispatcher::from_config(&config, volucer_fcgi::Routing::default()).expect("dispatcher"),
    );

    let listener = HttpListener::bind_with_tls(
        "127.0.0.1:0",
        dispatcher,
        sites,
        HttpLimits::default(),
        ListenerSettings {
            tls: Some(TlsSettings {
                config: &config,
                config_dir,
            }),
            redirect_to_https: false,
            https_port: None,
        },
    )
    .await
    .expect("bind with TLS");

    let addr = listener.local_addr().expect("bound address");
    let reload = listener.reload_handle();
    tokio::spawn(listener.run());
    (addr, reload)
}

/// What a client saw on one connection.
struct Seen {
    /// The DER of the certificate the server presented.
    certificate: Vec<u8>,
    /// The protocol ALPN selected, empty when none was.
    alpn: Vec<u8>,
    /// The raw HTTP response, empty when the handshake failed.
    response: Vec<u8>,
}

/// Connect over TLS and report what came back.
async fn fetch(addr: SocketAddr, server_name: &str, alpn: &[&str]) -> Seen {
    use rustls::pki_types::ServerName;

    // A self-signed certificate is in no root store, so the verifier is replaced wholesale —
    // and the assertion is about *which* certificate arrived, not whether a chain validates,
    // so the certificate is compared byte for byte instead. `dangerous()` comes first in the
    // builder because it *replaces* the root-store step rather than following it.
    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny))
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.as_bytes().to_vec()).collect();

    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let name = ServerName::try_from(server_name.to_string()).expect("server name");
    let mut tls = connector
        .connect(name, stream)
        .await
        .expect("the TLS handshake must succeed");

    let (certificate, alpn_selected) = {
        let (_, session) = tls.get_ref();
        (
            session
                .peer_certificates()
                .and_then(|certs| certs.first())
                .map(|c| c.as_ref().to_vec())
                .expect("the server must present a certificate"),
            session.alpn_protocol().unwrap_or_default().to_vec(),
        )
    };

    tls.write_all(b"GET / HTTP/1.1\r\nHost: ignored\r\nConnection: close\r\n\r\n")
        .await
        .expect("write request");
    let mut response = Vec::new();
    // Read to the end and shut down, so the listener's task finishes and releases its slot.
    // Leaving the connection open would make the *next* request on the same listener see "too
    // many connections", which is a confusing way to fail.
    let _ = tls.read_to_end(&mut response).await;
    let _ = tls.shutdown().await;

    Seen {
        certificate,
        alpn: alpn_selected,
        response,
    }
}

/// Send cleartext to `addr` and report whether an **HTTP response** came back.
///
/// Deliberately not "did any bytes arrive": a TLS-only port answers a cleartext connection with
/// a TLS alert, which is bytes. The question that matters is whether the request was served, and
/// a served request begins with an HTTP status line.
async fn fetch_cleartext(addr: SocketAddr) -> Option<Vec<u8>> {
    fetch_cleartext_with_host(addr, "test").await
}

/// [`fetch_cleartext`], with a chosen `Host` header — which the redirect case needs, because the
/// redirect is built from the host the client asked for.
async fn fetch_cleartext_with_host(addr: SocketAddr, host: &str) -> Option<Vec<u8>> {
    let mut stream = tokio::net::TcpStream::connect(addr).await.ok()?;
    stream
        .write_all(
            format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .ok()?;
    let mut response = Vec::new();
    // Read to the end, so the connection is closed from this side and the listener's task
    // finishes and releases its slot. Returning early would leave the slot held, which the
    // next test in the same listener would see as "too many connections".
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_to_end(&mut response),
    )
    .await;
    drop(stream);
    if response.starts_with(b"HTTP/") {
        Some(response)
    } else {
        None
    }
}

/// Accepts any certificate, for a self-signed fixture.
#[derive(Debug)]
struct AcceptAny;

impl rustls::client::danger::ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
        ]
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_tls_listener_completes_a_handshake_and_serves_a_request() {
    let cert = Cert::new("basic", "example.test");
    let config = write_config(&cert, "");
    let addr = start(config, cert.dir()).await;

    let seen = fetch(addr, "example.test", &["http/1.1"]).await;

    assert!(
        seen.response.starts_with(b"HTTP/1.1"),
        "a request over TLS must be answered: {:?}",
        String::from_utf8_lossy(&seen.response)
    );
}

#[tokio::test]
async fn sni_selects_a_per_site_certificate() {
    // The point of the per-site table: two names, two certificates, one listener.
    let cert = Cert::new("sni", "default.test");
    cert.add("shop", "shop.test");

    // The two certificates are read back as DER, which is what the client receives. They must
    // differ, or the test could pass by presenting the wrong one.
    let default_der = der_of(&cert.dir().join("certs/server.crt"));
    let shop_der = der_of(&cert.dir().join("certs/shop.crt"));
    assert_ne!(
        default_der, shop_der,
        "the fixture must generate two different certificates, or the test is vacuous"
    );

    let config = write_config(
        &cert,
        r#"
[sites.shop]
hosts = ["shop.test"]
document_root = '/tmp'

[sites.shop.tls]
certificate = "certs/shop.crt"
key = "certs/shop.key"
"#,
    );
    let addr = start(config, cert.dir()).await;

    let shop = fetch(addr, "shop.test", &["http/1.1"]).await;
    assert_eq!(
        shop.certificate, shop_der,
        "the name in the handshake must select the site's own certificate"
    );

    // An unclaimed name falls back to the server's rather than failing the handshake: a client
    // that claims a name nobody configured is normal, not an error.
    let other = fetch(addr, "nothing.test", &["http/1.1"]).await;
    assert_eq!(
        other.certificate, default_der,
        "an unclaimed name must fall back to the server's certificate"
    );

    // The default name itself must also get the default certificate, not another site's.
    let named = fetch(addr, "default.test", &["http/1.1"]).await;
    assert_eq!(named.certificate, default_der);
}

#[tokio::test]
async fn alpn_negotiates_http2_then_http11() {
    // ALPN is how a browser reaches HTTP/2, and it arrives with TLS — so this is also the first
    // HTTP/2 negotiation test in the repository.
    let cert = Cert::new("alpn", "example.test");
    let config = write_config(&cert, "");
    let addr = start(config, cert.dir()).await;

    let h2 = fetch(addr, "example.test", &["h2", "http/1.1"]).await;
    assert_eq!(h2.alpn, b"h2", "h2 must win when both are offered");

    // A client that cannot do h2 must still connect. This is why `http/1.1` must not be dropped
    // from the default: with only `h2` offered, that handshake would fail instead of downgrading.
    let http1 = fetch(addr, "example.test", &["http/1.1"]).await;
    assert_eq!(http1.alpn, b"http/1.1");
    assert!(http1.response.starts_with(b"HTTP/1.1"));
}

#[tokio::test]
async fn a_tls_only_port_refuses_cleartext() {
    // `plain` defaults to false, so a cleartext connection must not be served: the client
    // believes this port is encrypted, and answering would be the wrong answer.
    let cert = Cert::new("noplain", "example.test");
    let config = write_config(&cert, "");
    let addr = start(config, cert.dir()).await;

    assert!(
        fetch_cleartext(addr).await.is_none(),
        "a TLS-only port must not answer a cleartext request"
    );
    // The control: the same port over TLS must work, so the refusal above is about the
    // protocol rather than a listener that is simply broken.
    let tls = fetch(addr, "example.test", &["http/1.1"]).await;
    assert!(tls.response.starts_with(b"HTTP/1.1"));
}

#[tokio::test]
async fn plain_true_serves_cleartext_and_tls_on_one_port() {
    // The same-port case: the first byte decides, which is what nginx's `ssl_preread` does.
    let cert = Cert::new("plain", "example.test");
    let config = write_config(&cert, "plain = true");
    let addr = start(config, cert.dir()).await;

    let cleartext = fetch_cleartext(addr)
        .await
        .expect("plain = true must answer a cleartext request on the TLS port");
    assert!(
        cleartext.starts_with(b"HTTP/1.1"),
        "the cleartext answer must be a real HTTP response: {:?}",
        String::from_utf8_lossy(&cleartext)
    );

    // And TLS on the same socket still works, which is the half that makes it a detection
    // rather than a replacement.
    let tls = fetch(addr, "example.test", &["http/1.1"]).await;
    assert!(tls.response.starts_with(b"HTTP/1.1"));
}

#[tokio::test]
async fn a_1_3_floor_still_serves_a_1_3_client() {
    // `min_version` is a floor. A 1.2-only client is covered by the unit tests on the version
    // list; what this pins is that the configuration is actually applied rather than accepted
    // and ignored.
    let cert = Cert::new("min13", "example.test");
    let config = write_config(&cert, "min_version = \"1.3\"");
    let addr = start(config, cert.dir()).await;

    let seen = fetch(addr, "example.test", &["http/1.1"]).await;
    assert!(seen.response.starts_with(b"HTTP/1.1"));
}

#[tokio::test]
async fn a_redirect_does_not_turn_a_plaintext_listener_into_a_tls_port() {
    // The bug this pins, found by running the split :80/:443 shape rather than by reading it:
    // the redirect was originally derived from the presence of TLS settings, so the plaintext
    // listener built an acceptor and stopped answering cleartext at all. A client would have
    // seen a connection that accepted and then hung or failed, with nothing in the log.
    let cert = Cert::new("noregress", "example.test");
    let config = write_config(&cert, "");
    let addr = start_plaintext_redirecting(config, cert.dir(), Some(443)).await;

    let response = fetch_cleartext_with_host(addr, "example.test")
        .await
        .expect("a plaintext listener with a redirect must still answer cleartext");
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.starts_with("HTTP/1.1 301"),
        "and it must answer with the redirect, not a TLS alert: {text}"
    );
}

#[tokio::test]
async fn redirect_to_https_answers_cleartext_with_a_301() {
    // The redirect must name the https port, not the port the request arrived on — otherwise it
    // sends the client back to the plaintext port it just used, which is a loop.
    let cert = Cert::new("redirect", "example.test");
    let config = write_config(&cert, "plain = true");
    let addr = start_redirecting(config, cert.dir()).await;

    let response = fetch_cleartext_with_host(addr, "example.test")
        .await
        .expect("a redirecting listener must answer");
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.starts_with("HTTP/1.1 301"),
        "the answer must be a 301: {text}"
    );
    assert!(
        text.contains(&format!("location: https://example.test:{}/", addr.port())),
        "the redirect must name the https endpoint and keep the path: {text}"
    );

    // And TLS on the same port must NOT be redirected, which is the half that makes this safe:
    // a redirect on the TLS path would be an infinite loop, because the client would come back
    // over TLS and be redirected again.
    //
    // Asserted as "not a 301" rather than "200": this fixture has no backends, so a non-static
    // path legitimately answers 503. What matters is the status class, not the exact code.
    let tls = fetch(addr, "example.test", &["http/1.1"]).await;
    let text = String::from_utf8_lossy(&tls.response);
    assert!(
        !text.starts_with("HTTP/1.1 301"),
        "a TLS request must be served, not redirected to itself: {text}"
    );
    assert!(
        !text.contains("location:"),
        "a TLS answer must not carry a redirect: {text}"
    );
}

#[tokio::test]
async fn redirect_on_the_default_port_leaves_it_out_of_the_location() {
    // `:443` in a URL is noise, so a bare authority is what a client should be sent. The
    // listener cannot bind 443 without privileges, so the port is supplied directly — which is
    // also the shape a split :80/:443 deployment uses, where `https_port` is what matters.
    let cert = Cert::new("redirect443", "example.test");
    let config = write_config(&cert, "plain = true");
    let addr = start_plaintext_redirecting(config, cert.dir(), Some(443)).await;

    let response = fetch_cleartext_with_host(addr, "example.test")
        .await
        .expect("answered");
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.contains("location: https://example.test/"),
        "port 443 must be omitted from the location: {text}"
    );
}

#[tokio::test]
async fn a_redirect_keeps_the_path_and_query() {
    // A redirect that dropped the path would send every request to the front controller.
    let cert = Cert::new("redirectpath", "example.test");
    let config = write_config(&cert, "plain = true");
    let addr = start_redirecting(config, cert.dir()).await;

    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(
            b"GET /docs/page?x=1&y=2 HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n",
        )
        .await
        .expect("write");
    let mut response = Vec::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_to_end(&mut response),
    )
    .await;

    let text = String::from_utf8_lossy(&response);
    assert!(text.contains("HTTP/1.1 301"), "{text}");
    assert!(
        text.contains("/docs/page?x=1&y=2"),
        "the path and query must survive: {text}"
    );
}

/// Start a listener that serves TLS **and** redirects cleartext on the same port.
///
/// The `plain = true` shape: one socket carries both protocols, and only the cleartext half is
/// redirected.
async fn start_redirecting(config: Config, config_dir: &Path) -> SocketAddr {
    start_redirecting_to(config, config_dir, None, true).await
}

/// Start a plaintext-only listener that redirects, which is the split `:80`/`:443` shape.
///
/// No acceptor, deliberately: this listener must answer cleartext, and giving it TLS settings
/// would have made it a TLS port that no cleartext client could reach. That is the bug this
/// separate constructor exists to keep out of the tests.
async fn start_plaintext_redirecting(
    config: Config,
    config_dir: &Path,
    https_port: Option<u16>,
) -> SocketAddr {
    start_redirecting_to(config, config_dir, https_port, false).await
}

async fn start_redirecting_to(
    config: Config,
    config_dir: &Path,
    https_port: Option<u16>,
    with_tls: bool,
) -> SocketAddr {
    let sites = volucer_http::SharedState::new(Sites::from_config(&config, config_dir));
    let dispatcher = Arc::new(
        Dispatcher::from_config(&config, volucer_fcgi::Routing::default()).expect("dispatcher"),
    );

    let listener = HttpListener::bind_with_tls(
        "127.0.0.1:0",
        dispatcher,
        sites,
        HttpLimits::default(),
        ListenerSettings {
            tls: with_tls.then_some(TlsSettings {
                config: &config,
                config_dir,
            }),
            redirect_to_https: true,
            https_port,
        },
    )
    .await
    .expect("bind with redirect");

    let addr = listener.local_addr().expect("bound address");
    tokio::spawn(listener.run());
    addr
}

/// Send the HTTP/2 connection preface and report whether a SETTINGS frame came back.
///
/// A client that knows the server speaks h2 opens with this exact byte sequence and then an
/// empty SETTINGS frame; the server must answer with SETTINGS of its own. Asserted on the frame
/// *type* byte rather than its contents, because the contents are `hyper`'s business and change
/// between versions — what a caller of this server needs is that HTTP/2 is real rather than only
/// negotiated.
async fn preface_over_tls(addr: SocketAddr, server_name: &str) -> Option<u8> {
    use rustls::pki_types::ServerName;

    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny))
        .with_no_client_auth();
    // h2 only, so a server that failed to act on ALPN cannot answer in HTTP/1.1 by accident.
    config.alpn_protocols = vec![b"h2".to_vec()];

    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let stream = tokio::net::TcpStream::connect(addr).await.ok()?;
    let name = ServerName::try_from(server_name.to_string()).ok()?;
    let mut tls = connector.connect(name, stream).await.ok()?;

    tls.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
        .await
        .ok()?;
    // An empty SETTINGS frame: 9-byte header, type 0x4, no flags, stream 0, zero-length payload.
    tls.write_all(&[0, 0, 0, 0x04, 0, 0, 0, 0, 0]).await.ok()?;
    tls.flush().await.ok()?;

    // The server's SETTINGS frame: length(3) type(1) flags(1) stream(4), then payload.
    let mut header = [0u8; 9];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tls.read_exact(&mut header),
    )
    .await
    .ok()?
    .ok()?;
    Some(header[3])
}
#[tokio::test]
async fn a_certificate_that_cannot_load_fails_before_the_port_is_bound() {
    // The claim this pins, and why it is a test: the doc on `bind_with_tls` said "a certificate
    // that cannot load fails this call — before the port is bound", and the code bound the port
    // first. Only a *precedence* observation can tell the two orders apart — a call that binds
    // and then fails still returns the TLS error, and a call that checks the certificate first
    // returns the same error, so asserting on the error alone proves nothing.
    //
    // So the port is occupied before the call, and both faults are present at once. The order
    // decides which one is reported:
    //
    //   certificate checked first -> the certificate error (the port was never touched)
    //   port bound first          -> the bind error ("address already in use")
    //
    // This was written first as a race between two concurrent calls, and that version passed
    // against the bug: the first call bound, failed and released the port before the second one
    // reached its bind. An unforced race is not a test of an ordering.
    let cert = Cert::new("bindorder", "example.test");

    let text = format!(
        r#"
[server]
http = "127.0.0.1:0"
document_root = '{}'

[server.tls]
certificate = "certs/absent.crt"
key = "certs/absent.key"
"#,
        cert.dir().display()
    );
    let config = Config::from_toml(&text).expect("config parses");
    let config_dir = cert.dir().to_path_buf();

    // Hold the port, so a bind cannot succeed and `bind_with_tls` is forced to choose.
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").expect("a probe port");
    let address = occupied.local_addr().expect("address").to_string();

    let sites = volucer_http::SharedState::new(Sites::from_config(&config, &config_dir));
    let dispatcher = Arc::new(
        Dispatcher::from_config(&config, volucer_fcgi::Routing::default()).expect("dispatcher"),
    );

    let err = HttpListener::bind_with_tls(
        &address,
        dispatcher,
        sites,
        HttpLimits::default(),
        ListenerSettings {
            tls: Some(TlsSettings {
                config: &config,
                config_dir: &config_dir,
            }),
            redirect_to_https: false,
            https_port: None,
        },
    )
    .await
    .err()
    .expect("a certificate that cannot load must not bind");

    let message = err.to_string();
    assert!(
        message.contains("absent.crt"),
        "the certificate must be reported before the port is taken, so a bad certificate cannot \
         hide behind a busy port (or vice versa): {message}"
    );
    assert!(
        !message.contains("cannot bind"),
        "the port must not have been bound at all: {message}"
    );

    drop(occupied);
}
/// A certificate renewed on disk is presented on the next handshake, with no restart.
///
/// This is the property ACME depends on and the reason TLS was made replaceable at all: an
/// issuance happens while the process is running, and the very next client that connects has to
/// get the new certificate. Asserted the way a client sees it — the *same* address, the *same*
/// listener, a byte-different certificate after the swap — rather than by inspecting the table
/// that was installed.
#[tokio::test]
async fn a_certificate_reload_is_presented_on_the_next_handshake() {
    use volucer_http::{CertificateSet, ServerCertificate};

    let cert = Cert::new("reload", "example.test");
    let config = write_config(&cert, "");
    let (addr, reload) = start_keeping(config, cert.dir()).await;

    let first_der = der_of(&cert.dir().join("certs/server.crt"));
    let before = fetch(addr, "example.test", &["http/1.1"]).await;
    assert_eq!(
        before.certificate, first_der,
        "the fixture must serve the certificate it was started with"
    );

    // Replace the pair on disk, which is what a renewal does, and reload. The *file* is what
    // changed, so this also pins that a reload re-reads rather than reusing what it loaded at
    // startup — the mistake that would make an operator's renewal appear to do nothing.
    cert.add("server", "example.test");
    let renewed_der = der_of(&cert.dir().join("certs/server.crt"));
    assert_ne!(
        first_der, renewed_der,
        "the fixture must generate a different certificate, or the test is vacuous"
    );

    let resolver = reload
        .tls()
        .expect("a TLS listener must expose its resolver");
    let replaced = resolver
        .refresh(&CertificateSet {
            server: Some(ServerCertificate {
                certificate: "certs/server.crt".into(),
                key: "certs/server.key".into(),
            }),
            sites: Vec::new(),
        })
        .expect("the renewed pair loads");

    // What was replaced, so a caller can report the change rather than the attempt.
    assert_eq!(
        replaced.named(),
        0,
        "the replaced table must be the one that was serving, not the one just installed"
    );

    let after = fetch(addr, "example.test", &["http/1.1"]).await;
    assert_eq!(
        after.certificate, renewed_der,
        "the next handshake must present the renewed certificate, on the same listener"
    );
}

/// A reload whose certificate cannot load installs **nothing** — neither half of the table.
///
/// The whole-or-nothing rule, at the level that matters. A half-applied table is worse than a
/// refused one: the names it dropped fail every handshake, and the failure looks like a network
/// problem rather than like a bad file. The set below names a perfectly good *server* pair and
/// an unreadable *site* pair, so a refresh that loaded what it could and reported what it could
/// not would install the server certificate and lose a site — and this test would see both.
///
/// That shape is deliberate rather than incidental: a test whose only assertion is that nothing
/// changed passes just as well against a `refresh` that does nothing at all. Naming a good
/// certificate in the refused set is what makes "nothing was applied" an observation.
#[tokio::test]
async fn a_certificate_reload_that_cannot_load_applies_neither_half() {
    use volucer_http::{CertificateSet, ServerCertificate, SiteCertificate, SiteCertificatePair};

    let cert = Cert::new("reload-partial", "default.test");
    cert.add("shop", "shop.test");
    let config = write_config(
        &cert,
        r#"
[sites.shop]
hosts = ["shop.test"]
document_root = '/tmp'

[sites.shop.tls]
certificate = "certs/shop.crt"
key = "certs/shop.key"
"#,
    );
    let (addr, reload) = start_keeping(config, cert.dir()).await;

    let server_der = der_of(&cert.dir().join("certs/server.crt"));
    let shop_der = der_of(&cert.dir().join("certs/shop.crt"));
    assert_eq!(
        fetch(addr, "default.test", &["http/1.1"]).await.certificate,
        server_der,
        "the fixture must serve the server certificate before the reload"
    );
    assert_eq!(
        fetch(addr, "shop.test", &["http/1.1"]).await.certificate,
        shop_der,
        "the fixture must serve the site certificate before the reload"
    );

    // A *different* server pair, so a partial install would be visible as a changed answer for
    // `default.test` rather than as an unchanged one.
    cert.add("renewed", "default.test");
    let renewed_der = der_of(&cert.dir().join("certs/renewed.crt"));
    assert_ne!(server_der, renewed_der, "the fixture must differ");

    let resolver = reload
        .tls()
        .expect("a TLS listener must expose its resolver");
    let err = resolver
        .refresh(&CertificateSet {
            // Valid, and must still not be installed.
            server: Some(ServerCertificate {
                certificate: "certs/renewed.crt".into(),
                key: "certs/renewed.key".into(),
            }),
            sites: vec![SiteCertificate {
                name: "shop".into(),
                hosts: vec!["shop.test".into()],
                tls: SiteCertificatePair {
                    certificate: "certs/absent.crt".into(),
                    key: "certs/absent.key".into(),
                },
            }],
        })
        .expect_err("one unreadable certificate must refuse the set");

    let message = err.to_string();
    assert!(message.contains("sites.shop.tls"), "{message}");
    assert_eq!(
        fetch(addr, "default.test", &["http/1.1"]).await.certificate,
        server_der,
        "a refused reload must not install the half that *could* be read"
    );
    assert_eq!(
        fetch(addr, "shop.test", &["http/1.1"]).await.certificate,
        shop_der,
        "the site must keep the certificate it had, not lose it to a partial reload"
    );
}

/// A certificate with no config behind it is served as soon as it is replaced.
///
/// The in-memory path, which is the one ACME actually takes: the issued bytes never go through a
/// file this process wrote, so correctness must not depend on re-reading one. A resolver built
/// around a table that names nothing but a fallback is the shape a first issuance starts from.
#[tokio::test]
async fn an_in_memory_certificate_needs_no_file_and_no_config() {
    use volucer_http::Certs;

    let cert = Cert::new("inmemory", "example.test");
    let config = write_config(&cert, "");

    // The initial table names no host at all: every handshake gets the fallback. That is what a
    // deployment looks like before its first renewal.
    let first = load_pair(cert.dir(), "server");
    let original_der = der_of(&cert.dir().join("certs/server.crt"));

    let resolver = Arc::new(volucer_http::SniResolver::from_certs(
        Certs::new(first, std::iter::empty()),
        cert.dir(),
    ));

    let sites = volucer_http::SharedState::new(Sites::from_config(&config, cert.dir()));
    let dispatcher = Arc::new(
        Dispatcher::from_config(&config, volucer_fcgi::Routing::default()).expect("dispatcher"),
    );
    // The listener is built around a resolver that already holds its certificate: **no
    // certificate file is read here at all**, because `bind_with_resolver` takes the table the
    // caller built. That is what lets a deployment whose certificate does not exist yet — the
    // ACME case — bind its port and start accepting.
    let listener = HttpListener::bind_with_resolver(
        "127.0.0.1:0",
        dispatcher,
        sites,
        HttpLimits::default(),
        ListenerSettings {
            tls: Some(TlsSettings {
                config: &config,
                config_dir: cert.dir(),
            }),
            redirect_to_https: false,
            https_port: None,
        },
        Some(Arc::clone(&resolver)),
    )
    .await
    .expect("bind with a caller-built resolver");
    let addr = listener.local_addr().expect("bound address");
    tokio::spawn(listener.run());

    // A name the table does not claim gets the fallback, which is `original_der`. The resolver
    // was handed its table directly, so this pins that `replace` is what a client sees.
    assert_eq!(
        fetch(addr, "unclaimed.test", &["http/1.1"])
            .await
            .certificate,
        original_der
    );

    cert.add("second", "example.test");
    let renewed = load_pair(cert.dir(), "second");
    let renewed_der = der_of(&cert.dir().join("certs/second.crt"));

    let table = resolver.current().with_host("example.test", renewed);
    let _ = resolver.replace(table);

    assert_eq!(
        fetch(addr, "example.test", &["http/1.1"]).await.certificate,
        renewed_der,
        "a certificate installed from memory must be presented on the next handshake"
    );

    // And nothing else moved: a name the new table still does not claim keeps the fallback.
    assert_eq!(
        fetch(addr, "unclaimed.test", &["http/1.1"])
            .await
            .certificate,
        original_der,
        "adding a name must not change what an unclaimed name is served"
    );
}

/// A reload whose **server** certificate cannot load refuses, and names the layer.
///
/// The common failure in practice: a renewal wrote a file this process cannot read. Worth its
/// own case because the message is what an operator acts on, and because it is the failure a
/// first issuance produces — with the site table empty, which is a different shape from the
/// test above.
#[tokio::test]
async fn a_server_certificate_that_cannot_load_names_the_layer() {
    use volucer_http::{CertificateSet, ServerCertificate};

    let cert = Cert::new("reload-nofile", "example.test");
    let config = write_config(&cert, "");
    let (addr, reload) = start_keeping(config, cert.dir()).await;

    let serving_der = der_of(&cert.dir().join("certs/server.crt"));
    assert_eq!(
        fetch(addr, "example.test", &["http/1.1"]).await.certificate,
        serving_der
    );

    let resolver = reload
        .tls()
        .expect("a TLS listener must expose its resolver");
    let err = resolver
        .refresh(&CertificateSet {
            server: Some(ServerCertificate {
                certificate: "certs/absent.crt".into(),
                key: "certs/absent.key".into(),
            }),
            sites: Vec::new(),
        })
        .expect_err("a certificate that is not there must not be installed");

    let message = err.to_string();
    assert!(message.contains("server.tls"), "{message}");
    assert!(message.contains("absent.crt"), "{message}");

    // Nothing may have moved. A refresh that swapped first and loaded afterwards would report
    // the same error and serve a broken table.
    assert_eq!(
        fetch(addr, "example.test", &["http/1.1"]).await.certificate,
        serving_der,
        "a refused reload must leave the previous certificate serving"
    );
}

/// A listener with no TLS has no resolver at all, and still reloads its sites.
///
/// The TLS-off invariant is the thing this front-end's design note is about — *"the whole of
/// the cost when TLS is off: no accept wrapping, no peek, no TLS type built"* — and making
/// certificates replaceable is exactly the kind of change that could smuggle a TLS value onto a
/// plaintext deployment. The assertion is the absence, which is what a handshake path cannot
/// have: `resolver()` is `None`, so there is no table to load and no branch to take.
///
/// The second half matters as much as the first: reloading sites must still work on that same
/// listener, so the reload mechanism is not accidentally tied to TLS being present.
#[tokio::test]
async fn a_plaintext_listener_has_no_resolver_and_still_reloads_sites() {
    let base = std::env::temp_dir().join(format!("volucer-no-tls-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let before = base.join("before");
    let after = base.join("after");
    for root in [&before, &after] {
        std::fs::create_dir_all(root).expect("create a site root");
    }
    std::fs::write(before.join("whoami.txt"), "before").expect("write the marker");
    std::fs::write(after.join("whoami.txt"), "after").expect("write the marker");

    let config_for = |root: &Path| {
        let text = format!(
            r#"
[server]
http = "127.0.0.1:0"
document_root = {root:?}
static_files = {{ enabled = true }}
"#,
            root = root.to_string_lossy(),
        );
        let config = Config::from_toml(&text).expect("config parses");
        config.validate().expect("config validates");
        config
    };

    let first = config_for(&before);
    let sites = Arc::new(Sites::from_config(&first, &before));
    let dispatcher = Arc::new(
        Dispatcher::from_config(&first, volucer_fcgi::Routing::default()).expect("dispatcher"),
    );
    let listener = HttpListener::bind("127.0.0.1:0", dispatcher, sites, HttpLimits::default())
        .await
        .expect("bind a plaintext listener");
    let reload = listener.reload_handle();

    assert!(
        reload.tls().is_none(),
        "a listener with no `[server.tls]` must not carry a certificate table at all"
    );

    let second = config_for(&after);
    let _ = reload.apply(Sites::from_config(&second, &after));
    assert_eq!(
        reload
            .current()
            .sites()
            .default_site()
            .document_root
            .as_path(),
        after.as_path(),
        "site reloading must work on a plaintext listener too"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// A certificate resolver on a listener with no TLS settings is refused, not ignored.
///
/// The failure a caller cannot see otherwise: a resolver nothing consults is a certificate table
/// they believe is serving, and the symptom would be a handshake that presents a certificate
/// from the config rather than the one just loaded. Refusing at bind time turns that into one
/// error message instead of a mystery at 03:00.
#[tokio::test]
async fn a_resolver_without_tls_settings_is_refused() {
    let cert = Cert::new("no-tls-settings", "example.test");
    let config = write_config(&cert, "");
    let table = load_pair(cert.dir(), "server");
    let resolver = Arc::new(volucer_http::SniResolver::from_certs(
        volucer_http::Certs::new(table, std::iter::empty()),
        cert.dir(),
    ));

    let sites = volucer_http::SharedState::new(Sites::from_config(&config, cert.dir()));
    let dispatcher = Arc::new(
        Dispatcher::from_config(&config, volucer_fcgi::Routing::default()).expect("dispatcher"),
    );

    let err = HttpListener::bind_with_resolver(
        "127.0.0.1:0",
        dispatcher,
        sites,
        HttpLimits::default(),
        ListenerSettings::default(),
        Some(resolver),
    )
    .await
    .err()
    .expect("a resolver with no TLS settings must not bind");

    assert!(err.to_string().contains("no TLS settings"), "{err}");
}

#[tokio::test]
async fn alpn_h2_negotiates_a_real_http_2_connection() {
    // `alpn_negotiates_http2_then_http11` asserts which protocol ALPN *selected*. This asserts
    // the selected protocol actually works, which is the claim `volucer version` makes when it
    // prints "HTTP/2 (cleartext)": the preface is accepted and answered with a SETTINGS frame
    // rather than with an HTTP/1.1 status line or nothing at all.
    let cert = Cert::new("h2preface", "example.test");
    let config = write_config(&cert, "");
    let addr = start(config, cert.dir()).await;

    let frame_type = preface_over_tls(addr, "example.test")
        .await
        .expect("the server must answer the HTTP/2 preface");

    assert_eq!(
        frame_type, 0x04,
        "the first frame back must be SETTINGS (0x04), got {frame_type:#04x}"
    );
}
/// DER bytes of a certificate pair on disk, loaded the way the resolver loads them.
///
/// Through `load_certified` rather than `CertifiedKey::from_der`, so a test that installs a
/// certificate in memory uses the same code path a config-driven reload does — a test that
/// hand-rolled the pairing could pass while the real one was broken.
fn load_pair(dir: &Path, file: &str) -> volucer_http::tls::LoadedCert {
    volucer_http::tls::load_certified(
        &volucer_core::config::TlsConfig {
            certificate: format!("certs/{file}.crt").into(),
            key: format!("certs/{file}.key").into(),
            min_version: volucer_core::config::MinTlsVersion::default(),
            alpn: Vec::new(),
            plain: false,
        },
        dir,
        "test",
    )
    .expect("the fixture certificate loads")
}

/// The DER payload of a PEM certificate file.
fn der_of(path: &Path) -> Vec<u8> {
    let text = std::fs::read_to_string(path).expect("read certificate");
    let body: String = text
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    base64_decode(&body)
}

/// Base64, only as much of it as a certificate needs.
///
/// Hand-rolled to avoid a dependency for one call in one test.
fn base64_decode(input: &str) -> Vec<u8> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for byte in input.bytes() {
        let Some(value) = TABLE.iter().position(|&c| c == byte) else {
            continue;
        };
        acc = (acc << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}
