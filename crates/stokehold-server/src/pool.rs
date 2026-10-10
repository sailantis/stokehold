//! A pool of `php-cgi` processes, one per port.
//!
//! This is the `spawn_cgi` backend, and on Windows it is the only way to get
//! per-request SAPI reset at all. The shape is forced by the platform
//! (`docs/plan.md` §9):
//!
//! - `PHP_FCGI_CHILDREN` relies on `fork()`, so `php-cgi` cannot spawn its own
//!   children on Windows. One process per slot is therefore mandatory.
//! - Several processes cannot share one port via `SO_REUSEADDR` on Windows, so
//!   each slot needs its own port.
//! - FastCGI request ids would allow multiplexing, but `php-cgi` processes
//!   sequentially, so multiplexing would add a demux table and buy nothing.
//!
//! The pool owns the processes, assigns the ports, health-checks by connecting,
//! restarts a slot that died, and hands out a connected client per request.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use stokehold_core::config::SpawnCgiConfig;
use stokehold_fcgi::{Client, Endpoint, Request, Response};

use crate::error::{Result, ServerError};
use crate::lifecycle::{configure_command, LifecycleGuard};

/// How long to wait for a slot to start accepting connections.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeout for a single request to a slot.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One `php-cgi` process and the port it serves.
#[derive(Debug)]
struct Slot {
    child: Child,
    port: u16,
    /// Requests this slot has served, for `max_requests` recycling.
    served: u64,
    /// A live connection, reused while the backend allows it.
    client: Option<Client>,
}

impl Slot {
    fn endpoint(&self) -> Endpoint {
        Endpoint::Tcp(format!("127.0.0.1:{}", self.port))
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Send one request over this slot, reconnecting if the backend closed.
    fn request(&mut self, request: &Request) -> Result<Response> {
        // Reuse the connection only when the previous request said it may be
        // reused; `php-cgi` may close after every request, and assuming
        // otherwise is how a pool ends up writing into a dead socket.
        let endpoint = self.endpoint();
        if self.client.as_ref().is_some_and(|c| !c.is_reusable()) {
            self.client = None;
        }
        if self.client.is_none() {
            self.client = Some(Client::connect(&endpoint, Some(REQUEST_TIMEOUT)).map_err(
                |source| ServerError::Worker {
                    backend: "spawn_cgi".to_string(),
                    source,
                },
            )?);
        }

        let client = self.client.as_mut().expect("client just connected");
        let mut attempt = 0;
        loop {
            match client.request(request) {
                Ok(response) => {
                    self.served += 1;
                    return Ok(response);
                }
                // One retry on a transport error: a slot may have recycled
                // between requests without the port changing, and php-cgi closes
                // the connection even when keep-alive was requested.
                Err(err) if attempt == 0 && retryable(&err) => {
                    attempt += 1;
                    *client =
                        Client::connect(&endpoint, Some(REQUEST_TIMEOUT)).map_err(|source| {
                            ServerError::Worker {
                                backend: "spawn_cgi".to_string(),
                                source,
                            }
                        })?;
                }
                Err(source) => {
                    return Err(ServerError::Worker {
                        backend: "spawn_cgi".to_string(),
                        source,
                    })
                }
            }
        }
    }
}

fn is_connection_error(err: &stokehold_fcgi::FcgiError) -> bool {
    crate::backend::is_transport_error(err)
}

/// The retry predicate, kept named so the intent is visible at the call site.
fn retryable(err: &stokehold_fcgi::FcgiError) -> bool {
    is_connection_error(err)
}

/// A pool of `php-cgi` processes.
#[derive(Debug)]
pub struct CgiPool {
    name: String,
    slots: Vec<Slot>,
    binary: PathBuf,
    bind_host: String,
    max_requests: Option<u64>,
    /// Held for the life of the pool; dropping it reaps every slot.
    _guard: LifecycleGuard,
}

impl CgiPool {
    /// Start a pool from configuration.
    pub fn start(name: &str, config: &SpawnCgiConfig) -> Result<Self> {
        if !config.binary.is_file() {
            return Err(ServerError::Misconfigured {
                backend: name.to_string(),
                reason: format!("{} does not exist", config.binary.display()),
            });
        }

        let guard = LifecycleGuard::new(&format!("stokehold-{name}")).map_err(|source| {
            ServerError::Misconfigured {
                backend: name.to_string(),
                reason: format!("could not create the process lifecycle guard: {source}"),
            }
        })?;

        let count = worker_count(config.workers);
        let mut pool = CgiPool {
            name: name.to_string(),
            slots: Vec::with_capacity(count as usize),
            binary: config.binary.clone(),
            bind_host: config.bind_host.clone(),
            max_requests: config.max_requests,
            _guard: guard,
        };

        let mut port = config.base_port;
        for _ in 0..count {
            // Skip ports that are already taken rather than failing the whole
            // pool: another service on 19001 should not stop startup.
            while !port_is_free(&pool.bind_host, port) {
                port = port
                    .checked_add(1)
                    .ok_or_else(|| ServerError::Misconfigured {
                        backend: name.to_string(),
                        reason: "ran out of ports while allocating slots".to_string(),
                    })?;
            }
            let slot = pool.spawn_slot(port)?;
            pool.slots.push(slot);
            port = port
                .checked_add(1)
                .ok_or_else(|| ServerError::Misconfigured {
                    backend: name.to_string(),
                    reason: "ran out of ports while allocating slots".to_string(),
                })?;
        }

        tracing::info!(
            backend = %pool.name,
            slots = pool.slots.len(),
            binary = %pool.binary.display(),
            "php-cgi pool started"
        );
        Ok(pool)
    }

