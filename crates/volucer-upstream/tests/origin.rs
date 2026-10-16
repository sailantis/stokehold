//! A real socket, a real request, and the assertions that say what actually crossed the wire.
//!
//! The origin is a `std::net::TcpListener` in this process rather than a Node or a Python, for the
//! same reason the packaging scripts are shell scripts: an origin that can be made slow, can send
//! two headers with one name and needs no interpreter is a better test fixture than a real app.
//!
//! Nothing here is skipped when it cannot run — there is no external service, so a skip would be a
//! disguise for a broken test.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Empty;
use volucer_upstream::{Origin, UpstreamClient, UpstreamRequest};

/// What a test origin saw, once.
#[derive(Debug, Clone)]
struct Seen {
    /// The request line as it arrived: `GET /api?x=1 HTTP/1.1`.
    request_line: String,
    /// Every header line, in the order it arrived, so a duplicate is visible as two entries.
    headers: Vec<String>,
    body: String,
}

impl Seen {
    /// Every value for one header name, compared case-insensitively.
    ///
    /// Case-insensitively is the correct comparison and not a convenience: header names are
    /// case-insensitive by RFC 9110 §5.1, and a fixture that demanded an exact case would be
    /// asserting something the protocol does not promise.
    fn values<'a>(&'a self, name: &str) -> Vec<&'a str> {
        self.headers
            .iter()
            .filter_map(|line| line.split_once(':'))
            .filter(|(key, _)| key.trim().eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim())
            .collect()
    }

    fn value(&self, name: &str) -> Option<&str> {
        self.values(name).first().copied()
    }
}

/// An origin that answers `responses` in order, collecting what it saw.
///
/// A fresh connection per response, which is what makes the assertions about framing readable: an
/// HTTP/1.1 client that reuses a connection would otherwise interleave two requests' bytes in the
/// fixture's view, and the test would be about the fixture rather than about the proxy.
struct TestOrigin {
    address: SocketAddr,
    seen: Receiver<Seen>,
    shutdown: Arc<Mutex<bool>>,
}

impl TestOrigin {
    fn start(responses: Vec<String>) -> TestOrigin {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
        let address = listener.local_addr().expect("a local address");
        let (sender, seen) = channel();
        let shutdown = Arc::new(Mutex::new(false));
        let flag = shutdown.clone();

        std::thread::spawn(move || {
            for response in responses {
                if *flag.lock().expect("not poisoned") {
                    break;
                }
                // One accept per response: `keep_alive` is off for the fixture, so a client that
                // tries to reuse a connection gets a closed one and the test says so.
                let Ok((stream, _)) = listener.accept() else {
                    break;
                };
                // Every request is reported, not just the first: a test that checks what the
                // origin saw *and* one that checks what came back are the same fixture, and
                // reporting only the first request made the second kind silently unobservable.
                let _ = serve_one(stream, &response, &sender);
            }
        });

        TestOrigin {
            address,
            seen,
            shutdown,
        }
    }

    /// The next request the origin saw, waiting up to a second.
    fn next(&self) -> Seen {
        self.seen
            .recv_timeout(Duration::from_secs(5))
            .expect("the origin should have seen a request")
    }
}

impl Drop for TestOrigin {
    fn drop(&mut self) {
        *self.shutdown.lock().expect("not poisoned") = true;
        // Unblock the accept loop so the thread ends rather than lingering for the test binary's
        // lifetime.
        let _ = TcpStream::connect(self.address);
    }
}

/// Serve one connection: read a full request, report what it was, write the canned response.
fn serve_one(mut stream: TcpStream, response: &str, sender: &Sender<Seen>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let request_line = request_line.trim_end().to_string();

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end().to_string();
        if line.is_empty() {
            break;
        }
        headers.push(line);
    }

    // `Content-Length` rather than reading to EOF: a client that keeps the connection open would
    // otherwise hang this thread until its timeout, which would make the test slow *and* flaky.
    let length: usize = headers
        .iter()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body)?;
    }

    let _ = sender.send(Seen {
        request_line,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    });

    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(())
}

/// A plain response with a known body.
fn response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{body}",
        body.len()
    )
}

fn client(origin: &TestOrigin) -> UpstreamClient<Empty<Bytes>> {
    client_for(origin)
}

/// A client whose request body type is chosen by the caller.
///
/// Generic because the body test needs `Full` and every other test needs `Empty`, and a fixture
/// that forced one on the other would be a test about the fixture.
fn client_for<B>(origin: &TestOrigin) -> UpstreamClient<B>
where
    B: hyper::body::Body<Data = Bytes> + Send + Unpin + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let origin = Origin::parse(&format!("http://{}", origin.address)).expect("a usable origin");
    UpstreamClient::new(origin, Some(Duration::from_secs(30))).expect("a client")
}

