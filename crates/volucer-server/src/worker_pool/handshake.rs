//! Starting a worker and getting it ready: `spawn`, the `HELLO` / `HELLO_ACK`
//! handshake under a deadline, the socket-transport accept poll, and the `stderr`
//! drain thread. Everything that waits here has a deadline, and the deadline is
//! enforced by killing the worker — see the note in [`super`].

use super::exchange::{Channel, ChannelExchange};
use super::spec::Spec;
use crate::error::{Hint, Result, ServerError, SOCKET_WORKER_HINT};
use crate::lifecycle::{configure_command, LifecycleGuard};
use std::io::{BufRead, BufReader, ErrorKind};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};
use volucer_core::config::WORKER_SOCKET_ENV;
use volucer_worker::{BoundedChannel, Checksum, HelloAck, WorkerError};

/// How long a worker gets to complete the `HELLO` / `HELLO_ACK` handshake.
///
/// Generous, because it covers loading the application: a framework boot costs
/// real time, and a worker that boots once should not be killed for taking a
/// while at the one moment where being slow is expected.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// How often a socket-transport pool looks for a worker that has not connected yet.
///
/// The connection is made by a process this server has just spawned, so it arrives within a
/// millisecond of being made and the poll interval is the whole cost of finding out. It is
/// short for that reason: this delay is paid once per worker at startup, not per request.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// A worker that has been started and has completed its handshake.
#[derive(Debug)]
pub(super) struct Ready {
    pub(super) child: Child,
    pub(super) exchange: ChannelExchange,
    pub(super) ack: HelloAck,
}

/// Wait for a socket-transport worker to connect back, and give up rather than hang.
///
/// A blocking `accept` here has no deadline, and the failure it waits for is a quiet one: a
/// worker that starts but connects to the wrong address, or does not connect at all, produces
/// no error anywhere — the accept simply never returns. That turns a misconfiguration into a
/// server that hangs at startup instead of one that says what is wrong, which is the same
/// failure the named-pipe transport had to solve for reads (plan hazard 43).
///
/// The listener is made non-blocking and polled, rather than blocking in `accept` with the
/// deadline enforced elsewhere: there is no portable `accept` timeout, and this runs once
/// per worker at startup, so the 2 ms granularity is not on any request path. The accepted
/// stream is put back to blocking, which is what the exchange thread wants.
fn accept_with_deadline(
    listener: &TcpListener,
    spec: &Spec,
    child: &mut Child,
    timeout: Duration,
) -> Result<TcpStream> {
    let deadline = Instant::now() + timeout;
    listener
        .set_nonblocking(true)
        .map_err(|source| ServerError::Spawn {
            binary: spec.binary.clone(),
            source,
        })?;

    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                // Restore blocking before the stream is handed to the exchange thread.
                let _ = stream.set_nonblocking(false);
                return Ok(stream);
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                // A worker that has already exited will never connect. Saying so is far more
                // useful than waiting out the full handshake budget, and the exit status is
                // usually the reason: a fatal error, a missing extension, a bad path.
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(ServerError::StartupExited {
                        binary: spec.binary.clone(),
                        status: status.to_string(),
                        hint: Hint(Some(SOCKET_WORKER_HINT)),
                    });
                }
                if Instant::now() >= deadline {
                    return Err(ServerError::StartupTimeout {
                        binary: spec.binary.clone(),
                        address: listener
                            .local_addr()
                            .map(|found| found.to_string())
                            .unwrap_or_else(|_| "the worker socket".to_string()),
                        timeout,
                        hint: Hint(Some(SOCKET_WORKER_HINT)),
                    });
                }
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(source) => {
                return Err(ServerError::Spawn {
                    binary: spec.binary.clone(),
                    source,
                })
            }
        }
    }
}

/// The hint for a startup failure, given how the worker was going to be reached.
///
/// A pipe-transport worker that exits during its handshake is an ordinary application failure
/// — a syntax error, a missing extension, a bad path — and `StartupExited` already says
/// everything useful about it. A socket-transport worker has one far more likely explanation
/// that the status alone cannot express, which is the whole reason the hint exists.
fn transport_hint(spec: &Spec) -> Option<&'static str> {
    if spec.transport.is_pipe() {
        None
    } else {
        Some(SOCKET_WORKER_HINT)
    }
}

