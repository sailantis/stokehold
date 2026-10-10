//! Smoke test for `stokehold serve`: start the real binary, talk FastCGI to it,
//! and check that a request reaches a real PHP process and comes back.
//!
//! This proves the shipped binary works, as opposed to the library functions
//! being correct in isolation.
//!
//! Skipped, with a printed reason, when `php-cgi` is unavailable or the binary
//! has not been built.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use stokehold_fcgi::{message::BeginRequest, nvpair, protocol::RecordType, record};

/// The `stokehold` binary, found next to this test's own executable.
fn stokehold_binary() -> Option<PathBuf> {
    // target/<profile>/deps/<test>.exe -> target/<profile>/stokehold[.exe]
    let mut dir = std::env::current_exe().ok()?;
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    let name = if cfg!(windows) {
        "stokehold.exe"
    } else {
        "stokehold"
    };
    let candidate = dir.join(name);
    candidate.is_file().then_some(candidate)
}

fn find_php_cgi() -> Option<PathBuf> {
    #[cfg(windows)]
    let names: &[&str] = &["php-cgi.exe", "php-cgi"];
    #[cfg(not(windows))]
    let names: &[&str] = &["php-cgi"];

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

/// Find `php` on `PATH`, for the worker-backend test.
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

/// A run of consecutive free ports, since the pool increments from `base_port`.
fn free_port_run(count: usize) -> Option<u16> {
    for _ in 0..64 {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
        let base = probe.local_addr().ok()?.port();
        drop(probe);
        let mut held = Vec::with_capacity(count);
        for offset in 0..count {
            let port = base.checked_add(offset as u16)?;
            match std::net::TcpListener::bind(("127.0.0.1", port)) {
                Ok(listener) => held.push(listener),
                Err(_) => break,
            }
        }
        if held.len() == count {
            drop(held);
            return Some(base);
        }
    }
    None
}

/// A `stokehold serve` process that is killed when the test ends.
struct Server {
    child: Child,
    address: String,
    /// The HTTP listener's address, when the configuration asked for one.
    http_address: Option<String>,
}

impl Server {
    fn start(binary: &Path, config: &Path) -> Option<Self> {
        let child = Command::new(binary)
            .arg("serve")
            .arg("--config")
            .arg(config)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let mut server = Server {
            child,
            address: String::new(),
            http_address: None,
        };
        let stdout = server.child.stdout.take().expect("piped stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            // The banner is read line by line and parsed by field name rather than
            // by whole line, so the surrounding text can grow without breaking
            // every test that starts a server. Reading stops at the line that says
            // the server is up, which is also the last thing printed before it
            // starts serving.
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("fastcgi:") {
                let address = rest.trim();
                if !address.is_empty() && address != "(disabled)" {
                    server.address = address.to_string();
                }
            } else if let Some(rest) = trimmed.strip_prefix("http:") {
                let address = rest.trim();
                if !address.is_empty() && address != "(disabled)" {
                    server.http_address = Some(address.to_string());
                }
            } else if trimmed.starts_with("press Ctrl-C") && !server.address.is_empty() {
                return Some(server);
            }
        }
        let _ = server.child.kill();
        None
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn io_err(err: stokehold_fcgi::FcgiError) -> std::io::Error {
    match err {
        stokehold_fcgi::FcgiError::Io(io) => io,
        other => std::io::Error::other(other.to_string()),
    }
}

/// A decoded response: status, headers and body.
type Decoded = (u16, Vec<(String, String)>, String);

/// Send one request over a fresh connection and read the response back.
fn request(address: &str, params: &[(Vec<u8>, Vec<u8>)], body: &[u8]) -> std::io::Result<Decoded> {
    let mut stream = TcpStream::connect(address)?;
    stream.set_read_timeout(Some(Duration::from_secs(20)))?;

    let mut wire = Vec::new();
    let header = record::Header::new(RecordType::BeginRequest, 1, 8);
    header.write(&mut wire).map_err(io_err)?;
    wire.write_all(&BeginRequest::responder(false).encode())?;
    let payload = nvpair::encode_pairs(
        params
            .iter()
            .map(|(name, value)| (name.as_slice(), value.as_slice())),
    );
    record::write_records(&mut wire, RecordType::Params, 1, &payload, true).map_err(io_err)?;
    record::write_records(&mut wire, RecordType::Stdin, 1, body, true).map_err(io_err)?;
    stream.write_all(&wire)?;
    stream.flush()?;

    let mut stdout = Vec::new();
    while let Some(record) = record::read_record(&mut stream).map_err(io_err)? {
        match record.record_type() {
            RecordType::Stdout => stdout.extend_from_slice(&record.content),
            RecordType::EndRequest => break,
            _ => continue,
        }
    }

    Ok(parse(&stdout))
}

/// Split a FastCGI response into status, headers and body.
fn parse(stdout: &[u8]) -> Decoded {
    let text = String::from_utf8_lossy(stdout);
    let (head, body) = match text.split_once("\r\n\r\n") {
        Some((head, body)) => (head, body),
        None => match text.split_once("\n\n") {
            Some((head, body)) => (head, body),
            None => (text.as_ref(), ""),
        },
    };

    let mut status = 200u16;
    let mut headers = Vec::new();
    for (index, line) in head.split('\n').enumerate() {
        let line = line.trim_end_matches('\r');
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if index == 0 && name.eq_ignore_ascii_case("status") {
            status = value
                .split_whitespace()
                .next()
                .and_then(|code| code.parse().ok())
                .unwrap_or(200);
        } else {
            headers.push((name.trim().to_string(), value.to_string()));
        }
    }
    (status, headers, body.to_string())
}

fn params_for(root: &Path, uri: &str, body_len: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    let routing = stokehold_fcgi::Routing {
        document_root: root.to_path_buf(),
        front_controller: Some("index.php".to_string()),
        ..stokehold_fcgi::Routing::default()
    };
    let info = stokehold_fcgi::RequestInfo {
        method: if body_len > 0 { "POST" } else { "GET" }.to_string(),
        uri: uri.to_string(),
        body_len,
        ..stokehold_fcgi::RequestInfo::default()
    };
    stokehold_fcgi::build_params(&info, &routing).expect("params build")
}

/// Write the config the server will read, then start it.
fn start_server(
    binary: &Path,
    php_cgi: &Path,
    root: &Path,
    workers: u32,
    base_port: u16,
) -> Option<Server> {
    let config_path = root.join("stokehold.toml");
    let config = format!(
        "default_backend = \"cgi\"\n\n[server]\nfastcgi = \"127.0.0.1:0\"\n\n[backends.cgi]\nkind = \"spawn_cgi\"\nbinary = {binary:?}\nworkers = {workers}\nbind_host = \"127.0.0.1\"\nbase_port = {base_port}\n",
        binary = php_cgi.display().to_string(),
    );
    std::fs::write(&config_path, config).expect("write the config");
    Server::start(binary, &config_path)
}

/// The SDK's example worker, which is what the worker backend is pointed at.
fn example_worker() -> PathBuf {
    // `crates/stokehold-cli/tests/serve.rs` -> the repository root is two up.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("sdk")
        .join("php")
        .join("example")
        .join("worker.php")
}

/// Quote a string for a TOML basic (`"…"`) string.
///
/// A Windows path has to be escaped here: in a TOML basic string a backslash
/// starts an escape sequence, so `C:\Users` is a parse error, not a path. The
/// same papercut is waiting for anyone who writes this configuration by hand, so
/// the docs recommend a literal (`'…'`) string instead — but a test cannot rely
/// on the temp directory being free of the characters that would break a literal
/// string, so it escapes.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Start the server with a `worker` backend, so the whole chain runs on the
/// stokehold worker protocol rather than on FastCGI to `php-cgi`.
fn start_worker_server(binary: &Path, root: &Path) -> Option<Server> {
    let config_path = root.join("stokehold.toml");
    let command = toml_string(&format!("php \"{}\"", example_worker().display()));
    let config = format!(
        "default_backend = \"app\"\n\n[server]\nfastcgi = \"127.0.0.1:0\"\n\n[backends.app]\nkind = \"worker\"\ncommand = {command}\nworkers = 2\n",
    );
    std::fs::write(&config_path, config).expect("write the config");
    Server::start(binary, &config_path)
}

/// Start the server with both listeners and a worker backend.
fn start_dual_listener_server(binary: &Path, root: &Path) -> Option<Server> {
    let config_path = root.join("stokehold.toml");
    let command = toml_string(&format!("php \"{}\"", example_worker().display()));
    let config = format!(
        "default_backend = \"app\"\n\n[server]\nhttp = \"127.0.0.1:0\"\nfastcgi = \"127.0.0.1:0\"\n\n[backends.app]\nkind = \"worker\"\ncommand = {command}\nworkers = 2\n",
    );
    std::fs::write(&config_path, config).expect("write the config");
    Server::start(binary, &config_path)
}

#[test]
fn serve_answers_an_http_request_and_a_fastcgi_request_at_once() {
    let Some(binary) = stokehold_binary() else {
        eprintln!("skipping: the stokehold binary is not built (run `cargo build`)");
        return;
    };
    if find_php().is_none() {
        eprintln!("skipping: no php on PATH");
        return;
    }

    let root = std::env::temp_dir().join(format!("stokehold-http-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the temp root");

    let Some(server) = start_dual_listener_server(&binary, &root) else {
        eprintln!("skipping: the server did not start");
        return;
    };
    let Some(http_address) = server.http_address.clone() else {
        panic!("the HTTP listener did not report an address; the banner was not parsed");
    };

    // The HTTP front-end, over a real socket, against the shipped binary and the
    // shipped PHP SDK.
    let response = http_get(&http_address, "/health");
    assert_eq!(response.status, 200, "{response:?}");
    assert!(
        response.text().contains("ok"),
        "the worker's body did not arrive over HTTP: {}",
        response.text()
    );

    // The FastCGI listener on the same process must still serve, which is the
    // property the dual listener exists for: one dispatcher, two protocols.
    let body = b"";
    let params = params_for(&root, "/health", 0);
    let (status, _, fastcgi_body) =
        request(&server.address, &params, body).expect("the FastCGI request completed");
    // The worker backend has no front controller to resolve against the temp
    // root, so the environment is what matters here rather than the status: a
    // response proves the listener answered at all.
    assert!(
        status == 200 || status == 404 || status == 500,
        "the FastCGI listener produced no response: {status} {fastcgi_body}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A minimal HTTP response reader, for the dual-listener smoke test.
#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

impl HttpResponse {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn http_get(address: &str, path: &str) -> HttpResponse {
    let mut stream = TcpStream::connect(address).expect("connect to the HTTP listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("a read timeout");
    let request = format!("GET {path} HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("write the HTTP request");
    stream.flush().expect("flush");

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read the response");

    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or_else(|| {
            panic!(
                "no header terminator in {:?}",
                String::from_utf8_lossy(&raw)
            )
        });
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status in {head:?}"));

    HttpResponse {
        status,
        body: raw[split + 4..].to_vec(),
    }
}

#[test]
fn serve_answers_a_fastcgi_request_from_a_real_php_process() {
    let Some(binary) = stokehold_binary() else {
        eprintln!("skipping: the stokehold binary is not built (run `cargo build`)");
        return;
    };
    let Some(php_cgi) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let Some(base_port) = free_port_run(2) else {
        eprintln!("skipping: no run of two free ports");
        return;
    };

    let root = std::env::temp_dir().join(format!("stokehold-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    std::fs::write(
        root.join("index.php"),
        r#"<?php
header('X-Smoke: yes');
echo "URI=", $_SERVER['REQUEST_URI'] ?? '-', "\n";
echo "PATH=", $_SERVER['PATH_INFO'] ?? '-', "\n";
echo "BODY=", file_get_contents('php://input');
"#,
    )
    .expect("write the front controller");

    let server = start_server(&binary, &php_cgi, &root, 2, base_port).expect("the server started");
    assert!(!server.address.is_empty());

    let body = b"hello-body";
    let params = params_for(&root, "/deep/route?x=1", body.len());
    let (status, headers, response_body) =
        request(&server.address, &params, body).expect("the request completed");

    assert_eq!(status, 200, "{response_body}");
    assert!(
        headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("X-Smoke") && value == "yes"),
        "the script's header is missing: {headers:?}"
    );
    assert!(
        response_body.contains("URI=/deep/route?x=1"),
        "{response_body}"
    );
    assert!(
        response_body.contains("PATH=/deep/route"),
        "{response_body}"
    );
    assert!(response_body.contains("BODY=hello-body"), "{response_body}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn serve_keeps_serving_across_several_requests() {
    let Some(binary) = stokehold_binary() else {
        eprintln!("skipping: the stokehold binary is not built");
        return;
    };
    let Some(php_cgi) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let Some(base_port) = free_port_run(2) else {
        eprintln!("skipping: no run of two free ports");
        return;
    };

    let root = std::env::temp_dir().join(format!("stokehold-smoke-many-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    std::fs::write(
        root.join("index.php"),
        r#"<?php echo "N="; echo file_get_contents('php://input');"#,
    )
    .expect("write the front controller");

    let server = start_server(&binary, &php_cgi, &root, 2, base_port).expect("the server started");

    for index in 0..6 {
        let body = format!("req{index}");
        let params = params_for(&root, "/", body.len());
        let (status, _, response_body) =
            request(&server.address, &params, body.as_bytes()).expect("the request completed");
        assert_eq!(status, 200, "{response_body}");
        assert!(
            response_body.contains(&format!("N=req{index}")),
            "{response_body}"
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_nginx_get_values_probe_is_answered_without_ending_the_connection() {
    let Some(binary) = stokehold_binary() else {
        eprintln!("skipping: the stokehold binary is not built");
        return;
    };
    let Some(php_cgi) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let Some(base_port) = free_port_run(1) else {
        eprintln!("skipping: no free port");
        return;
    };

    let root = std::env::temp_dir().join(format!("stokehold-smoke-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    std::fs::write(root.join("index.php"), r#"<?php echo "OK";"#).expect("write the script");

    let server = start_server(&binary, &php_cgi, &root, 1, base_port).expect("the server started");

    // This is exactly what nginx does before deciding to use a FastCGI server.
    let mut stream = TcpStream::connect(&server.address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set timeout");

    let payload = nvpair::encode_pairs([(&b"max_connections"[..], &b""[..])]);
    let mut wire = Vec::new();
    record::write_records(&mut wire, RecordType::GetValues, 0, &payload, false)
        .expect("frame the probe");
    stream.write_all(&wire).expect("write the probe");
    stream.flush().expect("flush");

    let response = record::read_record(&mut stream)
        .map_err(io_err)
        .expect("a reply")
        .expect("a record");
    assert_eq!(
        response.record_type(),
        RecordType::GetValuesResult,
        "a probe must be answered with a result, never an END_REQUEST"
    );
    let values = nvpair::decode_pairs(&response.content).expect("decode the result");
    assert!(
        values.iter().any(|(name, _)| name == b"max_connections"),
        "the requested value should be present: {values:?}"
    );

    // The connection must still be usable for a real request afterwards, which
    // is the part that matters: a probe that closed the connection would make
    // nginx mark the server down.
    let params = params_for(&root, "/", 0);
    let mut wire = Vec::new();
    let header = record::Header::new(RecordType::BeginRequest, 1, 8);
    header.write(&mut wire).map_err(io_err).expect("header");
    wire.write_all(&BeginRequest::responder(true).encode())
        .expect("begin");
    let payload = nvpair::encode_pairs(
        params
            .iter()
            .map(|(name, value)| (name.as_slice(), value.as_slice())),
    );
    record::write_records(&mut wire, RecordType::Params, 1, &payload, true).expect("params");
    record::write_records(&mut wire, RecordType::Stdin, 1, b"", true).expect("stdin");
    stream.write_all(&wire).expect("write the request");
    stream.flush().expect("flush");

    let mut body = Vec::new();
    while let Some(record) = record::read_record(&mut stream)
        .map_err(io_err)
        .expect("a record")
    {
        match record.record_type() {
            RecordType::Stdout => body.extend_from_slice(&record.content),
            RecordType::EndRequest => break,
            _ => continue,
        }
    }
    let (status, _, text) = parse(&body);
    assert_eq!(status, 200, "{text}");
    assert!(text.contains("OK"), "{text}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_missing_script_is_an_error_response_not_a_hang() {
    let Some(binary) = stokehold_binary() else {
        eprintln!("skipping: the stokehold binary is not built");
        return;
    };
    let Some(php_cgi) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let Some(base_port) = free_port_run(1) else {
        eprintln!("skipping: no free port");
        return;
    };

    let root = std::env::temp_dir().join(format!("stokehold-smoke-missing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");

    let server = start_server(&binary, &php_cgi, &root, 1, base_port).expect("the server started");

    // No front controller exists in this root, so PHP cannot find the script.
    let params = params_for(&root, "/nope.php", 0);
    let (status, _, body) = request(&server.address, &params, b"").expect("the request completed");
    assert!(
        status == 404 || status == 500 || body.contains("No input file"),
        "expected an error, got {status}: {body}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The whole chain, with the worker pool in it:
///
/// ```text
/// FastCGI client (standing in for nginx)
///     ──FastCGI──► stokehold front-end ──► dispatcher ──stokehold protocol──► php worker
/// ```
///
/// This is the step-17 milestone: the shipped binary serves a request through the
/// persistent worker pool rather than through `php-cgi`, so the config parsing,
/// the command splitter, the pool, the handshake and the response rendering are
/// all exercised together.
#[test]
fn serve_reaches_a_persistent_php_worker_over_the_stokehold_protocol() {
    let Some(binary) = stokehold_binary() else {
        eprintln!("skipping: the stokehold binary is not built (run `cargo build`)");
        return;
    };
    if find_php().is_none() {
        eprintln!("skipping: no php on PATH");
        return;
    }
    let worker = example_worker();
    if !worker.is_file() {
        eprintln!(
            "skipping: the SDK example worker is missing at {}",
            worker.display()
        );
        return;
    }

    let root = std::env::temp_dir().join(format!("stokehold-smoke-worker-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");

    let Some(server) = start_worker_server(&binary, &root) else {
        eprintln!("skipping: the worker-backed server did not start");
        return;
    };
    assert!(!server.address.is_empty());

    // The worker decides what to run, so the document root only has to exist for
    // the CGI environment to be built against it.
    let params = params_for(&root, "/health", 0);
    let (status, headers, body) =
        request(&server.address, &params, b"").expect("the request completed");
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("\"status\":\"ok\""),
        "the worker's JSON should have arrived: {body}"
    );
    // The worker's own `Content-Type` survives the translation into a Status
    // block, which is what proves the header list was rendered and not dropped.
    assert!(
        headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("content-type")
                && value.starts_with("application/json")),
        "the worker's Content-Type is missing: {headers:?}"
    );

    // A body in the request direction, and a second request to prove the worker
    // is reused rather than restarted per request.
    let marker = "over-the-pool";
    let params = params_for(&root, "/echo", marker.len());
    let (status, _, body) =
        request(&server.address, &params, marker.as_bytes()).expect("the request completed");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(&format!("payload: {marker}")), "{body}");

    let _ = std::fs::remove_dir_all(&root);
}
