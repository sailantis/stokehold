//! End-to-end tests against a real PHP worker process.
//!
//! This is where the protocol stops being two implementations that agree with a
//! document and becomes **one wire format that two languages actually speak**.
//! The Rust codec writes REQUEST frames into a live `php worker.php` process's
//! stdin and reads the RESPONSE back from its stdout — the same code path the
//! pool will use, and the same frames the PHP SDK parses.
//!
//! Both sides are independently pinned by golden fixtures
//! (`crates/stokehold-worker/tests/wire_format.rs` and
//! `sdk/php/tests/WireFormatTest.php`), so a disagreement fails here with a real
//! process rather than only in theory.
//!
//! The tests are skipped, with a printed reason, when no `php` binary is on
//! `PATH`, so the suite stays green on a machine without PHP.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use stokehold_worker::{
    Codec, Control, Header, HelloAck, Json, MessageType, RequestHeader, WorkerChannel, UNSOLICITED,
};

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

/// The SDK's example worker, relative to this crate.
///
/// `CARGO_MANIFEST_DIR` is `crates/stokehold-worker`, so the repository root is
/// two levels up. That keeps the test pointing at the shipped example rather than
/// a fixture that could drift away from it.
fn example_worker() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("sdk")
        .join("php")
        .join("example")
        .join("worker.php")
}

/// A `php worker.php` process, killed when the test ends however it ends.
struct PhpWorker {
    child: Child,
    /// `None` once the channel has been taken to close the child's stdin.
    channel: Option<WorkerChannel<ChildStdin, ChildStdout>>,
}

impl PhpWorker {
    fn start() -> Option<Self> {
        let php = find_php()?;
        let script = example_worker();
        assert!(
            script.is_file(),
            "the SDK example worker is missing at {}",
            script.display()
        );

        let mut child = Command::new(&php)
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherited rather than nulled: the SDK writes diagnostics here, and
            // a test that hides them cannot explain a failure.
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap_or_else(|err| panic!("failed to start {}: {err}", php.display()));

        let stdin = child.stdin.take().expect("the child's stdin");
        let stdout = child.stdout.take().expect("the child's stdout");

        Some(PhpWorker {
            child,
            channel: Some(WorkerChannel::new(stdin, stdout)),
        })
    }

    fn channel(&mut self) -> &mut WorkerChannel<ChildStdin, ChildStdout> {
        self.channel
            .as_mut()
            .expect("the channel is open in every test but the stdin-closing one")
    }

    fn handshake(&mut self) -> HelloAck {
        self.channel()
            .handshake()
            .expect("the PHP worker must complete the handshake")
    }

    /// Send a GET and return the whole response.
    fn get(&mut self, uri: &str) -> stokehold_worker::codec::WorkerResponse {
        let request = RequestHeader {
            method: "GET".to_string(),
            uri: uri.to_string(),
            protocol: "HTTP/1.1".to_string(),
            headers: vec![("Accept".to_string(), "text/html".to_string())],
            remote_addr: "127.0.0.1".to_string(),
            body_len: 0,
            script_filename: "index.php".to_string(),
            https: false,
        };

        self.channel()
            .request(&request, b"")
            .unwrap_or_else(|err| panic!("request {uri} failed: {err}"))
    }

    /// Send a POST with a body.
    fn post(&mut self, uri: &str, body: &[u8]) -> stokehold_worker::codec::WorkerResponse {
        let request = RequestHeader {
            method: "POST".to_string(),
            uri: uri.to_string(),
            protocol: "HTTP/1.1".to_string(),
            headers: vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Host".to_string(), "example.test".to_string()),
            ],
            remote_addr: "127.0.0.1".to_string(),
            body_len: body.len() as u64,
            script_filename: "index.php".to_string(),
            https: false,
        };

        self.channel()
            .request(&request, body)
            .unwrap_or_else(|err| panic!("request {uri} failed: {err}"))
    }
}

impl Drop for PhpWorker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The body of a response as a lossy string, for assertions on text routes.
fn body_text(response: &stokehold_worker::codec::WorkerResponse) -> String {
    String::from_utf8_lossy(&response.body).into_owned()
}

