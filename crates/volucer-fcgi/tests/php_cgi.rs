//! End-to-end tests against a real `php-cgi` process.
//!
//! These are the tests that prove the codec and client work against PHP rather
//! than against our own fixtures. They are skipped, with a printed reason, when
//! no `php-cgi` binary is available, so the suite stays green on a machine
//! without PHP.

use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use volucer_fcgi::{build_params, Client, Endpoint, Request, RequestInfo, Routing};

/// Find `php-cgi` on `PATH`.
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

/// Ask the OS for an unused port, then release it.
///
/// Racy in principle, but the window is microseconds and the alternative is a
/// hardcoded port that collides with whatever else is running.
///
/// **The race is real and CI found it.** These tests run in parallel inside one binary, so two of
/// them can be handed the same ephemeral port before either binds it: on `ubuntu-latest` the
/// `a_status_header_from_the_script_is_preserved` test connected to a `php-cgi` that belonged to a
/// different test, which answered the readiness probe and then dropped the connection as
/// `ConnectionReset` — the process it actually spoke to had already been killed by its owner's
/// `Drop`, and php-cgi accepts one connection at a time. It is intermittent (it passed on a rerun
/// and on Windows) which is exactly what a port race looks like.
///
/// `crates/volucer-server/tests/common/mod.rs` solved this for the pool tests with a process-wide
/// mutex, for the same reason and with the same wording. This is that fix, applied where the same
/// helper exists. Not a retry: retrying would hide the collision rather than remove it.
fn free_port() -> u16 {
    let _serial = port_lock();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    listener
        .local_addr()
        .expect("read the bound address")
        .port()
}

/// Serialise the tests that take a port from the OS.
///
/// Only the *reservation* is serialised, not the process that later binds it: the window is the
/// gap between `free_port` returning and `php-cgi` calling `bind`, and two tests entering that gap
/// at once is what produced the CI failure above.
fn port_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The `php-cgi` fixture, reachable over whichever transport the platform offers.
struct PhpCgi {
    child: Child,
    endpoint: Endpoint,
}

