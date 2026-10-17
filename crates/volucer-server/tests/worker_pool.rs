//! Integration tests for the worker pool against real PHP worker processes.
//!
//! These are the tests that prove the pool actually supervises: it boots
//! workers, completes the handshake, serves requests through every worker in
//! rotation, replaces a worker that died or asked to be recycled, and leaves
//! nothing running when it is dropped.
//!
//! They drive the SDK's `example/worker.php` rather than a fixture, so the PHP
//! end is the shipped SDK and a change to either side fails here. The SDK lives
//! in the `sailantis/volucer-php-sdk` repository; `VOLUCER_SDK_DIR` points
//! at a checkout that is not beside this one. The Rust codec is
//! covered on its own in `crates/volucer-worker`; what is under test *here* is
//! the supervision around it.
//!
//! Skipped, with a printed reason, when no `php` binary is on `PATH` or when the
//! SDK checkout cannot be found. libtest hides a passing test's stderr, so a
//! skip only *shows* with `--nocapture`; set `VOLUCER_E2E=1` where a skip
//! should be a failure.

use std::path::{Path, PathBuf};
use std::time::Duration;

use volucer_core::config::{
    BackendConfig, Config, KvStorageConfig, LockConfig, MemoryKvConfig, MemoryLockConfig,
    WorkerConfig,
};
use volucer_fcgi::{build_params, Request, RequestInfo, Routing};
use volucer_server::{Dispatcher, WorkerPool};

mod common;
use common::serial;

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

/// The SDK the example worker lives in, which is now a repository of its own.
///
/// `VOLUCER_SDK_DIR` overrides it, which is how a CI checkout at any path —
/// or a sibling clone under a different name — is pointed at. Otherwise the
/// layout this project is developed in is assumed: `sailantis/volucer` and
/// `sailantis/volucer-php-sdk` next to each other.
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

/// The SDK's example worker, which is what the pool is pointed at.
fn example_worker() -> PathBuf {
    sdk_dir().join("example").join("worker.php")
}

/// Why this test cannot run here, if it cannot.
fn skip_reason() -> Option<String> {
    if find_php().is_none() {
        return Some("no php on PATH".to_string());
    }

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

/// Report that this test cannot run here.
///
/// libtest hides the stderr of a passing test, so a skip is invisible in an
/// ordinary run. Set `VOLUCER_E2E=1` on a machine that is *supposed* to have
/// PHP and the SDK, and a skip becomes a failure instead.
fn report_skip(reason: String) {
    if skip_is_a_failure() {
        panic!("VOLUCER_E2E is set, so a skipped test is a failure: {reason}");
    }
    eprintln!("skipping: {reason}");
}

/// Whether `VOLUCER_E2E` asks for skips to be failures. `0` or empty means no.
fn skip_is_a_failure() -> bool {
    match std::env::var("VOLUCER_E2E") {
        Ok(value) => !value.is_empty() && value != "0",
        Err(_) => false,
    }
}

/// Skip the test, loudly, when PHP or the SDK checkout is missing.
macro_rules! require_sdk {
    () => {
        if let Some(reason) = skip_reason() {
            report_skip(reason);
            return;
        }
    };
}

/// A pool configuration for the example worker.
///
/// The command is quoted so that the test exercises the real command splitter
/// with a path that may contain spaces — a temporary directory on Windows
/// reliably does.
fn config(workers: u32) -> WorkerConfig {
    WorkerConfig {
        command: format!("php \"{}\"", example_worker().display()),
        workers,
        memory_limit_mb: None,
        max_requests: None,
        working_dir: None,
        env: Default::default(),
        checksum: Default::default(),
        services: Vec::new(),
        transport: Default::default(),
    }
}

/// Start a pool, or report why the test cannot run.
fn start(workers: u32) -> Option<WorkerPool> {
    if let Some(reason) = skip_reason() {
        report_skip(reason);
        return None;
    }
    match WorkerPool::start("test", &config(workers)) {
        Ok(pool) => Some(pool),
        Err(err) => {
            eprintln!("skipping: the worker pool could not start: {err}");
            None
        }
    }
}

/// A GET request routed through the real CGI environment.
fn get(uri: &str) -> Request {
    request("GET", uri, &[])
}

/// A pool configuration that reaches the worker over a loopback socket.
///
/// The worker connects back to `VOLUCER_WORKER_SOCKET`, which the server sets itself; see
/// `volucer_core::config::WorkerTransport` for why the option exists.
///
/// This found a real ordering bug the first time it ran. `boot` set the address before
/// applying the configuration's own `env` map, so a configuration carrying any `env` at all
/// replaced the address — and the workers then sat waiting for a connection that was never
/// coming, for ever, because nothing about "connected to the wrong place" looks like a
/// failure. The address is now set *after* `env` so it always wins.
fn socket_config(workers: u32) -> WorkerConfig {
    WorkerConfig {
        transport: volucer_core::config::WorkerTransport::Socket,
        ..config(workers)
    }
}

/// A pool configuration that reaches the worker over its own stdio, **explicitly**.
///
/// `config()` takes whatever transport the platform defaults to, which is the honest thing for
/// most tests here — it is what a deployment gets. It is the wrong thing for the comparison
/// below, and the day the default became platform-dependent this helper is what stopped that
/// test from comparing the socket transport with itself on Windows.
fn pipe_config(workers: u32) -> WorkerConfig {
    WorkerConfig {
        transport: volucer_core::config::WorkerTransport::Pipe,
        ..config(workers)
    }
}

/// A socket-transport pool serves exactly what a pipe-transport pool serves.
///
/// This is what makes `transport = "socket"` a supported configuration rather than a
/// documented possibility. The two pools run the same worker file, the same request and the
/// same assertion; only the transport differs.
///
/// It is worth having even though the socket path is one branch in `boot`, because the
/// failure modes it guards are silent ones: a worker that connects to the wrong address, or
/// a channel built from a stream that does not carry the protocol, would still *start*.
#[test]
fn a_socket_transport_pool_serves_the_same_bytes_as_a_pipe_one() {
    let _serial = serial();
    if let Some(reason) = skip_reason() {
        report_skip(reason);
        return;
    }

    for (label, settings) in [("pipe", pipe_config(2)), ("socket", socket_config(2))] {
        let mut pool = WorkerPool::start(label, &settings)
            .unwrap_or_else(|err| panic!("the {label} pool starts: {err}"));

        assert_eq!(pool.len(), 2, "the {label} pool keeps its size");

        for index in 0..4 {
            let response = pool
                .request(&get("/health"))
                .unwrap_or_else(|err| panic!("the {label} pool answered: {err}"));
            let body = String::from_utf8_lossy(&response.stdout);
            // The example worker answers `/health` with a JSON status, so the assertion is
            // on the application's own output rather than on a status code alone.
            assert!(
                body.contains("ok"),
                "the {label} pool returned {body:?} for request {index}"
            );
        }

        pool.shutdown();
        assert!(pool.is_empty(), "the {label} pool stopped");
    }
}

/// A request routed through the real CGI environment, with a body.
fn request(method: &str, uri: &str, body: &[u8]) -> Request {
    let routing = Routing {
        document_root: Path::new(".").to_path_buf(),
        front_controller: Some("index.php".to_string()),
        ..Routing::default()
    };
    let info = RequestInfo {
        method: method.to_string(),
        uri: uri.to_string(),
        body_len: body.len(),
        ..RequestInfo::default()
    };
    Request {
        params: build_params(&info, &routing).expect("params build"),
        body: body.to_vec(),
        keep_conn: true,
    }
}

/// The body of a rendered response, with the `Status:` block stripped.
fn body_of(response: &volucer_fcgi::Response) -> String {
    let text = String::from_utf8_lossy(&response.stdout);
    match text.split_once("\r\n\r\n") {
        Some((_, body)) => body.to_string(),
        None => text.to_string(),
    }
}

fn kill(pid: u32) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }
}

