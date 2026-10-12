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
//!    worker.** A pipe read has no portable timeout, so the exchange runs on a
//!    thread the caller is free to stop waiting on, and the worker is killed to
//!    end it. The process *is* the cancellation token. Every worker owns that
//!    thread for its whole life — a request is handed to it and answered over a
//!    channel — so the deadline costs one `recv_timeout` and never a thread
//!    creation.
//! 3. **Recycling is the pool's business, not the client's.** A worker that
//!    reported `keep_alive = false`, hit `max_requests`, or exceeded
//!    `memory_limit_mb` is replaced in place, so the pool keeps its size and the
//!    next request never sees the difference.
//! 4. **A clean shutdown is the worker's own doing.** Its stdin is closed and it
//!    exits by itself; killing is the fallback, never the first move.
//!
//! One difference from the FastCGI backends is deliberate and worth stating,
//! because the other two do the opposite: **a failed request is retried only when
//! the method says replaying it is safe.** There, a transport error means the
//! request never reached the application — the bytes of a half-written frame
//! cannot have been executed — so replaying any method is safe. Here, a failure can
//! happen *after* the application ran and produced side effects, and a frame the
//! server did not finish reading is not evidence that nothing happened. So
//! `GET`/`HEAD`/`OPTIONS`/`TRACE`/`PUT`/`DELETE` are replayed on a fresh worker when
//! the old one's stream ended, and `POST`/`PATCH` are reported as failures instead:
//! replaying a `POST` because its response was malformed is how an application is
//! made to run twice.
//!
//! **Concurrency is per slot.** The pool takes `&self` and each slot is behind
//! its own `Mutex`, so a pool of `n` workers serves `n` requests at once and a
//! request to a free worker never waits for a busy one. The protocol's
//! one-request-at-a-time rule is per *worker*, not per pool, and each slot's
//! exchange thread is what enforces it now: a request is handed to that thread
//! and the answer comes back on the slot's own channel, so two requests cannot
//! reach one worker at once. The lock means a caller holds its slot for the
//! duration, which is the same guarantee the borrow checker used to give.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::borrow::Cow;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use stokehold_core::config::{ChecksumMode, WorkerConfig};
use stokehold_fcgi::protocol::ProtocolStatus;
use stokehold_fcgi::{EndRequest, NameValuePairs, Request, Response};
use stokehold_worker::codec::RequestStage;
use stokehold_worker::codec::WorkerResponse;
use stokehold_worker::{
    BoundedChannel, BorrowedHeaders, Checksum, Codec, Control, HelloAck, Json, LogRecord,
    MessageType, RequestHeader, RequestHeaderJson, WorkerError, UNSOLICITED,
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

/// Both directions of one worker's pipes, buffered.
///
/// Buffered because every frame is written as a header and then a payload and read the same
/// way: over an unbuffered `ChildStdin`/`ChildStdout` each of those calls is its own syscall,
/// and each read its own wakeup of the exchange thread. See [`BoundedChannel`].
type Channel = BoundedChannel;

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
        match self.attempt(request) {
            Err(failure) if failure.retryable => {
                // The worker died while this request was in flight, and the pool
                // has already replaced it. `invoke` decided a replay is sound: the
                // stage says whether the request could have been delivered, and the
                // verb says whether delivering it twice would matter. Without this
                // a recycled worker answers the request that landed during the
                // restart with a 502, which is a routine event rather than a fault:
                // `max_requests` recycling, a memory-limit recycle and a worker that
                // crashed and was restarted all land here.
                self.attempt(request).map_err(|failure| failure.source)
            }
            outcome => outcome.map_err(|failure| failure.source),
        }
    }

    /// One try at a request, with the slot chosen here so a retry can pick a
    /// different one.
    fn attempt(&self, request: &Request) -> std::result::Result<Response, FailedAttempt> {
        let index = self.pick_slot().map_err(|source| FailedAttempt {
            source,
            // Nothing was sent, so nothing can be repeated: a missing worker is a
            // configuration or capacity condition, not a lost request.
            retryable: false,
        })?;
        let mut slot = self.slots[index]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let started = Instant::now();

        let response = match invoke(&mut slot, request) {
            Ok(response) => response,
            Err(AttemptError { source, retryable }) => {
                // `invoke` has already decided the worker's fate: a fatal
                // failure dropped its exchange, a recoverable one put it back. So
                // a slot without an exchange is a slot that needs a replacement.
                if slot.exchange.is_none() {
                    replace(&self.name, &self.spec, &self.guard, &mut slot);
                }
                return Err(FailedAttempt {
                    source: ServerError::WorkerProtocol {
                        backend: self.name.clone(),
                        source,
                    },
                    retryable,
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

/// Run one request on a worker, putting the slot back into rotation if the pipes
/// survived.
fn invoke(slot: &mut Slot, request: &Request) -> std::result::Result<WorkerResponse, AttemptError> {
    let result = match slot.exchange.as_mut() {
        // The head is serialized here, while the CGI environment it borrows is alive: the exchange
        // thread cannot borrow it, because on a timeout that thread outlives this call. Encoding it
        // first is what lets the translation stay borrowed — nine `String`s became one buffer.
        //
        // The body length is stamped in here because the channel no longer sees the body before the
        // head: it takes bytes that are already framed, so the declared length and the bytes sent
        // are reconciled at the one place that holds both.
        Some(exchange) => {
            let translated = borrowed_request(request);
            let draft = translated.draft();
            let head = RequestHeaderJson {
                body_len: request.body.len() as u64,
                ..draft
            };
            exchange.request(encode_head(&head), &request.body, REQUEST_TIMEOUT)
        }
        // A slot is only chosen while it has an exchange, so this cannot happen;
        // it is reported rather than panicked because a panic here would take the
        // whole server down for one request.
        None => Err((
            RequestStage::Sending,
            WorkerError::Closed("the worker has no exchange".to_string()),
        )),
    };

    match result {
        Ok(response) => Ok(response),
        Err((stage, source)) => {
            if source.is_fatal() {
                // The stream has desynchronised, or the worker is gone. Either
                // way this slot must not serve again until it is replaced, and
                // `Slot::exchange` is what `pick_slot` and `request` read to
                // decide that.
                slot.terminate();
            }
            // Whether the request may be replayed is decided where both the
            // failure and the request are known: the stage says whether it was
            // delivered, the verb says whether delivering it twice would matter,
            // and neither alone is enough. See `may_retry`.
            Err(AttemptError {
                retryable: may_retry(stage, request),
                source,
            })
        }
    }
}

/// A failed attempt at one request, and whether it may be made again.
#[derive(Debug)]
struct AttemptError {
    /// Why the attempt failed.
    source: WorkerError,
    /// Whether the request may be sent to the backend again.
    retryable: bool,
}

/// The same decision, at the boundary of one whole backend call.
///
/// Carried out of `attempt` so `request` can act on it without recomputing the
/// stage-and-verb rule, and so that a retry decision is made exactly once, in
/// `invoke`, where the failing exchange actually happened.
#[derive(Debug)]
struct FailedAttempt {
    /// The backend-level failure, reported if the retry is not made or also fails.
    source: ServerError,
    /// Whether the request may be sent to the backend again.
    retryable: bool,
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
            // `0` means "never", not "immediately" — php-fpm's `pm.max_requests = 0`
            // and RoadRunner's `max_jobs: 0` both mean a pool that never recycles, and
            // a bare `served >= max` comparison would instead destroy the worker after
            // every request. The tests cover the distinction directly.
            if max > 0 && slot.served >= max {
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
/// `exchange` is `None` whenever the worker must not be used: after a fatal
/// protocol failure, and after the worker has been stopped. A request in flight
/// leaves it `Some` — the exchange thread stays alive across requests, and the
/// slot's lock is what keeps two requests off one worker.
#[derive(Debug)]
struct Slot {
    child: Child,
    exchange: Option<ChannelExchange>,
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
            exchange: Some(ready.exchange),
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
        self.exchange.is_some() && matches!(self.child.try_wait(), Ok(None))
    }
    /// Kill the worker and drop its pipes.
    fn terminate(&mut self) {
        // The exchange thread goes first, so nothing can read or write a
        // half-written frame while the process is being reaped.
        self.exchange = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Ask the worker to stop, and make sure that it does.
    ///
    /// Two signals go out, in this order: a `CONTROL drain` frame so the worker
    /// can flush and clean up, then end-of-file on its stdin, which is the
    /// shutdown mechanism that cannot fail — the worker's read loop returns and
    /// the process exits on its own. The frame is queued ahead of the close, so
    /// the worker sees both signals in order. Waiting for the acknowledgement
    /// would need a deadline for no gain. Killing is only the fallback for a
    /// worker that ignores both.
    fn stop(&mut self) {
        if let Some(mut exchange) = self.exchange.take() {
            // The control frame is unsolicited — no request id — and the
            // exchange thread will not return an answer, because nothing reads
            // it: the next thing to happen is the close below.
            let payload =
                <stokehold_worker::Json as stokehold_worker::Codec>::encode(&Control::Drain)
                    .expect("a unit enum serializes");
            let _ = exchange.send(MessageType::Control, UNSOLICITED, &payload, 0);
            exchange.close();
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
    exchange: ChannelExchange,
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
    /// The checksum policy to offer at handshake.
    ///
    /// On the spec rather than passed to `boot` because a *replaced* worker must be
    /// handshaken on the same terms as the one it replaces: a pool that negotiated
    /// `optional` at startup and then respawned a worker under `required` would have one
    /// worker verifying and the rest not, which is a difference nothing would report.
    checksum: Checksum,
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
            checksum: match config.checksum {
                ChecksumMode::Required => Checksum::Required,
                ChecksumMode::Optional => Checksum::Optional,
            },
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

    let channel = BoundedChannel::new(stdin, stdout);
    match handshake_with_timeout(channel, HANDSHAKE_TIMEOUT, spec.checksum) {
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

/// What the exchange thread is asked to do, and what the caller is told.
///
/// `WorkerChannel::request` and `send_frame` answer one exchange each, so the
/// `request_id` that matches a response lives inside the channel and the caller
/// only has to say which call it made. Both answer `None` for a response, because
/// the only exchange that produces one is [`Exchange::Request`] — the handshake is
/// already done by the time this thread starts.
enum Exchange {
    /// Send one request and collect its whole response.
    ///
    /// The head arrives **already encoded**. The pool's translation borrows the CGI environment —
    /// that is what `borrowed_request` is for — and a borrow cannot travel to this thread, because
    /// on a timeout the thread outlives the call that made it. So the pool serializes the head and
    /// sends the bytes, and the worker cannot tell which side of the boundary encoded them.
    Request { head: Vec<u8>, body: Vec<u8> },    /// Send one frame from an already-encoded payload.
    Send {
        message_type: MessageType,
        request_id: u32,
        payload: Vec<u8>,
        extra_flags: u16,
    },
}

impl Exchange {
    /// Run this exchange on a live channel.
    ///
    /// `on_stage` is called as the request moves from writing to reading, so a
    /// failure can be attributed to the half it happened in. Only
    /// [`Exchange::Request`] has two phases; every other exchange reports `Reading`
    /// once, because a control or a frame write has nothing to read and its result
    /// is not retried.
    fn run(
        self,
        channel: &mut Channel,
        on_stage: &mut dyn FnMut(RequestStage),
    ) -> std::result::Result<Option<WorkerResponse>, WorkerError> {
        match self {
            Exchange::Request { head, body } => {
                // The stage is reported by the channel as it moves from writing to reading, which
                // is the only place that can know where a failure happened.
                let result = channel.request_encoded(head, &body, &mut *on_stage);
                result.map(Some)
            }
            Exchange::Send {
                message_type,
                request_id,
                payload,
                extra_flags,
            } => {
                on_stage(RequestStage::Reading);
                channel.send_frame(message_type, request_id, &payload, extra_flags)?;
                Ok(None)
            }
        }
    }
}

/// A worker's pipes, and the thread that owns them.
///
/// **This is the fix for the per-request thread cost.** A pipe read has no
/// portable timeout on any platform stokehold runs on, so the deadline has to be
/// enforced by something outside the read: the worker process is killed, which is
/// what makes the read return. A thread is what can then be given up on — but it
/// does *not* have to be a new thread per request. One thread per worker, alive
/// for the worker's whole life, does the same job and is created once: a request
/// is posted to it on a channel, and the deadline is a `recv_timeout` on that
/// channel. Measured on the trivial handler this was worth about half the arm's
/// throughput (worker 20,422 rps vs `spawn_cgi` 29,950 at concurrency 32).
///
/// Two consequences, both deliberate:
///
/// - **A timeout still leaks this thread and its pipes.** The thread is blocked
///   in `read` and will not return, so nothing may be sent to it again — the
///   caller drops the whole exchange and kills the worker, and the process exit
///   is what finally ends the read. Sending a second request to a worker whose
///   first one timed out is the one thing that must not happen, and dropping the
///   exchange is what makes it impossible.
/// - **A write error means the thread has ended.** The sender is kept for the
///   whole call rather than re-sent, so nothing is delivered to a stale request's
///   answer.
#[derive(Debug)]
struct ChannelExchange {
    work: Option<Sender<Exchange>>,
    /// The thread's answers, exactly one per exchange.
    answers: mpsc::Receiver<Answer>,
    /// Set once the thread has ended, so a write to a dead thread is reported as
    /// "closed" rather than as an unexplained send failure.
    ended: bool,
}

/// The thread's answers, exactly one per exchange.
///
/// A failure carries the [`RequestStage`] it happened in, because that is what says
/// whether the request could have been delivered.
type Answer = std::result::Result<Option<WorkerResponse>, (RequestStage, WorkerError)>;

impl ChannelExchange {
    /// Start the exchange thread that owns a freshly handshaken worker's pipes.
    ///
    /// Takes ownership of the channel and never gives it back: the thread ends
    /// when the channel is dropped, which is what `close` arranges.
    pub fn start(mut channel: Channel) -> Self {
        let (work_tx, work_rx) = mpsc::channel::<Exchange>();
        let (answer_tx, answers) = mpsc::channel();

        std::thread::spawn(move || {
            while let Ok(exchange) = work_rx.recv() {
                // The stage is tracked here, in the only place that can know it: a
                // failure is stamped with the half of the request it happened in.
                // The default is the send phase, so a failure before the callback
                // fires is still attributed to the write.
                let mut stage = RequestStage::Sending;
                let result = exchange.run(&mut channel, &mut |reported| stage = reported);
                // The receiver is gone only if the caller dropped this exchange,
                // which happens on a timeout. Then the answer is dropped with the
                // thread — the caller has already decided not to wait.
                if answer_tx.send(result.map_err(|source| (stage, source))).is_err() {
                    break;
                }
            }
        });

        ChannelExchange {
            work: Some(work_tx),
            answers,
            ended: false,
        }
    }

    /// Run one request, giving up after `timeout`.
    ///
    /// One copy of the request body per request, and it is worth being honest about why:
    /// the work item is moved to the exchange thread, so it must own its bytes, and a
    /// channel of borrowed data would need a lifetime the standard library's channels
    /// cannot express. Removing that copy means `Request::body` becoming something cheap to
    /// share (`Arc<[u8]>`), which is a change to the FastCGI layer every backend shares —
    /// not worth it while the cost is one `memcpy` the previous design also paid. What is
    /// gone is the thread, which was the measured cost.
    ///
    /// The *head* is not copied by the channel either: it arrives already encoded, and the channel
    /// writes it straight out. What it costs is one `Vec` per request, and that is the price of
    /// crossing this thread boundary at all — the body copy below pays the same price for the same
    /// reason. Building the JSON is what the pool used to do, but *borrowing* the header for the
    /// worker is what it does now, and nine `String`s became one buffer.
    pub fn request(
        &mut self,
        head: Vec<u8>,
        body: &[u8],
        timeout: Duration,
    ) -> std::result::Result<WorkerResponse, (RequestStage, WorkerError)> {
        let exchange = Exchange::Request {
            head,
            body: body.to_vec(),
        };

        match self.round_trip(exchange, timeout) {
            Ok(Some(response)) => Ok(response),
            // `request` always answers with a response, so a `None` here would be
            // the thread misbehaving rather than a protocol condition.
            Ok(None) => Err((
                RequestStage::Reading,
                WorkerError::Closed(
                    "the exchange thread answered without a response".to_string(),
                ),
            )),
            Err(failure) => Err(failure),
        }
    }

    /// Write one frame, without waiting for anything to come back.
    ///
    /// The payload arrives already encoded — `CONTROL drain` is a fixed byte
    /// string — so this needs no serializer and does not force one on this crate.
    /// A control frame is unsolicited, so its id is not a request id and is not
    /// correlated: the answer is drained because the thread produces one per
    /// exchange, and leaving it in the channel would make the *next* request read
    /// it and fail for a reason that has nothing to do with the request.
    pub fn send(
        &mut self,
        message_type: MessageType,
        request_id: u32,
        payload: &[u8],
        extra_flags: u16,
    ) -> std::result::Result<(), WorkerError> {
        let exchange = Exchange::Send {
            message_type,
            request_id,
            payload: payload.to_vec(),
            extra_flags,
        };

        match self.round_trip(exchange, Duration::from_secs(1)).map_err(|(_, source)| source)? {
            // `Send` writes a frame and reads nothing, so its answer carries no
            // response and never can.
            None => Ok(()),
            Some(_) => Err(WorkerError::Closed(
                "the exchange thread answered a frame write with a response".to_string(),
            )),
        }
    }
    /// End the exchange thread by closing the worker's stdin.
    ///
    /// Dropping the sender ends the thread's `recv`, and dropping the channel then
    /// closes the pipes. This is the shutdown signal that cannot fail, and it is
    /// deliberately not a method on `WorkerChannel`: the worker sees end-of-file on
    /// its next read and exits under its own control.
    pub fn close(self) {
        let ChannelExchange { work, answers, .. } = self;
        drop(work);
        drop(answers);
    }

    /// Post one exchange and wait for its answer.
    ///
    /// Exactly one exchange is in flight per worker at a time, so the single
    /// answer on `answers` is necessarily this one's and no request id needs to
    /// travel with it to say so. That is a property of the slot's lock, and it is
    /// why this is not a general-purpose multiplexer.
    fn round_trip(
        &mut self,
        exchange: Exchange,
        timeout: Duration,
    ) -> std::result::Result<Option<WorkerResponse>, (RequestStage, WorkerError)> {
        // Fail fast once the thread is known to be gone. Without this the send below
        // would succeed — the sender is still alive, only the answer end is closed —
        // and the call would block until the timeout for a worker that cannot answer.
        if self.ended {
            return Err((
                RequestStage::Reading,
                WorkerError::Closed("the worker's exchange thread has ended".to_string()),
            ));
        }

        let Some(work) = self.work.as_ref() else {
            return Err((
                RequestStage::Sending,
                WorkerError::Closed("the worker's exchange thread has already ended".to_string()),
            ));
        };

        work.send(exchange).map_err(|_| {
            self.ended = true;
            (
                RequestStage::Sending,
                WorkerError::Closed("the worker's exchange thread has ended".to_string()),
            )
        })?;

        match self.answers.recv_timeout(timeout) {
            Ok(result) => result,
            // Timed out. The thread is blocked in a read on this worker, so the
            // caller has to kill the worker: that is what makes the read return so
            // the thread can exit and drop the pipes.
            Err(RecvTimeoutError::Timeout) => Err((RequestStage::Reading, WorkerError::Timeout(timeout))),
            Err(RecvTimeoutError::Disconnected) => {
                self.ended = true;
                Err((
                    RequestStage::Reading,
                    WorkerError::Closed(
                        "the exchange thread ended without answering".to_string(),
                    ),
                ))
            }
        }
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

/// A CGI parameter as a borrowed string, lossily for the same reason the owned path is.
///
/// `from_utf8_lossy` returns `Cow::Borrowed` for the bytes both front-ends actually produce —
/// they build every value from a `&str` — so the common path copies nothing, and a value that
/// is somehow not UTF-8 still becomes the same replacement-character string the owned
/// translation would have made.
fn param_cow<'a>(params: &'a NameValuePairs, name: &str) -> Option<Cow<'a, str>> {
    params
        .iter()
        .find(|(key, _)| key.as_slice() == name.as_bytes())
        .map(|(_, value)| String::from_utf8_lossy(value))
}

/// The request, read back out of the CGI environment without copying it.
///
/// This is the pool's translation, and the counterpart of [`request_header`]: that one owns
/// everything, this one borrows everything. Both must produce the same frame, and the pool uses
/// this one because owning a `RequestHeader` only to serialize it is nine `String`s and two
/// `Vec`s per request that are dropped before the worker has answered.
///
/// Every field is a slice of the `PARAMS` pair it came from — including the header values, whose
/// bytes a CGI variable shares with the client's header — and the one thing that is *not* a slice,
/// the `HTTP_ACCEPT_ENCODING` → `Accept-Encoding` name, is transformed while it is being
/// serialized rather than into an owned `String` first. See [`CgiHeader`].
#[doc(hidden)]
pub struct BorrowedRequest<'a> {
    method: Cow<'a, str>,
    uri: Cow<'a, str>,
    protocol: Cow<'a, str>,
    remote_addr: Cow<'a, str>,
    script_filename: Cow<'a, str>,
    https: bool,
    headers: RequestHeaderParams<'a>,
}

impl<'a> BorrowedRequest<'a> {
    /// The frame's JSON head, borrowing this translation and the environment under it.
    ///
    /// `body_len` is left at zero because the channel stamps in the length of the body it is
    /// about to frame, so the declared length and the bytes sent cannot disagree.
    #[doc(hidden)]
    pub fn draft(&self) -> RequestHeaderJson<'_, RequestHeaderParams<'_>, CgiHeader<'_>> {
        RequestHeaderJson {
            method: &self.method,
            uri: &self.uri,
            protocol: &self.protocol,
            headers: BorrowedHeaders::new(self.headers.clone()),
            remote_addr: &self.remote_addr,
            body_len: 0,
            script_filename: &self.script_filename,
            https: self.https,
        }
    }
}

/// Serialize a request head into an owned buffer.
///
/// One allocation per request, and it buys the removal of nine: the head describes the request by
/// *borrowing* the CGI environment, so the alternative is not "no buffer" but "an owned
/// `RequestHeader`" — five `String`s, two per request header, plus a `Vec` for the pairs.
///
/// The caller has already stamped `body_len` in, because this function is on the pool's side of
/// the thread boundary and the body is on the channel's side of it.
fn encode_head<'a>(
    head: &RequestHeaderJson<'a, RequestHeaderParams<'a>, CgiHeader<'a>>,
) -> Vec<u8> {
    Json::encode(head).unwrap_or_else(|err| {
        // Serializing `&str`s and `u64`s cannot fail, and a panic here would take the server down
        // for one request. An empty payload makes the worker answer with a protocol error, which is
        // reported as the backend failure it is.
        tracing::error!(%err, "the worker request head could not be serialized");
        Vec::new()
    })
}

/// Turn a FastCGI request into the worker frame's head, borrowing the environment.
///
/// The same translation as [`request_header`], with the same defaults for a missing variable,
/// and it exists so the pool does not build the owned `RequestHeader` at all.
///
/// `pub` and `#[doc(hidden)]` so `tests/request_header_allocations.rs` can measure it beside
/// `request_header`; it is not re-exported and is not part of the interface.
#[doc(hidden)]
pub fn borrowed_request(request: &Request) -> BorrowedRequest<'_> {
    let params = &request.params;

    BorrowedRequest {
        method: param_cow(params, "REQUEST_METHOD").unwrap_or(Cow::Borrowed("GET")),
        uri: param_cow(params, "REQUEST_URI").unwrap_or(Cow::Borrowed("/")),
        protocol: param_cow(params, "SERVER_PROTOCOL").unwrap_or(Cow::Borrowed("HTTP/1.1")),
        remote_addr: param_cow(params, "REMOTE_ADDR").unwrap_or(Cow::Borrowed("")),
        script_filename: param_cow(params, "SCRIPT_FILENAME").unwrap_or(Cow::Borrowed("")),
        https: match params.iter().find(|(key, _)| key.as_slice() == b"HTTPS") {
            // Present means the request arrived over TLS; CGI uses "on" and "off", and
            // the front-end only writes the former.
            Some((_, value)) => !value.eq_ignore_ascii_case(b"off") && !value.is_empty(),
            None => false,
        },
        headers: RequestHeaderParams::new(params),
    }
}

/// The request's headers, borrowed out of the CGI environment.
///
/// A CGI variable's value *is* the client's header value — `to_cgi` put those bytes there — so
/// the value needs no copy at all. The name does need one transformation, and that is deferred to
/// [`CgiHeader`], which does it into a stack buffer while the JSON is being written.
///
/// `CONTENT_TYPE` is appended last because it does not carry the `HTTP_` prefix, exactly as
/// [`headers_from_params`] appends it. `CONTENT_LENGTH` is deliberately absent from both.
#[derive(Clone)]
#[doc(hidden)]
pub struct RequestHeaderParams<'a> {
    params: std::slice::Iter<'a, (Vec<u8>, Vec<u8>)>,
    content_type: Option<&'a [u8]>,
    content_type_yielded: bool,
}

impl<'a> RequestHeaderParams<'a> {
    fn new(params: &'a NameValuePairs) -> Self {
        Self {
            params: params.iter(),
            content_type: params
                .iter()
                .find(|(key, _)| key.as_slice() == b"CONTENT_TYPE")
                .map(|(_, value)| value.as_slice()),
            content_type_yielded: false,
        }
    }
}

impl<'a> Iterator for RequestHeaderParams<'a> {
    type Item = CgiHeader<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        for (name, value) in self.params.by_ref() {
            let Ok(name) = std::str::from_utf8(name) else {
                continue;
            };
            // Not a request header — `DOCUMENT_ROOT`, `SCRIPT_FILENAME`, `HTTPS` and the rest
            // are all environment variables with their own meaning.
            let Some(suffix) = name.strip_prefix("HTTP_") else {
                continue;
            };
            // A bare `HTTP_` prefix names nothing, and `headers_from_params` drops it rather
            // than producing a header with an empty name. The two translations have to agree
            // byte for byte, so this drops it too.
            if suffix.is_empty() {
                continue;
            }
            return Some(CgiHeader {
                name,
                value: String::from_utf8_lossy(value),
            });
        }

        if std::mem::replace(&mut self.content_type_yielded, true) {
            return None;
        }
        self.content_type.map(|value| CgiHeader {
            name: "CONTENT_TYPE",
            value: String::from_utf8_lossy(value),
        })
    }
}

/// One request header, named the way the CGI environment names it.
///
/// Serializes as a two-element array, byte for byte like the `(&str, &str)` pairs the owned path
/// produces, so the worker cannot tell which translation it was sent.
#[doc(hidden)]
pub struct CgiHeader<'a> {
    name: &'a str,
    value: Cow<'a, str>,
}

impl serde::Serialize for CgiHeader<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        (HttpHeaderName(self.name), self.value.as_ref()).serialize(serializer)
    }
}

impl<'a> serde::Serialize for HttpHeaderName<'a> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        // The name is transformed here rather than before, which is what keeps it off the
        // heap: a header name is short, so it fits the stack buffer, and the common case
        // allocates nothing at all. Anything else — a name too long, or one that is not
        // ASCII, where the character-wise case folding `header_name` does differs from the
        // byte-wise kind — falls back to the owning form.
        let suffix = self.0.strip_prefix("HTTP_").unwrap_or(self.0);
        let mut stack = [0u8; HTTP_HEADER_NAME_CAPACITY];
        match render_http_header_name(suffix, &mut stack) {
            Some(name) => serializer.serialize_str(name),
            None => serializer.serialize_str(&header_name(suffix)),
        }
    }
}