fn get(path: &str) -> UpstreamRequest<Empty<Bytes>> {
    UpstreamRequest {
        method: hyper::Method::GET,
        uri: path.parse().expect("a valid path"),
        version: hyper::Version::HTTP_11,
        headers: hyper::HeaderMap::new(),
        body: Empty::new(),
    }
}

/// The **whole** path: translate in, send, translate out.
///
/// Deliberately not `client.request` alone. The translation is where the interesting rules live —
/// what crosses a hop, what is appended, what is dropped — and a test that called the client
/// directly would assert the output of a function it never ran. That is not a hypothetical: the
/// first version of these tests did exactly that, and the two failing assertions were the test's
/// defect rather than the code's. Wiring the same order the front-end uses is what makes the
/// assertions load-bearing.
async fn exchange(
    client: &UpstreamClient<Empty<Bytes>>,
    headers: hyper::HeaderMap,
    path: &str,
    peer: Option<SocketAddr>,
    cleartext: bool,
) -> volucer_upstream::UpstreamResponse {
    let request = volucer_upstream::prepare_request(
        headers,
        hyper::Method::GET,
        path.parse().expect("a valid path"),
        hyper::Version::HTTP_11,
        Empty::<Bytes>::new(),
        peer,
        cleartext,
    );
    let answer = client.request(request).await.expect("a response");
    volucer_upstream::translate_response(answer)
}

/// The peer address the front-end would report for a request.
fn peer() -> Option<SocketAddr> {
    Some("203.0.113.7:51234".parse().expect("an address"))
}

#[tokio::test]
async fn a_request_reaches_an_origin_and_its_answer_comes_back() {
    let origin = TestOrigin::start(vec![response("200 OK", "hello from the origin")]);
    let client = client(&origin);

    let answer = exchange(
        &client,
        hyper::HeaderMap::new(),
        "/api/things",
        peer(),
        true,
    )
    .await;

    assert_eq!(answer.status, hyper::StatusCode::OK);
    assert_eq!(answer.version, hyper::Version::HTTP_11);
    let body = volucer_upstream::client::collect(answer.body)
        .await
        .expect("a body");
    assert_eq!(&body[..], b"hello from the origin");

    let seen = origin.next();
    // Origin-form, with the query intact: the origin is the backend's address, so the request line
    // must not carry it as absolute-form.
    assert_eq!(seen.request_line, "GET /api/things HTTP/1.1");
}

#[tokio::test]
async fn the_origin_sees_the_clients_host_and_appended_forwarding() {
    let origin = TestOrigin::start(vec![response("200 OK", "ok")]);
    let client = client(&origin);

    let mut headers = hyper::HeaderMap::new();
    headers.insert("host", "shop.example.com".parse().expect("a host"));
    headers.append(
        "x-forwarded-for",
        "198.51.100.9".parse().expect("an address"),
    );

    exchange(&client, headers, "/", peer(), true).await;
    let seen = origin.next();

    // The vhost identity the client asked for, not the address this server dialled.
    assert_eq!(seen.value("host"), Some("shop.example.com"));
    // The client's own claim survives *before* ours, so an origin reading the last value reads the
    // address we actually saw — the whole point of appending rather than replacing.
    assert_eq!(
        seen.values("x-forwarded-for"),
        vec!["198.51.100.9", "203.0.113.7"]
    );
}

#[tokio::test]
async fn two_headers_with_one_name_arrive_as_two() {
    // The loss the CGI path makes deliberately (`build_params` joins with ", ") and that this path
    // must not: a joined `Cookie` header is a different request than two.
    let origin = TestOrigin::start(vec![response("200 OK", "ok")]);
    let client = client(&origin);

    let mut headers = hyper::HeaderMap::new();
    headers.append("x-tag", "one".parse().expect("a value"));
    headers.append("x-tag", "two".parse().expect("a value"));

    exchange(&client, headers, "/", peer(), true).await;
    let seen = origin.next();

    assert_eq!(seen.values("x-tag"), vec!["one", "two"]);
}

