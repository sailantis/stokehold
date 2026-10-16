//! An `https:` origin, and the assertion that its certificate is really verified.
//!
//! This file exists for one reason: **it is easy to write a proxy that looks encrypted and checks
//! nothing.** nginx's `proxy_ssl_verify` defaults to *off*, which is why so many deployments
//! believe they have a verified origin when they do not. volucer has no such knob — no
//! `dangerous()` verifier is ever installed — and a claim like that is worth nothing unless a test
//! fails when it stops being true.
//!
//! So the same origin is reached three ways:
//!
//! | `ca` | expected |
//! | --- | --- |
//! | the root that signed the origin's leaf | the request succeeds |
//! | a *different* root | the handshake fails |
//! | nothing named, so the public roots | the handshake fails — the leaf is not publicly trusted |
//!
//! The middle row is the one that catches a missing verifier. The third catches "no `ca` means no
//! verification" instead of "no `ca` means the public roots", which is the same bug wearing a
//! plausible default.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Empty;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use volucer_upstream::{Origin, UpstreamClient, UpstreamRequest, UpstreamTls};

/// Make the crypto provider available, as every rustls entry point needs.
///
/// Called explicitly rather than relied on to have happened in the crate, because this test builds
/// a *server* configuration before it builds a client — and `ServerConfig::builder` needs the
/// provider installed, not merely installed eventually.
fn install_provider() {
    use std::sync::OnceLock;
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// A root certificate, and a leaf it signed for `127.0.0.1`.
struct Chain {
    /// The leaf's chain, as an origin would present it.
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    /// Where the root's PEM was written, so a client can be pointed at it.
    root_path: PathBuf,
    /// A root that did *not* sign the leaf, for the negative case.
    other_root_path: PathBuf,
}

/// Build a root, a leaf signed by it, and a second unrelated root.
///
/// Two roots rather than one is the whole point: "verification failed" is not a useful assertion
/// unless the same client succeeds when pointed at the right root — otherwise the failure could be
/// caused by anything in the handshake.
fn chain(name: &str) -> Chain {
    install_provider();

    let directory =
        std::env::temp_dir().join(format!("volucer-upstream-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a fixture directory");

    let mut root_params = rcgen::CertificateParams::default();
    root_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    root_params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    let root_key = rcgen::KeyPair::generate().expect("a root key");
    let root = root_params
        .self_signed(&root_key)
        .expect("a self-signed root");

    // The address is the name the client verifies against, because the origin string names an IP
    // and no `sni` is set — so the leaf needs an **IP SAN**, not just a DNS one. A DNS-only
    // certificate fails this handshake with a name mismatch, which is a different failure from a
    // missing verifier and would make the test prove the wrong thing.
    let mut leaf_params = rcgen::CertificateParams::default();
    leaf_params.subject_alt_names = vec![
        rcgen::SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        rcgen::SanType::DnsName("localhost".try_into().expect("a dns name")),
    ];
    let leaf_key = rcgen::KeyPair::generate().expect("a leaf key");
    let leaf = leaf_params
        .signed_by(&leaf_key, &root, &root_key)
        .expect("a leaf signed by the root");

    let mut other_params = rcgen::CertificateParams::default();
    other_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    other_params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign];
    let other_key = rcgen::KeyPair::generate().expect("a second root key");
    let other = other_params
        .self_signed(&other_key)
        .expect("a second self-signed root");

    let root_path = directory.join("root.pem");
    std::fs::write(&root_path, root.pem()).expect("the root is written");
    let other_root_path = directory.join("other.pem");
    std::fs::write(&other_root_path, other.pem()).expect("the other root is written");

    Chain {
        chain: vec![leaf.der().clone()],
        key: PrivateKeyDer::try_from(leaf_key.serialize_der()).expect("a private key"),
        root_path,
        other_root_path,
    }
}

/// A TLS origin that answers one request and then stays up.
fn tls_origin(chain: &Chain) -> SocketAddr {
    install_provider();

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain.chain.clone(), chain.key.clone_key())
        .expect("a server configuration");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

    // A standard listener so the address is known before the runtime thread starts, which keeps
    // the test free of an async setup dance for a fixture.
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    let address = std_listener.local_addr().expect("an address");
    std_listener
        .set_nonblocking(true)
        .expect("a non-blocking listener");

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        runtime.block_on(async move {
            let listener = TcpListener::from_std(std_listener).expect("the listener");
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(stream).await else {
                        // A refused or failed handshake is the *expected* outcome in the negative
                        // cases, so it is not an error here.
                        return;
                    };
                    // Read the request head off the stream. Manual reading rather than
                    // `BufReader`, because a `BufReader` *owns* a `&mut` borrow of the stream and
                    // then the response cannot be written back through it.
                    let mut buffer = Vec::new();
                    let mut byte = [0u8; 1];
                    while tls.read_exact(&mut byte).await.is_ok() {
                        buffer.push(byte[0]);
                        if buffer.ends_with(b"\r\n\r\n") {
                            break;
                        }
                        if buffer.len() > 64 * 1024 {
                            break;
                        }
                    }
                    if buffer.is_empty() {
                        return;
                    }
                    let body = b"hello over tls";
                    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                    let _ = tls.write_all(head.as_bytes()).await;
                    let _ = tls.write_all(body).await;
                    let _ = tls.flush().await;
                });
            }
        });
    });

    address
}