/// The longest header name rendered without allocating.
///
/// Long enough for any real name — the longest registered request header is
/// `Access-Control-Request-Headers` at 30 bytes — and short enough to be free on the stack.
const HTTP_HEADER_NAME_CAPACITY: usize = 128;

/// `ACCEPT_ENCODING` → `Accept-Encoding`, into a caller-supplied buffer.
///
/// Returns `None` when the name does not fit or is not ASCII, and the caller then owns it. The
/// rendering can never be longer than the suffix: one byte of `_` becomes one byte of `-`, and
/// the case fold is in place.
fn render_http_header_name<'b>(
    suffix: &str,
    out: &'b mut [u8; HTTP_HEADER_NAME_CAPACITY],
) -> Option<&'b str> {
    if !suffix.is_ascii() || suffix.len() > out.len() {
        return None;
    }
    let mut written = 0;
    for (index, segment) in suffix.split('_').enumerate() {
        if index > 0 {
            out[written] = b'-';
            written += 1;
        }
        for (position, byte) in segment.bytes().enumerate() {
            out[written] = if position == 0 {
                byte.to_ascii_uppercase()
            } else {
                byte.to_ascii_lowercase()
            };
            written += 1;
        }
    }
    std::str::from_utf8(&out[..written]).ok()
}

