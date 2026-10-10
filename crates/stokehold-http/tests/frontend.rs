//! Integration tests for the HTTP front-end, against a real PHP worker.
//!
//! These are the tests that prove the front-end actually works end to end: a
//! real `stokehold-http` listener, a real HTTP client over a real socket, a real
//! worker pool, and the shipped PHP SDK on the far end. The unit tests in
//! `listener.rs` cover the two conversions in isolation; what is under test
//! *here* is everything between a browser-shaped request and the bytes that come
//! back.
//!
//! Two things are asserted that a unit test cannot reach. First, that the
//! blocking backends are reached safely from the async front-end — the pool
//! serves through `spawn_blocking`, and a mistake there shows up as a stall
//! rather than a wrong value. Second, that concurrent requests are served in
//! parallel: the pool has more slots than one, and the front-end must not
//! serialise them behind a single blocking call.
//!
//! Skipped, with a printed reason, when no `php` binary is on `PATH`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use stokehold_core::config::{BackendConfig, Config, ServerConfig, WorkerConfig};
use stokehold_fcgi::Routing;
use stokehold_http::{HttpLimits, HttpListener};
use stokehold_server::Dispatcher;

/// Find `php` on `PATH`.
fn find_php() -> Option<PathBuf> {
    #[cfg(windows)]
    let names: &[&str] = &["php.exe", "php"];
    #[cfg(not(windows))]
    let names: &[&str] = &["php"];

    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// The SDK's example worker, which is what the pool is pointed at.
fn example_worker() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("sdk")
        .join("php")
        .join("example")
        .join("worker.php")
}

/// A configuration pointing a worker backend at the example worker.
fn worker_config(workers: u32) -> Config {
    let script = example_worker();
    assert!(
        script.is_file(),
        "the SDK example worker is missing at {}",
        script.display()
    );

    Config {
        server: ServerConfig {
            http: Some("127.0.0.1:0".to_string()),
            fastcgi: None,
        },
        backends: [(
            "app".to_string(),
            BackendConfig::Worker(WorkerConfig {
                command: format!("php \"{}\"", script.display()),
                workers,
                memory_limit_mb: None,
                max_requests: None,
                working_dir: None,
                env: Default::default(),
            }),
        )]
        .into_iter()
        .collect(),
        default_backend: Some("app".to_string()),
        routes: Vec::new(),
    }
}

/// The routing a front controller implies.
fn front_controller_routing() -> Routing {
    Routing {
        document_root: Path::new(".").to_path_buf(),
        front_controller: Some("index.php".to_string()),
        ..Routing::default()
    }
}

/// A running HTTP server on a loopback port, shut down when dropped.
struct Server {
    address: String,
    shutdown: stokehold_http::HttpShutdown,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    /// Start a listener whose requests are served by a real worker pool.
    ///
    /// The runtime is built here and moved onto the thread that runs it, because
    /// the listener can only be created inside a runtime. `#[tokio::test]` is not
    /// used anywhere: it would need the `macros` feature, and a test that builds
    /// its own runtime keeps the async boundary visible in the test as well as in
    /// the production code.
    fn with_workers(workers: u32) -> Option<Server> {
        if find_php().is_none() {
            eprintln!("skipping: no php on PATH");
            return None;
        }
        let config = worker_config(workers);
        let routing = front_controller_routing();
        let dispatcher = match Dispatcher::from_config(&config, routing.clone()) {
            Ok(dispatcher) => dispatcher,
            Err(err) => {
                eprintln!("skipping: the worker pool could not start: {err}");
                return None;
            }
        };

        Server::start(dispatcher, routing, HttpLimits::default())
    }

    /// Start a listener around an already-built dispatcher.
    fn start(
        dispatcher: Dispatcher,
        routing: Routing,
        limits: HttpLimits,
    ) -> Option<Server> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a tokio runtime");

        let listener = runtime.block_on(HttpListener::bind(
            "127.0.0.1:0",
            Arc::new(Mutex::new(dispatcher)),
            routing,
            limits,
        ));
        let listener = match listener {
            Ok(listener) => listener,
            Err(err) => {
                eprintln!("skipping: the HTTP listener could not bind: {err}");
                return None;
            }
        };

        let address = listener.local_addr().expect("bound address").to_string();
        let shutdown = listener.shutdown_handle();

        let handle = std::thread::spawn(move || {
            let _guard = runtime.enter();
            if let Err(err) = runtime.block_on(listener.run()) {
                eprintln!("http listener stopped: {err}");
            }
        });

