//! What answers a worker's call.
//!
//! # Why a registry, and why here
//!
//! [`volucer_worker`]'s read loop sees a `CALL` and hands it to a callback, because whether `kv.get`
//! means anything is a question about the process's configuration and the frame layer has no business
//! knowing the answer. This module *is* the answer: a map from `"<namespace>.<verb>"` to something that
//! can serve it.
//!
//! It lives in `volucer-server` because that is what owns the things a call reaches — the worker pool
//! for `workers.list` and `worker.reset`, the configuration for `config.describe` — and because the
//! front-ends already depend on it. A registry in the CLI would be unreachable from the pool that has to
//! install the handler on each worker's channel.
//!
//! # The three control methods need no service at all
//!
//! `workers.list`, `worker.reset` and `config.describe` read machinery that already exists, so a control
//! plane is available from the first commit that installs a handler rather than after KV is written.
//! That was the argument for making the call path the foundation of `docs/plugins.md` §4, and this is
//! where it pays.
//!
//! # Capability is per backend
//!
//! A registry is built **for one backend's workers**, and the caller decides which namespaces go in it.
//! A worker serving the public gets a registry with no `workers` namespace, so it cannot restart its
//! siblings; an operator's worker gets one that can. A single process-wide registry would give every
//! worker every capability, which is the shape §4.5 refuses.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use volucer_jobs::Pushed;
use volucer_lock::{Lease, Mode};
use volucer_worker::{Call, Return};

use crate::params::{decode_base64, encode_base64, Params};

/// Serves one namespace's calls.
///
/// `&self` and `Send + Sync`, because one registry is installed on every worker's channel in a pool and
/// those channels live on different threads. Whatever a handler accumulates belongs behind its own lock.
pub trait Namespace: Send + Sync {
    /// The prefix a call's name must start with, without the dot: `"kv"` for `"kv.get"`.
    ///
    /// Returned rather than stored so that a handler and the name it answers to are one thing. An
    /// implementation that computed this would be odd; one that returned a constant different from what
    /// its `serve` matches on is the bug this signature makes awkward.
    fn name(&self) -> &'static str;

    /// Answer one call. Never fails: an error is a [`Return`] that says so.
    ///
    /// The signature is deliberately not a `Result`. A call that cannot be served is an ordinary answer
    /// to an ordinary question — a missing key, a lock somebody else holds — and `Return` already
    /// carries the three outcomes that matter, including the one that says the effect is unknown.
    fn serve(&self, call: &Call) -> Return;
}

/// The set of namespaces one backend's workers may call.
///
/// A `BTreeMap` rather than a `HashMap`: the keys are read in a deterministic order when the registry
/// describes itself, and a `HashMap`'s iteration order is not stable — which turns "list the methods a
/// worker may call" into output that differs between runs.
#[derive(Clone, Default)]
pub struct MethodRegistry {
    namespaces: BTreeMap<String, Arc<dyn Namespace>>,
}

