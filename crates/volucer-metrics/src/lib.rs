//! A metric registry, and the Prometheus text format written out by hand.
//!
//! # Why this crate exists at all
//!
//! `volucer-observe` answers "what happened to *this* request": an id, a trace context, an access
//! log. A metric answers a different question — "how is the process doing" — and nothing in the
//! tree answered it. The absence is not cosmetic: without it there is no way to see that a worker
//! pool is unhealthy, a queue is not draining, or a lock is contended, and every feature that
//! follows this one would be built with no way to observe whether it works.
//!
//! # The wire path already exists and goes nowhere
//!
//! A worker can already publish a metric. `volucer-php-sdk` has `emitMetric`, the protocol has a
//! `METRIC` frame (type 8), and `volucer-worker` already decodes it — `absorb_metric` at
//! `crates/volucer-worker/src/codec.rs:720`. What happens next is nothing: the sample is attached
//! to the request's response and dropped. So the interesting half of this feature is not the
//! registry, it is the call that gives those bytes somewhere to land.
//!
//! # The format is written by hand, deliberately
//!
//! The `prometheus` crate is the obvious dependency and it is the wrong one here. The format is a
//! page of specification — `# HELP`, `# TYPE`, `name{label="value"} 1.0` — and the crate brings
//! `protobuf` with it for OpenMetrics, which nothing here needs. This project made the same call
//! twice already, for the static-file layer and the access log. A hand-written encoder is also the
//! only way to guarantee the property that matters most: **exposition cannot fail**, because it
//! runs on a scrape and must never panic.
//!
//! # Cardinality is the hazard, and the design is shaped around it
//!
//! A metric whose label comes from a request path is how a metrics endpoint takes down the
//! Prometheus server it feeds. Two rules follow, and both are enforced rather than documented:
//!
//! 1. **Series are capped per metric name** ([`Registry::with_series_cap`]). Past the cap a new
//!    label set is refused, once per name at `warn` and then silently, and
//!    [`Registry::refused_series`] counts it so the refusal is visible rather than inferred from a
//!    hole in a graph.
//! 2. **Labels are compared and emitted in sorted order.** Two call sites that pass the same pairs
//!    in different orders must land on one series, or the cap is spent twice and the graph has two
//!    lines for one thing.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// How many distinct label sets one metric name may have.
///
/// Chosen to be far above any legitimate use in this server and far below the point at which
/// exposition becomes the bottleneck: the pool metrics are per-pid, so a 64-worker pool is 64
/// series of `rr_http_worker_state`. Anything approaching this number is a bug at a call site
/// rather than a busy deployment.
pub const DEFAULT_SERIES_CAP: usize = 512;

/// A counter, a gauge or a histogram.
///
/// Histograms are why this is an enum rather than one type: a counter and a gauge are one
/// `AtomicU64`, and a histogram is `buckets + 2`. Making every metric pay for the largest shape is
/// how a registry ends up costing more than the thing it measures.
#[derive(Debug)]
enum Metric {
    Counter(AtomicU64),
    Gauge(AtomicU64),
    Histogram(Histogram),
}

/// Counts in cumulative buckets, plus a sum and a count.
///
/// The bounds are the *upper* bounds, sorted, with an implicit `+Inf` that is the total count —
/// which is why `count` is stored separately rather than read off the last bucket. A histogram
/// that treated its last bucket as `+Inf` would be wrong the moment someone added a bucket.
#[derive(Debug)]
struct Histogram {
    /// Upper bounds in seconds, sorted, one count each.
    bounds: Vec<f64>,
    counts: Vec<AtomicU64>,
    /// The sum of every observation, as raw `f64` bits, because there is no `AtomicF64`.
    sum_bits: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    fn new(bounds: Vec<f64>) -> Self {
        let counts = (0..bounds.len()).map(|_| AtomicU64::new(0)).collect();
        Histogram {
            bounds,
            counts,
            sum_bits: AtomicU64::new(0f64.to_bits()),
            count: AtomicU64::new(0),
        }
    }

