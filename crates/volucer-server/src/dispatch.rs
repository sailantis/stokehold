//! Request dispatch: choosing a backend and translating errors.
//!
//! Route-level selection is deliberately simple for now — a default backend plus
//! per-host or per-path overrides, which is what the design calls for. Anything
//! more elaborate belongs with the front-ends, where the request is understood.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, Weak};

use volucer_core::config::{
    BackendConfig, Config, KvStorageConfig, LockConfig, PipelineConfig, RouteConfig,
};
use volucer_fcgi::{cgi, Request, Response};

use crate::backend::{Backend, UpstreamBody};
use crate::calls::{
    ConfigNamespace, JobsNamespace, KvNamespace, LockNamespace, MethodRegistry, PoolLookup,
    WorkerStatus, WorkersNamespace,
};
use crate::error::{Result, ServerError};

/// Routes requests to backends.
#[derive(Debug)]
pub struct Dispatcher {
    backends: BTreeMap<String, Backend>,
    default: String,
    routing: cgi::Routing,
    /// The route table from the configuration, in declaration order.
    ///
    /// Kept here rather than in each front-end so there is exactly one place a
    /// backend is chosen, which is what lets the FastCGI and HTTP front-ends
    /// behave identically for the same request.
    routes: Vec<RouteConfig>,
    /// What a `worker.reset` call needs in order to find a pool.
    ///
    /// **Weak handles to the pools, and the weakness is the point.** The handler for `worker.reset`
    /// lives inside each worker's channel, which is owned by that worker's pool — so a strong handle
    /// here would be the cycle pool → registry → directory → pool. The cycle would be invisible in every
    /// test, and what it costs is that the pools never drop, so [`WorkerPool::shutdown`] never runs and
    /// every worker process is leaked at exit, because `Drop` is what reaps them.
    ///
    /// Filled in at the end of [`Dispatcher::from_config`], from the pools that function has just
    /// started — no self-reference, no deferred initialisation a caller could forget.
    directory: Arc<PoolDirectory>,
    /// The queues, for the tests and for a future consumer side to reach from outside a worker.
    ///
    /// A strong handle, unlike the directory above, and the difference is a real one rather than an
    /// oversight: a driver is not the owner of anything that owns the dispatcher — no pool holds a
    /// driver, so there is no cycle to avoid. The consumer side of step 5 will poll these, which is why
    /// they are kept rather than rebuilt.
    queues: Option<Arc<JobsPlane>>,
}

/// What `worker.reset` reaches the pools through, without holding one alive.
///
/// # Why weak, and why the list is filled in last
///
/// The handler for `worker.reset` lives inside each worker's channel, which is owned by that worker's
/// pool — so a strong handle from here back to a pool would be the cycle
/// pool → registry → directory → pool. The cycle would be invisible in every test, and what it costs is
/// that the pools never drop, so [`WorkerPool::shutdown`] never runs and every worker process is leaked
/// at exit, because `Drop` is what reaps them.
///
/// So the pools are held weakly, and the list is filled in by [`Dispatcher::from_config`] once the pools
/// exist. The consequence is worth stating: a call that arrives *during startup* sees a list that is not
/// populated yet, and `workers.list` answers with the pools that are already up rather than with all of
/// them. That is the honest answer — a pool that has not finished booting its workers is not a pool an
/// operator can act on — and the window closes the moment the last backend is up.
pub struct PoolDirectory {
    /// Pools by backend name, in the configuration's order.
    pools: OnceLock<Vec<(String, Weak<crate::WorkerPool>)>>,
}

impl std::fmt::Debug for PoolDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolDirectory")
            .field(
                "pools",
                &self
                    .pools
                    .get()
                    .map(|pools| pools.iter().map(|(name, _)| name).collect::<Vec<_>>()),
            )
            .finish()
    }
}

impl Default for PoolDirectory {
    fn default() -> Self {
        PoolDirectory {
            pools: OnceLock::new(),
        }
    }
}

/// The one place that knows how `workers.list` enumerates pools.
///
/// **The order is the backend map's, and it must not change.** `worker.reset` takes an index into this
/// list, so an enumeration that reordered itself between two calls would make "restart worker 2" mean
/// two different workers depending on when it was sent. The `BTreeMap` the backends live in gives that
/// for free, which is the same reason [`MethodRegistry`]'s namespaces are one.
impl PoolDirectory {
    /// Record the pools, once.
    ///
    /// Called by [`Dispatcher::from_config`] after the backends are built. The second call is ignored
    /// rather than panicking, because the only caller is that one function and a panic there would be a
    /// startup crash for a condition that cannot occur.
    fn install(&self, pools: Vec<(String, Weak<crate::WorkerPool>)>) {
        let _ = self.pools.set(pools);
    }

    /// Every live pool, by backend name, in configuration order.
    ///
    /// A pool whose strong handle has gone is skipped rather than resurrected: the `Weak` upgrade is a
    /// *borrow* that fails once the last strong reference goes, which is exactly the moment the pools
    /// are being torn down.
    fn visit<T>(&self, mut f: impl FnMut(&str, &crate::WorkerPool) -> T) -> Vec<T> {
        match self.pools.get() {
            Some(pools) => pools
                .iter()
                .filter_map(|(backend, pool)| pool.upgrade().map(|pool| f(backend.as_str(), &pool)))
                .collect(),
            None => Vec::new(),
        }
    }
}

/// Build every configured storage, once.
///
/// The name in the map is the name a call uses — `kv.<name>` — and the `Arc` is what lets two backends
/// granted the same storage share one map. A per-backend copy would be a cache that is correct and
/// useless: both backends would work perfectly and see nothing of each other's writes.
///
/// **Fallible, and the one driver that can fail is why.** A Redis that cannot be reached is a startup
/// failure, exactly as an unreachable worker command is, rather than a 500 on the first request that reads
/// a cache. The worker pool follows the same rule for the same reason: a misconfiguration belongs where the
/// configuration is read.
fn build_stores(config: &Config) -> Result<BTreeMap<String, Arc<dyn volucer_kv::Store>>> {
    let mut stores: BTreeMap<String, Arc<dyn volucer_kv::Store>> = BTreeMap::new();
    for (name, storage) in &config.kv {
        let store: Arc<dyn volucer_kv::Store> = match storage {
            KvStorageConfig::Memory(memory) => {
                Arc::new(volucer_kv::MemoryStore::with_max_bytes(memory.max_bytes))
            }
            #[cfg(feature = "redis")]
            KvStorageConfig::Redis(redis) => {
                // Built here rather than per backend, so two backends granted the same storage share one
                // pool. A per-backend pool would be `pool_size` connections *per backend* — 160 sockets for
                // ten backends — and would still be correct, which is why nothing else would notice.
                let store = volucer_kv::RedisStore::connect(&redis.url, redis.pool_size, name)
                    .map_err(|err| ServerError::Misconfigured {
                        backend: format!("kv.{name}"),
                        reason: err.to_string(),
                    })?;
                Arc::new(store)
            }
        };
        tracing::info!(
            storage = %name,
            driver = %storage.driver(),
            "kv storage ready"
        );
        stores.insert(name.clone(), store);
    }
    Ok(stores)
}