impl std::fmt::Debug for MethodRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written because `dyn Namespace` is not `Debug`, and the names are more useful than the
        // handlers anyway.
        f.debug_struct("MethodRegistry")
            .field("namespaces", &self.namespaces.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl MethodRegistry {
    /// An empty registry, which refuses every call by name.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a namespace. A second handler for one name replaces the first.
    ///
    /// Replacing rather than refusing, because `serve` builds this once and a test builds it per case;
    /// an error would be a question with one answer at one call site and no answer at the other.
    #[must_use]
    pub fn with(mut self, namespace: impl Namespace + 'static) -> Self {
        self.namespaces
            .insert(namespace.name().to_string(), Arc::new(namespace));
        self
    }

    /// The namespaces this registry serves, sorted.
    pub fn names(&self) -> Vec<&str> {
        self.namespaces.keys().map(String::as_str).collect()
    }

    /// Whether this registry serves anything at all.
    pub fn is_empty(&self) -> bool {
        self.namespaces.is_empty()
    }

    /// Answer one call.
    ///
    /// **A method that is not registered is a `Return` error naming it**, not a dropped frame and not a
    /// dead worker. That is the whole of the "no handler" story at this level: the channel already
    /// refuses when *no registry* is installed, and this refuses when the registry is installed but the
    /// method is not in it, so the two failures are distinguishable in a log.
    pub fn serve(&self, call: &Call) -> Return {
        let Some((namespace, verb)) = call.name.split_once('.') else {
            return Return::refused(format!(
                "call {:?} is not \"<namespace>.<verb>\", so there is nothing to route it to",
                call.name
            ));
        };
        if namespace.is_empty() || verb.is_empty() {
            return Return::refused(format!(
                "call {:?} has an empty namespace or verb",
                call.name
            ));
        }
        match self.namespaces.get(namespace) {
            Some(handler) => handler.serve(call),
            None => Return::refused(format!(
                "no {} namespace is configured for this worker, so {} cannot be served",
                namespace, call.name
            )),
        }
    }
}

/// The `config.describe` namespace: the configuration, with anything credential-bearing removed.
///
/// # Why redaction is a type and not a convention
///
/// The answer to this call includes driver configuration — a KV store's address, a queue's credentials,
/// a TLS key path — and it travels to a **worker**, which is application code. An application that can
/// read the deployment's credentials can leak them, and the leak looks like nothing at all in a log.
///
/// So the redaction is applied *here*, on the way out, rather than being each namespace's
/// responsibility. `docs/plugins.md` §9.5 is the divergence: RoadRunner's `rpc.Config` returns the
/// parsed document, and this deliberately does not.
pub struct ConfigNamespace {
    /// What to describe, already assembled. The caller does the assembly so that what is describable is
    /// a decision made in one place rather than a property of parsing.
    description: serde_json::Value,
}

/// Keys whose values are elided wherever they appear, matched on a **segment** of the key name.
///
/// **By name, not by pattern.** A pattern — "looks like a password" — is a heuristic that will be wrong
/// in both directions, and this is a security boundary: a value that looks like a secret and is not is a
/// confusing answer, and one that is a secret and does not look like it is a leak. So the list is
/// explicit and a new credential field has to be added to it, which is a decision somebody makes.
///
/// # Why a segment and not a substring
///
/// The first version of this used `key.contains(part)`, and `"ca"` then matched `certificate` — so the
/// answer elided every certificate path, which is a fact about the deployment an operator may reasonably
/// want to see and not a secret at all. A test caught it. Splitting on the separators a configuration key
/// uses and comparing whole words keeps `sentinel_password`, `tls_key` and `aws_secret_access_key`
/// matching while `certificate`, `cache` and `category` do not — and unlike a substring rule, a reader
/// can predict the outcome by looking at the key.
const ELIDED_KEY_PARTS: [&str; 7] = [
    "password",
    "token",
    "secret",
    "key",
    "authorization",
    "credential",
    "ca",
];

/// What replaces an elided value.
///
/// A word rather than an empty string, so that a reader of the answer can tell "there is a value here
/// and you may not see it" from "this field is unset" — two very different operational situations.
const ELIDED: &str = "<elided>";

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str) -> Call {
        Call {
            name: name.to_string(),
            params: serde_json::Value::Null,
            traceparent: None,
        }
    }

    /// A jobs namespace over two real memory pipelines, and the drivers behind it.
    ///
    /// **The real driver rather than a stub**, for the reason the lock tests give: the properties worth
    /// pinning are at the boundary between the wire format and the queue — an opaque payload, headers that
    /// are strings — and a stub would have to reimplement what a driver does with them to be asked.
    fn queues() -> (
        JobsNamespace,
        BTreeMap<String, Arc<volucer_jobs::MemoryDriver>>,
    ) {
        let mut drivers: BTreeMap<String, Arc<volucer_jobs::MemoryDriver>> = BTreeMap::new();
        for name in ["emails", "reports"] {
            drivers.insert(
                name.to_string(),
                Arc::new(volucer_jobs::MemoryDriver::single(name)),
            );
        }
        let namespace = JobsNamespace::new(
            drivers
                .iter()
                .map(|(name, driver)| {
                    (
                        name.clone(),
                        Arc::clone(driver) as Arc<dyn volucer_jobs::Driver>,
                    )
                })
                .collect(),
        );
        (namespace, drivers)
    }

    /// A push names the pipeline, the task and the payload, and the driver stores all three.
    #[test]
    fn a_push_reaches_the_named_pipeline_with_an_opaque_payload() {
        let (namespace, drivers) = queues();

        // Bytes above `0x7f`, because that is the whole reason the payload is base64: an application that
        // compresses or encrypts what it queues has such bytes constantly, and a JSON string cannot carry
        // one.
        let payload: Vec<u8> = vec![0x00, 0xff, 0x10, b'{', b'}'];
        let answer = namespace.serve(&call_with(
            "jobs.push",
            serde_json::json!({
                "pipeline": "emails",
                "task": "App\\Jobs\\SendEmail",
                "payload": crate::params::encode_base64(&payload),
                "headers": { "traceparent": "00-abc-def-01", "attempt": "1" },
            }),
        ));
        assert!(answer.ok, "{answer:?}");
        let result = answer.result.expect("a result");
        assert_eq!(result["pushed"], serde_json::json!(true));
        assert!(
            result["id"].as_str().is_some_and(|id| !id.is_empty()),
            "the push answers with the id the driver assigned: {result}"
        );

        let reserved = volucer_jobs::Driver::reserve(
            &*drivers["emails"],
            "emails",
            1,
            Duration::from_secs(60),
        )
        .expect("reserved");
        assert_eq!(reserved.len(), 1);
        assert_eq!(reserved[0].job.task, "App\\Jobs\\SendEmail");
        assert_eq!(
            reserved[0].job.payload, payload,
            "the payload arrived byte for byte, which is the property base64 is there for"
        );
        assert_eq!(reserved[0].job.header("traceparent"), Some("00-abc-def-01"));
        assert_eq!(reserved[0].job.header("attempt"), Some("1"));

        // And the *other* pipeline is untouched, which is what makes the name meaningful.
        assert!(volucer_jobs::Driver::reserve(
            &*drivers["reports"],
            "reports",
            1,
            Duration::from_secs(60)
        )
        .expect("reserved")
        .is_empty());
    }

    /// A payload may be absent, and an empty one is a job with no arguments rather than a mistake.
    #[test]
    fn a_push_needs_no_payload_and_an_empty_one_is_a_payload() {
        let (namespace, drivers) = queues();

        for params in [
            serde_json::json!({ "pipeline": "emails", "task": "App\\Jobs\\Ping" }),
            serde_json::json!({
                "pipeline": "emails",
                "task": "App\\Jobs\\Ping",
                "payload": ""
            }),
        ] {
            let answer = namespace.serve(&call_with("jobs.push", params));
            assert!(answer.ok, "{answer:?}");
        }

        let reserved = volucer_jobs::Driver::reserve(
            &*drivers["emails"],
            "emails",
            10,
            Duration::from_secs(60),
        )
        .expect("reserved");
        assert_eq!(reserved.len(), 2);
        assert!(reserved.iter().all(|held| held.job.payload.is_empty()));
    }

    /// A header that is not a string is refused, naming the header and saying where the value belongs.
    ///
    /// Coercing it would make the queue a place where types go to die: the value would come back as a
    /// string to a consumer that pushed an integer, and the failure would surface in application code far
    /// from the line that caused it.
    #[test]
    fn a_header_that_is_not_a_string_is_refused_by_name() {
        let (namespace, _drivers) = queues();

        let answer = namespace.serve(&call_with(
            "jobs.push",
            serde_json::json!({
                "pipeline": "emails",
                "task": "t",
                "headers": { "attempts": 3 },
            }),
        ));
        assert!(!answer.ok, "{answer:?}");
        let message = answer.error.expect("a reason");
        assert!(
            message.contains("attempts"),
            "it names the header: {message}"
        );
        assert!(
            message.contains("payload"),
            "and says where a structured value belongs: {message}"
        );

        // A headers value that is not an object at all is a different message, because it is a different
        // mistake.
        let answer = namespace.serve(&call_with(
            "jobs.push",
            serde_json::json!({ "pipeline": "emails", "task": "t", "headers": ["a"] }),
        ));
        assert!(!answer.ok);
        assert!(
            answer.error.expect("a reason").contains("object"),
            "headers that are an array must be refused as an array"
        );
    }

    /// Pushing to a pipeline that does not exist is refused with the names that do.
    #[test]
    fn a_push_to_an_unknown_pipeline_is_refused_with_the_ones_that_exist() {
        let (namespace, _drivers) = queues();

        let answer = namespace.serve(&call_with(
            "jobs.push",
            serde_json::json!({ "pipeline": "emials", "task": "t" }),
        ));
        assert!(!answer.ok, "{answer:?}");
        let message = answer.error.expect("a reason");
        assert!(message.contains("emials"), "{message}");
        assert!(
            message.contains("\"emails\""),
            "and what there is: {message}"
        );

        // A namespace with no pipelines at all says something different on purpose: there is nothing to
        // rename *to*.
        let empty = JobsNamespace::new(BTreeMap::new());
        let answer = empty.serve(&call_with(
            "jobs.push",
            serde_json::json!({ "pipeline": "emails", "task": "t" }),
        ));
        assert!(answer
            .error
            .expect("a reason")
            .contains("no pipelines at all"));
    }

    /// A batch is **partially** applied rather than atomic, and says where it stopped.
    ///
    /// The alternative — pretending the batch is one unit — would make a caller's retry duplicate the
    /// pushes that already succeeded. So a batch that fails halfway answers with the ids that made it and
    /// the index that did not, and the caller decides what to do about it.
    #[test]
    fn a_batch_that_fails_halfway_reports_what_it_pushed_rather_than_pretending_to_be_atomic() {
        let (namespace, drivers) = queues();

        let answer = namespace.serve(&call_with(
            "jobs.push_batch",
            serde_json::json!({
                "jobs": [
                    { "pipeline": "emails", "task": "first" },
                    { "pipeline": "emails", "task": "second" },
                    // The third names a pipeline that does not exist, which is the failure a batch has to
                    // survive without lying about.
                    { "pipeline": "emials", "task": "third" },
                    { "pipeline": "emails", "task": "fourth" },
                ]
            }),
        ));
        assert!(answer.ok, "{answer:?}");
        let result = answer.result.expect("a result");
        assert_eq!(result["pushed"], serde_json::json!(2), "{result}");
        assert_eq!(
            result["failed_index"],
            serde_json::json!(2),
            "and the index that failed, so a caller knows what to retry: {result}"
        );
        assert_eq!(
            result["ids"].as_array().map(Vec::len),
            Some(2),
            "with the ids that made it: {result}"
        );
        assert!(
            result["error"]
                .as_str()
                .is_some_and(|e| e.contains("emials")),
            "and a reason naming the pipeline: {result}"
        );

        // The two that succeeded are really there, and the fourth — which came after the failure — is not.
        let reserved = volucer_jobs::Driver::reserve(
            &*drivers["emails"],
            "emails",
            10,
            Duration::from_secs(60),
        )
        .expect("reserved");
        assert_eq!(
            reserved
                .iter()
                .map(|held| held.job.task.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"],
            "a batch stops at the failure rather than skipping it, so the caller's retry is the whole tail"
        );
    }

    /// A batch that succeeds in full answers with its ids and no failure index.
    #[test]
    fn a_batch_that_succeeds_reports_every_id() {
        let (namespace, drivers) = queues();

        let answer = namespace.serve(&call_with(
            "jobs.push_batch",
            serde_json::json!({
                "jobs": [
                    { "pipeline": "emails", "task": "a" },
                    { "pipeline": "reports", "task": "b" },
                ]
            }),
        ));
        assert!(answer.ok, "{answer:?}");
        let result = answer.result.expect("a result");
        assert_eq!(result["pushed"], serde_json::json!(2));
        assert_eq!(result["failed_index"], serde_json::json!(null));
        assert_eq!(result["ids"].as_array().map(Vec::len), Some(2));

        // One to each pipeline, which is the property a batch across pipelines needs.
        for pipeline in ["emails", "reports"] {
            assert_eq!(
                volucer_jobs::Driver::size(&*drivers[pipeline], pipeline).expect("size"),
                1
            );
        }
    }

    /// `size` answers both numbers, because the pair is what an operator reads.
    #[test]
    fn size_answers_the_ready_count_and_the_in_flight_count_together() {
        let (namespace, drivers) = queues();

        for task in ["a", "b", "c"] {
            namespace.serve(&call_with(
                "jobs.push",
                serde_json::json!({ "pipeline": "emails", "task": task }),
            ));
        }
        // One of them reserved, so the two numbers are different — which is the case a single number
        // cannot express and the reason both are reported.
        volucer_jobs::Driver::reserve(&*drivers["emails"], "emails", 1, Duration::from_secs(60))
            .expect("reserved");

        let answer = namespace.serve(&call_with(
            "jobs.size",
            serde_json::json!({ "pipeline": "emails" }),
        ));
        assert!(answer.ok, "{answer:?}");
        let result = answer.result.expect("a result");
        assert_eq!(result["size"], serde_json::json!(2));
        assert_eq!(result["in_flight"], serde_json::json!(1));

        // An unknown pipeline is refused by the namespace, before any driver is consulted.
        let answer = namespace.serve(&call_with(
            "jobs.size",
            serde_json::json!({ "pipeline": "emials" }),
        ));
        assert!(!answer.ok);
        assert!(answer.error.expect("a reason").contains("emials"));
    }

    /// `pipelines` lists the names and their drivers, which is what `config.describe` cannot say.
    #[test]
    fn pipelines_lists_each_pipeline_with_its_driver() {
        let (namespace, _drivers) = queues();

        let answer = namespace.serve(&call_with("jobs.pipelines", serde_json::json!({})));
        assert!(answer.ok, "{answer:?}");
        let pipelines = answer.result.expect("a result")["pipelines"]
            .as_array()
            .expect("an array")
            .clone();
        assert_eq!(pipelines.len(), 2);
        assert_eq!(pipelines[0]["name"], serde_json::json!("emails"));
        assert_eq!(pipelines[0]["driver"], serde_json::json!("memory"));
        assert_eq!(pipelines[1]["name"], serde_json::json!("reports"));
    }

    /// A missing `pipeline` or `task`, and a verb that does not exist, are each refused by name.
    #[test]
    fn a_jobs_call_that_cannot_be_understood_is_refused_by_name() {
        let (namespace, _drivers) = queues();

        let missing = namespace.serve(&call_with("jobs.push", serde_json::json!({ "task": "t" })));
        assert!(!missing.ok);
        assert!(missing.error.expect("a reason").contains("pipeline"));

        let missing = namespace.serve(&call_with(
            "jobs.push",
            serde_json::json!({ "pipeline": "emails" }),
        ));
        assert!(!missing.ok);
        assert!(missing.error.expect("a reason").contains("task"));

        let unknown = namespace.serve(&call_with("jobs.reserve", serde_json::json!({})));
        assert!(!unknown.ok);
        let message = unknown.error.expect("a reason");
        assert!(message.contains("reserve"), "{message}");
        assert!(
            message.contains("push_batch"),
            "and lists what does exist, because a typo and a missing feature read the same otherwise: \
             {message}"
        );

        for name in ["jobs", "jobs."] {
            let answer = namespace.serve(&call(name));
            assert!(!answer.ok, "{name} must be refused");
            assert!(answer.error.expect("a reason").contains("jobs method"));
        }
    }

    /// A lock namespace over a real memory backend, and the backend itself for assertions.    ///
    /// **The real driver rather than a stub**, because the properties worth pinning here are the ones at
    /// the boundary between the wire format and the semantics — `ttl = 0` meaning for ever, an absent id
    /// being generated — and a stub would have to reimplement the meaning of a TTL to be asked about it.
    fn locks() -> (LockNamespace, Arc<volucer_lock::MemoryLocks>) {
        let backend = Arc::new(volucer_lock::MemoryLocks::new());
        let namespace = LockNamespace::new(Arc::clone(&backend) as Arc<dyn volucer_lock::Locks>);
        (namespace, backend)
    }

    /// A call with `params`, which is what most of these tests need.
    ///
    /// Named `call_with` rather than per-service, because the envelope is the same for every namespace and
    /// a `lock_call`/`jobs_call` pair would be the same four lines twice.
    fn call_with(name: &str, params: serde_json::Value) -> Call {
        Call {
            name: name.to_string(),
            params,
            traceparent: None,
        }
    }

    /// The `id` an answer carried, or a panic explaining what the answer was instead.
    fn answered_id(answer: &Return) -> String {
        answer
            .result
            .as_ref()
            .and_then(|result| result.get("id"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("the answer carries no id: {answer:?}"))
            .to_string()
    }

    /// A fresh id every time, so two calls in one test are two holders rather than one that contends
    /// with itself.
    fn an_id() -> serde_json::Value {
        serde_json::json!(volucer_lock::generate_id())
    }

    /// An acquisition with no id gets one, and the caller is told what it is.
    ///
    /// Without the id in the answer the lock is unappealing and unreleasable: release and renewal are
    /// scoped by it, so a caller that did not choose one has no way to say which holder it is. RoadRunner's
    /// client generates its own instead; the argument for generating it on the server is in the type's
    /// documentation, and this pins that the value comes back either way.
    #[test]
    fn an_acquisition_without_an_id_is_given_one_and_the_answer_carries_it() {
        let (namespace, _backend) = locks();

        let answer = namespace.serve(&call_with(
            "lock.acquire",
            serde_json::json!({ "resource": "pdf:create" }),
        ));
        assert!(answer.ok, "{answer:?}");
        let result = answer.result.clone().expect("a result");
        assert_eq!(result["acquired"], serde_json::json!(true), "{result}");
        assert_eq!(result["mode"], serde_json::json!("exclusive"));

        let id = answered_id(&answer);
        assert_eq!(id.len(), 32, "a generated id is sixteen bytes of hex: {id}");

        // And the id it gave out is the one that releases it.
        let released = namespace.serve(&call_with(
            "lock.release",
            serde_json::json!({ "resource": "pdf:create", "id": id }),
        ));
        assert_eq!(
            released.result.expect("a result")["released"],
            serde_json::json!(true)
        );
    }

    /// `ttl = 0` means held until released — the opposite of what it means to a KV value.
    ///
    /// The test exists because the obvious implementation copies the KV TTL accessor, and in KV a zero TTL
    /// is "delete this key". A lock that did the same would be granted and released inside one call, which
    /// reads to the application as a lock that was never granted but *does not contend* — so it would not
    /// fail loudly, it would fail by letting everything through.
    #[test]
    fn a_zero_ttl_on_a_lock_means_held_until_released() {
        let (namespace, backend) = locks();

        for ttl in [serde_json::json!(0), serde_json::json!(null)] {
            let answer = namespace.serve(&call_with(
                "lock.acquire",
                serde_json::json!({ "resource": "pdf:create", "ttl": ttl, "mode": "shared" }),
            ));
            assert!(answer.ok, "{answer:?}");
            assert_eq!(
                answer.result.expect("a result")["remaining_ms"],
                serde_json::json!(null),
                "a lease with no deadline reports no remaining time"
            );
        }

        let holders = volucer_lock::Locks::holders(&*backend, "pdf:create").expect("holders");
        assert_eq!(
            holders.len(),
            2,
            "both holders are there and neither has a deadline"
        );
        assert!(holders.iter().all(|holder| holder.remaining.is_none()));

        // A non-zero TTL is a real deadline, which is the other half of the same accessor.
        let answer = namespace.serve(&call_with(
            "lock.acquire",
            serde_json::json!({
                "resource": "other",
                "ttl": 5_000_000,
                "mode": "exclusive"
            }),
        ));
        let remaining = answer.result.expect("a result")["remaining_ms"].clone();
        assert!(
            remaining.as_u64().is_some_and(|ms| ms > 4_000 && ms <= 5_000),
            "five million microseconds is five seconds, not five million of something else: {remaining}"
        );
    }

    /// Contention is a successful call with `acquired: false`, not a refusal.
    ///
    /// The distinction is the one RoadRunner's own documentation is emphatic about: a completed call can
    /// still mean "you did not get it", and an application that read `ok` alone would walk into the
    /// section. So `ok` is true and `acquired` is false, and the failure the caller must not miss is the
    /// *third* answer — `unknown` — which is asserted separately below.
    #[test]
    fn contention_is_an_answer_with_acquired_false_rather_than_a_refusal() {
        let (namespace, _backend) = locks();

        let first = namespace.serve(&call_with(
            "lock.acquire",
            serde_json::json!({ "resource": "pdf:create", "id": an_id() }),
        ));
        assert_eq!(
            first.result.expect("a result")["acquired"],
            serde_json::json!(true)
        );

        let second = namespace.serve(&call_with(
            "lock.acquire",
            serde_json::json!({ "resource": "pdf:create", "id": an_id() }),
        ));
        assert!(second.ok, "contention is not an error: {second:?}");
        let result = second.result.expect("a result");
        assert_eq!(result["acquired"], serde_json::json!(false), "{result}");
        assert_eq!(result["mode"], serde_json::json!(null));
        assert!(
            !result["id"].is_null(),
            "and the id that contended is echoed, because the client needs the one it will use next: \
             {result}"
        );

        // A reader is excluded by the writer too, in the same shape.
        let reader = namespace.serve(&call_with(
            "lock.acquire",
            serde_json::json!({ "resource": "pdf:create", "mode": "shared", "id": an_id() }),
        ));
        assert_eq!(
            reader.result.expect("a result")["acquired"],
            serde_json::json!(false)
        );
    }

    /// `try_acquire` does not wait, and `acquire` with no `wait` does not either.
    ///
    /// Asserted by timing, because that is the only thing that distinguishes them from `acquire` with a
    /// wait: an implementation that treated a missing `wait` as "wait for ever" would hang here, and one
    /// that treated `try_acquire` as `acquire` would make the test slow rather than fail — so the bound is
    /// generous enough not to be flaky and tight enough that a real wait would miss it.
    #[test]
    fn neither_try_acquire_nor_an_absent_wait_waits_for_anything() {
        let (namespace, _backend) = locks();
        let held = an_id();
        let acquired = namespace.serve(&call_with(
            "lock.acquire",
            serde_json::json!({ "resource": "pdf:create", "id": held }),
        ));
        assert_eq!(
            acquired.result.expect("a result")["acquired"],
            serde_json::json!(true)
        );

        let started = std::time::Instant::now();
        for method in ["lock.try_acquire", "lock.acquire"] {
            let answer = namespace.serve(&call_with(
                method,
                serde_json::json!({ "resource": "pdf:create", "id": an_id() }),
            ));
            assert_eq!(
                answer.result.expect("a result")["acquired"],
                serde_json::json!(false),
                "{method} must not have acquired it"
            );
        }
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "two calls with nothing to wait on took {:?}, so one of them waited",
            started.elapsed()
        );

        // And `try_acquire` ignores a `wait` it is sent, rather than waiting for it: the method name is
        // the statement, and a client that sends both means the one it called.
        let started = std::time::Instant::now();
        namespace.serve(&call_with(
            "lock.try_acquire",
            serde_json::json!({ "resource": "pdf:create", "id": an_id(), "wait": 10_000_000 }),
        ));
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "try_acquire waited {:?} for a wait it must ignore",
            started.elapsed()
        );
    }

    /// An acquisition that a backend could not decide is `unknown`, never "not acquired".
    ///
    /// The property the whole third answer exists for, and it cannot be reached through the memory driver
    /// — which is the point of a driver that has nothing to fail — so the backend is a stub that fails on
    /// demand. A caller told `acquired: false` here would enter the section believing it was empty.
    #[test]
    fn an_acquisition_a_backend_could_not_decide_is_reported_as_unknown() {
        /// A backend that refuses every acquisition with whatever `Error` it was given.
        #[derive(Debug)]
        struct Failing(volucer_lock::Error);

        impl volucer_lock::Locks for Failing {
            fn name(&self) -> &'static str {
                "failing"
            }
            fn try_acquire(
                &self,
                _resource: &str,
                _id: &str,
                _mode: Mode,
                _lease: Lease,
            ) -> volucer_lock::Result<Option<volucer_lock::Holder>> {
                Err(clone_error(&self.0))
            }
            fn acquire(
                &self,
                resource: &str,
                id: &str,
                mode: Mode,
                lease: Lease,
                _wait: Duration,
            ) -> volucer_lock::Result<Option<volucer_lock::Holder>> {
                self.try_acquire(resource, id, mode, lease)
            }
            fn release(&self, _resource: &str, _id: &str) -> volucer_lock::Result<bool> {
                Err(clone_error(&self.0))
            }
            fn release_all(&self, _resource: &str) -> volucer_lock::Result<bool> {
                Err(clone_error(&self.0))
            }
            fn holders(&self, _resource: &str) -> volucer_lock::Result<Vec<volucer_lock::Holder>> {
                Err(clone_error(&self.0))
            }
            fn update_lease(
                &self,
                _resource: &str,
                _id: &str,
                _lease: Lease,
            ) -> volucer_lock::Result<bool> {
                Err(clone_error(&self.0))
            }
        }

        fn clone_error(err: &volucer_lock::Error) -> volucer_lock::Error {
            match err {
                volucer_lock::Error::Unknown {
                    resource,
                    operation,
                    reason,
                } => volucer_lock::Error::Unknown {
                    resource: resource.clone(),
                    operation,
                    reason: reason.clone(),
                },
                volucer_lock::Error::Backend { driver, reason } => volucer_lock::Error::Backend {
                    driver,
                    reason: reason.clone(),
                },
                other => volucer_lock::Error::Backend {
                    driver: "failing",
                    reason: other.to_string(),
                },
            }
        }

        let backend = Arc::new(Failing(volucer_lock::Error::Backend {
            driver: "redis",
            reason: "the connection dropped after the command was sent".to_string(),
        }));
        let namespace = LockNamespace::new(backend as Arc<dyn volucer_lock::Locks>);

        let answer = namespace.serve(&call_with(
            "lock.acquire",
            serde_json::json!({ "resource": "pdf:create" }),
        ));
        assert!(!answer.ok, "{answer:?}");
        assert!(
            answer.unknown,
            "a failed acquisition must be `unknown`, because the caller cannot tell whether the command \
             ran: {answer:?}"
        );

        // The third case, and the one that is easy to get wrong in the other direction: a *decided*
        // failure during an acquisition. A name the backend refuses locally was never sent anywhere, so
        // it is a refusal — and the version of this code that reported it as unknown is what this
        // assertion was written after.
        #[derive(Debug)]
        struct Refusing;
        impl volucer_lock::Locks for Refusing {
            fn name(&self) -> &'static str {
                "refusing"
            }
            fn try_acquire(
                &self,
                resource: &str,
                _id: &str,
                _mode: Mode,
                _lease: Lease,
            ) -> volucer_lock::Result<Option<volucer_lock::Holder>> {
                Err(volucer_lock::Error::BadResource {
                    reason: format!("{resource:?} is not a name this backend stores"),
                })
            }
            fn acquire(
                &self,
                resource: &str,
                id: &str,
                mode: Mode,
                lease: Lease,
                _wait: Duration,
            ) -> volucer_lock::Result<Option<volucer_lock::Holder>> {
                self.try_acquire(resource, id, mode, lease)
            }
            fn release(&self, _resource: &str, _id: &str) -> volucer_lock::Result<bool> {
                Ok(false)
            }
            fn release_all(&self, _resource: &str) -> volucer_lock::Result<bool> {
                Ok(false)
            }
            fn holders(&self, _resource: &str) -> volucer_lock::Result<Vec<volucer_lock::Holder>> {
                Ok(Vec::new())
            }
            fn update_lease(
                &self,
                _resource: &str,
                _id: &str,
                _lease: Lease,
            ) -> volucer_lock::Result<bool> {
                Ok(false)
            }
        }
        let refusing = LockNamespace::new(Arc::new(Refusing) as Arc<dyn volucer_lock::Locks>);
        let answer = refusing.serve(&call_with(
            "lock.acquire",
            serde_json::json!({ "resource": "pdf:create" }),
        ));
        assert!(!answer.ok, "{answer:?}");
        assert!(
            !answer.unknown,
            "a backend that refused the name touched nothing, so there is nothing undetermined here: \
             {answer:?}"
        );

        // The other direction, and it is what makes the flag mean anything: a failure that *is* decided —
        // a read — is a refusal, so a caller can retry it without wondering.
        let answer = namespace.serve(&call_with(
            "lock.exists",
            serde_json::json!({ "resource": "pdf:create" }),
        ));
        assert!(!answer.ok, "{answer:?}");
        assert!(
            !answer.unknown,
            "a read that failed did nothing, so it is a refusal rather than an unknown: {answer:?}"
        );
    }

    /// `update_ttl` replaces the lease and answers `false` for an id that holds nothing.
    ///
    /// The `false` is the part worth having: a caller renewing a lock it had already lost must stop, and
    /// a method that reported success because the *call* succeeded is how an application keeps writing
    /// into a section two processes are in.
    #[test]
    fn update_ttl_replaces_the_lease_and_tells_a_caller_when_it_renewed_nothing() {
        let (namespace, _backend) = locks();
        let id = an_id();
        namespace.serve(&call_with(
            "lock.acquire",
            serde_json::json!({ "resource": "pdf:create", "id": serde_json::json!(id) }),
        ));

        let renewed = namespace.serve(&call_with(
            "lock.update_ttl",
            serde_json::json!({
                "resource": "pdf:create",
                "id": serde_json::json!(id),
                "ttl": 2_000_000
            }),
        ));
        assert_eq!(
            renewed.result.expect("a result")["updated"],
            serde_json::json!(true)
        );

        let holders = namespace.serve(&call_with(
            "lock.holders",
            serde_json::json!({ "resource": "pdf:create" }),
        ));
        let remaining = holders.result.expect("a result")["holders"][0]["remaining_ms"].clone();
        assert!(
            remaining
                .as_u64()
                .is_some_and(|ms| ms > 1_000 && ms <= 2_000),
            "two seconds, from now, rather than added to whatever was left: {remaining}"
        );

        let nobody = namespace.serve(&call_with(
            "lock.update_ttl",
            serde_json::json!({ "resource": "pdf:create", "id": an_id(), "ttl": 1_000_000 }),
        ));
        assert_eq!(
            nobody.result.expect("a result")["updated"],
            serde_json::json!(false)
        );
    }

    /// A missing `resource`, an unknown `mode`, and a verb that does not exist are each refused by name.
    ///
    /// The three are different mistakes with different fixes, so each message has to say which it is — and
    /// a missing resource must not be reported as a resource the *backend* refused, which would send an
    /// operator looking at the wrong thing.
    #[test]
    fn a_lock_call_that_cannot_be_understood_is_refused_by_name() {
        let (namespace, _backend) = locks();

        let missing = namespace.serve(&call_with("lock.acquire", serde_json::json!({})));
        assert!(!missing.ok);
        let message = missing.error.expect("a reason");
        assert!(message.contains("\"resource\""), "{message}");
        assert!(
            message.contains("required"),
            "a missing parameter must not be reported as a bad one: {message}"
        );

        let bad_mode = namespace.serve(&call_with(
            "lock.acquire",
            serde_json::json!({ "resource": "pdf", "mode": "readonly" }),
        ));
        let message = bad_mode.error.expect("a reason");
        assert!(message.contains("readonly"), "{message}");
        assert!(
            message.contains("shared"),
            "and say what is accepted: {message}"
        );

        let unknown = namespace.serve(&call_with(
            "lock.steal",
            serde_json::json!({ "resource": "pdf" }),
        ));
        let message = unknown.error.expect("a reason");
        assert!(message.contains("steal"), "{message}");
        assert!(
            message.contains("release_all"),
            "and list the methods that do exist, because a typo and a missing feature read the same \
             otherwise: {message}"
        );

        // A name that is not a lock call at all, and one that is only the namespace.
        for name in ["lock", "lock."] {
            let answer = namespace.serve(&call(name));
            assert!(!answer.ok, "{name} must be refused");
            assert!(answer.error.expect("a reason").contains("lock method"));
        }
    }

    /// `release` and `release_all` answer `false` when nothing was there, and `holders` shows the live set.
    #[test]
    fn releasing_what_is_not_held_is_an_answer_rather_than_a_failure() {
        let (namespace, _backend) = locks();

        let nothing = namespace.serve(&call_with(
            "lock.release",
            serde_json::json!({ "resource": "pdf:create", "id": an_id() }),
        ));
        assert!(nothing.ok, "{nothing:?}");
        assert_eq!(
            nothing.result.expect("a result")["released"],
            serde_json::json!(false)
        );

        let all = namespace.serve(&call_with(
            "lock.release_all",
            serde_json::json!({ "resource": "pdf:create" }),
        ));
        assert!(all.ok);
        assert_eq!(
            all.result.expect("a result")["released"],
            serde_json::json!(false)
        );

        let exists = namespace.serve(&call_with(
            "lock.exists",
            serde_json::json!({ "resource": "pdf:create" }),
        ));
        assert_eq!(
            exists.result.expect("a result")["exists"],
            serde_json::json!(false)
        );

        // Now hold it with two readers and a writer's view, to see the list work.
        let held = an_id();
        namespace.serve(&call_with(
            "lock.acquire",
            serde_json::json!({ "resource": "pdf:create", "id": held, "mode": "shared" }),
        ));
        namespace.serve(&call_with(
            "lock.acquire",
            serde_json::json!({ "resource": "pdf:create", "id": an_id(), "mode": "shared" }),
        ));

        let holders = namespace.serve(&call_with(
            "lock.holders",
            serde_json::json!({ "resource": "pdf:create" }),
        ));
        let holders = holders.result.expect("a result")["holders"]
            .as_array()
            .expect("an array")
            .clone();
        assert_eq!(holders.len(), 2, "{holders:?}");
        assert!(holders.iter().all(|holder| holder["mode"] == "shared"));

        let exists = namespace.serve(&call_with(
            "lock.exists",
            serde_json::json!({ "resource": "pdf:create" }),
        ));
        assert_eq!(
            exists.result.expect("a result")["exists"],
            serde_json::json!(true)
        );

        // And the writer's view: `release_all` takes both readers, whatever ids they chose.
        let all = namespace.serve(&call_with(
            "lock.release_all",
            serde_json::json!({ "resource": "pdf:create" }),
        ));
        assert_eq!(
            all.result.expect("a result")["released"],
            serde_json::json!(true)
        );
        assert_eq!(
            namespace
                .serve(&call_with(
                    "lock.exists",
                    serde_json::json!({ "resource": "pdf:create" })
                ))
                .result
                .expect("a result")["exists"],
            serde_json::json!(false)
        );
    }

    /// An acquisition that hands in an id the backend will not take is refused, and nothing is stored.
    #[test]
    fn an_id_the_backend_refuses_is_refused_over_the_wire_by_name() {
        let (namespace, backend) = locks();

        let answer = namespace.serve(&call_with(
            "lock.acquire",
            serde_json::json!({ "resource": "pdf:create", "id": "bad\u{1}id" }),
        ));
        assert!(!answer.ok, "{answer:?}");
        assert!(
            !answer.unknown,
            "a name this server refuses was never sent anywhere, so nothing is undetermined: {answer:?}"
        );
        let message = answer.error.expect("a reason");
        assert!(message.contains("control character"), "{message}");
        assert_eq!(backend.tracked(), 0);
    }

    /// A namespace that answers one name and records what it was asked.
    struct Echo;

    impl Namespace for Echo {
        fn name(&self) -> &'static str {
            "kv"
        }

        fn serve(&self, call: &Call) -> Return {
            match call.name.as_str() {
                // Echoes the params back, so a test can see they arrived unaltered.
                "kv.echo" => Return::ok(call.params.clone()),
                other => Return::refused(format!("kv does not serve {other}")),
            }
        }
    }

    #[test]
    fn a_registered_method_is_served_and_unregistered_ones_are_refused_by_name() {
        let registry = MethodRegistry::new().with(Echo);

        let served = registry.serve(&call("kv.echo"));
        assert!(served.ok);

        let wrong_verb = registry.serve(&call("kv.missing"));
        assert!(!wrong_verb.ok);
        assert!(
            wrong_verb
                .error
                .as_deref()
                .is_some_and(|e| e.contains("kv.missing")),
            "the namespace's own refusal names the verb: {wrong_verb:?}"
        );

        let wrong_ns = registry.serve(&call("lock.acquire"));
        assert!(!wrong_ns.ok);
        assert!(
            wrong_ns
                .error
                .as_deref()
                .is_some_and(|e| e.contains("no lock namespace")),
            "{wrong_ns:?}"
        );
    }

    /// A name with no dot cannot be routed, and says so rather than being treated as a namespace called
    /// `kv` with a missing verb.
    #[test]
    fn a_name_without_a_namespace_or_verb_is_refused() {
        let registry = MethodRegistry::new().with(Echo);

        for name in ["kv", "kv.", ".echo", "", "."] {
            let answer = registry.serve(&call(name));
            assert!(!answer.ok, "{name:?} should not be served");
            assert!(
                !answer.unknown,
                "{name:?}: nothing happened, so nothing is ambiguous"
            );
        }
    }

    #[test]
    fn an_empty_registry_refuses_everything_by_name() {
        let registry = MethodRegistry::new();
        assert!(registry.is_empty());

        let answer = registry.serve(&call("kv.get"));
        assert!(!answer.ok);
        assert!(answer
            .error
            .as_deref()
            .is_some_and(|e| e.contains("kv.get")));
    }

    #[test]
    fn the_namespaces_are_listed_sorted() {
        let registry = MethodRegistry::new()
            .with(Echo)
            .with(ConfigNamespace::new(&()));
        assert_eq!(registry.names(), vec!["config", "kv"]);
    }

    /// The security boundary: a credential must not reach a worker, at any depth.
    ///
    /// The nesting is the point. A flat redaction looks correct against a flat fixture and misses every
    /// credential in a real driver table, which is nested under `pipelines.<name>.config`.
    #[test]
    fn config_describe_elides_every_credential_at_any_depth() {
        let description = serde_json::json!({
            "server": { "http": "127.0.0.1:8080" },
            "redis": {
                "addrs": ["127.0.0.1:6379"],
                "password": "hunter2",
                "sentinel_password": "also-secret",
                "tls_key": "/etc/ssl/key.pem"
            },
            "pipelines": {
                "orders": {
                    "driver": "amqp",
                    "config": { "url": "amqp://user:pw@host", "api_token": "abc123" }
                }
            },
            "tls": { "certificate": "certs/a.crt", "key": "certs/a.key" },
            "storage": [
                { "name": "one", "secret": "s1" },
                { "name": "two", "authorization": "Bearer x" }
            ]
        });

        let namespace = ConfigNamespace::new(&description);
        let answer = namespace.serve(&call("config.describe"));
        assert!(answer.ok);
        let text = serde_json::to_string(&answer.result).expect("serializable");

        for secret in [
            "hunter2",
            "also-secret",
            "/etc/ssl/key.pem",
            "abc123",
            "certs/a.key",
            "s1",
            "Bearer x",
        ] {
            assert!(
                !text.contains(secret),
                "{secret:?} must not appear in the description:\n{text}"
            );
        }
        assert!(
            text.contains(ELIDED),
            "and the elision is visible rather than silent:\n{text}"
        );

        // What is *not* a credential is still described, so the answer remains useful.
        assert!(
            text.contains("127.0.0.1:8080"),
            "the listener survives:\n{text}"
        );
        assert!(
            text.contains("certs/a.crt"),
            "a certificate path is not a secret, its key is:\n{text}"
        );
        assert!(text.contains("orders"), "structure survives:\n{text}");
    }

    /// An empty or absent value stays as it was: `"password": ""` means unset, and replacing it with
    /// `<elided>` would make "not configured" indistinguishable from "configured and hidden".
    #[test]
    fn an_empty_credential_field_is_elided_too_because_the_key_is_what_is_decided_about() {
        let namespace = ConfigNamespace::new(&serde_json::json!({ "password": "" }));
        let answer = namespace.serve(&call("config.describe"));
        let text = serde_json::to_string(&answer.result).expect("serializable");
        assert!(
            text.contains(ELIDED),
            "the key is elided whether or not it holds anything: {text}"
        );
    }

    /// A driver field named `sentinel_password` or `tls_key` is caught by a substring, which is why the
    /// match is a substring and not an exact name.
    #[test]
    fn a_credential_field_with_a_prefix_is_caught() {
        for key in [
            "sentinel_password",
            "tls_key",
            "api_token",
            "aws_secret_access_key",
        ] {
            assert!(is_sensitive(key), "{key} should be treated as sensitive");
        }
        for key in ["addrs", "driver", "interval", "certificate", "path"] {
            assert!(!is_sensitive(key), "{key} is not a credential");
        }
    }
}