    /// Record one observation, and report whether it landed in a real bucket.
    ///
    /// `false` means the value was above every bound and was counted by `+Inf` alone. That is a
    /// legal observation rather than an error — but worth a caller knowing, because a histogram
    /// whose observations are all out of range measures nothing while looking like a quiet system.
    ///
    /// **Every bucket from the matching one onwards is incremented, not just the matching one.**
    /// Prometheus buckets are cumulative — `le="0.5"` counts everything at or below 0.5, including
    /// everything already in `le="0.1"` — and an implementation that incremented one bucket would
    /// emit a histogram whose buckets are not non-decreasing. Prometheus reads that as wrong for
    /// every quantile while the `_count` and `_sum` still look right, which is the sort of mistake
    /// that survives a glance at the graph.
    fn observe(&self, value: f64) -> bool {
        // The first bound that is not below the value. `partition_point` needs its predicate to be
        // monotonic over the slice, which "bound < value" is because the bounds are sorted. A value
        // exactly on a bound therefore starts at that bound's own bucket, which is what `le` means.
        let index = self.bounds.partition_point(|bound| *bound < value);
        let landed = index < self.bounds.len();
        if landed {
            for slot in &self.counts[index..] {
                slot.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        // Read-modify-write rather than `fetch_add`, because `f64` has no atomic add. A scrape
        // racing an observation loses at most one increment from the sum, which is the same
        // guarantee the count gives and is why every Prometheus client documents a histogram's sum
        // as approximate under concurrency.
        let mut current = self.sum_bits.load(Ordering::Relaxed);
        loop {
            let total = f64::from_bits(current) + value;
            match self.sum_bits.compare_exchange_weak(
                current,
                total.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(seen) => current = seen,
            }
        }
        landed
    }
}

/// One metric's identity within its family: the label pairs that distinguish it from its siblings.
///
/// The labels are kept **sorted by name** and compared as a tuple, which is what makes two call
/// sites that pass the same pairs in a different order land on one series. `HashMap` iteration
/// order is not stable, so a `HashMap` here would produce a different key for the same labels on
/// different runs, and the cap would be spent on duplicates.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    labels: Vec<(String, String)>,
}

impl Key {
    fn new(mut labels: Vec<(String, String)>) -> Self {
        labels.sort_by(|a, b| a.0.cmp(&b.0));
        // Sorted-then-deduped keeps the *first* of a repeated name, which is the one a caller
        // listed first. Two values for one label name is a call-site mistake; dropping the later
        // one keeps the series key well-formed rather than emitting a line with a duplicate label.
        labels.dedup_by(|a, b| a.0 == b.0);
        Key { labels }
    }
}

/// What a metric is, so that exposition can write the right `# TYPE` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Counter,
    Gauge,
    Histogram,
}

impl Kind {
    /// The word Prometheus expects after `# TYPE`.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
            Kind::Histogram => "histogram",
        }
    }
}

/// One metric family: a name, its kind, its help text, and the series that share it.
///
/// The help text is required rather than optional because `# HELP` is what makes a metric readable
/// six months later by someone who did not write it, and a registry that allowed it to be omitted
/// would produce a `/metrics` page of bare names.
#[derive(Debug)]
struct Family {
    kind: Kind,
    help: String,
    /// Upper bounds for a histogram, empty for every other kind. Set once at registration, so a
    /// series created later cannot disagree with its siblings about what a bucket means.
    bounds: Vec<f64>,
    series: RwLock<HashMap<Key, Arc<Metric>>>,
}

/// Why a metric for a name and label set could not be produced.
///
/// Returned rather than logged where it happens, because the caller is what knows whether the
/// refusal was already counted: counting in both places is how one refused call becomes two, which
/// is exactly what the first version of this did and what the cap test caught.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// The name was never described, so it has no `# TYPE` and no `# HELP`.
    Undeclared,
    /// The name is at its series cap.
    AtCap,
    /// The name exists but not at the kind the caller used.
    WrongKind,
}

fn refusal_reason(reason: Refusal) -> &'static str {
    match reason {
        Refusal::Undeclared => "undeclared",
        Refusal::AtCap => "at-cap",
        Refusal::WrongKind => "wrong-kind",
    }
}

/// The key a refusal is counted under: the metric name and the reason, so one name can appear more
/// than once and each line says which fix it wants.
fn refusal_key(name: &str, reason: Refusal) -> String {
    format!("{name}:{}", refusal_reason(reason))
}