/// A CGI header name, rendered as the HTTP name it stands for.
struct HttpHeaderName<'a>(&'a str);

/// Turn a FastCGI request into a worker `REQUEST` header.
///
/// The translation goes through the CGI parameters rather than the original HTTP
/// request, and that is the better direction: the front-end has already resolved
/// the path against the document root, so `SCRIPT_FILENAME` here is the file it
/// decided to run, not a second guess at it. It also means the pool needs no
/// routing of its own — one source of truth for what the request is.
///
/// The pool does **not** call this: [`borrowed_request`] is the same translation without the
/// ownership. This is kept because it is the readable statement of what the frame contains, it is
/// what the unit tests assert against, and `tests/request_header_allocations.rs` counts what it
/// costs, which is the number the borrowed path is measured against. `pub` and
/// `#[doc(hidden)]` so that test can reach it; it is not re-exported from the crate root.
#[doc(hidden)]
pub fn request_header(request: &Request) -> RequestHeader {
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

/// Whether a failed request may be sent to the backend again.
///
/// Two conditions, and both are needed. The stage decides what the failure proves
/// about whether the application ran; the verb decides whether running it twice
/// would matter. Neither alone is enough, and using the stage alone is the bug that
/// makes an application execute a `POST` twice.
fn may_retry(stage: RequestStage, request: &Request) -> bool {
    match stage {
        // The bytes never left, so there is nothing to run twice.
        RequestStage::Sending => true,
        // The request was delivered. A replay is a second execution, which is only
        // acceptable for a method that means the same thing twice.
        RequestStage::Reading => is_idempotent(request),
    }
}

/// Whether replaying this request cannot cause a second write.
///
/// HTTP defines which methods are idempotent, and that is the criterion a pool
/// retry needs. `GET`, `HEAD`, `OPTIONS` and `TRACE` are safe by definition; `PUT`
/// and `DELETE` are idempotent by definition. `POST` and `PATCH` are neither, so a
/// retried one is a second write — and the pool deliberately does not make that
/// decision for an application.
///
/// The check reads the method from the CGI parameters, which is the same place
/// `request_header` reads it, so a retry cannot disagree with the frame that was
/// sent. An absent or unrecognised method is treated as not idempotent: the safe
/// answer is to report the failure rather than to guess.
fn is_idempotent(request: &Request) -> bool {
    matches!(
        text_param(&request.params, "REQUEST_METHOD").as_deref(),
        Some("GET" | "HEAD" | "OPTIONS" | "TRACE" | "PUT" | "DELETE")
    )
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
    use stokehold_worker::{Codec, Json, ResponseHead};

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

    /// The stack-buffer rendering must agree with the owning one, name for name.
    ///
    /// `render_http_header_name` exists so a request header's name is never heap-allocated, and it
    /// is a second implementation of what `header_name` does — which is exactly the kind of pair
    /// that drifts. It also has to answer `None` rather than truncate for a name it cannot render,
    /// because a silently shortened header name would reach the application as a different header.
    #[test]
    fn the_stack_buffer_rendering_agrees_with_the_owning_one() {
        let names = [
            "HTTP_",
            "HTTP_HOST",
            "HTTP_ACCEPT_ENCODING",
            "HTTP_X_FORWARDED_FOR",
            "HTTP_CONTENT_TYPE",
            "HTTP_A",
            "HTTP_A_B_C_D",
            "HTTP_USER_AGENT",
            // Already canonical, so the fold is a no-op.
            "HTTP_Host",
        ];
        for name in names {
            let suffix = name.strip_prefix("HTTP_").unwrap_or(name);
            let mut stack = [0u8; HTTP_HEADER_NAME_CAPACITY];
            let rendered = render_http_header_name(suffix, &mut stack)
                .unwrap_or_else(|| panic!("{name} did not fit the stack buffer"));
            assert_eq!(rendered, header_name(suffix), "{name}");
        }

        // A name that cannot fit is refused rather than truncated: the rendered name can never be
        // longer than its suffix, so the capacity is the only limit.
        let long = "A".repeat(HTTP_HEADER_NAME_CAPACITY + 1);
        let mut stack = [0u8; HTTP_HEADER_NAME_CAPACITY];
        assert!(render_http_header_name(&long, &mut stack).is_none());

        // Non-ASCII would case-fold differently byte-wise than character-wise, so it goes to the
        // owning path rather than being folded here.
        let mut stack = [0u8; HTTP_HEADER_NAME_CAPACITY];
        assert!(render_http_header_name("CAFÉ", &mut stack).is_none());
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

    /// The two translations of one request must put the same bytes on the wire.
    ///
    /// The pool sends the borrowed one; `request_header` is the readable statement of what the
    /// frame contains. Two implementations of one protocol rule is how they drift, so this
    /// compares the encodings of the *whole* frame head — every field, not a selection — for
    /// environments chosen to cover the corners: no request headers, the ordinary set, a header
    /// whose value is not UTF-8, a variable that is not a request header at all, a bare `HTTP_`
    /// prefix, and `CONTENT_TYPE`, which keeps its CGI name.
    #[test]
    fn the_borrowed_translation_encodes_identically_to_the_owned_one() {
        let environments: Vec<NameValuePairs> = vec![
            // No request headers, so both paths are the fixed fields plus `CONTENT_TYPE`.
            params(&[("REQUEST_METHOD", "POST"), ("CONTENT_TYPE", "application/json")]),
            // The ordinary page: several headers, one with a multi-word name, one that is not
            // a request header, and `CONTENT_TYPE` arriving in its CGI spelling.
            params(&[
                ("REQUEST_METHOD", "POST"),
                ("REQUEST_URI", "/index.php/foo?x=1"),
                ("SERVER_PROTOCOL", "HTTP/1.1"),
                ("REMOTE_ADDR", "127.0.0.1"),
                ("SCRIPT_FILENAME", "/srv/app/index.php"),
                ("DOCUMENT_ROOT", "/srv/app"),
                ("PATH_INFO", "/foo"),
                ("HTTP_HOST", "example.test"),
                ("HTTP_ACCEPT_ENCODING", "gzip, deflate"),
                ("HTTP_X_FORWARDED_FOR", "10.0.0.1"),
                ("CONTENT_TYPE", "application/json"),
                ("CONTENT_LENGTH", "12"),
            ]),
            // Corners: a bare `HTTP_` prefix (which names nothing and must be dropped by both),
            // a non-UTF-8 value (which both must render lossily the same way), and no
            // `CONTENT_TYPE` at all.
            vec![
                (b"REQUEST_METHOD".to_vec(), b"GET".to_vec()),
                (b"HTTP_".to_vec(), b"empty-name".to_vec()),
                (b"HTTP_X_BYTES".to_vec(), vec![0x80, 0xff, b'x']),
            ],
        ];

        for environment in environments {
            let request = Request {
                params: environment,
                body: b"a body".to_vec(),
                keep_conn: true,
            };

            // `body_len` is zero on both: the channel stamps it in from the body it frames.
            let owned = Json::encode(&request_header(&request).draft(0)).expect("encode");
            let borrowed =
                Json::encode(&borrowed_request(&request).draft()).expect("encode");

            assert_eq!(
                String::from_utf8_lossy(&borrowed),
                String::from_utf8_lossy(&owned),
                "the borrowed head must encode identically to the owned one"
            );
            assert_eq!(borrowed, owned, "and byte for byte, not merely printed alike");
        }
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
            checksum: Default::default(),
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
            checksum: Default::default(),
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
            checksum: Default::default(),
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
