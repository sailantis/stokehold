//! A pool of persistent PHP workers, spoken to over their pipes.
//!
//! This is the `worker` backend. `crates/volucer-worker` owns the codec and
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
//! **Concurrency is a lease, one per slot.** The pool takes `&self`, and a slot is only ever
//! handed out by taking it out of the pool's free list and returned when the [`Lease`] that
//! holds it drops. So a pool of `n` workers serves `n` requests at once, a request is never
//! given a slot another request is using, and a request that finds every slot taken waits for
//! one rather than for a particular worker. The protocol's one-request-at-a-time rule is per
//! *worker*, not per pool, and the lease is what enforces it now: a slot cannot be leased twice,
//! so two requests cannot reach one worker at once.
//!
//! This replaced a scan that walked the slots from an atomic cursor and locked each one until a
//! worker answered "healthy". That is a different question from "free", and it was answered
//! with the mutex released before the caller took it again — so under load several threads were
//! told the same slot was theirs and then raced for it, queueing behind one worker while others
//! idled. Measured, that cost a p99 of 16 ms against a p50 of 19 µs at concurrency 32: most
//! requests were served at once and a few waited for whole requests. What the free list buys is
//! that a wait is always for the slot the request will actually use, and what [`MAX_SLOT_WAIT`]
//! buys is that the wait cannot grow without end.

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

pub use request::borrowed_request;
pub use request::request_header;
pub use request::{BorrowedRequest, CgiHeader, RequestHeaderParams};

use crate::calls::MethodRegistry;
use crate::error::{Result, ServerError};
use crate::lifecycle::LifecycleGuard;
use crate::pool::worker_count;
use exchange::ChannelExchange;
use handshake::{boot, Ready};
use request::{encode_head, text_param};
use response::render;
use spec::Spec;
use std::process::Child;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use volucer_core::config::WorkerConfig;
use volucer_fcgi::{Request, Response};
use volucer_worker::codec::RequestStage;
use volucer_worker::codec::WorkerResponse;
use volucer_worker::message::MetricRecord;
use volucer_worker::{Control, HelloAck, MessageType, RequestHeaderJson, WorkerError, UNSOLICITED};

mod exchange;
mod handshake;
mod request;
mod response;
mod spec;

/// How long a single request may take before the worker is replaced.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a worker has to exit after being told to, before it is killed.
const EXIT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a request may wait for a slot before it is refused.
///
/// Not `None`. A bounded wait is what the tail measurement asked for, but *refusing* as soon
/// as every slot is busy would answer a full pool with a 503 — which is a throughput
/// regression on any workload whose concurrency exceeds `workers`, and the measurement that
/// led here showed the pool reaching 19,000 rps at c=32 with 12 workers. Twelve workers
/// cannot serve 32 clients without someone waiting, so the choice is not whether to queue but
/// how to queue fairly and how far to let the queue grow.
///
/// So the queue is fair and this is its bound. A request that waits longer than this is
/// refused with `503`, which is the point at which waiting has stopped being queueing and
/// started being a latency leak: it caps both queue depth and the latency a client can see.
/// The bound is deliberately generous relative to a request's cost — `GET /items` takes about
/// 600 µs, so half a second is roughly eight hundred requests of queue, which is far past any
/// depth this pool produces in practice. It exists to stop an unbounded tail, not to shape
/// ordinary load.
const MAX_SLOT_WAIT: Option<Duration> = Some(Duration::from_millis(500));

/// How many buckets a slot wait is counted into, for reading a percentile out of it.
///
/// Twenty covers 1 microsecond to a second in powers of two, which is the whole range a
/// slot wait can occupy: below a microsecond it is measurement noise, and above a second
/// the request has hit `REQUEST_TIMEOUT` and been replaced. Powers of two rather than
/// even steps because the question is "is a tail of milliseconds real", and a bucket
/// width of a millisecond cannot resolve the microseconds a healthy wait lives at.
const WAIT_BUCKETS: usize = 20;

/// The bucket a wait of `micros` belongs in.
///
/// Never negative and never zero-valued, so the index is `floor(log2)` of the wait.
/// `micros == 0` is counted in bucket 0 with the sub-microsecond waits rather than
/// discarded: a request that waited nothing is data too, and dropping it would make every
/// percentile a percentile of the waits that happened to be long enough.
fn wait_bucket(micros: u64) -> usize {
    let value = micros.max(1);
    let floor_log2 = 63 - value.leading_zeros() as usize;
    floor_log2.min(WAIT_BUCKETS - 1)
}

/// The largest wait a bucket can hold, in microseconds.
///
/// A bucket's upper edge is what is reported, and reading a percentile from a histogram
/// always means choosing an edge: this one never understates the bucket, which is the
/// direction that matters when the number is being used to decide whether a tail is real.
fn bucket_upper_edge(index: usize) -> u64 {
    1u64 << (index + 1)
}

/// The pool's free slots, and how many requests are owed one.
///
/// The two live under one mutex because they have to be read together: a slot on the list that
/// a waiter has already been woken for is not available to a request arriving now, and that is
/// only knowable if the count and the list are consistent.
#[derive(Debug, Default)]
struct FreeSlots {
    ids: Vec<usize>,
    /// Requests parked waiting for a slot.
    ///
    /// Counted so an arriving request can tell whether the next release is already owed to
    /// someone. Waking a waiter and that waiter re-acquiring the mutex are not atomic, so
    /// without this a later request can barge into the gap and take the slot the woken waiter
    /// was told about — the classic barging problem, and under sustained load it is what would
    /// preserve a tail: the requests that queue would be the ones repeatedly overtaken, which
    /// is the unfairness this change exists to remove.
    waiters: usize,
}

