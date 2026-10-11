//! End-to-end test of the full chain, with no nginx present:
//!
//! ```text
//! FastCGI client (standing in for nginx)
//!     â”€â”€FastCGIâ”€â”€â–º stokehold front-end â”€â”€â–º dispatcher â”€â”€FastCGIâ”€â”€â–º php-cgi
//! ```
//!
//! This is the Phase 6 milestone from `docs/plan.md`. The client role is played
//! by the same codec stokehold uses as a client, which is legitimate here: the
//! point is to prove the front-end parses a real FastCGI request, resolves the
//! script through the CGI environment, gets a response from a real PHP process,
//! and re-frames it back without corrupting it.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

use stokehold_core::config::{BackendConfig, Config, ServerConfig, SpawnCgiConfig};
use stokehold_fcgi::{
    message::BeginRequest, nvpair, protocol::RecordType, record, Endpoint, Request, Response,
};
use stokehold_server::{backend, Dispatcher};

mod common;
use common::{find_php_cgi, free_port_run, serial, write_script};

/// Run one request through the in-process front-end on a fresh connection.
///
/// The front-end work is done with the library functions the real listener will
/// call, so the protocol handling under test is the real one.
fn front_end_round_trip(
    dispatcher: &mut Dispatcher,
    request_id: u16,
    params: &[(Vec<u8>, Vec<u8>)],
    body: &[u8],
    keep_conn: bool,
) -> stokehold_server::Result<Response> {
    // Serialise the request exactly as nginx would.
    let mut wire = Vec::new();
    let begin = BeginRequest::responder(keep_conn);
    let header = record::Header::new(RecordType::BeginRequest, request_id, 8);
    header
        .write(&mut wire)
        .map_err(stokehold_server::ServerError::Fcgi)?;
    wire.write_all(&begin.encode())?;

    let payload = nvpair::encode_pairs(
        params
            .iter()
            .map(|(name, value)| (name.as_slice(), value.as_slice())),
    );
    record::write_records(&mut wire, RecordType::Params, request_id, &payload, true)
        .map_err(stokehold_server::ServerError::Fcgi)?;
    record::write_records(&mut wire, RecordType::Stdin, request_id, body, true)
        .map_err(stokehold_server::ServerError::Fcgi)?;

    // The front-end reads it.
    let mut cursor = wire.as_slice();
    let incoming = stokehold_server::frontend::read_request(&mut cursor)
        .expect("the front-end parses the request")
        .expect("a request was present");

    // Dispatch to the backend.
    let response = dispatcher
        .dispatch(&incoming.clone().into_backend_request())
        .expect("the backend served the request");

    // Re-frame it as the front-end would.
    let mut out = Vec::new();
    stokehold_server::frontend::write_response(
        &mut out,
        request_id,
        &response.stdout,
        &response.stderr,
        incoming.keep_conn,
    )
    .expect("the front-end writes the response");

    // And read it back the way nginx would.
    let mut reader = out.as_slice();
    let mut decoded = Response::default();
    while let Some(record) =
        record::read_record(&mut reader).map_err(stokehold_server::ServerError::Fcgi)?
    {
        match record.record_type() {
            RecordType::Stdout => decoded.stdout.extend_from_slice(&record.content),
            RecordType::Stderr => decoded.stderr.extend_from_slice(&record.content),
            RecordType::EndRequest => {
                decoded.end = Some(
                    stokehold_fcgi::EndRequest::decode(&record.content)
                        .map_err(stokehold_server::ServerError::Fcgi)?,
                );
                break;
            }
            _ => continue,
        }
    }
    Ok(decoded)
}

/// Start a `php-cgi` pool backend plus a dispatcher in front of it.
fn chain(root: &Path, binary: &Path, base_port: u16) -> Dispatcher {
    let config = Config {
        server: ServerConfig {
            http: Some("127.0.0.1:0".to_string()),
            fastcgi: Some("127.0.0.1:0".to_string()),
            ..Default::default()
        },
        backends: [(
            "cgi".to_string(),
            BackendConfig::SpawnCgi(SpawnCgiConfig {
                binary: binary.to_path_buf(),
                workers: 1,
                bind_host: "127.0.0.1".to_string(),
                base_port,
                max_requests: None,
            }),
        )]
        .into_iter()
        .collect(),
        default_backend: Some("cgi".to_string()),
        routes: Vec::new(),
    };
    config.validate().expect("the config is valid");

    let routing = stokehold_fcgi::Routing {
        document_root: root.to_path_buf(),
        front_controller: Some("index.php".to_string()),
        ..stokehold_fcgi::Routing::default()
    };
    Dispatcher::from_config(&config, routing).expect("the dispatcher starts")
}

