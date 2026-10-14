//! Integration tests for the `spawn_cgi` pool against real `php-cgi` processes.
//!
//! These are the tests that prove the supervisor actually supervises: it starts
//! processes, serves requests through them, restarts a dead slot, recycles on
//! `max_requests`, and leaves nothing running when it is dropped.

use std::path::{Path, PathBuf};
use std::time::Duration;

use volucer_core::config::SpawnCgiConfig;
use volucer_fcgi::{build_params, Request, RequestInfo, Routing};
use volucer_server::CgiPool;

mod common;
use common::{find_php_cgi, free_port_run, serial, write_script};

fn config_for(binary: &Path, workers: u32, base_port: u16) -> SpawnCgiConfig {
    SpawnCgiConfig {
        binary: binary.to_path_buf(),
        workers,
        bind_host: "127.0.0.1".to_string(),
        base_port,
        socket: None,
        max_requests: None,
    }
}

/// A request for the script, built through the real CGI environment.
fn request_for(root: &Path, script_name: &str, marker: &str) -> Request {
    let routing = Routing {
        document_root: root.to_path_buf(),
        front_controller: Some(script_name.to_string()),
        ..Routing::default()
    };
    let body = marker.as_bytes().to_vec();
    let info = RequestInfo {
        method: "POST".to_string(),
        uri: "/".to_string(),
        body_len: body.len(),
        ..RequestInfo::default()
    };
    Request {
        params: build_params(&info, &routing).expect("params build"),
        body,
        keep_conn: true,
    }
}

