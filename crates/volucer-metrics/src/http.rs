//! The `rr_http_*` metric set: the names, their types, and the calls that move them.
//!
//! # Why the names live in the metrics crate and not at the call sites
//!
//! A metric name is an interface. `rr_http_request_total` is what a dashboard, an alert and a run
//! book are written against, and a name that drifted between the place that describes a metric and
//! the place that writes it would produce a graph that silently goes flat. Putting the strings in
//! one module means the only way to get a name wrong is to edit this file.
//!
//! # Why they are `rr_`-prefixed
//!
//! These are RoadRunner's names, kept deliberately. An operator arriving from RoadRunner has
//! dashboards already written against `rr_http_request_total` and friends, and renaming them buys
//! nothing and costs every one of those a migration. The *activation* rule differs — RoadRunner
//! only emits HTTP metrics when an `http_metrics` middleware is listed, where volucer emits them
//! whenever `[metrics]` is configured — and that difference is stated in `docs/plugins.md` §9.3
//! rather than hidden in a name.
//!
//! # Labels come from closed sets, and that is the point
//!
//! `status` is a three-digit code formatted from a `u16`, and `state` is one of four constants. No
//! label here is a path, a query string or a host, because a label whose values come from the
//! request is how a metrics endpoint grows without bound — the registry caps that, and this module
//! is where the cap is avoided rather than relied on.

use crate::Registry;

/// The prefix every metric in this set carries.
pub const PREFIX: &str = "rr_http_";

/// The four states a worker slot can be in.
///
/// A `&str` rather than an enum because the value crosses into a label, and the four constants are
/// the whole vocabulary: a fifth state added here without adding it to the state gauge's readers
/// would be a graph with an unexplained line rather than a compile error either way.
pub mod state {
    pub const READY: &str = "ready";
    pub const WORKING: &str = "working";
    pub const INVALID: &str = "invalid";
    pub const STOPPED: &str = "stopped";
}

/// Buckets for request duration, in seconds.
///
/// The same shape every Prometheus client library uses, extended to 60 s because a PHP request is
/// allowed to take that long before the pool's `exec_ttl` kills it. A ladder that stopped at 10 s
/// would put every timeout in `+Inf` and make the slowest requests — the only interesting ones —
/// indistinguishable from each other.
pub const DURATION_BUCKETS: [f64; 12] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 60.0,
];

/// Register every metric in the set.
///
/// Called once at startup, and idempotent — a second call with the same names and help is a no-op,
/// so a caller does not have to be careful about doing it twice.
pub fn describe(registry: &Registry) {
    registry.describe(
        &name("total_workers"),
        crate::Kind::Gauge,
        "Total number of workers in the pool",
    );
    registry.describe(
        &name("workers_ready"),
        crate::Kind::Gauge,
        "Number of workers ready to serve a request",
    );
    registry.describe(
        &name("workers_working"),
        crate::Kind::Gauge,
        "Number of workers currently serving a request",
    );
    registry.describe(
        &name("workers_invalid"),
        crate::Kind::Gauge,
        "Number of workers that cannot serve a request",
    );
    registry.describe(
        &name("workers_memory_bytes"),
        crate::Kind::Gauge,
        "Total resident memory of all workers, in bytes",
    );
    registry.describe(
        &name("worker_memory_bytes"),
        crate::Kind::Gauge,
        "Resident memory of one worker, in bytes",
    );
    registry.describe(
        &name("worker_state"),
        crate::Kind::Gauge,
        "Whether a worker is in a given state: 1 for its state, 0 for the others",
    );
    registry.describe(
        &name("request_total"),
        crate::Kind::Counter,
        "Requests served, by response status",
    );
    registry.describe_histogram(
        &name("request_duration_seconds"),
        "Time to serve a request, by response status",
        &DURATION_BUCKETS,
    );
    registry.describe(
        &name("requests_queue"),
        crate::Kind::Gauge,
        "Requests waiting for a worker",
    );
    registry.describe(
        &name("uptime_seconds"),
        crate::Kind::Gauge,
        "Seconds since this pool started",
    );
    registry.describe(
        &name("no_free_workers_total"),
        crate::Kind::Counter,
        "Times a request found no worker free and had to wait",
    );
}