/// Build the lock backend, once.
///
/// # Why this is not a map like `build_stores`
///
/// A lock is named by its **resource**, so one deployment has one lock namespace and there is nothing for
/// a second backend to mean: two of them would not see each other's locks, and two processes locked on
/// `"pdf:create"` through different ones would both believe they were inside the section. `[lock]` is a
/// single table for that reason, and this returns a single backend for the same one.
///
/// Falls back to [`volucer_lock::MemoryLocks`] when no `[lock]` table is configured — but the caller only
/// uses the result for backends that were *granted* `lock`, and `Config::validate` refuses that grant
/// without a table. So the fallback is for a `Config` built in a test and never validated, and it is the
/// memory driver rather than a refusal because a test wanting to exercise a lock namespace should not
/// have to write a `[lock]` table to do it.
fn build_locks(config: &Config) -> Result<Option<Arc<dyn volucer_lock::Locks>>> {
    let Some(lock) = &config.lock else {
        return Ok(None);
    };

    let locks: Arc<dyn volucer_lock::Locks> = match lock {
        LockConfig::Memory(_) => Arc::new(volucer_lock::MemoryLocks::new()),
        #[cfg(feature = "redis")]
        LockConfig::Redis(redis) => {
            // Built here rather than lazily on the first acquisition: an unreachable server is a startup
            // failure, exactly as it is for a KV storage, rather than a 500 on the first request that
            // needs a lock — and `connect` is what `PING`s to find out.
            let locks = volucer_lock::RedisLocks::connect(
                &redis.url,
                redis.pool_size,
                std::time::Duration::from_millis(redis.read_timeout_ms),
            )
            .map_err(|err| ServerError::Misconfigured {
                backend: "lock".to_string(),
                reason: err.to_string(),
            })?;
            Arc::new(locks)
        }
    };
    tracing::info!(driver = %lock.driver(), "lock backend ready");
    Ok(Some(locks))
}

/// Build the queues, once.
///
/// # Why this is one object rather than a map the registry holds
///
/// The drivers are per-pipeline and the *namespace* is one, because a push names the pipeline and the
/// namespace resolves it. So the map is built here and handed over whole — which is also what lets the
/// consumer side of step 5's second half be added without changing this shape: the same drivers will be
/// polled by pools, and a driver behind an `Arc<dyn Driver>` can be both.
///
/// # Why a pipeline's driver is chosen per table
///
/// Unlike `[lock]`, where a second backend would be a second namespace that does not see the first, two
/// pipelines with different drivers is the *normal* case: a Redis-backed mail queue and an in-memory
/// report queue in one deployment is what the configuration is for. So each `[jobs.pipelines.<name>]`
/// names its own driver, and `build_queues` builds each.
fn build_queues(config: &Config) -> Result<Option<Arc<JobsPlane>>> {
    let Some(jobs) = &config.jobs else {
        return Ok(None);
    };

    let mut pipelines: BTreeMap<String, Arc<dyn volucer_jobs::Driver>> = BTreeMap::new();
    for (name, pipeline) in &jobs.pipelines {
        let driver: Arc<dyn volucer_jobs::Driver> = match pipeline {
            // Built per pipeline rather than per deployment, because a driver here *is* a pipeline's
            // storage: two memory pipelines are two independent queues, and sharing one driver between
            // them would make a push to either appear in both.
            PipelineConfig::Memory(_) => Arc::new(volucer_jobs::MemoryDriver::single(name.clone())),
        };
        tracing::info!(pipeline = %name, driver = %pipeline.driver(), "queue ready");
        pipelines.insert(name.clone(), driver);
    }

    Ok(Some(Arc::new(JobsPlane { pipelines })))
}

/// The pipelines, and which of them this server consumes.
///
/// One object rather than the two halves being passed separately, because the consumer side of step 5 will
/// need the same drivers the producer namespace holds — and threading two maps through three signatures
/// to avoid one struct is how a seam gets missed.
pub struct JobsPlane {
    pipelines: BTreeMap<String, Arc<dyn volucer_jobs::Driver>>,
}

impl JobsPlane {
    /// The pipelines the producer namespace pushes to.
    ///
    /// A clone of the map, not a `&`: [`JobsNamespace`] owns what it is given, and the `Arc`s it copies
    /// point at the same drivers — so a push through the namespace and a poll by a consumer are talking to
    /// one queue, which is the property that matters.
    fn pipelines(&self) -> BTreeMap<String, Arc<dyn volucer_jobs::Driver>> {
        self.pipelines.clone()
    }

    /// The names this deployment consumes, in the order the configuration wrote them.
    ///
    /// For the consumer side, and it is here rather than read from `Config` at the point of use so that a
    /// name which is consumed but has no pipeline cannot reach this far — `Config::validate` refuses it.
    pub fn consumed(&self) -> Vec<(&str, Arc<dyn volucer_jobs::Driver>)> {
        self.pipelines
            .iter()
            .map(|(name, driver)| (name.as_str(), Arc::clone(driver)))
            .collect()
    }
}