    /// Recycle a slot after this many requests, the equivalent of php-fpm's
    /// `pm.max_requests`.
    pub fn with_max_requests(mut self, max_requests: Option<u64>) -> Self {
        self.max_requests = max_requests;
        self
    }

    fn spawn_slot(&self, port: u16) -> Result<Slot> {
        let mut command = Command::new(&self.binary);
        command
            .arg("-b")
            .arg(format!("{}:{port}", self.bind_host))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_command(&mut command);

        let child = command.spawn().map_err(|source| ServerError::Spawn {
            binary: self.binary.clone(),
            source,
        })?;

        // Adopt before waiting, so a failure here cannot leave an orphan.
        self._guard
            .adopt(&child)
            .map_err(|source| ServerError::Spawn {
                binary: self.binary.clone(),
                source,
            })?;

        let mut slot = Slot {
            child,
            port,
            served: 0,
            client: None,
        };
        wait_until_listening(&mut slot, &self.binary, STARTUP_TIMEOUT)?;
        Ok(slot)
    }

    /// Number of live slots.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Send a request through the pool.
    ///
    /// Slots are used in order and each one handles one request at a time, which
    /// is exactly what `php-cgi` supports. Replacing the child process is
    /// `restart`'s job.
    pub fn request(&mut self, request: &Request) -> Result<Response> {
        if self.slots.is_empty() {
            return Err(ServerError::NoWorker {
                backend: self.name.clone(),
            });
        }

        let index = self.next_slot_index();
        let response = self.slots[index].request(request)?;

        // Recycle on the request count, mirroring `pm.max_requests`: this is what
        // bounds the effect of any leak in the application under test.
        if let Some(max) = self.max_requests {
            if self.slots[index].served >= max {
                self.restart(index)?;
            }
        }
        Ok(response)
    }

    /// Pick a slot, preferring a live one.
    ///
    /// A slot that died is restarted in place so the pool keeps its size; the
    /// alternative is a pool that silently shrinks under repeated crashes.
    fn next_slot_index(&mut self) -> usize {
        for index in 0..self.slots.len() {
            if self.slots[index].is_alive() {
                return index;
            }
            // Give up on restart here rather than in a dispatch path: if this
            // fails the slot is replaced lazily on the next tick.
            tracing::warn!(
                backend = %self.name,
                port = self.slots[index].port,
                "a php-cgi slot died; restarting"
            );
            if self.restart(index).is_err() {
                break;
            }
        }
        0
    }