/// A registry of metrics, shared by everything that publishes.
///
/// `Arc`-shaped and cheap to clone: a front-end, a worker pool and a worker's own telemetry all
/// hold the same registry, which is what makes one `/metrics` page describe the whole process.
/// There is deliberately **no global**: a global registry is what makes two servers in one process
/// impossible and makes a test's assertions depend on test ordering.
#[derive(Debug, Clone)]
pub struct Registry {
    families: Arc<RwLock<HashMap<String, Arc<Family>>>>,
    cap: usize,
    /// Series refused for exceeding the cap, by metric name, so the refusal is countable rather
    /// than only visible as a gap.
    refused: Arc<RwLock<HashMap<String, u64>>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self::with_series_cap(DEFAULT_SERIES_CAP)
    }

    pub fn with_series_cap(cap: usize) -> Self {
        Registry {
            families: Arc::new(RwLock::new(HashMap::new())),
            cap,
            refused: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a counter or a gauge.
    ///
    /// A second registration with a different kind or help text is a mistake at a call site and is
    /// **warned and ignored** rather than made fatal: refusing to start over a help string is a
    /// worse outcome than one metric with stale text, and this runs during startup where a panic
    /// would take the server down.
    pub fn describe(&self, name: &str, kind: Kind, help: &str) {
        self.register(name, kind, help, Vec::new());
    }

    /// Register a histogram and its bucket bounds.
    ///
    /// The bounds are sorted here, once, so `observe` can use `partition_point`. An empty set of
    /// bounds is legal and means every observation lands in `+Inf`, which is what a caller who only
    /// wants a sum and a count should ask for.
    pub fn describe_histogram(&self, name: &str, help: &str, bounds: &[f64]) {
        let mut sorted: Vec<f64> = bounds.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        self.register(name, Kind::Histogram, help, sorted);
    }

    fn register(&self, name: &str, kind: Kind, help: &str, bounds: Vec<f64>) {
        let mut families = self.families.write().expect("the registry lock");
        if let Some(existing) = families.get(name) {
            if existing.kind != kind || existing.help != help {
                tracing::warn!(
                    metric = name,
                    "registered twice with a different kind or help; the first one is kept"
                );
            }
            return;
        }
        families.insert(
            name.to_string(),
            Arc::new(Family {
                kind,
                help: help.to_string(),
                bounds,
                series: RwLock::new(HashMap::new()),
            }),
        );
    }

    /// The metric behind a name and label set, created on first use.
    ///
    /// `Err` is a refusal, and the caller counts it — the function itself never does, so one failed
    /// call cannot be counted twice.
    fn metric_for(
        &self,
        name: &str,
        labels: Vec<(String, String)>,
    ) -> Result<Arc<Metric>, Refusal> {
        let family = {
            let families = self.families.read().expect("the registry lock");
            match families.get(name) {
                Some(family) => Arc::clone(family),
                None => return Err(Refusal::Undeclared),
            }
        };
        let key = Key::new(labels);

        {
            let series = family.series.read().expect("the series lock");
            if let Some(existing) = series.get(&key) {
                return Ok(Arc::clone(existing));
            }
        }

        let mut series = family.series.write().expect("the series lock");
        // Re-checked under the write lock: two threads that both missed the read would otherwise
        // both insert, and the second insert would replace a metric that was already counting.
        if let Some(existing) = series.get(&key) {
            return Ok(Arc::clone(existing));
        }
        if series.len() >= self.cap {
            return Err(Refusal::AtCap);
        }

        let metric = Arc::new(match family.kind {
            Kind::Counter => Metric::Counter(AtomicU64::new(0)),
            Kind::Gauge => Metric::Gauge(AtomicU64::new(0)),
            Kind::Histogram => Metric::Histogram(Histogram::new(family.bounds.clone())),
        });
        series.insert(key, Arc::clone(&metric));
        Ok(metric)
    }

    /// Count a refused series, and say so once per name at `warn`.
    ///
    /// Once per name and reason rather than once per refusal, because a request path in a label
    /// would otherwise produce a log line per request — turning a cardinality problem into a
    /// logging problem, which is strictly worse.
    ///
    /// The reason is part of the key, so a metric that is undeclared and one that is at its cap are
    /// counted separately rather than summed into a number that says something happened but not
    /// what. That distinction is the whole value of the counter: "at the cap" is a deployment
    /// problem, and "undeclared" is a typo, and they want different fixes.
    fn refuse(&self, name: &str, reason: Refusal) {
        let key = refusal_key(name, reason);
        let first = {
            let mut refused = self.refused.write().expect("the refusal lock");
            let count = refused.entry(key).or_insert(0);
            let first = *count == 0;
            *count += 1;
            first
        };
        if first {
            tracing::warn!(
                metric = name,
                reason = refusal_reason(reason),
                cap = self.cap,
                "refusing a metric sample; see refused_series for the count"
            );
        }
    }

    /// A metric that exists but was used at the wrong kind.
    ///
    /// Counted under a reason of its own rather than as `AtCap` or `Undeclared`, because neither of
    /// those is what happened and a counter that named the wrong one would send a reader looking at
    /// the wrong thing.
    fn refuse_mismatch(&self, name: &str) {
        self.refuse(name, Refusal::WrongKind);
    }

    /// How many series were refused, by `name:reason`, sorted.
    ///
    /// The reason is part of the key rather than a separate call, so a metric that is undeclared and
    /// one that is at its cap are counted separately instead of summed into a number that says
    /// something happened but not what. That distinction is the whole value of the counter: "at the
    /// cap" is a deployment problem and "undeclared" is a typo, and they want different fixes.
    pub fn refused_series(&self) -> Vec<(String, u64)> {
        let refused = self.refused.read().expect("the refusal lock");
        let mut out: Vec<(String, u64)> = refused.iter().map(|(k, v)| (k.clone(), *v)).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Add to a counter.
    ///
    /// A name that was never described is ignored rather than created, and so is a name described at
    /// a different kind. Both are counted by [`Registry::refused_series`], so a call-site mistake is
    /// visible rather than becoming a metric that exists and never moves.
    pub fn counter_add(&self, name: &str, labels: &[(&str, &str)], delta: u64) {
        match self.metric_for(name, owned(labels)) {
            Ok(metric) => match &*metric {
                Metric::Counter(value) => {
                    value.fetch_add(delta, Ordering::Relaxed);
                }
                _ => self.refuse_mismatch(name),
            },
            Err(refusal) => self.refuse(name, refusal),
        }
    }

    /// Set a gauge. The value is stored as `f64` bits, because there is no `AtomicF64`.
    pub fn gauge_set(&self, name: &str, labels: &[(&str, &str)], value: f64) {
        match self.metric_for(name, owned(labels)) {
            Ok(metric) => match &*metric {
                Metric::Gauge(slot) => slot.store(value.to_bits(), Ordering::Relaxed),
                _ => self.refuse_mismatch(name),
            },
            Err(refusal) => self.refuse(name, refusal),
        }
    }

    /// Record one observation in a histogram.
    ///
    /// Returns whether the value landed inside a bucket. See [`Histogram::observe`] for why a
    /// caller wants to know: an observation above every bound is legal and invisible.
    #[must_use]
    pub fn observe(&self, name: &str, labels: &[(&str, &str)], value: f64) -> bool {
        match self.metric_for(name, owned(labels)) {
            Ok(metric) => match &*metric {
                Metric::Histogram(histogram) => histogram.observe(value),
                _ => {
                    self.refuse_mismatch(name);
                    false
                }
            },
            Err(refusal) => {
                self.refuse(name, refusal);
                false
            }
        }
    }

    /// Every metric name currently registered, sorted.
    pub fn names(&self) -> Vec<String> {
        let families = self.families.read().expect("the registry lock");
        let mut names: Vec<String> = families.keys().cloned().collect();
        names.sort();
        names
    }

    /// Render every metric in the Prometheus text exposition format.
    ///
    /// **This cannot fail.** It is called on a scrape, from a request path, so it returns a
    /// `String` rather than a `Result` — there is no failure a caller could act on that would not
    /// be better expressed as a metric that is missing. Family names are emitted in sorted order so
    /// that two scrapes of an unchanged process produce byte-identical output, which is what makes
    /// a diff of two scrapes readable and a golden test possible.
    pub fn render(&self) -> String {
        let mut families: Vec<(String, Arc<Family>)> = {
            let families = self.families.read().expect("the registry lock");
            families
                .iter()
                .map(|(name, family)| (name.clone(), Arc::clone(family)))
                .collect()
        };
        families.sort_by(|a, b| a.0.cmp(&b.0));

        let mut out = String::new();
        for (name, family) in families {
            let series = family.series.read().expect("the series lock");
            if series.is_empty() {
                // A family with no series is omitted entirely rather than printed with a `# TYPE`
                // and nothing under it. Prometheus would accept it; a human reading the page would
                // wonder why a metric that does not exist is described.
                continue;
            }
            let _ = writeln!(out, "# HELP {name} {}", escape_help(&family.help));
            let _ = writeln!(out, "# TYPE {name} {}", family.kind.as_str());

            let mut sorted: Vec<(&Key, &Arc<Metric>)> = series.iter().collect();
            sorted.sort_by(|a, b| a.0.labels.cmp(&b.0.labels));

            for (key, metric) in sorted {
                match &**metric {
                    Metric::Counter(value) => {
                        let v = value.load(Ordering::Relaxed);
                        let _ = writeln!(out, "{name}{} {v}", labels(&key.labels));
                    }
                    Metric::Gauge(bits) => {
                        let v = f64::from_bits(bits.load(Ordering::Relaxed));
                        let _ = writeln!(out, "{name}{} {}", labels(&key.labels), number(v));
                    }
                    Metric::Histogram(histogram) => {
                        render_histogram(&mut out, &name, key, histogram);
                    }
                }
            }
        }
        out
    }
}

/// Write one histogram: a bucket line per bound, then `+Inf`, then the sum and the count.
///
/// The bucket label is the *bound*, formatted the way Prometheus expects (`le="0.005"`), and `+Inf`
/// comes last. The format requires the cumulative counts to be non-decreasing and `le="+Inf"` to
/// equal the count, which is why `count` is written from the counter rather than off the last
/// bucket.
fn render_histogram(out: &mut String, name: &str, key: &Key, histogram: &Histogram) {
    for (index, bound) in histogram.bounds.iter().enumerate() {
        let count = histogram.counts[index].load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "{name}_bucket{} {count}",
            labels_with_le(&key.labels, &number(*bound))
        );
    }
    let total = histogram.count.load(Ordering::Relaxed);
    let _ = writeln!(
        out,
        "{name}_bucket{} {total}",
        labels_with_le(&key.labels, "+Inf")
    );
    let sum = f64::from_bits(histogram.sum_bits.load(Ordering::Relaxed));
    let _ = writeln!(out, "{name}_sum{} {}", labels(&key.labels), number(sum));
    let _ = writeln!(out, "{name}_count{} {total}", labels(&key.labels));
}

/// Render `{a="1",b="2"}`, or nothing when there are no labels.
fn labels(pairs: &[(String, String)]) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let mut out = String::from("{");
    for (index, (name, value)) in pairs.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(out, "{name}=\"{}\"", escape_label(value));
    }
    out.push('}');
    out
}

