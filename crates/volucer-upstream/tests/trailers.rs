//! Do an origin's HTTP trailers survive the proxy?
//!
//! `docs/plugins.md` §6.6 rests on the claim that they do, and on it rests the argument that the
//! first gRPC tier is small: an origin's trailers are already carried, so what is missing is only
//! the code that **constructs** one. That claim was derived by reading three files —
//! `incoming_body` preserving frames, `translate_response` never touching them, and the front-end
//! handing the body straight out — and reading three files is not a measurement.
//!
//! So this is the measurement. A gRPC status *is* a trailer, which is what makes it load-bearing.
//!
//! The origin is hand-written for the same reason it is in `origin.rs`: a fixture that needs no
//! interpreter and can be told to send exactly one odd thing is a better test of framing than a
//! real application. Nothing here is skipped when it cannot run — there is no external service, so
//! a skip would be a disguise for a broken test.

use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use volucer_upstream::{Origin, UpstreamClient};

/// An origin that answers every connection with one canned response, then stops.
struct TestOrigin {
    address: std::net::SocketAddr,
    /// Kept so the port is held for the test's lifetime, and so a `Drop` can unblock the accept
    /// loop rather than leaving the thread alive for the whole test binary.
    shutdown: Arc<Mutex<bool>>,
    _answers: Receiver<()>,
}

impl TestOrigin {
    fn spawn(response: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
        let address = listener.local_addr().expect("an address");
        let shutdown = Arc::new(Mutex::new(false));
        let (sender, answers) = channel();

        let stop = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if *stop.lock().expect("not poisoned") {
                    break;
                }
                let Ok(mut stream) = stream else { break };
                if serve_one(&mut stream, &response).is_ok() {
                    let _ = sender.send(());
                }
            }
        });

        TestOrigin {
            address,
            shutdown,
            _answers: answers,
        }
    }
}

impl Drop for TestOrigin {
    fn drop(&mut self) {
        *self.shutdown.lock().expect("not poisoned") = true;
        let _ = TcpStream::connect(self.address);
    }
}

/// Read a request head off the socket, then write the canned response.
fn serve_one(stream: &mut TcpStream, response: &str) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);

    // The head only: the request body in every case here is empty, so reading to the blank line is
    // enough and a `Content-Length` dance would be fixture machinery nothing asserts on.
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 || line.trim_end().is_empty() {
            break;
        }
    }

    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(())
}

/// A `Transfer-Encoding: chunked` response whose trailer section carries `trailers`.
///
/// `Trailer` is sent as a header on purpose. It is in `translate::NEVER_FORWARDED`, so the proxy
/// strips the *declaration* — and the point of this fixture is that the fields themselves still
/// arrive, because they travel as frames and not as headers. A test that omitted the declaration
/// would not exercise that.
fn chunked_with_trailers(body: &str, trailers: &[(&str, &str)]) -> String {
    let mut out = String::from("HTTP/1.1 200 OK\r\nContent-Type: application/grpc\r\n");
    if !trailers.is_empty() {
        let names: Vec<&str> = trailers.iter().map(|(name, _)| *name).collect();
        out.push_str(&format!("Trailer: {}\r\n", names.join(", ")));
    }
    out.push_str("Transfer-Encoding: chunked\r\n\r\n");
    out.push_str(&format!("{:x}\r\n{body}\r\n", body.len()));
    // The terminating chunk, then the trailer section, then the blank line that ends it.
    out.push_str("0\r\n");
    for (name, value) in trailers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("\r\n");
    out
}

fn client(origin: &TestOrigin) -> UpstreamClient<Empty<Bytes>> {
    let origin = Origin::parse(&format!("http://{}", origin.address)).expect("a usable origin");
    UpstreamClient::new(origin, Some(Duration::from_secs(30))).expect("a client")
}

/// The **whole** path, in the order the front-end runs it: translate in, send, translate out.
///
/// Not `client.request` alone — `translate_response` is half of what is being claimed, and a test
/// that called the client directly would assert the output of a function it never ran.
async fn exchange(origin: &TestOrigin) -> volucer_upstream::UpstreamResponse {
    let client = client(origin);
    let peer: Option<IpAddr> = Some("203.0.113.7".parse().expect("an address"));
    let request = volucer_upstream::prepare_request(
        hyper::HeaderMap::new(),
        hyper::Method::POST,
        "/pkg.Service/Method".parse().expect("a valid path"),
        hyper::Version::HTTP_11,
        Empty::<Bytes>::new(),
        peer,
        true,
    );
    let answer = client.request(request).await.expect("a response");
    volucer_upstream::translate_response(answer)
}

#[tokio::test]
async fn a_plaintext_origins_trailers_reach_the_caller() {
    let origin = TestOrigin::spawn(chunked_with_trailers(
        "grpc-payload",
        &[("grpc-status", "0"), ("grpc-message", "OK")],
    ));

    let answer = exchange(&origin).await;

    // The declaration is stripped deliberately — it is in the hop-by-hop list — so its absence is
    // asserted rather than assumed, because "stripped" and "the fields were lost with it" are the
    // two readings this test exists to tell apart.
    assert!(
        !answer.headers.contains_key("trailer"),
        "the Trailer declaration should be stripped, and the fields should not go with it"
    );
    assert_eq!(answer.status, hyper::StatusCode::OK);

    let collected = answer.body.collect().await.expect("the body to arrive");

    // Taken before `to_bytes`, which consumes.
    let trailers = collected
        .trailers()
        .expect("the origin's trailers to have crossed the proxy");
    assert_eq!(
        trailers.get("grpc-status").expect("grpc-status").as_bytes(),
        b"0",
        "and to be the origin's own, not a placeholder"
    );
    assert_eq!(
        trailers
            .get("grpc-message")
            .expect("grpc-message")
            .as_bytes(),
        b"OK"
    );

    // The data is still the data: a trailer carried at the cost of the body would be a different
    // bug wearing the same green test.
    assert_eq!(collected.to_bytes().as_ref(), b"grpc-payload");
}

/// The control. Without this, a `trailers()` that returned `Some` for any reason at all would pass
/// the test above — which is the shape of hazard 67: a verification that cannot fail is not a
/// verification.
#[tokio::test]
async fn a_response_with_no_trailers_reports_none() {
    let origin = TestOrigin::spawn(chunked_with_trailers("grpc-payload", &[]));

    let answer = exchange(&origin).await;
    let collected = answer.body.collect().await.expect("the body to arrive");

    assert!(
        collected.trailers().is_none(),
        "a chunked response with an empty trailer section has no trailers"
    );
    assert_eq!(collected.to_bytes().as_ref(), b"grpc-payload");
}