fn is_running(pid: u32) -> bool {
    #[cfg(windows)]
    {
        match std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
        {
            Ok(output) => String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()),
            // If we cannot ask, assume it is running rather than assert wrongly.
            Err(_) => true,
        }
    }
    #[cfg(unix)]
    {
        // `/proc` is Linux, not Unix: macOS has no procfs, so the first arm is `false` there and
        // the whole predicate used to be `false` — which is how this test failed on the macOS
        // runner and passed on Linux and Windows. `kill -0` asks the question portably: it sends
        // no signal and only reports whether the process exists and we may signal it.
        // `crates/volucer-server/tests/pool.rs` already had this fallback; this copy had not.
        PathBuf::from(format!("/proc/{pid}")).exists()
            || std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .map(|status| status.success())
                .unwrap_or(true)
    }
}

#[test]
fn starts_a_pool_of_real_workers_and_serves_through_all_of_them() {
    let _serial = serial();
    let Some(pool) = start(3) else {
        return;
    };

    assert_eq!(pool.len(), 3);
    let pids = pool.pids();
    assert!(pids.iter().all(|pid| *pid != 0), "{pids:?}");

    // The handshake is part of starting, so the PHP version is known before the
    // first request.
    for version in pool.php_versions() {
        assert!(
            version.starts_with("8."),
            "unexpected PHP version {version}"
        );
    }

    // Three requests, so round-robin reaches every worker rather than only the
    // first.
    for _ in 0..3 {
        let response = pool.request(&get("/health")).expect("a worker answered");
        assert_eq!(response.parse_status_headers().0, 200);
        let body = body_of(&response);
        assert!(body.contains("\"status\":\"ok\""), "{body}");
        assert!(response.check_protocol().is_ok());
    }

    // Every worker should have served exactly one request, which is the whole
    // point of round-robin: an uneven split is also an uneven memory picture.
    let pids_after = pool.pids();
    assert_eq!(pids_after, pids, "no worker should have been recycled");
}

#[test]
fn a_request_body_reaches_the_application() {
    let _serial = serial();
    let Some(pool) = start(1) else {
        return;
    };

    // A marker rather than a length: a body that arrives but is mangled, or one
    // that arrives in the wrong frame, fails here rather than passing a check
    // that only counted bytes.
    let marker = "the-body-arrived-intact";
    let response = pool
        .request(&request("POST", "/echo", marker.as_bytes()))
        .expect("a worker answered");

    assert_eq!(response.parse_status_headers().0, 200);
    let body = body_of(&response);
    assert!(body.contains(&format!("payload: {marker}")), "{body}");
    assert!(
        body.contains(&format!("body: {} bytes", marker.len())),
        "{body}"
    );
}

#[test]
fn a_large_body_is_chunked_and_arrives_whole() {
    let _serial = serial();
    let Some(pool) = start(1) else {
        return;
    };

    // 800 000 bytes is more than one protocol chunk (64 KiB), so this exercises
    // the LAST flag and the declared `body_len` together.
    let response = pool.request(&get("/big")).expect("a worker answered");
    let body = body_of(&response);
    assert_eq!(body.len(), 800_000, "the whole body should have arrived");
    // `/big` is `str_repeat('abcdefgh', 100000)`, so a body that arrived
    // *complete* but reordered or truncated mid-chunk would fail here.
    assert!(
        body.starts_with("abcdefgh"),
        "{}",
        &body[..16.min(body.len())]
    );
    assert!(body.ends_with("abcdefgh"));
    assert_eq!(body.matches("abcdefgh").count(), 100_000);
    // A chunked body is the case where a length/flag disagreement shows up.
    assert!(response.check_protocol().is_ok());
}

#[test]
fn an_uncaught_application_error_is_a_500_and_the_worker_survives() {
    let _serial = serial();
    let Some(pool) = start(1) else {
        return;
    };

    let before = pool.pids();

    let response = pool.request(&get("/boom")).expect("the worker answered");
    assert_eq!(
        response.parse_status_headers().0,
        500,
        "a throwing handler must become a 500"
    );
    // The application's own status is not the HTTP status: the route threw, so
    // `app_status` is non-zero while the response is a perfectly valid 500 page.
    assert_eq!(response.app_status(), Some(1));

    // One bad request does not replace the worker.
    assert_eq!(
        pool.pids(),
        before,
        "the worker should not have been recycled"
    );

    // And the next request still works.
    let response = pool.request(&get("/health")).expect("the worker answered");
    assert_eq!(response.parse_status_headers().0, 200);
}

#[test]
fn worker_telemetry_reaches_the_backend_stderr() {
    let _serial = serial();
    let Some(pool) = start(1) else {
        return;
    };

    // `/` emits a metric and a log line before answering, through the SDK's
    // out-of-band channel.
    let response = pool.request(&get("/")).expect("a worker answered");
    let stderr = String::from_utf8_lossy(&response.stderr);

    assert_eq!(response.parse_status_headers().0, 200);
    assert!(stderr.contains("serving GET /"), "{stderr}");
    assert!(stderr.contains("[info]"), "{stderr}");
    // The telemetry travels beside the response, never inside it.
    assert!(!body_of(&response).contains("serving GET /"));
}

#[test]
fn a_killed_worker_is_replaced_in_place() {
    let _serial = serial();
    let Some(pool) = start(1) else {
        return;
    };

    let before = pool.pids()[0];
    // Kill the worker behind the pool's back, the way a crash or an OOM kill
    // would. Its pipes stay open at our end but are at end-of-file.
    kill(before);
    std::thread::sleep(Duration::from_millis(300));

    let response = pool
        .request(&get("/health"))
        .expect("the pool recovered from a dead worker");
    assert_eq!(response.parse_status_headers().0, 200);

    let after = pool.pids();
    assert_eq!(after.len(), 1, "the pool kept its size");
    assert_ne!(after[0], before, "the dead worker was replaced");
}

#[test]
fn max_requests_zero_means_never_recycle_not_recycle_every_request() {
    let _serial = serial();
    require_sdk!();

    // The value a php-fpm deployment is most likely to copy: `pm.max_requests = 0`
    // there means the pool never recycles. A bare `served >= max` comparison would
    // destroy the worker after every request instead, which is one application boot
    // per request — the exact thing this backend exists to avoid. So this is not a
    // boundary case, it is the case.
    let mut config = config(1);
    config.max_requests = Some(0);
    let pool = WorkerPool::start("test", &config).expect("pool starts");
    let original = pool.pids()[0];

    for _ in 0..5 {
        pool.request(&get("/health")).expect("a worker answered");
    }

    assert_eq!(
        pool.pids()[0],
        original,
        "max_requests = 0 must mean never, not recycle after every request"
    );
    assert!(
        is_running(original),
        "the worker should still be the original"
    );
}

#[test]
fn recycling_replaces_the_worker_after_max_requests() {
    let _serial = serial();
    require_sdk!();

    let mut config = config(1);
    config.max_requests = Some(2);
    let pool = WorkerPool::start("test", &config).expect("pool starts");
    let original = pool.pids()[0];

    for _ in 0..2 {
        pool.request(&get("/health")).expect("a worker answered");
    }

    assert_ne!(
        pool.pids()[0],
        original,
        "the worker should have been recycled after max_requests"
    );
    assert!(!is_running(original), "the recycled worker should be gone");

    // And the replacement serves.
    assert_eq!(
        pool.request(&get("/health"))
            .expect("the replacement answered")
            .parse_status_headers()
            .0,
        200
    );
}