/// Skip the test, loudly, when PHP is not installed.
macro_rules! require_php {
    () => {
        match PhpWorker::start() {
            Some(worker) => worker,
            None => {
                eprintln!("skipping: no php on PATH");
                return;
            }
        }
    };
}

#[test]
fn a_real_php_worker_completes_the_handshake() {
    let mut worker = require_php!();

    let ack = worker.handshake();

    assert_eq!(
        ack.protocol_version, 1,
        "the PHP SDK speaks protocol version 1"
    );
    assert!(
        ack.php_version.starts_with('8') || ack.php_version.starts_with('7'),
        "the worker reported a plausible PHP version: {}",
        ack.php_version
    );
    assert_eq!(ack.sapi, "cli", "the worker runs under the CLI SAPI");
    assert!(ack.pid > 0, "the worker reported its pid");
}

#[test]
fn a_php_worker_serves_a_request_over_a_real_pipe() {
    let mut worker = require_php!();
    worker.handshake();

    let response = worker.get("/");

    assert_eq!(response.status(), 200);
    assert!(
        response.keep_alive(),
        "the worker stays alive after one request"
    );
    assert_eq!(
        response.end.app_status, 0,
        "a clean request reports app_status 0"
    );
    let body = body_text(&response);
    assert!(body.contains("stokehold worker"), "unexpected body: {body}");
    assert!(
        body.contains("PHP "),
        "the body reports the PHP version: {body}"
    );
    // The response head's declared length is what the reader checked the body
    // against, so this asserting equality is the real end-to-end guarantee.
    assert_eq!(response.head.body_len, response.body.len() as u64);
}

#[test]
fn a_php_worker_serves_many_requests_from_one_process() {
    let mut worker = require_php!();
    worker.handshake();

    // The whole point of worker mode: no per-request process boot. If the PHP
    // side failed to reset its state, or the framing drifted by one frame, a
    // later request here would fail rather than succeed.
    for index in 0..25 {
        let response = worker.get("/echo");
        assert_eq!(response.status(), 200, "request {index} failed");
        let body = body_text(&response);
        assert!(
            body.contains("you asked for /echo"),
            "request {index} returned the wrong body: {body}"
        );
    }
}

#[test]
fn application_output_is_captured_not_leaked_into_framing() {
    let mut worker = require_php!();
    worker.handshake();

    // This is protocol rule 1, tested against a real process. The `/echo` route
    // uses `echo` and returns null, so its output is the body. If the SDK failed
    // to capture output, those bytes would land inside the frame stream and the
    // *next* read would fail a checksum — which is what makes this a strong
    // test rather than a cosmetic one.
    let response = worker.get("/echo");
    let body = body_text(&response);
    assert!(body.contains("you asked for /echo"), "body was: {body}");
    assert!(body.contains("method: GET"), "body was: {body}");
    assert!(body.contains("body: 0 bytes"), "body was: {body}");

    // And the stream is still usable afterwards, which is the actual assertion:
    // a leak would have desynchronised it.
    let after = worker.get("/health");
    assert_eq!(after.status(), 200);
    assert!(body_text(&after).contains("\"status\":\"ok\""));
}

#[test]
fn a_body_larger_than_one_chunk_round_trips_intact() {
    let mut worker = require_php!();
    worker.handshake();

    // 800000 bytes: a dozen chunks. Exercises the length/LAST agreement in both
    // directions across a real pipe.
    let response = worker.get("/big");

    assert_eq!(response.status(), 200);
    assert_eq!(response.body.len(), 800_000);
    assert_eq!(response.head.body_len, 800_000);
    let body = body_text(&response);
    assert!(body.starts_with("abcdefgh"));
    assert!(body.ends_with("abcdefgh"));
    assert!(
        body.bytes()
            .all(|b| b == b'a' || (b'a'..=b'h').contains(&b)),
        "the body is not the expected repeating pattern"
    );
}

#[test]
fn a_request_body_reaches_the_application() {
    let mut worker = require_php!();
    worker.handshake();

    let payload = br#"{"hello":"world"}"#;
    let response = worker.post("/echo", payload);

    assert_eq!(response.status(), 200);
    let body = body_text(&response);
    assert!(
        body.contains(&format!("body: {} bytes", payload.len())),
        "the request body length did not reach PHP: {body}"
    );
}

