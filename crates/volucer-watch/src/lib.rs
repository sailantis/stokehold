//! The state a running volucer serves from, and replacing it without a restart.
//!
//! This crate exists to answer one question in one place: **what does a reload replace, and what
//! can never be replaced?** It is separate from the front-ends because volucer has two of them —
//! an HTTP listener and a FastCGI listener for a deployment behind nginx — and a reload that
//! reached only one of them would be worse than no reload at all. That is not hypothetical: the
//! site table used to be held by each listener separately, so a reloaded site worked over `:80`
//! and silently kept reaching the old backend behind nginx, which reads as *the other front-end
//! being right* rather than as a bug.
//!
//! # Why a swap and not a lock
//!
//! The read is the request path and the write happens once per reload, which is the shape a
//! read-write lock handles badly: `RwLock` still has every reader touch one atomic counter, and a
//! writer cannot make progress until they all leave. That is fine at startup and wasteful per
//! request — and this project's whole design note is that a request's cost is not allowed to
//! depend on a feature the deployment is not using.
//!
//! [`arc_swap::ArcSwap`] is the reverse: readers load a pointer that no writer ever blocks. The
//! cost is measured rather than asserted —
//! `volucer-competition/scripts/reload-cost-ab.js` is the harness, and the result is −0.09%, 95%
//! CI [−1.43%, +1.27%] at n = 20, against an A-vs-A control at +0.34%.
//!
//! # The one pointer
//!
//! One value, not one per field, and that is the correctness rather than the tidiness: three
//! separate swaps are not one snapshot, so a request could take the new site table and the old
//! routing map and resolve a site the map has no entry for. The failure would not look like a
//! race — it would look like a site serving the wrong document root, once, under load.
//!
//! # What is *not* reloadable, and why that is not a limitation to work around
//!
//! Some settings describe a socket that is already bound or a process that is already running:
//! listener addresses, backend commands and worker counts, and the TLS settings that live inside a
//! `rustls::ServerConfig` (`min_version`, `alpn`). Those cannot be replaced in place, and pretending
//! otherwise is how a reload comes to change half of what it was asked to change. The rule this
//! crate follows is that **everything behind a [`Swappable`] is replaceable *whole*, and everything
//! outside one is restart-only** — so the line is drawn by the type rather than by a comment a later
//! change can contradict. The restart-only list is *reported* by every reload and named in
//! `volucer serve`'s own output, rather than left to be discovered from behaviour.
//!
//! Note what is **not** in that list, because an earlier draft of it said otherwise: a site's route
//! table and its static layer are both read per request out of the swapped table
//! (`Reloaded::from_sites` builds them from the site), so both are reloadable. The published list is
//! drawn from the code.
//!
//! # One apply path per process
//!
//! [`Registry`] owns the shared table *and* every front-end's snapshot-holder, so a caller applies a
//! reload through one call and no front-end can be missed. That is what makes a *trigger* writable:
//! `volucer serve`'s `SIGHUP` handler, a test, and a listener's own [`crate::SharedState`] all reach
//! the same `apply`. Before this, only a listener could apply a reload — so the process's own signal
//! handler would have had to either duplicate the rebuild or reach the table through one listener.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use volucer_core::config::StaticConfig;
use volucer_core::sites::{ResolvedSite, Sites};
use volucer_fcgi::cgi::Routing;

/// A value that can be replaced while readers are using it.
///
/// Always used behind an `Arc`, because a snapshot has to be cloneable into the blocking task
/// that serves the request: [`Swappable::load`] returns an owned `Arc`, so a handler that takes
/// one snapshot keeps the value it started with even if a reload lands mid-request. That is
/// deliberate. A request that resolved its site against one table and then routed against another
/// would be two answers to one question, and the failure would look like a site serving the wrong
/// document root rather than like a race.
///
/// `T: Send + Sync` is what the `Arc` requires for the readers to be on other threads, and it is
/// the whole of the type's constraint: a value that can be shared concurrently can be swapped
/// concurrently.
#[derive(Debug)]
pub struct Swappable<T> {
    value: ArcSwap<T>,
}

impl<T> Swappable<T> {
    /// Wrap a value: the state a listener starts with.
    pub fn new(value: T) -> Self {
        Self {
            value: ArcSwap::from_pointee(value),
        }
    }

