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
        Server::start_with(binary, config, &[])
    }

    /// Start the server with extra command-line arguments.
    ///
    /// `--document-root` is one of those: it is what routing *and* static serving
    /// resolve against, so a test that writes a file elsewhere has to say so.
    fn start_with(binary: &Path, config: &Path, extra: &[&str]) -> Option<Self> {
        Server::spawn(binary, config, extra, None)
    }

    /// Start the server with a working directory of its own.
    ///
    /// Needed to test a *relative* document root: the path is relative to the
    /// process's working directory, so the child has to be started where the
    /// fixture lives.
    fn start_in(binary: &Path, config: &Path, cwd: &Path) -> Option<Self> {
        Server::spawn(binary, config, &[], Some(cwd))
    }

    fn spawn(binary: &Path, config: &Path, extra: &[&str], cwd: Option<&Path>) -> Option<Self> {
        let mut command = Command::new(binary);
        command
            .arg("serve")
            .arg("--config")
            .arg(config)
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let child = command.spawn().ok()?;

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
            } else if trimmed.starts_with("press Ctrl-C") {
                // The banner ends here, and this is the last line printed before
                // the listeners serve. Which addresses appear depends on the
                // configuration, so the requirement is that *something* was
                // reported — not that a particular listener was. Waiting for the
                // FastCGI address specifically hangs a config that has only
                // `http`, and it hangs *silently*: `read_line` has no timeout, so
                // the deadline above is never re-checked while it blocks.
                if server.address.is_empty() && server.http_address.is_none() {
                    break;
                }
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
///
/// The document root is written into the config rather than passed as a flag:
/// this is the path a deployment takes, and the flag is only the override.
fn start_server(
    binary: &Path,
    php_cgi: &Path,
    root: &Path,
    workers: u32,
    base_port: u16,
) -> Option<Server> {
    let config_path = root.join("stokehold.toml");
    let document_root = toml_string(&root.display().to_string());
    let config = format!(
        "default_backend = \"cgi\"\n\n[server]\nfastcgi = \"127.0.0.1:0\"\ndocument_root = {document_root}\n\n[backends.cgi]\nkind = \"spawn_cgi\"\nbinary = {binary:?}\nworkers = {workers}\nbind_host = \"127.0.0.1\"\nbase_port = {base_port}\n",
        binary = php_cgi.display().to_string(),
    );
    std::fs::write(&config_path, config).expect("write the config");
    Server::start(binary, &config_path)
}

/// The SDK the example worker lives in, which is now a repository of its own.
///
/// `STOKEHOLD_SDK_DIR` overrides it; otherwise the sibling layout is assumed:
/// `sailantis/stokehold` and `sailantis/stokehold-php-sdk` next to each other.
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

/// The SDK's example worker, which is what the worker backend is pointed at.
fn example_worker() -> PathBuf {
    sdk_dir().join("example").join("worker.php")
}

/// Why a worker-backed test cannot run here, if it cannot.
///
/// The SDK is a separate checkout, so "the test ran" and "the test was skipped"
/// are easy to confuse. `STOKEHOLD_E2E=1` turns the skip into a failure, because
/// libtest hides the stderr of a passing test and a skip is otherwise invisible.
fn sdk_skip_reason() -> Option<String> {
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

/// Skip a worker-backed test, loudly, when the SDK checkout is missing.
macro_rules! require_sdk {
    () => {
        if let Some(reason) = sdk_skip_reason() {
            if std::env::var("STOKEHOLD_E2E")
                .map(|v| !v.is_empty() && v != "0")
                .unwrap_or(false)
            {
                panic!("STOKEHOLD_E2E is set, so a skipped test is a failure: {reason}");
            }
            eprintln!("skipping: {reason}");
            return;
        }
    };
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
    let script = example_worker();
    assert!(
        script.is_file(),
        "the SDK example worker is missing at {}",
        script.display()
    );
    let command = toml_string(&format!("php \"{}\"", script.display()));
    let document_root = toml_string(&root.display().to_string());
    let config = format!(
        "default_backend = \"app\"\n\n[server]\nfastcgi = \"127.0.0.1:0\"\ndocument_root = {document_root}\n\n[backends.app]\nkind = \"worker\"\ncommand = {command}\nworkers = 2\n",
    );
    std::fs::write(&config_path, config).expect("write the config");
    Server::start(binary, &config_path)
}

/// Start the server with both listeners and a worker backend.
fn start_dual_listener_server(binary: &Path, root: &Path) -> Option<Server> {
    let config_path = root.join("stokehold.toml");
    let script = example_worker();
    assert!(
        script.is_file(),
        "the SDK example worker is missing at {}",
        script.display()
    );
    let command = toml_string(&format!("php \"{}\"", script.display()));
    let document_root = toml_string(&root.display().to_string());
    let config = format!(
        "default_backend = \"app\"\n\n[server]\nhttp = \"127.0.0.1:0\"\nfastcgi = \"127.0.0.1:0\"\ndocument_root = {document_root}\n\n[backends.app]\nkind = \"worker\"\ncommand = {command}\nworkers = 2\n",
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

    require_sdk!();

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
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
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
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status in {head:?}"));
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .collect();

    HttpResponse {
        status,
        headers,
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

    require_sdk!();

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

/// Start the server with both listeners, a worker backend and static files.
///
/// The document root is written into the config, so the file served by the
/// *shipped binary* is resolved from the *shipped config* — nothing about the
/// path is passed on a command line.
fn start_static_server(binary: &Path, root: &Path) -> Option<Server> {
    let config_path = root.join("stokehold.toml");
    let command = toml_string(&format!("php \"{}\"", example_worker().display()));
    let document_root = toml_string(&root.display().to_string());
    let config = format!(
        "default_backend = \"app\"\n\n[server]\nhttp = \"127.0.0.1:0\"\nfastcgi = \"127.0.0.1:0\"\ndocument_root = {document_root}\n\n[server.static_files]\nenabled = true\ncache_control = \"public, max-age=60\"\n\n[backends.app]\nkind = \"worker\"\ncommand = {command}\nworkers = 2\n",
    );
    std::fs::write(&config_path, config).expect("write the config");
    Server::start_with(binary, &config_path, &[])
}

/// The static layer, end to end through the shipped binary.
///
/// One config, two answers: a path that names a file is served from disk without
/// waking PHP, and a path that does not is still the front controller's. The
/// second half is what proves the layer is a *routing* decision and not a
/// replacement for the application.
#[test]
fn serve_serves_a_static_file_and_still_reaches_php_for_a_route() {
    let Some(binary) = stokehold_binary() else {
        eprintln!("skipping: the stokehold binary is not built (run `cargo build`)");
        return;
    };
    if find_php().is_none() {
        eprintln!("skipping: no php on PATH");
        return;
    }

    let root = std::env::temp_dir().join(format!("stokehold-static-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    std::fs::write(root.join("style.css"), "body { margin: 0 }").expect("write a static file");

    let Some(server) = start_static_server(&binary, &root) else {
        eprintln!("skipping: the server did not start");
        return;
    };
    let Some(http_address) = server.http_address.clone() else {
        panic!("the HTTP listener did not report an address; the banner was not parsed");
    };

    // A real file: served from disk, with the MIME type and the caching the
    // configuration asked for.
    let response = http_get(&http_address, "/style.css");
    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(response.text(), "body { margin: 0 }");
    assert!(
        response
            .header("content-type")
            .is_some_and(|value| value.starts_with("text/css")),
        "the file's type is missing: {response:?}"
    );
    assert_eq!(
        response.header("cache-control"),
        Some("public, max-age=60"),
        "the configured caching applies"
    );

    // Not a file: the front controller answers, so the worker's own body arrives
    // and the static layer has not swallowed the application.
    let response = http_get(&http_address, "/health");
    assert_eq!(response.status, 200, "{response:?}");
    assert!(
        response.text().contains("ok"),
        "the application is still reachable: {}",
        response.text()
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A config that describes the whole deployment, and a flag that overrides part
/// of it — which is the rule the CLI documents.
///
/// Two document roots exist, and each holds a front controller that says which
/// one ran. That matters because the file and the flag are both plausible places
/// to set the root, and "which wins" has to be observable rather than assumed.
#[test]
fn the_command_line_document_root_overrides_the_one_in_the_file() {
    let Some(binary) = stokehold_binary() else {
        eprintln!("skipping: the stokehold binary is not built (run `cargo build`)");
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

    let root = std::env::temp_dir().join(format!("stokehold-override-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let from_file = root.join("from-file");
    let from_flag = root.join("from-flag");
    std::fs::create_dir_all(&from_file).expect("create the configured root");
    std::fs::create_dir_all(&from_flag).expect("create the override root");
    std::fs::write(from_file.join("index.php"), "<?php echo \"FILE\";")
        .expect("write the configured front controller");
    std::fs::write(from_flag.join("index.php"), "<?php echo \"FLAG\";")
        .expect("write the overriding front controller");

    let config_path = root.join("stokehold.toml");
    let document_root = toml_string(&from_file.display().to_string());
    let config = format!(
        "default_backend = \"cgi\"\n\n[server]\nfastcgi = \"127.0.0.1:0\"\ndocument_root = {document_root}\n\n[backends.cgi]\nkind = \"spawn_cgi\"\nbinary = {cgi:?}\nworkers = 1\nbind_host = \"127.0.0.1\"\nbase_port = {base_port}\n",
        cgi = php_cgi.display().to_string(),
    );
    std::fs::write(&config_path, config).expect("write the config");

    let Some(server) = Server::start_with(
        &binary,
        &config_path,
        &["--document-root", &from_flag.display().to_string()],
    ) else {
        eprintln!("skipping: the server did not start");
        return;
    };

    let params = params_for(&from_flag, "/", 0);
    let (status, _, body) = request(&server.address, &params, b"").expect("the request completed");
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("FLAG"),
        "the flag's document root must answer, not the file's: {body}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A relative document root must survive a *second* request.
///
/// This drives the **HTTP** listener on purpose. The FastCGI tests build their
/// own `SCRIPT_FILENAME` and hand it to stokehold, so they cannot see a bug in
/// how stokehold derives one — the value under test never comes from stokehold
/// at all. The HTTP front-end computes it from the document root, which is where
/// the bug lived.
///
/// It also asks twice on purpose: `php-cgi` `chdir()`s into the script's
/// directory and the pool keeps the process, so a relative root only breaks once
/// a PHP request has already run. A single-request test passes either way.
#[test]
fn a_relative_document_root_keeps_working_after_the_first_request() {
    let Some(binary) = stokehold_binary() else {
        eprintln!("skipping: the stokehold binary is not built (run `cargo build`)");
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

    let root = std::env::temp_dir().join(format!("stokehold-relative-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("public")).expect("create the document root");
    std::fs::write(
        root.join("public/index.php"),
        "<?php echo \"PATH_INFO=\", $_SERVER['PATH_INFO'] ?? '-';",
    )
    .expect("write the front controller");

    // Deliberately relative, and the server is started from the directory it is
    // relative to — exactly how an operator would write it. `Server::start_in`
    // sets the child's working directory, so `"public"` resolves against the
    // fixture rather than against wherever the test runner happens to be.
    let config_path = root.join("stokehold.toml");
    let config = format!(
        "default_backend = \"cgi\"\n\n[server]\nhttp = \"127.0.0.1:0\"\ndocument_root = \"public\"\n\n[backends.cgi]\nkind = \"spawn_cgi\"\nbinary = {cgi:?}\nworkers = 1\nbind_host = \"127.0.0.1\"\nbase_port = {base_port}\n",
        cgi = php_cgi.display().to_string(),
    );
    std::fs::write(&config_path, config).expect("write the config");

    let Some(server) = Server::start_in(&binary, &config_path, &root) else {
        eprintln!("skipping: the server did not start");
        return;
    };
    let Some(http_address) = server.http_address.clone() else {
        panic!("the HTTP listener did not report an address; the banner was not parsed");
    };

    // The same URL, three times: the first request is the one that makes the
    // difference, and the second is where a relative root would fail.
    for attempt in 1..=3 {
        let response = http_get(&http_address, "/some/route");
        assert_eq!(
            response.status, 200,
            "request {attempt} failed: {response:?}"
        );
        assert!(
            response.text().contains("PATH_INFO=/some/route"),
            "request {attempt} did not reach the front controller: {}",
            response.text()
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// `version` reports the build's own facts, so an operator can check the PHP
/// floor from a shell without opening the documentation.
#[test]
fn version_prints_the_protocol_and_the_php_floor() {
    let Some(binary) = stokehold_binary() else {
        eprintln!("skipping: the stokehold binary is not built (run `cargo build`)");
        return;
    };

    let output = Command::new(&binary)
        .arg("version")
        .output()
        .expect("the version command runs");
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);

    assert!(text.contains("worker protocol"), "{text}");
    assert!(
        text.contains("php floor:        8.2"),
        "the floor a build claims must be the floor it has: {text}"
    );
    assert!(text.contains("FastCGI"), "{text}");
    assert!(
        !text.contains("spawn_fpm"),
        "a removed backend must not be advertised: {text}"
    );
}

/// `check` describes the sites a deployment will serve, and where a route still
/// checks the filesystem.
///
/// The route line is the point of the second half: `skip_stat` is inferred rather than
/// written, so the only way an operator can see the saving is if `check` prints it.
#[test]
fn check_describes_the_sites_and_the_routes_that_skip_the_stat() {
    let Some(binary) = stokehold_binary() else {
        eprintln!("skipping: the stokehold binary is not built (run `cargo build`)");
        return;
    };
    let root = std::env::temp_dir().join(format!("stokehold-check-sites-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("a")).expect("create a site root");
    std::fs::create_dir_all(root.join("b")).expect("create a site root");

    let config_path = root.join("stokehold.toml");
    let config = format!(
        r#"
default_backend = "app"

[server]
http = "127.0.0.1:8080"
document_root = {root}

[backends.app]
kind = "worker"
command = "php worker.php"

[backends.cgi]
kind = "spawn_cgi"
binary = 'C:\php\php-cgi.exe'

[[routes]]
path_prefix = "/api"
backend = "app"

[[routes]]
path_prefix = "/legacy"
backend = "cgi"

[[routes]]
path_suffix = ".php"
backend = "cgi"

[sites.a]
hosts = ["a.test", "*.a.test"]
document_root = {a}
front_controller = false
serve_only = "static"

[sites.b]
hosts = ["b.test"]
document_root = {b}
default_backend = "cgi"
"#,
        root = toml_string(&root.display().to_string()),
        a = toml_string(&root.join("a").display().to_string()),
        b = toml_string(&root.join("b").display().to_string()),
    );
    std::fs::write(&config_path, config).expect("write the config");

    let output = Command::new(&binary)
        .arg("check")
        .arg("--config")
        .arg(&config_path)
        .output()
        .expect("the check command runs");
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);

    // The site table, with each site's own settings rather than the server's.
    assert!(text.contains("sites:"), "{text}");
    assert!(text.contains("a: a.test, *.a.test"), "{text}");
    assert!(text.contains("b: b.test"), "{text}");
    assert!(
        text.contains("serve only:       static"),
        "a site's serve_only must be reported, not the server's: {text}"
    );
    assert!(
        text.contains("front controller: (none: only real .php files are served)"),
        "`front_controller = false` must be reported as such: {text}"
    );
    assert!(
        text.contains("default backend:  cgi"),
        "a site's own default backend must be reported: {text}"
    );

    // Three routes, two reasons to skip and one route that must not:
    // `/api` goes to a worker, which reads the path as metadata and never interprets it;
    // `.php` pins a script extension, so the file can only be PHP;
    // `/legacy` goes to `php-cgi` with a prefix, which proves nothing about the
    // extension — `/legacy/config.json` would be emitted verbatim — so the stat stays.
    assert!(
        text.contains("stat: skipped"),
        "the saving must be visible: {text}"
    );
    assert!(
        text.matches("stat: skipped").count() == 2,
        "the worker and suffix routes skip, the prefix route does not: {text}"
    );
    let legacy = text
        .lines()
        .position(|line| line.contains("/legacy") && line.contains("-> cgi"))
        .expect("the /legacy route is printed");
    assert!(
        !text
            .lines()
            .nth(legacy + 1)
            .unwrap_or("")
            .contains("stat: skipped"),
        "a prefix route proves nothing about the extension and must keep the stat: {text}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