impl PhpCgi {
    fn start() -> Option<Self> {
        let binary = find_php_cgi()?;
        let port = free_port();

        let child = Command::new(&binary)
            .arg("-b")
            .arg(format!("127.0.0.1:{port}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|err| panic!("failed to start {}: {err}", binary.display()));

        let mut server = PhpCgi {
            child,
            endpoint: Endpoint::Tcp(format!("127.0.0.1:{port}")),
        };
        if !server.wait_until_listening(Duration::from_secs(10)) {
            panic!("php-cgi did not start listening on port {port}");
        }
        Some(server)
    }

    /// Start `php-cgi` bound to a named pipe instead of a port.
    ///
    /// `label` distinguishes the pipe from every other test's, because a pipe name
    /// is a global namespace: two tests sharing one name bind two servers onto the
    /// same pipe (php-cgi creates it with `PIPE_UNLIMITED_INSTANCES`) and then race
    /// for clients, and killing one server breaks the other's connection. Tests in
    /// one binary run in parallel, so this is not hypothetical.
    ///
    /// `PHP_FCGI_CHILDREN=0` because the variable is read from php-cgi's own
    /// environment and a host may have exported it for php-fpm: its default of 1
    /// would fork a child per connection, and 20 under it — see the pool's
    /// `spawn_slot` for the measurement. One process, one pipe, is what these tests
    /// are about.
    #[cfg(windows)]
    fn start_on_pipe(label: &str) -> Option<Self> {
        let binary = find_php_cgi()?;
        let name = pipe_name(label);

        let child = Command::new(&binary)
            .arg("-b")
            .arg(&name)
            .env("PHP_FCGI_CHILDREN", "0")
            .env("PHP_FCGI_MAX_REQUESTS", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|err| panic!("failed to start {}: {err}", binary.display()));

        let mut server = PhpCgi {
            child,
            endpoint: Endpoint::Pipe(name),
        };
        if !server.wait_until_listening(Duration::from_secs(10)) {
            panic!(
                "php-cgi did not start listening on pipe {}",
                server.endpoint
            );
        }
        Some(server)
    }

    fn wait_until_listening(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("poll php-cgi") {
                panic!("php-cgi exited during startup with {status}");
            }
            if self
                .endpoint
                .connect(Some(Duration::from_millis(250)))
                .is_ok()
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn endpoint(&self) -> Endpoint {
        self.endpoint.clone()
    }
}

/// A pipe name unique to this process *and* this test, so two tests running in
/// parallel cannot bind the same pipe.
#[cfg(windows)]
fn pipe_name(label: &str) -> String {
    format!(r"\\.\pipe\volucer-it-{}-{label}", std::process::id())
}

impl Drop for PhpCgi {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Write a temporary PHP script and return its path.
fn write_script(name: &str, body: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("volucer-it-{name}-{}.php", std::process::id()));
    let mut file = std::fs::File::create(&path).expect("create the test script");
    file.write_all(body.as_bytes()).expect("write the script");
    path
}

/// Build the CGI environment php-cgi needs for a script.
///
/// `body_len` becomes `CONTENT_LENGTH`. This is not optional: without it PHP
/// leaves `php://input` empty and never runs the request-body parser, so the raw
/// bytes we send as `STDIN` are silently dropped. This is exactly the trap
/// `cgi-env` has to avoid for every request, not just tests.
fn params_for(script: &Path, method: &str, body_len: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    let pairs = vec![
        ("GATEWAY_INTERFACE".to_string(), "FastCGI/1.0".to_string()),
        ("REQUEST_METHOD".to_string(), method.to_string()),
        ("SCRIPT_FILENAME".to_string(), script.display().to_string()),
        ("SCRIPT_NAME".to_string(), "/index.php".to_string()),
        ("REQUEST_URI".to_string(), "/index.php".to_string()),
        ("SERVER_PROTOCOL".to_string(), "HTTP/1.1".to_string()),
        ("SERVER_SOFTWARE".to_string(), "volucer".to_string()),
        ("REMOTE_ADDR".to_string(), "127.0.0.1".to_string()),
        ("SERVER_NAME".to_string(), "localhost".to_string()),
        ("SERVER_PORT".to_string(), "8080".to_string()),
        ("CONTENT_LENGTH".to_string(), body_len.to_string()),
        (
            "DOCUMENT_ROOT".to_string(),
            std::env::temp_dir().display().to_string(),
        ),
    ];
    pairs
        .into_iter()
        .map(|(name, value)| (name.as_bytes().to_vec(), value.as_bytes().to_vec()))
        .collect()
}

/// The value of a response header, compared case-insensitively.
fn header<'a>(headers: &'a [(&'a str, &'a str)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| *value)
}

#[test]
fn serves_a_script_over_fastcgi() {
    let Some(server) = PhpCgi::start() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };

    let script = write_script("basic", r#"<?php echo "VOLUCER-OK";"#);
    let mut client = Client::connect(&server.endpoint(), Some(Duration::from_secs(5)))
        .expect("connect to php-cgi");

    let response = client
        .request(&Request {
            params: params_for(&script, "GET", 0),
            body: Vec::new(),
            keep_conn: true,
        })
        .expect("php-cgi answered");

    response
        .check_protocol()
        .expect("backend completed the request");

    let stdout = String::from_utf8_lossy(&response.stdout);
    assert!(
        stdout.contains("VOLUCER-OK"),
        "body missing from response: {stdout:?}"
    );

    let (status, headers) = response.parse_status_headers();
    assert_eq!(status, 200, "php-cgi defaults to 200: {stdout:?}");
    assert!(
        header(&headers, "Content-type").is_some(),
        "php-cgi sends a Content-type header: {headers:?}"
    );

    let _ = std::fs::remove_file(&script);
}

#[test]
fn passes_the_request_body_through_as_stdin() {
    let Some(server) = PhpCgi::start() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };

    // The whole point of not parsing bodies ourselves: php-cgi turns the raw
    // STDIN into php://input, and would do the same for multipart and $_FILES.
    let script = write_script(
        "body",
        r#"<?php echo "BODY="; echo file_get_contents("php://input");"#,
    );

    let mut client = Client::connect(&server.endpoint(), Some(Duration::from_secs(5)))
        .expect("connect to php-cgi");

    let body = b"raw-body-bytes".to_vec();
    let response = client
        .request(&Request {
            params: params_for(&script, "POST", body.len()),
            body,
            keep_conn: true,
        })
        .expect("php-cgi answered");

    response
        .check_protocol()
        .expect("backend completed the request");
    let stdout = String::from_utf8_lossy(&response.stdout);
    assert!(
        stdout.contains("BODY=raw-body-bytes"),
        "STDIN did not reach php://input: {stdout:?}"
    );

    let _ = std::fs::remove_file(&script);
}

#[test]
fn a_status_header_from_the_script_is_preserved() {
    let Some(server) = PhpCgi::start() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };

    let script = write_script(
        "status",
        "<?php header('X-Volucer: yes'); http_response_code(201); echo 'created';",
    );

    let mut client = Client::connect(&server.endpoint(), Some(Duration::from_secs(5)))
        .expect("connect to php-cgi");

    let response = client
        .request(&Request {
            params: params_for(&script, "GET", 0),
            body: Vec::new(),
            keep_conn: true,
        })
        .expect("php-cgi answered");

