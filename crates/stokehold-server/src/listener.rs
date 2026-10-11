//! The listener loop: accept connections, serve requests, dispatch to backends.
//!
//! Deliberately thread-per-connection on blocking sockets rather than an async
//! runtime. The reason is the backend, not the front-end: `php-cgi` processes
//! requests sequentially and a slot serves one request at a time, so the
//! concurrency ceiling is the number of slots. Threads map onto that directly,
//! keep the code readable, and avoid an executor in a program whose real work is
//! process supervision.
//!
//! The dispatcher is behind a mutex for the same reason: serialising dispatch is
//! not a compromise, it is what the backends already require. When the worker
//! pool lands and a slot can truly serve concurrently, this is the single place
//! that has to change.

use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::dispatch::Dispatcher;
use crate::error::{Result, ServerError};
use crate::frontend::fastcgi::{self, ServerErrorKind};

/// How long an idle keep-alive connection may sit before it is closed.
///
/// Without this a keep-alive connection from nginx would occupy a thread for
/// ever, and enough of them would exhaust the process's threads.
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(15);

/// Limits on concurrent connections, so a misbehaving client cannot make the
/// process allocate unboundedly many threads.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_connections: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_connections: 512,
        }
    }
}

/// What the listener reports about its own behaviour.
#[derive(Debug, Default)]
pub struct Stats {
    pub connections: AtomicUsize,
    pub requests: AtomicUsize,
    pub rejected: AtomicUsize,
    pub failed: AtomicUsize,
}

impl Stats {
    fn snapshot(&self) -> (usize, usize, usize, usize) {
        (
            self.connections.load(Ordering::Relaxed),
            self.requests.load(Ordering::Relaxed),
            self.rejected.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
        )
    }
}

/// A running FastCGI listener.
pub struct FastCgiListener {
    listener: TcpListener,
    dispatcher: Arc<Mutex<Dispatcher>>,
    shutdown: Arc<AtomicBool>,
    stats: Arc<Stats>,
    limits: Limits,
}

impl FastCgiListener {
    /// Bind a listener.
    ///
    /// Binding happens here rather than inside `run`, so a port conflict is
    /// reported before any backend is started.
    ///
    /// The dispatcher arrives behind an `Arc<Mutex<_>>` because it is shared: the
    /// HTTP front-end serves the same backends, so there is one set of processes
    /// behind one routing decision, whichever protocol a request arrived on.
    pub fn bind(address: &str, dispatcher: Arc<Mutex<Dispatcher>>, limits: Limits) -> Result<Self> {
        let listener = TcpListener::bind(address).map_err(|source| ServerError::Misconfigured {
            backend: "<listener>".to_string(),
            reason: format!("cannot bind {address}: {source}"),
        })?;
        listener.set_nonblocking(false).map_err(ServerError::Io)?;

        Ok(FastCgiListener {
            listener,
            dispatcher,
            shutdown: Arc::new(AtomicBool::new(false)),
            stats: Arc::new(Stats::default()),
            limits,
        })
    }

    /// The address actually bound, which matters when port 0 was requested.
    pub fn local_addr(&self) -> Result<std::net::SocketAddr> {
        self.listener.local_addr().map_err(ServerError::Io)
    }

