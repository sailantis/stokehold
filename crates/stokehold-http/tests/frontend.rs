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

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use stokehold_core::config::{BackendConfig, Config, ServerConfig, WorkerConfig};
use stokehold_core::sites::Sites;
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

/// The SDK the example worker lives in, which is now a repository of its own.
///
/// `STOKEHOLD_SDK_DIR` overrides it; otherwise the sibling layout is assumed.
fn sdk_dir() -> PathBuf {
    match std::env::var_os("STOKEHOLD_SDK_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("..")
            .join("stokehold-php-sdk"),
    }
}

/// The SDK's example worker, which is what the pool is pointed at.
fn example_worker() -> PathBuf {
    sdk_dir().join("example").join("worker.php")
}

/// Why this test cannot run here, if it cannot.
fn skip_reason() -> Option<String> {
    if find_php().is_none() {
        return Some("no php on PATH".to_string());
    }

    let script = example_worker();
    if !script.is_file() {
        return Some(format!(
            "the SDK example worker is not at {} — set STOKEHOLD_SDK_DIR to a \
             stokehold-php-sdk checkout",
            script.display()
        ));
    }

    None
}

/// Report that this test cannot run here.
///
/// libtest hides the stderr of a passing test, so a skip is invisible in an
/// ordinary run. Set `STOKEHOLD_E2E=1` where a skip should be a failure.
fn report_skip(reason: String) {
    if skip_is_a_failure() {
        panic!("STOKEHOLD_E2E is set, so a skipped test is a failure: {reason}");
    }
    eprintln!("skipping: {reason}");
}

/// Whether `STOKEHOLD_E2E` asks for skips to be failures. `0` or empty means no.
fn skip_is_a_failure() -> bool {
    match std::env::var("STOKEHOLD_E2E") {
        Ok(value) => !value.is_empty() && value != "0",
        Err(_) => false,
    }
}