    /// A snapshot, for the length of one request or one connection.
    ///
    /// This is the request-path read, so it is deliberately the only way in: there is no `Deref`,
    /// because a `Deref` would let a caller hold a borrow across an `.await` and thereby hold a
    /// snapshot it never chose to hold. Taking an `Arc` is the explicit version of the same thing,
    /// and it cannot outlive the guard by accident.
    pub fn load(&self) -> Arc<T> {
        self.value.load_full()
    }

    /// Replace the value, returning the previous one.
    ///
    /// Returns the old value so a caller can say *what* changed rather than only that a reload
    /// happened — which is the difference between an operator reading "site `api` document_root
    /// changed" and reading "configuration reloaded" and having to go and diff the file themselves.
    ///
    /// A reader that already loaded is unaffected: it keeps the value it took, which is the
    /// property that makes this safe to call while requests are in flight.
    #[must_use]
    pub fn replace(&self, value: T) -> Arc<T> {
        self.value.swap(Arc::new(value))
    }
}

impl<T: Default> Default for Swappable<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// The site table both front-ends serve from, and what a reload replaces.
///
/// Wraps [`Sites`] rather than being an alias for it so that the *shared* thing is named: both
/// listeners are handed a clone of this handle, so replacing the table moves both of them or
/// neither, and there is no way to write the code that moves one.
///
/// The per-front-end state that is *derived* from the table — the HTTP listener's prebuilt
/// routings and static layers — is not here, because it is not shared and rebuilding it for a
/// front-end that has no use for it would be work for nothing. It is rebuilt from the table this
/// handle returns, so it cannot outlive the table it was built from.
#[derive(Clone)]
pub struct SharedState {
    sites: Arc<Swappable<Sites>>,
}

impl SharedState {
    /// A shared site table.
    pub fn new(sites: Sites) -> Self {
        SharedState {
            sites: Arc::new(Swappable::new(sites)),
        }
    }

    /// A snapshot of the site table, taken once per request.
    ///
    /// The single load is the point: everything downstream — site resolution, the routing map,
    /// the static layer — reads from the value this returns, so a request is served under exactly
    /// one configuration even if a reload lands while it is in flight.
    pub fn sites(&self) -> Arc<Sites> {
        self.sites.load()
    }

    /// Replace the site table, returning the one that was serving.
    ///
    /// Every front-end holding a clone of this handle is affected at once, which is the property
    /// the two-listener case needs and the reason this is one handle rather than a table per
    /// listener.
    #[must_use]
    pub fn replace(&self, sites: Sites) -> Arc<Sites> {
        self.sites.replace(sites)
    }
}

impl std::fmt::Debug for SharedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedState")
            .field("sites", &self.sites.load().named().len())
            .finish_non_exhaustive()
    }
}

/// The key the default site's routing is stored under.
///
/// A name that cannot collide with a site's own, because a site name comes from a TOML
/// table key and `<>` is not valid in a bare TOML key.
pub const DEFAULT_SITE_KEY: &str = "<default>";

/// The static layer's configuration and document root.
///
/// A plain data struct rather than the HTTP listener's own type, and that is deliberate: the
/// layer is configured *from* a site, so it has to be built by whatever builds the site table —
/// which must not be the HTTP front-end, because a reload applies while no HTTP listener may
/// exist at all. The HTTP listener adds the behaviour and the response types; this is only what
/// it decides from. See `volucer_http::static_files::StaticFiles`, which is a type alias for it.
#[derive(Debug, Clone)]
pub struct StaticFiles {
    /// The layer's settings, with the site's inheritance already applied.
    pub config: StaticConfig,
    /// The root a request path is resolved against.
    pub document_root: PathBuf,
    /// The listener's own port, for a redirect when no `Host` header arrived.
    ///
    /// Fixed for the life of a listener — the socket is already bound — so a reload cannot
    /// change it, and it is passed in rather than stored with the table.
    pub server_port: u16,
}