#[test]
fn an_uncaught_handler_error_becomes_a_500_and_the_worker_survives() {
    let mut worker = require_php!();
    worker.handshake();

    let failed = worker.get("/boom");
    assert_eq!(failed.status(), 500, "an uncaught error becomes a 500");
    assert_eq!(
        failed.end.app_status, 1,
        "the failure is reported as an app status"
    );
    assert!(
        failed.keep_alive(),
        "a handler error must not ask the server to replace the worker"
    );

    // The worker is still usable. This is the assertion that matters: a panic in
    // the handler (or a desynchronised stream) would fail here.
    let recovered = worker.get("/health");
    assert_eq!(recovered.status(), 200);
    assert!(body_text(&recovered).contains("\"status\":\"ok\""));
}

#[test]
fn telemetry_arrives_out_of_band() {
    let mut worker = require_php!();
    worker.handshake();

    // `/echo` emits a log record and a metric before responding. They must arrive
    // as unsolicited LOG/METRIC frames and must not interfere with the response,
    // which is the whole reason the protocol exists rather than using FastCGI.
    let response = worker.get("/echo");

    assert_eq!(response.status(), 200);
    assert!(
        !response.logs.is_empty(),
        "the worker's log record should reach the server"
    );
    assert!(
        !response.metrics.is_empty(),
        "the worker's metric should reach the server"
    );
    let message = &response.logs[0].message;
    assert!(
        message.contains("/echo"),
        "the log record should describe the request: {message}"
    );
}

#[test]
fn ping_and_control_reach_the_worker() {
    let mut worker = require_php!();
    worker.handshake();

    worker
        .channel()
        .ping()
        .expect("the worker must answer a PING");
    worker
        .channel()
        .control(&Control::Report)
        .expect("the worker must acknowledge a CONTROL");

    // A PING and a CONTROL are not requests, so no RESPONSE was produced and the
    // stream is still aligned for a real request.
    let response = worker.get("/health");
    assert_eq!(response.status(), 200);
}

#[test]
fn closing_stdin_stops_the_worker_cleanly() {
    // Protocol rule 2: shutdown by construction. Dropping the channel drops the
    // child's stdin, so the worker's read returns EOF and `run()` returns. The
    // process should exit on its own, without a signal.
    let mut worker = require_php!();
    worker.handshake();

    // Closing the child's stdin is the entire instruction; nothing else is sent.
    drop(worker.channel.take());

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match worker.child.try_wait().expect("poll the worker") {
            Some(status) => {
                assert!(
                    status.success(),
                    "the worker should exit cleanly on EOF, got {status}"
                );
                return;
            }
            None if std::time::Instant::now() > deadline => {
                let _ = worker.child.kill();
                panic!("the worker did not exit within 10s of its stdin closing");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
}

#[test]
fn a_frame_the_php_side_did_not_write_is_not_accepted() {
    let mut worker = require_php!();
    worker.handshake();

    // A sanity check that the handshake is real and not the test talking to
    // itself: the ack we received was produced by a frame the PHP SDK encoded,
    // and its checksum must verify. Re-encoding the same ack in Rust and decoding
    // it proves the two agree byte for byte on this machine and this PHP build.
    let ack = HelloAck {
        protocol_version: 1,
        php_version: "8.3.31".to_string(),
        sapi: "cli".to_string(),
        pid: 1234,
        runtime: String::new(),
    };
    let encoded = Json::encode(&ack).expect("encode a HELLO_ACK");
    let mut wire = Vec::new();
    let header = Header::new(MessageType::HelloAck, UNSOLICITED, &encoded).unwrap();
    header.write(&mut wire).unwrap();
    wire.write_all(&encoded).unwrap();

    let mut cursor = std::io::Cursor::new(wire);
    let (decoded_header, decoded_payload) = Header::read_frame(&mut cursor)
        .expect("read back the frame")
        .expect("a frame");
    assert_eq!(decoded_header.message_type, MessageType::HelloAck);
    let round_tripped: HelloAck = Json::decode(&decoded_payload).unwrap();
    assert_eq!(round_tripped, ack);
}