impl FreeSlots {
    /// The slot a request arriving now may take.
    ///
    /// `None` while any request is parked: the next release is owed to one of them, and giving
    /// it to a newcomer instead would leave the parked request waiting for a slot that has
    /// already been handed out.
    fn take_available(&mut self) -> Option<usize> {
        if self.waiters > 0 {
            return None;
        }
        self.ids.pop()
    }

    /// The slot a parked request takes when it is woken.
    ///
    /// Identical to [`FreeSlots::take_available`] except that it does not check the waiter
    /// count — a parked request *is* the waiter that count is about. Keeping them separate is
    /// what makes the barging rule legible: the only thing distinguishing a newcomer from a
    /// request that has been waiting is which of these two it is allowed to call.
    fn take_woken(&mut self) -> Option<usize> {
        self.ids.pop()
    }

    /// Whether `index` is on the list.
    fn contains(&self, index: usize) -> bool {
        self.ids.contains(&index)
    }
}

/// A slot handed to one request for as long as it serves it.
///
/// The fairness mechanism, and therefore the reason a request cannot be served on a slot
/// another request is using: a lease can only be obtained for a slot sitting in the pool's
/// free list, and holding it is what keeps it out of that list. No request has to be
/// *told* a slot is free, which was the old scan's job and its defect — it asked whether a
/// worker was healthy, took the mutex to find out, and left a window in which two requests
/// could both be told the same slot was theirs.
///
/// `Drop` is what returns the slot, and that is deliberate: the request path has several
/// early returns — a protocol error, a worker that died mid-request — and a release that
/// had to be written at each of them is a release that will eventually be missed. A leaked
/// slot is not a lost request, it is a pool that silently shrinks.
///
/// The queue is fair, and it is bounded by [`WorkerPool::max_wait`]: a request that waits
/// longer than the bound is refused with `503` rather than parked indefinitely, because an
/// unbounded wait is what made the p99 grow without limit in the first place.
struct Lease<'a> {
    pool: &'a WorkerPool,
    index: usize,
}

impl<'a> Lease<'a> {
    fn index(&self) -> usize {
        self.index
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.pool.release(self.index);
    }
}

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
    /// The slots no request is using, as a stack: the head is taken first.
    ///
    /// A `Mutex<Vec<usize>>` rather than a channel because the common case is "take one and
    /// go" — an uncontended mutex plus a `Vec::pop` is a few nanoseconds and allocates
    /// nothing — and the uncommon case is "wait for one", which the paired `Condvar` covers
    /// without a second structure.
    ///
    /// The list is what makes the queue fair, and that is the whole fix: a request waits for
    /// *the slot it will actually use*, never for one that another request has already been
    /// given. The old scan could not make that promise, which is why a request could be told
    /// a slot was free, lose it in the window before taking it, and then queue behind one
    /// worker while eleven sat idle. A released slot goes straight back to the head of this
    /// list, so it is available to the next request immediately rather than reserved for
    /// whichever waiter has been waiting longest — what matters is that the slot is *free
    /// and unused*, not which order the waiting requests are in.
    free: Mutex<FreeSlots>,
    /// Woken when a slot is released, so a request that is waiting for one is told rather
    /// than left to poll.
    ///
    /// Only used when [`WorkerPool::max_wait`] is set: the default refuses instead of
    /// waiting, so on a default pool this is never waited on and the fast path never touches
    /// it.
    released: Condvar,
    /// How long a request may wait for a slot before it is refused, or `None` to be
    /// refused the moment no slot is free.
    ///
    /// Some bound by default; see [`MAX_SLOT_WAIT`] for why it is not `None` and why it is
    /// not zero. The queue this bounds is *fair* — the fairness is the free list, not this
    /// number — so what is left for the number to decide is only how much latency is
    /// acceptable before a 503 is the more honest answer.
    max_wait: Option<Duration>,
    /// What waiting for each slot has cost, in microseconds, summed over the requests
    /// that had to wait.
    ///
    /// Per slot rather than one pool-wide total, because the question is never "is
    /// the pool contended" but "is one slot being fought over". A single total could
    /// not tell that apart from a cost spread evenly over the pool, and the shape is
    /// the whole answer to whether the slot assignment is what a slow tail is made of.
    ///
    /// `Relaxed` throughout: these are counters for a human to read, nothing in the
    /// program branches on them, and ordering them would put a fence on the request
    /// path to make a diagnostic look tidier.
    wait_us: Vec<AtomicU64>,
    /// Every slot wait, counted into powers of two, so a percentile can be read without
    /// keeping a sample per request.
    ///
    /// A mean alone cannot answer the question this exists for. Two pools with the same
    /// mean wait are not the same pool: one may give every request a fair share, and the
    /// other may serve most requests quickly and starve a few, and only the second has a
    /// tail. The shape is the finding.
    ///
    /// Boxed for size rather than for speed. Twenty counters inline make the pool large
    /// enough that `Backend::Worker` would dominate its own enum - a lint, and a real one:
    /// the enum is moved around on the startup path, not per request. One allocation, once
    /// per pool, in exchange for not paying for the counters on every move.
    wait_buckets: Box<[AtomicU64]>,
    /// Requests served, so a total can be read as a mean. Counts *attempts*, so a
    /// request the pool replayed on a second slot counts twice.
    served: AtomicU64,
    /// `memory_limit_mb` in bytes, worked out once.
    memory_limit_bytes: Option<u64>,
    max_requests: Option<u64>,
    /// Metrics the workers themselves have emitted, on their way to the registry.
    ///
    /// **A queue between the worker and the scrape, and that indirection is the whole design.** A
    /// worker's `METRIC` frame arrives mid-request, on the exchange thread, holding no reference to
    /// any registry — and it must not: the frame layer would then know what a metric is, and the
    /// registry would become reachable from a thread that exists to shuffle bytes. So the pool
    /// accumulates, and the next scrape drains. The alternative, publishing on arrival, would need the
    /// registry threaded through the exchange, which is the coupling `volucer-metrics` exists to avoid.
    ///
    /// A plain `Mutex<Vec>`: the write is once per worker per metric and the read is once per scrape,
    /// so neither side is on a hot path. Drained rather than copied, so a long-running deployment does
    /// not grow this without bound — a worker that emits a per-request metric is bounded by the requests
    /// it serves, but a worker that emitted one per loop iteration would not be.
    pending_metrics: Mutex<Vec<MetricRecord>>,
    /// Held for the life of the pool; dropping it reaps every worker.
    guard: LifecycleGuard,
    /// What this pool's workers may call, or `None` to grant nothing.
    ///
    /// **Held by the pool and not by the caller**, because a worker is replaced many times over a
    /// deployment's life — on `max_requests`, on a memory threshold, on a failed request, on an
    /// operator's `worker.reset` — and every one of those goes through the same `boot`. A registry that
    /// had to be handed in again at each replacement is a registry that would eventually be forgotten at
    /// one of them, and the symptom would be one recycled worker in a pool that silently cannot call
    /// anything while its siblings can.
    call_handler: Option<Arc<MethodRegistry>>,
}