impl std::fmt::Debug for JobsPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written because `dyn Driver` is not `Debug`, and the names are more useful anyway.
        f.debug_struct("JobsPlane")
            .field("pipelines", &self.pipelines.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Build one worker backend's method registry from its `services` list.
///
/// # Why the `config.describe` document is assembled here
///
/// The value a worker is told about its deployment can only be decided in one place if it is decided
/// *once*, at startup, rather than assembled per call from live state. Assembling it here also means the
/// redaction in [`ConfigNamespace::new`] runs once, and that the answer cannot drift as the server changes
/// underneath it — a worker that asks twice gets the same document, which is what makes it safe to
/// cache the answer at all.
///
/// # Why an unknown name cannot reach this function
///
/// It is checked by [`Config::validate`](volucer_core::config::Config::validate) against
/// [`CALL_NAMESPACES`](volucer_core::config::CALL_NAMESPACES) before a configuration is ever served, so
/// the fall-through arm here is unreachable in a running server. It exists because this function is also
/// reachable from a `Config` built in a test and never validated, and a silently ignored name would make
/// that test lie.
fn worker_registry(
    services: &[String],
    config: &Config,
    directory: &Arc<PoolDirectory>,
    stores: &BTreeMap<String, Arc<dyn volucer_kv::Store>>,
    locks: &Option<Arc<dyn volucer_lock::Locks>>,
    jobs: &Option<Arc<JobsPlane>>,
) -> MethodRegistry {
    let mut registry = MethodRegistry::new();
    // Collected rather than installed per entry, because there is **one** `kv` namespace per registry and
    // a second `with` call for it would replace the first. A backend granted two storages must find both
    // in the one namespace, so the map is built first and installed once below.
    let mut granted: BTreeMap<String, Arc<dyn volucer_kv::Store>> = BTreeMap::new();

    for service in services {
        match service.as_str() {
            "config" => registry = registry.with(ConfigNamespace::new(config)),
            "workers" => {
                registry = registry.with(WorkersNamespace::new(
                    Arc::clone(directory) as Arc<dyn PoolLookup>
                ))
            }
            // Installed from the built backend rather than from the configuration, for the reason the KV
            // arm below skips a storage it cannot find: a worker granted a lock plane that could not be
            // built must be refused by name, not handed a namespace that answers every call with "no lock
            // backend configured" — which reads like a configuration mistake rather than the internal one
            // it would be. `Config::validate` refuses the grant in the first place, so this arm is only
            // reachable from a `Config` built in a test and never validated.
            "lock" => match locks {
                Some(locks) => registry = registry.with(LockNamespace::new(Arc::clone(locks))),
                None => tracing::error!(
                    "a worker is granted the `lock` service but no lock backend was built; its calls \
                     will be refused, which should have been impossible"
                ),
            },
            "jobs" => match jobs {
                Some(jobs) => registry = registry.with(JobsNamespace::new(jobs.pipelines())),
                None => tracing::error!(
                    "a worker is granted the `jobs` service but no queue was built; its calls will be \
                     refused, which should have been impossible"
                ),
            },
            other => match other.strip_prefix("kv.") {
                // A name checked against the `[kv.*]` tables when the configuration was validated, so a
                // miss here is a store that was not built rather than a name never checked. Skipping is
                // right: installing an empty `kv` namespace would make `services = ["kv.sessions"]` look
                // granted while every call was refused with "no kv storage configured", which reads like
                // a configuration mistake rather than the internal one it is.
                Some(storage) => match stores.get(storage) {
                    Some(store) => {
                        granted.insert(storage.to_string(), Arc::clone(store));
                    }
                    None => tracing::error!(
                        storage = %storage,
                        "a configured kv service has no store built for it; a worker asking for it \
                         will be refused, which should have been impossible"
                    ),
                },
                None => {
                    tracing::warn!(
                        service = %other,
                        "a configured service has no implementation and is ignored; this should \
                         have been refused when the configuration was validated"
                    );
                }
            },
        }
    }

    if !granted.is_empty() {
        registry = registry.with(KvNamespace::new(granted));
    }
    if !registry.is_empty() {
        tracing::debug!(
            namespaces = ?registry.names(),
            "installing a call handler on this backend's workers"
        );
    }
    registry
}

impl PoolLookup for PoolDirectory {
    fn workers(&self) -> Vec<WorkerStatus> {
        self.visit(|backend, pool| {
            pool.worker_status()
                .into_iter()
                .map(|mut status| {
                    status.backend = backend.to_string();
                    status
                })
                .collect::<Vec<_>>()
        })
        .into_iter()
        .flatten()
        .collect()
    }

    fn restart(&self, index: usize) -> std::result::Result<(), String> {
        // One walk, counting slots as it goes, because the index is global across pools and the pools
        // are the only thing that knows how many workers each holds. The count is finished even when
        // the index is not found, so the refusal can say how many there are.
        let mut seen = 0usize;
        let mut found = None;
        self.visit(|_, pool| {
            let len = pool.len();
            if found.is_none() && index >= seen && index < seen + len {
                found = Some(pool.restart(index - seen));
            }
            seen += len;
        });
        match found {
            Some(Ok(())) => Ok(()),
            Some(Err(err)) => Err(err.to_string()),
            None => Err(format!(
                "worker index {index} does not exist; there are {seen} workers, and \
                 workers.list gives the index to use"
            )),
        }
    }
}

impl Dispatcher {
    /// Build every configured backend and prepare dispatch.
    ///
    /// Backends are started here rather than lazily, so a misconfiguration is
    /// reported at startup instead of on the first request that happens to hit
    /// the broken one.
    pub fn from_config(config: &Config, routing: cgi::Routing) -> Result<Self> {
        // The directory is built first and installed on every worker channel as its pool boots, because
        // a channel needs its handler before the handshake and the handshake is inside
        // `WorkerPool::start`. It reaches the pools through a `Weak` filled in at the end of this
        // function, which is what keeps the reference out of both directions at once.
        //
        // A registry is built **per worker backend**, from that backend's own `services` list, so a
        // worker facing the public gets one that cannot restart its siblings. That is
        // `docs/plugins.md` §4.5, and building it once for the whole process would be the shape it
        // refuses.
        // The stores are built before any backend, because a backend's registry needs them: a worker
        // granted `kv.sessions` has to find the store already there. Built once and shared, so two
        // backends granted the same storage share one map rather than getting a private copy that
        // silently diverges — which would be the worst possible bug in a cache, since both would work.
        let stores = build_stores(config)?;
        // And the lock backend, for the same reason: a worker granted `lock` has to find it already
        // there, and the registry is built per backend while the backend itself is built once.
        let locks = build_locks(config)?;
        // And the queues, for the same reason again: the registry is built per backend and the pipelines
        // are built once, so a worker granted `jobs` finds the drivers already there.
        let jobs = build_queues(config)?;

        let directory = Arc::new(PoolDirectory::default());
        let mut backends = BTreeMap::new();
        let mut registries: BTreeMap<String, Arc<MethodRegistry>> = BTreeMap::new();
        for (name, backend_config) in &config.backends {
            let registry = match backend_config {
                BackendConfig::Worker(worker) => {
                    let registry = Arc::new(worker_registry(
                        &worker.services,
                        config,
                        &directory,
                        &stores,
                        &locks,
                        &jobs,
                    ));
                    registries.insert(name.clone(), Arc::clone(&registry));
                    Some(registry)
                }
                // The other three kinds have no channel, so there is nothing to install a handler
                // on and nothing that can call. Not an oversight: `spawn_cgi` and `external` speak
                // FastCGI to PHP, which has no call surface at all, and an upstream is somebody
                // else's process.
                _ => None,
            };
            backends.insert(
                name.clone(),
                Backend::from_config_with(name, backend_config, registry)?,
            );
        }

        // The pools are recorded now that they are all up, and recorded *weakly*: see
        // `PoolDirectory` for why a strong handle here would leak every worker process.
        directory.install(
            backends
                .iter()
                .filter_map(|(name, backend)| match backend {
                    Backend::Worker { pool, .. } => Some((name.clone(), Arc::downgrade(pool))),
                    _ => None,
                })
                .collect(),
        );

        // No backends at all is a legitimate deployment rather than a mistake: a
        // listener with nothing to dispatch to is how static files are served
        // without PHP, and with no backends there is also no route that could
        // select one, because `Config::validate` requires every route to name a
        // configured backend. It is allowed here so that `check` and `serve`
        // agree about the file — `check` reports it as a warning, because a server
        // that can answer nothing but static files is usually a configuration
        // still being written rather than a finished one.
        let default = match config.default_backend.clone() {
            Some(name) => name,
            None if backends.is_empty() => String::new(),
            None => {
                return Err(ServerError::Misconfigured {
                    backend: "<default>".to_string(),
                    reason: "no default_backend is configured".to_string(),
                })
            }
        };

        if !backends.contains_key(&default) && !backends.is_empty() {
            return Err(ServerError::Misconfigured {
                backend: default,
                reason: "default_backend does not match any configured backend".to_string(),
            });
        }

        Ok(Dispatcher {
            backends,
            default,
            routing,
            routes: config.routes.clone(),
            directory,
            queues: jobs,
        })
    }

    /// The pools this backend's workers may address through a call, or `None` if they may not.
    ///
    /// The one accessor that answers a question about the *deployment* rather than about a request, and it
    /// exists because the dispatcher itself has to answer a call: `volucer workers` and `volucer reset`
    /// on the command line are clients of this same machinery (`docs/plugins.md` §4.6), and a CLI
    /// that re-derived "which pool is worker 3 in" would be a second implementation of the one rule
    /// `worker.reset` exists to get right.
    pub fn pools(&self) -> Option<Arc<PoolDirectory>> {
        self.backends.values().find_map(|backend| match backend {
            Backend::Worker { .. } => Some(Arc::clone(&self.directory)),
            _ => None,
        })
    }

    /// The queues this deployment built, if it has a `[jobs]` table.
    ///
    /// For the tests, and for whatever polls them next — the consumer half of step 5. `None` means no
    /// `[jobs]` table, which is the same distinction `[lock]` makes: absent is not "the default driver".
    pub fn queues(&self) -> Option<Arc<JobsPlane>> {
        self.queues.as_ref().map(Arc::clone)
    }

    /// Whether any backend was configured.
    ///
    /// False is the static-only deployment, where a request that names no file
    /// has nowhere to go. The front-ends answer 500 for one, and the CLI warns at
    /// startup, so an operator is not left reading a bare "internal error".
    pub fn has_backends(&self) -> bool {
        !self.backends.is_empty()
    }

    /// The routing table in use, so a front-end can resolve paths for logging.
    pub fn routing(&self) -> &cgi::Routing {
        &self.routing
    }

    /// The default backend's name.
    pub fn default_backend(&self) -> &str {
        &self.default
    }

    /// Whether the named backend is a worker pool.
    ///
    /// A worker reads `script_filename` as metadata and routes on `REQUEST_URI`, so a
    /// path naming no script is not a file the router needs to resolve for it — which is
    /// what lets a route to a worker skip the `stat` entirely. An unknown name is not a
    /// worker: a name that selects nothing cannot vouch for the safety of anything.
    pub fn is_worker(&self, backend: &str) -> bool {
        self.backends.get(backend).is_some_and(Backend::is_worker)
    }

    /// Whether the named backend reads the path as a file to execute.
    ///
    /// The question a route's `skip_stat` turns on. Not the negation of [`Self::is_worker`]: an
    /// upstream does not interpret the path either, but for its own reason, and a route to one must
    /// skip the `stat` just as a worker's does. An unknown name answers `true` — a backend that is
    /// not there cannot vouch for skipping a check.
    pub fn interprets_path(&self, backend: &str) -> bool {
        self.backends
            .get(backend)
            .is_none_or(Backend::interprets_path)
    }

    /// Names of every backend, for diagnostics.
    pub fn backend_names(&self) -> Vec<&str> {
        self.backends.values().map(Backend::name).collect()
    }

    /// The backend that should serve this request.
    ///
    /// A route may override the default, and both front-ends reach the decision
    /// through `volucer_core::config::select_backend`, so a request gets the same
    /// answer whichever protocol carried it and whichever front-end asked. That
    /// sharing is the point: the two used to have their own copy of the precedence
    /// rule, and the HTTP one had grown `path_suffix` and the exact-host tiebreak
    /// while this one had not.
    pub fn backend_for(&self, request: &Request) -> &str {
        self.backend_for_routes(&self.routes, request)
    }

    /// The backend that should serve an HTTP request, asked without a CGI environment.
    ///
    /// The same route rule [`Self::backend_for`] uses, reached from the other front-end. It exists
    /// because the HTTP front-end has `Parts`, not a CGI environment, and building one just to ask
    /// this question would be a second, lossy representation of a request that already exists —
    /// the flattening this whole step is about avoiding.
    ///
    /// `host` is the `Host` header as sent. `path` is the request path *without* its query or
    /// fragment, and a caller that forgets is a routing difference between the two front-ends for
    /// `/api?x=1`, so the rule lives here rather than in each caller.
    pub fn backend_for_target(&self, host: Option<&str>, path: &str) -> &str {
        let path = strip_query(path);
        volucer_core::config::select_backend(&self.routes, host, path)
            .unwrap_or(self.default.as_str())
    }

    /// [`Self::backend_for_target`], against a site's own route table.
    pub fn backend_for_target_routes<'a>(
        &'a self,
        routes: &'a [RouteConfig],
        host: Option<&str>,
        path: &str,
    ) -> &'a str {
        let path = strip_query(path);
        volucer_core::config::select_backend(routes, host, path).unwrap_or(self.default.as_str())
    }

    /// Whether the named backend is an HTTP upstream.
    ///
    /// The front-end's branch, and it has to be at the front-end: every other backend is a blocking
    /// process on a pipe, so making the dispatcher's *type* async to accommodate the one that is
    /// not would drag `spawn_blocking` into the type system of all four. An unknown name answers
    /// `false`, so a name that selects nothing keeps the CGI path's behaviour.
    pub fn is_upstream(&self, backend: &str) -> bool {
        matches!(self.backends.get(backend), Some(Backend::Upstream { .. }))
    }

    /// The client for a named upstream backend.
    ///
    /// `None` for the three CGI kinds, so a caller that asks for the wrong one cannot accidentally
    /// reach an origin — and the client is borrowed rather than cloned, because it shares its pool
    /// through `&self` and cloning it per request would be an `Arc` bump for nothing.
    pub fn upstream(
        &self,
        backend: &str,
    ) -> Option<&volucer_upstream::UpstreamClient<UpstreamBody>> {
        match self.backends.get(backend) {
            Some(Backend::Upstream { client, .. }) => Some(client),
            _ => None,
        }
    }

    /// Send an HTTP request to a named upstream backend.
    ///
    /// The one place the two request paths meet, and the reason it is a method here rather than in
    /// the front-end: choosing the backend, knowing whether it is an upstream, and holding the
    /// client are all the dispatcher's business, so the listener has exactly one call to make for
    /// either path.
    pub async fn dispatch_upstream(
        &self,
        backend: &str,
        request: volucer_upstream::UpstreamRequest<UpstreamBody>,
    ) -> Result<volucer_upstream::UpstreamResponse> {
        let Some(client) = self.upstream(backend) else {
            // Reached only if a caller asked for the wrong kind, which the front-end's own branch
            // makes unreachable in practice. Naming it rather than panicking keeps a future caller's
            // mistake a 500 with a reason instead of a crashed server.
            return Err(match self.backends.get(backend) {
                Some(_) => ServerError::WrongProtocol {
                    backend: backend.to_string(),
                },
                None => ServerError::NoWorker {
                    backend: backend.to_string(),
                },
            });
        };
        client
            .request(request)
            .await
            .map_err(|source| ServerError::Upstream {
                backend: backend.to_string(),
                source,
            })
    }

    /// Whether this deployment configures an upstream backend at all.
    ///
    /// The capability check the CLI makes before refusing a whole configuration: without it,
    /// `serve` would have to reject every deployment, including the ones that never mention an
    /// upstream. It answers for the *deployment*, not for a route, because the thing that is
    /// missing is a request path rather than a routing decision.
    pub fn has_upstream(&self) -> bool {
        self.backends
            .values()
            .any(|backend| matches!(backend, Backend::Upstream { .. }))
    }

    /// [`Dispatcher::backend_for`], against a site's own route table.
    ///
    /// A site carries its own routes, so a request from nginx must be routed by the table
    /// of the site its `Host` names rather than by the server's. Passing the table in keeps
    /// one rule for both front-ends; what differs behind nginx is only that the document root
    /// is already settled, because nginx resolved `SCRIPT_FILENAME` before forwarding.
    pub fn backend_for_routes<'a>(
        &'a self,
        routes: &'a [RouteConfig],
        request: &Request,
    ) -> &'a str {
        let host = param(request, "HTTP_HOST").or_else(|| param(request, "SERVER_NAME"));
        let path = param(request, "REQUEST_URI");
        let path = path.as_deref().map(strip_query).unwrap_or("/");

        volucer_core::config::select_backend(routes, host.as_deref(), path)
            // A route that matches nothing leaves the decision to `default_backend`, which is
            // the name this deployment's configuration gave — not necessarily the first route.
            .unwrap_or(self.default.as_str())
    }

    /// Send a request to a backend chosen from a given route table.
    ///
    /// The routes are a parameter rather than a field because a site carries its own: a request
    /// from nginx must be routed by the table of the site its `Host` names. One rule, two callers
    /// — which is what keeps the two front-ends interchangeable.
    pub fn dispatch_with_routes(
        &self,
        routes: &[RouteConfig],
        request: &Request,
    ) -> Result<Response> {
        let name = self.backend_for_routes(routes, request);
        self.dispatch_to(name, request)
    }

    /// Send a request to the backend its route selects.
    pub fn dispatch(&self, request: &Request) -> Result<Response> {
        // The name is borrowed rather than cloned into a `String`. `backend_for` and
        // `dispatch_to` both only read `self`, so the borrow it returns is still live
        // when `dispatch_to` is called; owning the name was an allocation per request
        // that bought nothing.
        let name = self.backend_for(request);
        self.dispatch_to(name, request)
    }

    /// Send a request to a named backend.
    pub fn dispatch_to(&self, name: &str, request: &Request) -> Result<Response> {
        let backend = self
            .backends
            .get(name)
            // A static-only configuration reaches here with no backends at all.
            // Reported as `NoWorker` rather than as a misconfiguration, because by
            // the time a request arrives the file is a decision rather than a
            // mistake — the CLI already warned about it at startup.
            .ok_or_else(|| ServerError::NoWorker {
                backend: name.to_string(),
            })?;
        backend.request(request)
    }

    /// Send a request to the default backend, ignoring the route table.
    ///
    /// An alias used by the listener, where "default" is the meaningful word.
    /// Returns `NoWorker` for a static-only configuration, which is the one case
    /// where a request can arrive with nowhere to go.
    pub fn dispatch_to_default(&self, request: &Request) -> Result<Response> {
        self.dispatch_to(&self.default, request)
    }

    /// Slot contention in every worker pool, for the caller to report.
    ///
    /// `&self`, so it is readable at any time. The CLI reads it once, after the
    /// listeners have stopped, so a load test's numbers are in the log next to the
    /// arm they belong to.
    pub fn contention(&self) -> Vec<crate::backend::Contention> {
        self.backends
            .values()
            .filter_map(Backend::worker_contention)
            .collect()
    }

    /// Whether every worker pool can serve a request now.
    ///
    /// **Only worker pools count, and the asymmetry is the point.** A pool that volucer started
    /// itself is the one that spends a measurable time booting — four PHP processes, each loading
    /// an application — so it is the one a readiness probe should wait for. An `external` php-fpm is
    /// already running and supervised by systemd, and an `upstream` origin is somebody else's
    /// process; waiting for either would be this server reporting on a runtime it does not own.
    ///
    /// A dispatcher with no worker pools at all is ready, which is the honest answer for a
    /// static-only deployment or one behind an already-running FPM.
    pub fn is_ready(&self) -> bool {
        self.backends.values().all(|backend| backend.is_ready())
    }

    /// One line per worker pool that cannot serve yet, for the body of a 503.
    pub fn readiness_reasons(&self) -> Vec<String> {
        self.backends
            .values()
            .filter_map(Backend::readiness_reason)
            .collect()
    }

    /// Publish every worker pool's gauges into a metrics registry.
    ///
    /// `&self` and one call at one moment, so the numbers a scrape reads are one picture of the
    /// pool rather than several: `workers_ready + workers_working + workers_invalid` equals
    /// `total_workers` by construction, and a graph where the parts do not sum to the whole is a
    /// graph nobody trusts when it matters.
    ///
    /// Called on the scrape rather than on a timer. A background timer would be a thread whose only
    /// job is to be read by a metric nobody may ever fetch, and the cost here is a handful of atomic
    /// stores per pool — measured against a scrape that already renders every family in the process.
    pub fn publish_metrics(&self, registry: &volucer_metrics::Registry) {
        for backend in self.backends.values() {
            backend.publish_metrics(registry);
        }
    }

    /// Stop every backend.
    ///
    /// `&mut self`, and only reachable by the owner: `Drop` is what reaps the
    /// processes, so this is not on the request path.
    pub fn shutdown(&mut self) {
        for backend in self.backends.values_mut() {
            backend.shutdown();
        }
    }
}

