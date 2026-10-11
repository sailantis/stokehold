//! A pool of persistent PHP workers, spoken to over their pipes.
//!
//! This is the `worker` backend. `crates/stokehold-worker` owns the codec and
//! `docs/worker-protocol.md` owns the wire format; this owns the processes:
//! starting them, completing the handshake, correlating requests, draining
//! `stderr`, recycling on a threshold, and making sure nothing is left running.
//!
//! Four things here are forced rather than chosen, and each one is the
//! difference between a pool that recovers and one that hangs:
//!
//! 1. **`stderr` is drained, always.** A worker whose `stderr` pipe fills up
//!    blocks for ever on its next write, and nothing about the failure looks
//!    like a crash. So there is one reader thread per worker, and it logs what
//!    it reads: for PHP that is where `display_errors = stderr` output goes, so
//!    it is also the only place a syntax error or an uncaught fatal ever
//!    appears.
//! 2. **Every wait has a deadline, and the deadline is enforced by killing the
//!    worker.** A pipe read has no portable timeout, so each exchange runs on a
//!    thread the caller is free to stop waiting on, and the worker is killed to
//!    end it. The process *is* the cancellation token.
//! 3. **Recycling is the pool's business, not the client's.** A worker that
//!    reported `keep_alive = false`, hit `max_requests`, or exceeded
//!    `memory_limit_mb` is replaced in place, so the pool keeps its size and the
//!    next request never sees the difference.
//! 4. **A clean shutdown is the worker's own doing.** Its stdin is closed and it
//!    exits by itself; killing is the fallback, never the first move.
//!
//! One difference from the FastCGI backends is deliberate and worth stating,
//! because the other two do the opposite: **a failed request is not retried.**
//! There, a transport error means the request never reached the application — the
//! bytes of a half-written frame cannot have been executed — so replaying it is
//! safe. Here, a failure can happen *after* the application ran and produced
//! side effects, and a frame the server did not finish reading is not evidence
//! that nothing happened. Replaying a `POST` because its response was malformed
//! is how an application is made to run twice, so the error is reported instead.
//!
//! **Concurrency is per slot.** The pool takes `&self` and each slot is behind
//! its own `Mutex`, so a pool of `n` workers serves `n` requests at once and a
//! request to a free worker never waits for a busy one. The protocol's
//! one-request-at-a-time rule is per *worker*, not per pool, and the lock is what
//! enforces it now: a worker's pipes are moved into the thread that uses them for
//! the duration of one request, so nothing else can read or write them meanwhile.
//! That is the borrow checker doing it, rather than a comment.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use stokehold_core::config::WorkerConfig;
use stokehold_fcgi::protocol::ProtocolStatus;
use stokehold_fcgi::{EndRequest, NameValuePairs, Request, Response};
use stokehold_worker::codec::WorkerResponse;
use stokehold_worker::{
    Control, HelloAck, LogRecord, MessageType, RequestHeader, WorkerChannel, WorkerError,
    UNSOLICITED,
};

use crate::error::{Result, ServerError};
use crate::lifecycle::{configure_command, LifecycleGuard};
use crate::pool::worker_count;

/// How long a worker gets to complete the `HELLO` / `HELLO_ACK` handshake.
///
/// Generous, because it covers loading the application: a framework boot costs
/// real time, and a worker that boots once should not be killed for taking a
/// while at the one moment where being slow is expected.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a single request may take before the worker is replaced.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a worker has to exit after being told to, before it is killed.
const EXIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Both directions of one worker's pipes.
type Channel = WorkerChannel<ChildStdin, ChildStdout>;

/// A pool of persistent PHP workers.
#[derive(Debug)]
pub struct WorkerPool {
    name: String,
    spec: Spec,
    /// The workers, each independently lockable.
    ///
    /// The lock is per slot rather than around the pool, so a pool of `n`
    /// workers serves `n` requests at once. That is the whole point of
    /// `workers`: the PHP processes are separate and the protocol is
    /// one-request-at-a-time *per worker*, so nothing about the protocol wants
    /// requests serialised. A single lock around the pool would make `workers`
    /// buy recycling and memory spreading but not parallelism, which is a
    /// surprising thing for a setting called "workers" to mean.
    slots: Vec<Mutex<Slot>>,
    /// Round-robin position, so work and therefore memory are spread evenly.
    cursor: AtomicUsize,
    /// `memory_limit_mb` in bytes, worked out once.
    memory_limit_bytes: Option<u64>,
    max_requests: Option<u64>,
    /// Held for the life of the pool; dropping it reaps every worker.
    guard: LifecycleGuard,
}