/// Params for a POST, the way nginx would compute them for an app root.
///
/// The method and body length matter: PHP only reads the body when
/// `CONTENT_LENGTH` is present, so a body sent with GET params would be ignored
/// by PHP (correctly) and the test would be proving nothing.
fn params_for(root: &Path, uri: &str, body_len: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    let routing = stokehold_fcgi::Routing {
        document_root: root.to_path_buf(),
        front_controller: Some("index.php".to_string()),
        ..stokehold_fcgi::Routing::default()
    };
    let info = stokehold_fcgi::RequestInfo {
        method: "POST".to_string(),
        uri: uri.to_string(),
        body_len,
        ..stokehold_fcgi::RequestInfo::default()
    };
    stokehold_fcgi::build_params(&info, &routing).expect("params build")
}

#[test]
fn a_fastcgi_request_travels_the_whole_chain_and_comes_back_intact() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(1) else {
        eprintln!("skipping: no free port");
        return;
    };

    let root = std::env::temp_dir().join(format!("stokehold-chain-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    write_script(
        &root,
        "index.php",
        r#"<?php
header('X-Chain: yes');
echo "URI=", $_SERVER['REQUEST_URI'] ?? '-', "\n";
echo "PATH=", $_SERVER['PATH_INFO'] ?? '-', "\n";
echo "BODY=", file_get_contents('php://input');
"#,
    );

    let mut dispatcher = chain(&root, &binary, base);
    let body = b"payload";
    let params = params_for(&root, "/deep/route?x=1", body.len());

    let response = front_end_round_trip(&mut dispatcher, 1, &params, body, true)
        .expect("the round trip completed");

    let stdout = String::from_utf8_lossy(&response.stdout);
    assert!(stdout.contains("URI=/deep/route?x=1"), "{stdout}");
    assert!(stdout.contains("PATH=/deep/route"), "{stdout}");
    assert!(stdout.contains("BODY=payload"), "{stdout}");
    assert!(stdout.contains("X-Chain: yes"), "{stdout}");

    assert!(
        response
            .end
            .map(|end| end.protocol_status.is_complete())
            .unwrap_or(false),
        "the front-end must end the request with REQUEST_COMPLETE"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_front_end_serves_several_requests_in_a_row() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(1) else {
        eprintln!("skipping: no free port");
        return;
    };

    let root = std::env::temp_dir().join(format!("stokehold-chain-many-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    write_script(
        &root,
        "index.php",
        r#"<?php echo "I="; echo file_get_contents('php://input');"#,
    );

    let mut dispatcher = chain(&root, &binary, base);

    for index in 0..4 {
        let body = format!("req{index}");
        let params = params_for(&root, "/", body.len());
        let response = front_end_round_trip(&mut dispatcher, 1, &params, body.as_bytes(), true)
            .unwrap_or_else(|err| panic!("request {index} failed: {err}"));
        let stdout = String::from_utf8_lossy(&response.stdout);
        assert!(stdout.contains(&format!("I=req{index}")), "{stdout}");
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_backend_failure_becomes_a_502_rather_than_a_broken_connection() {
    let _serial = serial();

    // Point at a socket with nothing behind it, so the dispatch fails.
    let config = Config {
        server: ServerConfig {
            fastcgi: Some("127.0.0.1:0".to_string()),
            ..Default::default()
        },
        backends: [(
            "dead".to_string(),
            BackendConfig::External(stokehold_core::config::ExternalConfig {
                socket: "127.0.0.1:1".to_string(),
            }),
        )]
        .into_iter()
        .collect(),
        default_backend: Some("dead".to_string()),
        routes: Vec::new(),
    };
    config.validate().expect("valid config");
    let dispatcher =
        Dispatcher::from_config(&config, stokehold_fcgi::Routing::default()).expect("dispatcher");

    let wire = {
        let mut wire = Vec::new();
        let begin = BeginRequest::responder(true);
        let header = record::Header::new(RecordType::BeginRequest, 5, 8);
        header.write(&mut wire).unwrap();
        wire.write_all(&begin.encode()).unwrap();
        record::write_records(&mut wire, RecordType::Params, 5, b"", true).unwrap();
        record::write_records(&mut wire, RecordType::Stdin, 5, b"", true).unwrap();
        wire
    };

    let mut cursor = wire.as_slice();
    let incoming = stokehold_server::frontend::read_request(&mut cursor)
        .unwrap()
        .unwrap();

    // The dispatch fails, so the front-end must answer with an error response.
    let err = dispatcher
        .dispatch(&incoming.into_backend_request())
        .expect_err("nothing is listening");

    let kind = match &err {
        stokehold_server::ServerError::Worker { .. } => backend::is_transport_error(match &err {
            stokehold_server::ServerError::Worker { source, .. } => source,
            _ => unreachable!(),
        }),
        _ => false,
    };
    let kind = if kind {
        stokehold_server::frontend::ServerErrorKind::Backend
    } else {
        stokehold_server::frontend::ServerErrorKind::Overloaded
    };

    let mut out = Vec::new();
    stokehold_server::frontend::write_error(&mut out, 5, &err.to_string(), kind)
        .expect("the error response is written");

    // nginx sees a 502, and the connection stays framed.
    let mut reader = out.as_slice();
    let mut body = Vec::new();
    while let Some(record) = record::read_record(&mut reader).unwrap() {
        match record.record_type() {
            RecordType::Stdout => body.extend_from_slice(&record.content),
            RecordType::EndRequest => break,
            _ => continue,
        }
    }
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("502 Bad Gateway"), "{body}");
}

/// Proves the front-end and the real client agree on framing, over a real
/// socket rather than an in-memory buffer.
#[test]
fn the_frame_codec_works_over_a_real_socket() {
    let _serial = serial();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a listener");
    let address = listener.local_addr().expect("address");

    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let request = stokehold_server::frontend::read_request(&mut stream)
            .expect("read the request")
            .expect("a request arrived");
        let method = request.param_str("REQUEST_METHOD").unwrap_or_default();
        let body = String::from_utf8_lossy(&request.body).into_owned();
        let reply = format!("Status: 200 OK\r\n\r\necho={method}:{body}");
        stokehold_server::frontend::write_response(&mut stream, 1, reply.as_bytes(), b"log", true)
            .expect("write the response");
        (method, body)
    });

    let mut client = stokehold_fcgi::Client::connect(
        &Endpoint::Tcp(address.to_string()),
        Some(Duration::from_secs(5)),
    )
    .expect("connect");

    let response = client
        .request(&Request {
            params: vec![
                (b"REQUEST_METHOD".to_vec(), b"POST".to_vec()),
                (b"CONTENT_LENGTH".to_vec(), b"4".to_vec()),
            ],
            body: b"data".to_vec(),
            keep_conn: true,
        })
        .expect("the front-end answered");

    let (method, body) = server.join().expect("the server thread finished");
    assert_eq!(method, "POST");
    assert_eq!(body, "data");

    let stdout = String::from_utf8_lossy(&response.stdout);
    assert!(stdout.contains("echo=POST:data"), "{stdout}");
    assert_eq!(response.stderr, b"log");
}

/// A raw HTTP-ish request through a TcpStream, to show the listener shape.
#[test]
fn a_socket_front_end_can_be_built_from_the_parser() {
    let _serial = serial();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");

    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        serve_one(stream)
    });

    let mut stream = TcpStream::connect(address).expect("connect");
    let params = nvpair::encode_pairs([
        (&b"REQUEST_METHOD"[..], &b"GET"[..]),
        (&b"REQUEST_URI"[..], &b"/hello"[..]),
    ]);
    let mut wire = Vec::new();
    let header = record::Header::new(RecordType::BeginRequest, 1, 8);
    header.write(&mut wire).unwrap();
    wire.write_all(&BeginRequest::responder(false).encode())
        .unwrap();
    record::write_records(&mut wire, RecordType::Params, 1, &params, true).unwrap();
    record::write_records(&mut wire, RecordType::Stdin, 1, b"", true).unwrap();

    stream.write_all(&wire).expect("write the request");
    stream.flush().expect("flush");

    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).expect("read the reply");

    let mut cursor = reply.as_slice();
    let mut body = Vec::new();
    while let Some(record) = record::read_record(&mut cursor).expect("parse the reply") {
        if record.record_type() == RecordType::Stdout {
            body.extend_from_slice(&record.content);
        }
    }

    assert_eq!(handle.join().expect("join"), "GET /hello");
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("GET /hello"), "{body}");
}

/// Handle exactly one request on a connection, then close.
fn serve_one(mut stream: TcpStream) -> String {
    let request = stokehold_server::frontend::read_request(&mut stream)
        .expect("read the request")
        .expect("a request arrived");
    let summary = format!(
        "{} {}",
        request.param_str("REQUEST_METHOD").unwrap_or_default(),
        request.param_str("REQUEST_URI").unwrap_or_default()
    );
    let reply = format!("Status: 200 OK\r\n\r\n{summary}");
    stokehold_server::frontend::write_response(&mut stream, 1, reply.as_bytes(), b"", false)
        .expect("write the response");
    let _ = stream.shutdown(std::net::Shutdown::Write);
    summary
}