    /// Replace the process in a slot, keeping its port.
    pub fn restart(&mut self, index: usize) -> Result<()> {
        let port = self.slots[index].port;
        let mut replacement = self.spawn_slot(port)?;
        replacement.served = 0;
        let old = std::mem::replace(&mut self.slots[index], replacement);
        let mut old = old;
        let _ = old.child.kill();
        let _ = old.child.wait();
        Ok(())
    }

    /// Stop every slot, waiting for it to exit.
    pub fn shutdown(&mut self) {
        for slot in &mut self.slots {
            let _ = slot.child.kill();
            let _ = slot.child.wait();
        }
        self.slots.clear();
    }

    /// The ports in use, for diagnostics.
    pub fn ports(&self) -> Vec<u16> {
        self.slots.iter().map(|slot| slot.port).collect()
    }

    /// The process ids of the slots, for diagnostics and tests.
    pub fn pids(&self) -> Vec<u32> {
        self.slots.iter().map(|slot| slot.child.id()).collect()
    }
}

impl Drop for CgiPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Turn a configured worker count into an actual count.
///
/// `0` is documented as "one per logical CPU", matching the example
/// configuration and php-fpm's own `pm.start_servers` convention.
///
/// Shared with the worker pool, so both pool kinds interpret `workers = 0` the
/// same way.
pub(crate) fn worker_count(configured: u32) -> u32 {
    if configured > 0 {
        return configured;
    }
    std::thread::available_parallelism()
        .map(|count| count.get() as u32)
        .unwrap_or(1)
}

/// Whether a port can be bound right now.
fn port_is_free(host: &str, port: u16) -> bool {
    TcpListener::bind((host, port)).is_ok()
}

/// Wait for a slot to accept connections.
fn wait_until_listening(slot: &mut Slot, binary: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match slot.child.try_wait() {
            Ok(Some(status)) => {
                return Err(ServerError::StartupExited {
                    binary: binary.to_path_buf(),
                    status: status.to_string(),
                })
            }
            Ok(None) => {}
            Err(source) => return Err(ServerError::Io(source)),
        }
        if std::net::TcpStream::connect(("127.0.0.1", slot.port)).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(ServerError::StartupTimeout {
        binary: binary.to_path_buf(),
        address: format!("127.0.0.1:{}", slot.port),
        timeout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_workers_means_the_available_parallelism() {
        assert!(worker_count(0) >= 1);
        assert_eq!(worker_count(4), 4);
        assert_eq!(worker_count(1), 1);
    }

    #[test]
    fn port_is_free_reports_the_truth() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        let port = listener.local_addr().unwrap().port();
        assert!(!port_is_free("127.0.0.1", port), "the port is held");
        drop(listener);
        assert!(port_is_free("127.0.0.1", port), "the port was released");
    }

    #[test]
    fn a_missing_binary_is_a_misconfiguration_not_a_panic() {
        let config = SpawnCgiConfig {
            binary: PathBuf::from("definitely-not-a-php-cgi"),
            workers: 1,
            bind_host: "127.0.0.1".to_string(),
            base_port: 19_700,
            max_requests: None,
        };
        let err = CgiPool::start("test", &config).unwrap_err();
        assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn connection_errors_are_recognised_for_retry() {
        use stokehold_fcgi::FcgiError;
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::UnexpectedEof,
        ] {
            assert!(is_connection_error(&FcgiError::Io(std::io::Error::new(
                kind, "test"
            ))));
        }
        assert!(is_connection_error(&FcgiError::IncompleteResponse {
            request_id: 1,
            complete: true
        }));
        // A protocol-level refusal is not a connection problem.
        assert!(!is_connection_error(&FcgiError::BackendProtocolStatus {
            status: stokehold_fcgi::ProtocolStatus::Overloaded
        }));
    }
}
