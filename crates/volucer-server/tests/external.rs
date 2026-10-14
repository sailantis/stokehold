//! Integration tests for the `external` backend, which attaches to a FastCGI
//! server it does not own.
//!
//! `php-fpm` does not exist on Windows, so these tests point the backend at a
//! `php-cgi -b` process started by the test itself. That is the same code path:
//! the backend only ever sees a socket it did not create.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use volucer_core::config::{BackendConfig, Config, ExternalConfig, ServerConfig};
use volucer_fcgi::{
    build_params, read_record, write_records, EndRequest, RecordType, Request, RequestInfo,
    Routing, Stream, REQUEST_ID,
};
use volucer_server::Dispatcher;

mod common;
use common::{find_php_cgi, serial, write_script};

/// A FastCGI server the test owns, standing in for an external `php-fpm`.
struct ExternalServer {
    child: Child,
    port: u16,
}

impl ExternalServer {
    fn start(binary: &Path) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a port");
        let port = listener.local_addr().expect("address").port();
        drop(listener);

        let child = Command::new(binary)
            .arg("-b")
            .arg(format!("127.0.0.1:{port}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the stand-in server");

        let server = ExternalServer { child, port };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return server;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the stand-in server never started listening on {port}");
    }

    fn socket(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}

impl Drop for ExternalServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A FastCGI server that answers every connection at once, with nothing to
/// serialise on.
///
/// The `spawn_cgi` stand-in used by the other tests is a real `php-cgi -b`, and a
/// single Linux `php-cgi` serves its connections **one at a time** however many
/// are open: it takes the second request on the connection it is already holding,
/// because the client asked for keep-alive (`PHP_FCGI_CHILDREN` is read only in the
/// Windows branch of `sapi/cgi/cgi_main.c`, so there is no child pool to distribute
/// to). A *real* `php-fpm` is a master over a child pool and answers as many at
/// once as `pm.max_children` allows, which is the thing `ClientPool` exists for —
/// so an `external` backend pointed at a lone `php-cgi` is a test fixture that
/// cannot exhibit the property under test.
///
/// This server can. It accepts, reads a request, sleeps, and replies, all in a
/// thread per connection, and it deliberately does **not** honour keep-alive: one
/// request per connection, then close. That is a stricter backend than `php-fpm`
/// (which is happy to be reused) and it keeps the assertion about the pool rather
/// than about the protocol.
struct ConcurrentServer {
    port: u16,
}

impl ConcurrentServer {
    fn start(sleep: Duration) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a port");
        let port = listener.local_addr().expect("address").port();

        thread::spawn(move || {
            let mut workers = Vec::new();
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                workers.push(thread::spawn(move || {
                    serve_once(Stream::Tcp(stream), sleep)
                }));
            }
        });

        ConcurrentServer { port }
    }

    fn socket(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}

/// Read one FastCGI request and answer it, sleeping first to widen the window in
/// which the pool's concurrency is observable.
fn serve_once(stream: Stream, sleep: Duration) {
    let mut stream = stream;

    // BEGIN_REQUEST, PARAMS, PARAMS-terminator, STDIN, STDIN-terminator.
    for _ in 0..5 {
        match read_record(&mut stream) {
            Ok(Some(_)) => {}
            _ => return,
        }
    }

    thread::sleep(sleep);

    let body = b"Status: 200 OK\r\nContent-Type: text/plain\r\n\r\nCONCURRENT";
    if write_records(&mut stream, RecordType::Stdout, REQUEST_ID, body, true).is_err() {
        return;
    }
    let end = EndRequest::complete();
    let _ = write_records(
        &mut stream,
        RecordType::EndRequest,
        REQUEST_ID,
        &end.encode(),
        false,
    );
}