/// Start a worker and complete its handshake.
///
/// Everything that can go wrong with a worker does so here, which is the point:
/// a worker that cannot boot is a startup error, not a mystery on the first
/// request that happens to reach it.
pub(super) fn boot(name: &str, spec: &Spec, guard: &LifecycleGuard) -> Result<Ready> {
    // A socket-transport worker is connected to a listener that has to exist *before* the
    // process starts, because the address is handed to it as an environment variable
    // rather than discovered by it. So the listener is bound first and the worker spawns
    // into a connection that is already waiting.
    let listener = if spec.transport.is_pipe() {
        None
    } else {
        Some(
            TcpListener::bind(("127.0.0.1", 0)).map_err(|source| ServerError::Spawn {
                binary: spec.binary.clone(),
                source,
            })?,
        )
    };
    let socket_address = match &listener {
        Some(listener) => Some(listener.local_addr().map_err(|source| ServerError::Spawn {
            binary: spec.binary.clone(),
            source,
        })?),
        None => None,
    };

    let mut command = Command::new(&spec.binary);
    command
        .stdin(if spec.transport.is_pipe() {
            Stdio::piped()
        } else {
            // With the protocol on a socket, stdio is free to be nothing. Leaving stdin
            // piped would also work, but a worker that reads the wrong stream should fail
            // loudly rather than block, and `null` gives it immediate end-of-file.
            Stdio::null()
        })
        .stdout(if spec.transport.is_pipe() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        // Piped, never nulled: PHP writes its diagnostics here, and a worker
        // whose stderr pipe fills up blocks for ever on its next write.
        .stderr(Stdio::piped());
    command.args(&spec.args);
    if let Some(dir) = &spec.working_dir {
        command.current_dir(dir);
    }
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    if let Some(address) = socket_address {
        // Set *after* the configured `env`, so the address always wins. A configuration
        // that carries its own `env` map would otherwise be able to point the worker at a
        // different address than the listener we are about to accept on, and the failure
        // would be a worker that boots and never connects.
        command.env(WORKER_SOCKET_ENV, address.to_string());
    }
    configure_command(&mut command);

    let mut child = command.spawn().map_err(|source| ServerError::Spawn {
        binary: spec.binary.clone(),
        source,
    })?;

    // Adopt before waiting on anything, so a failure below cannot leave an
    // orphan holding a pipe.
    if let Err(source) = guard.adopt(&child) {
        // A process that exited before it could be adopted is not a lifecycle
        // failure, and saying "job object" would hide the real reason.
        return Err(match child.try_wait() {
            Ok(Some(status)) => ServerError::StartupExited {
                binary: spec.binary.clone(),
                status: status.to_string(),
                hint: Hint(transport_hint(spec)),
            },
            _ => ServerError::Spawn {
                binary: spec.binary.clone(),
                source,
            },
        });
    }

    if let Some(stderr) = child.stderr.take() {
        drain_stderr(name.to_string(), child.id(), stderr);
    }

    let channel = match listener {
        // The worker connects back to the listener bound above. The timeout matters: a
        // worker that never connects must fail here rather than hold startup for ever, and
        // it is bounded by the same budget the handshake gets.
        Some(listener) => {
            let accepted =
                match accept_with_deadline(&listener, spec, &mut child, HANDSHAKE_TIMEOUT) {
                    Ok(stream) => stream,
                    Err(source) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(source);
                    }
                };
            // The protocol is latency-sensitive and its frames are small; Nagle would
            // only add delay to a request that is written frame by frame.
            let _ = accepted.set_nodelay(true);
            let reader = accepted.try_clone().map_err(|source| ServerError::Spawn {
                binary: spec.binary.clone(),
                source,
            })?;
            BoundedChannel::new(accepted, reader)
        }
        None => {
            let stdin = child.stdin.take().expect("stdin was piped");
            let stdout = child.stdout.take().expect("stdout was piped");
            BoundedChannel::new(stdin, stdout)
        }
    };

    match handshake_with_timeout(channel, HANDSHAKE_TIMEOUT, spec.checksum) {
        Ok((channel, ack)) => {
            tracing::debug!(
                backend = %name,
                pid = child.id(),
                php = %ack.php_version,
                sapi = %ack.sapi,
                transport = %spec.transport,
                "worker ready"
            );
            Ok(Ready {
                child,
                exchange: ChannelExchange::start(channel),
                ack,
            })
        }
        Err(failure) => {
            if matches!(failure, HandshakeFailure::TimedOut) {
                // The reader thread is still blocked in a read; ending the
                // process is what ends it.
                let _ = child.kill();
            }
            let status = child.try_wait().ok().flatten();
            let _ = child.wait();

            let source = match failure {
                HandshakeFailure::TimedOut => WorkerError::Timeout(HANDSHAKE_TIMEOUT),
                HandshakeFailure::Refused(err) => err,
            };

            Err(match status {
                // The worker exited instead of answering, so its own exit is the
                // useful part of the story.
                Some(status) => ServerError::StartupExited {
                    binary: spec.binary.clone(),
                    status: status.to_string(),
                    hint: Hint(transport_hint(spec)),
                },
                None => ServerError::WorkerProtocol {
                    backend: name.to_string(),
                    source,
                },
            })
        }
    }
}