/// Everything a front-end derives from the shared site table, as one replaceable value.
///
/// The three travel together because they are derived from one another: the routing and
/// static maps are built *from* the site table, so a request that took them from different
/// generations would resolve a site the map has no entry for. [`Reloaded::from_sites`] is
/// the only constructor for that reason — the maps cannot be set independently of the table
/// they describe.
///
/// The `Arc`s are for the request path: a handler takes one snapshot and moves it into the
/// blocking dispatch task, so the request keeps the configuration it began under.
///
/// It lives in this crate rather than in a front-end because **both** front-ends need it *and* a
/// reload has to build it. Building it inside the HTTP listener meant a reload could not be
/// applied by anything that did not have that listener in hand, which is what made a single
/// shared site table impossible to reach from the process's own reload path.
pub struct Reloaded {
    /// The site table this was built from, as a snapshot of [`SharedState`].
    ///
    /// Held rather than borrowed so a request is served under one generation even if a reload
    /// lands while it is in flight — the property the in-flight test pins.
    sites: Arc<Sites>,
    /// One prebuilt routing per site, by site name, plus the default under
    /// [`DEFAULT_SITE_KEY`].
    ///
    /// Prebuilt, not derived per request: a `Routing` is a document root and a front
    /// controller, and rebuilding one for every request would put a `PathBuf` clone on
    /// the hot path to answer a question the configuration already answered at startup.
    routings: BTreeMap<String, Routing>,
    /// The static layer per site, for the same reason.
    statics: BTreeMap<String, StaticFiles>,
}

impl Reloaded {
    /// Build a front-end's maps from a snapshot of the shared site table.
    ///
    /// Takes the `Arc` the shared handle handed out rather than a table, so the value a request
    /// reads and the per-site maps it routes against are guaranteed to be the *same* generation:
    /// there is no constructor that could pair them from two.
    ///
    /// `server_port` is the bound port, which the static layer stamps into generated
    /// redirects — a reload cannot change it, so it is passed in rather than stored.
    pub fn from_sites(sites: Arc<Sites>, server_port: u16) -> Self {
        let mut routings = BTreeMap::new();
        let mut statics = BTreeMap::new();
        for site in sites
            .named()
            .values()
            .chain(std::iter::once(sites.default_site()))
        {
            routings.insert(
                site.name.clone(),
                Routing {
                    document_root: site.document_root.clone(),
                    front_controller: site.front_controller.clone(),
                    ..Routing::default()
                },
            );
            statics.insert(
                site.name.clone(),
                StaticFiles {
                    config: site.static_files.clone(),
                    document_root: site.document_root.clone(),
                    server_port,
                },
            );
        }
        Reloaded {
            sites,
            routings,
            statics,
        }
    }

    /// The site table this snapshot was built from.
    pub fn sites(&self) -> &Sites {
        &self.sites
    }

    /// Resolve a host against *this* generation and return everything the request needs from it.
    ///
    /// One call rather than a resolve followed by a lookup, because the two must agree: see
    /// [`Resolved`].
    pub fn resolve(&self, host: Option<&str>) -> Resolved<'_> {
        let site = self.sites.resolve(host);
        let (routing, statics) = self.for_site(site);
        Resolved {
            site,
            routing,
            statics,
        }
    }

    /// The routing and static layer for one site.
    ///
    /// A site resolved by [`Sites`] is always in both maps — they are built from the
    /// same table in the same pass — so a missing entry would mean the two had
    /// diverged, and falling back to the default is the safe direction: it serves the
    /// deployment's own root rather than an empty path.
    fn for_site(&self, site: &ResolvedSite) -> (&Routing, &StaticFiles) {
        let routing = self
            .routings
            .get(&site.name)
            .or_else(|| self.routings.get(DEFAULT_SITE_KEY))
            .expect("the default site is always built");
        let statics = self
            .statics
            .get(&site.name)
            .or_else(|| self.statics.get(DEFAULT_SITE_KEY))
            .expect("the default site is always built");
        (routing, statics)
    }
}

impl std::fmt::Debug for Reloaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reloaded")
            .field("sites", &self.sites.named().len())
            .finish_non_exhaustive()
    }
}

/// One request's configuration: a resolved site and the maps that go with it, from one generation.
///
/// Returned as a single value rather than as a pair of calls because the pairing *is* the
/// correctness. The site has to come from the `Reloaded` a request took, not from the shared table
/// live at the moment of resolution: a reload landing between the two would otherwise resolve a
/// site against one generation and route it against another, which is the failure this whole
/// mechanism exists to prevent — and it would look like a site serving the wrong document root,
/// once, under load.
///
/// The two borrows are tied to the `Arc<Reloaded>` the caller holds, so a request cannot keep them
/// past its own snapshot.
pub struct Resolved<'a> {
    /// The site this request is served by.
    pub site: &'a ResolvedSite,
    /// Its routing.
    pub routing: &'a Routing,
    /// Its static layer.
    pub statics: &'a StaticFiles,
}