fn dispatcher_for(socket: &str, root: &Path) -> Dispatcher {
    let config = Config {
        server: ServerConfig {
            http: Some("127.0.0.1:8080".to_string()),
            ..Default::default()
        },
        backends: [(
            "fpm".to_string(),
            BackendConfig::External(ExternalConfig {
                socket: socket.to_string(),
            }),
        )]
        .into_iter()
        .collect(),
        default_backend: Some("fpm".to_string()),
        routes: Vec::new(),
        sites: BTreeMap::new(),
    };
    config.validate().expect("the config is valid");

    let routing = Routing {
        document_root: root.to_path_buf(),
        front_controller: Some("index.php".to_string()),
        ..Routing::default()
    };
    Dispatcher::from_config(&config, routing).expect("the dispatcher starts")
}

fn request_for(root: &Path, marker: &str) -> Request {
    let body = marker.as_bytes().to_vec();
    let info = RequestInfo {
        method: "POST".to_string(),
        uri: "/".to_string(),
        body_len: body.len(),
        ..RequestInfo::default()
    };
    let routing = Routing {
        document_root: root.to_path_buf(),
        front_controller: Some("index.php".to_string()),
        ..Routing::default()
    };
    Request {
        params: build_params(&info, &routing).expect("params build"),
        body,
        keep_conn: true,
    }
}