/// Why a boot handshake did not produce a `HELLO_ACK`.
#[derive(Debug)]
enum HandshakeFailure {
    /// The deadline expired. The reader thread is still blocked and the caller
    /// must kill the worker to end it.
    TimedOut,
    /// The worker answered with something unusable, or its pipe ended first.
    Refused(WorkerError),
}

/// Run the handshake on its own thread, so that it can have a deadline.
///
/// A pipe read has no portable timeout, so the deadline is enforced from the
/// outside: this gives up, the caller kills the worker, and the pending read then
/// fails — which is how the reader thread gets to exit. Nothing is closed from
/// here, because closing the pipes would need a handle this function deliberately
/// does not have.
fn handshake_with_timeout(
    mut channel: Channel,
    timeout: Duration,
    policy: Checksum,
) -> std::result::Result<(Channel, HelloAck), HandshakeFailure> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        // The offer is recorded *before* the handshake because that is the frame it travels in:
        // `false` means no `HELLO` is ever sent offering an unchecked frame, `true` means one is
        // sent and the option is adopted only if the worker answers with it accepted.
        channel.offer_no_checksum(policy == Checksum::Optional);
        let result = channel.handshake();
        // Hand the channel back so the winner can keep using it. A send failure
        // means the receiver gave up, and then the channel is dropped with this
        // thread, closing the pipes.
        let _ = sender.send((channel, result));
    });

    match receiver.recv_timeout(timeout) {
        Ok((channel, Ok(ack))) => Ok((channel, ack)),
        Ok((_channel, Err(err))) => Err(HandshakeFailure::Refused(err)),
        Err(RecvTimeoutError::Timeout) => Err(HandshakeFailure::TimedOut),
        Err(RecvTimeoutError::Disconnected) => Err(HandshakeFailure::Refused(WorkerError::Closed(
            "the handshake thread ended without answering".to_string(),
        ))),
    }
}

/// Log a worker's `stderr` until it ends.
///
/// This thread exists so the pipe cannot fill up, and it logs because for PHP
/// this is where errors go: the SDK points `display_errors` at `stderr` precisely
/// so that diagnostics cannot corrupt the protocol stream. For a worker written
/// without the SDK it is simply free-form output.
///
/// It must run on its own thread, and that is not incidental: reading `stderr` to
/// end-of-file blocks until the worker exits, so doing it inline would deadlock
/// the boot — the worker waits for `HELLO` while we wait for a line that will
/// never come. This is the hazard in `docs/plan.md` §5, "drain `STDERR` for every
/// backend, always", seen from the other side: draining it on the caller's thread
/// is the same bug with a different shape.
fn drain_stderr(backend: String, pid: u32, stderr: ChildStderr) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&line);
                    let text = text.trim_end();
                    if !text.is_empty() {
                        tracing::debug!(backend = %backend, pid, line = %text, "worker stderr");
                    }
                }
                Err(err) => {
                    tracing::debug!(backend = %backend, pid, %err, "worker stderr ended");
                    break;
                }
            }
        }
    });
}

