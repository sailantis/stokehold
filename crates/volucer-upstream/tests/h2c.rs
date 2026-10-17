//! Can the upstream client speak HTTP/2 to a plaintext origin?
//!
//! `docs/plugins.md` §6.6's first gRPC tier routes a call to a Unix-socket or `http://` origin, and gRPC
//! *is* HTTP/2. The measurement in `crates/volucer-upstream/tests/trailers.rs` found that
//! `negotiated_h2` is set from the ALPN result alone, so those origins are pinned to HTTP/1.1 today.
//! This is the other half of that question, and it is the one that decides how large tier A really is:
//! **if the connector said so, would hyper speak h2c?**
//!
//! It would. `Connected::negotiated_h2()` is the whole mechanism — `client-legacy` reads it off the
//! connector's response and dispatches to its HTTP/2 client instead of its HTTP/1.1 one — and the real
//! `ConnIo` already forwards `connected()` to whatever `Io` it wraps. So an h2c path for a plaintext
//! origin is a decision about *when* to answer `negotiated_h2`, not new machinery.
//!
//! **Both halves are asserted, and the negative one is what makes the positive one mean anything.** With
//! only the positive case, a client that always sent h2c would pass. And the assertion is on the *bytes
//! on the wire* — the connection preface — rather than on a version field hyper reported, because the
//! question is what hyper actually sent, and a version field would be hyper's opinion of itself.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Empty;
use hyper_util::client::legacy::connect::{Connected, Connection};

/// The HTTP/2 connection preface a client sends before anything else.
const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// A plaintext socket that does or does not claim to be HTTP/2.
///
/// The claim is the whole variable under test, so it is a field rather than two connectors: the two tests
/// then differ in exactly one thing, which is what makes the difference between their assertions
/// attributable.
struct PlainIo {
    stream: tokio::net::TcpStream,
    claims_h2: bool,
}

impl Connection for PlainIo {
    fn connected(&self) -> Connected {
        if self.claims_h2 {
            Connected::new().negotiated_h2()
        } else {
            Connected::new()
        }
    }
}

/// Forwarding to the socket, because hyper needs the IO traits as well as the `Connection` answer — and
/// the whole point of this type is that it carries the answer *without* changing the IO, so that the two
/// tests differ in nothing else.
impl tokio::io::AsyncRead for PlainIo {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for PlainIo {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

/// A connector that dials the URI and wraps it in [`PlainIo`], claiming h2 or not.
///
/// Hand-written rather than reusing `volucer_upstream::connect::Connector`, because the real one would
/// conflate two questions: "hyper cannot speak h2c" and "our connector does not say it can". A local
/// connector answers only the first.
#[derive(Clone, Copy)]
struct DialWith {
    claims_h2: bool,
}

impl tower_service::Service<hyper::Uri> for DialWith {
    type Response = hyper_util::rt::TokioIo<PlainIo>;
    type Error = std::io::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = std::io::Result<Self::Response>> + Send + 'static>,
    >;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::result::Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: hyper::Uri) -> Self::Future {
        let claims_h2 = self.claims_h2;
        Box::pin(async move {
            let host = uri.host().unwrap_or("127.0.0.1").to_string();
            let port = uri.port_u16().unwrap_or(80);
            let stream = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
            Ok(hyper_util::rt::TokioIo::new(PlainIo { stream, claims_h2 }))
        })
    }
}

/// An origin that records the first bytes of whatever a client sends.
struct PrefetchOrigin {
    address: std::net::SocketAddr,
    first_bytes: Receiver<Vec<u8>>,
    shutdown: Arc<Mutex<bool>>,
}

impl PrefetchOrigin {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
        let address = listener.local_addr().expect("an address");
        let shutdown = Arc::new(Mutex::new(false));
        let (sender, first_bytes) = channel();

        let stop = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if *stop.lock().expect("not poisoned") {
                    break;
                }
                let Ok(mut stream) = stream else { break };
                // Enough for the preface plus an HTTP/1 request line.
                let mut buffer = [0u8; 64];
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let read = stream.read(&mut buffer).unwrap_or(0);
                let _ = sender.send(buffer[..read].to_vec());
                // An empty SETTINGS frame, so a client that does speak h2 sees a server rather than a
                // reset — and so the *read* is not the thing under test.
                let _ = stream.write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0]);
                let _ = stream.flush();
            }
        });

        PrefetchOrigin {
            address,
            first_bytes,
            shutdown,
        }
    }

    fn waited(&self) -> Vec<u8> {
        self.first_bytes
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_default()
    }
}

impl Drop for PrefetchOrigin {
    fn drop(&mut self) {
        *self.shutdown.lock().expect("not poisoned") = true;
        let _ = TcpStream::connect(self.address);
    }
}

/// Send one gRPC-shaped POST and report what the origin saw.
///
/// The request is *discarded* — the fixture answers with a SETTINGS frame and nothing else, so the call
/// cannot complete — because the assertion is on what was sent rather than on the outcome. A test that
/// asserted on the response would be testing the fixture.
async fn what_was_sent(claims_h2: bool) -> Vec<u8> {
    let origin = PrefetchOrigin::spawn();
    let client: hyper_util::client::legacy::Client<DialWith, Empty<Bytes>> =
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build(DialWith { claims_h2 });

    let request = hyper::Request::builder()
        .method("POST")
        .uri(format!("http://{}/pkg.Service/Method", origin.address))
        .header("content-type", "application/grpc+proto")
        .header("te", "trailers")
        .body(Empty::<Bytes>::new())
        .expect("a request");
    let _ = tokio::time::timeout(Duration::from_secs(3), client.request(request)).await;

    origin.waited()
}

/// **The measurement.** A connector claiming h2 makes hyper open with the HTTP/2 preface.
#[tokio::test]
async fn claiming_h2_makes_hyper_speak_h2c_to_a_plaintext_origin() {
    let sent = what_was_sent(true).await;
    assert!(
        sent.starts_with(PREFACE),
        "a connection claiming h2 must open with the HTTP/2 preface.\n\
         expected it to start with {:?}\n\
         got {:?}",
        String::from_utf8_lossy(PREFACE),
        String::from_utf8_lossy(&sent)
    );
}

/// The control. Without the claim, the same client sends an HTTP/1.1 request line — which is what makes
/// the test above a measurement of the *claim* rather than of hyper's default.
#[tokio::test]
async fn without_the_claim_hyper_speaks_http_1_1() {
    let sent = what_was_sent(false).await;
    assert!(
        sent.starts_with(b"POST /pkg.Service/Method HTTP/1.1"),
        "an unclaimed connection must speak HTTP/1.1.\n\
         got {:?}",
        String::from_utf8_lossy(&sent)
    );
    assert!(
        !sent.starts_with(PREFACE),
        "and must not send the h2 preface"
    );
}
