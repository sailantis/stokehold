//! State a running listener can replace without dropping a connection.
//!
//! A reload is the one operation that has to change what a server is doing *while it is
//! doing it*, so this module exists to make that a single decision in one place rather than
//! a property each field has to remember.
//!
//! # Why a swap and not a lock
//!
//! The read is the request path and the write happens once per reload, which is the shape a
//! read-write lock handles badly: `RwLock` still has every reader touch one atomic counter,
//! and a writer cannot make progress until they all leave. That is fine at startup and
//! wasteful per request — and this front-end's whole design note is that a request's cost is
//! not allowed to depend on a feature the deployment is not using.
//!
//! [`arc_swap::ArcSwap`] is the reverse: readers load a pointer with no write ever blocking
//! them, which is what makes a reload safe to do under load rather than merely possible.
//!
//! # The one number that matters
//!
//! This is on the request path, so "it is only an atomic load" is a claim to **measure**
//! rather than assert. `volucer-competition/scripts/tls-off-ab.js` is the harness and its
//! method — interleaved arms, rotated order, a discarded warmup, and an A-vs-A control that
//! gives a null its scale. The cost of this indirection is recorded there, because a number
//! this project asserts about a hot path is a number it has to have taken.
//!
//! # What is *not* reloadable, and why that is not a limitation to work around
//!
//! Some settings describe a socket that is already bound or a process that is already
//! running: listener addresses, backend commands and worker counts. Those cannot be
//! replaced in place, and pretending otherwise is how a reload comes to change half of what
//! it was asked to change. The rule this module follows is that everything behind a
//! [`Swappable`] is replaceable *whole*, and everything outside one is restart-only — so the
//! line is drawn by the type rather than by a comment that a later change can contradict.

use std::sync::Arc;

use arc_swap::ArcSwap;

/// A value that can be replaced while readers are using it.
///
/// Always used behind an `Arc`, because a snapshot has to be cloneable into the blocking task
/// that serves the request: [`Swappable::load`] returns an owned `Arc`, so a handler that
/// takes one snapshot keeps the value it started with even if a reload lands mid-request.
/// That is deliberate. A request that resolved its site against one table and then routed
/// against another would be two answers to one question, and the failure would look like a
/// site serving the wrong document root rather than like a race.
///
/// `T: Send + Sync` is what the `Arc` requires for the readers to be on other threads, and
/// it is the whole of the type's constraint: a value that can be shared concurrently can be
/// swapped concurrently.
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
    /// This is the request-path read, so it is deliberately the only way in: there is no
    /// `Deref`, because a `Deref` would let a caller hold a borrow across an `.await` and
    /// thereby hold a snapshot it never chose to hold. Taking an `Arc` is the explicit
    /// version of the same thing, and it cannot outlive the guard by accident.
    pub fn load(&self) -> Arc<T> {
        self.value.load_full()
    }

    /// Replace the value, returning the previous one.
    ///
    /// Returns the old value so a caller can say *what* changed rather than only that a
    /// reload happened — which is the difference between an operator reading "site `api`
    /// document_root changed" and reading "configuration reloaded" and having to go and
    /// diff the file themselves.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn a_snapshot_taken_before_a_replace_still_reads_the_old_value() {
        // The property the request path depends on: a request that has its site table keeps
        // it, even if a reload lands before the response is written. If this ever failed, a
        // request could change document root halfway through and the symptom would be a
        // served file from the wrong site.
        let state = Swappable::new(1u32);
        let held = state.load();
        let previous = state.replace(2);
        assert_eq!(*held, 1, "a held snapshot must not change under it");
        assert_eq!(*previous, 1, "replace reports what it replaced");
        assert_eq!(*state.load(), 2);
    }

    #[test]
    fn a_reader_that_loads_while_a_replace_is_racing_sees_one_value_or_the_other() {
        // Not a proof of atomicity — that is `ArcSwap`'s and its own tests' business. What
        // this checks is the property the *callers* rely on: no reader observes a value that
        // was never installed. A torn read would show up here as a value that is neither.
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
}