    let stdout = String::from_utf8_lossy(&response.stdout);
    let (status, headers) = response.parse_status_headers();
    assert_eq!(status, 201, "{stdout:?}");
    assert_eq!(header(&headers, "X-Volucer"), Some("yes"), "{headers:?}");

    let _ = std::fs::remove_file(&script);
}

#[test]
fn a_missing_script_is_reported_by_the_backend_not_hung_on() {
    let Some(server) = PhpCgi::start() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };

    let missing = std::env::temp_dir().join("volucer-does-not-exist.php");
    let mut client = Client::connect(&server.endpoint(), Some(Duration::from_secs(5)))
        .expect("connect to php-cgi");

    let response = client
        .request(&Request {
            params: params_for(&missing, "GET", 0),
            body: Vec::new(),
            keep_conn: true,
        })
        .expect("php-cgi still answers with an error document");

    let stdout = String::from_utf8_lossy(&response.stdout);
    let (status, _) = response.parse_status_headers();
    assert!(
        status == 404 || stdout.contains("No input file specified") || stdout.contains("not found"),
        "expected an error response, got status {status}: {stdout:?}"
    );
}

#[test]
fn get_values_is_answered_or_declined_without_hanging() {
    let Some(server) = PhpCgi::start() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };

    let mut client = Client::connect(&server.endpoint(), Some(Duration::from_secs(5)))
        .expect("connect to php-cgi");

    // php-cgi may answer with a result or decline with FCGI_UNKNOWN_TYPE; both
    // are fine. What matters is that it does not hang the client.
    match client.get_values([&b"max_connections"[..]]) {
        Ok(values) => eprintln!("php-cgi answered GET_VALUES with {values:?}"),
        Err(err) => eprintln!("php-cgi declined GET_VALUES: {err}"),
    }
}

/// A request served through the real CGI environment builder rather than
/// hand-written params: this is the end-to-end path a listener will take.
#[test]
fn the_cgi_environment_builder_produces_a_working_request() {
    let Some(server) = PhpCgi::start() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };

    // A front controller that reports what PHP actually received.
    let root = std::env::temp_dir().join(format!("volucer-cgi-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    std::fs::write(
        root.join("index.php"),
        r#"<?php
echo "METHOD=", $_SERVER['REQUEST_METHOD'] ?? '?', "\n";
echo "URI=", $_SERVER['REQUEST_URI'] ?? '?', "\n";
echo "PATH_INFO=", $_SERVER['PATH_INFO'] ?? '-', "\n";
echo "QUERY=", $_SERVER['QUERY_STRING'] ?? '-', "\n";
echo "HOST=", $_SERVER['HTTP_X_CUSTOM'] ?? '-', "\n";
echo "POST=", $_POST['field'] ?? '-', "\n";
echo "INPUT=", file_get_contents('php://input');
"#,
    )
    .expect("write the front controller");

    let routing = Routing {
        document_root: root.clone(),
        front_controller: Some("index.php".to_string()),
        ..Routing::default()
    };

    let body = b"field=value".to_vec();
    let info = RequestInfo {
        method: "POST".to_string(),
        // A route that must reach the front controller with PATH_INFO.
        uri: "/some/deep/route?x=1".to_string(),
        query_string: "x=1".to_string(),
        headers: vec![
            ("X-Custom".to_string(), "present".to_string()),
            (
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            ),
        ],
        body_len: body.len(),
        ..RequestInfo::default()
    };

    let params = build_params(&info, &routing).expect("params build");

    let mut client = Client::connect(&server.endpoint(), Some(Duration::from_secs(5)))
        .expect("connect to php-cgi");

    let response = client
        .request(&Request {
            params,
            body,
            keep_conn: true,
        })
        .expect("php-cgi answered");

    response.check_protocol().expect("completed");
    let stdout = String::from_utf8_lossy(&response.stdout);

    assert!(stdout.contains("METHOD=POST"), "{stdout}");
    assert!(stdout.contains("URI=/some/deep/route?x=1"), "{stdout}");
    assert!(stdout.contains("PATH_INFO=/some/deep/route"), "{stdout}");
    assert!(stdout.contains("QUERY=x=1"), "{stdout}");
    assert!(stdout.contains("HOST=present"), "{stdout}");
    // The two that prove CONTENT_LENGTH and CONTENT_TYPE did their job: PHP
    // parsed the body into $_POST *and* left the raw bytes on php://input.
    assert!(stdout.contains("POST=value"), "{stdout}");
    assert!(stdout.contains("INPUT=field=value"), "{stdout}");

    let _ = std::fs::remove_dir_all(&root);
}