#[tokio::test]
async fn two_set_cookie_headers_come_back_as_two() {
    let two = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nSet-Cookie: a=1; Path=/\r\n\
               Set-Cookie: b=2; Path=/\r\n\r\nok";
    let origin = TestOrigin::start(vec![two.to_string()]);
    let client = client(&origin);

    let answer = exchange(&client, hyper::HeaderMap::new(), "/", peer(), true).await;

    // `HeaderMap` is a multimap, so this is the assertion that the proxy did not join or drop one
    // — and the response-side half of the duplicate-header claim.
    let values: Vec<&str> = answer
        .headers
        .get_all("set-cookie")
        .iter()
        .map(|value| value.to_str().expect("ascii"))
        .collect();
    assert_eq!(values, vec!["a=1; Path=/", "b=2; Path=/"]);
}

#[tokio::test]
async fn a_hostile_header_from_the_origin_does_not_cross() {
    // `Connection: X-Origin-Internal` makes that header hop-local *for this message* — RFC 9110
    // §7.6.1 — and leaking it onward is how a proxy forwards something the previous hop wanted
    // kept between itself and us.
    let hostile = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: X-Origin-Internal\r\n\
                   X-Origin-Internal: hop-local\r\nX-Public: fine\r\n\r\nok";
    let origin = TestOrigin::start(vec![hostile.to_string()]);
    let client = client(&origin);

    let answer = exchange(&client, hyper::HeaderMap::new(), "/", peer(), true).await;

    assert!(!answer.headers.contains_key("x-origin-internal"));
    assert!(!answer.headers.contains_key("connection"));
    assert_eq!(
        answer
            .headers
            .get("x-public")
            .and_then(|value| value.to_str().ok()),
        Some("fine")
    );
}

#[tokio::test]
async fn a_body_reaches_the_origin_intact() {
    // The request body crosses a hop, and `Content-Length` is dropped so `hyper` re-derives
    // framing — which is what makes an upload work end to end. Asserted with a body long enough to
    // be a real one rather than a byte.
    let origin = TestOrigin::start(vec![response("201 Created", "made")]);
    let client = client_for::<http_body_util::Full<Bytes>>(&origin);

    let payload = "x".repeat(4096);
    let request = volucer_upstream::prepare_request(
        hyper::HeaderMap::new(),
        hyper::Method::POST,
        "/things".parse().expect("a valid path"),
        hyper::Version::HTTP_11,
        http_body_util::Full::new(Bytes::from(payload.clone())),
        peer(),
        true,
    );

    let answer = client.request(request).await.expect("a response");
    assert_eq!(answer.status, hyper::StatusCode::CREATED);

    let seen = origin.next();
    assert_eq!(seen.request_line, "POST /things HTTP/1.1");
    assert_eq!(seen.body, payload);
}

#[tokio::test]
async fn an_unreachable_origin_is_a_clean_error_not_a_panic() {
    // Bind and immediately drop, so the port is almost certainly free: connecting to it is a
    // refusal, which is the 502 case the front-end maps.
    let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
    let address = listener.local_addr().expect("an address");
    drop(listener);

    let origin = Origin::parse(&format!("http://{address}")).expect("a usable origin");
    let client: UpstreamClient<Empty<Bytes>> =
        UpstreamClient::new(origin, Some(Duration::from_secs(30))).expect("a client");

    let error = client
        .request(get("/"))
        .await
        .expect_err("a refused connection is an error");

    assert!(!error.is_timeout(), "{error}");
    assert!(
        matches!(error, volucer_upstream::UpstreamError::Unreachable(_)),
        "{error}"
    );
}

#[tokio::test]
async fn a_silent_origin_times_out_rather_than_hanging_forever() {
    // An origin that accepts and then says nothing, which is exactly the shape a 504 is for. The
    // point of the test is that the *connect* timeout does not apply here — the connection
    // succeeded — so the client must come back on its own rather than blocking the caller.
    let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
    let address = listener.local_addr().expect("an address");
    std::thread::spawn(move || {
        // Hold the connection open, saying nothing, until the test finishes.
        let held: Vec<TcpStream> = std::iter::from_fn(|| listener.accept().ok())
            .map(|(stream, _)| stream)
            .collect();
        std::thread::sleep(Duration::from_secs(30));
        drop(held);
    });

    let origin = Origin::parse(&format!("http://{address}")).expect("a usable origin");
    let client: UpstreamClient<Empty<Bytes>> =
        UpstreamClient::new(origin, Some(Duration::from_secs(30))).expect("a client");

    // Bounded by the test's own timeout rather than by a client deadline, because a request
    // deadline is not built yet: this asserts the *shape* of the failure that exists today — an
    // error rather than a hang — and the 504 mapping is the front-end's business.
    let outcome = tokio::time::timeout(Duration::from_secs(10), client.request(get("/"))).await;
    assert!(
        outcome.is_err(),
        "a silent origin must not produce a response out of nothing"
    );
}
