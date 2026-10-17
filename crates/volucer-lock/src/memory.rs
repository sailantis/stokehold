//! The in-memory driver: one process's locks, and nothing else.
//!
//! # What it is for
//!
//! A single server's workers are most of the lock traffic there is — the same two requests racing to
//! render the same PDF, the same cache miss being collapsed by eight workers — and this driver handles
//! all of it with no service installed. A second server, or a process that is not a worker, needs the
//! Redis driver and the deployment has to say so.
//!
//! # Expiry is decided on read, and swept when an insert is about to be refused
//!
//! The same two-mechanism shape the KV memory driver uses, for the same reason and with the same
//! division of labour. A read never reports an expired holder, so a caller cannot be told a lock is
//! held a moment after it expired; the sweep is what keeps a *process* from accumulating an entry per
//! resource it ever locked, which is a leak that only shows up as memory growth in a deployment that
//! has been up for weeks.
//!
//! The sweep runs when an insert arrives and the last one was longer ago than [`SWEEP_INTERVAL`],
//! rather than on a timer: a timer needs a thread of its own, and the thing being swept is only ever
//! in the way of an insert. There is no byte bound here as there is on a KV store — a lock holds a name
//! and an id, both already bounded, and refusing to *acquire* because the server is holding too many
//! locks would turn a leak into an outage.
//!
//! # Waiting is a poll, and the slice is a real trade-off
//!
//! There is no condition variable to signal on, because the thing being waited for is a lease expiry
//! and a lease can expire because *another process* stopped existing rather than because anybody acted.
//! So a waiting acquisition polls, and [`WAIT_SLICE`] is how often. Too long and every contended
//! acquisition pays it — a worker waiting five milliseconds for a lock held for one is a worker serving
//! five milliseconds fewer requests — and too short and a pool of thirty waiters is a spin. Five
//! milliseconds is the compromise, stated here rather than hidden, and it is deliberately **not**
//! RoadRunner's one millisecond: a call is answered on the worker's own exchange thread, so a tight poll
//! costs a core rather than a task.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::{Entry, Error, Holder, Lease, Locks, Mode, Registry, Result};

#[cfg(test)]
use crate::MAX_NAME_BYTES;

/// How often a waiting acquisition re-tries.
///
/// See the module documentation for why this number and why it is not one millisecond.
pub const WAIT_SLICE: Duration = Duration::from_millis(5);

/// How often an insert is allowed to walk the whole registry.
///
/// The sweep is over *every* resource rather than one, because the leak it prevents is a resource whose
/// last holder left and which is never named again — which by definition has nothing to point at it.
/// One second is short enough that a busy process never accumulates more than a second of them and long
/// enough that a process inserting in a loop does not walk its own registry on every call.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// One process's locks.
#[derive(Debug)]
pub struct MemoryLocks {
    inner: Mutex<Inner>,
    wait_slice: Duration,
    sweep_interval: Duration,
}

#[derive(Debug)]
struct Inner {
    registry: Registry,
    last_sweep: Instant,
}

impl Default for MemoryLocks {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryLocks {
    /// A new, empty store.
    pub fn new() -> Self {
        MemoryLocks {
            inner: Mutex::new(Inner {
                registry: Registry::default(),
                last_sweep: Instant::now(),
            }),
            wait_slice: WAIT_SLICE,
            sweep_interval: SWEEP_INTERVAL,
        }
    }

    /// The same store with a shorter wait slice and sweep interval, for a test that cannot afford to
    /// spend a second finding out whether the sweep happens at all.
    ///
    /// Public because a *test in another crate* is the caller: the properties worth pinning — that a
    /// contended acquisition eventually succeeds, that a wait expires — involve real time, and a
    /// private constructor would mean either tests that sleep for a second apiece or no tests.
    pub fn with_timing(wait_slice: Duration, sweep_interval: Duration) -> Self {
        MemoryLocks {
            inner: Mutex::new(Inner {
                registry: Registry::default(),
                last_sweep: Instant::now(),
            }),
            wait_slice,
            sweep_interval,
        }
    }

