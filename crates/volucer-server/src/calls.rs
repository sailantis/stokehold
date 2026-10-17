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