impl WorkerPool {
    /// Start a pool from configuration.
    ///
    /// Every worker is booted and handshaken here, so a worker that cannot start
    /// is reported at startup rather than on the first request that lands on it.
    ///
    /// This is [`WorkerPool::start_with`] with no call handler: the pool's workers may call nothing,
    /// which is what a backend with no `services` gets.
    pub fn start(name: &str, config: &WorkerConfig) -> Result<Self> {
        Self::start_with(name, config, None)
    }

    /// Start a pool whose workers may make calls.
    ///
    /// The same pool as [`WorkerPool::start`] with a method registry installed on every worker's
    /// channel, including those started later to replace a recycled one. Two constructors rather than
    /// one taking an `Option`, because the distinction they draw is not "configured or not" but
    /// **capability or no capability**, and a caller that has to write `None` at the call site has to
    /// think about which one it means. `docs/plugins.md` §4.5 is the argument for keeping that decision
    /// visible.
    pub fn start_with(
        name: &str,
        config: &WorkerConfig,
        call_handler: Option<Arc<MethodRegistry>>,
    ) -> Result<Self> {
        let spec = Spec::from_config(name, config)?;

        let guard = LifecycleGuard::new(&format!("volucer-{name}")).map_err(|source| {
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
            // behind. That is why the booted workers are collected as `Slot`s and not
            // as the `Ready`s `boot` returns: `Slot::drop` reaps the process and
            // `Ready` has no `Drop` at all, so a vector of `Ready`s would leak every
            // PHP child this loop had already started.
            slots.push(Mutex::new(Slot::from_boot(boot(
                name,
                &spec,
                &guard,
                call_handler.as_ref(),
            )?)));
        }

        let pool = WorkerPool {
            name: name.to_string(),
            spec,
            // Reversed so that taking from the head serves slot 0 first: a cold pool's
            // traffic then lands in index order, which makes a log or a diagnostic read
            // the way it was configured.
            free: Mutex::new(FreeSlots {
                // Reversed so that taking from the head serves slot 0 first: a cold pool's
                // traffic then lands in index order, which makes a log or a diagnostic read
                // the way it was configured.
                ids: (0..slots.len()).rev().collect(),
                waiters: 0,
            }),
            released: Condvar::new(),
            max_wait: MAX_SLOT_WAIT,
            slots,
            wait_us: (0..count as usize).map(|_| AtomicU64::new(0)).collect(),
            wait_buckets: (0..WAIT_BUCKETS).map(|_| AtomicU64::new(0)).collect(),
            served: AtomicU64::new(0),
            memory_limit_bytes: config
                .memory_limit_mb
                .map(|mb| mb.saturating_mul(1024 * 1024)),
            max_requests: config.max_requests,
            pending_metrics: Mutex::new(Vec::new()),
            guard,
            call_handler,
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

    /// The mean time a request waited for a slot, in microseconds.
    ///
    /// The request path times each request from the moment it asks for a slot to the
    /// moment it holds one, and adds that up here; the mean is over the requests this
    /// backend served, not over the slots, so it is comparable with the client-side
    /// figures a load generator reports. It is the queueing half of a request's latency:
    /// a slot is held for the whole request, so this is time spent behind another
    /// request rather than any time the worker spent working.
    ///
    /// Its floor is not zero: the measurement spans the scan as well as the serving
    /// lock, because a request can wait in either, so every request contributes the cost
    /// of a lock taken and released to ask a question - microseconds, against the
    /// milliseconds a real queue is made of.
    ///
    /// Read at shutdown and reported in the log. Near zero on a pool whose workers were
    /// never contended for, which is the answer that matters: it is what says a slow
    /// tail belongs to the front-end or the load generator rather than to the pool.
    ///
    /// Its floor is not zero. The measurement spans the scan as well as the serving
    /// lock, because a request can wait in either, so every request contributes the cost
    /// of a lock taken and released to ask a question — microseconds, against the
    /// milliseconds a real queue is made of. A figure of a few microseconds on an idle
    /// pool is that cost, not contention.
    pub fn mean_slot_wait_us(&self) -> u64 {
        let total: u64 = self
            .wait_us
            .iter()
            .map(|us| us.load(Ordering::Relaxed))
            .sum();
        let served = self.served.load(Ordering::Relaxed);
        total.checked_div(served).unwrap_or(0)
    }

    /// The largest wait that at most `fraction` of requests exceeded, in microseconds.
    ///
    /// The pool's own p99, or p50, or whatever fraction is asked for - the counterpart of
    /// the client's figure, measured inside the pool so the two can be compared without a
    /// load generator in between. A histogram edge rather than an exact sample, so the
    /// answer is never smaller than the truth; see [`bucket_upper_edge`].
    pub fn slot_wait_percentile_us(&self, fraction: f64) -> u64 {
        let served = self.served.load(Ordering::Relaxed);
        if served == 0 {
            return 0;
        }
        let counts: Vec<u64> = self
            .wait_buckets
            .iter()
            .map(|bucket| bucket.load(Ordering::Relaxed))
            .collect();
        let mut cumulative = 0u64;
        for (index, count) in counts.iter().enumerate() {
            cumulative += count;
            if cumulative as f64 >= fraction * served as f64 {
                return bucket_upper_edge(index);
            }
        }
        bucket_upper_edge(WAIT_BUCKETS - 1)
    }

    /// Requests this pool attempted, which is the denominator of the means above.
    pub fn requests_served(&self) -> u64 {
        self.served.load(Ordering::Relaxed)
    }

    /// The slot with the most wait attributed to it, and that wait in microseconds.
    ///
    /// The counterpart of [`WorkerPool::mean_slot_wait_us`]: a mean hides the shape
    /// this exists to detect, where one slot takes the queueing and the rest are idle.
    pub fn busiest_slot_wait_us(&self) -> (usize, u64) {
        self.wait_us
            .iter()
            .enumerate()
            .map(|(index, us)| (index, us.load(Ordering::Relaxed)))
            .max_by_key(|(_, us)| *us)
            .unwrap_or((0, 0))
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

    /// This pool's workers, one entry each, for `workers.list`.
    ///
    /// `backend` is left empty and filled in by the caller that knows what this pool is called — the
    /// dispatcher — because a pool does not know its own configured name in the sense a list needs: it
    /// has one, but it is a label for logs, and a status entry whose backend came from the pool could
    /// disagree with the key the dispatcher indexed it under.
    ///
    /// # `try_lock`, never `lock`, and why that is not a shortcut
    ///
    /// **This method is reachable from a call, and a call is served on the exchange thread of a worker
    /// whose request is in flight** — which holds that worker's slot lock for the whole of the request.
    /// So a `workers.list` that took each slot lock in turn would block on the caller's *own* slot,
    /// waiting for a lock the caller is holding. That is not a theoretical race: it is what the first
    /// end-to-end test of the call plane did, hanging until the 30-second request deadline killed the
    /// worker with no hint of which call was to blame. It is also why `locked_slots`'s "the lock is never
    /// held across a request" is no longer true — a call runs *inside* one.
    ///
    /// `try_lock` turns that into an answer. A contended slot is one that is serving a request right now,
    /// which is precisely what `idle: false` means, so it is described correctly rather than skipped or
    /// waited for. Its counters are behind the lock the request holds and are reported as zero, which is
    /// the honest value for "not observable from here" — and they are not actionable anyway: a worker
    /// serving a request cannot be reset, `WorkerPool::restart` refuses it with `AllBusy`.
    ///
    /// A poisoned lock is reported the same way. It should not happen — the only panic inside a slot is
    /// caught by `catch_unwind` in the codec — and a pool whose locks are poisoned is not one to hand
    /// work to, so "not idle, nothing known" is the safer of the two available answers.
    pub fn worker_status(&self) -> Vec<crate::calls::WorkerStatus> {
        // The free list is read **once**, before the slots are, so one answer describes one instant: a
        // slot cannot be described as idle here and taken a microsecond later by a request that this
        // list never saw.
        let free = self
            .free
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .ids
            .clone();
        self.slots
            .iter()
            .enumerate()
            .map(|(index, slot)| {
                let unobserved = crate::calls::WorkerStatus {
                    backend: String::new(),
                    index,
                    pid: 0,
                    idle: false,
                    served: 0,
                    memory_bytes: 0,
                    runtime: String::new(),
                };
                match slot.try_lock() {
                    Ok(slot) => crate::calls::WorkerStatus {
                        pid: slot.child.id(),
                        idle: free.contains(&index),
                        served: slot.served,
                        memory_bytes: slot.peak_memory_bytes,
                        runtime: format!("{} ({})", slot.hello.php_version, slot.hello.sapi),
                        ..unobserved
                    },
                    Err(_) => unobserved,
                }
            })
            .collect()
    }

    /// Every slot, locked in turn.
    ///
    /// The lock is held only long enough to read a field, so this is safe from any thread that is *not*
    /// serving one of these workers. It is **not** safe from the exchange thread: a call is handled there
    /// while its request holds the slot lock, so a call that reached this would wait for itself; see
    /// [`WorkerPool::worker_status`] for the `try_lock` version the call path uses.
    fn locked_slots(&self) -> Vec<std::sync::MutexGuard<'_, Slot>> {
        self.slots
            .iter()
            .map(|slot| slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
            .collect()
    }

    /// Whether this pool has at least one worker able to take a request.
    ///
    /// **A pool is ready when a slot is free, not when every worker is idle.** The distinction
    /// matters because a worker pool is built by starting every process and waiting for each to
    /// finish its handshake, and that takes as long as the application takes to boot — hundreds of
    /// milliseconds for a framework, more for one that opens database connections at boot. So a
    /// readiness probe that waited for all of them would hold traffic back long after the first
    /// worker could have answered, while one that answered immediately would send traffic to a pool
    /// that has nothing to serve it with.
    ///
    /// A free slot is exactly the condition the request path needs, which is why this asks it that
    /// way rather than counting handshakes: `free_slots` is already what `pick_slot` waits on, and a
    /// second, subtly different notion of "available" is how a readiness probe and a dispatcher come
    /// to disagree about whether a deployment is up.
    pub fn is_ready(&self) -> bool {
        self.free_slots() > 0
    }

    /// Publish this pool's gauges into a metrics registry.
    ///
    /// One call at one moment, so the numbers a scrape reads are one picture of the pool: the parts
    /// sum to the whole by construction. `total_workers` is written even when a pool has no workers
    /// yet, because "zero workers" is the answer a graph needs rather than a missing series.
    ///
    /// The per-worker figures come from the slots, which is why this holds every lock in turn — the
    /// same thing `pids` and `peak_memory_bytes` do, and safe for the same reason: no lock is held
    /// across a request.
    pub fn publish_metrics(&self, registry: &volucer_metrics::Registry) {
        // What the workers themselves emitted, first — so a sample from a worker that has since been
        // recycled still reaches the graph, and so this drain happens even if the pool turns out to
        // have no slots to describe below.
        //
        // Drained rather than copied, and that bounds the queue: a worker emitting one sample per
        // request is bounded by the requests it serves, but one emitting a sample per loop iteration
        // is not, and a queue that only ever grows would be a leak wearing a feature's name. Drained
        // *before* the registry is written to, so a sample is never counted twice by two concurrent
        // scrapes — the `Mutex` makes the drain atomic, and the second scrape finds an empty queue.
        let samples: Vec<(String, f64)> = {
            let mut pending = self
                .pending_metrics
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *pending)
                .into_iter()
                .map(|record| (record.name, record.value))
                .collect()
        };
        for (name, value) in samples {
            // An undeclared name is refused by the registry and counted there, which is the right place
            // for that answer: whether a name is known is a property of the registry, not of the pool
            // that forwarded it.
            let _ = registry.observe(&name, &[], value);
        }

        // The free list is the pool's own answer to "which slots are available", so both the count
        // and the per-worker state are read from it rather than inferred from a second flag that
        // could disagree with it. Taken and released immediately: this is a reading, not a lease.
        //
        // A `HashSet` rather than a `Vec::contains`, because this runs once per scrape and the
        // membership test is inside a per-slot loop — quadratic on a pool large enough that the
        // numbers matter.
        let free_ids: std::collections::HashSet<usize> = {
            let free = self
                .free
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            free.ids.iter().copied().collect()
        };
        let idle = free_ids.len();

        let slots = self.locked_slots();
        let total = slots.len();
        let working = total.saturating_sub(idle);
        let mut workers = Vec::with_capacity(total);
        for (index, slot) in slots.iter().enumerate() {
            // A slot is either in the free list or leased by a request. There is no third state,
            // because a worker that fails is replaced rather than parked.
            let state = if free_ids.contains(&index) {
                volucer_metrics::http::state::READY
            } else {
                volucer_metrics::http::state::WORKING
            };
            workers.push(volucer_metrics::http::WorkerSnapshot {
                pid: slot.child.id(),
                state,
                // From the worker's own `END` frame, so it is the peak it reported and zero until it
                // has served something. Reporting the pool's own resident set instead would be a
                // different number wearing the same name.
                memory_bytes: slot.peak_memory_bytes,
            });
        }

        volucer_metrics::http::set_pool(
            registry,
            &volucer_metrics::http::PoolSnapshot {
                total_workers: total,
                workers_ready: idle,
                workers_working: working,
                // Nothing here can be "invalid": the pool replaces a worker that fails rather than
                // keeping it in the rotation, so a slot in the list is one that is expected to serve.
                // Reported as zero rather than omitted, because a dashboard that queries this wants a
                // line, and the absence of a line is indistinguishable from a broken exporter.
                workers_invalid: 0,
                workers,
            },
        );
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
        // Timed from asking for a slot to holding one, which is now a single operation:
        // the lease is *taken* if a slot is free and *waited for* if the pool is full, so
        // this measures queueing and nothing else. There is no second acquisition to fold
        // in any more — the old path locked a slot in the scan and then locked the same
        // slot again to serve, and the wait lived in whichever of the two happened to
        // block.
        let queued_at = Instant::now();
        let lease = self.acquire().map_err(|source| FailedAttempt {
            source,
            // Nothing was sent, so nothing can be repeated: a full pool is a capacity
            // condition, not a lost request.
            retryable: false,
        })?;
        let index = lease.index();
        let waited_us = queued_at.elapsed().as_micros() as u64;
        if let Some(counter) = self.wait_us.get(index) {
            counter.fetch_add(waited_us, Ordering::Relaxed);
        }
        if let Some(bucket) = self.wait_buckets.get(wait_bucket(waited_us)) {
            bucket.fetch_add(1, Ordering::Relaxed);
        }
        self.served.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();

        // The lease is held for everything below and released when it drops, so this slot
        // cannot be handed to another request until this one is finished with it.
        let mut slot = self.slots[index]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // A leased slot can still hold a dead worker: one that exited since its last
        // request, or that was killed, leaves its pipes at end-of-file and would look
        // healthy to anything that only inspected the exchange. The process is the only
        // thing that answers the question, which is why `is_available` asks it. The check
        // lives here rather than in the slot search now — the search is about *which* slot
        // to use, and whether its worker is alive is not something the caller can act on by
        // choosing a different one.
        if !slot.is_available() {
            tracing::warn!(backend = %self.name, slot = index, "a worker died; replacing it");
            let outcome = respawn(
                &self.name,
                &self.spec,
                &self.guard,
                self.call_handler.as_ref(),
                &mut slot,
            );
            drop(slot);
            // Always retryable, and for a different reason than `invoke`'s failures are:
            // nothing was sent to this worker, so a replay cannot run anything twice whatever
            // the method is. The request simply has to be offered to a slot with a live
            // worker in it, which is what the retry does. A `respawn` that failed is retried
            // for the same reason and may land on a slot that is healthy.
            return Err(FailedAttempt {
                source: match outcome {
                    Ok(()) => ServerError::NoWorker {
                        backend: self.name.clone(),
                    },
                    Err(source) => source,
                },
                retryable: true,
            });
        }

        let response = match invoke(&mut slot, request) {
            Ok(response) => response,
            Err(AttemptError { source, retryable }) => {
                // `invoke` has already decided the worker's fate: a fatal
                // failure dropped its exchange, a recoverable one put it back. So
                // a slot without an exchange is a slot that needs a replacement.
                if slot.exchange.is_none() {
                    replace(
                        &self.name,
                        &self.spec,
                        &self.guard,
                        self.call_handler.as_ref(),
                        &mut slot,
                    );
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

        // Whatever the worker said about itself on the way to answering, kept for the next scrape.
        // Taken here rather than at the scrape, because the response is about to be consumed by
        // `render` and the records live on it — and because a worker that is recycled before the next
        // scrape would otherwise take its samples with it, which is precisely when they are
        // interesting.
        if !response.metrics.is_empty() {
            let mut pending = self
                .pending_metrics
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            pending.extend(response.metrics.iter().cloned());
        }

        let duration = started.elapsed();
        self.after_response(&mut slot, &response);
        Ok(render(response, duration))
    }

    /// Replace the worker in a slot, for an operator or a test.
    ///
    /// The slot must be idle, which the lease is what establishes: taking it is the only way
    /// to know the slot is not mid-request, because the mutex that guards a slot is also held
    /// for the whole of a request. An operator asking to restart a worker that is currently
    /// working gets `AllBusy` rather than a hang or a replacement swapped out from under a
    /// live request.
    ///
    /// # This can be reached from a call, and it blocks for as long as a worker takes to boot
    ///
    /// `worker.reset` is a call, so this runs **on the exchange thread** of whichever worker asked —
    /// while that worker's request is in flight, and while its slot lock is held. `respawn` boots a PHP
    /// process and waits for its handshake, bounded by `HANDSHAKE_TIMEOUT` and in practice the
    /// application's boot time. So a worker that resets a sibling stalls its *own* request for that long,
    /// and if the stall outlasts the request's deadline the pool kills it — the concrete instance of the
    /// hazard in `docs/plugins.md` §4.2.
    ///
    /// Not fixed here, and deliberately not papered over with a shorter timeout: what a call's deadline
    /// should be is open decision §10 item 9, and picking one here would decide the policy by accident. What
    /// *is* settled is that it cannot deadlock — `take_specific` only returns a slot that is in the free
    /// list, so the only lock taken below is one that no request holds.
    pub fn restart(&self, index: usize) -> Result<()> {
        let Some(slot) = self.slots.get(index) else {
            return Err(ServerError::NoWorker {
                backend: self.name.clone(),
            });
        };
        if self.take_specific(index).is_none() {
            return Err(ServerError::AllBusy {
                backend: self.name.clone(),
            });
        }

        let mut slot = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let outcome = respawn(
            &self.name,
            &self.spec,
            &self.guard,
            self.call_handler.as_ref(),
            &mut slot,
        );
        drop(slot);
        self.release(index);
        outcome
    }

    /// Stop every worker, waiting for each to exit.
    ///
    /// `&mut self` because stopping means emptying the pool, and only the owner
    /// may do that. Nothing on the request path needs it: the server drops the
    /// pool, and `Drop` calls this.
    ///
    /// The free list is cleared *before* the workers are stopped, so a lease dropped during
    /// the shutdown — there is none on a synchronous path, but a `Drop` must not be the
    /// thing that decides this — cannot push an index into a list whose slots no longer
    /// exist.
    pub fn shutdown(&mut self) {
        self.free
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .ids
            .clear();
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

    /// Take a slot, or refuse the request once `max_wait` is up.
    ///
    /// This is the whole slot-assignment mechanism, and it is a fair queue over a free list
    /// rather than a scan. The difference is not stylistic: the old scan asked each candidate
    /// slot whether its worker was *healthy*, which is not the same question as whether the
    /// slot was *free*, and it released the mutex before the caller took it again — so under
    /// load several threads were told the same slot was theirs and then raced for it,
    /// queueing behind one worker while others sat idle. A slot only leaves the free list
    /// when it is leased and only returns when that lease drops, so two requests cannot be
    /// given the same one, and a request waits for the slot it was given rather than for
    /// whichever worker the cursor happened to point at.
    ///
    /// The waiter's own lock is held across the whole wait loop, which is what makes a
    /// release and a wake-up one step rather than two: `release` cannot take the free list to
    /// push a slot until this thread is parked, so a slot cannot be pushed while the waiter is
    /// between checking and waiting, and no wake-up can be lost.
    fn acquire(&self) -> Result<Lease<'_>> {
        if self.slots.is_empty() {
            return Err(ServerError::NoWorker {
                backend: self.name.clone(),
            });
        }

        // Fast path: take what is free, but only if no parked request is already owed it.
        // Without that condition a constant trickle of new arrivals would take every released
        // slot before a woken waiter could re-acquire the mutex, and the requests that queued
        // would be the ones starved — a tail kept alive by the fix itself.
        if let Some(index) = self.take_free() {
            return Ok(Lease { pool: self, index });
        }

        // No slot is free, so this request is about to be charged for someone else's work.
        // That is unavoidable — twelve workers cannot serve thirty-two clients — and the only
        // question is how long it is allowed to be charged for.
        let Some(limit) = self.max_wait else {
            return Err(ServerError::AllBusy {
                backend: self.name.clone(),
            });
        };

        let deadline = Instant::now() + limit;
        let mut free = self
            .free
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        free.waiters += 1;
        loop {
            if let Some(index) = free.take_woken() {
                free.waiters -= 1;
                drop(free);
                return Ok(Lease { pool: self, index });
            }
            let now = Instant::now();
            if now >= deadline {
                // Waiting has stopped being queueing and started being a leak. A 503 is the
                // honest answer, and it is what bounds the tail.
                free.waiters -= 1;
                return Err(ServerError::AllBusy {
                    backend: self.name.clone(),
                });
            }
            let (guard, _timeout) = self
                .released
                .wait_timeout(free, deadline.saturating_duration_since(now))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            free = guard;
        }
    }

    /// Take a free slot, if one is free *and* not already owed to a parked request.
    fn take_free(&self) -> Option<usize> {
        self.free
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take_available()
    }

    /// Take `index` if it is free, for [`WorkerPool::restart`].
    fn take_specific(&self, index: usize) -> Option<usize> {
        let mut free = self
            .free
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let at = free.ids.iter().position(|free| *free == index)?;
        Some(free.ids.swap_remove(at))
    }

    /// How long a request may wait for a slot, or `None` for "refuse at once".
    pub fn max_wait(&self) -> Option<Duration> {
        self.max_wait
    }

    /// How long a request may wait for a slot before it is refused, or `None` to refuse as
    /// soon as none is free.
    ///
    /// The queue is fair whatever this is set to; this decides only how deep it may get.
    /// `None` is the strict policy — a request that cannot be served now is answered `503`
    /// rather than made to wait — and a duration caps how much queueing latency a client can
    /// be charged for.
    pub fn set_max_wait(&mut self, limit: Option<Duration>) -> &mut Self {
        self.max_wait = limit;
        self
    }

    /// How many slots are free right now.
    ///
    /// For tests and diagnostics: it is the one number that says the free list agrees with
    /// reality, which is the invariant a leak would break.
    pub fn free_slots(&self) -> usize {
        self.free
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .ids
            .len()
    }

    /// Put a slot back, and wake one request waiting for one.
    ///
    /// Called from [`Lease::drop`] and from [`WorkerPool::restart`], and from nowhere else: the
    /// free list is the pool's invariant, and every path that returns a slot goes through here
    /// so the list cannot drift out of step with the slots that exist.
    ///
    /// The wake-up is unconditional once a slot is on the list, including when one *already*
    /// was — a released slot means any parked request can now be satisfied, and the one that
    /// wakes will find the list non-empty whether or not this release is the one it takes.
    fn release(&self, index: usize) {
        {
            let mut free = self
                .free
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if free.contains(index) {
                // A slot released twice would be handed to two requests at once, which is the
                // exact failure this whole mechanism exists to prevent. Reported loudly rather
                // than silently doubled: it is a bug in the caller, not a condition to
                // tolerate.
                tracing::error!(backend = %self.name, slot = index, "a slot was released twice");
                return;
            }
            free.ids.push(index);
        }
        self.released.notify_one();
    }

    /// Decide what a completed response means for the worker that produced it.
    ///
    /// A method rather than a free function because it needs six things that all live on the pool — the
    /// name, the spec, the guard, the call handler and both recycle thresholds — and passing them one by
    /// one is a signature a reader has to check against the call site rather than trust.
    fn after_response(&self, slot: &mut Slot, response: &WorkerResponse) {
        let name = self.name.as_str();
        slot.served += 1;
        slot.peak_memory_bytes = response.end.peak_memory_bytes;

        let mut recycle = if response.end.keep_alive {
            None
        } else {
            Some("the worker asked to be recycled")
        };

        if recycle.is_none() {
            if let Some(max) = self.max_requests {
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
            if let Some(limit) = self.memory_limit_bytes {
                if slot.peak_memory_bytes > limit {
                    recycle = Some("memory_limit_mb");
                }
            }
        }
        if let Some(reason) = recycle {
            tracing::debug!(backend = %name, pid = slot.child.id(), reason, "recycling a worker");
            replace(
                name,
                &self.spec,
                &self.guard,
                self.call_handler.as_ref(),
                slot,
            );
        }
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.shutdown();
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

/// Replace the worker in a slot, keeping the pool's shape.
///
/// `call_handler` is carried through so the replacement is the *same worker* in the only sense that
/// matters to an application: same configuration, same capability. A replacement booted without a
/// handler would answer every call with "no call handler is installed" and look healthy doing it.
fn respawn(
    name: &str,
    spec: &Spec,
    guard: &LifecycleGuard,
    call_handler: Option<&Arc<MethodRegistry>>,
    slot: &mut Slot,
) -> Result<()> {
    slot.terminate();
    let ready = boot(name, spec, guard, call_handler)?;
    *slot = Slot::from_boot(ready);
    Ok(())
}

/// Replace the worker in a slot, reporting rather than propagating failure.
///
/// The caller is already handling a request that failed; a second error on the
/// same path would only hide the first. The slot is left without pipes, so
/// `pick_slot` tries again on the next request instead of handing out a worker
/// that is known to be dead.
fn replace(
    name: &str,
    spec: &Spec,
    guard: &LifecycleGuard,
    call_handler: Option<&Arc<MethodRegistry>>,
    slot: &mut Slot,
) {
    if let Err(err) = respawn(name, spec, guard, call_handler, slot) {
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
            let payload = <volucer_worker::Json as volucer_worker::Codec>::encode(&Control::Drain)
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
use volucer_fcgi::NameValuePairs;
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

    /// The free list's two rules, which is where a fair queue is won or lost.
    #[test]
    fn a_free_slot_is_not_taken_from_a_request_that_is_waiting_for_it() {
        let mut free = FreeSlots {
            ids: vec![3, 2, 1, 0],
            waiters: 0,
        };

        // An idle pool with no waiters: a new request takes a slot.
        assert_eq!(free.take_available(), Some(0));
        assert_eq!(free.ids, vec![3, 2, 1]);

        // With a request parked, a newcomer gets nothing even though slots *are* free. This is the
        // barging rule: the parked request was woken for the next release, and handing that slot to
        // an arrival instead would leave the parked one waiting for a slot it was already promised.
        free.waiters = 1;
        assert_eq!(free.take_available(), None);
        assert_eq!(free.ids, vec![3, 2, 1], "nothing was taken");

        // The parked request takes it, and the rule lifts for the next arrival.
        assert_eq!(free.take_woken(), Some(1));
        free.waiters -= 1;
        assert_eq!(free.take_available(), Some(2));

        // A woken request can still find nothing, if the slot it was woken for went to another
        // waiter first. That is why the queue loop re-checks rather than assuming a wake-up means a
        // slot, and the last line is the state it must handle.
        free.waiters = 2;
        assert_eq!(free.take_woken(), Some(3));
        assert_eq!(free.take_woken(), None);
    }

    /// The histogram's bucket edges, which is where a percentile can go wrong quietly.
    #[test]
    fn wait_buckets_are_monotonic_and_cover_the_range_a_wait_can_occupy() {
        // Every bucket is reachable, so a wait of zero and a wait of a whole request are never
        // counted together: below a microsecond is measurement noise and belongs in the first
        // bucket, not in a discard the percentiles would then be computed without.
        assert_eq!(wait_bucket(0), 0);
        assert_eq!(wait_bucket(1), 0);
        assert_eq!(wait_bucket(2), 1);
        assert_eq!(wait_bucket(3), 1);
        assert_eq!(wait_bucket(1_000), 9);
        assert_ne!(wait_bucket(1_000), wait_bucket(2_000));

        // Monotonic, so the cumulative walk in `slot_wait_percentile_us` cannot see a larger
        // wait sorted into an earlier bucket and report a percentile that is too small.
        let mut previous = 0;
        for micros in 0..200_000u64 {
            let bucket = wait_bucket(micros);
            assert!(bucket >= previous, "{micros} us went backwards");
            previous = bucket;
        }

        // A wait past the last edge must saturate rather than index out of the array, and the
        // last edge is beyond the point at which the request timeout replaces the worker.
        assert_eq!(wait_bucket(1_000_000), WAIT_BUCKETS - 1);
        assert_eq!(wait_bucket(u64::MAX), WAIT_BUCKETS - 1);
        assert!(bucket_upper_edge(WAIT_BUCKETS - 1) > 1_000_000);
    }
}