    /// How many resources are tracked, expired ones included.
    ///
    /// A diagnostic for a test — see the module documentation on why the sweep is what keeps this from
    /// growing, and why nothing in the API needs to expose it.
    pub fn tracked(&self) -> usize {
        self.lock().registry.len()
    }

    /// A poisoned lock is one a previous call panicked while holding.
    ///
    /// Recovering is right for the same reason the KV drivers do it: the guarded value is a registry of
    /// names and deadlines that no panic can leave inconsistent — every mutation here is a single call
    /// that either ran or did not — and refusing every later acquisition for the rest of the process's
    /// life would turn one panicked request into a server that cannot lock anything.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Sweep if enough time has passed, then insert.
    ///
    /// The two are one function so that no insert can be written that forgets to consider the sweep —
    /// which is exactly how a bounded structure becomes an unbounded one.
    fn insert_housekeeping(&self, inner: &mut Inner, resource: &str, entry: Entry) {
        let now = Instant::now();
        if now.saturating_duration_since(inner.last_sweep) >= self.sweep_interval {
            inner.registry.sweep(now);
            inner.last_sweep = now;
        }
        inner.registry.insert(resource, entry);
    }

    /// The deadline a lease implies, from `now`.
    fn deadline(lease: Lease, now: Instant) -> Option<Instant> {
        // `checked_add` rather than `+`: `Lease::Forever` is `None` here, and a clamped lease is at most
        // ten years, so this cannot overflow in practice — but an overflow panic inside a lock
        // acquisition would be a server that dies rather than one that refuses, and the clamp is in
        // another crate that could change.
        lease
            .millis()
            .and_then(|millis| now.checked_add(Duration::from_millis(millis)))
    }
}

impl Locks for MemoryLocks {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn try_acquire(
        &self,
        resource: &str,
        id: &str,
        mode: Mode,
        lease: Lease,
    ) -> Result<Option<Holder>> {
        crate::validate(resource, id)?;
        let mut inner = self.lock();
        let now = Instant::now();
        inner.registry.purge(resource, now);

        // Two questions, asked in this order and both asked here rather than in each driver, because
        // they are the two ways an acquisition is refused and the order decides which message a caller
        // gets. Same id first: an id that holds the resource is *also* a conflict when it holds it
        // exclusively, and "you already hold this" is the more useful of the two answers.
        if let Some(held) = inner.registry.held_by(resource, id, now) {
            return Err(Error::AlreadyHeld {
                resource: resource.to_string(),
                id: id.to_string(),
                mode: held.as_str(),
            });
        }
        if inner.registry.conflicted(resource, mode, now) {
            // Contention is `Ok(None)`: an ordinary answer, not an error. The caller can retry, wait, or
            // do something else, and none of those is a failure.
            return Ok(None);
        }

        let entry = Entry {
            id: id.to_string(),
            mode,
            deadline: Self::deadline(lease, now),
        };
        let holder = entry.holder(now);
        self.insert_housekeeping(&mut inner, resource, entry);
        Ok(Some(holder))
    }

