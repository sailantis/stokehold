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

//!
//! # How this module is laid out
//!
//! The pool's own machinery — the slots, the request path, recycling, the retry
//! policy and the lifecycle guard — is here. Everything else was split out along
//! the seams where it was already separable, and each piece is reachable through
//! this module as [`spec`], [`handshake`], [`exchange`], [`request`], [`response`],
//! so nothing moved further than the item itself:
//!
//! - [`spec`] decides *what* to run; [`handshake`] runs it and waits;
//!   [`exchange`] is the pipe and its thread.
//! - [`request`] and [`response`] are the two pure translations, and neither
//!   touches a process or a lock.


pub use request::{BorrowedRequest, CgiHeader, RequestHeaderParams};
pub use request::borrowed_request;
pub use request::request_header;

use std::process::{Child};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use stokehold_core::config::{WorkerConfig};
use stokehold_fcgi::{Request, Response};
use stokehold_worker::codec::RequestStage;
use stokehold_worker::codec::WorkerResponse;
use stokehold_worker::{Control, HelloAck, MessageType, RequestHeaderJson, WorkerError, UNSOLICITED};
use crate::error::{Result, ServerError};
use crate::lifecycle::{LifecycleGuard};
use crate::pool::worker_count;
use exchange::{ChannelExchange};
use handshake::{Ready, boot};
use request::{encode_head, text_param};
use response::{render};
use spec::{Spec};

mod exchange;
mod handshake;
mod request;
mod response;
mod spec;


/// How long a single request may take before the worker is replaced.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a worker has to exit after being told to, before it is killed.
const EXIT_TIMEOUT: Duration = Duration::from_secs(2);

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
            // Logged because the transport is invisible on the wire: a pool that ended up on
            // the wrong one answers every request with a 200, only slower, and on Windows the
            // difference is 21× — so this is the only place the choice is ever visible.
            transport = %pool.spec.transport,
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

    /// Whether each worker agreed to omit the per-frame checksum.
    ///
    /// The worker's *answer*, recorded from its `HELLO_ACK`, not the pool's configuration: a
    /// worker running an SDK that predates the option answers `false` whatever was offered, and
    /// an operator comparing this against `checksum = "optional"` is the only way to see the
    /// difference. A `false` here while the configuration says `optional` means that worker is
    /// still hashing every frame.
    pub fn no_checksum_agreed(&self) -> Vec<bool> {
        self.locked_slots()
            .iter()
            .map(|slot| slot.hello.no_checksum)
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
    /// can flush and clean up, then the close of the connection, which is the
    /// shutdown mechanism that cannot fail — the worker's read loop returns and
    /// the process exits on its own. On the default transport the close is
    /// end-of-file on the worker's stdin; on the socket transport it is the socket
    /// closing, which the worker's read sees the same way. The frame is queued ahead
    /// of the close, so the worker sees both signals in order. Waiting for the
    /// acknowledgement would need a deadline for no gain. Killing is only the
    /// fallback for a worker that ignores both.
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

/// Whether a failed request may be sent to the backend again.
///
/// Two conditions, and both are needed. The stage decides what the failure proves
/// about whether the application ran; the verb decides whether running it twice
/// would matter. Neither alone is enough, and using the stage alone is the bug that
/// makes an application execute a `POST` twice.
pub(super) fn may_retry(stage: RequestStage, request: &Request) -> bool {
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
pub(super) fn is_idempotent(request: &Request) -> bool {
    matches!(
        text_param(&request.params, "REQUEST_METHOD").as_deref(),
        Some("GET" | "HEAD" | "OPTIONS" | "TRACE" | "PUT" | "DELETE")
    )
}


#[cfg(test)]
use stokehold_fcgi::NameValuePairs;
#[cfg(test)]
pub(super) fn params(pairs: &[(&str, &str)]) -> NameValuePairs {
    pairs
        .iter()
        .map(|(name, value)| (name.as_bytes().to_vec(), value.as_bytes().to_vec()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