/// The process's one reloadable state, and the handle a trigger applies a new configuration
/// through.
///
/// One handle rather than one per front-end, because the failure this prevents is a reload that
/// reaches *some* of a running server: an HTTP listener and a FastCGI listener can both be
/// serving, and a reload that moved one and not the other takes effect on one protocol and reads
/// as the other front-end being right. Holding the shared site table *and* each listener's
/// derived maps here means "apply this configuration" is one call with no way to half-do it.
///
/// The two parts are not the same kind of thing, and that is why both are here:
///
/// - [`SharedState`] is the **live** table every front-end writes through, so replacing it moves
///   all of them.
/// - Each `Swappable<Reloaded>` is one front-end's **snapshot-holder**, rebuilt from the table
///   that is now live, so a request in flight keeps the generation it started under.
///
/// Folding them together would mean a reload had to reach the table *through* a listener, which is
/// precisely how the FastCGI front-end came to keep serving the old one.
pub struct Registry {
    shared: SharedState,
    /// One entry per registered front-end.
    ///
    /// Behind a mutex rather than an `ArcSwap` because registration is not the request path: it
    /// runs once per listener at startup, so a serialised write costs nothing that matters.
    front_ends: std::sync::Mutex<Vec<RegisteredFrontEnd>>,
    /// How many reloads have been applied. Reported, and what makes a test able to wait for the
    /// effect of a signal rather than sleep and hope.
    generations: Arc<AtomicU64>,
}

/// One front-end, as the registry needs to reach it.
pub struct RegisteredFrontEnd {
    /// The front-end's own snapshot-holder.
    state: Arc<Swappable<Reloaded>>,
    /// The port that front-end is bound to.
    ///
    /// Per front-end rather than one per registry, because two listeners on two ports share one
    /// site table and a generated redirect has to name the port the client actually reached — so
    /// one number for the process would be wrong for whichever listener it was not.
    port: u16,
}

impl RegisteredFrontEnd {
    /// A front-end, for the registry to rebuild on a reload.
    pub fn new(state: Arc<Swappable<Reloaded>>, port: u16) -> Self {
        RegisteredFrontEnd { state, port }
    }
}