impl WorkerPool {
    /// Start a pool from configuration.
    ///
    /// Every worker is booted and handshaken here, so a worker that cannot start
    /// is reported at startup rather than on the first request that lands on it.
    pub fn start(name: &str, config: &WorkerConfig) -> Result<Self> {
        let spec = Spec::from_config(name, config)?;

        let guard = LifecycleGuard::new(&format!("stokehold-{name}")).map_err(|source| {
            ServerError::Misconfigured {
                backend: name.to_string(),
                reason: format!("could not create the process lifecycle guard: {source}"),
            }
        })?;

        let count = worker_count(config.workers);
        let mut slots = Vec::with_capacity(count as usize);
        for _ in 0..count {
            // A failure here returns early, and `slots` is dropped — which kills
            // every worker that did start, so a partial pool leaves nothing
            // behind.
            slots.push(Mutex::new(Slot::from_boot(boot(name, &spec, &guard)?)));
        }

        let pool = WorkerPool {
            name: name.to_string(),
            spec,
            slots,
            cursor: AtomicUsize::new(0),
            memory_limit_bytes: config
                .memory_limit_mb
                .map(|mb| mb.saturating_mul(1024 * 1024)),
            max_requests: config.max_requests,
            guard,
        };

        tracing::info!(
            backend = %pool.name,
            workers = pool.slots.len(),
            command = %config.command,
            "worker pool started"
        );
        Ok(pool)
    }

    /// Number of workers in the pool, which does not change.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// The process ids of the workers, for diagnostics and tests.
    ///
    /// A replaced worker gets a new one, so this is also how a test observes a
    /// recycle without having to make the pool report it.
    pub fn pids(&self) -> Vec<u32> {
        self.locked_slots()
            .iter()
            .map(|slot| slot.child.id())
            .collect()
    }

    /// What each worker last reported as its peak resident set, in bytes.
    ///
    /// Zero until a worker has served a request, because the figure comes from
    /// the `END` frame and a worker that has served nothing has not reported one.
    pub fn peak_memory_bytes(&self) -> Vec<u64> {
        self.locked_slots()
            .iter()
            .map(|slot| slot.peak_memory_bytes)
            .collect()
    }

    /// The PHP version each worker reported at handshake.
    ///
    /// Worth knowing at startup: a pool running an unexpected PHP version is a
    /// deployment problem, and the handshake is the only place it is ever said.
    /// Owned strings rather than borrows, because the values live behind a lock
    /// that is released as soon as this returns.
    pub fn php_versions(&self) -> Vec<String> {
        self.locked_slots()
            .iter()
            .map(|slot| slot.hello.php_version.clone())
            .collect()
    }

    /// Every slot, locked in turn.
    ///
    /// The lock is never held across a request here, which is why this is safe
    /// to call while other threads are serving: it takes each lock only long
    /// enough to read a field.
    fn locked_slots(&self) -> Vec<std::sync::MutexGuard<'_, Slot>> {
        self.slots
            .iter()
            .map(|slot| slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
            .collect()
    }

    /// Serve one request on the next worker in rotation.
    ///
    /// Takes `&self`, so the same pool can be serving other requests on other
    /// threads at the same time; each one holds only its own slot's lock, and
    /// only for as long as its worker needs.
    pub fn request(&self, request: &Request) -> Result<Response> {
        let index = self.pick_slot()?;
        let mut slot = self.slots[index]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let started = Instant::now();

        let response = match invoke(&mut slot, request) {
            Ok(response) => response,
            Err(source) => {
                // `invoke` has already decided the worker's fate: a fatal
                // failure dropped its pipes, a recoverable one put them back. So
                // a slot without pipes is a slot that needs a replacement.
                if slot.channel.is_none() {
                    replace(&self.name, &self.spec, &self.guard, &mut slot);
                }
                return Err(ServerError::WorkerProtocol {
                    backend: self.name.clone(),
                    source,
                });
            }
        };

        let duration = started.elapsed();
        after_response(
            &self.name,
            &self.spec,
            &self.guard,
            self.max_requests,
            self.memory_limit_bytes,
            &mut slot,
            &response,
        );
        Ok(render(response, duration))
    }

    /// Replace the worker in a slot, for an operator or a test.
    ///
    /// The lock means the slot must not be mid-request, so this is only
    /// meaningful for an idle slot, which is why it is not called from anywhere
    /// on the request path.
    pub fn restart(&self, index: usize) -> Result<()> {
        let Some(slot) = self.slots.get(index) else {
            return Err(ServerError::NoWorker {
                backend: self.name.clone(),
            });
        };
        let mut slot = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        respawn(&self.name, &self.spec, &self.guard, &mut slot)
    }