impl ConfigNamespace {
    /// Build the namespace from anything serializable.
    ///
    /// Redaction happens here, once, so the stored value is already safe: a handler that redacted on
    /// each call would be one code path away from returning the raw one, and the raw one would be sitting
    /// in a field waiting to be returned by accident.
    pub fn new<T: serde::Serialize>(description: &T) -> Self {
        let mut value = serde_json::to_value(description).unwrap_or(serde_json::Value::Null);
        redact(&mut value);
        ConfigNamespace { description: value }
    }
}

impl Namespace for ConfigNamespace {
    fn name(&self) -> &'static str {
        "config"
    }

    fn serve(&self, call: &Call) -> Return {
        match call.name.as_str() {
            "config.describe" => Return::ok(self.description.clone()),
            other => Return::refused(format!("the config namespace does not serve {other}")),
        }
    }
}

/// Elide every value whose key looks like a credential, at any depth.
///
/// Recursive, because a driver's configuration is nested: a queue's credentials live under
/// `pipelines.<name>.config`, and a redaction that only looked at the top level would miss every one of
/// them while looking correct on a flat test fixture.
fn redact(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if is_sensitive(key) {
                    *entry = serde_json::Value::String(ELIDED.to_string());
                } else {
                    redact(entry);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact(item);
            }
        }
        _ => {}
    }
}

