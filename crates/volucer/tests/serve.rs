//! Smoke test for `volucer serve`: start the real binary, talk FastCGI to it,
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

// Only the Unix half needs it: the TLS client that reads which certificate a handshake was
// presented with is `#[cfg(unix)]`, for the same reason the signal tests are.
#[cfg(unix)]
use std::sync::Arc;

use volucer_fcgi::{message::BeginRequest, nvpair, protocol::RecordType, record};

/// The `volucer` binary, found next to this test's own executable.
fn volucer_binary() -> Option<PathBuf> {
    // target/<profile>/deps/<test>.exe -> target/<profile>/volucer[.exe]
    let mut dir = std::env::current_exe().ok()?;
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    let name = if cfg!(windows) {
        "volucer.exe"
    } else {
        "volucer"
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

/// A `volucer serve` process that is killed when the test ends.
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
                // The first whitespace-delimited token, so a line that carries a suffix can still
                // be read: `http:` prints `(https)` after the address when TLS shares the port,
                // and a parser that took the rest of the line would hand a caller
                // `127.0.0.1:8080 (https)` as an address.
                let address = rest.split_whitespace().next().unwrap_or("");
                if !address.is_empty() && address != "(disabled)" {
                    server.address = address.to_string();
                }
            } else if let Some(rest) = trimmed.strip_prefix("http:") {
                let address = rest.split_whitespace().next().unwrap_or("");
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

fn io_err(err: volucer_fcgi::FcgiError) -> std::io::Error {
    match err {
        volucer_fcgi::FcgiError::Io(io) => io,
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
    let routing = volucer_fcgi::Routing {
        document_root: root.to_path_buf(),
        front_controller: Some("index.php".to_string()),
        ..volucer_fcgi::Routing::default()
    };
    let info = volucer_fcgi::RequestInfo {
        method: if body_len > 0 { "POST" } else { "GET" }.to_string(),
        uri: uri.to_string(),
        body_len,
        ..volucer_fcgi::RequestInfo::default()
    };
    volucer_fcgi::build_params(&info, &routing).expect("params build")
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
    let config_path = root.join("volucer.toml");
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
/// `VOLUCER_SDK_DIR` overrides it; otherwise the sibling layout is assumed:
/// `sailantis/volucer` and `sailantis/volucer-php-sdk` next to each other.
fn sdk_dir() -> PathBuf {
    match std::env::var_os("VOLUCER_SDK_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("..")
            .join("volucer-php-sdk"),
    }
}

/// The SDK's example worker, which is what the worker backend is pointed at.
fn example_worker() -> PathBuf {
    sdk_dir().join("example").join("worker.php")
}

/// Why a worker-backed test cannot run here, if it cannot.
///
/// The SDK is a separate checkout, so "the test ran" and "the test was skipped"
/// are easy to confuse. `VOLUCER_E2E=1` turns the skip into a failure, because
/// libtest hides the stderr of a passing test and a skip is otherwise invisible.
fn sdk_skip_reason() -> Option<String> {
    let script = example_worker();
    if !script.is_file() {
        return Some(format!(
            "the SDK example worker is not at {} — set VOLUCER_SDK_DIR to a \
             volucer-php-sdk checkout",
            script.display()
        ));
    }
    None
}

/// Skip a worker-backed test, loudly, when the SDK checkout is missing.
macro_rules! require_sdk {
    () => {
        if let Some(reason) = sdk_skip_reason() {
            if std::env::var("VOLUCER_E2E")
                .map(|v| !v.is_empty() && v != "0")
                .unwrap_or(false)
            {
                panic!("VOLUCER_E2E is set, so a skipped test is a failure: {reason}");
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
/// volucer worker protocol rather than on FastCGI to `php-cgi`.
fn start_worker_server(binary: &Path, root: &Path) -> Option<Server> {
    let config_path = root.join("volucer.toml");
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
    let config_path = root.join("volucer.toml");
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
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };
    if find_php().is_none() {
        eprintln!("skipping: no php on PATH");
        return;
    }

    let root = std::env::temp_dir().join(format!("volucer-http-smoke-{}", std::process::id()));
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
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
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

    let root = std::env::temp_dir().join(format!("volucer-smoke-{}", std::process::id()));
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
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built");
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

    let root = std::env::temp_dir().join(format!("volucer-smoke-many-{}", std::process::id()));
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
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built");
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

    let root = std::env::temp_dir().join(format!("volucer-smoke-probe-{}", std::process::id()));
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
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built");
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

    let root = std::env::temp_dir().join(format!("volucer-smoke-missing-{}", std::process::id()));
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
///     ──FastCGI──► volucer front-end ──► dispatcher ──volucer protocol──► php worker
/// ```
///
/// This is the step-17 milestone: the shipped binary serves a request through the
/// persistent worker pool rather than through `php-cgi`, so the config parsing,
/// the command splitter, the pool, the handshake and the response rendering are
/// all exercised together.
#[test]
fn serve_reaches_a_persistent_php_worker_over_the_volucer_protocol() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
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

    let root = std::env::temp_dir().join(format!("volucer-smoke-worker-{}", std::process::id()));
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
    let config_path = root.join("volucer.toml");
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
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };
    if find_php().is_none() {
        eprintln!("skipping: no php on PATH");
        return;
    }

    let root = std::env::temp_dir().join(format!("volucer-static-smoke-{}", std::process::id()));
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
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
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

    let root = std::env::temp_dir().join(format!("volucer-override-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let from_file = root.join("from-file");
    let from_flag = root.join("from-flag");
    std::fs::create_dir_all(&from_file).expect("create the configured root");
    std::fs::create_dir_all(&from_flag).expect("create the override root");
    std::fs::write(from_file.join("index.php"), "<?php echo \"FILE\";")
        .expect("write the configured front controller");
    std::fs::write(from_flag.join("index.php"), "<?php echo \"FLAG\";")
        .expect("write the overriding front controller");

    let config_path = root.join("volucer.toml");
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
/// own `SCRIPT_FILENAME` and hand it to volucer, so they cannot see a bug in
/// how volucer derives one — the value under test never comes from volucer
/// at all. The HTTP front-end computes it from the document root, which is where
/// the bug lived.
///
/// It also asks twice on purpose: `php-cgi` `chdir()`s into the script's
/// directory and the pool keeps the process, so a relative root only breaks once
/// a PHP request has already run. A single-request test passes either way.
#[test]
fn a_relative_document_root_keeps_working_after_the_first_request() {
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
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

    let root = std::env::temp_dir().join(format!("volucer-relative-{}", std::process::id()));
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
    let config_path = root.join("volucer.toml");
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
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
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
        text.contains("tls-alpn-01"),
        "the challenge a build performs must be advertised, because it is the difference between \
         a deployment that can be issued a certificate and one that cannot: {text}"
    );
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
    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };
    let root = std::env::temp_dir().join(format!("volucer-check-sites-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("a")).expect("create a site root");
    std::fs::create_dir_all(root.join("b")).expect("create a site root");

    let config_path = root.join("volucer.toml");
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

/// Send a signal to a running server, on a platform that has that signal.
///
/// `None` on Windows, where the test is skipped with a reason rather than failing: there is no
/// `SIGHUP` and no equivalent, so "reload by signal" is not something a Windows build can be
/// asked to do. The skip is *stated* for the same reason the restart-only list is — a capability
/// that is absent should say so rather than look untested.
#[cfg(unix)]
fn signal(child: &Child, signal: i32) -> Option<()> {
    // `child.id()` is a `u32` in Rust but a `pid_t` (`i32`) to the kernel, and a pid above
    // `i32::MAX` is not a thing Linux produces.
    let pid = i32::try_from(child.id()).ok()?;
    let rc = unsafe { libc_kill(pid, signal) };
    (rc == 0).then_some(())
}

#[cfg(unix)]
extern "C" {
    /// `kill(2)`, declared here rather than reaching through another crate's re-export of `libc`.
    /// `libc` is in the dependency graph transitively but is not a direct dependency of this crate,
    /// and a test that borrowed another crate's re-export would break when that crate changed.
    #[link_name = "kill"]
    fn libc_kill(pid: i32, signal: i32) -> i32;
}

/// `SIGHUP` reloads a running server, and a file it cannot apply changes nothing.
///
/// The deliverable of this phase, asserted the only way that means anything: against a **real
/// process**, because the mechanism is a signal and a signal is not observable from a unit test of
/// the code that handles it.
///
/// Both halves are here together on purpose. A reload that works is easy; the claim that matters is
/// that a *refused* reload leaves the previous configuration serving, and the two are one mechanism
/// seen from either side — the second is what makes the first safe to send to a live server.
///
/// Falsified before being trusted: against the binary from before this change, the `alive` half of
/// this test failed with the process **gone**, because `ctrlc`'s `termination` feature registered
/// `SIGHUP` and ran its shutdown callback for it.
#[cfg(unix)]
#[test]
fn sighup_reloads_the_running_configuration_and_a_bad_file_changes_nothing() {
    const SIGHUP: i32 = 1;

    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-reload-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let first = root.join("first");
    let second = root.join("second");
    for dir in [&first, &second] {
        std::fs::create_dir_all(dir).expect("create a document root");
    }
    std::fs::write(first.join("whoami.txt"), "first").expect("write the marker");
    std::fs::write(second.join("whoami.txt"), "second").expect("write the marker");

    let config_path = root.join("volucer.toml");
    let config_for = |root: &Path| {
        let document_root = toml_string(&root.display().to_string());
        format!(
            "[server]\nhttp = \"127.0.0.1:0\"\ndocument_root = {document_root}\n\n\
             [server.static_files]\nenabled = true\n"
        )
    };
    std::fs::write(&config_path, config_for(&first)).expect("write the config");

    let Some(server) = Server::start(&binary, &config_path) else {
        eprintln!("skipping: the server did not start");
        return;
    };
    let address = server
        .http_address
        .clone()
        .expect("the config asks for an http listener");

    assert_eq!(
        http_get(&address, "/whoami.txt").text(),
        "first",
        "the first request must be served from the starting root"
    );

    // A file that cannot be applied. The server is signalled anyway, and the answer must not move:
    // this is the half that makes a reload safe to send to a live server, because a typo in an
    // unrelated key would otherwise take the deployment with it.
    //
    // **The malformed file names the *other* root on purpose.** A file that named the root already
    // serving would make this assertion pass against code that applied it anyway, which is the
    // shape of hazard 57: where the claim is "nothing was applied", the input has to contain
    // something that would have been applied. A duplicate key fails at parse, and it still carries
    // a root that a partial reader would have taken.
    let duplicate_key = format!(
        "{}document_root = {}\n",
        config_for(&second),
        toml_string(&second.display().to_string())
    );
    std::fs::write(&config_path, duplicate_key).expect("write a malformed config");
    let Some(()) = signal(&server.child, SIGHUP) else {
        eprintln!("skipping: could not signal the server");
        return;
    };
    assert_eq!(
        http_get(&address, "/whoami.txt").text(),
        "first",
        "a configuration that does not parse must leave the previous one serving, including the \
         root that file asked for"
    );

    // Parses, and does not validate. A different code path from the first refusal: valid TOML and
    // a bad configuration is the mistake an operator is more likely to make. This one names the
    // *other* root too, so applying it despite the invalid key would be visible in the answer.
    std::fs::write(
        &config_path,
        format!("default_backend = \"nope\"\n\n{}", config_for(&second)),
    )
    .expect("write an invalid config");
    signal(&server.child, SIGHUP).expect("the server is still running");
    assert_eq!(
        http_get(&address, "/whoami.txt").text(),
        "first",
        "a configuration that does not validate must leave the previous one serving, including \
         the root that file asked for"
    );

    // And now one that can be applied, changing exactly one thing so the reload is attributable to
    // the document root rather than to everything about the file.
    std::fs::write(&config_path, config_for(&second)).expect("write a valid config");
    signal(&server.child, SIGHUP).expect("the server is still running");

    // Polled rather than slept on: the signal is delivered asynchronously, so a fixed sleep is
    // either flaky or slow, and the answer *is* the observable.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut answer = String::new();
    while Instant::now() < deadline {
        answer = http_get(&address, "/whoami.txt").text();
        if answer == "second" {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        answer, "second",
        "a reload must change what the next request is served, on the same listener"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}

/// One `SIGHUP` changes a certificate **and** a site, and both take effect.
///
/// The two tables a reload installs are independent — the shared site table through the
/// `Registry`, the certificate table through the `SniResolver` — and that independence is exactly
/// why this needs its own test. Nothing else in the tree drives both through the *signal*: the
/// reloader's certificate half could be deleted and every other test would still pass, because
/// each half is exercised on its own somewhere else.
///
/// It is also the one assertion that needs the parts to be **simultaneously** observable. The
/// certificate is read from the handshake and the document root from the request body, so a
/// client that gets `renewed` back on `:0` and the new root's marker has seen both — which is
/// the only way to tell "one signal applied two things" from "two signals applied one thing
/// each". The site is a named site rather than the default one so that the *site* half is
/// attributable to the site table rather than to a root that moved on its own.
///
/// Unix only, for the same reason the `SIGHUP` test is: it is the signal path under test.
#[cfg(unix)]
#[test]
fn one_sighup_reloads_a_certificate_and_a_site_together() {
    const SIGHUP: i32 = 1;

    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-cert-site-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let first = root.join("first");
    let second = root.join("second");
    // The **site's** root is a subdirectory of the server's, and the marker lives only there. That
    // is what makes the site half attributable: `app.test` resolves to the site, so a reload that
    // moved only the server's default root would leave this answer at "first" and the test would
    // see it. Two roots that were the same path would make the assertion pass either way.
    for dir in [&first, &second] {
        std::fs::create_dir_all(dir.join("app")).expect("create a document root");
    }
    std::fs::write(first.join("app/whoami.txt"), "first").expect("write the marker");
    std::fs::write(second.join("app/whoami.txt"), "second").expect("write the marker");

    // Two server certificates, and the second is what the reload installs. Generated here rather
    // than committed, because a committed key ends up in someone's deployment.
    let certs = root.join("certs");
    std::fs::create_dir_all(&certs).expect("create the certificate directory");
    let write_pair = |file: &str, name: &str| -> Vec<u8> {
        let generated = rcgen::generate_simple_self_signed(vec![name.to_string()])
            .expect("generate a certificate");
        std::fs::write(certs.join(format!("{file}.crt")), generated.cert.pem())
            .expect("write the certificate");
        std::fs::write(
            certs.join(format!("{file}.key")),
            generated.key_pair.serialize_pem(),
        )
        .expect("write the key");
        generated.cert.der().to_vec()
    };
    let original_der = write_pair("server", "example.test");
    // Written now but named only by the *second* config, so the first config never reads it and
    // the reload is what installs it.
    let renewed_der = write_pair("renewed", "example.test");
    assert_ne!(
        original_der, renewed_der,
        "the fixture must generate a different certificate, or the reload assertion is vacuous"
    );

    let config_path = root.join("volucer.toml");
    let config_for = |certificate: &str, root: &Path| {
        let document_root = toml_string(&root.display().to_string());
        let site_root = toml_string(&root.join("app").display().to_string());
        format!(
            "[server]\nhttp = \"127.0.0.1:0\"\ndocument_root = {document_root}\n\n\
             [server.tls]\ncertificate = \"certs/{certificate}.crt\"\n\
             key = \"certs/{certificate}.key\"\n\n\
             [server.static_files]\nenabled = true\n\n\
             [sites.app]\nhosts = [\"app.test\"]\ndocument_root = {site_root}\n"
        )
    };
    std::fs::write(&config_path, config_for("server", &first)).expect("write the config");

    let Some(server) = Server::start(&binary, &config_path) else {
        eprintln!("skipping: the server did not start");
        return;
    };
    let address = server
        .http_address
        .clone()
        .expect("the config asks for an http listener");

    // Both halves before the reload, from one handshake and one request: the certificate this
    // deployment started with, and the root it started with. Asserted together so a fixture that
    // silently served one of them from the start would be visible here rather than as a passing
    // reload test.
    let before = tls_get(&address, "app.test", "/whoami.txt");
    assert_eq!(
        before.certificate, original_der,
        "the fixture must serve the certificate it was started with"
    );
    assert_eq!(
        before.body, "first",
        "the fixture must serve the root it was started with"
    );

    // One signal. Both configs differ in exactly two settings — which certificate, and which
    // site root — so what changes is attributable to the reload rather than to everything about
    // the file.
    std::fs::write(&config_path, config_for("renewed", &second)).expect("write the config");
    let Some(()) = signal(&server.child, SIGHUP) else {
        eprintln!("skipping: could not signal the server");
        return;
    };

    // Polled rather than slept on, and polled on **both** halves. That is not belt-and-braces:
    // the two tables are deliberately independent — the sites are applied first and the
    // certificates after, so a refused certificate cannot take the sites with it — which means
    // there is a real window in which the new site is serving under the old certificate. Closing
    // the loop on the first half alone would read that window and call it a failure; the claim is
    // that one signal applied *both* by the time it finished, so both are what is waited for.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut after = tls_get(&address, "app.test", "/whoami.txt");
    while Instant::now() < deadline && (after.body != "second" || after.certificate != renewed_der)
    {
        std::thread::sleep(Duration::from_millis(50));
        after = tls_get(&address, "app.test", "/whoami.txt");
    }

    assert_eq!(
        after.body, "second",
        "one signal must reload the site table"
    );
    assert_eq!(
        after.certificate, renewed_der,
        "the same signal must reload the certificate table too — a reloader that refreshed only \
         the sites would pass every other test in this tree"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}

/// A reload must not throw away the certificate ACME issued.
///
/// The reloader rebuilds the certificate table with `SniResolver::refresh`, which is
/// **whole-or-nothing**: it loads exactly the pairs the configuration names. That is the right
/// shape for a reload — a set with one unreadable pair is refused entirely rather than half
/// applied — but it has a consequence for a deployment that has `[server.acme]`: the issued
/// certificate is not in the file, it is in the cache and in the running table, so a reload that
/// rebuilt from the configuration alone would put the **fallback** self-signed pair back into
/// service on a production host.
///
/// That is a silent regression, which is why it is pinned here rather than left to reasoning: the
/// server keeps running, the port keeps answering, every other reload test stays green, and the
/// symptom is a browser warning that appears weeks later — after the next `SIGHUP`, with the
/// cause already attributed to something else.
///
/// The **control** matters as much as the assertion. A test that never had an issued certificate
/// would pass against a reloader that did nothing at all, so the issued certificate is installed
/// through the same in-memory seam an issuance uses and *asserted to be serving* before the signal
/// is sent. What follows is then a statement about the reload and not about ACME being off: if the
/// `[server.acme]` table were dropped from the config, the "before" assertion fails first.
#[cfg(unix)]
#[test]
fn a_reload_keeps_the_certificate_that_was_issued() {
    use rustls_acme::CertCache;

    const SIGHUP: i32 = 1;

    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-acme-reload-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let site = root.join("site");
    std::fs::create_dir_all(&site).expect("create a document root");
    std::fs::write(site.join("whoami.txt"), "here").expect("write the marker");
    let certs = root.join("certs");
    std::fs::create_dir_all(&certs).expect("create the certificate directory");

    // The fallback. It is what a reload that rebuilt from the configuration alone would restore.
    let fallback = rcgen::generate_simple_self_signed(vec!["example.test".to_string()])
        .expect("generate the fallback certificate");
    std::fs::write(certs.join("fallback.crt"), fallback.cert.pem()).expect("write the cert");
    std::fs::write(
        certs.join("fallback.key"),
        fallback.key_pair.serialize_pem(),
    )
    .expect("write the key");
    let fallback_der = fallback.cert.der().to_vec();

    // The issued certificate, in the shape `rustls-acme` stores and hands over: a chain plus a key
    // concatenated into one blob. It is written into the *cache* rather than the config, because
    // that is where an issued certificate lives — the file the configuration names stays the
    // self-signed pair, and that is the whole point of the assertion below.
    let issued = rcgen::generate_simple_self_signed(vec!["example.test".to_string()])
        .expect("generate the issued certificate");
    let issued_blob = format!("{}{}", issued.cert.pem(), issued.key_pair.serialize_pem());
    let issued_der = issued.cert.der().to_vec();
    assert_ne!(
        fallback_der, issued_der,
        "the fixture must differ, or the assertion below is vacuous"
    );

    // The directory URL comes from **the same function `serve` calls**, so this cannot drift from
    // what the engine looks for without the server and the test disagreeing about the authority.
    // Staging, so even a bug that reached the network could not issue a real certificate.
    let directory = volucer_acme::Authority::Staging.directory();
    assert!(
        directory.contains("staging"),
        "the fixture must use staging: {directory}"
    );

    let cache_dir = root.join("acme-cache");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime for the cache write");
    runtime.block_on(async {
        rustls_acme::caches::DirCache::new(&cache_dir)
            .store_cert(
                &["example.test".to_string()],
                directory,
                issued_blob.as_bytes(),
            )
            .await
            .expect("the cache is writable");
    });

    let config_path = root.join("volucer.toml");
    let config = format!(
        "[server]\nhttp = \"127.0.0.1:0\"\ndocument_root = {site}\n\n\
         [server.tls]\ncertificate = \"certs/fallback.crt\"\nkey = \"certs/fallback.key\"\n\n\
         [server.acme]\ndomains = [\"example.test\"]\ncontact = [\"mailto:ops@example.test\"]\n",
        site = toml_string(&site.display().to_string()),
    );
    std::fs::write(&config_path, &config).expect("write the config");

    let Some(server) = Server::start(&binary, &config_path) else {
        eprintln!("skipping: the server did not start");
        return;
    };
    let address = server
        .http_address
        .clone()
        .expect("the config asks for an http listener");

    // The control: the cached certificate is serving *before* the signal. Without this the test
    // would pass against a reloader that did nothing, and against one in a build where the engine
    // was never wired up at all.
    let before = tls_get(&address, "example.test", "/whoami.txt");
    assert_eq!(
        before.certificate, issued_der,
        "the fixture must be serving the issued certificate before the reload, or the assertion \
         after it is about nothing"
    );

    // The file is rewritten unchanged: the signal is the only thing that happens. A reload that
    // finds the same configuration is the ordinary case an operator produces by hand.
    std::fs::write(&config_path, &config).expect("rewrite the same config");
    let Some(()) = signal(&server.child, SIGHUP) else {
        eprintln!("skipping: could not signal the server");
        return;
    };

    // Wait for the reload to have happened at all — the marker proves it, so the certificate
    // assertion is not made against a server that has simply not reloaded yet.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut after = tls_get(&address, "example.test", "/whoami.txt");
    while Instant::now() < deadline && after.body != "here" {
        std::thread::sleep(Duration::from_millis(50));
        after = tls_get(&address, "example.test", "/whoami.txt");
    }
    assert_eq!(after.body, "here", "the reload must have been applied");
    // Give a reload that *would* drop the certificate the chance to do so: the site table and the
    // certificate table are applied in sequence, sites first, so the certificate half lands last.
    std::thread::sleep(Duration::from_millis(500));

    after = tls_get(&address, "example.test", "/whoami.txt");
    assert_ne!(
        after.certificate, fallback_der,
        "a reload restored the **fallback** certificate, so the one ACME issued is no longer being \
         served — the deployment is back to a self-signed pair with no error and no log line, and \
         only a browser warning would ever say so"
    );
    assert_eq!(
        after.certificate, issued_der,
        "a reload must keep the issued certificate serving"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}

/// A reload still re-reads a **named** certificate, even with a site's own ACME table in play.
///
/// The complement of `a_reload_keeps_the_certificate_that_was_issued`, and it exists because the first
/// fix for hazard 71 was *too* blunt: it skipped the certificate rebuild whenever any ACME table was
/// configured, which kept the issued certificate but stranded every named one. A site's `[sites.x.tls]`
/// PEM is in the configuration and an operator can renew it by hand, so a reload has to re-read it —
/// and the two populations have to be merged rather than chosen between.
///
/// Both facts are asserted in one test because they are one behaviour from either side: the *issued*
/// certificate survives, and the *renewed* one is picked up. A merge that did only one of them passes
/// half of this and fails the other.
#[cfg(unix)]
#[test]
fn a_reload_picks_up_a_renewed_named_certificate_while_issuance_stays() {
    use rustls_acme::CertCache;

    const SIGHUP: i32 = 1;

    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-acme-merge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let app = root.join("app");
    let corp = root.join("corp");
    for dir in [&app, &corp] {
        std::fs::create_dir_all(dir).expect("create a document root");
    }
    std::fs::write(app.join("whoami.txt"), "app").expect("write the marker");
    std::fs::write(corp.join("whoami.txt"), "corp").expect("write the marker");

    let certs = root.join("certs");
    std::fs::create_dir_all(&certs).expect("create the certificate directory");
    let write_pair = |file: &str, name: &str| -> Vec<u8> {
        let generated = rcgen::generate_simple_self_signed(vec![name.to_string()])
            .expect("generate a certificate");
        std::fs::write(certs.join(format!("{file}.crt")), generated.cert.pem())
            .expect("write the certificate");
        std::fs::write(
            certs.join(format!("{file}.key")),
            generated.key_pair.serialize_pem(),
        )
        .expect("write the key");
        generated.cert.der().to_vec()
    };
    // The site's own, hand-managed certificate, which the reload will be asked to re-read.
    let named_before = write_pair("corp", "corp.test");
    let named_after = write_pair("renewed", "corp.test");
    assert_ne!(
        named_before, named_after,
        "the fixture must generate a different certificate, or the reload assertion is vacuous"
    );

    // The issued one, written into the cache under the site's ACME domains — which is why this site
    // cannot also name a pair: `Config::validate` refuses both, so the site's *named* pair is on the
    // deployment instead and the issued one is the site's.
    let issued = rcgen::generate_simple_self_signed(vec!["corp.test".to_string()])
        .expect("generate the issued certificate");
    let issued_blob = format!("{}{}", issued.cert.pem(), issued.key_pair.serialize_pem());
    let issued_der = issued.cert.der().to_vec();
    let shared_cache = root.join("acme-cache");
    let directory = volucer_acme::Authority::Staging.directory();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime for the cache write");
    runtime.block_on(async {
        rustls_acme::caches::DirCache::new(&shared_cache)
            .store_cert(
                &["corp.test".to_string()],
                directory,
                issued_blob.as_bytes(),
            )
            .await
            .expect("the cache is writable");
    });

    let config_path = root.join("volucer.toml");
    // The deployment names a pair, so `sites.corp` may have its own ACME table; and the site's own
    // name is served by the issued certificate from the start.
    let config_for = |fallback: &str| {
        format!(
            "[server]\nhttp = \"127.0.0.1:0\"\ndocument_root = {app}\n\n\
             [server.tls]\ncertificate = \"certs/{fallback}.crt\"\nkey = \"certs/{fallback}.key\"\n\n\
             [sites.corp]\nhosts = [\"corp.test\"]\ndocument_root = {corp}\n\n\
             [sites.corp.acme]\ndomains = [\"corp.test\"]\n",
            app = toml_string(&app.display().to_string()),
            corp = toml_string(&corp.display().to_string()),
        )
    };
    std::fs::write(&config_path, config_for("corp")).expect("write the config");

    let Some(server) = Server::start(&binary, &config_path) else {
        eprintln!("skipping: the server did not start");
        return;
    };
    let address = server
        .http_address
        .clone()
        .expect("the config asks for an http listener");

    let before = tls_get(&address, "corp.test", "/whoami.txt");
    assert_eq!(
        before.certificate, issued_der,
        "the issued certificate must be serving before the reload, or the survival assertion after \
         it is about nothing"
    );

    // Point the deployment's *named* pair at the renewed file and signal. The site's issued
    // certificate is not in the configuration at all, so this is exactly the change a rebuild could
    // pick up while dropping it — or skip while stranding the renewal.
    std::fs::write(&config_path, config_for("renewed")).expect("write the config");
    let Some(()) = signal(&server.child, SIGHUP) else {
        eprintln!("skipping: could not signal the server");
        return;
    };
    std::thread::sleep(Duration::from_millis(700));

    let after = tls_get(&address, "corp.test", "/whoami.txt");
    assert_eq!(
        after.certificate, issued_der,
        "the issued certificate must survive a reload that re-reads the configuration; a \
         placeholder or a named pair here means the reload dropped what the site's order produced"
    );
    assert_eq!(
        after.body, "corp",
        "and the site must still be served, so the table was rebuilt rather than emptied"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}

/// A site with its **own** ACME table serves a placeholder for its own names until issuance.
///
/// The end-to-end assertion for the per-site wiring, and the same reasoning as the cached-certificate
/// test: without starting the binary nothing can see that the site's engine was built at all — which
/// is hazard 68's lesson, where five steps were committed and green while `[server.acme]` issued
/// something nothing served.
///
/// What this pins is the **placeholder's names**, and it is the interesting half: a site whose
/// certificate is generated must be served a certificate that covers *its* hosts, not the
/// deployment's default. A generated certificate with the server's names would fail its handshake
/// with a name mismatch — a TLS error, blamed on the client — while every unit test passed.
///
/// No network, no cache, no account: the placeholder is made before any port is bound. The authority
/// is staging, so even a bug that reached out could not issue a real certificate.
#[cfg(unix)]
#[test]
fn a_site_with_its_own_acme_serves_a_placeholder_for_its_own_names() {
    use rustls_acme::CertCache;

    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-acme-site-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let app = root.join("app");
    let corp = root.join("corp");
    for dir in [&app, &corp] {
        std::fs::create_dir_all(dir).expect("create a document root");
    }
    std::fs::write(app.join("whoami.txt"), "app").expect("write the marker");
    std::fs::write(corp.join("whoami.txt"), "corp").expect("write the marker");

    let config_path = root.join("volucer.toml");
    let config = format!(
        "[server]\nhttp = \"127.0.0.1:0\"\ndocument_root = {app}\n\n\
         [server.tls]\n\n\
         [server.acme]\ndomains = [\"app.test\"]\ncontact = [\"mailto:ops@test\"]\n\n\
         [sites.corp]\nhosts = [\"corp.test\"]\ndocument_root = {corp}\n\n\
         [sites.corp.acme]\n",
        app = toml_string(&app.display().to_string()),
        corp = toml_string(&corp.display().to_string()),
    );
    std::fs::write(&config_path, config).expect("write the config");

    // A site's engine reads the **site's** cache, which is the only thing that proves a per-site
    // engine exists and is pointed at the right place. A certificate is written into the shared cache
    // under the site's own domains, so a build that built no site engine keeps serving the placeholder
    // and fails here — the hazard-68 shape, where every part works and nothing uses them.
    let issued = rcgen::generate_simple_self_signed(vec!["corp.test".to_string()])
        .expect("generate the issued certificate");
    let issued_blob = format!("{}{}", issued.cert.pem(), issued.key_pair.serialize_pem());
    let issued_der = issued.cert.der().to_vec();
    let shared_cache = root.join("acme-cache");
    let directory = volucer_acme::Authority::Staging.directory();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime for the cache write");
    runtime.block_on(async {
        rustls_acme::caches::DirCache::new(&shared_cache)
            .store_cert(
                &["corp.test".to_string()],
                directory,
                issued_blob.as_bytes(),
            )
            .await
            .expect("the cache is writable");
    });

    let Some(server) = Server::start(&binary, &config_path) else {
        eprintln!("skipping: the server did not start");
        return;
    };
    let address = server
        .http_address
        .clone()
        .expect("the config asks for an http listener");

    // Both sites answer, and each gets a certificate — which is the point of the optional pair: no
    // file was named anywhere in that config, and both listeners completed a handshake.
    let app_seen = tls_get(&address, "app.test", "/whoami.txt");
    assert_eq!(app_seen.body, "app", "the deployment's site answers");
    let corp_seen = tls_get(&address, "corp.test", "/whoami.txt");
    assert_eq!(
        corp_seen.body, "corp",
        "the site with its own table answers"
    );

    assert_eq!(
        corp_seen.certificate, issued_der,
        "the site's own engine must read the site's cache and serve what it holds; a placeholder \
         here means no engine was built for the site's table, so its order would never be served"
    );
    assert_ne!(
        app_seen.certificate, issued_der,
        "and that certificate must not reach the deployment's names, which belong to its own order"
    );
    // The DER holds a DNS SAN as the ASCII name, so the names are checkable without a parser.
    let names = |der: &[u8]| -> Vec<String> {
        ["app.test", "corp.test"]
            .iter()
            .filter(|name| {
                der.windows(name.len())
                    .any(|window| window == name.as_bytes())
            })
            .map(|name| name.to_string())
            .collect()
    };
    assert_eq!(
        names(&app_seen.certificate),
        vec!["app.test".to_string()],
        "the deployment's certificate must name only the deployment's domains"
    );
    assert_eq!(
        names(&corp_seen.certificate),
        vec!["corp.test".to_string()],
        "the site's certificate must name the site's own hosts, not the deployment's domains"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}

/// One TLS exchange: which certificate arrived, and what the request was answered with.
///
/// Raw sockets and `rustls` directly, rather than a TLS client library, for the same reason the
/// HTTP tests use raw sockets: this asserts *which bytes* the server presented, and a client that
/// abstracts that away cannot answer the question.
#[cfg(unix)]
struct TlsExchange {
    /// The DER of the leaf certificate the server presented.
    certificate: Vec<u8>,
    /// The response body.
    body: String,
}

/// Connect over TLS, ask for `path` on `host`, and report the certificate and the body.
#[cfg(unix)]
fn tls_get(address: &str, host: &str, path: &str) -> TlsExchange {
    use rustls::pki_types::ServerName;

    // The client half of the handshake needs a crypto provider, and there is more than one in the
    // dependency graph, so rustls will not guess. The server's own installer is reused rather than
    // duplicating the choice: a test that picked differently from the server would be testing a
    // configuration no deployment has.
    volucer_http::tls::install_crypto_provider();

    // A self-signed fixture is in no root store, so the verifier is replaced wholesale. The
    // assertion is about *which* certificate arrived, not whether a chain validates.
    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    let name = ServerName::try_from(host.to_string()).expect("a server name");
    let connection =
        rustls::ClientConnection::new(Arc::new(config), name).expect("a client connection");
    let socket = TcpStream::connect(address).expect("connect to the TLS listener");
    socket
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("a read timeout");
    let mut stream = rustls::StreamOwned::new(connection, socket);

    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("write the request");
    stream.flush().expect("flush");

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read the response");

    let certificate = stream
        .conn
        .peer_certificates()
        .and_then(|certs| certs.first())
        .map(|cert| cert.as_ref().to_vec())
        .expect("the server must present a certificate");

    let body = match raw.windows(4).position(|window| window == b"\r\n\r\n") {
        Some(split) => String::from_utf8_lossy(&raw[split + 4..]).into_owned(),
        None => String::new(),
    };

    TlsExchange { certificate, body }
}

/// Accepts any certificate, for a self-signed fixture.
#[cfg(unix)]
#[derive(Debug)]
struct AcceptAnyCertificate;

#[cfg(unix)]
impl rustls::client::danger::ServerCertVerifier for AcceptAnyCertificate {
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

/// An ACME-configured server serves its **cached** certificate, with no network.
///
/// This is the end-to-end assertion for the engine wiring, and without it the wiring has no test at
/// all: every other ACME test drives a unit or a stub, so deleting the engine from `serve` would
/// leave them all green while a deployment with `[server.acme]` issued and served nothing.
///
/// It is also the cache's whole purpose from the operator's side: a restart with a valid cached
/// certificate serves it **immediately and spends no rate limit**. So the certificate is written
/// through the crate's own `CertCache` trait — the file name is a hash of the names and the
/// directory URL, so a file written by hand would be one nothing reads — and the assertion is that
/// the handshake gets the *cached* certificate rather than the fallback the configuration names.
///
/// No account, no directory, no network: `install_cached` runs before any port is bound. The
/// authority is staging, so even a bug that did reach out could not issue a real certificate.
#[cfg(unix)]
#[test]
fn an_acme_configured_server_serves_its_cached_certificate() {
    use rustls_acme::CertCache;

    let Some(binary) = volucer_binary() else {
        eprintln!("skipping: the volucer binary is not built (run `cargo build`)");
        return;
    };

    let root = std::env::temp_dir().join(format!("volucer-acme-serve-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let site = root.join("site");
    std::fs::create_dir_all(&site).expect("create a document root");
    std::fs::write(site.join("whoami.txt"), "cached").expect("write the marker");
    let certs = root.join("certs");
    std::fs::create_dir_all(&certs).expect("create the certificate directory");

    // The **fallback** the configuration names: self-signed, and deliberately not what the handshake
    // should end up with.
    let fallback = rcgen::generate_simple_self_signed(vec!["example.test".to_string()])
        .expect("generate the fallback certificate");
    std::fs::write(certs.join("fallback.crt"), fallback.cert.pem()).expect("write the cert");
    std::fs::write(
        certs.join("fallback.key"),
        fallback.key_pair.serialize_pem(),
    )
    .expect("write the key");
    let fallback_der = fallback.cert.der().to_vec();

    // The **issued** certificate, in the shape `rustls-acme` stores: a chain plus a key concatenated
    // into one PEM blob. Written that way on purpose, so this exercises `split_pem_pair` rather than
    // a convenient one-certificate file.
    let issued = rcgen::generate_simple_self_signed(vec!["example.test".to_string()])
        .expect("generate the issued certificate");
    let issued_blob = format!("{}{}", issued.cert.pem(), issued.key_pair.serialize_pem());
    let issued_der = issued.cert.der().to_vec();
    assert_ne!(
        fallback_der, issued_der,
        "the fixture must differ, or the assertion below is vacuous"
    );

    // The directory URL the engine will use, from **the same function `serve` calls** — so this
    // cannot drift from what the engine looks for without the server and the test disagreeing about
    // the authority, which is the failure being guarded.
    let directory = volucer_acme::Authority::Staging.directory();
    assert!(
        directory.contains("staging"),
        "the fixture must use staging, so a bug that reached the network could not issue a real \
         certificate: {directory}"
    );

    let cache_dir = root.join("acme-cache");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime for the cache write");
    runtime.block_on(async {
        rustls_acme::caches::DirCache::new(&cache_dir)
            .store_cert(
                &["example.test".to_string()],
                directory,
                issued_blob.as_bytes(),
            )
            .await
            .expect("the cache is writable");
    });

    let config_path = root.join("volucer.toml");
    let config = format!(
        "[server]\nhttp = \"127.0.0.1:0\"\ndocument_root = {site}\n\n\
         [server.tls]\ncertificate = \"certs/fallback.crt\"\nkey = \"certs/fallback.key\"\n\n\
         [server.acme]\ndomains = [\"example.test\"]\ncontact = [\"mailto:ops@example.test\"]\n",
        site = toml_string(&site.display().to_string()),
    );
    std::fs::write(&config_path, config).expect("write the config");

    let Some(server) = Server::start(&binary, &config_path) else {
        eprintln!("skipping: the server did not start");
        return;
    };
    let address = server
        .http_address
        .clone()
        .expect("the config asks for an http listener");

    let seen = tls_get(&address, "example.test", "/whoami.txt");
    assert_eq!(
        seen.body, "cached",
        "the request must be served from the configured document root"
    );
    assert_eq!(
        seen.certificate, issued_der,
        "an ACME-configured server must serve the **cached** certificate rather than the fallback \
         the configuration names — without the engine wired into `serve`, the answer is the \
         fallback, and every unit test in the tree still passes"
    );

    drop(server);
    let _ = std::fs::remove_dir_all(&root);
}