/// The same, with `le` merged into its sorted position.
///
/// `le` is a label like any other, so the sorted order must be preserved; appending it instead
/// would make a label set whose order differs from every other metric's, and `a="1",le="2"` is not
/// the same series as `le="2",a="1"` to a reader even when Prometheus treats them alike.
fn labels_with_le(pairs: &[(String, String)], bound: &str) -> String {
    let mut all: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    all.push(("le", bound));
    all.sort_by(|a, b| a.0.cmp(b.0));
    let mut out = String::from("{");
    for (index, (name, value)) in all.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        // `le` is a bound, not application data, so it is written verbatim: escaping it would
        // leave `+Inf` and a numeric bound unchanged either way, and this says plainly that it did
        // not come from the application.
        if *name == "le" {
            let _ = write!(out, "{name}=\"{value}\"");
        } else {
            let _ = write!(out, "{name}=\"{}\"", escape_label(value));
        }
    }
    out.push('}');
    out
}

/// Escape a label value per the exposition format: backslash, quote and newline.
///
/// Only those three. The format does **not** use JSON escapes — no `\t`, no `\r`, no `\uXXXX` — so a
/// general-purpose escaper would produce a value Prometheus reads differently from what the
/// application meant.
fn escape_label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

/// Escape a `# HELP` text: backslash and newline only, because a quote is not special there.
fn escape_help(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

/// A gauge or a sum as Prometheus reads it.
///
/// `+Inf`, `-Inf` and `NaN` are spelled the way the format spells them rather than as Rust's
/// `inf`/`NaN`, because a scraper that cannot parse a line drops the whole family. An integral
/// value is written without a decimal point, which is what every client library does and what makes
/// a byte-for-byte golden test stable.
fn number(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value.is_sign_positive() {
            "+Inf".to_string()
        } else {
            "-Inf".to_string()
        };
    }
    if value.fract() == 0.0 && value.abs() < 1e15 {
        return format!("{}", value as i64);
    }
    format!("{value}")
}

fn owned(labels: &[(&str, &str)]) -> Vec<(String, String)> {
    labels
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect()
}