/// Whether a key name is one whose value must not be described.
fn is_sensitive(key: &str) -> bool {
    key.to_ascii_lowercase()
        // `sentinel_password`, `tls-key`, `pipelines.orders.config` all split into the words a reader
        // would name them by. A key with no separator is one segment, which is what makes a bare `ca`,
        // `key` or `token` matchable at all.
        .split(['_', '-', '.', ' ', '/'])
        .filter(|segment| !segment.is_empty())
        .any(|segment| ELIDED_KEY_PARTS.contains(&segment))
}

/// The `kv` namespace: key-value storage, holding only the storages *this backend* was granted.
///
/// # One namespace holding a map, and why that is the right shape
///
/// The registry routes on the text before the **first** dot, so `kv.sessions.get` is the `kv` namespace
/// with the verb `sessions.get`. There is therefore one `kv` namespace per registry, and which storages
/// are in it is decided by the `services` list of the backend whose workers it was built for. That is
/// exactly §4.5's capability boundary: a worker cannot reach a storage it was not granted, even though
/// every storage in the process shares one namespace prefix, and the refusal names both what it asked
/// for and what it may have.
///
/// # Verbs are the PSR-16 method set, and the wire carries bytes
///
/// `kv.<storage>.get`, `.set`, `.has`, `.delete`, `.clear`, `.ttl`, `.getMultiple`, `.setMultiple`.
/// Keys and values are **base64** on the wire, because a JSON string cannot carry a byte above `0x7f`
/// and an application that compresses or encrypts what it caches has such bytes constantly. See
/// `volucer-kv`'s module documentation for why the value is opaque in the first place.
///
/// Results are objects rather than bare values — `{"value": …}`, `{"present": true}` — so a field can be
/// added later without breaking a client that reads one key.
pub struct KvNamespace {
    /// The storages these workers may reach, by the name they use in a call.
    ///
    /// A `BTreeMap` for the same reason [`MethodRegistry`] holds one: a refusal that lists the available
    /// storages must be stable between two runs, or an operator comparing two log lines sees a difference
    /// that is not there.
    storages: BTreeMap<String, Arc<dyn volucer_kv::Store>>,
}