/// One pool, as the metrics need to see it.
///
/// A snapshot rather than a set of accessors on the pool, so that emission is one call at one
/// moment: reading `ready` and then `working` from a live pool would take two different pictures of
/// it, and a graph where `ready + working` does not equal `total_workers` is a graph nobody trusts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoolSnapshot {
    pub total_workers: usize,
    pub workers_ready: usize,
    pub workers_working: usize,
    pub workers_invalid: usize,
    /// Per-worker resident memory and pid, in slot order.
    pub workers: Vec<WorkerSnapshot>,
}

/// One worker slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerSnapshot {
    pub pid: u32,
    pub state: &'static str,
    pub memory_bytes: u64,
}

/// Publish a whole pool snapshot.
///
/// Sets the gauges it is given and **does not zero the ones it is not**, because every field is
/// present by construction — a caller that wanted to leave one out would have to say so, and there
/// is no case in this server where a pool does not know how many workers it has.
pub fn set_pool(registry: &Registry, snapshot: &PoolSnapshot) {
    registry.gauge_set(&name("total_workers"), &[], snapshot.total_workers as f64);
    registry.gauge_set(&name("workers_ready"), &[], snapshot.workers_ready as f64);
    registry.gauge_set(
        &name("workers_working"),
        &[],
        snapshot.workers_working as f64,
    );
    registry.gauge_set(
        &name("workers_invalid"),
        &[],
        snapshot.workers_invalid as f64,
    );

    let mut total_memory = 0u64;
    for worker in &snapshot.workers {
        total_memory = total_memory.saturating_add(worker.memory_bytes);
        let pid = worker.pid.to_string();
        registry.gauge_set(
            &name("worker_memory_bytes"),
            &[("pid", &pid)],
            worker.memory_bytes as f64,
        );
        // One series per worker *and state*, which is what makes a stack of these graphs in a
        // dashboard readable: the state a worker is in is the `1`, and the other three are `0` rather
        // than absent, so a worker that stops reporting does not silently leave a gap.
        for candidate in [state::READY, state::WORKING, state::INVALID, state::STOPPED] {
            registry.gauge_set(
                &name("worker_state"),
                &[("pid", &pid), ("state", candidate)],
                if candidate == worker.state { 1.0 } else { 0.0 },
            );
        }
    }
    registry.gauge_set(&name("workers_memory_bytes"), &[], total_memory as f64);
}

/// Record one served request.
///
/// The status is formatted from the code rather than taken from the response's reason phrase,
/// because a phrase is prose and a label set built from prose has an unbounded number of values —
/// one per status an application decides to invent.
pub fn observe_request(registry: &Registry, status: u16, seconds: f64) {
    let status = status.to_string();
    registry.counter_add(&name("request_total"), &[("status", &status)], 1);
    let _ = registry.observe(
        &name("request_duration_seconds"),
        &[("status", &status)],
        seconds,
    );
}

/// How many requests are waiting for a free worker.
pub fn set_requests_queue(registry: &Registry, waiting: usize) {
    registry.gauge_set(&name("requests_queue"), &[], waiting as f64);
}

/// How long this pool has been up.
pub fn set_uptime_seconds(registry: &Registry, seconds: f64) {
    registry.gauge_set(&name("uptime_seconds"), &[], seconds);
}

/// Record a request that found no worker free.
pub fn count_no_free_workers(registry: &Registry) {
    registry.counter_add(&name("no_free_workers_total"), &[], 1);
}

/// The full name for a metric in this set.
fn name(suffix: &str) -> String {
    format!("{PREFIX}{suffix}")
}
