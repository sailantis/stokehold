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

use stokehold_fcgi::{build_params, Client, Endpoint, Request, RequestInfo, Routing};

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
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    listener
        .local_addr()
        .expect("read the bound address")
        .port()
}

/// A `php-cgi` server that is killed when the test ends, however it ends.
struct PhpCgi {
    child: Child,
    port: u16,
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

        let mut server = PhpCgi { child, port };
        if !server.wait_until_listening(Duration::from_secs(10)) {
            panic!("php-cgi did not start listening on port {port}");
        }
        Some(server)
    }

    fn wait_until_listening(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("poll php-cgi") {
                panic!("php-cgi exited during startup with {status}");
            }
            if std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint::Tcp(format!("127.0.0.1:{}", self.port))
    }
}

impl Drop for PhpCgi {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Write a temporary PHP script and return its path.
fn write_script(name: &str, body: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("stokehold-it-{name}-{}.php", std::process::id()));
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
        ("SERVER_SOFTWARE".to_string(), "stokehold".to_string()),
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
fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

#[test]
fn serves_a_script_over_fastcgi() {
    let Some(server) = PhpCgi::start() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };

    let script = write_script("basic", r#"<?php echo "STOKEHOLD-OK";"#);
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
        stdout.contains("STOKEHOLD-OK"),
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
        "<?php header('X-Stokehold: yes'); http_response_code(201); echo 'created';",
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
    assert_eq!(header(&headers, "X-Stokehold"), Some("yes"), "{headers:?}");

    let _ = std::fs::remove_file(&script);
}

#[test]
fn a_missing_script_is_reported_by_the_backend_not_hung_on() {
    let Some(server) = PhpCgi::start() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };

    let missing = std::env::temp_dir().join("stokehold-does-not-exist.php");
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
    let root = std::env::temp_dir().join(format!("stokehold-cgi-e2e-{}", std::process::id()));
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