    /// A handle that stops the listener from another thread.
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            flag: Arc::clone(&self.shutdown),
        }
    }

    /// Statistics, for logging.
    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
    }

    /// Accept and serve connections until [`ShutdownHandle::shutdown`] is called.
    pub fn run(self) -> Result<()> {
        // A read timeout on the listener would break accept; instead the flag is
        // checked between accepts, and the loop uses a short poll interval.
        self.listener
            .set_nonblocking(true)
            .map_err(ServerError::Io)?;

        let mut handlers = Vec::new();
        while !self.shutdown.load(Ordering::Relaxed) {
            match self.listener.accept() {
                Ok((stream, peer)) => {
                    let active = self.stats.connections.fetch_add(1, Ordering::Relaxed) + 1;
                    if active > self.limits.max_connections {
                        self.stats.rejected.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(%peer, active, "refusing a connection: at the limit");
                        drop(stream);
                        continue;
                    }

                    let dispatcher = Arc::clone(&self.dispatcher);
                    let stats = Arc::clone(&self.stats);
                    let shutdown = Arc::clone(&self.shutdown);
                    let max_connections = self.limits.max_connections;
                    handlers.retain(|handle: &std::thread::JoinHandle<()>| !handle.is_finished());
                    handlers.push(std::thread::spawn(move || {
                        if let Err(err) =
                            serve_connection(stream, dispatcher, stats, shutdown, max_connections)
                        {
                            tracing::debug!(%peer, %err, "connection ended");
                        }
                    }));
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(source) => {
                    // A transient accept failure should not kill the listener.
                    tracing::warn!(%source, "accept failed");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }

        tracing::info!("listener stopping");
        for handle in handlers {
            let _ = handle.join();
        }
        let (connections, requests, rejected, failed) = self.stats.snapshot();
        tracing::info!(connections, requests, rejected, failed, "listener stopped");
        Ok(())
    }
}

/// Stops a listener from another thread.
#[derive(Clone)]
pub struct ShutdownHandle {
    flag: Arc<AtomicBool>,
}

impl ShutdownHandle {
    pub fn shutdown(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    pub fn is_shutting_down(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

/// Serve requests on one connection until the peer closes or keep-alive ends.
fn serve_connection(
    mut stream: std::net::TcpStream,
    dispatcher: Arc<Mutex<Dispatcher>>,
    stats: Arc<Stats>,
    shutdown: Arc<AtomicBool>,
    max_connections: usize,
) -> Result<()> {
    // On Windows an accepted socket inherits the listener's non-blocking mode,
    // so this must be reset explicitly. Without it every read returns
    // WSAEWOULDBLOCK immediately and the connection is dropped right after the
    // first record — which is exactly what happened before this line existed.
    stream.set_nonblocking(false).map_err(ServerError::Io)?;

    // The keep-alive timeout is what stops an idle connection holding a thread.
    stream
        .set_read_timeout(Some(KEEPALIVE_TIMEOUT))
        .map_err(ServerError::Io)?;
    let _ = stream.set_nodelay(true);

    let max_connections_string = max_connections.to_string();
    let mut requests_string = String::new();

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }

        let next = match fastcgi::read_next(&mut stream) {
            Ok(Some(next)) => next,
            // Clean end of stream: the peer closed between requests.
            Ok(None) => return Ok(()),
            Err(err) => return Err(err),
        };

        // A `GET_VALUES` probe is not a request: it must be answered with a
        // result record, never an END_REQUEST. nginx uses it to decide whether
        // this server is usable.
        let incoming = match next {
            fastcgi::NextRequest::GetValues(names) => {
                tracing::debug!(count = names.len(), "answering a GET_VALUES probe");
                let values = [
                    ("max_connections", max_connections_string.as_str()),
                    ("requests", requests_string.as_str()),
                ];
                fastcgi::write_get_values(&mut stream, &values)?;
                continue;
            }
            fastcgi::NextRequest::Request(incoming) => incoming,
        };

        if incoming.aborted {
            // The client gave up before we produced anything, so there is
            // nothing to write.
            tracing::debug!("a request was aborted before it was served");
            continue;
        }

        let keep_conn = incoming.keep_conn;
        let request_id = stokehold_fcgi::REQUEST_ID;

        let response = {
            let mut dispatcher = dispatcher
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            dispatcher.dispatch_to_default(&incoming.into_backend_request())
        };

        match response {
            Ok(response) => {
                stats.requests.fetch_add(1, Ordering::Relaxed);
                requests_string = stats.requests.load(Ordering::Relaxed).to_string();
                if !response.stderr.is_empty() {
                    // nginx puts this in its error log, which is where a
                    // developer looks first when PHP complains.
                    tracing::debug!(
                        stderr = %String::from_utf8_lossy(&response.stderr).trim_end(),
                        "backend wrote to STDERR"
                    );
                }
                fastcgi::write_response(
                    &mut stream,
                    request_id,
                    &response.stdout,
                    &response.stderr,
                    keep_conn,
                )?;
            }
            Err(err) => {
                stats.failed.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(%err, "dispatch failed");
                fastcgi::write_error(
                    &mut stream,
                    request_id,
                    &err.to_string(),
                    ServerErrorKind::Backend,
                )?;
            }
        }

        if !keep_conn {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_have_a_sane_default() {
        assert!(Limits::default().max_connections > 0);
    }

    #[test]
    fn a_shutdown_handle_stops_the_listener() {
        let flag = Arc::new(AtomicBool::new(false));
        let handle = ShutdownHandle {
            flag: Arc::clone(&flag),
        };
        assert!(!handle.is_shutting_down());
        handle.shutdown();
        assert!(handle.is_shutting_down());
        assert!(flag.load(Ordering::Relaxed));
    }

    #[test]
    fn binding_a_used_port_is_a_misconfiguration_not_a_panic() {
        let held = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = held.local_addr().expect("address").to_string();

        let config = stokehold_core::config::Config {
            server: stokehold_core::config::ServerConfig {
                http: None,
                fastcgi: Some(address.clone()),
                static_files: Default::default(),
            },
            // Nothing here has to start: the dispatcher is built but never used,
            // because binding fails first. An `external` backend pointing at a
            // port nothing listens on costs nothing and starts no process.
            backends: [(
                "fpm".to_string(),
                stokehold_core::config::BackendConfig::External(
                    stokehold_core::config::ExternalConfig {
                        socket: "127.0.0.1:1".to_string(),
                    },
                ),
            )]
            .into_iter()
            .collect(),
            default_backend: Some("fpm".to_string()),
            routes: Vec::new(),
        };
        let dispatcher = Dispatcher::from_config(&config, stokehold_fcgi::Routing::default())
            .expect("dispatcher");

        let err = FastCgiListener::bind(
            &address,
            std::sync::Arc::new(Mutex::new(dispatcher)),
            Limits::default(),
        )
        .err()
        .unwrap_or_else(|| panic!("binding {address} should have failed"));
        assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
        assert!(err.to_string().contains("cannot bind"), "{err}");
    }

    #[test]
    fn stats_start_at_zero() {
        let stats = Stats::default();
        assert_eq!(stats.snapshot(), (0, 0, 0, 0));
    }
}