impl Registry {
    /// An empty registry: no front-end has registered yet.
    pub fn new(shared: SharedState) -> Self {
        Registry {
            shared,
            front_ends: std::sync::Mutex::new(Vec::new()),
            generations: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Register a front-end's snapshot-holder, so a reload reaches it.
    ///
    /// Returns the handle the front-end stores in its own request path. The caller builds it with
    /// [`Reloaded::from_sites`] for its bound port; registering it is what makes a later
    /// [`Registry::apply`] touch it.
    ///
    /// Takes `&self` and synchronises internally, because registration happens while the registry
    /// is already shared: `serve` hands the same registry to a listener that binds before the next
    /// one does. Behind a mutex rather than an `RwLock` because this is not the request path — it
    /// runs once per listener at startup — so a cheaper write path is worth more here than a
    /// cheaper read, which does not exist.
    pub fn register(&self, front_end: RegisteredFrontEnd) {
        self.front_ends
            .lock()
            .expect("the registry mutex is not poisoned by anything but a panic in register")
            .push(front_end);
    }

    /// The site table every front-end serves from.
    pub fn shared(&self) -> &SharedState {
        &self.shared
    }

    /// How many reloads have been applied, for diagnostics and for a test that must not sleep.
    pub fn generation(&self) -> u64 {
        self.generations.load(Ordering::Acquire)
    }

    /// Apply a new configuration's site table, whole, to every registered front-end.
    ///
    /// Whole-or-nothing is the only safe direction and it is a property of this call rather than a
    /// convention: the table is built by the caller *before* this is reached, so a configuration
    /// that failed validation never gets here at all, and there is no partial path once it does.
    ///
    /// This is the *applying* half; the trigger is `volucer serve`'s `signals` module. One apply path
    /// for the process is the point — a test and the process's own `SIGHUP` handler come through the
    /// same call, so there is no way to reload one front-end and not another.
    ///
    /// Returns the state the **first** front-end replaced, or `None` when nothing is registered.
    /// Reported rather than returned as a difference, because "what changed" is a comparison
    /// between two configurations, which is a question for the caller that has both of them — this
    /// only guarantees both are installed.
    #[must_use]
    pub fn apply(&self, sites: Sites) -> Option<Arc<Reloaded>> {
        let _replaced = self.shared.replace(sites);
        // Read back through the handle rather than from the argument: `SharedState::replace`
        // returns the table it *replaced*, so rebuilding from its return value would build the
        // next generation's maps out of the previous generation's table. That bug was written and
        // caught once already; reading back is what makes it unwritable.
        let live = self.shared.sites();

        let front_ends = self
            .front_ends
            .lock()
            .expect("the registry mutex is not poisoned by anything but a panic in register");

        let mut previous = None;
        for front_end in front_ends.iter() {
            // The *front-end's own* port, not the one in the shared table: a redirect has to name
            // the port the client actually reached, and two listeners on two ports share one table.
            let next = Reloaded::from_sites(Arc::clone(&live), front_end.port);
            let replaced = front_end.state.replace(next);
            previous = previous.or(Some(replaced));
        }
        self.generations.fetch_add(1, Ordering::Release);
        previous
    }
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let front_ends = self.front_ends.lock().map(|list| list.len()).unwrap_or(0);
        f.debug_struct("Registry")
            .field("front_ends", &front_ends)
            .field("generation", &self.generation())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use volucer_core::config::Config;

    fn sites_for(root: &str) -> Sites {
        let text = format!(
            "[server]\nhttp = \"127.0.0.1:0\"\ndocument_root = {root:?}\n",
            root = root
        );
        let config = Config::from_toml(&text).expect("config parses");
        Sites::from_config(&config, Path::new(root))
    }

    #[test]
    fn a_snapshot_taken_before_a_replace_still_reads_the_old_value() {
        // The property the request path depends on: a request that has its site table keeps it,
        // even if a reload lands before the response is written. If this ever failed, a request
        // could change document root halfway through and the symptom would be a served file from
        // the wrong site.
        let state = Swappable::new(1u32);
        let held = state.load();
        let previous = state.replace(2);
        assert_eq!(*held, 1, "a held snapshot must not change under it");
        assert_eq!(*previous, 1, "replace reports what it replaced");
        assert_eq!(*state.load(), 2);
    }

    #[test]
    fn a_reader_that_loads_while_a_replace_is_racing_sees_one_value_or_the_other() {
        // Not a proof of atomicity — that is `ArcSwap`'s and its own tests' business. What this
        // checks is the property the *callers* rely on: no reader observes a value that was never
        // installed. A torn read would show up here as a value that is neither.
        let state = Swappable::new(0u32);
        let stop = AtomicBool::new(false);
        let seen: Vec<u32> = std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let mut seen = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    seen.push(*state.load());
                }
                seen
            });
            for value in 1..=500u32 {
                let _ = state.replace(value);
            }
            stop.store(true, Ordering::Relaxed);
            reader.join().expect("the reader thread panicked")
        });
        assert!(
            seen.iter().all(|value| *value <= 500),
            "a reader saw a value that was never installed: {seen:?}"
        );
        assert_eq!(
            *state.load(),
            500,
            "the last write must be the value visible now"
        );
    }

    #[test]
    fn replacing_the_shared_table_moves_every_holder_at_once() {
        // The two-listener case, and the reason this type exists: a clone handed to the FastCGI
        // front-end sees the replacement the HTTP front-end made, so a reloaded site cannot take
        // effect on one protocol only.
        let http = SharedState::new(sites_for("/srv/first"));
        let fastcgi = http.clone();

        assert_eq!(
            fastcgi.sites().default_site().document_root,
            Path::new("/srv/first")
        );
        let replaced = http.replace(sites_for("/srv/second"));

        assert_eq!(
            replaced.default_site().document_root,
            Path::new("/srv/first"),
            "replace must report the table it replaced, not the one it installed"
        );
        assert_eq!(
            fastcgi.sites().default_site().document_root,
            Path::new("/srv/second"),
            "a clone taken before the replacement must see the new table"
        );
        assert_eq!(
            http.sites().default_site().document_root,
            Path::new("/srv/second")
        );
    }

    /// A site table whose site and root are both named, so a sample can tell them apart.
    fn named_sites(root: &str) -> Sites {
        let text = format!(
            "[server]\nhttp = \"127.0.0.1:0\"\ndocument_root = {root:?}\n\n\
             [sites.app]\nhosts = [\"app.test\"]\ndocument_root = {root:?}\n",
            root = root
        );
        let config = Config::from_toml(&text).expect("config parses");
        Sites::from_config(&config, Path::new(root))
    }

    /// The sampling technique used by the concurrency test **would** catch a split generation.
    ///
    /// This is the control for `a_reload_under_concurrent_requests_is_one_configuration_per_request`
    /// in `volucer-http`, and it exists because a test whose claim is "no sample ever disagreed"
    /// passes just as well against a sampler that cannot disagree. Hazard 57's shape exactly: the
    /// assertion has to be able to fail.
    ///
    /// So the three parts are split *deliberately* — three `Swappable`s where the real type has
    /// one — and the same loop is run. If a disagreement is not observed here, the real test is
    /// not observing anything either.
    ///
    /// `Reloaded` is the type that makes this unwritable for callers, so the split is modelled
    /// with what a caller *could* have written: separate values for the table, the routing and the
    /// static layer, moved one after another.
    #[test]
    fn three_separate_swaps_be_observed_mid_generation() {
        let table = Swappable::new(named_sites("/srv/first"));
        let routings = Swappable::new(Path::new("/srv/first").to_path_buf());
        let statics = Swappable::new(Path::new("/srv/first").to_path_buf());

        let observed_a_mixture = std::sync::atomic::AtomicBool::new(false);
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    // A request's three reads, in the order it would make them. Between any two
                    // of these a writer can land, which is the whole defect.
                    let sites = table.load();
                    let root = sites.default_site().document_root.clone();
                    let routing = routings.load();
                    let statics = statics.load();
                    if root != *routing || root != *statics {
                        observed_a_mixture.store(true, Ordering::Relaxed);
                    }
                }
            });

            for round in 0..20_000 {
                // Alternating, so a reader that only ever samples one direction cannot look clean
                // by accident.
                let to = if round % 2 == 0 {
                    "/srv/second"
                } else {
                    "/srv/first"
                };
                // A yield between the three writes, which is what makes a mixture observable
                // rather than a matter of luck: the reader's loop is pure atomic loads, so it gets
                // in between them every time. Without the yields this test is a race that would
                // only *sometimes* see the split, and a control that sometimes cannot fail is not
                // a control.
                let _ = table.replace(named_sites(to));
                std::thread::yield_now();
                let _ = routings.replace(Path::new(to).to_path_buf());
                std::thread::yield_now();
                let _ = statics.replace(Path::new(to).to_path_buf());
            }
            stop.store(true, Ordering::Relaxed);
            reader.join().expect("the reader thread panicked");
        });

        assert!(
            observed_a_mixture.load(Ordering::Relaxed),
            "the sampler never saw a split generation even with the parts deliberately separated, \
             so the concurrency test's clean run proves nothing"
        );
    }

    /// Every part of one generation is installed by **one** swap, which is what makes the
    /// concurrency test's clean run meaningful rather than lucky.
    ///
    /// The counterpart to the test above: the same racing reader, against the real type. The
    /// reader can only ever see two complete generations, and the assertion is that the sampling
    /// loop *ran* — a sampler that never sampled would "observe no mixture" too.
    #[test]
    fn one_swap_never_yields_a_partial_generation() {
        let state = Swappable::new(Reloaded::from_sites(
            Arc::new(named_sites("/srv/first")),
            8080,
        ));

        let stop = AtomicBool::new(false);
        let mut seen: Vec<PathBuf> = Vec::new();
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let mut seen = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    let snapshot = state.load();
                    let resolved = snapshot.resolve(Some("app.test"));
                    assert_eq!(
                        resolved.routing.document_root, resolved.site.document_root,
                        "one swap produced a routing from another generation"
                    );
                    assert_eq!(
                        resolved.statics.document_root, resolved.site.document_root,
                        "one swap produced a static layer from another generation"
                    );
                    seen.push(resolved.site.document_root.clone());
                }
                seen
            });

            for round in 0..5_000 {
                let root = if round % 2 == 0 {
                    "/srv/first"
                } else {
                    "/srv/second"
                };
                let _ = state.replace(Reloaded::from_sites(Arc::new(named_sites(root)), 8080));
            }
            stop.store(true, Ordering::Relaxed);
            seen = reader.join().expect("the reader thread panicked");
        });

        assert!(
            !seen.is_empty(),
            "the sampler never ran, so it observed nothing"
        );
        assert!(
            seen.iter()
                .all(|root| root == Path::new("/srv/first") || root == Path::new("/srv/second")),
            "a sample resolved to a root no generation named"
        );
    }
}