    fn acquire(
        &self,
        resource: &str,
        id: &str,
        mode: Mode,
        lease: Lease,
        wait: Duration,
    ) -> Result<Option<Holder>> {
        // One attempt when there is nothing to wait for, so a caller that passed zero does not read the
        // clock and does not sleep — and so that `wait = 0` is provably the same call as `try_acquire`
        // rather than a loop that happens to exit first time.
        if wait.is_zero() {
            return self.try_acquire(resource, id, mode, lease);
        }

        let deadline = Instant::now() + wait;
        loop {
            // The deadline is checked *before* an attempt, not after, so a wait that has already expired
            // does not make one more attempt than it promised. The first pass is always an attempt,
            // because the deadline is in the future until the clock passes it.
            if let Some(holder) = self.try_acquire(resource, id, mode, lease)? {
                return Ok(Some(holder));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            // Sleep the slice, or what is left of the wait if that is less — so the last sleep never
            // overshoots the deadline the caller gave, which would make `wait = 1ms` take up to 5.
            std::thread::sleep(self.wait_slice.min(deadline.saturating_duration_since(now)));
        }
    }

    fn release(&self, resource: &str, id: &str) -> Result<bool> {
        crate::validate(resource, id)?;
        let mut inner = self.lock();
        Ok(inner.registry.remove(resource, id, Instant::now()))
    }

    fn release_all(&self, resource: &str) -> Result<bool> {
        crate::validate_resource(resource)?;
        let mut inner = self.lock();
        Ok(inner.registry.remove_all(resource, Instant::now()))
    }

    fn holders(&self, resource: &str) -> Result<Vec<Holder>> {
        crate::validate_resource(resource)?;
        let mut inner = self.lock();
        Ok(inner.registry.holders(resource, Instant::now()))
    }

    fn update_lease(&self, resource: &str, id: &str, lease: Lease) -> Result<bool> {
        crate::validate(resource, id)?;
        let mut inner = self.lock();
        let now = Instant::now();
        // **Replaces** the deadline rather than extending it: the new one is computed from `now`, so a
        // caller renewing for the same duration the lock originally had gets that duration again and not
        // the old deadline plus it. RoadRunner's own documentation is explicit that it does not add.
        Ok(inner
            .registry
            .set_deadline(resource, id, Self::deadline(lease, now), now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> MemoryLocks {
        MemoryLocks::new()
    }

    /// A lease long enough that a slow machine cannot expire it mid-test, and short enough that a test
    /// never waits for the sweep.
    fn long() -> Lease {
        Lease::for_duration(Duration::from_secs(60))
    }

    #[test]
    fn an_exclusive_holder_excludes_and_an_id_that_holds_it_is_told_so() {
        let store = store();
        let first = store
            .try_acquire("pdf", "worker-1", Mode::Exclusive, long())
            .expect("nothing holds it")
            .expect("so the acquisition succeeds");
        assert_eq!(first.id, "worker-1");
        assert_eq!(first.mode, Mode::Exclusive);
        assert!(first.remaining.is_some());

        // Somebody else: contention, which is `None` and not an error.
        assert_eq!(
            store
                .try_acquire("pdf", "worker-2", Mode::Exclusive, long())
                .expect("contention is an ordinary answer"),
            None
        );

        // The same id again: a mistake, and a distinct error naming the way out of it.
        let err = store
            .try_acquire("pdf", "worker-1", Mode::Exclusive, long())
            .expect_err("one id may not hold one resource twice");
        assert!(matches!(err, Error::AlreadyHeld { .. }), "{err}");
        assert!(err.to_string().contains("update_lease"), "{err}");
    }

    #[test]
    fn readers_share_and_a_writer_excludes_them() {
        let store = store();
        for reader in ["reader-1", "reader-2", "reader-3"] {
            store
                .try_acquire("pdf", reader, Mode::Shared, long())
                .unwrap_or_else(|err| panic!("{reader} should have shared it: {err}"))
                .unwrap_or_else(|| panic!("{reader} should have shared it"));
        }
        assert_eq!(store.holders("pdf").expect("holders").len(), 3);

        assert_eq!(
            store
                .try_acquire("pdf", "writer", Mode::Exclusive, long())
                .expect("contention is an ordinary answer"),
            None,
            "three readers must keep a writer out"
        );

        // And the other direction: a writer keeps readers out.
        store.release_all("pdf").expect("release all");
        store
            .try_acquire("pdf", "writer", Mode::Exclusive, long())
            .expect("nothing holds it")
            .expect("so the writer gets it");
        assert_eq!(
            store
                .try_acquire("pdf", "reader-1", Mode::Shared, long())
                .expect("contention is an ordinary answer"),
            None,
            "an exclusive holder must keep a reader out"
        );
    }

    #[test]
    fn an_id_that_holds_a_resource_shared_is_refused_a_second_acquisition_in_either_mode() {
        let store = store();
        store
            .try_acquire("pdf", "worker-1", Mode::Shared, long())
            .expect("nothing holds it")
            .expect("so it shares the resource");

        // A shared holder does not *conflict* with itself, so a driver that only asked about conflicts
        // would grant this — and the id would then hold two leases it cannot release separately. Only
        // the already-held question catches it, which is why both questions are asked.
        let err = store
            .try_acquire("pdf", "worker-1", Mode::Shared, long())
            .expect_err("a second shared acquisition with the same id is still a mistake");
        assert!(matches!(err, Error::AlreadyHeld { .. }), "{err}");
    }

    #[test]
    fn release_answers_whether_there_was_something_to_release() {
        let store = store();
        store
            .try_acquire("pdf", "worker-1", Mode::Exclusive, long())
            .expect("nothing holds it")
            .expect("acquired");

        assert!(store.release("pdf", "worker-1").expect("release"));
        assert!(
            !store.release("pdf", "worker-1").expect("release again"),
            "a second release is an answer, not an error: a caller in a `finally` must not have to \
             check first, and must not be told its cleanup failed"
        );
        assert!(
            !store.release("pdf", "worker-2").expect("release"),
            "and an id that never held it is the same answer"
        );
    }

    #[test]
    fn release_all_takes_the_resource_from_everyone_and_says_whether_anybody_held_it() {
        let store = store();
        store
            .try_acquire("pdf", "reader-1", Mode::Shared, long())
            .expect("nothing holds it")
            .expect("acquired");
        store
            .try_acquire("pdf", "reader-2", Mode::Shared, long())
            .expect("nothing holds it")
            .expect("acquired");

        assert!(store.release_all("pdf").expect("release all"));
        assert!(store.holders("pdf").expect("holders").is_empty());
        assert!(
            !store.release_all("pdf").expect("release all again"),
            "and the second call answers `false` rather than pretending to have released something"
        );
        assert!(
            !store.release_all("never-locked").expect("release all"),
            "a resource nobody ever held reports nothing released, which is the same `false`"
        );
    }

    #[test]
    fn update_lease_replaces_the_deadline_rather_than_adding_to_it() {
        let store = store();
        store
            .try_acquire(
                "pdf",
                "worker-1",
                Mode::Exclusive,
                Lease::for_duration(Duration::from_secs(10)),
            )
            .expect("nothing holds it")
            .expect("acquired");

        // Shorter: a renewer that added would make this *longer* than the original, so a value that
        // comes back smaller than the first one is the property that distinguishes replace from extend.
        assert!(store
            .update_lease(
                "pdf",
                "worker-1",
                Lease::for_duration(Duration::from_secs(1))
            )
            .expect("update"));
        let remaining = store.holders("pdf").expect("holders")[0]
            .remaining
            .expect("a lease with a deadline");
        assert!(
            remaining <= Duration::from_secs(1),
            "the new deadline is a second from now, not ten plus one: {remaining:?}"
        );
        assert!(
            remaining > Duration::from_millis(900),
            "and it is a whole second, not a fraction of one: {remaining:?}"
        );

        // Forever is reachable the same way, and it is the state a caller renewing a lock it does not
        // intend to drop wants.
        assert!(store
            .update_lease("pdf", "worker-1", Lease::Forever)
            .expect("update"));
        assert_eq!(store.holders("pdf").expect("holders")[0].remaining, None);

        assert!(
            !store
                .update_lease("pdf", "worker-2", long())
                .expect("update"),
            "an id that holds nothing gets `false`, which is the answer a caller must act on: the \
             section it is in is no longer protected"
        );
    }

    #[test]
    fn an_expired_lease_frees_the_resource_without_anybody_releasing_it() {
        let store = store();
        store
            .try_acquire(
                "pdf",
                "worker-1",
                Mode::Exclusive,
                Lease::for_duration(Duration::from_millis(1)),
            )
            .expect("nothing holds it")
            .expect("acquired");

        // The property a TTL lock exists for: a worker that died holding it does not wedge the resource
        // for ever. Sleeping past a one-millisecond lease costs a test nothing.
        std::thread::sleep(Duration::from_millis(20));

        assert!(
            !store.exists("pdf").expect("exists"),
            "the lease has run out, so nothing holds the resource"
        );
        let holder = store
            .try_acquire("pdf", "worker-2", Mode::Exclusive, long())
            .expect("nothing holds it")
            .expect("so a second worker takes it without waiting for anybody to release");
        assert_eq!(holder.id, "worker-2");
    }

    #[test]
    fn a_waiting_acquisition_takes_the_resource_as_soon_as_the_holder_releases_it() {
        let store = std::sync::Arc::new(store());
        store
            .try_acquire("pdf", "worker-1", Mode::Exclusive, long())
            .expect("nothing holds it")
            .expect("acquired");

        let contender = std::sync::Arc::clone(&store);
        let waiting = std::thread::spawn(move || {
            contender
                .acquire(
                    "pdf",
                    "worker-2",
                    Mode::Exclusive,
                    long(),
                    Duration::from_secs(5),
                )
                .expect("waiting is not an error")
        });

        // Long enough that the contender has certainly made more than one attempt — the point of the
        // test is that it does *not* succeed on the first one and does not give up.
        std::thread::sleep(Duration::from_millis(50));
        assert!(store.release("pdf", "worker-1").expect("release"));

        let acquired = waiting.join().expect("the contender thread must not panic");
        assert_eq!(
            acquired.map(|holder| holder.id),
            Some("worker-2".to_string()),
            "the contender must have acquired it as soon as it was free"
        );
    }

    #[test]
    fn a_wait_that_nobody_satisfies_ends_in_none_rather_than_an_error() {
        let store = store();
        store
            .try_acquire("pdf", "worker-1", Mode::Exclusive, long())
            .expect("nothing holds it")
            .expect("acquired");

        let started = Instant::now();
        let outcome = store
            .acquire(
                "pdf",
                "worker-2",
                Mode::Exclusive,
                long(),
                Duration::from_millis(30),
            )
            .expect("an expired wait is not an error");
        let waited = started.elapsed();

        assert_eq!(outcome, None, "nobody released it, so the wait ran out");
        assert!(
            waited >= Duration::from_millis(30),
            "and it waited the whole time it was given: {waited:?}"
        );
    }

    #[test]
    fn a_zero_wait_makes_exactly_one_attempt() {
        let store = MemoryLocks::with_timing(Duration::from_secs(5), Duration::from_secs(60));
        store
            .try_acquire("pdf", "worker-1", Mode::Exclusive, long())
            .expect("nothing holds it")
            .expect("acquired");

        // With a five-second slice, an implementation that looped even once would make this test take
        // five seconds. It taking none is the property: `wait = 0` is one attempt, not a wait with a
        // short deadline.
        let started = Instant::now();
        let outcome = store
            .acquire("pdf", "worker-2", Mode::Exclusive, long(), Duration::ZERO)
            .expect("zero wait");
        assert_eq!(outcome, None);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a zero wait must not sleep at all: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_lease_that_does_not_expire_reports_no_remaining_time() {
        let store = store();
        let holder = store
            .try_acquire("pdf", "worker-1", Mode::Exclusive, Lease::Forever)
            .expect("nothing holds it")
            .expect("acquired");
        assert_eq!(holder.remaining, None, "`ttl = 0` is held until released");

        let holders = store.holders("pdf").expect("holders");
        assert_eq!(holders.len(), 1);
        assert_eq!(holders[0].remaining, None);
        assert!(store.exists("pdf").expect("exists"));
    }

    #[test]
    fn a_holder_is_reported_with_what_is_left_of_its_lease_rather_than_what_it_asked_for() {
        let store = store();
        store
            .try_acquire(
                "pdf",
                "worker-1",
                Mode::Exclusive,
                Lease::for_duration(Duration::from_secs(30)),
            )
            .expect("nothing holds it")
            .expect("acquired");

        // A caller deciding whether to renew needs what is left. A driver reporting the requested
        // duration would tell it how much it once had, and the difference grows with the delay.
        let remaining = store.holders("pdf").expect("holders")[0]
            .remaining
            .expect("a deadline");
        assert!(
            remaining <= Duration::from_secs(30) && remaining > Duration::from_secs(29),
            "close to thirty seconds and never above it: {remaining:?}"
        );
    }

    #[test]
    fn resources_are_independent_so_locking_one_does_not_lock_another() {
        let store = store();
        store
            .try_acquire("pdf:a", "worker-1", Mode::Exclusive, long())
            .expect("nothing holds it")
            .expect("acquired");

        store
            .try_acquire("pdf:b", "worker-1", Mode::Exclusive, long())
            .expect("a different resource is a different lock")
            .expect("and the same id may hold both");

        // The same id holding two *different* resources is fine — the `AlreadyHeld` rule is per
        // resource, which is what makes an id usable as "this worker's name" rather than as a lock.
        assert_eq!(store.holders("pdf:a").expect("holders").len(), 1);
        assert_eq!(store.holders("pdf:b").expect("holders").len(), 1);
    }

    #[test]
    fn an_empty_or_oversized_name_is_refused_before_anything_is_stored() {
        let store = store();
        assert!(matches!(
            store.try_acquire("", "worker-1", Mode::Exclusive, long()),
            Err(Error::BadResource { .. })
        ));
        assert!(matches!(
            store.try_acquire("pdf", "", Mode::Exclusive, long()),
            Err(Error::BadId { .. })
        ));
        assert!(matches!(
            store.release("", "worker-1"),
            Err(Error::BadResource { .. })
        ));
        assert!(matches!(
            store.release_all(""),
            Err(Error::BadResource { .. })
        ));
        assert!(matches!(store.holders(""), Err(Error::BadResource { .. })));

        let oversized = "x".repeat(MAX_NAME_BYTES + 1);
        assert!(matches!(
            store.try_acquire(&oversized, "worker-1", Mode::Exclusive, long()),
            Err(Error::BadResource { .. })
        ));
        assert_eq!(
            store.tracked(),
            0,
            "and a refused acquisition must not have stored anything"
        );
    }

    #[test]
    fn the_registry_does_not_grow_for_ever_when_nothing_is_named_again() {
        // The leak the sweep exists for: every acquisition of a distinct resource leaves an entry, and
        // nothing ever names those resources again to trigger a lazy purge. A server that ran for weeks
        // would grow by one entry per lock it ever took.
        let store = MemoryLocks::with_timing(WAIT_SLICE, Duration::from_millis(1));
        for i in 0..50 {
            store
                .try_acquire(
                    &format!("resource-{i}"),
                    "worker-1",
                    Mode::Exclusive,
                    Lease::for_duration(Duration::from_millis(1)),
                )
                .expect("nothing holds it")
                .expect("acquired");
        }
        assert_eq!(store.tracked(), 50, "fifty resources were locked");

        // Let the leases expire and the sweep interval pass, then insert once more. That one insert is
        // what reclaims the other forty-nine, which is the whole design: the sweep rides on the work
        // rather than on a timer thread.
        std::thread::sleep(Duration::from_millis(20));
        store
            .try_acquire("one-more", "worker-1", Mode::Exclusive, long())
            .expect("nothing holds it")
            .expect("acquired");

        assert_eq!(
            store.tracked(),
            1,
            "one live resource, and everything that expired is gone"
        );
    }
}