/// A call's storage and verb, or the reason there is nothing to serve.
///
/// A type rather than a `Return` at each call site so that *routing* is decided in one function. Every
/// method below then begins with a store and a key, or with a refusal, and there is no path on which the
/// two disagree.
enum Resolved<'a> {
    Ready {
        store: &'a Arc<dyn volucer_kv::Store>,
        params: Params,
        /// The verb, without the storage component: `"get"` for `kv.sessions.get`.
        verb: &'a str,
    },
    Refused(Return),
}

impl KvNamespace {
    /// A namespace over the storages one backend may reach.
    pub fn new(storages: BTreeMap<String, Arc<dyn volucer_kv::Store>>) -> Self {
        KvNamespace { storages }
    }

    /// A namespace with no storages, which refuses every call by name.
    pub fn empty() -> Self {
        KvNamespace {
            storages: BTreeMap::new(),
        }
    }

    /// The storages these workers may reach, for a diagnostic or a test.
    pub fn storages(&self) -> Vec<&str> {
        self.storages.keys().map(String::as_str).collect()
    }

    /// Split `kv.<storage>.<verb>` and find the store, or say which part was wrong.
    ///
    /// The refusals are deliberately distinct, because they are different mistakes with different fixes:
    /// a name that is not a `kv` call at all, a storage this backend was not granted, and a verb no
    /// storage has. An operator reading one of them should not have to guess which of the three it is.
    fn resolve<'a>(&'a self, call: &'a Call) -> Resolved<'a> {
        let params = match Params::from_value(&call.params, &call.name) {
            Ok(params) => params,
            Err(err) => {
                return Resolved::Refused(Return::refused(format!(
                    "the {} method was called with {err}",
                    call.name
                )))
            }
        };

        let Some((storage, verb)) = split_storage_verb(&call.name) else {
            return Resolved::Refused(Return::refused(format!(
                "{} is not \"kv.<storage>.<verb>\", so there is nothing to route it to; {}",
                call.name,
                self.available()
            )));
        };
        if verb.is_empty() {
            return Resolved::Refused(Return::refused(format!(
                "{} names a storage but no method; {}",
                call.name,
                self.available()
            )));
        }

        match self.storages.get(storage) {
            Some(store) => Resolved::Ready {
                store,
                params,
                verb,
            },
            None => Resolved::Refused(Return::refused(format!(
                "this backend's workers may not reach the {storage:?} storage; {}",
                self.available()
            ))),
        }
    }

