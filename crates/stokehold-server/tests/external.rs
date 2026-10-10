//! Integration tests for the `external` backend, which attaches to a FastCGI
//! server it does not own.
//!
//! `php-fpm` does not exist on Windows, so these tests point the backend at a
//! `php-cgi -b` process started by the test itself. That is the same code path:
//! the backend only ever sees a socket it did not create.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use stokehold_core::config::{BackendConfig, Config, ExternalConfig, ServerConfig};
use stokehold_fcgi::{build_params, Request, RequestInfo, Routing};
use stokehold_server::Dispatcher;

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

fn dispatcher_for(socket: &str, root: &Path) -> Dispatcher {
    let config = Config {
        server: ServerConfig {
            http: Some("127.0.0.1:8080".to_string()),
            fastcgi: None,
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
    let root = std::env::temp_dir().join(format!("stokehold-external-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    write_script(
        &root,
        "index.php",
        r#"<?php echo "EXT="; echo file_get_contents("php://input");"#,
    );

    let mut dispatcher = dispatcher_for(&server.socket(), &root);
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
    let root = std::env::temp_dir().join(format!("stokehold-reuse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    write_script(
        &root,
        "index.php",
        r#"<?php echo "N="; echo file_get_contents("php://input");"#,
    );

    let mut dispatcher = dispatcher_for(&server.socket(), &root);

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
    let root = std::env::temp_dir().join(format!("stokehold-reconnect-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    write_script(
        &root,
        "index.php",
        r#"<?php echo "RC="; echo file_get_contents("php://input");"#,
    );

    let mut dispatcher = dispatcher_for(&server.socket(), &root);
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
    let root = std::env::temp_dir().join(format!("stokehold-dead-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("create the document root");

    // Attaching to a socket is lazy: the dispatcher must start even when nothing
    // is listening, because php-fpm may be starting alongside us. The failure
    // then belongs to the request.
    let mut dispatcher = dispatcher_for("127.0.0.1:1", &root);
    let err = dispatcher
        .dispatch(&request_for(&root, "x"))
        .expect_err("nothing is listening on port 1");
    assert!(err.to_string().contains("fpm"), "{err}");

    let _ = std::fs::remove_dir_all(&root);
}
