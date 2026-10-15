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
//! listener addresses, backend commands and worker counts. Those cannot be replaced in place, and
//! pretending otherwise is how a reload comes to change half of what it was asked to change. The
//! rule this crate follows is that **everything behind a [`Swappable`] is replaceable *whole*, and
//! everything outside one is restart-only** — so the line is drawn by the type rather than by a
//! comment a later change can contradict. The restart-only list is *reported* rather than
//! discovered; see `volucer check`.

use std::sync::Arc;

use arc_swap::ArcSwap;
use volucer_core::sites::Sites;

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
}