/// A document root plus a script that echoes whatever arrived on php://input.
fn fixture(name: &str) -> (PathBuf, String) {
    let root = std::env::temp_dir().join(format!(
        "volucer-pool-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create the document root");
    write_script(
        &root,
        "index.php",
        r#"<?php echo "MARK="; echo file_get_contents("php://input");"#,
    );
    (root, "index.php".to_string())
}

/// A pool bound to unix sockets must serve exactly what the TCP pool serves.
///
/// This is the test that makes `socket = "unix:..."` a supported configuration
/// rather than a documented possibility: the same request, the same script, and
/// the same bytes back — with the difference that the transport is a file on disk
/// and `php-cgi`'s own `-b` is given a bare path rather than `host:port`.
#[cfg(unix)]
#[test]
fn a_pool_bound_to_unix_sockets_serves_the_same_bytes() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };

    let (root, script_name) = fixture("unix");
    let prefix = format!("/tmp/volucer-pool-unix-{}", std::process::id());
    let config = SpawnCgiConfig {
        binary: binary.clone(),
        workers: 2,
        bind_host: "127.0.0.1".to_string(),
        base_port: 19_900,
        socket: Some(format!("unix:{prefix}")),
        max_requests: None,
    };

    let mut pool = CgiPool::start("unix-test", &config).expect("a unix-socket pool starts");
    assert_eq!(pool.len(), 2);
    assert!(
        pool.ports().is_empty(),
        "a unix-socket pool binds no ports, got {:?}",
        pool.ports()
    );
    let expected_paths = vec![
        PathBuf::from(format!("{prefix}.0.sock")),
        PathBuf::from(format!("{prefix}.1.sock")),
    ];
    assert_eq!(pool.socket_paths(), expected_paths.as_slice());
    assert!(
        expected_paths.iter().all(|path| path.exists()),
        "php-cgi should have bound both paths"
    );

    for index in 0..4 {
        let response = pool
            .request(&request_for(&root, &script_name, &format!("unix{index}")))
            .expect("a unix-socket slot answered");
        response.check_protocol().expect("completed");
        let stdout = String::from_utf8_lossy(&response.stdout);
        assert!(
            stdout.contains(&format!("MARK=unix{index}")),
            "request {index} returned {stdout:?}"
        );
    }

    pool.shutdown();
    assert!(pool.is_empty());
    // A stopped pool must not leave its sockets behind: the next run would bind
    // them only because php-cgi unlinks first, and an operator would see litter
    // that looks like a live server.
    assert!(
        expected_paths.iter().all(|path| !path.exists()),
        "shutdown should have removed every socket"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A pool bound to Windows named pipes must serve exactly what the TCP pool serves.
///
/// `php-cgi -b` takes a pipe name on Windows instead of `host:port`, so this is the
/// supported way to keep the CGI arm off loopback TCP there. The same request, the
/// same script and the same bytes back — and a 4 KiB exchange costs about 20% less
/// per request than over loopback TCP (measured: 168 µs against 211 µs, spreads not
/// overlapping), which is the reason to prefer it.
#[cfg(windows)]
#[test]
fn a_pool_bound_to_named_pipes_serves_the_same_bytes() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let (root, script_name) = fixture("pipe");

    // A name unique to this process and test, so parallel binaries cannot collide.
    let prefix = format!(r"\\.\pipe\volucer-pool-{}", std::process::id());
    let config = SpawnCgiConfig {
        binary: binary.clone(),
        workers: 2,
        bind_host: "127.0.0.1".to_string(),
        base_port: 19_920,
        socket: Some(prefix.clone()),
        max_requests: None,
    };

    let mut pool = CgiPool::start("pipe-test", &config).expect("a pipe pool starts");
    assert_eq!(pool.len(), 2);
    assert!(
        pool.ports().is_empty(),
        "a pipe pool binds no ports, got {:?}",
        pool.ports()
    );
    let expected = vec![
        format!("{prefix}.0"),
        format!("{prefix}.1"),
    ];
    let mut endpoints = pool.endpoints();
    endpoints.sort();
    assert_eq!(endpoints, expected, "each slot gets its own pipe");

    for index in 0..4 {
        let response = pool
            .request(&request_for(&root, &script_name, &format!("pipe{index}")))
            .expect("a pipe slot answered");
        response.check_protocol().expect("completed");
        let stdout = String::from_utf8_lossy(&response.stdout);
        assert!(
            stdout.contains(&format!("MARK=pipe{index}")),
            "request {index} returned {stdout:?}"
        );
    }

    pool.shutdown();
    assert!(pool.is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

/// A dead unix-socket slot is replaced on the same path, not on a new one.
#[cfg(unix)]
#[test]
fn a_dead_unix_socket_slot_is_replaced_on_the_same_path() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };

    let (root, script_name) = fixture("unix-restart");
    let prefix = format!("/tmp/volucer-pool-unix-restart-{}", std::process::id());
    let config = SpawnCgiConfig {
        binary: binary.clone(),
        workers: 1,
        bind_host: "127.0.0.1".to_string(),
        base_port: 19_950,
        socket: Some(format!("unix:{prefix}")),
        max_requests: None,
    };

    let mut pool = CgiPool::start("unix-restart", &config).expect("pool starts");
    let before = pool.pids();
    kill(before[0]);
    std::thread::sleep(Duration::from_millis(300));

    let response = pool
        .request(&request_for(&root, &script_name, "unix-recovered"))
        .expect("the pool recovered");
    let stdout = String::from_utf8_lossy(&response.stdout);
    assert!(stdout.contains("MARK=unix-recovered"), "{stdout:?}");
    assert_ne!(pool.pids()[0], before[0], "the dead slot was replaced");
    assert_eq!(
        pool.endpoints(),
        vec![format!("unix:{prefix}.0.sock")],
        "the replacement kept the path"
    );

    pool.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn starts_a_pool_and_serves_requests_through_every_slot() {    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(3) else {
        eprintln!("skipping: no run of three free ports");
        return;
    };

    let (root, script_name) = fixture("serve");
    let mut pool = CgiPool::start("test", &config_for(&binary, 3, base)).expect("pool starts");

    assert_eq!(pool.len(), 3);
    // Consecutive ports from base_port, so every slot is reachable.
    assert_eq!(pool.ports(), vec![base, base + 1, base + 2]);
    assert!(pool.pids().iter().all(|pid| *pid != 0));

    for index in 0..3 {
        let response = pool
            .request(&request_for(&root, &script_name, &format!("slot{index}")))
            .expect("a slot answered");
        response.check_protocol().expect("completed");
        let stdout = String::from_utf8_lossy(&response.stdout);
        assert!(
            stdout.contains(&format!("MARK=slot{index}")),
            "request {index} returned {stdout:?}"
        );
    }

    pool.shutdown();
    assert!(pool.is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_killed_slot_is_replaced_and_the_pool_keeps_serving() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(1) else {
        eprintln!("skipping: no free port");
        return;
    };

    let (root, script_name) = fixture("restart");
    let mut pool = CgiPool::start("test", &config_for(&binary, 1, base)).expect("pool starts");
    let before = pool.pids();
    assert_eq!(before.len(), 1);

    // Kill the slot behind the pool's back, the way a crash or an OOM kill would.
    kill(before[0]);
    std::thread::sleep(Duration::from_millis(300));

    // The next request must still succeed: the pool restarts the slot in place.
    let response = pool
        .request(&request_for(&root, &script_name, "recovered"))
        .expect("the pool recovered");
    let stdout = String::from_utf8_lossy(&response.stdout);
    assert!(stdout.contains("MARK=recovered"), "{stdout:?}");

    let after = pool.pids();
    assert_eq!(after.len(), 1, "the pool kept its size");
    assert_ne!(after[0], before[0], "the dead slot was replaced");
    assert_eq!(pool.ports(), vec![base], "the replacement kept the port");

    pool.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn recycling_replaces_the_process_after_max_requests() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(1) else {
        eprintln!("skipping: no free port");
        return;
    };

    let (root, script_name) = fixture("recycle");
    let mut config = config_for(&binary, 1, base);
    config.max_requests = Some(2);
    let mut pool = CgiPool::start("test", &config).expect("pool starts");
    let original = pool.pids()[0];

    for _ in 0..2 {
        let response = pool
            .request(&request_for(&root, &script_name, "x"))
            .expect("a slot answered");
        assert!(String::from_utf8_lossy(&response.stdout).contains("MARK=x"));
    }

    // Two requests is the limit, so the slot must have been replaced.
    assert_ne!(
        pool.pids()[0],
        original,
        "the slot should have been recycled after max_requests"
    );
    assert_eq!(pool.ports(), vec![base], "the recycled slot kept the port");

    pool.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn dropping_the_pool_leaves_no_processes_behind() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };
    let Some(base) = free_port_run(2) else {
        eprintln!("skipping: no run of two free ports");
        return;
    };

    let pids = {
        let pool = CgiPool::start("test", &config_for(&binary, 2, base)).expect("pool starts");
        let pids = pool.pids();
        assert_eq!(pids.len(), 2);
        pids
        // `pool` is dropped here.
    };

    std::thread::sleep(Duration::from_millis(300));

    for pid in pids {
        assert!(
            !is_running(pid),
            "pid {pid} survived the pool being dropped"
        );
    }

    // The ports being free again is the symptom an orphan would cause.
    for offset in 0..2u16 {
        let port = base + offset;
        assert!(
            std::net::TcpListener::bind(("127.0.0.1", port)).is_ok(),
            "port {port} is still held after the pool was dropped"
        );
    }
}

#[test]
fn a_pool_skips_ports_that_are_already_in_use() {
    let _serial = serial();
    let Some(binary) = find_php_cgi() else {
        eprintln!("skipping: no php-cgi on PATH");
        return;
    };

    // Hold the base port itself, so the pool has to move past it.
    let blocker = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = blocker.local_addr().expect("address").port();
    // `base` comes from the ephemeral range, so `base + 100` can pass 65535 and
    // panic on overflow — which made this test fail or pass depending on which
    // port the OS handed out. Saturating arithmetic keeps the search in range.
    let next_free = (base.saturating_add(1)..base.saturating_add(100))
        .find(|port| std::net::TcpListener::bind(("127.0.0.1", *port)).is_ok());
    let Some(next_free) = next_free else {
        eprintln!("skipping: no free port after {base}");
        return;
    };

    let mut pool = CgiPool::start("test", &config_for(&binary, 1, base)).expect("pool starts");
    assert_eq!(pool.len(), 1);
    assert_eq!(
        pool.ports(),
        vec![next_free],
        "the occupied port should have been skipped"
    );

    pool.shutdown();
    drop(blocker);
}

/// Kill a process by id, without pulling in a signal crate.
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

/// Whether a process is still running.
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
        PathBuf::from(format!("/proc/{pid}")).exists()
            || std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .map(|status| status.success())
                .unwrap_or(true)
    }
}