/// A configuration pointing a worker backend at the example worker.
///
/// Callers check `skip_reason()` first, so reaching the assertion below means
/// something asked for this config without one — which is a test bug, not an
/// environment fact, and has to fail loudly rather than skip.
fn worker_config(workers: u32) -> Config {
    let script = example_worker();
    assert!(
        script.is_file(),
        "the SDK example worker is missing at {} — set STOKEHOLD_SDK_DIR to a \
         stokehold-php-sdk checkout",
        script.display()
    );

    Config {
        server: ServerConfig {
            http: Some("127.0.0.1:0".to_string()),
            ..Default::default()
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
                checksum: Default::default(),
                transport: Default::default(),
            }),
        )]
        .into_iter()
        .collect(),
        default_backend: Some("app".to_string()),
        routes: Vec::new(),
        sites: BTreeMap::new(),
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
        if let Some(reason) = skip_reason() {
            report_skip(reason);
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

        Server::start(
            dispatcher,
            routing,
            HttpLimits::default(),
            Default::default(),
        )
    }

    /// Start a listener around an already-built dispatcher.
    fn start(
        dispatcher: Dispatcher,
        routing: Routing,
        limits: HttpLimits,
        statics: stokehold_core::config::StaticConfig,
    ) -> Option<Server> {
        Server::start_sites(dispatcher, Arc::new(sites_for(&routing, statics)), limits)
    }

    /// Start a listener around an explicit site table, for tests that serve more than one.
    fn start_sites(
        dispatcher: Dispatcher,
        sites: Arc<Sites>,
        limits: HttpLimits,
    ) -> Option<Server> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a tokio runtime");

        let listener = runtime.block_on(HttpListener::bind(
            "127.0.0.1:0",
            Arc::new(dispatcher),
            sites,
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

/// A `GET` naming the host, so a test can drive two sites through one listener.
fn get_as(address: &str, host: &str, path: &str) -> Exchange {
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    exchange(address, request.as_bytes())
}

fn parse(raw: &[u8]) -> Exchange {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no header terminator in {:?}", String::from_utf8_lossy(raw)));
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
    assert!(
        body.contains("ok"),
        "the worker's body did not arrive: {body}"
    );
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

    let Some(server) = Server::start(
        dispatcher,
        routing,
        HttpLimits::default(),
        Default::default(),
    ) else {
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
            ..Default::default()
        },
        backends: [(
            "fpm".to_string(),
            BackendConfig::External(stokehold_core::config::ExternalConfig { socket }),
        )]
        .into_iter()
        .collect(),
        default_backend: Some("fpm".to_string()),
        routes: Vec::new(),
        sites: BTreeMap::new(),
    }
}

#[test]
fn an_unreachable_backend_is_a_502() {
    let routing = front_controller_routing();
    let dispatcher = Dispatcher::from_config(&unreachable_config(), routing.clone())
        .expect("a dispatcher that starts no process");

    let Some(server) = Server::start(
        dispatcher,
        routing,
        HttpLimits::default(),
        Default::default(),
    ) else {
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
    let dispatcher = Dispatcher::from_config(&config, routing.clone()).expect("a dispatcher");

    // A tiny limit, so the refusal is observable without sending megabytes.
    let limits = HttpLimits {
        max_body_bytes: 64,
        ..HttpLimits::default()
    };
    let Some(server) = Server::start(dispatcher, routing, limits, Default::default()) else {
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

// --------------------------------------------------------- static content
//
// The static layer needs no PHP at all, which is most of the point: these tests
// start a listener over an *unreachable* backend, so a request answered correctly
// can only have been answered by the file server. A request for a path that names
// no file must still reach the backend, and that is what the 502s below prove.

/// A document root with the files a static test needs.
fn static_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "stokehold-http-static-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("assets")).expect("create the root");
    std::fs::write(root.join("index.html"), "<h1>home</h1>").expect("write index.html");
    std::fs::write(root.join("assets/app.css"), "body { margin: 0 }").expect("write app.css");
    // A directory needs an index for the trailing-slash redirect to mean
    // anything: without one there is nothing to serve at `/assets/`.
    std::fs::write(root.join("assets/index.html"), "<h1>assets</h1>").expect("write assets index");
    std::fs::write(root.join("assets/app.js"), "console.log(1)").expect("write app.js");
    std::fs::write(root.join("assets/app.js.br"), "brotli-bytes").expect("write app.js.br");
    std::fs::write(root.join("assets/logo.svg"), "<svg/>").expect("write logo.svg");
    std::fs::write(root.join(".env"), "SECRET=1").expect("write a dotfile");
    // A script, so a test can prove this layer refuses to hand one over. Its
    // contents are what a client would receive if that ever regressed.
    std::fs::write(root.join("index.php"), "<?php echo 'SECRET-SOURCE';")
        .expect("write a front controller");
    std::fs::write(root.join("big.bin"), vec![b'x'; 256 * 1024]).expect("write a binary file");
    root
}

/// A server that serves files from `root`, with a backend nothing can reach.
fn static_server(root: &Path, config: stokehold_core::config::StaticConfig) -> Option<Server> {
    let routing = Routing {
        document_root: root.to_path_buf(),
        front_controller: Some("index.php".to_string()),
        ..Routing::default()
    };
    let dispatcher = Dispatcher::from_config(&unreachable_config(), routing.clone())
        .expect("a dispatcher that starts no process");

    Server::start(dispatcher, routing, HttpLimits::default(), config)
}

/// The single default site a test's routing and static settings describe.
///
/// The listener takes a [`Sites`] table because that is what `serve` builds, and a test
/// exercising the same path should build one the same way rather than through a side door.
fn sites_for(routing: &Routing, statics: stokehold_core::config::StaticConfig) -> Sites {
    let config = stokehold_core::config::Config::from_toml(&toml_for(routing, &statics))
        .expect("the test's own configuration parses");
    Sites::from_config(&config, &routing.document_root)
}

fn toml_for(routing: &Routing, statics: &stokehold_core::config::StaticConfig) -> String {
    let root = routing.document_root.to_string_lossy();
    // Everything lives on `[server]` — the default site — except "files only", which the
    // global `front_controller` cannot spell: it reserves the empty value, because TOML
    // cannot tell `""` from an absent key. A site may say `false`, so the one case that
    // needs it is expressed as a site naming the host these tests send.
    let files_only = match routing.front_controller.as_deref() {
        Some(path) => format!("front_controller = {path:?}\n"),
        None => String::new(),
    };
    let site = match routing.front_controller {
        Some(_) => String::new(),
        None => format!(
            "\n[sites.test]\nhosts = [\"example.test\", \"localhost\"]\n\
             document_root = {root:?}\nfront_controller = false\n"
        ),
    };
    let text = format!(
        "[server]\nhttp = \"127.0.0.1:0\"\ndocument_root = {root:?}\n{files_only}\
         static_files = {{ enabled = {enabled}, precompressed = {precompressed}, \
         directory_index = {directory_index} }}\n{site}",
        enabled = statics.enabled,
        precompressed = statics.precompressed,
        directory_index = statics.directory_index,
    );
    text
}

/// The static configuration a test wants, with serving switched on.
fn static_config() -> stokehold_core::config::StaticConfig {
    stokehold_core::config::StaticConfig {
        enabled: true,
        ..Default::default()
    }
}

/// A request with arbitrary headers, ending at EOF.
fn request_with(address: &str, request: &str) -> Exchange {
    exchange(address, request.as_bytes())
}

#[test]
fn a_file_is_served_without_a_backend() {
    let root = static_root("serve");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    let response = get(&server.address, "/assets/app.css");

    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(response.text(), "body { margin: 0 }");
    assert_eq!(
        response.header("content-type"),
        Some("text/css; charset=utf-8")
    );
    assert_eq!(
        response.header("content-length"),
        Some(response.body.len().to_string().as_str())
    );
    assert_eq!(response.header("accept-ranges"), Some("bytes"));
    assert_eq!(
        response.header("cache-control"),
        Some("public, max-age=3600")
    );
    assert!(
        response.header("etag").is_some(),
        "a file needs a validator to be cacheable"
    );
    assert!(response.header("last-modified").is_some());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_path_that_names_no_file_still_reaches_the_backend() {
    let root = static_root("fallthrough");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    // `/about` is not a file, so it goes to the front controller — and the
    // backend here is unreachable, which is how this test proves the request
    // really was dispatched rather than answered from the filesystem.
    let response = get(&server.address, "/about");
    assert_eq!(response.status, 502, "{response:?}");

    // A real PHP file is not the static layer's business either: it is a script,
    // and the backend decides what happens.
    let response = get(&server.address, "/index.php");
    assert_eq!(response.status, 502, "{response:?}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_head_request_describes_the_file_without_sending_it() {
    let root = static_root("head");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    let response = request_with(
        &server.address,
        "HEAD /assets/app.css HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n",
    );

    assert_eq!(response.status, 200, "{response:?}");
    assert!(response.body.is_empty(), "a HEAD has no body");
    assert_eq!(
        response.header("content-length"),
        Some("18"),
        "but it still declares the length it would have sent"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_unchanged_file_is_a_304_with_no_body() {
    let root = static_root("conditional");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    let first = get(&server.address, "/assets/app.css");
    let tag = first.header("etag").expect("an etag").to_string();

    let response = request_with(
        &server.address,
        &format!(
            "GET /assets/app.css HTTP/1.1\r\nHost: example.test\r\nIf-None-Match: {tag}\r\nConnection: close\r\n\r\n"
        ),
    );

    assert_eq!(response.status, 304, "{response:?}");
    assert!(response.body.is_empty(), "a 304 carries no body");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_range_request_returns_just_those_bytes() {
    let root = static_root("range");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    let response = request_with(
        &server.address,
        "GET /big.bin HTTP/1.1\r\nHost: example.test\r\nRange: bytes=0-9\r\nConnection: close\r\n\r\n",
    );

    assert_eq!(response.status, 206, "{response:?}");
    assert_eq!(response.header("content-range"), Some("bytes 0-9/262144"));
    assert_eq!(response.header("content-length"), Some("10"));
    assert_eq!(response.body, vec![b'x'; 10]);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_range_past_the_end_is_a_416() {
    let root = static_root("unsatisfiable");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    let response = request_with(
        &server.address,
        "GET /assets/app.css HTTP/1.1\r\nHost: example.test\r\nRange: bytes=9999-\r\nConnection: close\r\n\r\n",
    );

    assert_eq!(response.status, 416, "{response:?}");
    assert_eq!(
        response.header("content-range"),
        Some("bytes */18"),
        "the client is told what would have worked"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_directory_without_its_slash_redirects() {
    let root = static_root("redirect");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    let response = get(&server.address, "/assets");

    assert_eq!(response.status, 301, "{response:?}");
    assert_eq!(
        response.header("location"),
        Some("//example.test/assets/"),
        "the client's own Host is used, so the redirect stays on it"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_dotfile_is_not_served_and_is_not_reported_as_missing() {
    let root = static_root("dotfile");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    let response = get(&server.address, "/.env");

    // 404 rather than 403: whether a file exists is not a client's business, and
    // a 403 would confirm it is there.
    assert_eq!(response.status, 404, "{response:?}");

    let _ = std::fs::remove_dir_all(&root);
}

/// With static serving **off**, a non-script file must not reach PHP either.
///
/// The backend here is unreachable, so a 502 is the proof that routing declined
/// to use the file as a script. Before this was fixed, routing matched *any*
/// existing file and handed it to php-cgi, which does not check that a script is
/// PHP: given `/.env` or `/config.json` it emitted the contents verbatim with a
/// 200. The static layer's own refusals do not cover this, because the request
/// never reached it — `POST` never does, and nor does anything at all when the
/// layer is disabled.
#[test]
fn a_non_script_file_is_not_dispatched_to_php() {
    let root = static_root("nonscript");
    // The fixture has `.env`; add a plain non-PHP data file next to it.
    std::fs::write(root.join("config.json"), "{\"secret\":1}").expect("write config.json");

    let Some(server) = static_server(&root, Default::default()) else {
        return;
    };

    for path in ["/config.json", "/assets/app.css"] {
        let response = get(&server.address, path);
        assert_eq!(
            response.status, 502,
            "{path} must reach the backend as application input, not be read as a script: {response:?}"
        );
        assert!(
            !response.text().contains("secret") && !response.text().contains("SECRET"),
            "{path} leaked file contents: {response:?}"
        );
    }

    // A dotfile is hidden outright, not passed on as input.
    let response = get(&server.address, "/.env");
    assert_eq!(response.status, 404, "{response:?}");
    assert!(
        !response.text().contains("SECRET"),
        "the dotfile's contents leaked: {response:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_traversal_is_refused() {
    let root = static_root("traversal");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    let response = get(&server.address, "/../Cargo.toml");
    assert!(
        response.status == 403 || response.status == 404,
        "a traversal must not be served: {response:?}"
    );
    assert!(
        !response.text().contains("stokehold"),
        "and must not leak a file above the root"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_precompressed_variant_is_served_when_it_is_accepted() {
    let root = static_root("precompressed");
    let config = stokehold_core::config::StaticConfig {
        enabled: true,
        precompressed: true,
        ..Default::default()
    };
    let Some(server) = static_server(&root, config) else {
        return;
    };

    let response = request_with(
        &server.address,
        "GET /assets/app.js HTTP/1.1\r\nHost: example.test\r\nAccept-Encoding: gzip, br\r\nConnection: close\r\n\r\n",
    );

    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(response.text(), "brotli-bytes");
    assert_eq!(response.header("content-encoding"), Some("br"));
    assert_eq!(
        response.header("vary"),
        Some("accept-encoding"),
        "a cache has to key on the encoding"
    );

    // A client that does not accept it gets the original.
    let response = request_with(
        &server.address,
        "GET /assets/app.js HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(response.text(), "console.log(1)");
    assert_eq!(response.header("content-encoding"), None);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn static_serving_is_inert_until_it_is_enabled() {
    let root = static_root("disabled");
    // The same document root, but serving switched off: every request must reach
    // the backend, exactly as it did before this layer existed.
    let Some(server) = static_server(&root, Default::default()) else {
        return;
    };

    let response = get(&server.address, "/assets/app.css");
    assert_eq!(
        response.status, 502,
        "a disabled static layer must not answer at all: {response:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_large_file_arrives_whole() {
    let root = static_root("large");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    // 256 KiB is four times the streaming block size, so this crosses the
    // boundary where a hand-written body would drop or duplicate a block.
    let response = get(&server.address, "/big.bin");

    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(response.body.len(), 256 * 1024);
    assert!(response.body.iter().all(|byte| *byte == b'x'));

    let _ = std::fs::remove_dir_all(&root);
}

/// A script must never be handed over as a file.
///
/// This is the one failure in this layer that is a *disclosure* rather than a
/// wrong answer, and it is silent by nature: PHP source is a valid body, so the
/// only symptom is that the client received the program instead of its output.
/// The backend here is unreachable, so a 502 is the proof that the layer
/// declined and left the decision to the layer that executes scripts.
#[test]
fn a_php_script_is_never_served_as_a_file() {
    let root = static_root("phpguard");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    let response = get(&server.address, "/index.php");

    assert_eq!(
        response.status, 502,
        "a .php path must reach the backend, never the file server: {response:?}"
    );
    assert!(
        !response.text().contains("SECRET-SOURCE"),
        "the script's source was disclosed: {response:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The extension rule is the routing layer's, not a second list.
///
/// A path that names a script *and* then more path is the `PATH_INFO` case, and
/// it is the one nginx users rely on every day; it has to reach PHP too.
#[test]
fn a_php_script_with_path_info_still_reaches_the_backend() {
    let root = static_root("phpinfo");
    let Some(server) = static_server(&root, static_config()) else {
        return;
    };

    let response = get(&server.address, "/index.php/deep/route");

    assert_eq!(
        response.status, 502,
        "PATH_INFO must reach the backend: {response:?}"
    );
    assert!(
        !response.text().contains("SECRET-SOURCE"),
        "the script's source was disclosed: {response:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Two sites in one process: the same path answers differently per `Host`.
///
/// This is the whole feature, and it is asserted with static content because that is what
/// makes the difference visible without a backend: the document root is chosen by the
/// `Host` header before anything else is decided, so `/whoami.txt` cannot be the same file
/// for two hosts.
#[test]
fn two_sites_in_one_process_serve_different_roots() {
    let base = std::env::temp_dir().join(format!("stokehold-sites-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    for (name, body) in [
        ("a", "site-a"),
        ("b", "site-b"),
        ("default", "site-default"),
    ] {
        std::fs::create_dir_all(base.join(name)).expect("create a site root");
        std::fs::write(base.join(name).join("whoami.txt"), body).expect("write the marker");
    }

    let text = format!(
        r#"
[server]
http = "127.0.0.1:0"
document_root = {default:?}
static_files = {{ enabled = true }}

[sites.a]
hosts = ["a.test", "*.a.test"]
document_root = {a:?}
static_files = {{ enabled = true }}

[sites.b]
hosts = ["b.test"]
document_root = {b:?}
static_files = {{ enabled = true }}
"#,
        default = base.join("default").to_string_lossy(),
        a = base.join("a").to_string_lossy(),
        b = base.join("b").to_string_lossy(),
    );
    let config = stokehold_core::config::Config::from_toml(&text).expect("config parses");
    let sites = Arc::new(Sites::from_config(&config, &base.join("default")));
    let routing = Routing {
        document_root: base.join("default"),
        front_controller: Some("index.php".to_string()),
        ..Routing::default()
    };
    let dispatcher =
        Dispatcher::from_config(&unreachable_config(), routing).expect("no process is started");

    let Some(server) = Server::start_sites(dispatcher, sites, HttpLimits::default()) else {
        return;
    };
    let address = server.address.clone();

    let answer = |host: &str| {
        let response = get_as(&address, host, "/whoami.txt");
        assert_eq!(response.status, 200, "{host}: {response:?}");
        response.text()
    };

    assert_eq!(answer("a.test"), "site-a");
    assert_eq!(answer("b.test"), "site-b");
    // The wildcard belongs to `a`, and is matched without regard to case or port.
    assert_eq!(answer("x.a.test"), "site-a");
    assert_eq!(answer("A.TEST:8080"), "site-a");
    // An unlisted host is the default site's.
    assert_eq!(answer("elsewhere.test"), "site-default");

    // A path naming no file belongs to the matching site's front controller, which is
    // unreachable here — 502 rather than a 404 or another site's content.
    let missing = get_as(&address, "b.test", "/no-such-file");
    assert_eq!(missing.status, 502, "{missing:?}");

    drop(server);
    let _ = std::fs::remove_dir_all(&base);
}