/// A named pipe is a first-class transport, not a Windows curiosity.
///
/// On Windows `php-cgi -b` takes a pipe name instead of `host:port`, and the SAPI
/// detects which it was given with `!GetNamedPipeInfo(..)`. This is the same
/// request the TCP test above sends, over that pipe, and it is the end-to-end
/// proof that `Endpoint::Pipe` reaches real PHP and gets real bytes back.
///
/// It is deliberately *not* a test of the client's protocol handling — that is
/// covered above — so it asserts only that the transport carried a complete
/// exchange and that PHP's answer is intact.
#[cfg(windows)]
#[test]
fn serves_a_script_over_a_named_pipe() {
    let Some(server) = PhpCgi::start_on_pipe("serve") else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };

    let script = write_script("pipe", r#"<?php echo "PIPE-OK";"#);
    let mut client = Client::connect(&server.endpoint(), Some(Duration::from_secs(5)))
        .expect("connect to the php-cgi pipe");

    let response = client
        .request(&Request {
            params: params_for(&script, "GET", 0),
            body: Vec::new(),
            keep_conn: true,
        })
        .expect("php-cgi answered over the pipe");

    response
        .check_protocol()
        .expect("the backend completed the request over a pipe");

    let stdout = String::from_utf8_lossy(&response.stdout);
    assert!(
        stdout.contains("PIPE-OK"),
        "body missing from the pipe response: {stdout:?}"
    );
    let (status, headers) = response.parse_status_headers();
    assert_eq!(
        status, 200,
        "php-cgi defaults to 200 over a pipe too: {stdout:?}"
    );
    assert!(
        header(&headers, "Content-type").is_some(),
        "php-cgi sends a Content-type header over a pipe: {headers:?}"
    );

    let _ = std::fs::remove_file(&script);
}

/// A pipe read that is never answered times out; it does not hang for ever.
///
/// This is the property the whole overlapped-I/O arrangement exists for, and the
/// one this project has already paid for once: a request path with no read
/// deadline sat for **17 minutes** in a test that should have failed instead
/// (hazard 43). A blocking `ReadFile` on a pipe has no deadline of its own, so
/// without the check below a backend that accepts and then goes quiet would hold
/// a request thread indefinitely.
///
/// The connection is made and *nothing* is sent, so `php-cgi` has no request to
/// answer and the read cannot complete. The assertion is on wall-clock time, not
/// just the error kind, because a timeout that arrives after two minutes is not a
/// timeout in any useful sense.
#[cfg(windows)]
#[test]
fn a_pipe_read_without_an_answer_times_out_rather_than_hanging() {
    use std::io::Read as _;

    let Some(server) = PhpCgi::start_on_pipe("timeout") else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };

    let deadline = Duration::from_millis(300);
    let mut stream = server
        .endpoint()
        .connect(Some(deadline))
        .expect("connect to the php-cgi pipe");

    let mut buf = [0u8; 8];
    let started = Instant::now();
    let outcome = stream.read(&mut buf);
    let elapsed = started.elapsed();

    let err = outcome.expect_err("an unanswered read must not report success");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::TimedOut,
        "a read with no answer should time out, not fail some other way: {err:?}"
    );
    assert!(
        elapsed >= deadline,
        "the read returned before its deadline: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "the deadline was not honoured: waited {elapsed:?} for a {deadline:?} timeout"
    );

    // The connection must still be usable rather than left with a stale completion
    // that the next operation would misread. A second read on the *same* stream is
    // what proves it: it has to drain the abandoned operation first, then time out
    // again on its own rather than returning a stale result or blocking.
    let started = Instant::now();
    let outcome = stream.read(&mut buf);
    let elapsed = started.elapsed();
    let err = outcome.expect_err("the second read must not report success either");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::TimedOut,
        "the second read should time out like the first: {err:?}"
    );
    assert!(
        elapsed >= deadline && elapsed < Duration::from_secs(3),
        "the second read did not honour its deadline: {elapsed:?}"
    );
}
///
/// This is the property that makes a pipe usable behind a supervisor: a stale
/// configuration or a backend that never came up has to be an error at connect
/// time. The name carries a process id and a nanosecond counter so no other
/// process can be holding it.
/// A pipe with nobody listening on it fails; it does not wait for ever.
///
/// This is the property that makes a pipe usable behind a supervisor: a stale
/// configuration or a backend that never came up has to be an error at connect
/// time. The name carries a process id and a nanosecond counter so no other
/// process can be holding it.
#[cfg(windows)]
#[test]
fn connecting_to_an_absent_pipe_fails_promptly() {
    let name = format!(
        r"\\.\pipe\volucer-absent-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let endpoint = Endpoint::parse(&name).expect("a pipe name parses");

    let started = Instant::now();
    let outcome = endpoint.connect(Some(Duration::from_millis(500)));
    let elapsed = started.elapsed();

    assert!(outcome.is_err(), "an absent pipe must not connect");
    assert!(
        elapsed < Duration::from_secs(3),
        "an absent pipe must fail promptly, took {elapsed:?}"
    );
}