/// Look up a CGI parameter as a lossy UTF-8 string.
fn param(request: &Request, name: &str) -> Option<String> {
    request
        .params
        .iter()
        .find(|(key, _)| key.as_slice() == name.as_bytes())
        .map(|(_, value)| String::from_utf8_lossy(value).into_owned())
}

/// A path with its query and fragment removed.
///
/// One function for both front-ends, because two copies of this rule is how `/api?x=1` comes to
/// route differently depending on which protocol carried it. A `REQUEST_URI` from a CGI client
/// carries its query the same way a request line does, so there is genuinely one rule here and not
/// two that happen to agree.
fn strip_query(path: &str) -> &str {
    path.split(['?', '#']).next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use volucer_core::config::{
        BackendConfig, Config, ExternalConfig, MemoryKvConfig, ServerConfig,
    };

    /// A call with parameters, for the registry tests.
    fn call_with(name: &str, params: serde_json::Value) -> volucer_worker::Call {
        volucer_worker::Call {
            name: name.to_string(),
            params,
            traceparent: None,
        }
    }

    /// A backend that can be *built* without starting anything, but cannot serve:
    /// an `external` backend pointing at a loopback port nothing listens on.
    ///
    /// Deliberately not a `worker` backend: the worker pool starts real processes
    /// in `from_config`, and these tests are about routing, not supervision.
    fn unreachable_backend() -> BackendConfig {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a probe port");
        let socket = listener.local_addr().expect("address").to_string();
        drop(listener);
        BackendConfig::External(ExternalConfig { socket })
    }

    fn config_with(backends: &[(&str, BackendConfig)], default: &str) -> Config {
        Config {
            server: ServerConfig {
                http: Some("127.0.0.1:8080".to_string()),
                ..Default::default()
            },
            backends: backends
                .iter()
                .map(|(name, config)| (name.to_string(), config.clone()))
                .collect(),
            default_backend: Some(default.to_string()),
            routes: Vec::new(),
            sites: BTreeMap::new(),
            // Named rather than `..Default::default()` would be shorter, but listing every field
            // is what made adding one a compile error in four test files. A test that asserts on
            // routing has nothing to say about metrics, so it takes the rest as it comes.
            ..Config::default()
        }
    }

    /// A request as a front-end would build it: the CGI parameters the route
    /// table reads, and nothing else.
    fn request_for(host: &str, uri: &str) -> Request {
        let pair = |name: &str, value: &str| (name.as_bytes().to_vec(), value.as_bytes().to_vec());
        Request {
            params: vec![pair("HTTP_HOST", host), pair("REQUEST_URI", uri)],
            ..Request::default()
        }
    }

    #[test]
    fn builds_every_backend_and_dispatch_reaches_the_default() {
        let config = config_with(
            &[
                ("app", unreachable_backend()),
                ("other", unreachable_backend()),
            ],
            "app",
        );
        let dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();

        assert_eq!(dispatcher.default_backend(), "app");
        let mut names = dispatcher.backend_names();
        names.sort();
        assert_eq!(names, vec!["app", "other"]);

        // The backend cannot connect, but the routing is what is under test: the
        // failure names the backend we reached, which is the default one.
        let err = dispatcher.dispatch(&Request::default()).unwrap_err();
        assert!(err.to_string().contains("app"), "{err}");
    }

    #[test]
    fn dispatch_to_an_unknown_backend_reports_it_by_name() {
        let config = config_with(&[("app", unreachable_backend())], "app");
        let dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();

        let err = dispatcher
            .dispatch_to("nope", &Request::default())
            .unwrap_err();
        assert!(matches!(err, ServerError::NoWorker { .. }), "{err}");
        assert!(err.to_string().contains("nope"), "{err}");
    }

    #[test]
    fn rejects_a_default_backend_that_does_not_exist() {
        let config = config_with(&[("app", unreachable_backend())], "missing");
        let err = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap_err();
        assert!(err.to_string().contains("default_backend"), "{err}");
    }

    /// A misspelled namespace in `services` is refused at validation, and a *valid* one builds a
    /// registry — the two halves of the same rule.
    ///
    /// The second half is what this asserts, because the first half lives in `volucer-core`'s own
    /// tests. What matters here is that the allow-list and the implementations agree: a name that
    /// validates but that [`worker_registry`] does not know would be a worker granted a capability it
    /// silently does not have, which is exactly the failure the allow-list exists to prevent.
    #[test]
    fn a_granted_namespace_reaches_the_registry_and_nothing_else_does() {
        let config = config_with(&[("app", unreachable_backend())], "app");
        let directory = Arc::new(PoolDirectory::default());
        let stores = build_stores(&config).expect("no storage needs a server");
        assert!(
            worker_registry(&[], &config, &directory, &stores, &None, &None).is_empty(),
            "no services must mean no namespaces"
        );

        let granted = worker_registry(
            &["config".to_string()],
            &config,
            &directory,
            &stores,
            &None,
            &None,
        );
        assert_eq!(granted.names(), vec!["config"]);
        assert!(
            !granted.names().contains(&"workers"),
            "granting config must not grant workers"
        );

        // Every name the validator allows has an implementation here. Asserted as a loop rather than
        // one case so that adding a name to `CALL_NAMESPACES` without an arm fails *this* test rather
        // than a deployment.
        for name in volucer_core::config::CALL_NAMESPACES {
            let registry = worker_registry(
                &[name.to_string()],
                &config,
                &directory,
                &stores,
                &None,
                &None,
            );
            assert_eq!(
                registry.names(),
                vec![*name],
                "{name} is allowed by the configuration and must have an implementation"
            );
        }
    }

    /// A granted `lock` service reaches the registry, and it is reachable as a *call*.
    ///
    /// This is the `build_locks` seam — the third time in this document's history that a wiring function
    /// between two tested things was the only untested part, after `set_call_handler` and `build_stores`.
    /// So it is asserted by making a real call through the registry rather than by inspecting the
    /// namespace list: a registry that named `lock` while `build_locks` returned `None` would pass the
    /// weaker check, and that is precisely the bug the KV case had.
    #[test]
    fn a_granted_lock_service_is_reachable_through_the_registry() {
        let mut config = config_with(&[("app", unreachable_backend())], "app");
        config.lock = Some(volucer_core::config::LockConfig::Memory(
            volucer_core::config::MemoryLockConfig {},
        ));

        let directory = Arc::new(PoolDirectory::default());
        let stores = build_stores(&config).expect("no storage needs a server");
        let locks = build_locks(&config)
            .expect("the memory driver needs no server")
            .expect("a [lock] table builds a backend");

        let registry = worker_registry(
            &["lock".to_string()],
            &config,
            &directory,
            &stores,
            &Some(locks),
            &None,
        );
        assert_eq!(registry.names(), vec!["lock"]);

        // Through the registry, not around it: the call has to route on the first dot, find the
        // namespace, reach the backend and come back.
        let acquired = registry.serve(&call_with(
            "lock.acquire",
            serde_json::json!({ "resource": "pdf:create", "id": "worker-1" }),
        ));
        assert!(acquired.ok, "{acquired:?}");
        assert_eq!(
            acquired.result.expect("a result")["acquired"],
            serde_json::json!(true)
        );

        let twice = registry.serve(&call_with(
            "lock.acquire",
            serde_json::json!({ "resource": "pdf:create", "id": "worker-2" }),
        ));
        assert_eq!(
            twice.result.expect("a result")["acquired"],
            serde_json::json!(false),
            "a second worker must contend with the first, through the real backend"
        );
    }

    /// With no `[lock]` table there is no backend, so a granted `lock` is skipped rather than faked.
    ///
    /// `Config::validate` refuses that grant before a server serves it, so this is only reachable from a
    /// configuration built in a test. The behaviour that matters is which way it fails: an empty `lock`
    /// namespace installed anyway would answer `lock.acquire` with "no lock backend configured", which
    /// reads like a configuration mistake rather than the internal one it is.
    #[test]
    fn a_lock_service_with_no_backend_installs_nothing() {
        let config = config_with(&[("app", unreachable_backend())], "app");
        assert!(config.lock.is_none());

        let directory = Arc::new(PoolDirectory::default());
        let stores = build_stores(&config).expect("no storage needs a server");
        assert!(
            build_locks(&config).expect("nothing to build").is_none(),
            "an absent [lock] table must not produce a backend"
        );

        let registry = worker_registry(
            &["lock".to_string()],
            &config,
            &directory,
            &stores,
            &None,
            &None,
        );
        assert!(
            registry.is_empty(),
            "a granted service with nothing behind it must be refused by name rather than answered \
             with an empty namespace: {:?}",
            registry.names()
        );
    }

    /// The default read timeout is the one the documentation promises, and it is reachable by omitting it.
    ///
    /// Pinned because the value is written in three places — the configuration default, the lock crate's
    /// documentation, and RoadRunner's own default — and a default that drifted would be a deadline
    /// nobody could predict from the file.
    #[cfg(feature = "redis")]
    #[test]
    fn an_omitted_lock_read_timeout_is_five_seconds() {
        let text = r#"
[server]
http = "127.0.0.1:8080"
[lock]
driver = "redis"
url = "redis://127.0.0.1:6379"
"#;
        let config = Config::from_toml(text).expect("the file parses");
        let lock = config.lock.as_ref().expect("the table is there");
        let LockConfig::Redis(redis) = lock else {
            panic!("the driver is redis");
        };
        assert_eq!(redis.read_timeout_ms, 5000);
        assert_eq!(
            redis.pool_size, 16,
            "and the pool default is the KV one, written out here because this crate cannot name a \
             driver's constant — the two are one value in configuration"
        );
    }

    /// Two storages granted to one backend arrive in **one** `kv` namespace.
    ///
    /// The shape that makes this easy to get wrong: the registry routes on the text before the first dot,
    /// so both `kv.sessions` and `kv.cache` route to the namespace named `kv`. Installing one `with` call
    /// per granted storage would therefore have the second replace the first, and the symptom would be
    /// that only one of a backend's two storages worked — which a test with one storage cannot see.
    #[test]
    fn two_granted_storages_share_one_kv_namespace() {
        let mut config = config_with(&[("app", unreachable_backend())], "app");
        config.kv.insert(
            "sessions".to_string(),
            KvStorageConfig::Memory(MemoryKvConfig { max_bytes: 1024 }),
        );
        config.kv.insert(
            "cache".to_string(),
            KvStorageConfig::Memory(MemoryKvConfig { max_bytes: 1024 }),
        );

        let directory = Arc::new(PoolDirectory::default());
        let stores = build_stores(&config).expect("no storage needs a server");
        let registry = worker_registry(
            &["kv.sessions".to_string(), "kv.cache".to_string()],
            &config,
            &directory,
            &stores,
            &None,
            &None,
        );

        assert_eq!(
            registry.names(),
            vec!["kv"],
            "two storages are one namespace, because that is what the router keys on"
        );

        // And both are reachable through it. A call that reaches the namespace is answered for each,
        // which is the property the merge exists for; `store.set` then `store.get` is the one round trip
        // that proves the namespace found a real store rather than an empty map.
        for storage in ["sessions", "cache"] {
            let answer = registry.serve(&call_with(
                &format!("kv.{storage}.set"),
                serde_json::json!({
                    "key": crate::params::encode_base64(b"k"),
                    "value": crate::params::encode_base64(b"v"),
                }),
            ));
            assert!(answer.ok, "{storage} should accept a value: {answer:?}");

            let answer = registry.serve(&call_with(
                &format!("kv.{storage}.get"),
                serde_json::json!({ "key": crate::params::encode_base64(b"k") }),
            ));
            assert_eq!(
                answer.result.as_ref().and_then(|r| r["value"].as_str()),
                Some("dg=="),
                "{storage} should return what it stored: {answer:?}"
            );
        }
    }

    /// The two storages are separate maps, so writing one does not write the other.
    ///
    /// The other half of sharing a namespace: they share the *namespace*, not the store. A merge that got
    /// this wrong would make two named caches one, and the difference would only show as a key appearing
    /// in a place nobody put it.
    #[test]
    fn two_storages_in_one_namespace_do_not_share_their_map() {
        let mut config = config_with(&[("app", unreachable_backend())], "app");
        for name in ["sessions", "cache"] {
            config.kv.insert(
                name.to_string(),
                KvStorageConfig::Memory(MemoryKvConfig { max_bytes: 1024 }),
            );
        }

        let directory = Arc::new(PoolDirectory::default());
        let stores = build_stores(&config).expect("no storage needs a server");
        let registry = worker_registry(
            &["kv.sessions".to_string(), "kv.cache".to_string()],
            &config,
            &directory,
            &stores,
            &None,
            &None,
        );

        let key = crate::params::encode_base64(b"shared-key");
        registry.serve(&call_with(
            "kv.sessions.set",
            serde_json::json!({ "key": key, "value": crate::params::encode_base64(b"from-sessions") }),
        ));
        let answer = registry.serve(&call_with(
            "kv.cache.get",
            serde_json::json!({ "key": key }),
        ));

        assert_eq!(
            answer.result.as_ref().and_then(|r| r["value"].as_str()),
            None,
            "a key written to one storage must not appear in another: {answer:?}"
        );
    }

    /// A storage a backend was not granted is refused by name, and the refusal says what it *may* reach.
    ///
    /// This is §4.5 as a test. The distinction worth checking is between a storage the deployment has and
    /// this backend lacks — a `services` list to edit — and a storage that does not exist at all, because
    /// an operator reading one message and acting on it as though it were the other wastes their time.
    #[test]
    fn a_storage_outside_a_backends_grant_is_refused_by_name() {
        let mut config = config_with(&[("app", unreachable_backend())], "app");
        for name in ["sessions", "secrets"] {
            config.kv.insert(
                name.to_string(),
                KvStorageConfig::Memory(MemoryKvConfig { max_bytes: 1024 }),
            );
        }

        let directory = Arc::new(PoolDirectory::default());
        let stores = build_stores(&config).expect("no storage needs a server");
        let registry = worker_registry(
            &["kv.sessions".to_string()],
            &config,
            &directory,
            &stores,
            &None,
            &None,
        );

        let refused = registry.serve(&call_with(
            "kv.secrets.get",
            serde_json::json!({ "key": crate::params::encode_base64(b"k") }),
        ));
        assert!(!refused.ok);
        let message = refused.error.clone().unwrap_or_default();
        assert!(
            message.contains("secrets"),
            "the refusal must name what was asked for: {message}"
        );
        assert!(
            message.contains("sessions"),
            "and what may be reached, or the operator has to read the config file to find out: {message}"
        );

        // A storage that does not exist anywhere reads differently — it is refused as ungranted too, since
        // from this backend's side the two are the same question, but the message names the one it may
        // reach so the difference is visible to whoever reads it.
        let unknown = registry.serve(&call_with(
            "kv.typo.get",
            serde_json::json!({ "key": crate::params::encode_base64(b"k") }),
        ));
        assert!(!unknown.ok);
        assert!(
            unknown
                .error
                .as_deref()
                .is_some_and(|m| m.contains("typo") && m.contains("sessions")),
            "{unknown:?}"
        );
    }

    #[test]
    fn startup_fails_on_a_misconfigured_backend_rather_than_on_first_request() {
        #[cfg(not(unix))]
        let socket = "unix:/run/php.sock";
        #[cfg(unix)]
        let socket = "unix:";

        let config = config_with(
            &[(
                "fpm",
                BackendConfig::External(volucer_core::config::ExternalConfig {
                    socket: socket.to_string(),
                }),
            )],
            "fpm",
        );
        assert!(Dispatcher::from_config(&config, cgi::Routing::default()).is_err());
    }

    #[test]
    fn a_route_selects_the_backend_and_the_default_applies_otherwise() {
        let mut config = config_with(
            &[
                ("app", unreachable_backend()),
                ("legacy", unreachable_backend()),
            ],
            "app",
        );
        config.routes = vec![RouteConfig {
            path_prefix: Some("/api".to_string()),
            path_suffix: None,
            host: None,
            backend: "legacy".to_string(),
        }];
        let dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();

        assert_eq!(
            dispatcher.backend_for(&request_for("example.test", "/api/users")),
            "legacy"
        );
        assert_eq!(
            dispatcher.backend_for(&request_for("example.test", "/other")),
            "app"
        );
        // A query string is not part of the path, so it cannot change the route.
        assert_eq!(
            dispatcher.backend_for(&request_for("example.test", "/api?x=1")),
            "legacy"
        );
    }

    #[test]
    fn a_route_narrows_an_existing_one_rather_than_widening_it() {
        // Two routes match `/api/x`; the more specific one decides. This is the
        // property that makes the table predictable rather than order-dependent.
        let mut config = config_with(
            &[
                ("app", unreachable_backend()),
                ("api", unreachable_backend()),
                ("admin_api", unreachable_backend()),
            ],
            "app",
        );
        config.routes = vec![
            RouteConfig {
                path_prefix: Some("/api".to_string()),
                path_suffix: None,
                host: None,
                backend: "api".to_string(),
            },
            RouteConfig {
                path_prefix: Some("/".to_string()),
                path_suffix: None,
                host: Some("admin.test".to_string()),
                backend: "admin_api".to_string(),
            },
        ];
        let dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();

        assert_eq!(
            dispatcher.backend_for(&request_for("admin.test", "/api/x")),
            "admin_api"
        );
        assert_eq!(
            dispatcher.backend_for(&request_for("public.test", "/api/x")),
            "api"
        );
    }

    // -----------------------------------------------------------------------
    // The upstream arm: reachable from Rust, and the two routing sources must agree.
    // -----------------------------------------------------------------------

    /// An upstream config, with the origin pointed at a port nothing listens on.
    ///
    /// Deliberately unreachable: every test here is about *which* backend is chosen and whether the
    /// client exists, not about what an origin answers — that needs the listener, and lives with
    /// the front-end tests.
    fn unreachable_upstream() -> BackendConfig {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a probe port");
        let port = listener.local_addr().expect("address").port();
        drop(listener);
        BackendConfig::Upstream(volucer_core::config::UpstreamConfig {
            origin: format!("http://127.0.0.1:{port}"),
            tls: None,
        })
    }

    #[test]
    fn an_upstream_backend_builds_a_client_and_is_not_reached_over_cgi() {
        let config = config_with(
            &[
                ("node", unreachable_upstream()),
                ("cgi", unreachable_backend()),
            ],
            "node",
        );
        let dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();

        assert!(dispatcher.is_upstream("node"));
        assert!(!dispatcher.is_upstream("cgi"));
        assert!(dispatcher.upstream("node").is_some());
        assert!(dispatcher.upstream("cgi").is_none());
        // And the FastCGI path refuses it by *name* rather than trying a connection, so the error
        // says "wrong protocol" instead of "unreachable origin" — two different operator actions.
        let err = dispatcher
            .dispatch_to("node", &Request::default())
            .unwrap_err();
        assert!(matches!(err, ServerError::WrongProtocol { .. }), "{err}");
        assert!(err.to_string().contains("FastCGI"), "{err}");
    }

    #[test]
    fn an_upstream_whose_origin_cannot_be_built_is_a_misconfiguration() {
        // `Backend::from_config` is reachable without `Config::validate`, so the origin is parsed
        // here too: a backend built from an unvalidated config must fail at startup with the key
        // named, not 502 on every request.
        let config = config_with(
            &[(
                "node",
                BackendConfig::Upstream(volucer_core::config::UpstreamConfig {
                    origin: "not-an-origin".to_string(),
                    tls: None,
                }),
            )],
            "node",
        );
        let err = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap_err();
        assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
        assert!(err.to_string().contains("node"), "{err}");
        assert!(err.to_string().contains("no scheme"), "{err}");
    }

    #[test]
    fn a_ca_that_cannot_be_read_fails_at_startup_not_on_the_first_request() {
        // The rule the other backends already follow: a mistyped path is a startup failure. Without
        // it, a deployment serves happily and 502s every request for one backend.
        let config = config_with(
            &[(
                "node",
                BackendConfig::Upstream(volucer_core::config::UpstreamConfig {
                    origin: "https://node.internal:8443".to_string(),
                    tls: Some(volucer_core::config::UpstreamTlsConfig {
                        ca: Some(std::path::PathBuf::from(
                            "/definitely/not/a/ca-certificate.pem",
                        )),
                        sni: None,
                    }),
                }),
            )],
            "node",
        );
        let err = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap_err();
        assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
        assert!(err.to_string().contains("ca-certificate"), "{err}");
    }

    /// The two routing sources must give the same answer, including about a query string.
    ///
    /// This is the assertion that makes `backend_for_target` a *reuse* of the route rule rather
    /// than a second copy of it. `/api?x=1` is the case that matters: a path with its query left on
    /// matches a `path_prefix` differently from one without, so a front-end that forgot to strip it
    /// would route the same request differently depending on the protocol.
    #[test]
    fn the_cgi_and_parts_routing_agree_including_about_a_query() {
        let config = config_with(
            &[
                ("api", unreachable_backend()),
                ("app", unreachable_backend()),
                ("fallback", unreachable_backend()),
            ],
            "app",
        );
        let mut config = config;
        // Two routes, and the *default* is a third. That matters: several of the assertions below
        // compare against the default, so a test whose default was one of the routes would pass
        // even if a query-bearing path failed to match a route at all.
        config.default_backend = Some("fallback".to_string());
        config.routes = vec![
            RouteConfig {
                host: None,
                path_prefix: Some("/api".to_string()),
                path_suffix: None,
                backend: "api".to_string(),
            },
            // A *suffix* route, deliberately, because it is the one that makes the stripping
            // load-bearing: `/api` as a prefix still matches `/api?x=1` with the query left on,
            // whereas `/index.php?x=1` does not end with `.php`.
            RouteConfig {
                host: None,
                path_prefix: None,
                path_suffix: Some(".php".to_string()),
                backend: "app".to_string(),
            },
        ];
        let dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();

        let fallback = config
            .default_backend
            .clone()
            .expect("the fixture states one");
        for (host, uri, expected) in [
            ("public.test", "/api", Some("api")),
            ("public.test", "/api/x", Some("api")),
            ("public.test", "/api?x=1&y=2", Some("api")),
            ("public.test", "/api/x?trace=1", Some("api")),
            ("public.test", "/api#frag", Some("api")),
            ("public.test", "/index.php", Some("app")),
            // The case a front-end that forwarded the query would get wrong: without stripping,
            // `.php` does not match and this falls through to the default.
            ("public.test", "/index.php?x=1", Some("app")),
            ("public.test", "/index.php?x=1#top", Some("app")),
            ("public.test", "/index.php#top", Some("app")),
            ("public.test", "/other", None),
            ("public.test", "/nothing/here", None),
        ] {
            let from_cgi = dispatcher.backend_for(&request_for(host, uri));
            // `parts.uri.path()` never carries a query, and a caller that passes a whole URI needs
            // the rule — so the front-end hands over a path and the helper strips one if a caller
            // forgot. Both sources therefore agree without the front-end knowing the rule.
            let path = uri.split(['?', '#']).next().unwrap_or(uri);
            let from_parts = dispatcher.backend_for_target(Some(host), path);

            let expected = expected.unwrap_or(fallback.as_str());
            assert_eq!(from_cgi, expected, "{uri} from a CGI client");
            assert_eq!(from_parts, expected, "{uri} from an HTTP client");
        }
    }

    #[test]
    fn an_unknown_backend_is_not_an_upstream_and_keeps_the_cgi_behaviour() {
        // A name that selects nothing must not be treated as an upstream: it cannot vouch for
        // anything, so it keeps the CGI path's answer rather than sending a request to an origin
        // that was never configured.
        let config = config_with(&[("app", unreachable_backend())], "app");
        let dispatcher = Dispatcher::from_config(&config, cgi::Routing::default()).unwrap();
        assert!(!dispatcher.is_upstream("nope"));
        assert!(dispatcher.upstream("nope").is_none());
        let err = dispatcher
            .dispatch_to("nope", &Request::default())
            .unwrap_err();
        assert!(matches!(err, ServerError::NoWorker { .. }), "{err}");
    }

    #[test]
    fn has_upstream_answers_for_the_deployment_rather_than_a_route() {
        // The capability check `serve` makes. A deployment with no upstream must not be refused,
        // which is why this is not "does any route name one".
        let plain = config_with(&[("app", unreachable_backend())], "app");
        let dispatcher = Dispatcher::from_config(&plain, cgi::Routing::default()).unwrap();
        assert!(!dispatcher.has_upstream());

        let with_upstream = config_with(
            &[
                ("app", unreachable_backend()),
                ("node", unreachable_upstream()),
            ],
            "app",
        );
        let dispatcher = Dispatcher::from_config(&with_upstream, cgi::Routing::default()).unwrap();
        assert!(dispatcher.has_upstream());
    }
}
