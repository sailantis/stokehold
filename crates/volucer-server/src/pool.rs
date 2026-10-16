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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use volucer_core::config::SpawnCgiConfig;
use volucer_fcgi::{Client, Endpoint, Request, Response};

use crate::error::{Hint, Result, ServerError};
use crate::lifecycle::{configure_command, LifecycleGuard};

/// How long to wait for a slot to start accepting connections.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeout for a single request to a slot.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One `php-cgi` process and the address it serves.
#[derive(Debug)]
struct Slot {
    child: Child,
    /// Loopback TCP or a unix socket, depending on the configuration.
    endpoint: Endpoint,
    /// Requests this slot has served, for `max_requests` recycling.
    served: u64,
    /// A live connection, reused while the backend allows it.
    client: Option<Client>,
}

impl Slot {
    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// The address this slot is bound to, reconstructed so a replacement can bind
    /// the same one.
    ///
    /// Derived from `endpoint` rather than stored separately, so a slot cannot
    /// hold two disagreeing notions of where it listens.
    fn address(&self) -> SlotAddress {
        slot_address_of(&self.endpoint)
    }

    /// Send one request over this slot, reconnecting if the backend closed.
    fn request(&mut self, request: &Request) -> Result<Response> {
        // Reuse the connection only when the previous request said it may be
        // reused; `php-cgi` may close after every request, and assuming
        // otherwise is how a pool ends up writing into a dead socket.
        let endpoint = self.endpoint.clone();
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

fn is_connection_error(err: &volucer_fcgi::FcgiError) -> bool {
    crate::backend::is_transport_error(err)
}

/// The retry predicate, kept named so the intent is visible at the call site.
fn retryable(err: &volucer_fcgi::FcgiError) -> bool {
    is_connection_error(err)
}

/// Where a slot binds: a loopback port, a unix socket path, or a Windows pipe.
#[derive(Debug, Clone)]
enum SlotAddress {
    Port(u16),
    Socket(PathBuf),
    #[cfg(windows)]
    Pipe(PathBuf),
}

impl SlotAddress {
    /// The string `php-cgi -b` is given.
    fn bind_argument(&self, bind_host: &str) -> String {
        match self {
            SlotAddress::Port(port) => format!("{bind_host}:{port}"),
            SlotAddress::Socket(path) => path.display().to_string(),
            #[cfg(windows)]
            SlotAddress::Pipe(name) => name.display().to_string(),
        }
    }

    /// The address the client connects to.
    ///
    /// Fallible rather than infallible because the `Socket` variant is
    /// unrepresentable as an [`Endpoint`] on Windows; config validation rejects
    /// such a configuration before a pool is built, so reaching the error here
    /// means the pool was constructed by hand.
    fn endpoint(&self, bind_host: &str) -> Result<Endpoint> {
        match self {
            SlotAddress::Port(port) => Ok(Endpoint::Tcp(format!("{bind_host}:{port}"))),
            SlotAddress::Socket(path) => Endpoint::parse(&format!("unix:{}", path.display()))
                .map_err(|source| ServerError::Misconfigured {
                    backend: "spawn_cgi".to_string(),
                    reason: format!("{} cannot be a unix socket here: {source}", path.display()),
                }),
            #[cfg(windows)]
            SlotAddress::Pipe(name) => Ok(Endpoint::Pipe(name.display().to_string())),
        }
    }
}

/// Recover the bind address from a live endpoint.
///
/// This is the inverse of [`SlotAddress::endpoint`], and it is what lets recycling
/// and restart bind the *same* place again. A slot stores only its endpoint, so
/// there is no second field that could drift out of agreement with it.
fn slot_address_of(endpoint: &Endpoint) -> SlotAddress {
    match endpoint {
        Endpoint::Tcp(spec) => SlotAddress::Port(
            spec.rsplit_once(':')
                .and_then(|(_, port)| port.parse().ok())
                .unwrap_or_else(|| panic!("a TCP endpoint always carries a port, got {spec:?}")),
        ),
        #[cfg(windows)]
        Endpoint::Pipe(name) => SlotAddress::Pipe(PathBuf::from(name)),
        #[cfg(unix)]
        Endpoint::Unix(path) => SlotAddress::Socket(PathBuf::from(path)),
    }
}

/// A pool of `php-cgi` processes.
#[derive(Debug)]
pub struct CgiPool {
    name: String,
    /// Each slot behind its own lock, so the pool serves as many requests at once
    /// as it has slots. `php-cgi` processes one request at a time, so the lock is
    /// per process rather than per pool.
    slots: Vec<Mutex<Slot>>,
    /// Round-robin position, so work and therefore memory are spread evenly.
    cursor: AtomicUsize,
    binary: PathBuf,
    bind_host: String,
    /// Socket paths this pool bound and therefore must unlink on the way out.
    socket_paths: Vec<PathBuf>,
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

        let guard = LifecycleGuard::new(&format!("volucer-{name}")).map_err(|source| {
            ServerError::Misconfigured {
                backend: name.to_string(),
                reason: format!("could not create the process lifecycle guard: {source}"),
            }
        })?;

        let count = worker_count(config.workers);
        let mut pool = CgiPool {
            name: name.to_string(),
            slots: Vec::with_capacity(count as usize),
            cursor: AtomicUsize::new(0),
            binary: config.binary.clone(),
            bind_host: config.bind_host.clone(),
            socket_paths: Vec::new(),
            max_requests: config.max_requests,
            _guard: guard,
        };

        let mut port = config.base_port;
        for index in 0..count {
            let slot_address = match &config.socket {
                Some(prefix) => {
                    let path = volucer_core::config::cgi_socket_path(prefix, index);
                    #[cfg(windows)]
                    if path.to_str().is_some_and(|p| p.starts_with(r"\\.\pipe\")) {
                        SlotAddress::Pipe(path)
                    } else {
                        SlotAddress::Socket(path)
                    }
                    #[cfg(not(windows))]
                    SlotAddress::Socket(path)
                }
                None => {
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
                    let address = SlotAddress::Port(port);
                    port = port
                        .checked_add(1)
                        .ok_or_else(|| ServerError::Misconfigured {
                            backend: name.to_string(),
                            reason: "ran out of ports while allocating slots".to_string(),
                        })?;
                    address
                }
            };
            let slot = pool.spawn_slot(&slot_address)?;
            if let SlotAddress::Socket(path) = &slot_address {
                pool.socket_paths.push(path.clone());
            }
            pool.slots.push(Mutex::new(slot));
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
    /// `pm.max_requests`. **`0` means never recycle**, as it does there.
    pub fn with_max_requests(mut self, max_requests: Option<u64>) -> Self {
        self.max_requests = max_requests.filter(|max| *max > 0);
        self
    }

    fn spawn_slot(&self, address: &SlotAddress) -> Result<Slot> {
        let mut command = Command::new(&self.binary);
        command
            .arg("-b")
            .arg(address.bind_argument(&self.bind_host))
            // php-cgi's own preforking, which does not compose with this pool.
            //
            // A `php-cgi` started with `-b` is not one process: it is a parent
            // that spawns `PHP_FCGI_CHILDREN` children, all sharing the one bound
            // socket, and the parent's accept loop distributes connections
            // between them. Left at its default that is 20 on a 12-core machine
            // — measured here, and inherited, because php-cgi reads the variable
            // from *its* environment (sapi/cgi/cgi_main.c) and a deployment that
            // sets it for php-fpm hands it to us too.
            //
            // The pool already decides how many PHP processes exist, and each
            // slot is meant to be one process with one port so it can be
            // recycled, counted and reasoned about. 20 hidden children under it
            // make `workers` a lie: the slot looks idle while 20 requests run,
            // `max_requests` never matches what php-fpm would call a request
            // count, killing a slot does not stop the work its children are
            // doing, and every test silently starts 21 processes instead of 1.
            //
            // `0`, not `1`: php-cgi spawns "CHILDREN" *children in addition to
            // itself*, so `1` means two processes per slot. Measured against
            // php-cgi 8.3.31 — `0` gives exactly one process, `1` gives two.
            //
            // `max_requests = 0` is the matching statement on the child side:
            // php-cgi exits a child after 500 requests by default, and its own
            // check is `if (max_requests && requests == max_requests)`, so 0
            // means never. Recycling is the pool's decision here, so that
            // `max_requests` means the same thing for every backend — and that
            // meaning is php-fpm's, where `0` is also "never": see
            // `WorkerConfig::max_requests` for why the pool treats 0 that way
            // rather than recycling after every request.
            .env("PHP_FCGI_CHILDREN", "0")
            .env("PHP_FCGI_MAX_REQUESTS", "0")
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
            endpoint: address.endpoint(&self.bind_host)?,
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
    /// Takes `&self`, so one request per slot runs at a time and a request to a
    /// free slot does not wait for a busy one. Replacing the child process is
    /// `restart`'s job.
    pub fn request(&self, request: &Request) -> Result<Response> {
        if self.slots.is_empty() {
            return Err(ServerError::NoWorker {
                backend: self.name.clone(),
            });
        }

        let index = self.next_slot_index();
        let mut slot = self.slots[index]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let response = slot.request(request)?;

        // Recycle on the request count, mirroring `pm.max_requests`: this is what
        // bounds the effect of any leak in the application under test. `0` means
        // never, as it does for php-fpm — see `WorkerConfig::max_requests`.
        if let Some(max) = self.max_requests.filter(|max| *max > 0) {
            if slot.served >= max {
                let address = slot.address();
                let mut replacement = self.spawn_slot(&address)?;
                replacement.served = 0;
                let old = std::mem::replace(&mut *slot, replacement);
                let mut old = old;
                let _ = old.child.kill();
                let _ = old.child.wait();
            }
        }
        Ok(response)
    }

    /// Pick a slot, preferring a live one.
    ///
    /// A slot that died is restarted in place so the pool keeps its size; the
    /// alternative is a pool that silently shrinks under repeated crashes.
    fn next_slot_index(&self) -> usize {
        let count = self.slots.len();
        let base = self.cursor.fetch_add(1, Ordering::Relaxed);
        for offset in 0..count {
            let index = (base + offset) % count;
            let mut slot = self.slots[index]
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if slot.is_alive() {
                return index;
            }
            // Give up on restart here rather than in a dispatch path: if this
            // fails the slot is replaced lazily on the next tick.
            tracing::warn!(
                backend = %self.name,
                address = %slot.endpoint,
                "a php-cgi slot died; restarting"
            );
            if let Err(err) = self.replace_locked(&mut slot) {
                tracing::warn!(backend = %self.name, %err, "could not restart a slot");
                continue;
            }
            return index;
        }
        // Every slot is gone; the first one will be replaced on the next request.
        0
    }

    /// Replace the process in a slot, keeping its address.
    pub fn restart(&self, index: usize) -> Result<()> {
        let mut slot = self.slots[index]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.replace_locked(&mut slot)
    }

    /// Replace a locked slot's process with a fresh one on the same address.
    ///
    /// The replacement is booted before the old process is killed, so a slot that
    /// cannot be restarted keeps working rather than being left dead.
    fn replace_locked(&self, slot: &mut Slot) -> Result<()> {
        let address = slot.address();
        let mut replacement = self.spawn_slot(&address)?;
        replacement.served = 0;
        let mut old = std::mem::replace(slot, replacement);
        let _ = old.child.kill();
        let _ = old.child.wait();
        Ok(())
    }

    /// Stop every slot, waiting for it to exit.
    ///
    /// `&mut self` because stopping means emptying the pool, and only the owner
    /// may do that. Nothing on the request path needs it: the server drops the
    /// pool, and `Drop` calls this.
    pub fn shutdown(&mut self) {
        for slot in &mut self.slots {
            // `get_mut`: the pool is exclusively borrowed, so there is nothing to
            // synchronise against.
            let slot = slot
                .get_mut()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let _ = slot.child.kill();
            let _ = slot.child.wait();
        }
        self.slots.clear();
        // `php-cgi` unlinks a stale path before binding, so a leftover socket is
        // not fatal — but a stopped pool should not leave litter behind, and on
        // Linux an unlinked file is what releases the name for the next run.
        for path in self.socket_paths.drain(..) {
            if let Err(err) = std::fs::remove_file(&path) {
                if err.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        backend = %self.name,
                        path = %path.display(),
                        %err,
                        "could not remove a unix socket"
                    );
                }
            }
        }
    }

    /// The ports in use, for diagnostics. Unix-socket and pipe slots are not
    /// represented: they hold no port.
    pub fn ports(&self) -> Vec<u16> {
        self.locked_slots()
            .iter()
            .filter_map(|slot| match &slot.endpoint {
                Endpoint::Tcp(spec) => spec
                    .rsplit_once(':')
                    .and_then(|(_, port)| port.parse::<u16>().ok()),
                #[cfg(windows)]
                Endpoint::Pipe(_) => None,
                #[cfg(unix)]
                Endpoint::Unix(_) => None,
            })
            .collect()
    }

    /// Every slot's address, in slot order, for diagnostics and tests.
    pub fn endpoints(&self) -> Vec<String> {
        self.locked_slots()
            .iter()
            .map(|slot| slot.endpoint.to_string())
            .collect()
    }

    /// The unix socket paths this pool bound.
    pub fn socket_paths(&self) -> &[PathBuf] {
        &self.socket_paths
    }

    /// The process ids of the slots, for diagnostics and tests.
    pub fn pids(&self) -> Vec<u32> {
        self.locked_slots()
            .iter()
            .map(|slot| slot.child.id())
            .collect()
    }

    /// Every slot, locked in turn, for a read that does not span a request.
    fn locked_slots(&self) -> Vec<std::sync::MutexGuard<'_, Slot>> {
        self.slots
            .iter()
            .map(|slot| slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
            .collect()
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
///
/// Connects through the slot's own endpoint, so this works the same whether the
/// slot bound a port or a unix socket — and it is the only startup probe that
/// proves the *client* can reach the address, rather than that something is
/// listening somewhere.
fn wait_until_listening(slot: &mut Slot, binary: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match slot.child.try_wait() {
            Ok(Some(status)) => {
                return Err(ServerError::StartupExited {
                    binary: binary.to_path_buf(),
                    status: status.to_string(),
                    hint: Hint::default(),
                })
            }
            Ok(None) => {}
            Err(source) => return Err(ServerError::Io(source)),
        }
        if slot
            .endpoint
            .connect(Some(Duration::from_millis(250)))
            .is_ok()
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(ServerError::StartupTimeout {
        binary: binary.to_path_buf(),
        address: slot.endpoint.to_string(),
        timeout,
        hint: Hint::default(),
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
    fn a_slot_address_knows_how_to_bind_and_how_to_connect() {
        let port = SlotAddress::Port(19_001);
        assert_eq!(port.bind_argument("127.0.0.1"), "127.0.0.1:19001");
        assert_eq!(
            port.endpoint("127.0.0.1")
                .expect("a port is always an endpoint"),
            Endpoint::Tcp("127.0.0.1:19001".to_string())
        );

        let socket = SlotAddress::Socket(PathBuf::from("/run/volucer/cgi.0.sock"));
        assert_eq!(
            socket.bind_argument("127.0.0.1"),
            "/run/volucer/cgi.0.sock",
            "php-cgi takes a bare path for a unix socket, not the `unix:` prefix"
        );
    }

    /// Where unix sockets exist they resolve to a `Unix` endpoint; where they do
    /// not, the same input is an error rather than a silent fallback to TCP.
    #[cfg(unix)]
    #[test]
    fn a_unix_slot_address_resolves_to_a_unix_endpoint() {
        let socket = SlotAddress::Socket(PathBuf::from("/run/volucer/cgi.0.sock"));
        assert_eq!(
            socket
                .endpoint("127.0.0.1")
                .expect("unix sockets exist here"),
            Endpoint::Unix("/run/volucer/cgi.0.sock".to_string())
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn a_unix_slot_address_is_refused_where_the_platform_has_none() {
        let socket = SlotAddress::Socket(PathBuf::from("/run/volucer/cgi.0.sock"));
        let err = socket
            .endpoint("127.0.0.1")
            .expect_err("a unix socket cannot be reached here");
        assert!(err.to_string().contains("cannot be a unix socket"), "{err}");
    }

    /// A Windows pipe slot hands `php-cgi` the bare pipe name and connects to it.
    #[cfg(windows)]
    #[test]
    fn a_pipe_slot_address_binds_and_connects_by_name() {
        let pipe = SlotAddress::Pipe(PathBuf::from(r"\\.\pipe\volucer-cgi.0"));
        assert_eq!(
            pipe.bind_argument("127.0.0.1"),
            r"\\.\pipe\volucer-cgi.0",
            "php-cgi takes the pipe name verbatim, not `host:port`"
        );
        assert_eq!(
            pipe.endpoint("127.0.0.1")
                .expect("a pipe is always an endpoint"),
            Endpoint::Pipe(r"\\.\pipe\volucer-cgi.0".to_string())
        );
        // And the round trip has to come back to the same place, or a recycled slot
        // would bind a differently named pipe.
        assert!(matches!(
            slot_address_of(&pipe.endpoint("127.0.0.1").unwrap()),
            SlotAddress::Pipe(_)
        ));
    }

    #[test]
    fn a_slot_address_round_trips_through_an_endpoint() {
        // `slot_address_of` recovers the bind address from a live endpoint, which
        // is how recycling and restart bind the same place again. If that
        // reconstruction drifts, a recycled slot silently moves.
        let port = SlotAddress::Port(19_444);
        let endpoint = port
            .endpoint("127.0.0.1")
            .expect("a port is always reachable");
        match slot_address_of(&endpoint) {
            SlotAddress::Port(recovered) => assert_eq!(recovered, 19_444),
            SlotAddress::Socket(path) => panic!("expected a port, got {path:?}"),
            #[cfg(windows)]
            SlotAddress::Pipe(name) => panic!("expected a port, got pipe {name:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_unix_slot_address_round_trips_through_an_endpoint() {
        let socket = SlotAddress::Socket(PathBuf::from("/run/volucer/cgi.3.sock"));
        let endpoint = socket
            .endpoint("127.0.0.1")
            .expect("unix sockets exist here");
        match slot_address_of(&endpoint) {
            SlotAddress::Socket(recovered) => {
                assert_eq!(recovered, PathBuf::from("/run/volucer/cgi.3.sock"));
            }
            SlotAddress::Port(port) => panic!("expected a socket, got port {port}"),
            #[cfg(windows)]
            SlotAddress::Pipe(name) => panic!("expected a socket, got pipe {name:?}"),
        }
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
            socket: None,
            max_requests: None,
        };
        let err = CgiPool::start("test", &config).unwrap_err();
        assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn connection_errors_are_recognised_for_retry() {
        use volucer_fcgi::FcgiError;
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
            status: volucer_fcgi::ProtocolStatus::Overloaded
        }));
    }
}