#[test]
fn a_worker_that_asks_to_be_recycled_is_replaced() {
    let _serial = serial();
    let Some(pool) = start(1) else {
        return;
    };

    let original = pool.pids()[0];

    // `/recycle` calls `recycle()` on the SDK, which shows up as
    // `keep_alive = false` in END. The request itself succeeds — a worker asking
    // to be replaced is not an error, it is a request for cleanup.
    let response = pool.request(&get("/recycle")).expect("a worker answered");
    assert_eq!(response.parse_status_headers().0, 200);
    assert!(body_of(&response).contains("\"recycled\":true"));

    let replacement = pool.pids()[0];
    assert_ne!(
        replacement, original,
        "a worker that asked to be recycled should have been replaced"
    );
    assert!(!is_running(original), "the recycled worker should be gone");

    // A worker cannot replace itself, so the next request must work.
    assert_eq!(
        pool.request(&get("/health"))
            .expect("the replacement answered")
            .parse_status_headers()
            .0,
        200
    );
}

#[test]
fn the_pool_reports_the_peak_memory_its_workers_measured() {
    let _serial = serial();
    let Some(pool) = start(1) else {
        return;
    };

    assert_eq!(pool.peak_memory_bytes(), vec![0], "nothing served yet");

    pool.request(&get("/big")).expect("a worker answered");

    let peak = pool.peak_memory_bytes()[0];
    // The `/big` route builds an 800 000 byte body, so anything much below that
    // means the figure is not being read from END at all.
    assert!(
        peak > 800_000,
        "peak memory {peak} looks wrong after serving 800 KB"
    );
}

#[test]
fn a_memory_limit_recycles_a_worker_that_exceeds_it() {
    let _serial = serial();
    require_sdk!();

    // One megabyte is below what any PHP process uses, so every request
    // recycles. That makes the threshold's effect observable without needing to
    // know the exact figure the runtime reports.
    let mut config = config(1);
    config.memory_limit_mb = Some(1);
    let pool = WorkerPool::start("test", &config).expect("pool starts");
    let original = pool.pids()[0];

    let response = pool.request(&get("/health")).expect("a worker answered");
    assert_eq!(response.parse_status_headers().0, 200);

    assert_ne!(
        pool.pids()[0],
        original,
        "a worker above the memory limit should have been replaced"
    );
}

#[test]
fn dropping_the_pool_leaves_no_workers_behind() {
    let _serial = serial();
    let Some(mut pool) = start(2) else {
        return;
    };

    let pids = pool.pids();
    assert_eq!(pids.len(), 2);

    // `shutdown` closes each worker's stdin, so the worker exits on its own
    // rather than being killed.
    pool.shutdown();
    assert!(pool.is_empty());

    for pid in pids {
        assert!(!is_running(pid), "pid {pid} survived the shutdown");
    }
}

#[test]
fn a_pool_dropped_without_shutdown_still_stops_its_workers() {
    let _serial = serial();
    require_sdk!();

    let pids = {
        let pool = WorkerPool::start("test", &config(2)).expect("pool starts");
        assert_eq!(pool.pids().len(), 2);
        pool.pids()
        // `pool` is dropped here.
    };

    std::thread::sleep(Duration::from_millis(500));

    for pid in pids {
        assert!(
            !is_running(pid),
            "pid {pid} survived the pool being dropped"
        );
    }
}

#[test]
fn a_request_landing_on_a_killed_worker_is_answered_by_the_replacement() {
    let _serial = serial();
    let Some(pool) = start(1) else {
        return;
    };

    let before = pool.pids()[0];
    kill(before);
    // No sleep: the request is meant to arrive while the pool's view of the worker
    // is still "alive but about to be found dead", which is exactly the window a
    // real recycle opens. If the kill has not been reaped yet the pool finds the
    // pipes closed and retries; if it already has, the liveness check on the leased
    // slot replaces the worker first. Both are the recovery under test.
    let response = pool
        .request(&get("/health"))
        .expect("the replacement answered the request");
    assert_eq!(response.parse_status_headers().0, 200);
    assert_ne!(pool.pids()[0], before, "the dead worker was replaced");
}

#[test]
fn a_post_that_lands_on_a_killed_worker_is_reported_not_replayed() {
    let _serial = serial();
    let Some(pool) = start(1) else {
        return;
    };

    // The same window as above, but with a method HTTP does not define as
    // idempotent. A retry would be a second write, so the pool must surface the
    // failure rather than silently run the application twice. A `POST` that
    // *succeeds* (the worker was already replaced before the request was picked) is
    // fine — the assertion is that it never reports success by replaying.
    let before = pool.pids()[0];
    kill(before);

    match pool.request(&request("POST", "/echo", b"x")) {
        Ok(response) => {
            // Only legitimate if the replacement served it in one attempt.
            assert_eq!(response.parse_status_headers().0, 200);
            assert_ne!(pool.pids()[0], before);
        }
        Err(err) => {
            assert!(
                !err.is_timeout(),
                "a killed worker should not look like a timeout: {err}"
            );
        }
    }
}

#[test]
fn concurrent_requests_overlap_rather_than_queueing() {
    let _serial = serial();
    require_sdk!();

    // A worker with a route that takes measurable time. Written here rather than
    // added to the SDK's example, because it exists only to make overlap
    // observable.
    let dir = std::env::temp_dir().join(format!("volucer-slow-worker-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("slow-worker.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;
// `connect()`, not `create()`: this fixture is started through the same `WorkerConfig` as
// everything else, and on the platform where the socket is the default, `create()` would read
// the end-of-file stdin that the socket transport leaves behind and die during the handshake.
$worker = Worker::connect();
$worker->run(static function (Request $request, Worker $worker): Response|null {{
    $path = (string) parse_url($request->uri, PHP_URL_PATH);
    if ($path === '/slow') {{
        usleep(500000);
    }}
    return Response::json(['pid' => getmypid()]);
}});
"#
        ),
    )
    .expect("write the worker");

    let workers = 4;
    let config = WorkerConfig {
        command: format!("php \"{}\"", script.display()),
        workers,
        memory_limit_mb: None,
        max_requests: None,
        working_dir: None,
        env: Default::default(),
        checksum: Default::default(),
        services: Vec::new(),
        transport: Default::default(),
    };
    let pool = WorkerPool::start("test", &config).expect("pool starts");

    // One request per worker, all at once. Each sleeps 500 ms.
    let started = std::time::Instant::now();
    let answers: Vec<Option<u16>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    pool.request(&get("/slow"))
                        .ok()
                        .map(|response| response.parse_status_headers().0)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("thread"))
            .collect()
    });
    let elapsed = started.elapsed();

    for answer in &answers {
        assert_eq!(*answer, Some(200), "every request should have been served");
    }

    // Served in parallel this takes about one sleep; served one-at-a-time it
    // takes `workers` sleeps. The threshold sits between the two, well clear of
    // either, so the test is not sensitive to machine speed.
    let serial_minimum = Duration::from_millis(500) * workers;
    let threshold = Duration::from_millis(1500);
    assert!(
        elapsed < threshold,
        "{workers} concurrent requests took {elapsed:?}, which is close to the {serial_minimum:?} a \
         serialised pool would need — the requests are queueing rather than overlapping"
    );

    drop(pool);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_pool_configured_for_an_optional_checksum_negotiates_it_and_serves() {
    let _serial = serial();
    require_sdk!();

    // The end-to-end version of the option, through the pool rather than the channel: the pool
    // offers it in the handshake, the real SDK accepts, and requests then cross the pipe with no
    // checksum in either direction. What this catches that the channel test cannot is the *wiring*
    // — a `WorkerConfig` that never reached `Spec`, or a `Spec` that never reached `boot`.
    let mut config = config(1);
    config.checksum = volucer_core::config::ChecksumMode::Optional;
    let pool = WorkerPool::start("test", &config).expect("pool starts");

    // The answer is what is observable: the pool does not keep the channel (the exchange thread
    // owns it), but the `HELLO_ACK` recorded at boot says what each worker agreed to, and that is
    // the thing an operator wants to see.
    assert_eq!(
        pool.no_checksum_agreed(),
        vec![true],
        "the one worker should have agreed to skip the checksum"
    );

    // And a request still works, which is the part that would break if either end wrote
    // `NO_CHECKSUM` without the other agreeing.
    let response = pool.request(&get("/health")).expect("a worker answered");
    assert_eq!(response.parse_status_headers().0, 200);
    assert!(body_of(&response).contains("\"status\":\"ok\""));

    drop(pool);
}

