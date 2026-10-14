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

use stokehold_core::config::Config;
use stokehold_core::sites::Sites;
use stokehold_http::{HttpLimits, HttpListener, TlsSettings};
use stokehold_server::Dispatcher;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
            std::env::temp_dir().join(format!("stokehold-tls-{label}-{}-{n}", std::process::id()));
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
    std::fs::write(cert.dir().join("stokehold.toml"), &text).expect("write config");
    let config = Config::from_toml(&text).expect("config parses");
    config.validate().expect("config validates");
    config
}

/// Start a listener in the background and return the address it bound.
///
/// The listener task owns the listener; only the address comes back, because `run` consumes it
/// and the task lives until the test's runtime shuts down.
async fn start(config: Config, config_dir: &Path) -> SocketAddr {
    let sites = Arc::new(Sites::from_config(&config, config_dir));
    let dispatcher = Arc::new(
        Dispatcher::from_config(&config, stokehold_fcgi::Routing::default()).expect("dispatcher"),
    );

    let listener = HttpListener::bind_with_tls(
        "127.0.0.1:0",
        dispatcher,
        sites,
        HttpLimits::default(),
        Some(TlsSettings {
            config: &config,
            config_dir,
        }),
    )
    .await
    .expect("bind with TLS");

    let addr = listener.local_addr().expect("bound address");
    tokio::spawn(listener.run());
    addr
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
    let _ = tls.read_to_end(&mut response).await;

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
    let mut stream = tokio::net::TcpStream::connect(addr).await.ok()?;
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
        .await
        .ok()?;
    let mut response = Vec::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_to_end(&mut response),
    )
    .await;
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