#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use volucer_core::config::WorkerTransport;

/// The pieces of a worker specification `boot` does not read.
///
/// `boot` resolves no program and parses no command — those checks already happened in
/// `Spec::from_config` — so a test driving startup failures can leave the binary as a path
/// that does not exist.
#[cfg(test)]
fn bare_spec(transport: WorkerTransport) -> Spec {
    Spec {
        binary: PathBuf::from("php"),
        args: Vec::new(),
        working_dir: None,
        env: BTreeMap::new(),
        checksum: Checksum::Optional,
        transport,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use volucer_core::config::WorkerTransport;

    /// A socket-transport worker that never connects back fails startup, promptly.
    ///
    /// `boot` originally called a bare `listener.accept()`, which has no deadline. A worker
    /// that connects to the wrong address therefore hung startup for ever with nothing
    /// logged, because no step in the sequence reports a *missing* connection. The test
    /// asserts the two things that make the difference visible: the error names the cause,
    /// and it arrives in about a second rather than never.
    ///
    /// A short timeout is passed instead of the real 30 s handshake budget so the test stays
    /// fast; the deadline arithmetic is the same either way.
    #[test]
    fn a_socket_worker_that_never_connects_times_out_rather_than_hanging() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        // A process that has already exited, standing in for the worker that failed to
        // connect — this is the case where waiting out the deadline would be the wrong
        // answer, and the one `boot` must recognise by exit status.
        let mut child = Command::new(if cfg!(windows) { "cmd" } else { "sh" })
            .args(if cfg!(windows) {
                vec!["/C", "exit 3"]
            } else {
                vec!["-c", "exit 3"]
            })
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let started = Instant::now();
        let err = accept_with_deadline(
            &listener,
            &bare_spec(WorkerTransport::Socket),
            &mut child,
            Duration::from_secs(5),
        )
        .expect_err("a listener nobody connects to must not succeed");
        let elapsed = started.elapsed();

        let _ = child.kill();
        let _ = child.wait();

        assert!(
            elapsed < Duration::from_secs(3),
            "the failure must be reported as soon as the worker is known to be gone, \
         not after the full budget: took {elapsed:?}"
        );
        assert!(
            matches!(err, ServerError::StartupExited { .. }),
            "a worker that exited cannot connect, so its exit status is the useful part: {err:?}"
        );
        assert!(
            err.to_string().contains("php"),
            "the error names the binary that failed: {err}"
        );
    }

    /// The timeout path itself: a live worker that never dials back.
    #[test]
    fn a_live_socket_worker_that_never_connects_hits_the_startup_timeout() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();

        // Long enough to outlive the deadline below, and silent on both streams.
        let mut child = Command::new(if cfg!(windows) { "cmd" } else { "sleep" })
            .args(if cfg!(windows) {
                vec!["/C", "ping -n 6 127.0.0.1 >NUL"]
            } else {
                vec!["5"]
            })
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let err = accept_with_deadline(
            &listener,
            &bare_spec(WorkerTransport::Socket),
            &mut child,
            Duration::from_millis(200),
        )
        .expect_err("a live worker that never connects must not hold startup for ever");

        let _ = child.kill();
        let _ = child.wait();

        match err {
            ServerError::StartupTimeout {
                address: found,
                timeout,
                ..
            } => {
                assert_eq!(
                    found,
                    address.to_string(),
                    "the error names the address waited on"
                );
                assert_eq!(timeout, Duration::from_millis(200));
            }
            other => panic!("expected a startup timeout, got {other:?}"),
        }
    }
}