#[test]
fn a_pool_left_on_the_default_does_not_offer_the_option() {
    let _serial = serial();
    let Some(pool) = start(1) else {
        return;
    };

    // The same observation from the other side, and the one that matters more: the default must
    // still produce a checked exchange. A `[backends.x]` that says nothing is what every existing
    // deployment has.
    assert_eq!(
        pool.no_checksum_agreed(),
        vec![false],
        "a pool that did not offer the option must not have it accepted"
    );
}

#[test]
fn a_worker_that_cannot_boot_is_refused_at_startup() {
    let _serial = serial();
    require_sdk!();

    // A file that exists but is not a worker: PHP parses it, does nothing, and
    // exits, so the handshake can never complete. This is the case that would
    // otherwise hang the first request for ever.
    let dir = std::env::temp_dir().join(format!("volucer-not-a-worker-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("not-a-worker.php");
    std::fs::write(&script, "<?php // deliberately says nothing\n").expect("write the script");

    let config = WorkerConfig {
        command: format!("php \"{}\"", script.display()),
        workers: 1,
        memory_limit_mb: None,
        max_requests: None,
        working_dir: None,
        env: Default::default(),
        checksum: Default::default(),
        services: Vec::new(),
        transport: Default::default(),
    };

    let err = WorkerPool::start("test", &config).unwrap_err();
    let message = err.to_string();
    // Either the process exited (and its exit is the useful part) or the pipe
    // ended first. Both are startup errors, not hangs.
    assert!(
        message.contains("exited during startup") || message.contains("closed"),
        "unexpected error: {message}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A worker still written against the older API fails *fast* and says what to do.
///
/// This is the blast radius of making the socket the default on Windows: the transport is set
/// by the *server*, and a worker written before it exists — the shipped SDK's own README used
/// to teach `Worker::create()` — reads stdin, which the socket transport has set to
/// end-of-file, and exits during its handshake. The failure is therefore correct and does not
/// need to be prevented; what it must not be is *unexplained*, because "exit code 255" sends
/// the reader to PHP, where nothing is wrong.
///
/// The socket transport is requested explicitly rather than taken from the default, so this
/// asserts the same thing on every platform.
#[test]
fn a_worker_still_on_create_is_told_to_call_connect() {
    let _serial = serial();
    require_sdk!();

    let dir = std::env::temp_dir().join(format!("volucer-old-worker-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("old-style-worker.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Response;
use Volucer\Worker;
// Deliberately the old API: this is what a worker written before the option did.
$worker = Worker::create();
$worker->run(static function ($request, Worker $worker): Response {{
    return Response::text('ok');
}});
"#
        ),
    )
    .expect("write the worker");

    let mut config = config(1);
    config.transport = volucer_core::config::WorkerTransport::Socket;
    // The fixture, not the example worker: `config()` points at the SDK's example, which
    // calls `connect()` and would therefore connect perfectly well.
    config.command = format!("php \"{}\"", script.display());

    let started = std::time::Instant::now();
    let err = WorkerPool::start("test", &config).unwrap_err();
    let elapsed = started.elapsed();
    let message = err.to_string();

    // Fast, because the worker is known to be gone rather than waited out.
    assert!(
        elapsed < Duration::from_secs(10),
        "a worker that exits must be reported promptly, not waited out: took {elapsed:?}"
    );
    // And it names both ways out: the compatible fix, and the setting that restores the old
    // behaviour. A hint that named only one of them would leave half the readers stuck.
    assert!(
        message.contains("connect()"),
        "the message must name the fix the SDK ships: {message}"
    );
    assert!(
        message.contains("transport = \"pipe\""),
        "and the setting that keeps the worker as it is: {message}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The hint is specific to the transport, so a pipe worker does not get it.
///
/// A worker that dies during its handshake on the pipe is an ordinary application failure, and
/// appending advice about `connect()` to every one of them would be noise that teaches readers
/// to ignore the hint.
#[test]
fn a_pipe_worker_that_dies_is_not_told_about_the_socket() {
    let _serial = serial();
    require_sdk!();

    let dir = std::env::temp_dir().join(format!("volucer-dying-pipe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("dying-worker.php");
    std::fs::write(&script, "<?php exit(3);\n").expect("write the script");

    let mut config = config(1);
    config.transport = volucer_core::config::WorkerTransport::Pipe;
    config.command = format!("php \"{}\"", script.display());

    let err = WorkerPool::start("test", &config).unwrap_err();
    let message = err.to_string();
    // Either the exit was observed or the pipe ended first — both are startup failures, and
    // which one wins is a race that is not what this test is about.
    assert!(
        message.contains("exited during startup") || message.contains("closed"),
        "the failure is still a startup one: {message}"
    );
    assert!(
        !message.contains("connect()"),
        "a pipe failure must not be explained as a socket one: {message}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
/// The pool's own account of how long requests waited for a slot.
///
/// The counter exists to tell a queueing tail apart from a slow server, so the test is
/// that it counts in both directions: more requests than workers must make it report
/// waiting, and one request at a time on an idle pool must make it report none. The
/// second half is the one that matters. A counter wired to something that always
/// reports a number would pass the first half and be worse than useless, because the
/// whole point of reading it is to be told the pool is innocent.
#[test]
fn the_pool_reports_the_time_requests_spent_waiting_for_a_slot() {
    let _serial = serial();
    require_sdk!();

    // A pool of two, so eight simultaneous requests have to queue three deep. The
    // handler sleeps rather than returning at once, because the wait being measured is
    // the time spent behind another *request*, and a handler that could not be caught
    // mid-request would leave nothing to wait behind.
    let dir = std::env::temp_dir().join(format!("volucer-wait-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("slow-worker.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;
$worker = Worker::connect();
$worker->run(static function (Request $request, Worker $worker): Response|null {{
    usleep(50000);
    return Response::json(['pid' => getmypid()]);
}});
"#
        ),
    )
    .expect("write the worker");

    let workers = 2;
    let config = WorkerConfig {
        command: format!("php \"{}\"", script.display()),
        workers,
        memory_limit_mb: None,
        max_requests: None,
        working_dir: None,
        env: Default::default(),
        checksum: Default::default(),
        services: Vec::new(),
        transport: Default::default(),
    };
    let pool = WorkerPool::start("test", &config).expect("pool starts");

    // Idle: one request against two workers has nothing to wait for, so the pool must say
    // so. Not *zero*: the figure includes the scan's own lock-and-release, which every
    // request pays and which is microseconds. So the assertion is a floor rather than an
    // equality, and the floor is set far below anything a queue for a request could be.
    // `requests_served` is read alongside it, because a mean of zero and a mean of
    // nothing-measured are the same number otherwise.
    let single = pool.request(&get("/")).expect("the first request answers");
    assert_eq!(single.parse_status_headers().0, 200);
    assert_eq!(pool.requests_served(), 1);
    let idle = pool.mean_slot_wait_us();
    assert!(
        idle < 1_000,
        "one request against two idle workers waits for nothing but the scan: {idle} us"
    );

    // Contended: eight requests for two workers. The later batches each wait for a whole
    // 50 ms request to finish, so the pool's mean is at least tens of milliseconds.
    let requests: usize = 8;
    let contended_at = std::time::Instant::now();
    let answers: Vec<u16> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..requests)
            .map(|_| scope.spawn(|| pool.request(&get("/")).map(|r| r.parse_status_headers().0)))
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .expect("no thread panics")
                    .expect("every request answers")
            })
            .collect()
    });
    let contended_for = contended_at.elapsed();
    assert!(answers.iter().all(|status| *status == 200));

    assert_eq!(pool.requests_served(), 1 + requests as u64);
    let (busiest, busiest_us) = pool.busiest_slot_wait_us();
    let mean = pool.mean_slot_wait_us();
    // The wall clock comes first because it says whether there was anything to queue for.
    // Eight 50 ms requests on two workers is four batches, so anything near eight batches
    // means they never overlapped and a wait of zero would be the truth rather than a bug.
    assert!(
        contended_for >= Duration::from_millis(150),
        "eight 50 ms requests on two workers take at least four batches: {contended_for:?}"
    );
    assert!(
        mean >= 20_000,
        "eight requests on two 50 ms workers queue for tens of milliseconds each, not {mean} us \
         (busiest slot {busiest} at {busiest_us} us, {contended_for:?} wall clock)"
    );

    // And the wait lands on the slots rather than on the pool as a whole: a request waits
    // for the slot it was pointed at, so the busiest slot holds on the order of 8 x 50 ms
    // / 2, and is never a slot with nothing attributed to it.
    assert!(busiest < workers as usize);
    assert!(
        busiest_us >= mean,
        "the busiest slot is at least the mean: {busiest_us} us vs {mean} us"
    );

    // The distribution, not just the mean: the histogram has to put the median below the
    // tail and the tail above the median, or the two figures are not measuring a spread.
    // A mean alone cannot tell contention that is spread evenly from contention that
    // starves a few, and the second kind is the one that makes a latency tail.
    let p50 = pool.slot_wait_percentile_us(0.50);
    let p99 = pool.slot_wait_percentile_us(0.99);
    assert!(
        p50 <= p99,
        "the median wait cannot exceed the tail: p50 {p50} us, p99 {p99} us"
    );
    assert!(
        p99 >= 20_000,
        "one queueing wait in a hundred is a whole 50 ms request: p99 is {p99} us"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
/// A worker whose handler sleeps, so a slot is observably occupied.
///
/// Written to a temp directory per test rather than shared, because the tests that need it
/// run concurrently and each wants its own pool.
fn slow_worker(tag: &str, micros: u64) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("volucer-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("slow-worker.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;
$worker = Worker::connect();
$worker->run(static function (Request $request, Worker $worker): Response|null {{
    usleep({micros});
    return Response::json(['pid' => getmypid(), 'uri' => $request->uri]);
}});
"#
        ),
    )
    .expect("write the worker");
    script
}

/// A pool of `workers` slow workers, with the wait policy set explicitly.
fn slow_pool(tag: &str, workers: u32, micros: u64, max_wait: Option<Duration>) -> WorkerPool {
    let script = slow_worker(tag, micros);
    let config = WorkerConfig {
        command: format!("php \"{}\"", script.display()),
        workers,
        memory_limit_mb: None,
        max_requests: None,
        working_dir: None,
        env: Default::default(),
        checksum: Default::default(),
        services: Vec::new(),
        transport: Default::default(),
    };
    let mut pool = WorkerPool::start("test", &config).expect("pool starts");
    pool.set_max_wait(max_wait);
    pool
}

/// A slot leaves the free list when a request takes it and returns when that request ends.
///
/// This is the invariant the old scan could not state, let alone keep: it asked whether a
/// worker was *healthy*, which is true of a worker that is busy, so the number of slots
/// actually available was never something it knew. Here it is a number, and an idle pool must
/// have all of them.
#[test]
fn the_free_list_accounts_for_every_slot_exactly_once() {
    let _serial = serial();
    require_sdk!();

    let workers = 3;
    let pool = slow_pool("freelist", workers, 100_000, Some(Duration::from_secs(30)));
    assert_eq!(
        pool.free_slots(),
        workers as usize,
        "an idle pool offers every slot"
    );

    // Take them all at once and hold them: the handler sleeps, so the slots stay leased for
    // the duration. One extra request goes beyond the pool and must queue for a slot, which is
    // the other half of the policy.
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers as usize)
            .map(|_| {
                scope.spawn(|| {
                    let response = pool.request(&get("/")).expect("every slot is leased");
                    assert_eq!(response.parse_status_headers().0, 200);
                })
            })
            .collect();
        let queued = scope.spawn(|| {
            pool.request(&get("/"))
                .expect("a request beyond the pool waits and is then served")
                .parse_status_headers()
                .0
        });
        for handle in handles {
            handle.join().expect("no thread panics");
        }
        assert_eq!(queued.join().expect("no thread panics"), 200);
    });

    assert_eq!(
        pool.free_slots(),
        workers as usize,
        "every slot came back, so nothing leaked"
    );
    assert_eq!(pool.requests_served(), workers as u64 + 1);
}

/// A full pool refuses once the wait is up, rather than growing a queue without end.
///
/// The policy that bounds the tail. `None` is the strictest form - refuse as soon as no slot
/// is free - and the test is that the second request comes back *quickly* and as busy, rather
/// than being made to wait for the first one's whole duration.
#[test]
fn a_full_pool_refuses_rather_than_queueing_when_the_wait_is_none() {
    let _serial = serial();
    require_sdk!();

    let hold = 400_000;
    let pool = slow_pool("refuse", 1, hold, None);
    assert_eq!(pool.free_slots(), 1);

    std::thread::scope(|scope| {
        // Hold the only slot.
        let holder = scope.spawn(|| {
            pool.request(&get("/"))
                .expect("the first request has a slot");
        });

        // Wait until the slot is actually taken before asking for it, so this cannot pass by
        // racing the holder to it.
        let mut waited = Duration::ZERO;
        while pool.free_slots() != 0 && waited < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
            waited += Duration::from_millis(5);
        }
        assert_eq!(pool.free_slots(), 0, "the holder took the only slot");

        let refused_at = std::time::Instant::now();
        let outcome = pool.request(&get("/"));
        let refused_in = refused_at.elapsed();
        holder.join().expect("no thread panics");

        let err = outcome.expect_err("a full pool with no wait refuses");
        assert!(
            err.is_busy(),
            "a refused request is a capacity condition, not a failure: {err}"
        );
        assert!(
            refused_in < Duration::from_millis(200),
            "it was refused at once rather than made to wait {} ms for the holder",
            hold / 1000
        );
    });

    assert_eq!(pool.free_slots(), 1, "the holder's slot came back");
}

/// A slot comes back even when the request on it fails.
///
/// The leak this guards against is the quiet kind: every early return on the request path is
/// a place a release could be forgotten, and a pool that loses one slot per failure shrinks
/// without ever reporting an error. The release is a `Drop` for exactly this reason, and this
/// is the test that says so.
#[test]
fn a_slot_comes_back_after_a_failed_request() {
    let _serial = serial();
    require_sdk!();

    let workers = 2;
    let pool = slow_pool("afterfail", workers, 10_000, Some(Duration::from_secs(5)));
    assert_eq!(
        pool.request(&get("/"))
            .expect("warm-up")
            .parse_status_headers()
            .0,
        200
    );
    assert_eq!(pool.free_slots(), workers as usize);

    // Kill the workers and make requests land on them. However that fails and however many
    // attempts it takes, the accounting must end up whole.
    for pid in pool.pids() {
        kill(pid);
    }
    for _ in 0..3 {
        let _ = pool.request(&get("/"));
        assert_eq!(
            pool.free_slots(),
            workers as usize,
            "no slot was lost to the failures"
        );
    }
}

/// A metric a worker emitted reaches a registry, through a real scrape.
///
/// **This is the end-to-end proof of step 1, and it is the one thing that could not be tested in
/// pieces.** The registry is covered on its own, and so is the encoder; what is only observable here
/// is that a `METRIC` frame from a live PHP process — through the SDK's `emitMetric`, over the real
/// transport, decoded by the real codec — reaches the registry the metrics endpoint renders. Every
/// piece could pass its own tests with the wire between them disconnected, which is exactly the
/// failure the `absorb_metric` call this feature builds on had: it decoded the frame and dropped it.
///
/// The SDK's example worker emits `example.request` on every request, so no fixture is involved and a
/// change to either side fails here.
#[test]
fn a_workers_own_metric_reaches_the_registry() {
    let Some(pool) = start(1) else {
        return;
    };

    let registry = volucer_metrics::Registry::new();
    // Declared, because the registry refuses a name nobody described — and declaring it here rather
    // than in the pool is the point: a worker publishing a metric is publishing a name the *operator*
    // has to have decided to accept. `docs/plugins.md` §6.1 has the `[metrics] collect` table that will
    // make this a configuration entry rather than a call a test makes by hand.
    registry.describe_histogram(
        "example.request",
        "Requests the example worker counted",
        &[],
    );

    let answer = pool.request(&get("/")).expect("a response");
    assert!(
        answer.stdout.starts_with(b"Status: 200"),
        "the request should have been served: {}",
        String::from_utf8_lossy(&answer.stdout)
    );

    pool.publish_metrics(&registry);
    let rendered = registry.render();
    assert!(
        rendered.contains("example.request_count 1"),
        "the worker's own sample should reach the registry:\n{rendered}"
    );
    assert!(
        rendered.contains("# HELP example.request Requests the example worker counted"),
        "and keep the help text the operator declared, not the worker's:\n{rendered}"
    );
}

/// A sample is counted once, however many times a scrape runs.
///
/// The drain is what makes this true, and it is the property that would be easiest to get wrong: a
/// queue that were copied rather than drained would make a worker's single emission appear to grow
/// with the number of scrapes, and a graph of it would climb for a reason that has nothing to do with
/// the application.
#[test]
fn a_sample_is_drained_and_not_recounted_by_a_second_scrape() {
    let Some(pool) = start(1) else {
        return;
    };

    let registry = volucer_metrics::Registry::new();
    registry.describe_histogram("example.request", "Requests", &[]);

    pool.request(&get("/")).expect("a response");
    pool.publish_metrics(&registry);
    pool.publish_metrics(&registry);
    pool.publish_metrics(&registry);

    let rendered = registry.render();
    assert!(
        rendered.contains("example.request_count 1"),
        "three scrapes, one sample:\n{rendered}"
    );
}

/// A live worker makes a call mid-request and the registry answers it.
///
/// **This is the test the whole call plane was missing.** Everything below it had been proven
/// separately — the channel answers a `CALL` when a handler is installed, the registry routes
/// `"<namespace>.<verb>"` to a namespace, `WorkersNamespace` reads the pools, `Dispatcher::from_config`
/// installs the registry on a channel before its handshake — and none of it had ever been connected to a
/// running PHP process. So a deployment could have had every part correct and a call that never arrives,
/// which is exactly the failure a unit test per layer cannot see.
///
/// It goes through `Dispatcher::from_config`, not through a registry built by hand, because *that* is the
/// wiring under test: a registry the test assembles would prove the pieces work together while leaving the
/// code that assembles them in production unexercised. The consequence is visible in the assertions — the
/// worker's own `backend` name in `workers.list` comes from the directory the dispatcher filled in.
///
/// The path exercised is the whole of `docs/plugins.md` §4 end to end: application code inside a request
/// handler writes a frame the server did not ask for, the server's exchange thread — mid-request, holding
/// the pipes — recognises it as a call rather than a protocol error, dispatches it into the registry, and
/// the worker reads the answer out of a stream that still owes it a response.
#[test]
fn a_worker_calls_the_server_mid_request_and_the_registry_answers() {
    let _serial = serial();
    require_sdk!();

    let dir = std::env::temp_dir().join(format!("volucer-call-worker-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("call-worker.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;
$worker = Worker::connect();
$worker->run(static function (Request $request, Worker $worker): Response|null {{
    // Every call happens *inside* the handler, which is the only place it can happen: the server's
    // reader for this worker is parked between requests.
    $pool = $worker->call('workers.list');
    $description = $worker->call('config.describe');
    $refused = $worker->call('kv.get', ['key' => 'session:1']);

    return Response::json([
        'workers' => count($pool),
        'backends' => implode(',', array_unique(array_column($pool, 'backend'))),
        'idle_runtimes' => array_values(array_filter(array_column($pool, 'runtime'))),
        'unobservable' => count(array_filter($pool, static fn ($w) => $w['pid'] === 0)),
        // Read from the application's side of the channel, because that is the side redaction
        // protects: an application that can read the deployment's credentials can leak them. The
        // `command` is here as the control — it must survive, or the assertion above it would pass
        // against an answer that was redacted into nothing.
        'described_command' => $description['backends']['app']['command'] ?? null,
        'refused_error' => $worker->lastCallError(),
        'refused' => $refused,
    ]);
}});
"#
        ),
    )
    .expect("write the worker");

    let backend = BackendConfig::Worker(WorkerConfig {
        command: format!("php \"{}\"", script.display()),
        // Two workers, so the pool has a slot that is *not* the one serving this request. The caller's
        // own slot cannot describe itself — its lock is held by the request it is being called from —
        // and a pool of one would make that the only entry, hiding the difference between "reporting
        // works" and "reporting works for everyone except the caller".
        workers: 2,
        // Both namespaces, because `workers.list` needs one and `config.describe` the other, and the
        // point is that they arrive through the configuration rather than through a test's wiring.
        services: vec!["config".to_string(), "workers".to_string()],
        ..config(2)
    });
    let configured = Config {
        backends: [("app".to_string(), backend)].into_iter().collect(),
        default_backend: Some("app".to_string()),
        ..Config::default()
    };

    let dispatcher =
        Dispatcher::from_config(&configured, Routing::default()).expect("the pool starts");
    let response = dispatcher
        .dispatch(&get("/"))
        .expect("the request is served");
    let (status, _) = response.parse_status_headers();
    assert_eq!(
        status, 200,
        "the request that made the calls still succeeds"
    );

    let body = body_of(&response);
    let answer: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|err| panic!("the worker answered with JSON: {err}\n{body}"));
    assert_eq!(
        answer["workers"], 2,
        "the worker asked how many workers there are and got an answer from the live pool: {answer}"
    );
    assert_eq!(
        answer["backends"], "app",
        "and the entries name the backend the dispatcher filed the pool under, which is what a \
         worker.reset index has to be resolved against: {answer}"
    );
    assert!(
        answer["described_command"]
            .as_str()
            .is_some_and(|c| c.contains("call-worker.php")),
        "config.describe returned the backend's command, so the answer was assembled and not \
         redacted away: {answer}"
    );
    assert!(
        answer["refused"].is_null() && answer["refused_error"].as_str().is_some(),
        "a method no namespace serves is refused by name rather than killing the worker: {answer}"
    );
    assert!(
        answer["refused_error"]
            .as_str()
            .is_some_and(|e| e.contains("kv")),
        "and the refusal names the namespace that is missing, which is the whole of what a caller \
         can act on: {answer}"
    );

    // The two entries are the two halves of the `try_lock` rule that this test forced. The sibling is
    // idle and describes itself fully; the worker that is *asking* is serving a request, so its slot
    // lock is held by that request and it reports nothing about itself.
    //
    // The runtime string is `"<version> (<sapi>)"` — `8.3.31 (cli)` on the machine this was written on —
    // so it is asserted as that shape rather than as a substring "PHP", which it does not contain.
    assert!(
        answer["idle_runtimes"].as_array().is_some_and(|runtimes| {
            runtimes.iter().any(|r| {
                r.as_str()
                    .is_some_and(|r| r.contains('(') && r.contains('.'))
            })
        }),
        "the idle sibling reports the PHP version and SAPI it handshook with: {answer}"
    );
    assert_eq!(
        answer["unobservable"], 1,
        "and exactly one entry could not be read — the caller's own, which is locked by the request \
         it is calling from. That is the deadlock this test found: taking that lock would hang until \
         the request deadline killed the worker: {answer}"
    );

    // The pool still serves afterwards, which is the property that makes a call safe: it is an extra
    // frame in an exchange, not a second exchange.
    let again = dispatcher.dispatch(&get("/")).expect("a second request");
    assert_eq!(again.parse_status_headers().0, 200);
}