fn request() -> UpstreamRequest<Empty<Bytes>> {
    UpstreamRequest {
        method: hyper::Method::GET,
        uri: "/secure".parse().expect("a valid path"),
        version: hyper::Version::HTTP_11,
        headers: hyper::HeaderMap::new(),
        body: Empty::new(),
    }
}

fn https_origin(address: SocketAddr, tls: Option<UpstreamTls>) -> Origin {
    Origin::Network {
        base: format!("https://{address}").parse().expect("a URL"),
        tls,
    }
}

#[tokio::test]
async fn an_https_origin_signed_by_the_named_ca_is_reached() {
    let chain = chain("verified");
    let address = tls_origin(&chain);

    let origin = https_origin(
        address,
        Some(UpstreamTls {
            ca: Some(chain.root_path.clone()),
            sni: None,
        }),
    );
    let client: UpstreamClient<Empty<Bytes>> =
        UpstreamClient::new(origin, Some(Duration::from_secs(30))).expect("a client");

    let answer = client.request(request()).await.expect("a response");
    assert_eq!(answer.status, hyper::StatusCode::OK);
    let body = volucer_upstream::client::collect(answer.body)
        .await
        .expect("a body");
    assert_eq!(&body[..], b"hello over tls");
}

#[tokio::test]
async fn an_https_origin_signed_by_a_different_root_is_refused() {
    // **The test that makes "verification is not optional" a fact rather than a claim.** The origin
    // is the same, the request is the same, and only the trusted root differs — so a client that
    // installed no verifier would pass this, and a client that verifies fails it for exactly one
    // reason.
    let chain = chain("wrong-root");
    let address = tls_origin(&chain);

    let origin = https_origin(
        address,
        Some(UpstreamTls {
            ca: Some(chain.other_root_path.clone()),
            sni: None,
        }),
    );
    let client: UpstreamClient<Empty<Bytes>> =
        UpstreamClient::new(origin, Some(Duration::from_secs(30))).expect("a client");

    let error = client
        .request(request())
        .await
        .expect_err("a certificate that does not chain to the named root must be refused");

    let message = error.to_string();
    assert!(
        message.contains("TLS handshake") || message.contains("unreachable"),
        "the failure must be the handshake, not something else: {message}"
    );
    assert!(
        !error.is_timeout(),
        "a refused certificate is not a deadline: {error}"
    );
}

#[tokio::test]
async fn an_https_origin_with_no_ca_named_is_checked_against_the_public_roots() {
    // The plausible-wrong-default case: `ca` absent must mean "the public roots", not "no
    // verification". A self-signed fixture is not publicly trusted, so this must fail — and if it
    // ever starts passing, the default has quietly become insecure.
    let chain = chain("public-roots");
    let address = tls_origin(&chain);

    let origin = https_origin(
        address,
        Some(UpstreamTls {
            ca: None,
            sni: None,
        }),
    );
    let client: UpstreamClient<Empty<Bytes>> =
        UpstreamClient::new(origin, Some(Duration::from_secs(30))).expect("a client");

    let error = client
        .request(request())
        .await
        .expect_err("an untrusted certificate must not be accepted by default");

    assert!(!error.is_timeout(), "{error}");
}

#[tokio::test]
async fn a_ca_file_that_cannot_be_read_is_a_startup_error() {
    // A typo in a path must be a configuration failure with the path in it, not a 502 on the first
    // request at a moment when the operator is looking at the client instead.
    let origin = Origin::Network {
        base: "https://127.0.0.1:8443".parse().expect("a URL"),
        tls: Some(UpstreamTls {
            ca: Some(PathBuf::from("no/such/ca.pem")),
            sni: None,
        }),
    };

    let error = UpstreamClient::<Empty<Bytes>>::new(origin, None)
        .expect_err("an unreadable CA must be refused");

    let message = error.to_string();
    assert!(message.contains("no/such/ca.pem"), "{message}");
}

#[tokio::test]
async fn a_ca_file_with_no_certificate_in_it_is_a_startup_error() {
    // The other half: the file exists and is readable, so the failure cannot be "not found". An
    // empty `RootCertStore` is the state that would make every handshake fail for a reason nobody
    // could read off the configuration.
    let directory =
        std::env::temp_dir().join(format!("volucer-upstream-empty-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("a directory");
    let empty = directory.join("empty.pem");
    std::fs::write(&empty, b"# nothing here\n").expect("a file");

    let origin = Origin::Network {
        base: "https://127.0.0.1:8443".parse().expect("a URL"),
        tls: Some(UpstreamTls {
            ca: Some(empty.clone()),
            sni: None,
        }),
    };

    let error = UpstreamClient::<Empty<Bytes>>::new(origin, None)
        .expect_err("a CA file with no certificate must be refused");
    assert!(error.to_string().contains("no certificate"), "{error}");
}

#[test]
fn an_https_origin_is_not_reachable_without_a_configuration() {
    // `Origin::Network { tls: None }` with an `https` base is not constructible through `parse`,
    // and a caller that built it by hand must not get a silently plaintext connection to a TLS
    // port — the handshake would simply fail, which is what `dial` does with no config to give.
    let origin = Origin::Network {
        base: "https://127.0.0.1:8443".parse().expect("a URL"),
        tls: None,
    };
    assert!(!origin.is_tls());
}