    /// A phrase naming the storages that are reachable, for a refusal.
    fn available(&self) -> String {
        if self.storages.is_empty() {
            // The honest answer, and a different one from "there is a storage and you may not use it":
            // this backend was granted no `kv.<name>` at all, which is a `services` list to write rather
            // than a name to correct.
            "this backend has no kv storage configured".to_string()
        } else {
            format!(
                "it may reach {}",
                self.storages
                    .keys()
                    .map(|name| format!("{name:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }

    /// The TTL a call asked for, in the units PSR-16 gives it in.
    ///
    /// `None` means "no expiry", an explicit `0` becomes [`Some(Duration::ZERO)`] — which the store turns
    /// into a delete — and anything else is seconds. The distinction is preserved all the way down rather
    /// than collapsed here, because collapsing it is how a client that asked for "delete this" gets an
    /// immortal key.
    ///
    /// A TTL beyond `volucer-kv`'s ten-year bound is **clamped by the store rather than refused here**,
    /// deliberately: an application passing `PHP_INT_MAX` means "for ever", and refusing it would turn a
    /// working cache into an error after an unrelated upgrade. See `clamp_ttl`.
    fn ttl_from(params: &Params, method: &str) -> std::result::Result<Option<Duration>, Return> {
        match params.unsigned("ttl") {
            Ok(None) => Ok(None),
            Ok(Some(0)) => Ok(Some(Duration::ZERO)),
            Ok(Some(seconds)) => Ok(Some(Duration::from_secs(seconds))),
            Err(err) => Err(Return::refused(format!("the ttl for {method} {err}"))),
        }
    }

    /// Run a store operation, turning any failure into a refusal.
    ///
    /// One place, so no method can forget: a store error that reached a caller as `ok: true` with no value
    /// would be indistinguishable from a cache miss, and a cache miss is what an application is least
    /// able to notice.
    fn attempt<T>(
        method: &str,
        op: impl FnOnce() -> volucer_kv::Result<T>,
        success: impl FnOnce(T) -> Return,
    ) -> Return {
        match op() {
            Ok(value) => success(value),
            Err(err) => {
                // Logged as well as returned, because the two audiences differ: the worker sees a refusal
                // it can act on, and the operator sees why the cache is not working. A store failure that
                // only reached the application would be a log entry nobody has.
                tracing::warn!(method, %err, "a kv call failed");
                Return::refused(format!("the store refused {method}: {err}"))
            }
        }
    }
}

/// Split `kv.<storage>.<verb>`, or `None` when the name is not a `kv` call at all.
///
/// A name with no second dot becomes the storage with an empty verb, which `KvNamespace::resolve`
/// reports as "names a storage but no method" — a distinct message from "no such storage", because for
/// `kv.sessions.` and `kv.sessions` the storage may well exist and the mistake is the verb.
fn split_storage_verb(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("kv.")?;
    Some(match rest.split_once('.') {
        Some((storage, verb)) => (storage, verb),
        None => (rest, ""),
    })
}

impl Namespace for KvNamespace {
    fn name(&self) -> &'static str {
        // `kv` and not `kv.<something>`: the registry routes on the text before the first dot, so this is
        // the *only* key under which a storage can be reached, and it is one namespace per backend rather
        // than one per storage — see the type's documentation for why.
        "kv"
    }

    fn serve(&self, call: &Call) -> Return {
        let (store, params, verb) = match self.resolve(call) {
            Resolved::Ready {
                store,
                params,
                verb,
            } => (store, params, verb),
            Resolved::Refused(refusal) => return refusal,
        };

        match verb {
            "get" => Self::get(store, &params),
            "set" => Self::set(store, &params, call),
            "has" => Self::has(store, &params),
            "delete" => Self::delete(store, &params),
            "clear" => Self::clear(store),
            "ttl" => Self::ttl(store, &params),
            "getMultiple" => Self::get_multiple(store, &params),
            "setMultiple" => Self::set_multiple(store, &params, call),
            other => Return::refused(format!(
                "no kv storage serves {other:?}; the methods are get, set, has, delete, clear, ttl, \
                 getMultiple and setMultiple"
            )),
        }
    }
}

impl KvNamespace {
    /// The key a call names, or the refusal that says what was wrong with it.
    ///
    /// One helper because every method but `clear` and the batches begins the same way, and a method that
    /// read the key itself would be a method that could disagree about whether an empty key is a key.
    fn key_of(params: &Params, method: &str) -> std::result::Result<Vec<u8>, Return> {
        match params.base64("key") {
            Ok(Some(key)) if !key.is_empty() => Ok(key),
            Ok(Some(_)) => Err(Return::refused(format!(
                "{method} was given an empty key, which cannot be stored"
            ))),
            Ok(None) => Err(Return::refused(format!(
                "{method} needs a \"key\" parameter, base64-encoded"
            ))),
            Err(err) => Err(Return::refused(format!("{method} was called with {err}"))),
        }
    }

    fn get(store: &Arc<dyn volucer_kv::Store>, params: &Params) -> Return {
        let key = match Self::key_of(params, "kv.get") {
            Ok(key) => key,
            Err(refusal) => return refusal,
        };
        Self::attempt(
            "get",
            || store.get(&key),
            |value| match value {
                Some(bytes) => Return::ok(serde_json::json!({ "value": encode_base64(&bytes) })),
                // An explicit `null` rather than an absent field, so a client can tell "there is no value"
                // from "the server sent something this client's version does not understand".
                None => Return::ok(serde_json::json!({ "value": null })),
            },
        )
    }

    fn set(store: &Arc<dyn volucer_kv::Store>, params: &Params, call: &Call) -> Return {
        let key = match Self::key_of(params, "kv.set") {
            Ok(key) => key,
            Err(refusal) => return refusal,
        };
        let value = match params.base64("value") {
            Ok(Some(value)) => value,
            // An empty *value* is a value: caching `""` is an ordinary thing to do, and refusing it would
            // be this method inventing a rule PSR-16 does not have.
            Ok(None) => {
                return Return::refused("kv.set needs a \"value\" parameter, base64-encoded")
            }
            Err(err) => return Return::refused(format!("kv.set was called with {err}")),
        };
        let ttl = match Self::ttl_from(params, &call.name) {
            Ok(ttl) => ttl,
            Err(refusal) => return refusal,
        };

        Self::attempt(
            "set",
            || store.set(&key, value, ttl),
            |()| Return::ok(serde_json::json!({ "stored": true })),
        )
    }

    fn has(store: &Arc<dyn volucer_kv::Store>, params: &Params) -> Return {
        let key = match Self::key_of(params, "kv.has") {
            Ok(key) => key,
            Err(refusal) => return refusal,
        };
        // Answered through `ttl` rather than `get`, so that `has` does not copy a value that may be a
        // megabyte out of the store to find out whether it is there. The three-state answer is what makes
        // that possible, and it is the reason `Ttl` exists rather than `Option<Duration>`.
        Self::attempt(
            "has",
            || store.ttl(&key),
            |ttl| Return::ok(serde_json::json!({ "present": ttl.is_present() })),
        )
    }

    fn delete(store: &Arc<dyn volucer_kv::Store>, params: &Params) -> Return {
        let key = match Self::key_of(params, "kv.delete") {
            Ok(key) => key,
            Err(refusal) => return refusal,
        };
        Self::attempt(
            "delete",
            || store.delete(&key),
            |deleted| Return::ok(serde_json::json!({ "deleted": deleted })),
        )
    }

    fn clear(store: &Arc<dyn volucer_kv::Store>) -> Return {
        // Takes no parameters and ignores any it is given, which is deliberate rather than an omission: a
        // client sending `{}` and one sending nothing mean the same thing, and there is no key here whose
        // typo could be mistaken for a default.
        Self::attempt(
            "clear",
            || store.clear(),
            |()| Return::ok(serde_json::json!({ "cleared": true })),
        )
    }

    fn ttl(store: &Arc<dyn volucer_kv::Store>, params: &Params) -> Return {
        let key = match Self::key_of(params, "kv.ttl") {
            Ok(key) => key,
            Err(refusal) => return refusal,
        };
        Self::attempt(
            "ttl",
            || store.ttl(&key),
            |ttl| match ttl {
                // A word rather than a number, because the three states are not a range: a client checking
                // `seconds > 0` would read "never expires" as "expired".
                volucer_kv::Ttl::Missing => Return::ok(serde_json::json!({ "ttl": "missing" })),
                volucer_kv::Ttl::Forever => Return::ok(serde_json::json!({ "ttl": "forever" })),
                volucer_kv::Ttl::In(left) => Return::ok(serde_json::json!({
                    // Rounded *up*, so a client checking `remaining >= 1` does not see zero for a key that
                    // is still there for another 900 microseconds.
                    "ttl": left.as_secs_f64().ceil() as u64,
                })),
            },
        )
    }

    fn get_multiple(store: &Arc<dyn volucer_kv::Store>, params: &Params) -> Return {
        let keys = match Self::keys_of(params, "kv.getMultiple") {
            Ok(keys) => keys,
            Err(refusal) => return refusal,
        };
        let borrowed: Vec<&[u8]> = keys.iter().map(Vec::as_slice).collect();
        Self::attempt(
            "getMultiple",
            || store.get_many(&borrowed),
            |values| {
                // Positional on the wire too, and the same length as the request: a client pairing keys
                // with values by index is what a batch is for, so the answer must not be shorter.
                let encoded: Vec<serde_json::Value> = values
                    .into_iter()
                    .map(|value| match value {
                        Some(bytes) => serde_json::Value::String(encode_base64(&bytes)),
                        None => serde_json::Value::Null,
                    })
                    .collect();
                Return::ok(serde_json::json!({ "values": encoded }))
            },
        )
    }

    fn set_multiple(store: &Arc<dyn volucer_kv::Store>, params: &Params, call: &Call) -> Return {
        let ttl = match Self::ttl_from(params, &call.name) {
            Ok(ttl) => ttl,
            Err(refusal) => return refusal,
        };

        // An object rather than a list of pairs: JSON has no pair type, and `{"<base64 key>": "<base64
        // value>"}` is what a PHP caller builds from an array with no work at all.
        let Some(entries) = params.raw("values").and_then(serde_json::Value::as_object) else {
            return Return::refused(
                "kv.setMultiple needs a \"values\" object mapping base64 keys to base64 values",
            );
        };

        let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(entries.len());
        for (encoded_key, encoded_value) in entries {
            let key = match decode_base64(encoded_key) {
                Ok(key) => key,
                Err(reason) => return Return::refused(format!("a key in the batch {reason}")),
            };
            let Some(encoded_value) = encoded_value.as_str() else {
                return Return::refused(
                    "the value for a key in the batch is not a string, so it cannot be base64; every \
                     value must be a base64-encoded string",
                );
            };
            let value = match decode_base64(encoded_value) {
                Ok(value) => value,
                Err(reason) => return Return::refused(format!("a value in the batch {reason}")),
            };
            pairs.push((key, value));
        }

        let count = pairs.len();
        Self::attempt(
            "setMultiple",
            || store.set_many(&pairs, ttl),
            move |()| Return::ok(serde_json::json!({ "stored": count })),
        )
    }

    /// The keys of a batch call, refusing a parameter that is not a list of non-empty strings.
    fn keys_of(params: &Params, method: &str) -> std::result::Result<Vec<Vec<u8>>, Return> {
        match params.string_list("keys") {
            Ok(Some(keys)) if keys.iter().any(String::is_empty) => Err(Return::refused(format!(
                "{method} was given an empty key, which cannot be stored"
            ))),
            Ok(Some(keys)) => Ok(keys.into_iter().map(String::into_bytes).collect()),
            Ok(None) => Err(Return::refused(format!(
                "{method} needs a \"keys\" list of base64-encoded keys"
            ))),
            Err(err) => Err(Return::refused(format!("{method} was called with {err}"))),
        }
    }
}

/// The `lock` namespace: one lock backend, reached by resource name.
///
/// # One namespace holding one backend, unlike `kv`
///
/// `KvNamespace` holds several storages because a deployment names them and a worker has to say which.
/// A lock is named by its **resource**, so there is nothing for a second lock backend to mean: two of
/// them would be two namespaces that do not see each other, and two processes locked on `"pdf:create"`
/// through different ones would both believe they were inside the section. `[lock]` is therefore a
/// single table, and `services = ["lock"]` is the whole of what a worker writes.
///
/// # The verbs, and the two answers that are deliberately not a boolean
///
/// `lock.acquire`, `.try_acquire`, `.release`, `.release_all`, `.exists`, `.holders`, `.update_ttl`.
///
/// An acquisition answers `{"acquired": …, "id": …, "mode": …, "remaining_ms": …}`. `acquired: false` is
/// contention and is a *successful* call, because nothing failed: the caller knows it does not hold the
/// lock, which is exactly what RoadRunner's `Response.Ok` conveys on its own wire and what its
/// documentation tells applications to check before entering the section.
///
/// A backend failure during an acquisition is **`Return::unknown_state`**, not a refusal. That is the
/// whole reason the third answer exists: a Redis that dropped a reply mid-acquire may or may not have set
/// the key, and a caller told "not acquired" would walk into the section anyway. `unknown: true` tells it
/// to resolve the question with `lock.exists` — the instruction RoadRunner's own documentation gives in
/// prose, made mechanical.
///
/// # Ids
///
/// An acquisition with no `id` gets one from [`volucer_lock::generate_id`], and the answer carries it.
/// **The caller must keep it**: an id is what release and renewal are scoped by, and a caller that loses
/// one has a lock nobody can release until its lease runs out. That is a hazard of RoadRunner's design
/// rather than of this implementation — the alternative, a server-held handle keyed by a connection, does
/// not exist here, because a call arrives on a request and the request ends.
///
/// # `ttl` is in microseconds, and zero means "until released"
///
/// Both are RoadRunner's own wire semantics, and both are stated because each is the opposite of an
/// obvious guess. Microseconds because a contended cache fill wants tens of milliseconds and seconds
/// cannot express one. Zero-means-for-ever because that is what a lock TTL of zero has always meant,
/// and it is the *opposite* of what a zero TTL means to a KV value — so a copy of the KV accessor here
/// would have produced a lock released the instant it was granted.
pub struct LockNamespace {
    /// `None` when the deployment configured no `[lock]` table. The namespace is then not installed at
    /// all — see `dispatch.rs` — so this is a belt for a test, not a path a worker can reach.
    locks: Arc<dyn volucer_lock::Locks>,
}

impl LockNamespace {
    /// A namespace over one lock backend.
    pub fn new(locks: Arc<dyn volucer_lock::Locks>) -> Self {
        LockNamespace { locks }
    }

    /// The resource a call names, or a refusal.
    ///
    /// **Required, and checked for shape before the backend sees it**, so a missing `resource` is told
    /// apart from a resource the backend refuses. The first is a client built against a different method
    /// set; the second is a name this server will not store.
    fn resource_of(params: &Params, method: &str) -> std::result::Result<String, Return> {
        match params.required_string("resource") {
            Ok(resource) => Ok(resource),
            Err(err) => Err(Return::refused(format!("{method} was called with {err}"))),
        }
    }

    /// The id a call names, or a fresh one.
    ///
    /// Absent, empty and `null` all mean "generate one", which is deliberate: a client that sends
    /// `"id": ""` because it built the params from a template means what a client that omits the key
    /// means, and refusing it would be a distinction with no purpose behind it.
    fn id_of(params: &Params) -> std::result::Result<String, Return> {
        match params.string("id") {
            Ok(Some(id)) if !id.is_empty() => Ok(id),
            Ok(_) => Ok(volucer_lock::generate_id()),
            Err(err) => Err(Return::refused(format!("the id {err}"))),
        }
    }

    /// The hold a call asks for, from `mode` and `ttl`.
    ///
    /// The mode defaults to **exclusive**, which is the reading a caller who did not think about it
    /// wants: a shared lock is an optimisation for a read path, and defaulting to it would silently stop
    /// two writers from excluding each other.
    fn hold_of(params: &Params, method: &str) -> std::result::Result<(Mode, Lease), Return> {
        let mode = match params.string("mode") {
            Ok(None) => Mode::Exclusive,
            Ok(Some(mode)) => match mode.as_str() {
                "exclusive" | "write" => Mode::Exclusive,
                "shared" | "read" => Mode::Shared,
                other => {
                    return Err(Return::refused(format!(
                        "the mode {other:?} for {method} is neither \"exclusive\" nor \"shared\""
                    )))
                }
            },
            Err(err) => return Err(Return::refused(format!("the mode for {method} {err}"))),
        };
        Ok((mode, Self::lease_of(params, method)?))
    }

    /// The lease a call asks for, from `ttl` in microseconds.
    ///
    /// Zero and absent both mean [`Lease::Forever`] — "held until released" — which is the one place
    /// where copying the KV TTL accessor would have produced a lock released the instant it was granted.
    fn lease_of(params: &Params, method: &str) -> std::result::Result<Lease, Return> {
        match params.microseconds("ttl") {
            Ok(None) => Ok(Lease::Forever),
            Ok(Some(Duration::ZERO)) => Ok(Lease::Forever),
            Ok(Some(ttl)) => Ok(Lease::for_duration(ttl)),
            Err(err) => Err(Return::refused(format!("the ttl for {method} {err}"))),
        }
    }

    /// How long an acquisition may wait, from `wait` in microseconds.
    ///
    /// Absent and zero are both "one attempt", because that is what a zero wait has always meant here and
    /// a client that omits the key is the same client that sent zero.
    fn wait_of(params: &Params, method: &str) -> std::result::Result<Duration, Return> {
        match params.microseconds("wait") {
            Ok(None) => Ok(Duration::ZERO),
            Ok(Some(wait)) => Ok(wait),
            Err(err) => Err(Return::refused(format!("the wait for {method} {err}"))),
        }
    }

    /// Run a backend operation, turning any failure into the right kind of refusal.
    ///
    /// **The one place the unknown/known distinction is decided**, so no verb can get it wrong. Three
    /// kinds of failure arrive here and they are not the same answer:
    ///
    /// - `Error::Unknown` is undetermined by construction — the driver cannot say whether the command
    ///   took effect — so it becomes [`Return::unknown_state`] whatever the verb is.
    /// - `BadResource`, `BadId` and `AlreadyHeld` are **decided**: the backend refused before it touched
    ///   anything, so nothing can be in an unknown state, and a caller may retry with a better name.
    /// - `Error::Backend` is undetermined **during an acquisition only**, because the command may have
    ///   reached the server before the failure and a lock may exist that the caller does not know it
    ///   holds. Everywhere else it is a refusal.
    ///
    /// The first version of this got the last two wrong — it reported *any* failure during an acquisition
    /// as unknown, including a name the backend refused locally — and the test for it caught that. The
    /// reason it matters is not tidiness: `unknown` tells a caller to go and ask, and a flag set on
    /// failures that are perfectly well known is a flag callers learn to ignore.
    fn attempt<T>(
        method: &str,
        resource: &str,
        acquiring: bool,
        op: impl FnOnce() -> volucer_lock::Result<T>,
        success: impl FnOnce(T) -> Return,
    ) -> Return {
        match op() {
            Ok(value) => success(value),
            Err(err) => {
                tracing::warn!(method, resource, %err, "a lock call failed");
                let undetermined = match err {
                    volucer_lock::Error::Unknown {
                        ref reason,
                        operation,
                        ..
                    } => {
                        return Return::unknown_state(format!(
                            "{method} {operation} on {resource:?} left the lock state unknown: {reason}"
                        ))
                    }
                    volucer_lock::Error::Backend { .. } => acquiring,
                    volucer_lock::Error::BadResource { .. }
                    | volucer_lock::Error::BadId { .. }
                    | volucer_lock::Error::AlreadyHeld { .. } => false,
                };

                if undetermined {
                    return Return::unknown_state(format!(
                        "{method} on {resource:?} left the lock state unknown: {err}"
                    ));
                }
                Return::refused(format!("{method} on {resource:?} failed: {err}"))
            }
        }
    }

    /// The answer shape every acquisition uses.
    ///
    /// `id` is echoed even when nothing was acquired, because a client that sent no id has just been
    /// handed one and needs it regardless of the outcome: the next call it makes must use the same one or
    /// it will contend with itself.
    fn held(holder: Option<volucer_lock::Holder>, id: &str) -> Return {
        let Some(holder) = holder else {
            return Return::ok(serde_json::json!({
                "acquired": false,
                "id": id,
                "mode": null,
                "remaining_ms": null,
            }));
        };
        Return::ok(serde_json::json!({
            "acquired": true,
            "id": holder.id,
            "mode": holder.mode.as_str(),
            "remaining_ms": holder.remaining.map(|left| left.as_millis() as u64),
        }))
    }
}

impl Namespace for LockNamespace {
    fn name(&self) -> &'static str {
        // `lock` and nothing else: a lock is named by its resource, so there is one lock namespace per
        // deployment rather than one per backend — see the type's documentation.
        "lock"
    }

    fn serve(&self, call: &Call) -> Return {
        let params = match Params::from_value(&call.params, &call.name) {
            Ok(params) => params,
            Err(err) => {
                return Return::refused(format!("the {} method was called with {err}", call.name))
            }
        };

        // The prefix is stripped here rather than left to `MethodRegistry`, because the registry routes
        // on the text before the first dot: everything below is a verb with no `lock.` on it, which is
        // what makes the match arms read as the method names a client writes.
        let verb = match call.name.strip_prefix("lock.") {
            Some(verb) if !verb.is_empty() => verb,
            _ => {
                return Return::refused(format!(
                    "{} does not name a lock method; {}",
                    call.name, METHODS
                ))
            }
        };

        match verb {
            "acquire" | "try_acquire" => {
                let resource = match Self::resource_of(&params, &call.name) {
                    Ok(resource) => resource,
                    Err(refusal) => return refusal,
                };
                let (mode, lease) = match Self::hold_of(&params, &call.name) {
                    Ok(hold) => hold,
                    Err(refusal) => return refusal,
                };
                // `try_acquire` takes no `wait` and ignores one if it is sent, rather than refusing it:
                // the two methods exist to say different things, and a client that passes both means the
                // one it called.
                let wait = if verb == "try_acquire" {
                    Duration::ZERO
                } else {
                    match Self::wait_of(&params, &call.name) {
                        Ok(wait) => wait,
                        Err(refusal) => return refusal,
                    }
                };
                let id = match Self::id_of(&params) {
                    Ok(id) => id,
                    Err(refusal) => return refusal,
                };
                Self::attempt(
                    verb,
                    &resource,
                    true,
                    || self.locks.acquire(&resource, &id, mode, lease, wait),
                    |holder| Self::held(holder, &id),
                )
            }
            "release" => {
                let resource = match Self::resource_of(&params, &call.name) {
                    Ok(resource) => resource,
                    Err(refusal) => return refusal,
                };
                let id = match Self::id_of(&params) {
                    Ok(id) => id,
                    Err(refusal) => return refusal,
                };
                Self::attempt(
                    "release",
                    &resource,
                    false,
                    || self.locks.release(&resource, &id),
                    // `released: false` is an answer, not a failure: "my lock is already gone" is what a
                    // `finally`-shaped caller most wants to hear, and the `id` is echoed so it can log
                    // which one it was.
                    |released| Return::ok(serde_json::json!({ "released": released, "id": id })),
                )
            }
            "release_all" => {
                let resource = match Self::resource_of(&params, &call.name) {
                    Ok(resource) => resource,
                    Err(refusal) => return refusal,
                };
                Self::attempt(
                    "release_all",
                    &resource,
                    false,
                    || self.locks.release_all(&resource),
                    |released| Return::ok(serde_json::json!({ "released": released })),
                )
            }
            "exists" => {
                let resource = match Self::resource_of(&params, &call.name) {
                    Ok(resource) => resource,
                    Err(refusal) => return refusal,
                };
                Self::attempt(
                    "exists",
                    &resource,
                    false,
                    || self.locks.exists(&resource),
                    |exists| Return::ok(serde_json::json!({ "exists": exists })),
                )
            }
            "holders" => {
                let resource = match Self::resource_of(&params, &call.name) {
                    Ok(resource) => resource,
                    Err(refusal) => return refusal,
                };
                Self::attempt(
                    "holders",
                    &resource,
                    false,
                    || self.locks.holders(&resource),
                    |holders| {
                        // Built rather than serialized through a derive, because `Holder::remaining` is a
                        // `Duration` and the wire carries milliseconds: a `Serialize` on the type would put
                        // seconds-and-nanos in one answer and milliseconds in another, which is how a
                        // client reading two methods comes to disagree with itself.
                        let holders: Vec<serde_json::Value> = holders
                            .into_iter()
                            .map(|holder| {
                                serde_json::json!({
                                    "id": holder.id,
                                    "mode": holder.mode.as_str(),
                                    "remaining_ms": holder.remaining.map(|left| left.as_millis() as u64),
                                })
                            })
                            .collect();
                        Return::ok(serde_json::json!({ "holders": holders }))
                    },
                )
            }
            "update_ttl" => {
                let resource = match Self::resource_of(&params, &call.name) {
                    Ok(resource) => resource,
                    Err(refusal) => return refusal,
                };
                let id = match Self::id_of(&params) {
                    Ok(id) => id,
                    Err(refusal) => return refusal,
                };
                // Only the lease is read here, and the mode is deliberately not consulted: a renewal
                // changes a deadline and nothing else. Taking `mode` and ignoring it would be an argument
                // that looks like it does something.
                let lease = match Self::lease_of(&params, &call.name) {
                    Ok(lease) => lease,
                    Err(refusal) => return refusal,
                };
                Self::attempt(
                    "update_ttl",
                    &resource,
                    false,
                    || self.locks.update_lease(&resource, &id, lease),
                    // `updated: false` means the id holds nothing, which is the answer a caller must act
                    // on: the section it is in is no longer protected.
                    |updated| Return::ok(serde_json::json!({ "updated": updated, "id": id })),
                )
            }
            other => Return::refused(format!(
                "the lock namespace does not serve {other:?}; {METHODS}"
            )),
        }
    }
}

/// The method list, written once so a refusal and the documentation cannot disagree about it.
const METHODS: &str =
    "the lock methods are acquire, try_acquire, release, release_all, exists, holders \
                       and update_ttl";

/// The `jobs` namespace: the **producer** side of a pipeline.
///
/// # Only pushes, and that is the whole of it
///
/// `jobs.push`, `jobs.push_batch`, `jobs.size` and `jobs.pipelines`. A worker cannot *reserve* a job
/// through the call plane, and the reason is the design rather than an omission: a reservation is what a
/// consumer's own channel is for (`docs/plugins.md` §6.4), because a reserved job's lease starts ticking
/// the moment it is handed out — and a job handed into a *request* would be held by a worker that is
/// already busy with something else, with its lease running down while it finishes.
///
/// So the split is: an HTTP worker pushes (a request that needs work done later), a consumer process
/// reserves and acks (which is what it exists for). Both reach the same pipeline; neither can do the
/// other's half.
///
/// # A refused push is a refusal, never an unknown
///
/// There is no `unknown` here and there is deliberately no room for one: a push either put the job in the
/// pipeline or it did not, and a driver that could not tell would be one whose caller has to reconcile.
/// The one thing a caller must be told is *which* pipeline it did not go into, which is why the refusals
/// name it.
pub struct JobsNamespace {
    /// The pipelines this backend may push to, by name.
    ///
    /// A map rather than one driver, because a queue **is** named: unlike a KV storage there is no
    /// per-name capability here — the `jobs` service grants pushing to every pipeline in the deployment —
    /// but the name still has to be resolved to a driver on every call, and this is where that happens.
    pipelines: BTreeMap<String, Arc<dyn volucer_jobs::Driver>>,
}

impl JobsNamespace {
    /// A namespace over the deployment's pipelines.
    pub fn new(pipelines: BTreeMap<String, Arc<dyn volucer_jobs::Driver>>) -> Self {
        JobsNamespace { pipelines }
    }

    /// The driver for a pipeline, or a refusal naming the ones there are.
    ///
    /// `Driver`'s own methods refuse an unknown pipeline too, and this is not a duplicate of that: the
    /// *namespace* has to know which pipelines exist before it can even choose a driver, and a caller
    /// that asked for a pipeline the deployment does not have should be told so by this side rather than by
    /// whichever driver happened to answer.
    fn driver(&self, name: &str) -> std::result::Result<&Arc<dyn volucer_jobs::Driver>, Return> {
        self.pipelines.get(name).ok_or_else(|| {
            Return::refused(format!(
                "there is no pipeline named {name:?}; {}",
                self.available()
            ))
        })
    }

    fn available(&self) -> String {
        if self.pipelines.is_empty() {
            "this deployment configures no pipelines at all".to_string()
        } else {
            format!(
                "this deployment has {}",
                self.pipelines
                    .keys()
                    .map(|name| format!("{name:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }

    /// One pushed job, from a call's params.
    ///
    /// The headers are read as a JSON object of **strings**, refused otherwise rather than stringified:
    /// a header whose value is a number is a caller that meant to put it in the payload, and coercing it
    /// would make the queue a place where types go to die. It is the same rule `Params` enforces for a
    /// typed parameter, applied to a map.
    fn pushed(params: &Params, method: &str) -> std::result::Result<(String, Pushed), Return> {
        let pipeline = match params.required_string("pipeline") {
            Ok(pipeline) => pipeline,
            Err(err) => return Err(Return::refused(format!("{method} was called with {err}"))),
        };

        let task = match params.required_string("task") {
            Ok(task) => task,
            Err(err) => return Err(Return::refused(format!("{method} was called with {err}"))),
        };

        // **Base64, like a KV value and like the lock's resource.** A job payload is the application's
        // own bytes — a serialized object, a compressed blob, an encrypted message — and a JSON string
        // cannot carry a byte above `0x7f`. This is the same decision as §6.2's, and for the same reason.
        let payload = match params.base64("payload") {
            Ok(payload) => payload.unwrap_or_default(),
            Err(err) => return Err(Return::refused(format!("{method} was called with {err}"))),
        };

        let mut job = Pushed::new(task).with_payload(payload);
        if let Some(raw) = params.raw("headers") {
            match raw {
                serde_json::Value::Null => {}
                serde_json::Value::Object(headers) => {
                    for (name, value) in headers {
                        match value.as_str() {
                            Some(value) => {
                                job = job.with_header(name.clone(), value);
                            }
                            None => {
                                return Err(Return::refused(format!(
                                    "{method} was given a header {name:?} that is a {}, and headers are \
                                     strings — put a structured value in the payload instead",
                                    json_type_name(value)
                                )))
                            }
                        }
                    }
                }
                other => {
                    return Err(Return::refused(format!(
                        "{method} was given headers that are a {}, and they must be an object of strings",
                        json_type_name(other)
                    )))
                }
            }
        }

        Ok((pipeline, job))
    }

    /// The answer shape a push uses.
    fn pushed_answer(id: &str) -> Return {
        Return::ok(serde_json::json!({ "pushed": true, "id": id }))
    }
}

impl Namespace for JobsNamespace {
    fn name(&self) -> &'static str {
        "jobs"
    }

    fn serve(&self, call: &Call) -> Return {
        let params = match Params::from_value(&call.params, &call.name) {
            Ok(params) => params,
            Err(err) => {
                return Return::refused(format!("the {} method was called with {err}", call.name))
            }
        };

        let verb = match call.name.strip_prefix("jobs.") {
            Some(verb) if !verb.is_empty() => verb,
            _ => {
                return Return::refused(format!(
                    "{} does not name a jobs method; {}",
                    call.name, METHODS_JOBS
                ))
            }
        };

        match verb {
            "push" => {
                let (pipeline, job) = match Self::pushed(&params, &call.name) {
                    Ok(pair) => pair,
                    Err(refusal) => return refusal,
                };
                let driver = match self.driver(&pipeline) {
                    Ok(driver) => driver,
                    Err(refusal) => return refusal,
                };
                match driver.push(&pipeline, job) {
                    Ok(id) => Self::pushed_answer(&id),
                    Err(err) => {
                        tracing::warn!(pipeline, %err, "a job push failed");
                        Return::refused(format!("the push to {pipeline:?} failed: {err}"))
                    }
                }
            }
            "push_batch" => {
                // The batch is one call where the loop is many, which is what a batch is *for* — and it is
                // deliberately **not atomic**: a batch whose third push fails has already pushed two, and
                // claiming otherwise would make the caller's retry duplicate them. So a partial batch is
                // reported as a partial batch, with the ids that made it.
                let Some(jobs) = params.raw("jobs").and_then(serde_json::Value::as_array) else {
                    return Return::refused(
                        "jobs.push_batch needs a \"jobs\" array of {pipeline, task, payload, headers} \
                         objects",
                    );
                };

                let mut ids: Vec<String> = Vec::with_capacity(jobs.len());
                for (index, entry) in jobs.iter().enumerate() {
                    let nested = match Params::from_value(entry, &call.name) {
                        Ok(nested) => nested,
                        Err(err) => {
                            return Return::ok(serde_json::json!({
                                "pushed": ids.len(),
                                "ids": ids,
                                "failed_index": index,
                                "error": format!("the entry at index {index} was called with {err}"),
                            }))
                        }
                    };
                    let (pipeline, job) = match Self::pushed(&nested, &call.name) {
                        Ok(pair) => pair,
                        Err(refusal) => {
                            return Return::ok(serde_json::json!({
                                "pushed": ids.len(),
                                "ids": ids,
                                "failed_index": index,
                                "error": refusal.error,
                            }))
                        }
                    };
                    let driver = match self.driver(&pipeline) {
                        Ok(driver) => driver,
                        Err(refusal) => {
                            return Return::ok(serde_json::json!({
                                "pushed": ids.len(),
                                "ids": ids,
                                "failed_index": index,
                                "error": refusal.error,
                            }))
                        }
                    };
                    match driver.push(&pipeline, job) {
                        Ok(id) => ids.push(id),
                        Err(err) => {
                            return Return::ok(serde_json::json!({
                                "pushed": ids.len(),
                                "ids": ids,
                                "failed_index": index,
                                "error": format!("the push to {pipeline:?} failed: {err}"),
                            }))
                        }
                    }
                }
                Return::ok(
                    serde_json::json!({ "pushed": ids.len(), "ids": ids, "failed_index": null }),
                )
            }
            "size" => {
                let pipeline = match params.required_string("pipeline") {
                    Ok(pipeline) => pipeline,
                    Err(err) => return Return::refused(format!("jobs.size was called with {err}")),
                };
                let driver = match self.driver(&pipeline) {
                    Ok(driver) => driver,
                    Err(refusal) => return refusal,
                };
                match (driver.size(&pipeline), driver.in_flight(&pipeline)) {
                    (Ok(size), Ok(in_flight)) => Return::ok(serde_json::json!({
                        // Both numbers, in one answer, because the pair is what an operator reads: a queue
                        // whose `size` grows and whose `in_flight` stays flat is a consumer that is stuck,
                        // and neither number says that alone.
                        "size": size,
                        "in_flight": in_flight,
                    })),
                    (Err(err), _) | (_, Err(err)) => Return::refused(format!("{err}")),
                }
            }
            "pipelines" => {
                // The namespace's own list rather than a driver's, because a deployment may have several
                // drivers and the question is which *pipelines* exist. A driver's own `pipelines()` would
                // answer for the pipelines it was given, which is the same set today and would stop being
                // so the moment a second driver arrives.
                let pipelines: Vec<serde_json::Value> = self
                    .pipelines
                    .iter()
                    .map(|(name, driver)| {
                        serde_json::json!({ "name": name, "driver": driver.name() })
                    })
                    .collect();
                Return::ok(serde_json::json!({ "pipelines": pipelines }))
            }
            other => Return::refused(format!(
                "the jobs namespace does not serve {other:?}; {METHODS_JOBS}"
            )),
        }
    }
}

/// The jobs method list, written once so a refusal and the documentation cannot disagree.
const METHODS_JOBS: &str = "the jobs methods are push, push_batch, size and pipelines";

/// The JSON type a value is, for a refusal.
///
/// A free function rather than a method, because the one message that needs it is about a header value
/// inside a map and has no `Params` to ask.
fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// The `workers` namespace: what the pools look like, and how to restart one worker.
///
/// # Why this needs no new machinery
///
/// Every method here reads something that already exists. `workers.list` reads the pools the dispatcher
/// holds; `worker.reset` calls [`crate::WorkerPool::restart`], which the pool already had for its own
/// tests and for an operator. This is the payoff of making the call path the foundation: a control plane
/// is available from the first commit rather than after KV is written.
///
/// # Why it is a *separate* registry entry and not part of `config`
///
/// Because capability is per backend (`docs/plugins.md` §4.5). A worker serving the public gets a registry
/// with no `workers` namespace, so it cannot restart its siblings; an operator's worker gets one that can.
/// Splitting the namespaces is what makes that a configuration decision rather than a code change.
pub struct WorkersNamespace {
    /// The pools, by backend name. Held weakly rather than as a dispatcher reference, so a registry can
    /// be built before the dispatcher that owns it — which is the order `serve` needs, since the handler
    /// is installed on channels as pools are built.
    pools: Arc<dyn PoolLookup>,
}

/// What the `workers` namespace can ask about the pools, without knowing what a `Dispatcher` is.
///
/// A trait rather than a `&Dispatcher` so that the registry does not depend on the dispatcher's whole
/// surface, and so a test can supply two pools and no configuration. The dispatcher implements it; see
/// `dispatch.rs`.
pub trait PoolLookup: Send + Sync {
    /// One entry per worker, across every pool, in a stable order.
    fn workers(&self) -> Vec<WorkerStatus>;

    /// Restart one worker by its index in [`PoolLookup::workers`].
    ///
    /// Returns why it could not, rather than a `Result`, because the caller's only use for the failure is
    /// to put it in a `Return`.
    fn restart(&self, index: usize) -> std::result::Result<(), String>;
}

/// One worker, as `workers.list` describes it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WorkerStatus {
    /// The backend the pool belongs to, e.g. `app`.
    pub backend: String,
    /// The worker's index within its pool, which is what `worker.reset` takes.
    pub index: usize,
    /// The process id, for correlating with `ps`.
    pub pid: u32,
    /// Whether the slot is free.
    pub idle: bool,
    /// Requests this worker has served since it booted.
    pub served: u64,
    /// The peak resident set the worker last reported, in bytes.
    pub memory_bytes: u64,
    /// The PHP version and SAPI it reported at handshake, as one string.
    pub runtime: String,
}

impl WorkersNamespace {
    pub fn new(pools: Arc<dyn PoolLookup>) -> Self {
        WorkersNamespace { pools }
    }
}

impl Namespace for WorkersNamespace {
    fn name(&self) -> &'static str {
        "workers"
    }

    fn serve(&self, call: &Call) -> Return {
        match call.name.as_str() {
            "workers.list" => {
                // Serialized through the same codec boundary as everything else, so a field added to
                // `WorkerStatus` appears in the answer without a second place to remember.
                match serde_json::to_value(self.pools.workers()) {
                    Ok(value) => Return::ok(value),
                    Err(err) => {
                        Return::refused(format!("the worker list could not be encoded: {err}"))
                    }
                }
            }
            "worker.reset" => {
                // The index is taken from the params rather than from a name, because a name would have to
                // be unique across every pool and a slot's pid changes when it is replaced. The index is
                // what `workers.list` gives out, so the round trip is "list, then reset what you saw".
                let Some(index) = call.params.get("index").and_then(serde_json::Value::as_u64)
                else {
                    return Return::refused(
                        "worker.reset needs an integer \"index\"; take it from workers.list",
                    );
                };
                let Ok(index) = usize::try_from(index) else {
                    return Return::refused(format!(
                        "worker index {index} does not fit this platform"
                    ));
                };
                match self.pools.restart(index) {
                    Ok(()) => Return::ok(serde_json::json!({ "restarted": index })),
                    // A refusal, not an unknown state: `restart` either replaced the worker or it did
                    // not, and the pool knows which. Reporting `unknown` here would teach a caller to
                    // ignore the flag on exactly the call where it matters most.
                    Err(reason) => Return::refused(reason),
                }
            }
            other => Return::refused(format!("the workers namespace does not serve {other}")),
        }
    }
}