/// A live worker stores a value and the next request finds it, through the real store.
///
/// The first test in the tree where the *whole* path is real: a PHP application calls `Kv::set` inside a
/// request handler, the call crosses the worker's channel to the exchange thread, the registry routes it
/// into the `kv` namespace, a real `MemoryStore` holds the bytes, and the `get` that follows comes back
/// with them. Every layer of §4 and §6.2 is in this one run, and no test per layer can see a missing
/// `build_stores` — which is the same class of seam the call-plane test above found for the call path
/// itself.
///
/// It also pins what a store is easiest to get wrong: **two requests reach the same store**. A
/// per-request or per-connection store would pass everything else here and be useless, because the point
/// of a shared cache is that the next request finds what the last one left.
#[test]
fn a_worker_stores_a_value_and_the_next_request_finds_it() {
    let _serial = serial();
    require_sdk!();

    let dir = std::env::temp_dir().join(format!("volucer-kv-worker-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("kv-worker.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;
$worker = Worker::connect();
$worker->run(static function (Request $request, Worker $worker): Response|null {{
    $kv = $worker->kv('sessions');
    $path = (string) parse_url($request->uri, PHP_URL_PATH);

    if ($path === '/write') {{
        $kv->set('greeting', ['hello' => 'world'], 60);
        $kv->set('transient', 'gone soon', 1);
        $found = null;
        $kv->get('never-written', $found);

        return Response::json([
            'stored' => true,
            'missing_was_found' => $found,
            'ttl' => $kv->ttl('greeting'),
        ]);
    }}

    // A different request, and possibly a different worker process.
    $found = null;
    $value = $kv->get('greeting', $found);
    $transient = $kv->get('transient', $transient_found);

    return Response::json([
        'value' => $value,
        'found' => $found,
        'has' => $kv->has('greeting'),
        'transient' => $transient,
        'transient_found' => $transient_found,
    ]);
}});
"#
        ),
    )
    .expect("write the worker");

    let backend = BackendConfig::Worker(WorkerConfig {
        command: format!("php \"{}\"", script.display()),
        workers: 2,
        // The one line this test exists to check is honoured: without it the registry has no `kv` and
        // every call is refused, which is the default and the right default.
        services: vec!["kv.sessions".to_string()],
        ..config(2)
    });
    let mut configured = Config {
        backends: [("app".to_string(), backend)].into_iter().collect(),
        default_backend: Some("app".to_string()),
        ..Config::default()
    };
    configured.kv.insert(
        "sessions".to_string(),
        KvStorageConfig::Memory(MemoryKvConfig { max_bytes: 1 << 20 }),
    );

    let dispatcher =
        Dispatcher::from_config(&configured, Routing::default()).expect("the pool starts");

    let written = json_body(
        &dispatcher
            .dispatch(&get("/write"))
            .expect("the write is served"),
    );
    assert_eq!(written["stored"], true, "{written}");
    assert_eq!(
        written["missing_was_found"], false,
        "a key nobody wrote is reported absent, so `get`'s found flag is not always true: {written}"
    );
    assert_eq!(
        written["ttl"], 60,
        "and the TTL went in as seconds and came back as seconds: {written}"
    );

    let read = json_body(
        &dispatcher
            .dispatch(&get("/read"))
            .expect("the read is served"),
    );
    assert_eq!(
        read["value"],
        serde_json::json!({ "hello": "world" }),
        "the next request — possibly on the other worker — finds what the first one stored: {read}"
    );
    assert_eq!(read["found"], true, "{read}");
    assert_eq!(
        read["has"], true,
        "and `has` answers without the value having to be copied out to find out: {read}"
    );

    // A one-second TTL that has expired is *gone*. The sleep is the only way to test expiry at all, and
    // what it proves is that the deadline the client sent reached the store — which a store that ignored
    // TTLs entirely would pass every other assertion here without.
    std::thread::sleep(Duration::from_millis(1100));
    let after = json_body(
        &dispatcher
            .dispatch(&get("/read"))
            .expect("the read is served"),
    );
    assert_eq!(
        after["transient"],
        serde_json::Value::Null,
        "a key stored with a one-second TTL is gone after one: {after}"
    );
    assert_eq!(
        after["transient_found"], false,
        "and is reported absent rather than as a value of null: {after}"
    );
    assert_eq!(
        after["value"],
        serde_json::json!({ "hello": "world" }),
        "while the key with a sixty-second TTL is untouched: {after}"
    );
}

