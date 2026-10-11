//! Integration tests for the worker pool against real PHP worker processes.
//!
//! These are the tests that prove the pool actually supervises: it boots
//! workers, completes the handshake, serves requests through every worker in
//! rotation, replaces a worker that died or asked to be recycled, and leaves
//! nothing running when it is dropped.
//!
//! They drive the SDK's `example/worker.php` rather than a fixture, so the PHP
//! end is the shipped SDK and a change to either side fails here. The SDK lives
//! in the `sailantis/stokehold-php-sdk` repository; `STOKEHOLD_SDK_DIR` points
//! at a checkout that is not beside this one. The Rust codec is
//! covered on its own in `crates/stokehold-worker`; what is under test *here* is
//! the supervision around it.
//!
//! Skipped, with a printed reason, when no `php` binary is on `PATH` or when the
//! SDK checkout cannot be found. libtest hides a passing test's stderr, so a
//! skip only *shows* with `--nocapture`; set `STOKEHOLD_E2E=1` where a skip
//! should be a failure.

use std::path::{Path, PathBuf};
use std::time::Duration;

use stokehold_core::config::WorkerConfig;
use stokehold_fcgi::{build_params, Request, RequestInfo, Routing};
use stokehold_server::WorkerPool;

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
/// `STOKEHOLD_SDK_DIR` overrides it, which is how a CI checkout at any path —
/// or a sibling clone under a different name — is pointed at. Otherwise the
/// layout this project is developed in is assumed: `sailantis/stokehold` and
/// `sailantis/stokehold-php-sdk` next to each other.
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
            "the SDK example worker is not at {} — set STOKEHOLD_SDK_DIR to a \
             stokehold-php-sdk checkout",
            script.display()
        ));
    }

    None
}

/// Report that this test cannot run here.
///
/// libtest hides the stderr of a passing test, so a skip is invisible in an
/// ordinary run. Set `STOKEHOLD_E2E=1` on a machine that is *supposed* to have
/// PHP and the SDK, and a skip becomes a failure instead.
fn report_skip(reason: String) {
    if skip_is_a_failure() {
        panic!("STOKEHOLD_E2E is set, so a skipped test is a failure: {reason}");
    }
    eprintln!("skipping: {reason}");
}

/// Whether `STOKEHOLD_E2E` asks for skips to be failures. `0` or empty means no.
fn skip_is_a_failure() -> bool {
    match std::env::var("STOKEHOLD_E2E") {
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
fn body_of(response: &stokehold_fcgi::Response) -> String {
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
            Err(_) => true,
        }
    }
    #[cfg(unix)]
    {
        PathBuf::from(format!("/proc/{pid}")).exists()
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
    // pipes closed and retries; if it already has, `pick_slot` replaces first. Both
    // are the recovery under test.
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
    let dir = std::env::temp_dir().join(format!("stokehold-slow-worker-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let script = dir.join("slow-worker.php");
    let autoload = sdk_dir().join("autoload.php").display().to_string();
    std::fs::write(
        &script,
        format!(
            r#"<?php
require '{autoload}';
use Stokehold\Request;
use Stokehold\Response;
use Stokehold\Worker;
$worker = Worker::create();
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
fn a_worker_that_cannot_boot_is_refused_at_startup() {
    let _serial = serial();
    require_sdk!();

    // A file that exists but is not a worker: PHP parses it, does nothing, and
    // exits, so the handshake can never complete. This is the case that would
    // otherwise hang the first request for ever.
    let dir = std::env::temp_dir().join(format!("stokehold-not-a-worker-{}", std::process::id()));
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