    /// Stop every worker, waiting for each to exit.
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
            slot.stop();
        }
        self.slots.clear();
    }

    /// The slot to use next.
    ///
    /// Round-robin rather than always the first slot: with persistent workers an
    /// uneven distribution is also an uneven memory picture, and `max_requests`
    /// recycling would then only ever happen on one of them. A dead worker is
    /// replaced in place so the pool keeps its size; the alternative is a pool
    /// that silently shrinks under repeated crashes.
    ///
    /// A slot is only ever *reserved* here, not held: the index is returned and
    /// the caller takes the lock, so the search never blocks on a busy worker.
    fn pick_slot(&self) -> Result<usize> {
        let count = self.slots.len();
        if count == 0 {
            return Err(ServerError::NoWorker {
                backend: self.name.clone(),
            });
        }

        let base = self.cursor.fetch_add(1, Ordering::Relaxed);
        for offset in 0..count {
            let index = (base + offset) % count;
            let mut slot = self.slots[index]
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if slot.is_available() {
                return Ok(index);
            }
            tracing::warn!(backend = %self.name, slot = index, "a worker died; replacing it");
            if respawn(&self.name, &self.spec, &self.guard, &mut slot).is_err() {
                continue;
            }
            return Ok(index);
        }

        Err(ServerError::NoWorker {
            backend: self.name.clone(),
        })
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Run one request on a worker, putting its pipes back if they survive.
fn invoke(slot: &mut Slot, request: &Request) -> std::result::Result<WorkerResponse, WorkerError> {
    let header = request_header(request);
    // Owned rather than borrowed because the request runs on its own thread,
    // which is what makes the deadline implementable at all. One copy of the
    // request body per request is the price of a portable timeout.
    let body = request.body.clone();

    let channel = slot
        .channel
        .take()
        .expect("a slot is only chosen while it has its pipes");

    let attempt = request_with_deadline(channel, header, body, REQUEST_TIMEOUT);

    if let Some(channel) = attempt.channel {
        // The pipes are intact, so the worker goes back in rotation before we
        // interpret the failure: a worker that refused one request is still a
        // perfectly good worker.
        slot.channel = Some(channel);
    }

    match attempt.result {
        Ok(response) => Ok(response),
        Err(source) => {
            if source.is_fatal() {
                // The stream has desynchronised, or the worker is gone. Either
                // way its pipes cannot be trusted again.
                slot.terminate();
            }
            Err(source)
        }
    }
}

/// Decide what a completed response means for the worker that produced it.
fn after_response(
    name: &str,
    spec: &Spec,
    guard: &LifecycleGuard,
    max_requests: Option<u64>,
    memory_limit_bytes: Option<u64>,
    slot: &mut Slot,
    response: &WorkerResponse,
) {
    slot.served += 1;
    slot.peak_memory_bytes = response.end.peak_memory_bytes;

    let mut recycle = if response.end.keep_alive {
        None
    } else {
        Some("the worker asked to be recycled")
    };

    if recycle.is_none() {
        if let Some(max) = max_requests {
            if slot.served >= max {
                recycle = Some("max_requests");
            }
        }
    }

    if recycle.is_none() {
        if let Some(limit) = memory_limit_bytes {
            if slot.peak_memory_bytes > limit {
                recycle = Some("memory_limit_mb");
            }
        }
    }
    if let Some(reason) = recycle {
        tracing::debug!(backend = %name, pid = slot.child.id(), reason, "recycling a worker");
        replace(name, spec, guard, slot);
    }
}

/// Replace the worker in a slot, keeping the pool's shape.
fn respawn(name: &str, spec: &Spec, guard: &LifecycleGuard, slot: &mut Slot) -> Result<()> {
    slot.terminate();
    let ready = boot(name, spec, guard)?;
    *slot = Slot::from_boot(ready);
    Ok(())
}

/// Replace the worker in a slot, reporting rather than propagating failure.
///
/// The caller is already handling a request that failed; a second error on the
/// same path would only hide the first. The slot is left without pipes, so
/// `pick_slot` tries again on the next request instead of handing out a worker
/// that is known to be dead.
fn replace(name: &str, spec: &Spec, guard: &LifecycleGuard, slot: &mut Slot) {
    if let Err(err) = respawn(name, spec, guard, slot) {
        tracing::error!(
            backend = %name,
            pid = slot.child.id(),
            %err,
            "could not replace a worker"
        );
    }
}

/// One worker process and the pipes to talk to it on.
///
/// `channel` is `None` whenever the worker must not be used: while a request is
/// in flight (the pipes have been moved into the request thread), after a fatal
/// protocol failure, and after the worker has been stopped.
#[derive(Debug)]
struct Slot {
    child: Child,
    channel: Option<Channel>,
    /// The handshake answer, kept so a diagnostic can say which PHP and SAPI the
    /// slot is actually running.
    hello: HelloAck,
    /// Requests this worker has served, for `max_requests`.
    served: u64,
    /// The peak resident set the worker last reported, in bytes.
    ///
    /// The peak, not the current usage, because a high-water mark is what a leak
    /// threshold wants: a transient spike during one request is the interesting
    /// event, and it is exactly what a spot check between requests would miss.
    ///
    /// `memory_get_peak_usage(true)` is itself monotonic from process start, so on
    /// its own it would make this a "has this worker *ever* peaked" flag — a worker
    /// that legitimately peaked once would be recycled after every subsequent
    /// request, one application boot per request. The SDK avoids that by calling
    /// `memory_reset_peak_usage()` after reading the figure (present from PHP 8.2,
    /// which is the SDK's floor; a version below it leaves the report monotonic
    /// rather than failing), so the number in `END` measures the request that just
    /// finished. Verified in the PHP source: `zend_memory_reset_peak_usage` sets
    /// `real_peak = real_size`.
    ///
    /// A worker that does not use the SDK, or speaks an older protocol revision,
    /// may still report a cumulative figure. The pool acts on what it is told.
    peak_memory_bytes: u64,
}

impl Slot {
    fn from_boot(ready: Ready) -> Self {
        Slot {
            child: ready.child,
            channel: Some(ready.channel),
            hello: ready.ack,
            served: 0,
            peak_memory_bytes: 0,
        }
    }

    /// Whether this worker can be handed a request.
    ///
    /// Two independent conditions: the pipes are still attached (a worker that
    /// failed to boot or was discarded has none) and the process has not exited.
    /// `try_wait` is the only portable way to ask the second question, and it
    /// matters: a worker that died leaves its pipes at end-of-file, so without it
    /// the pool would confidently hand out a worker that is already gone.
    fn is_available(&mut self) -> bool {
        self.channel.is_some() && matches!(self.child.try_wait(), Ok(None))
    }

    /// Kill the worker and drop its pipes.
    fn terminate(&mut self) {
        // The pipes go first, so nothing can read a half-written frame while the
        // process is being reaped.
        self.channel = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Ask the worker to stop, and make sure that it does.
    ///
    /// Two signals go out, in this order: a `CONTROL drain` frame so the worker
    /// can flush and clean up, then end-of-file on its stdin, which is the
    /// shutdown mechanism that cannot fail — the worker's read loop returns and
    /// the process exits on its own. Waiting for the acknowledgement would need a
    /// reader thread and a deadline for no gain, because the frame is already in
    /// the pipe buffer when the pipe closes, so the worker sees both signals in
    /// order. Killing is only the fallback for a worker that ignores both.
    fn stop(&mut self) {
        if let Some(mut channel) = self.channel.take() {
            let _ = channel.send_json(MessageType::Control, UNSOLICITED, &Control::Drain, 0);
            // Dropping the channel closes stdin.
        }
        self.wait_or_kill(EXIT_TIMEOUT);
    }

    /// Wait for the process to exit, killing it if it does not.
    fn wait_or_kill(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        // The safety net for the paths that return early: a worker whose slot is
        // dropped must not survive, whatever happened before.
        self.terminate();
    }
}

/// A worker that has been started and has completed its handshake.
#[derive(Debug)]
struct Ready {
    child: Child,
    channel: Channel,
    ack: HelloAck,
}

/// What a worker process is started from.
#[derive(Debug, Clone)]
struct Spec {
    /// The resolved program, so a missing interpreter is a startup error rather
    /// than a confusing `CreateProcess` / `execvp` failure later.
    binary: PathBuf,
    args: Vec<String>,
    working_dir: Option<PathBuf>,
    env: BTreeMap<String, String>,
}

impl Spec {
    /// Split, resolve and check a configured worker command.
    ///
    /// Checked here rather than at spawn time so that a typo in `command` or
    /// `working_dir` is reported by name, before any port is bound and before a
    /// pool half-exists.
    fn from_config(name: &str, config: &WorkerConfig) -> Result<Self> {
        let (program, args) =
            split_command(&config.command).map_err(|reason| ServerError::Misconfigured {
                backend: name.to_string(),
                reason,
            })?;
        let binary = resolve_program(&program);

        if !binary.is_file() {
            return Err(ServerError::Misconfigured {
                backend: name.to_string(),
                reason: format!(
                    "the worker program {} does not exist (from command {:?})",
                    binary.display(),
                    config.command
                ),
            });
        }

        if let Some(dir) = &config.working_dir {
            if !dir.is_dir() {
                return Err(ServerError::Misconfigured {
                    backend: name.to_string(),
                    reason: format!("working_dir {} is not a directory", dir.display()),
                });
            }
        }

        Ok(Spec {
            binary,
            args,
            working_dir: config.working_dir.clone(),
            env: config.env.clone(),
        })
    }
}

/// Start a worker and complete its handshake.
///
/// Everything that can go wrong with a worker does so here, which is the point:
/// a worker that cannot boot is a startup error, not a mystery on the first
/// request that happens to reach it.
fn boot(name: &str, spec: &Spec, guard: &LifecycleGuard) -> Result<Ready> {
    let mut command = Command::new(&spec.binary);
    command
        .args(&spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Piped, never nulled: PHP writes its diagnostics here, and a worker
        // whose stderr pipe fills up blocks for ever on its next write.
        .stderr(Stdio::piped());
    if let Some(dir) = &spec.working_dir {
        command.current_dir(dir);
    }
    for (key, value) in &spec.env {
        command.env(key, value);
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
            },
            _ => ServerError::Spawn {
                binary: spec.binary.clone(),
                source,
            },
        });
    }

    let stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");
    if let Some(stderr) = child.stderr.take() {
        drain_stderr(name.to_string(), child.id(), stderr);
    }

    let channel = WorkerChannel::new(stdin, stdout);
    match handshake_with_timeout(channel, HANDSHAKE_TIMEOUT) {
        Ok((channel, ack)) => {
            tracing::debug!(
                backend = %name,
                pid = child.id(),
                php = %ack.php_version,
                sapi = %ack.sapi,
                "worker ready"
            );
            Ok(Ready {
                child,
                channel,
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
) -> std::result::Result<(Channel, HelloAck), HandshakeFailure> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
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

/// The outcome of one deadline-bounded request.
struct Attempt {
    /// The pipes, unless they were left behind in a thread that is still blocked
    /// and therefore cannot be used again.
    channel: Option<Channel>,
    result: std::result::Result<WorkerResponse, WorkerError>,
}

/// Send one request and collect its response, giving up after `timeout`.
///
/// The channel is *moved* into the thread, so nothing else can read or write
/// these pipes while the request is in flight — the borrow checker is what
/// enforces the protocol's one-request-at-a-time rule here.
fn request_with_deadline(
    mut channel: Channel,
    header: RequestHeader,
    body: Vec<u8>,
    timeout: Duration,
) -> Attempt {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let result = channel.request(&header, &body);
        let _ = sender.send((channel, result));
    });

    match receiver.recv_timeout(timeout) {
        Ok((channel, result)) => Attempt {
            channel: Some(channel),
            result,
        },
        // Timed out. The channel is inside a thread that is still blocked in a
        // read, so the caller has to kill the worker: that is what makes the read
        // return so the thread can exit and drop the pipes.
        Err(RecvTimeoutError::Timeout) => Attempt {
            channel: None,
            result: Err(WorkerError::Timeout(timeout)),
        },
        Err(RecvTimeoutError::Disconnected) => Attempt {
            channel: None,
            result: Err(WorkerError::Closed(
                "the request thread ended without answering".to_string(),
            )),
        },
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

/// Turn a FastCGI request into a worker `REQUEST` header.
///
/// The translation goes through the CGI parameters rather than the original HTTP
/// request, and that is the better direction: the front-end has already resolved
/// the path against the document root, so `SCRIPT_FILENAME` here is the file it
/// decided to run, not a second guess at it. It also means the pool needs no
/// routing of its own — one source of truth for what the request is.
fn request_header(request: &Request) -> RequestHeader {
    let params = &request.params;

    RequestHeader {
        method: text_param(params, "REQUEST_METHOD").unwrap_or_else(|| "GET".to_string()),
        uri: text_param(params, "REQUEST_URI").unwrap_or_else(|| "/".to_string()),
        protocol: text_param(params, "SERVER_PROTOCOL").unwrap_or_else(|| "HTTP/1.1".to_string()),
        headers: headers_from_params(params),
        remote_addr: text_param(params, "REMOTE_ADDR").unwrap_or_default(),
        // Overwritten by `WorkerChannel::request` from the body it frames, so the
        // declared length and the bytes sent cannot disagree.
        body_len: 0,
        script_filename: text_param(params, "SCRIPT_FILENAME").unwrap_or_default(),
        https: match params.iter().find(|(key, _)| key.as_slice() == b"HTTPS") {
            // Present means the request arrived over TLS; CGI uses "on" and
            // "off", and the front-end only writes the former.
            Some((_, value)) => !value.eq_ignore_ascii_case(b"off") && !value.is_empty(),
            None => false,
        },
    }
}

/// A CGI parameter as a string.
fn text_param(params: &NameValuePairs, name: &str) -> Option<String> {
    params
        .iter()
        .find(|(key, _)| key.as_slice() == name.as_bytes())
        .map(|(_, value)| String::from_utf8_lossy(value).into_owned())
}

/// Rebuild HTTP headers from the CGI parameters.
///
/// `CONTENT_LENGTH` is deliberately *not* turned back into a header: `body_len`
/// is authoritative, and two sources for one number is how they drift apart.
/// Everything else the front-end wrote round-trips, because it built these
/// variables from the client's headers in the first place.
fn headers_from_params(params: &NameValuePairs) -> Vec<(String, String)> {
    let mut headers = Vec::with_capacity(params.len());
    for (name, value) in params {
        let Ok(name) = std::str::from_utf8(name) else {
            continue;
        };
        let Some(suffix) = name.strip_prefix("HTTP_") else {
            continue;
        };
        if suffix.is_empty() {
            continue;
        }
        headers.push((
            header_name(suffix),
            String::from_utf8_lossy(value).into_owned(),
        ));
    }

    // `Content-Type` keeps its CGI name, which is exactly how it arrives.
    if let Some((_, value)) = params
        .iter()
        .find(|(key, _)| key.as_slice() == b"CONTENT_TYPE")
    {
        headers.push((
            "Content-Type".to_string(),
            String::from_utf8_lossy(value).into_owned(),
        ));
    }

    headers
}

/// `ACCEPT_ENCODING` → `Accept-Encoding`.
///
/// Cosmetic, but worth doing: these names end up in application logs and in
/// whatever a handler does with them, and `HTTP_X_FORWARDED_FOR` reconstructed by
/// the SDK means either spelling works for lookup.
fn header_name(cgi_suffix: &str) -> String {
    let mut name = String::with_capacity(cgi_suffix.len());
    for (index, segment) in cgi_suffix.split('_').enumerate() {
        if index > 0 {
            name.push('-');
        }
        let mut chars = segment.chars();
        if let Some(first) = chars.next() {
            name.extend(first.to_uppercase());
            name.extend(chars.flat_map(char::to_lowercase));
        }
    }
    name
}

/// Turn a worker response into what the rest of stokehold expects.
///
/// The body is relayed verbatim, exactly as for the FastCGI backends, but the
/// header block has to be produced here: a worker hands back a status and a list
/// of headers, while a front-end relays a `Status:`-prefixed block. Translating
/// once, at the backend, is what keeps both front-ends identical for every
/// backend kind.
fn render(response: WorkerResponse, duration: Duration) -> Response {
    let mut stdout = Vec::with_capacity(response.body.len() + 128);
    stdout.extend_from_slice(
        format!(
            "Status: {} {}\r\n",
            response.head.status,
            reason_phrase(response.head.status)
        )
        .as_bytes(),
    );
    for (name, value) in &response.head.headers {
        stdout.extend_from_slice(name.as_bytes());
        stdout.extend_from_slice(b": ");
        stdout.extend_from_slice(value.as_bytes());
        stdout.extend_from_slice(b"\r\n");
    }
    stdout.extend_from_slice(b"\r\n");
    stdout.extend_from_slice(&response.body);

    Response {
        stdout,
        stderr: render_logs(&response.logs),
        end: Some(EndRequest {
            // Negative would mean the application reported a status that cannot
            // be expressed here; clamping is the honest thing to do with a value
            // the protocol only defines for zero and above.
            app_status: response.end.app_status.max(0) as u32,
            protocol_status: ProtocolStatus::RequestComplete,
        }),
        // The pool, not this worker, owns reuse: a worker that asked to be
        // recycled is already replaced, so the backend as a whole is still
        // ready for the next request. Nothing reads this to decide whether the
        // *client* connection survives — that is the client's own keep-alive.
        keep_conn: true,
        duration,
    }
}

/// Format the telemetry a worker emitted while serving a request.
///
/// It goes to `STDERR` because that is where a front-end already puts whatever a
/// backend says out of band: nginx writes it to its error log, which is where a
/// developer looks first. Without this a worker's `emitLog()` would be read,
/// stored, and then silently dropped.
fn render_logs(logs: &[LogRecord]) -> Vec<u8> {
    if logs.is_empty() {
        return Vec::new();
    }
    let mut out = String::new();
    for record in logs {
        out.push_str(&format!("[{}] {}\n", record.level, record.message));
    }
    out.into_bytes()
}

/// The reason phrase for a status code.
///
/// A front-end relays the `Status:` line verbatim, and HTTP/1.0 clients exist
/// that read it, so a bare code is not quite enough. Only the codes PHP
/// applications actually produce are listed; anything else is sent without a
/// phrase, which every client accepts because HTTP/1.1 makes the phrase optional.
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        418 => "I'm a teapot",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => "",
    }
}

/// Split a command line into a program and its arguments.
///
/// Configuration carries one string (`command = "php worker.php"`), so it has to
/// be split into an argv. This is a deliberately small tokenizer, not a shell:
/// double quotes group, a backslash escapes the next character inside and outside
/// quotes, and there are no variables, globs, subshells or operators. That covers
/// every documented example — including a Windows path, where a backslash before
/// an ordinary character stays a backslash — while keeping a configuration file
/// from being able to run arbitrary shell syntax.
fn split_command(command: &str) -> std::result::Result<(String, Vec<String>), String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quoted = false;
    let mut chars = command.chars();

    while let Some(character) = chars.next() {
        match character {
            '"' => {
                // A quote starts a token even when it is empty, so `""` is an
                // argument rather than nothing.
                quoted = !quoted;
                started = true;
            }
            '\\' => {
                started = true;
                match chars.next() {
                    // `\\` and `\"` are the escapes that mean something; a
                    // backslash before anything else is kept, because that is
                    // what a Windows path needs.
                    Some(next @ ('\\' | '"')) => current.push(next),
                    Some(next) => {
                        current.push('\\');
                        current.push(next);
                    }
                    None => current.push('\\'),
                }
            }
            character if character.is_whitespace() && !quoted => {
                if started {
                    tokens.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            character => {
                started = true;
                current.push(character);
            }
        }
    }

    if quoted {
        return Err(format!("unbalanced quote in command {command:?}"));
    }
    if started {
        tokens.push(current);
    }

    let mut tokens = tokens.into_iter();
    let Some(program) = tokens.next() else {
        return Err("the worker command is empty".to_string());
    };
    Ok((program, tokens.collect()))
}

/// The program to execute for a command's first token.
///
/// `CreateProcess` on Windows and `execvp` on Unix both search `PATH` for a bare
/// name, but only the search is platform-specific and neither reports *which*
/// file it picked. Resolving here means one code path and one error message on
/// both, and it means the preflight check in [`Spec::from_config`] is checking
/// the same file that will actually be run.
fn resolve_program(program: &str) -> PathBuf {
    let path = Path::new(program);
    // A name with a separator is a path, not a `PATH` lookup.
    if path.components().count() > 1 {
        return path.to_path_buf();
    }
    find_on_path(program).unwrap_or_else(|| path.to_path_buf())
}

/// Look a bare program name up on `PATH`.
fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;

    #[cfg(windows)]
    let extensions = path_extensions();

    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }

        #[cfg(windows)]
        for extension in &extensions {
            let candidate = dir.join(format!("{program}.{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// The extensions a `PATH` search should try on Windows.
///
/// Read from `PATHEXT` rather than hard-coded, because `php.exe` is not the only
/// shape a PHP install can take — a shim manager may well provide `php.bat`.
#[cfg(windows)]
fn path_extensions() -> Vec<String> {
    let raw = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    raw.split(';')
        .map(|extension| {
            extension
                .trim()
                .trim_start_matches('.')
                .to_ascii_lowercase()
        })
        .filter(|extension| !extension.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use stokehold_worker::ResponseHead;

    fn params(pairs: &[(&str, &str)]) -> NameValuePairs {
        pairs
            .iter()
            .map(|(name, value)| (name.as_bytes().to_vec(), value.as_bytes().to_vec()))
            .collect()
    }

    #[test]
    fn a_command_is_split_on_whitespace() {
        let (program, args) = split_command("php worker.php").unwrap();
        assert_eq!(program, "php");
        assert_eq!(args, vec!["worker.php"]);
    }

    /// The concurrency guarantee, stated as a type-level fact.
    ///
    /// `WorkerPool` used to take `&mut self` on the request path, which meant the
    /// dispatcher had to hold a lock for the whole of `dispatch` and a pool of
    /// `n` workers served one request at a time — `workers` bought recycling but
    /// not parallelism. It now takes `&self`, with a lock per slot, so this
    /// compiles: a shared pool can be reached from several threads at once. If a
    /// future change reintroduces a pool-wide exclusive borrow, this test stops
    /// compiling, which is the earliest possible warning.
    #[test]
    fn a_pool_can_be_reached_from_several_threads_at_once() {
        fn assert_shareable<T: Send + Sync>() {}
        assert_shareable::<WorkerPool>();
        assert_shareable::<crate::pool::CgiPool>();

        // The shape that matters: the same pool, through a shared reference, from
        // more than one thread. `request` itself takes `&self` for the same
        // reason.
        fn reach_shared(pool: &WorkerPool) -> Vec<usize> {
            std::thread::scope(|scope| {
                let handles: Vec<_> = (0..4).map(|_| scope.spawn(|| pool.len())).collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            })
        }

        let _ = reach_shared;
    }

    #[test]
    fn quotes_group_an_argument_that_contains_spaces() {
        let (program, args) =
            split_command(r#""C:\Program Files\php\php.exe" -d x=1 worker.php"#).unwrap();
        assert_eq!(program, r"C:\Program Files\php\php.exe");
        assert_eq!(args, vec!["-d", "x=1", "worker.php"]);
    }

    #[test]
    fn a_windows_path_keeps_its_backslashes() {
        // The escapes that mean something are `\\` and `\"`; anything else keeps
        // the backslash, because otherwise every Windows path would need
        // doubling — which is exactly the papercut that makes a config file
        // unreadable.
        let (program, args) = split_command(r"C:\php\php.exe C:\app\worker.php").unwrap();
        assert_eq!(program, r"C:\php\php.exe");
        assert_eq!(args, vec![r"C:\app\worker.php"]);
    }

    #[test]
    fn an_empty_quoted_argument_survives() {
        let (_, args) = split_command(r#"php "" worker.php"#).unwrap();
        assert_eq!(args, vec!["", "worker.php"]);
    }

    #[test]
    fn an_unbalanced_quote_is_refused() {
        let err = split_command(r#"php "worker.php"#).unwrap_err();
        assert!(err.contains("unbalanced quote"), "{err}");
    }

    #[test]
    fn an_empty_command_is_refused() {
        let err = split_command("   ").unwrap_err();
        assert!(err.contains("empty"), "{err}");
    }

    #[test]
    fn cgi_header_names_are_rebuilt_in_canonical_form() {
        assert_eq!(header_name("ACCEPT_ENCODING"), "Accept-Encoding");
        assert_eq!(header_name("HOST"), "Host");
        assert_eq!(header_name("X_FORWARDED_FOR"), "X-Forwarded-For");
        assert_eq!(header_name("USER_AGENT"), "User-Agent");
    }

    #[test]
    fn a_request_header_is_translated_from_the_cgi_parameters() {
        let request = Request {
            params: params(&[
                ("REQUEST_METHOD", "POST"),
                ("REQUEST_URI", "/index.php/foo?x=1"),
                ("SERVER_PROTOCOL", "HTTP/1.1"),
                ("REMOTE_ADDR", "127.0.0.1"),
                ("SCRIPT_FILENAME", "/srv/app/index.php"),
                ("HTTP_HOST", "example.test"),
                ("HTTP_ACCEPT_ENCODING", "gzip"),
                ("CONTENT_TYPE", "application/json"),
                ("CONTENT_LENGTH", "12"),
                ("HTTPS", "on"),
            ]),
            body: Vec::new(),
            keep_conn: true,
        };

        let header = request_header(&request);
        assert_eq!(header.method, "POST");
        assert_eq!(header.uri, "/index.php/foo?x=1");
        assert_eq!(header.protocol, "HTTP/1.1");
        assert_eq!(header.remote_addr, "127.0.0.1");
        assert_eq!(header.script_filename, "/srv/app/index.php");
        assert!(header.https);
        assert_eq!(
            header.body_len, 0,
            "the channel decides this, not the CGI env"
        );
        assert_eq!(header.header("host"), Some("example.test"));
        assert_eq!(header.header("Accept-Encoding"), Some("gzip"));
        assert_eq!(header.header("content-type"), Some("application/json"));
        // `CONTENT_LENGTH` is the body's business, and the body is what says so.
        assert_eq!(header.header("content-length"), None);
    }

    #[test]
    fn the_https_flag_needs_a_truthy_value() {
        let off = Request {
            params: params(&[("HTTPS", "off")]),
            ..Request::default()
        };
        assert!(!request_header(&off).https);

        let absent = Request::default();
        assert!(!request_header(&absent).https);
    }

    #[test]
    fn a_response_is_rendered_as_a_status_header_block() {
        let response = WorkerResponse {
            head: ResponseHead {
                status: 404,
                headers: vec![
                    ("Content-Type".to_string(), "text/plain".to_string()),
                    ("Set-Cookie".to_string(), "a=1".to_string()),
                    ("Set-Cookie".to_string(), "b=2".to_string()),
                ],
                body_len: 9,
            },
            body: b"not found".to_vec(),
            end: stokehold_worker::RequestEnd {
                app_status: 1,
                peak_memory_bytes: 4096,
                duration_us: 250,
                keep_alive: false,
            },
            logs: vec![LogRecord {
                level: "warning".to_string(),
                message: "something happened".to_string(),
                request_id: 1,
            }],
            metrics: Vec::new(),
        };

        let rendered = render(response, Duration::from_millis(3));
        assert_eq!(
            rendered.stdout,
            b"Status: 404 Not Found\r\nContent-Type: text/plain\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\n\r\nnot found"
        );
        assert_eq!(rendered.app_status(), Some(1));
        assert!(rendered.check_protocol().is_ok());
        assert_eq!(
            rendered.stderr, b"[warning] something happened\n",
            "telemetry has to reach somewhere a developer looks"
        );
        // A worker that asked to be recycled does not make the backend unusable:
        // the pool replaces it.
        assert!(rendered.keep_conn);
    }

    #[test]
    fn a_worker_with_no_telemetry_produces_no_stderr() {
        assert!(render(WorkerResponse::default(), Duration::ZERO)
            .stderr
            .is_empty());
    }

    #[test]
    fn a_negative_application_status_is_clamped_rather_than_wrapped() {
        let mut response = WorkerResponse::default();
        response.end.app_status = -1;
        assert_eq!(render(response, Duration::ZERO).app_status(), Some(0));
    }

    #[test]
    fn an_unknown_status_has_no_reason_phrase() {
        assert_eq!(reason_phrase(200), "OK");
        assert_eq!(reason_phrase(500), "Internal Server Error");
        assert_eq!(reason_phrase(599), "");
        let mut response = WorkerResponse::default();
        response.head.status = 599;
        assert!(render(response, Duration::ZERO)
            .stdout
            .starts_with(b"Status: 599 \r\n"));
    }

    #[test]
    fn a_missing_program_is_a_misconfiguration_not_a_panic() {
        let config = WorkerConfig {
            command: "definitely-not-a-php-binary worker.php".to_string(),
            workers: 1,
            memory_limit_mb: None,
            max_requests: None,
            working_dir: None,
            env: Default::default(),
        };
        let err = Spec::from_config("test", &config).unwrap_err();
        assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn a_working_directory_that_is_not_a_directory_is_refused() {
        // A path that certainly exists but is not a directory: the test binary
        // itself.
        let binary = std::env::current_exe().expect("the test binary path");
        let config = WorkerConfig {
            command: binary.display().to_string(),
            workers: 1,
            memory_limit_mb: None,
            max_requests: None,
            working_dir: Some(binary),
            env: Default::default(),
        };
        let err = Spec::from_config("test", &config).unwrap_err();
        assert!(err.to_string().contains("is not a directory"), "{err}");
    }

    #[test]
    fn an_empty_command_is_a_misconfiguration() {
        let config = WorkerConfig {
            command: String::new(),
            workers: 1,
            memory_limit_mb: None,
            max_requests: None,
            working_dir: None,
            env: Default::default(),
        };
        let err = Spec::from_config("test", &config).unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn a_program_with_a_separator_is_not_looked_up_on_path() {
        let resolved = resolve_program("definitely/not/a/real/program");
        assert_eq!(resolved, PathBuf::from("definitely/not/a/real/program"));
    }
}