/// A call naming a storage the backend was not granted is refused, by name, over a real channel.
///
/// §4.5 as one request: the deployment has two storages, the backend was granted one, and the other is
/// unreachable — with a message naming what it *may* reach, so the fix is a configuration line rather
/// than an hour with a debugger.
#[test]
fn a_storage_outside_the_grant_is_refused_over_a_real_channel() {
    let _serial = serial();
    require_sdk!();

    let dir = std::env::temp_dir().join(format!("volucer-kv-denied-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("denied-worker.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;
$worker = Worker::connect();
$worker->run(static function (Request $request, Worker $worker): Response|null {{
    try {{
        $worker->kv('secrets')->get('key');
        return Response::json(['reached' => true]);
    }} catch (Throwable $e) {{
        // The refusal arrives as an exception from `call`, which is the point: an application can see it
        // and log it rather than being handed a value that is a lie.
        return Response::json(['reached' => false, 'message' => $e->getMessage()]);
    }}
}});
"#
        ),
    )
    .expect("write the worker");

    let backend = BackendConfig::Worker(WorkerConfig {
        command: format!("php \"{}\"", script.display()),
        workers: 1,
        // `sessions` only. `secrets` exists in the deployment and is not granted.
        services: vec!["kv.sessions".to_string()],
        ..config(1)
    });
    let mut configured = Config {
        backends: [("app".to_string(), backend)].into_iter().collect(),
        default_backend: Some("app".to_string()),
        ..Config::default()
    };
    for name in ["sessions", "secrets"] {
        configured.kv.insert(
            name.to_string(),
            KvStorageConfig::Memory(MemoryKvConfig { max_bytes: 1 << 20 }),
        );
    }

    let dispatcher =
        Dispatcher::from_config(&configured, Routing::default()).expect("the pool starts");
    let answer = json_body(
        &dispatcher
            .dispatch(&get("/"))
            .expect("the request is served"),
    );

    assert_eq!(
        answer["reached"], false,
        "a storage this backend was not granted must be unreachable: {answer}"
    );
    let message = answer["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("secrets"),
        "the refusal names what was asked for: {message}"
    );
    assert!(
        message.contains("sessions"),
        "and what may be reached, so the fix is a configuration line: {message}"
    );
}

