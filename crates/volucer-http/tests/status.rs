//! The metrics and status endpoints, over a real socket.
//!
//! A real `TcpStream` and a real HTTP request rather than a unit call on `answer`, because the
//! things worth testing here are the ones a unit call cannot reach: that a bound address is
//! actually served, that a query string does not change the path, that the status code a readiness
//! probe reads is the configured one, and that a second `start` on the same address fails rather
//! than silently sharing a port.
//!
//! Nothing here is skipped when it cannot run. There is no external service — the listener binds
//! loopback and the client is `std::net::TcpStream` — so a skip would be a disguise for a broken
//! test.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hyper::StatusCode;
use volucer_http::status::{Readiness, RunningStatus, StatusServices};

/// One HTTP/1.1 GET, returning the whole response as text.
///
/// `Connection: close` so the read terminates on EOF rather than on a length the test would have to
/// parse — the assertions are about the status line and the body, and a `Content-Length` parser
/// would be fixture machinery nothing checks.
fn get(address: std::net::SocketAddr, path: &str) -> (String, String) {
    let mut stream = TcpStream::connect(address).expect("connect to the listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("a read timeout");
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .expect("write the request");
    stream.flush().expect("flush");

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read the response");
    let raw = String::from_utf8(raw).expect("the response is UTF-8");

    let (head, body) = raw.split_once("\r\n\r\n").expect("a header/body split");
    (head.to_string(), body.to_string())
}

/// A readiness that can be flipped from the test, so both answers are reachable.
struct Flappable {
    ready: AtomicBool,
    reasons: Vec<String>,
}

impl Readiness for Flappable {
    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    fn reasons(&self) -> Vec<String> {
        if self.ready.load(Ordering::Relaxed) {
            Vec::new()
        } else {
            self.reasons.clone()
        }
    }
}

fn metrics_services() -> StatusServices {
    let registry = volucer_metrics::Registry::new();
    registry.describe("rr_test_total", volucer_metrics::Kind::Counter, "A counter");
    registry.counter_add("rr_test_total", &[], 1);

    StatusServices {
        metrics: Some(registry),
        metrics_path: "/metrics".to_string(),
        metrics_prefix: None,
        readiness: None,
        unavailable_status: StatusCode::SERVICE_UNAVAILABLE,
    }
}

#[test]
fn the_exposition_is_served_at_the_configured_path() {
    let running = RunningStatus::start("127.0.0.1:0", metrics_services()).expect("start");
    let (head, body) = get(running.local_addr(), "/metrics");

    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert!(
        head.contains("content-type: text/plain; version=0.0.4"),
        "the version is part of the content type, and a scraper reads it:\n{head}"
    );
    assert!(body.contains("# TYPE rr_test_total counter"), "{body}");
    assert!(body.contains("rr_test_total 1"), "{body}");
    running.stop();
}

/// A query string is how a scraper adds a cache-buster, so it must not change which endpoint is
/// reached. Treating `/metrics?x=1` as a different path would answer 404 to a legitimate scrape.
#[test]
fn a_query_string_does_not_change_the_path() {
    let running = RunningStatus::start("127.0.0.1:0", metrics_services()).expect("start");
    let (head, body) = get(running.local_addr(), "/metrics?cache=1234");

    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert!(body.contains("rr_test_total 1"), "{body}");
    running.stop();
}

/// A prefix is applied at render time, so the registry's names stay RoadRunner's. The awkward part
/// is a histogram's derived series — `_bucket`, `_sum`, `_count` — whose names must be prefixed in
/// step with the family the `# TYPE` line names.
#[test]
fn a_prefix_is_applied_to_every_name_including_a_histograms_suffixes() {
    let registry = volucer_metrics::Registry::new();
    registry.describe_histogram("rr_h", "A histogram", &[0.5]);
    let _ = registry.observe("rr_h", &[], 0.25);
    registry.describe("rr_c", volucer_metrics::Kind::Counter, "A counter");
    registry.counter_add("rr_c", &[], 2);

    let running = RunningStatus::start(
        "127.0.0.1:0",
        StatusServices {
            metrics: Some(registry),
            metrics_path: "/metrics".to_string(),
            metrics_prefix: Some("myapp_".to_string()),
            readiness: None,
            unavailable_status: StatusCode::SERVICE_UNAVAILABLE,
        },
    )
    .expect("start");

    let (_, body) = get(running.local_addr(), "/metrics");
    assert!(body.contains("# HELP myapp_rr_c A counter"), "{body}");
    assert!(body.contains("# TYPE myapp_rr_c counter"), "{body}");
    assert!(body.contains("myapp_rr_c 2"), "{body}");
    assert!(body.contains("# TYPE myapp_rr_h histogram"), "{body}");
    assert!(body.contains("myapp_rr_h_bucket{le=\"0.5\"} 1"), "{body}");
    assert!(body.contains("myapp_rr_h_sum 0.25"), "{body}");
    assert!(body.contains("myapp_rr_h_count 1"), "{body}");
    assert!(
        !body.contains("\nrr_c"),
        "no unprefixed line is left behind:\n{body}"
    );
    running.stop();
}

#[test]
fn health_answers_while_the_process_is_alive_even_if_it_is_not_ready() {
    let readiness = Arc::new(Flappable {
        ready: AtomicBool::new(false),
        reasons: vec!["http pool: 0 of 4 workers ready".to_string()],
    });
    let running = RunningStatus::start(
        "127.0.0.1:0",
        StatusServices {
            metrics: None,
            metrics_path: "/metrics".to_string(),
            metrics_prefix: None,
            readiness: Some(readiness),
            unavailable_status: StatusCode::SERVICE_UNAVAILABLE,
        },
    )
    .expect("start");

    // The distinction that matters: alive is not the same question as able to serve.
    let (head, _) = get(running.local_addr(), "/health");
    assert!(
        head.starts_with("HTTP/1.1 200 OK"),
        "a server that is merely still booting must not be restarted:\n{head}"
    );
    running.stop();
}

#[test]
fn ready_answers_the_configured_status_and_says_why_it_is_not() {
    let readiness = Arc::new(Flappable {
        ready: AtomicBool::new(false),
        reasons: vec!["http pool: 0 of 4 workers ready".to_string()],
    });
    let running = RunningStatus::start(
        "127.0.0.1:0",
        StatusServices {
            metrics: None,
            metrics_path: "/metrics".to_string(),
            metrics_prefix: None,
            readiness: Some(readiness.clone()),
            unavailable_status: StatusCode::SERVICE_UNAVAILABLE,
        },
    )
    .expect("start");

    let (head, body) = get(running.local_addr(), "/ready");
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");
    assert!(
        body.contains("0 of 4 workers ready"),
        "the body is what puts the reason in the probe's log:\n{body}"
    );

    // And the same endpoint flips without the listener being restarted, which is the whole reason
    // readiness is a trait rather than a snapshot.
    readiness.ready.store(true, Ordering::Relaxed);
    let (head, body) = get(running.local_addr(), "/ready");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert!(body.contains("ready"), "{body}");
    running.stop();
}

/// A load balancer that expects 503 specifically, rather than any non-2xx, is why the code is
/// configurable — so the setting has to reach the response.
#[test]
fn the_unavailable_status_is_the_configured_one() {
    let readiness = Arc::new(Flappable {
        ready: AtomicBool::new(false),
        reasons: Vec::new(),
    });
    let running = RunningStatus::start(
        "127.0.0.1:0",
        StatusServices {
            metrics: None,
            metrics_path: "/metrics".to_string(),
            metrics_prefix: None,
            readiness: Some(readiness),
            unavailable_status: StatusCode::INTERNAL_SERVER_ERROR,
        },
    )
    .expect("start");

    let (head, _) = get(running.local_addr(), "/ready");
    assert!(head.starts_with("HTTP/1.1 500"), "{head}");
    running.stop();
}

/// Every other path is a 404, so neither endpoint becomes a second way to ask whether the server is
/// up — and so a scrape endpoint does not become a surface for anything else.
#[test]
fn anything_else_is_a_404() {
    let running = RunningStatus::start("127.0.0.1:0", metrics_services()).expect("start");

    for path in [
        "/",
        "/health",
        "/ready",
        "/metrics/",
        "/METRICS",
        "/../metrics",
    ] {
        let (head, _) = get(running.local_addr(), path);
        assert!(
            head.starts_with("HTTP/1.1 404"),
            "{path} should not be served:\n{head}"
        );
    }
    running.stop();
}

/// With metrics configured and readiness not, `/health` and `/ready` are 404 — the endpoints belong
/// to the table that turned them on, and a deployment that wanted one must not silently get the
/// other.
#[test]
fn readiness_endpoints_are_absent_when_no_readiness_is_configured() {
    let running = RunningStatus::start("127.0.0.1:0", metrics_services()).expect("start");

    for path in ["/health", "/ready"] {
        let (head, _) = get(running.local_addr(), path);
        assert!(head.starts_with("HTTP/1.1 404"), "{path}:\n{head}");
    }
    running.stop();
}

/// The metrics path is a setting, so a deployment whose reverse proxy already owns `/metrics` can
/// move it. A fixed path would make that a routing conflict rather than a naming choice.
#[test]
fn the_metrics_path_is_configurable() {
    let mut services = metrics_services();
    services.metrics_path = "/internal/counters".to_string();
    let running = RunningStatus::start("127.0.0.1:0", services).expect("start");

    let (head, body) = get(running.local_addr(), "/internal/counters");
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert!(body.contains("rr_test_total 1"), "{body}");

    let (head, _) = get(running.local_addr(), "/metrics");
    assert!(
        head.starts_with("HTTP/1.1 404"),
        "the default path is no longer served:\n{head}"
    );
    running.stop();
}

/// An address that is already taken fails the bind rather than reporting a port it does not hold,
/// and — the part worth pinning — the failure happens *in* `start`, before any thread exists.
#[test]
fn a_second_listener_on_the_same_address_is_an_error() {
    let running = RunningStatus::start("127.0.0.1:0", metrics_services()).expect("start");
    let address = running.local_addr().to_string();

    let second = RunningStatus::start(&address, metrics_services());
    assert!(
        second.is_err(),
        "two listeners on one address must not be possible"
    );

    running.stop();
}

/// `stop` releases the port, which is what makes the previous test's failure a bind rather than a
/// leak, and what makes a restart in a deployment work at all.
#[test]
fn stopping_releases_the_address() {
    let running = RunningStatus::start("127.0.0.1:0", metrics_services()).expect("start");
    let address = running.local_addr().to_string();
    running.stop();

    let again = RunningStatus::start(&address, metrics_services());
    assert!(
        again.is_ok(),
        "the port should be free: {}",
        again.err().map_or(String::new(), |err| err.to_string())
    );
    if let Ok(listener) = again {
        listener.stop();
    }
}

/// With no metrics and no readiness, the listener still binds and answers 404 to everything. That
/// is what a `[status]` table with neither configured would produce, and it must not be a panic on
/// the first request.
#[test]
fn an_empty_configuration_answers_404_rather_than_failing() {
    let running = RunningStatus::start(
        "127.0.0.1:0",
        StatusServices {
            metrics: None,
            metrics_path: "/metrics".to_string(),
            metrics_prefix: None,
            readiness: None,
            unavailable_status: StatusCode::SERVICE_UNAVAILABLE,
        },
    )
    .expect("start");

    let (head, _) = get(running.local_addr(), "/metrics");
    assert!(head.starts_with("HTTP/1.1 404"), "{head}");
    running.stop();
}