#[test]
fn an_external_backend_serves_requests_over_its_socket() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };

    let server = ExternalServer::start(&binary);
    let root = std::env::temp_dir().join(format!("volucer-external-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    write_script(
        &root,
        "index.php",
        r#"<?php echo "EXT="; echo file_get_contents("php://input");"#,
    );

    let dispatcher = dispatcher_for(&server.socket(), &root);
    assert_eq!(dispatcher.default_backend(), "fpm");
    assert_eq!(dispatcher.backend_names(), vec!["fpm"]);

    let response = dispatcher
        .dispatch(&request_for(&root, "first"))
        .expect("the external backend answered");
    response.check_protocol().expect("completed");
    let stdout = String::from_utf8_lossy(&response.stdout);
    assert!(stdout.contains("EXT=first"), "{stdout:?}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_external_backend_reuses_its_connection_across_requests() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };

    let server = ExternalServer::start(&binary);
    let root = std::env::temp_dir().join(format!("volucer-reuse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    write_script(
        &root,
        "index.php",
        r#"<?php echo "N="; echo file_get_contents("php://input");"#,
    );

    let dispatcher = dispatcher_for(&server.socket(), &root);

    // Several requests in a row: the backend must not need a new connection each
    // time, and must not fail if the server does close between requests.
    for index in 0..5 {
        let response = dispatcher
            .dispatch(&request_for(&root, &format!("r{index}")))
            .unwrap_or_else(|err| panic!("request {index} failed: {err}"));
        response.check_protocol().expect("completed");
        let stdout = String::from_utf8_lossy(&response.stdout);
        assert!(stdout.contains(&format!("N=r{index}")), "{stdout:?}");
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_external_backend_reconnects_after_the_server_is_replaced() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };

    let mut server = ExternalServer::start(&binary);
    let port = server.port;
    let root = std::env::temp_dir().join(format!("volucer-reconnect-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    write_script(
        &root,
        "index.php",
        r#"<?php echo "RC="; echo file_get_contents("php://input");"#,
    );

    let dispatcher = dispatcher_for(&server.socket(), &root);
    dispatcher
        .dispatch(&request_for(&root, "before"))
        .expect("served before the restart");

    // Restart the server on the same port, the way systemd would reload php-fpm.
    // The dispatcher still holds a connection to the dead process.
    let _ = server.child.kill();
    let _ = server.child.wait();
    std::thread::sleep(Duration::from_millis(200));

    server.child = Command::new(&binary)
        .arg("-b")
        .arg(format!("127.0.0.1:{port}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("restart the stand-in server");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        std::thread::sleep(Duration::from_millis(50));
    }

    // The first request after a restart may hit the stale connection; what
    // matters is that the backend recovers rather than staying broken.
    let mut last_error = None;
    let mut served = false;
    for attempt in 0..3 {
        match dispatcher.dispatch(&request_for(&root, &format!("after{attempt}"))) {
            Ok(response) => {
                let stdout = String::from_utf8_lossy(&response.stdout);
                if stdout.contains(&format!("RC=after{attempt}")) {
                    served = true;
                    break;
                }
                last_error = Some(format!("unexpected body: {stdout:?}"));
            }
            Err(err) => last_error = Some(err.to_string()),
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    assert!(served, "the backend never recovered: {last_error:?}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_socket_that_is_not_listening_fails_the_request_not_the_startup() {
    let _serial = serial();
    let root = std::env::temp_dir().join(format!("volucer-dead-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("create the document root");

    // Attaching to a socket is lazy: the dispatcher must start even when nothing
    // is listening, because php-fpm may be starting alongside us. The failure
    // then belongs to the request.
    let dispatcher = dispatcher_for("127.0.0.1:1", &root);
    let err = dispatcher
        .dispatch(&request_for(&root, "x"))
        .expect_err("nothing is listening on port 1");
    assert!(err.to_string().contains("fpm"), "{err}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn concurrent_requests_reach_the_backend_at_the_same_time() {
    let _serial = serial();

    // The regression test for `docs/plan.md` hazard 22. The backend used to hold
    // one `Mutex<Option<Client>>` for the whole request, so a pool of any size
    // served exactly one request at a time. A backend that can only be talked to
    // through one connection is invisible to a sequential test — every request
    // succeeds and every response is correct — so the assertion has to be about
    // *time*, not about correctness.
    //
    // The backend is `ConcurrentServer` rather than a real `php-cgi`, which is the
    // correction this test needed. A lone Linux `php-cgi` serves one connection at
    // a time, so it serialises four requests no matter how good the pool is, and it
    // blocks forever on the second request if the first connection is kept alive —
    // on this host the test did not fail, it *hung*. Measured on the VM: one
    // request passes in 0.3 s, two hang past 60 s, and the threads sit in `recv`
    // inside `Stream::read` with no timeout set.
    //
    // Four requests that each sleep 300 ms must finish in about one sleep, not
    // four. The bound is deliberately loose (1 s rather than 400 ms) because this
    // is not a benchmark: it only has to separate "at the same time" from "one
    // after another", and 1.2 s of serialisation is unambiguous.
    const REQUESTS: usize = 4;
    const SLEEP: Duration = Duration::from_millis(300);

    let server = ConcurrentServer::start(SLEEP);
    let dispatcher = dispatcher_for(&server.socket(), std::env::temp_dir().as_path());

    let started = Instant::now();
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|scope| {
        for index in 0..REQUESTS {
            let tx = tx.clone();
            let dispatcher = &dispatcher;
            scope.spawn(move || {
                let mut info = RequestInfo {
                    method: "POST".to_string(),
                    uri: "/".to_string(),
                    ..RequestInfo::default()
                };
                let body = format!("c{index}").into_bytes();
                info.body_len = body.len();
                let request = Request {
                    params: build_params(&info, &Routing::default()).expect("params build"),
                    body,
                    keep_conn: true,
                };
                let outcome = dispatcher
                    .dispatch(&request)
                    .map(|response| String::from_utf8_lossy(&response.stdout).to_string())
                    .map_err(|err| err.to_string());
                let _ = tx.send(outcome);
            });
        }
        drop(tx);

        // Bounded, so a regression reports rather than waits. The original version
        // of this test hung for 17 minutes on the VM instead of failing, which is
        // how a real defect stayed invisible.
        for _ in 0..REQUESTS {
            let body = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("every request was answered within 5s")
                .expect("every request was answered");
            assert!(body.contains("CONCURRENT"), "{body:?}");
        }
    });
    let done = started.elapsed();

    let serial = SLEEP * REQUESTS as u32;
    assert!(
        done < Duration::from_secs(1),
        "{REQUESTS} concurrent requests took {done:?}, which is close to the \
         {serial:?} they would take if they were serialised on one connection"
    );
}