/// The JSON body of a response, as the fixtures write it.
fn json_body(response: &volucer_fcgi::Response) -> serde_json::Value {
    let body = body_of(response);
    serde_json::from_str(&body)
        .unwrap_or_else(|err| panic!("the worker answered with JSON: {err}\n{body}"))
}

/// A lock acquired by one request is contended by the next, over a real channel.
///
/// The same argument as the KV test above and the third instance of the pattern: `LockNamespace`,
/// `MemoryLocks`, `build_locks` and the `services` allow-list were each tested separately, and the thing
/// that could still be wrong is the seam — `build_locks` returning `None` while the configuration granted
/// `lock`, or the namespace being installed under a name the router does not key on. So this goes through
/// `Dispatcher::from_config` with a real PHP process and calls the methods from inside a request handler.
///
/// It asserts the three answers an acquisition has, in one run:
///
/// - **acquired**, with the id the server generated, because the caller sent none — the client-side half
///   of that design is only useful if the id comes back;
/// - **contended** (`ok` true, `acquired` false), which is an ordinary answer rather than an error, and
///   the distinction RoadRunner's own documentation is emphatic about;
/// - **released by the id it was given**, and then not contended any more — which is what makes the id
///   meaningful rather than decorative.
#[test]
fn a_lock_acquired_by_one_request_is_contended_by_the_next() {
    let _serial = serial();
    require_sdk!();

    let dir = std::env::temp_dir().join(format!("volucer-lock-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("lock-worker.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;
$worker = Worker::connect();

// Held across requests by being taken with no lease: `ttl` absent means "until released", which is the
// one lock semantic that is the opposite of what the same key means to a KV value.
$held = null;
$held_id = null;

$worker->run(static function (Request $request, Worker $worker) use (&$held, &$held_id): Response {{
    $path = parse_url($request->uri, PHP_URL_PATH);

    if ($path === '/hold') {{
        $acquired = $worker->call('lock.acquire', ['resource' => 'pdf:create']);
        $held = (bool) $acquired['acquired'];
        $held_id = $acquired['id'];
        return Response::json([
            'acquired' => $acquired['acquired'],
            'id_is_set' => $held_id !== null && $held_id !== '',
            'mode' => $acquired['mode'],
            // An id was generated for us, so `remaining_ms` is null because the lease has no deadline.
            'remaining_is_null' => $acquired['remaining_ms'] === null,
        ]);
    }}

    if ($path === '/contend') {{
        // The same worker, a second call, while the first lock is still held. Note that the id is a
        // *new* one: contending with itself is refused with an error rather than answered with
        // `acquired: false`, and neither of those is what this asserts.
        $second = $worker->call('lock.acquire', ['resource' => 'pdf:create']);
        $exists = $worker->call('lock.exists', ['resource' => 'pdf:create']);
        $holders = $worker->call('lock.holders', ['resource' => 'pdf:create']);
        return Response::json([
            'acquired' => $second['acquired'],
            'error' => $worker->lastCallError(),
            'exists' => $exists['exists'],
            'holders' => count($holders['holders']),
        ]);
    }}

    if ($path === '/release') {{
        $released = $worker->call('lock.release', ['resource' => 'pdf:create', 'id' => $held_id]);
        $after = $worker->call('lock.exists', ['resource' => 'pdf:create']);
        // A second release of the same id is `false` rather than an error, which is what a `finally`
        // block depends on: cleanup that could throw is cleanup that gets skipped.
        $again = $worker->call('lock.release', ['resource' => 'pdf:create', 'id' => $held_id]);
        return Response::json([
            'released' => $released['released'],
            'again' => $again['released'],
            'exists_after' => $after['exists'],
        ]);
    }}

    return Response::json(['error' => 'unknown path']);
}});
"#
        ),
    )
    .expect("write the worker");

    let backend = BackendConfig::Worker(WorkerConfig {
        command: format!("php \"{}\"", script.display()),
        // One worker, deliberately: the lock has to be visible to the *same* worker's next request, and a
        // pool of two would make the test depend on which slot served it. Cross-worker reachability is
        // what the memory driver's shared registry gives, and the KV test above already covers that shape.
        workers: 1,
        services: vec!["lock".to_string()],
        ..config(1)
    });
    let mut configured = Config {
        backends: [("app".to_string(), backend)].into_iter().collect(),
        default_backend: Some("app".to_string()),
        ..Config::default()
    };
    configured.lock = Some(LockConfig::Memory(MemoryLockConfig {}));

    let dispatcher =
        Dispatcher::from_config(&configured, Routing::default()).expect("the pool starts");

    let held = json_body(
        &dispatcher
            .dispatch(&get("/hold"))
            .expect("the hold is served"),
    );
    assert_eq!(held["acquired"], true, "{held}");
    assert_eq!(
        held["id_is_set"], true,
        "the server generated an id and the answer carries it, which is the only way a caller that sent \
         none can release the lock: {held}"
    );
    assert_eq!(
        held["mode"], "exclusive",
        "and the default mode is exclusive, because a shared default would stop two writers excluding \
         each other: {held}"
    );
    assert_eq!(
        held["remaining_is_null"], true,
        "`ttl` absent means held until released, so there is no remaining time: {held}"
    );

    let contended = json_body(
        &dispatcher
            .dispatch(&get("/contend"))
            .expect("the contention is served"),
    );
    assert_eq!(
        contended["acquired"], false,
        "a second acquisition contends rather than failing: {contended}"
    );
    assert_eq!(
        contended["error"],
        serde_json::Value::Null,
        "and `lastCallError` stays null, because contention is a successful call: {contended}"
    );
    assert_eq!(
        contended["exists"], true,
        "exists answers the question a caller resolving an unknown state asks: {contended}"
    );
    assert_eq!(contended["holders"], 1, "{contended}");

    let released = json_body(
        &dispatcher
            .dispatch(&get("/release"))
            .expect("the release is served"),
    );
    assert_eq!(released["released"], true, "{released}");
    assert_eq!(
        released["again"], false,
        "releasing what is already gone is an answer, not an error: {released}"
    );
    assert_eq!(released["exists_after"], false, "{released}");
}