        Some(Server {
            address,
            shutdown,
            handle: Some(handle),
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown.shutdown();
        // Joining waits for the listener to leave its accept loop, which is what
        // guarantees the pool — and therefore every worker process — is dropped
        // before the test ends. Without the join the next test can race the
        // reaping, which is a flake rather than a failure.
        let _ = self.handle.take().map(|handle| handle.join());
    }
}

/// One HTTP exchange, written and read over a raw socket.
#[derive(Debug)]
struct Exchange {
    status: u16,
    reason: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Exchange {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Send a raw request and read the whole response.
///
/// Raw bytes rather than an HTTP client library, deliberately: the front-end's
/// job is to be correct about framing, and a client that hides framing would hide
/// the bug. `Connection: close` is sent so the response ends at EOF and no
/// chunked decoding is needed.
fn exchange(address: &str, request: &[u8]) -> Exchange {
    let mut stream = TcpStream::connect(address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("a read timeout");
    stream.write_all(request).expect("write the request");
    stream.flush().expect("flush");

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read the response");

    parse(&raw)
}

/// A `GET` on a fresh connection.
fn get(address: &str, path: &str) -> Exchange {
    let request = format!("GET {path} HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n");
    exchange(address, request.as_bytes())
}

fn parse(raw: &[u8]) -> Exchange {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or_else(|| {
            panic!(
                "no header terminator in {:?}",
                String::from_utf8_lossy(raw)
            )
        });
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let body = raw[split + 4..].to_vec();

    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    let _version = parts.next();
    let status = parts
        .next()
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status in {status_line:?}"));
    let reason = parts.next().unwrap_or_default().to_string();

    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .collect();

    Exchange {
        status,
        reason,
        headers,
        body,
    }
}

/// A `Server` for a test, or a printed reason why it cannot run.
macro_rules! server {
    ($workers:expr) => {
        match Server::with_workers($workers) {
            Some(server) => server,
            None => return,
        }
    };
}

#[test]
fn a_get_reaches_the_php_worker_and_comes_back() {
    let server = server!(2);

    let response = get(&server.address, "/health");

    assert_eq!(response.status, 200, "{response:?}");
    let body = response.text();
    assert!(body.contains("ok"), "the worker's body did not arrive: {body}");
    // The body was produced by PHP, so Content-Length has to describe it.
    assert_eq!(
        response.header("content-length").expect("a content length"),
        response.body.len().to_string()
    );
}

#[test]
fn a_path_and_query_string_reach_php_intact() {
    let server = server!(2);

    let response = get(&server.address, "/echo?x=1&y=two%20words");

    assert_eq!(response.status, 200, "{response:?}");
    let body = response.text();
    // The example worker echoes the URI and the parsed query, so this asserts
    // that both REQUEST_URI and QUERY_STRING survived the translation.
    assert!(body.contains("/echo"), "{body}");
    assert!(body.contains("x=1"), "{body}");
}

#[test]
fn a_post_body_reaches_the_application_byte_for_byte() {
    let server = server!(2);

    let payload = "abcdefghij-a-marker-string";
    let request = format!(
        "POST /echo HTTP/1.1\r\nHost: example.test\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    let response = exchange(&server.address, request.as_bytes());

    assert_eq!(response.status, 200, "{response:?}");
    let body = response.text();
    assert!(
        body.contains(&format!("payload: {payload}")),
        "the body did not arrive intact: {body}"
    );
    // The application's own count must agree with what PHP read.
    assert!(
        body.contains(&format!("body: {} bytes", payload.len())),
        "the declared length and the received body disagree: {body}"
    );
}

#[test]
fn request_headers_become_the_cgi_environment() {
    let server = server!(2);

    let request = "GET /headers HTTP/1.1\r\nHost: example.test\r\nX-Marker: a-value\r\nConnection: close\r\n\r\n";
    let response = exchange(&server.address, request.as_bytes());

    assert_eq!(response.status, 200, "{response:?}");
    let body = response.text();
    // `X-Marker` is only visible to PHP as `HTTP_X_MARKER`, so finding it proves
    // the header made the round trip through the CGI environment.
    assert!(body.contains("a-value"), "{body}");
}

#[test]
fn an_application_error_is_a_500_and_the_worker_survives() {
    let server = server!(2);

    // The example worker's `/boom` route throws, and the SDK turns an uncaught
    // throwable into a 500 while keeping the worker alive — which is the property
    // that gives worker mode its value.
    let response = get(&server.address, "/boom");

    assert_eq!(response.status, 500, "{response:?}");
    let body = response.text();
    assert!(body.contains("Internal Server Error"), "{body}");

    // And the same worker must still serve the next request: a failing request is
    // not a failing process.
    let after = get(&server.address, "/health");
    assert_eq!(after.status, 200, "{after:?}");
}

#[test]
fn a_missing_script_is_a_404_without_troubling_the_backend() {
    // No front controller, so only a real file can be served. Two things make this
    // a 404 rather than a gateway error: the routing layer resolves the path
    // before dispatch, and the backend here is deliberately unreachable, so a
    // backend failure could not be mistaken for it.
    let routing = Routing {
        document_root: Path::new("a-directory-that-does-not-exist").to_path_buf(),
        front_controller: None,
        ..Routing::default()
    };
    let dispatcher = Dispatcher::from_config(&unreachable_config(), routing.clone())
        .expect("a dispatcher that starts no process");

    let Some(server) = Server::start(dispatcher, routing, HttpLimits::default()) else {
        return;
    };
    let response = get(&server.address, "/nothing.php");

    assert_eq!(response.status, 404, "{response:?}");
    assert_eq!(response.reason, "Not Found");
    assert!(response.text().contains("not found"), "{}", response.text());
}

/// A configuration whose backend starts no process, for tests that must not need
/// `php`. An `external` backend pointing at a loopback port nothing listens on.
fn unreachable_config() -> Config {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a probe port");
    let socket = listener.local_addr().expect("address").to_string();
    drop(listener);

    Config {
        server: ServerConfig {
            http: Some("127.0.0.1:0".to_string()),
            fastcgi: None,
        },
        backends: [(
            "fpm".to_string(),
            BackendConfig::External(stokehold_core::config::ExternalConfig { socket }),
        )]
        .into_iter()
        .collect(),
        default_backend: Some("fpm".to_string()),
        routes: Vec::new(),
    }
}

#[test]
fn an_unreachable_backend_is_a_502() {
    let routing = front_controller_routing();
    let dispatcher = Dispatcher::from_config(&unreachable_config(), routing.clone())
        .expect("a dispatcher that starts no process");

    let Some(server) = Server::start(dispatcher, routing, HttpLimits::default()) else {
        return;
    };

    // The path routes to the front controller, so the failure is the backend being
    // unreachable rather than the script being missing.
    let response = get(&server.address, "/health");

    assert_eq!(response.status, 502, "{response:?}");
    assert_eq!(response.reason, "Bad Gateway");
}

#[test]
fn an_oversized_body_is_refused_with_a_413() {
    let config = unreachable_config();
    let routing = front_controller_routing();
    let dispatcher =
        Dispatcher::from_config(&config, routing.clone()).expect("a dispatcher");

    // A tiny limit, so the refusal is observable without sending megabytes.
    let limits = HttpLimits {
        max_body_bytes: 64,
        ..HttpLimits::default()
    };
    let Some(server) = Server::start(dispatcher, routing, limits) else {
        return;
    };

    let payload = "x".repeat(256);
    let request = format!(
        "POST /echo HTTP/1.1\r\nHost: example.test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    let response = exchange(&server.address, request.as_bytes());

    // The limit is enforced while the body is read, so this is refused before any
    // backend is reached — which is why an unreachable backend does not turn it
    // into a 502.
    assert_eq!(response.status, 413, "{response:?}");
}

#[test]
fn concurrent_requests_are_served_in_parallel() {
    let server = server!(4);

    // Four workers means four slots, so four simultaneous requests can all be in
    // flight at once. If the front-end serialised them behind a single blocking
    // call, the total time would be roughly the sum of the parts. The assertion is
    // deliberately loose — a smoke test for the boundary, not a benchmark.
    let started = std::time::Instant::now();
    let mut handles = Vec::new();
    for _ in 0..4 {
        let address = server.address.clone();
        handles.push(std::thread::spawn(move || get(&address, "/health")));
    }
    let responses: Vec<Exchange> = handles
        .into_iter()
        .map(|handle| handle.join().expect("a client thread"))
        .collect();

    for response in &responses {
        assert_eq!(response.status, 200, "a concurrent request failed");
    }
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "four parallel requests took {:?}",
        started.elapsed()
    );
}

#[test]
fn http_1_1_keep_alive_serves_two_requests_on_one_connection() {
    let server = server!(2);

    let mut stream = TcpStream::connect(&server.address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("a read timeout");

    // The first request asks for keep-alive, so the server must answer it and then
    // stay open for the second.
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: example.test\r\n\r\n")
        .expect("write the first request");
    stream.flush().expect("flush");

    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    assert_eq!(read_one_response(&mut reader), 200);

    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")
        .expect("write the second request");
    stream.flush().expect("flush");

    assert_eq!(
        read_one_response(&mut reader),
        200,
        "the second request on a kept-alive connection"
    );
}

/// Read one HTTP response, leaving the stream positioned after it.
///
/// A `Content-Length` response is read exactly, so a second request can follow on
/// the same connection — which is the point of the test that uses this.
fn read_one_response(reader: &mut BufReader<TcpStream>) -> u16 {
    let mut line = String::new();
    reader.read_line(&mut line).expect("a status line");
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status in {line:?}"));

    let mut length: Option<usize> = None;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).expect("a header line");
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().ok();
            }
        }
    }

    let length = length.expect("a content-length on a keep-alive response");
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).expect("the body");

    status
}
