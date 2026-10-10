//! Shared helpers for the integration tests.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Serialise tests that need loopback ports.
///
/// The pool tests contend for a genuinely global resource: free ports are
/// released by the helper before the pool binds them, so two tests running
/// concurrently can be handed the same range. Serialising is the honest fix,
/// rather than hoping the race does not happen.
pub fn serial() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Find `php-cgi` on `PATH`.
pub fn find_php_cgi() -> Option<PathBuf> {
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

/// Find a run of `count` consecutive unused ports, returning the first.
///
/// Consecutive matters: the pool starts at `base_port` and increments, so
/// scattered free ports would not all be used. Every port in the run is held
/// while checking, so the run cannot be picked twice.
pub fn free_port_run(count: usize) -> Option<u16> {
    for _ in 0..64 {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
        let base = probe.local_addr().expect("bound address").port();
        drop(probe);

        let mut held = Vec::with_capacity(count);
        for offset in 0..count {
            let port = base.checked_add(offset as u16)?;
            match std::net::TcpListener::bind(("127.0.0.1", port)) {
                Ok(listener) => held.push(listener),
                Err(_) => break,
            }
        }
        if held.len() == count {
            drop(held);
            return Some(base);
        }
    }
    None
}

/// Write a PHP script into `root`, creating the file.
pub fn write_script(root: &Path, name: &str, body: &str) -> PathBuf {
    let path = root.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create the script directory");
    }
    std::fs::write(&path, body).expect("write the script");
    path
}