/// A lock call from a backend that was not granted the service is refused, by name.
///
/// The capability half of the same seam, and the one worth asserting separately: the *default* is that no
/// worker may lock anything, so a deployment that forgets the `services` line gets a refusal rather than a
/// distributed lock it did not mean to hand out.
#[test]
fn a_lock_call_from_a_backend_without_the_grant_is_refused() {
    let _serial = serial();
    require_sdk!();

    let dir = std::env::temp_dir().join(format!("volucer-lock-denied-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("denied-lock-worker.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Volucer\Request;
use Volucer\Response;
use Volucer\Worker;
$worker = Worker::connect();
$worker->run(static function (Request $request, Worker $worker): Response {{
    $answer = $worker->call('lock.acquire', ['resource' => 'pdf:create']);
    return Response::json([
        'acquired' => $answer['acquired'],
        'error' => $worker->lastCallError(),
        'unknown' => $worker->lastCallWasUnknown(),
    ]);
}});
"#
        ),
    )
    .expect("write the worker");

    let backend = BackendConfig::Worker(WorkerConfig {
        command: format!("php \"{}\"", script.display()),
        // No `services` at all, which is the default and the right default.
        workers: 1,
        ..config(1)
    });
    let mut configured = Config {
        backends: [("app".to_string(), backend)].into_iter().collect(),
        default_backend: Some("app".to_string()),
        ..Config::default()
    };
    // The table is there; the *grant* is what is missing, which is the distinction that makes this a
    // capability test rather than a configuration test.
    configured.lock = Some(LockConfig::Memory(MemoryLockConfig {}));

    let dispatcher =
        Dispatcher::from_config(&configured, Routing::default()).expect("the pool starts");
    let answer = json_body(
        &dispatcher
            .dispatch(&get("/"))
            .expect("the request is served"),
    );

    assert!(
        answer["acquired"].is_null(),
        "a worker with no `lock` in its services list must not acquire anything, so the call never \
         reaches an acquisition at all: {answer}"
    );
    assert_eq!(
        answer["unknown"], false,
        "and a refused call is decided, not undetermined: {answer}"
    );
    let message = answer["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("lock"),
        "the refusal names the namespace that is missing: {message}"
    );
}
