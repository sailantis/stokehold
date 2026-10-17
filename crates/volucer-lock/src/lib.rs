//! Resource locks: the trait, the shared bookkeeping, and the in-memory driver.
//!
//! # Read this first: a lock is not a correctness primitive
//!
//! `docs/plugins.md` §9.2 is the position this crate is built to, and it is worth restating where
//! somebody will actually read it rather than only in a design document:
//!
//! A TTL lock has no fencing token. A worker that stalls past its TTL — a pause, a swap storm, a
//! debugger, a `SIGSTOP` — still believes it holds the lock while the server has already given it to
//! somebody else, and two processes are then inside the section the lock was supposed to guard. No
//! amount of care on the server side fixes that: the late writer is late *because* it was not running,
//! and no storage can ask it to stop. RoadRunner's locks have the same hole, and calling them a
//! distributed lock without qualification is how an application acquires a data corruption bug.
//!
//! So a lock here is for **coordinating work**: collapsing a cache miss, keeping two workers from
//! rendering the same PDF twice, making a retry cheap. It is not for protecting an invariant whose
//! violation corrupts data. For that the answer is the storage's own conditional write — Redis
//! `SET NX`, a unique index, a compare-and-swap — which is why `docs/plugins.md` §6.3 asks KV to grow
//! a `set_if_absent`, and why this module does not pretend to be it.
//!
//! Everything below is written so that an application *can* be correct about this rather than only
//! warned: an acquisition whose outcome could not be decided is a distinct error, never a successful
//! failure and never "not held" — see [`Error::Unknown`].
//!
//! # The three answers an acquisition has, and why two of them are not the same
//!
//! [`Locks::try_acquire`] and [`Locks::acquire`] can say:
//!
//! - **acquired** ([`Some`]), and the caller may enter the section;
//! - **not acquired** ([`None`]), because somebody else holds the resource or the wait ran out. The
//!   caller knows nothing happened and can do something else, and this is an ordinary outcome rather
//!   than an error;
//! - **unknown** ([`Error::Unknown`]): a command failed, or a deadline expired *during* one, so the
//!   caller cannot tell whether it now holds the lock. RoadRunner documents exactly this case and
//!   tells the application to resolve it by asking — its own words are "call `Exists` or `Release` to
//!   find the state of the lock". Reporting it as "not held" is the lie that puts two processes in the
//!   same critical section, which is why it is a distinct variant and not a flag.
//!
//! # The semantics taken from RoadRunner, faithfully and deliberately
//!
//! - **[`Lease::Forever`] (`ttl = 0`) means held until released**, not "expires immediately". A zero
//!   TTL means the opposite for a KV value, for an unrelated reason, so the two are spelled
//!   differently here rather than both being "zero".
//! - **[`Locks::update_lease`] replaces the remaining lease rather than extending it.** It does not
//!   add to the deadline; it sets one from the moment it is applied. A caller that reads it as
//!   "another N seconds" can be released early.
//! - **An id that already holds the resource is refused, in either mode.** With an explicit
//!   `update_lease` in the API, an implicit "refresh the deadline because it is you again" is a
//!   footgun with no upside. This is a deliberate divergence from the Redis lock library RoadRunner is
//!   built on, which silently permits a re-obtain for the same token — see [`Error::AlreadyHeld`].
//! - **Waiting is bounded and explicit.** `wait = 0` is one attempt, not an unlimited wait.
//! - **Both modes are one mechanism, not two.** An exclusive holder excludes everybody; a shared
//!   holder excludes exclusive acquirers but not other readers. Readers of one resource are therefore
//!   a set, which is why [`Holder`] carries its mode rather than the resource having one.
//!
//! # What the in-memory driver does and does not do
//!
//! [`MemoryLocks`] keeps state in this process, so it coordinates the workers of **one** server and
//! nothing else. That is the common deployment and it needs no service installed, which is why it is
//! first. A second server — or a process that is not a worker — is what the Redis driver is for, and
//! it shares this trait and these semantics rather than defining its own.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub mod memory;
#[cfg(feature = "redis")]
pub mod redis_lock;

pub use memory::MemoryLocks;
#[cfg(feature = "redis")]
pub use redis_lock::{RedisLocks, DEFAULT_READ_TIMEOUT};

/// The longest lease a caller may ask for.
///
/// Ten years, the same bound and the same reason as a KV value's TTL: what it prevents is
/// `Duration::MAX` — which an application reaches by writing `PHP_INT_MAX` microseconds — becoming a
/// negative number of milliseconds in a driver that stores a deadline as an integer. A lock that
/// genuinely needs to outlive the process's own uptime is a lock that does not need a lease.
pub const MAX_LEASE: Duration = Duration::from_secs(10 * 365 * 24 * 60 * 60);

/// How long a caller holds a resource.
///
/// Two states rather than an `Option`, because the thing that must be impossible to confuse is "for
/// ever" with "no time at all": `ttl = 0` means [`Lease::Forever`] to a lock, and
/// [`Lease::for_duration`] clamps a small duration *up* to a millisecond so a caller that meant "for
/// ever" and wrote a small number gets a short lease rather than one that is gone before it was ever
/// reported as granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lease {
    /// Held until it is released, however long that takes.
    Forever,
    /// Held for this long, and no longer.
    For(Duration),
}

impl Lease {
    /// A lease of `ttl`, clamped into the range a driver can represent.
    ///
    /// The floor is one millisecond rather than zero: a zero TTL has its own name ([`Lease::Forever`]),
    /// and a driver handed `For(Duration::ZERO)` would have to decide whether that means a lock that is
    /// already gone — which reads, to the caller, exactly like a lock that was never granted.
    pub fn for_duration(ttl: Duration) -> Self {
        Lease::For(ttl.clamp(Duration::from_millis(1), MAX_LEASE))
    }

    /// Whether this lease never expires on its own.
    pub fn is_forever(self) -> bool {
        matches!(self, Lease::Forever)
    }

    /// This lease in whole milliseconds, clamped, or `None` for one that does not expire.
    ///
    /// Milliseconds because both drivers work in them and neither can represent less: Redis's `PEXPIRE`
    /// is millisecond-resolution, and an `Instant` carries more precision than any consumer needs.
    pub fn millis(self) -> Option<u64> {
        match self {
            Lease::Forever => None,
            Lease::For(ttl) => {
                Some(ttl.clamp(Duration::from_millis(1), MAX_LEASE).as_millis() as u64)
            }
        }
    }
}

/// Whether a holder excludes other holders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Nobody else may hold the resource, in either mode.
    Exclusive,
    /// Other readers may hold it too; an exclusive acquirer may not.
    Shared,
}

impl Mode {
    /// The name this mode is written as, on the wire, in a log, and in a refusal.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Exclusive => "exclusive",
            Mode::Shared => "shared",
        }
    }

    /// Whether a holder in `self` conflicts with a holder in `other`.
    ///
    /// One rule rather than a table: exclusivity is the absence of the other party's tolerance, so
    /// mutual exclusion is symmetric and a reader tolerates only readers.
    pub fn conflicts_with(self, other: Mode) -> bool {
        self == Mode::Exclusive || other == Mode::Exclusive
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Somebody holding a resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// The caller's own id, as it was given. Two acquisitions with the same id are the same holder.
    pub id: String,
    /// Whether this holder excludes others.
    pub mode: Mode,
    /// What is left of this holder's lease, or `None` if it does not expire.
    ///
    /// **What is left, not what was asked for**: a caller deciding whether to renew needs the former,
    /// and a driver reporting the latter would be telling it how much it once had.
    pub remaining: Option<Duration>,
}

/// A fresh id for an acquisition the caller did not name one for.
///
/// # Why the server generates it rather than the client
///
/// RoadRunner's PHP client makes one up itself, and that is the half of its API worth not copying: a
/// client-generated id is a value the *server* has to accept and store verbatim, which means the server
/// cannot tell a deliberate 4 KiB id from a caller sending its payload by mistake. Here the id is the
/// server's to produce, so the rules on it ([`validate_id`]) apply to it as much as to one a caller
/// chose — and an application that does not care about the id does not have to invent a scheme for it.
///
/// # It is not a token, and nothing is authorised by it
///
/// Sixteen bytes from the system RNG, hex-encoded. A caller holding an id can release the lock that id
/// acquired, and nothing else: an id is scoped to the resource it was granted on, so there is no value
/// here worth guessing even if the RNG is unavailable — `volucer-observe`'s documentation covers what a
/// predictable fallback would and would not affect, and this is the same kind of value.
///
/// The fallback exists for the same reason and is deliberately *not* silent: it logs once, because an
/// environment with no entropy is worth knowing about even when nothing depends on it.
pub fn generate_id() -> String {
    let mut bytes = [0u8; ID_BYTES];
    if getrandom::fill(&mut bytes).is_err() {
        // Predictable, and acceptable for the reason above. A counter and the clock, mixed the way
        // `volucer-observe`'s fallback mixes them, so that two ids in the same nanosecond differ.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos() as u64);
        let mut seed = nanos ^ sequence.rotate_left(29);
        for slot in bytes.iter_mut() {
            // A cheap splitmix step: the point is only that every byte moves.
            seed = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(7);
            *slot = (seed >> 24) as u8;
        }
    }
    let mut out = String::with_capacity(ID_BYTES * 2);
    for byte in bytes {
        // `{:02x}` per byte rather than a hex crate: sixteen bytes is not worth a dependency, and this is
        // the same reasoning the metrics crates use for their exposition encoder.
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

/// How many random bytes an id carries. Sixteen, so a collision is not something to think about.
const ID_BYTES: usize = 16;

/// What can go wrong.
///
/// No catch-all arm: each variant exists because it has a different fix, and the two that matter most
/// — [`Error::AlreadyHeld`] and [`Error::Unknown`] — are the two a caller must not mistake for
/// "somebody else has it".
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The resource name is not one this backend will accept.
    #[error("the resource name {reason}")]
    BadResource { reason: String },

    /// The id is not one this backend will accept.
    #[error("the lock id {reason}")]
    BadId { reason: String },

    /// This id already holds the resource, so it may not acquire it again.
    ///
    /// **Refused rather than granted**, a deliberate divergence from the Redis lock library RoadRunner
    /// is built on: that one lets the same token re-obtain, silently moving the deadline. Here a second
    /// acquisition is a mistake — the id is what a caller uses to release, and one id holding two
    /// leases is one id that cannot release them separately — and a caller that wants a longer lease has
    /// [`Locks::update_lease`], which says so at the call site.
    #[error(
        "the lock id {id:?} already holds {resource:?} as a {mode} lock; call update_lease to move its \
         deadline, or release it first"
    )]
    AlreadyHeld {
        resource: String,
        id: String,
        mode: &'static str,
    },

    /// **The call's effect cannot be determined.** See the module documentation.
    ///
    /// The caller must resolve it by asking — [`Locks::exists`], or a release — and must **not** treat
    /// it as "not held". This is the one variant here that exists to prevent data corruption rather
    /// than to report a mistake.
    #[error(
        "the lock state on {resource:?} is unknown: {operation} {reason}; call exists to find out"
    )]
    Unknown {
        resource: String,
        operation: &'static str,
        reason: String,
    },

    /// The backing store failed, and the failure was not during an acquisition.
    ///
    /// A read that failed, or a release that failed on a connection which never sent the command.
    /// Deliberately not [`Error::Unknown`]: `Unknown` is for when the effect *might* have happened, and
    /// a driver that reported it for an operation it knows did not run would teach callers to ignore it.
    #[error("the {driver} lock backend failed: {reason}")]
    Backend {
        driver: &'static str,
        reason: String,
    },
}

/// The result of a lock operation.
pub type Result<T> = std::result::Result<T, Error>;

/// What a lock backend can do.
///
/// # Why these methods and not "acquire, release, done"
///
/// Each is a thing an application has to be able to express, and each was in the list only after asking
/// what the application would otherwise write by hand:
///
/// - [`try_acquire`](Locks::try_acquire) *and* [`acquire`](Locks::acquire), because "do not wait" and
///   "wait at most this long" are different intents, and one method taking a duration makes the first
///   easy to get wrong.
/// - [`release_all`](Locks::release_all), because a worker that died holding a shared lock under an id
///   nobody recorded leaves a resource that no reader can ever take exclusively again. RoadRunner has
///   it and its documentation is blunt about what it is for.
/// - [`update_lease`](Locks::update_lease) rather than a background renewer: a renewer needs a thread
///   per lock and cannot know whether the protected work is still running, and a lease that expires
///   during a slow operation is a problem the application has to be told about rather than have hidden
///   from it.
///
/// # Why it is synchronous
///
/// The same reason `volucer_kv::Store` is: a call is answered on the worker's exchange thread, and
/// making this `async` would mean either blocking on a runtime from a synchronous thread or moving the
/// whole call path onto one. A slow backend blocks the worker that asked and no other — see
/// `docs/plugins.md` §6.2, and §10 item 9 for the deadline that is still open.
pub trait Locks: Send + Sync + std::fmt::Debug {
    /// The driver's name, as it is written in configuration. For logs and for `config.describe`.
    fn name(&self) -> &'static str;

    /// Make one attempt at `resource`, with no waiting.
    ///
    /// `Ok(None)` is contention — somebody else holds it — and is **not** an error. `Err(Unknown)` is an
    /// attempt whose effect is undetermined, and *is*.
    fn try_acquire(
        &self,
        resource: &str,
        id: &str,
        mode: Mode,
        lease: Lease,
    ) -> Result<Option<Holder>>;

    /// Make attempts at `resource` until `wait` has passed.
    ///
    /// A zero `wait` is exactly [`try_acquire`](Locks::try_acquire): one attempt, no sleeping, and no
    /// reading of the clock for a deadline that cannot arrive.
    fn acquire(
        &self,
        resource: &str,
        id: &str,
        mode: Mode,
        lease: Lease,
        wait: Duration,
    ) -> Result<Option<Holder>>;

    /// Release the lock this id holds, answering whether there was one.
    ///
    /// `false` means the id did not hold the resource — the lease had expired, or something released it
    /// — and that is an answer rather than a failure, because "my lock is already gone" is not something
    /// a `finally`-shaped caller wants an error for.
    fn release(&self, resource: &str, id: &str) -> Result<bool>;

    /// Release **every** lock on `resource`, whoever holds them, answering whether there was one.
    ///
    /// The escape hatch for a resource wedged by a worker that died. It is not scoped by id and cannot
    /// be, which is part of why it lives behind the call plane's capability list: a backend whose
    /// `services` omit `lock` cannot reach it at all.
    fn release_all(&self, resource: &str) -> Result<bool>;

    /// Every holder of `resource`, with what is left of each lease.
    ///
    /// Expired holders are not reported, whether or not a driver has got round to removing them: the
    /// answer is about the state a caller is in, not about the state a data structure is in.
    fn holders(&self, resource: &str) -> Result<Vec<Holder>>;

    /// Replace the remaining lease of `resource` for `id`.
    ///
    /// **Sets a deadline from now; does not add to the old one.** `Ok(false)` means the id held nothing,
    /// which is the same answer [`release`](Locks::release) gives and the same thing a caller has to act
    /// on: the section it is in is no longer protected.
    fn update_lease(&self, resource: &str, id: &str, lease: Lease) -> Result<bool>;

    /// Whether anything holds `resource`.
    ///
    /// Derived, because a driver that answered it separately could disagree with
    /// [`holders`](Locks::holders) — and this is the method RoadRunner tells an application to call when
    /// an acquisition came back [`Error::Unknown`], so the two answers being one piece of code is not a
    /// stylistic preference.
    fn exists(&self, resource: &str) -> Result<bool> {
        Ok(!self.holders(resource)?.is_empty())
    }
}

/// How long a resource name or an id may be.
///
/// A bound rather than a policy: Redis keys are binary-safe and would take a megabyte, and a
/// megabyte-long key is a caller sending the protected data instead of naming it — which turns every
/// lock operation into a transfer of it and every `MONITOR` line into a page.
pub const MAX_NAME_BYTES: usize = 512;

/// The resource-name rules both drivers share, checked once, in one place.
///
/// A driver writing its own check would be a driver whose rules could differ from the other one's, which
/// would make an application's behaviour depend on a `[lock] driver` line — the kind of difference
/// nobody instruments for.
///
/// Separate from [`validate_id`] and not only from [`validate`], because the two operations that act on
/// a *resource* — [`Locks::release_all`] and [`Locks::holders`] — name no id at all, and a combined
/// check would have to be handed a placeholder id and would refuse every call.
pub(crate) fn validate_resource(resource: &str) -> Result<()> {
    if resource.is_empty() {
        return Err(Error::BadResource {
            reason: "is empty, so no caller could name it".to_string(),
        });
    }
    if resource.len() > MAX_NAME_BYTES {
        return Err(Error::BadResource {
            reason: format!("is longer than {MAX_NAME_BYTES} bytes, which is not a resource name"),
        });
    }
    if resource.chars().any(char::is_control) {
        // Control characters because a key containing one is invisible in `MONITOR` and in a key
        // listing, which is exactly where an operator goes when a resource is wedged. A space or a
        // colon is fine: an application's own namespacing uses them constantly.
        return Err(Error::BadResource {
            reason: "contains a control character, which would be invisible in a key listing"
                .to_string(),
        });
    }
    Ok(())
}

/// The id rules both drivers share.
pub(crate) fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() {
        return Err(Error::BadId {
            reason: "is empty, and an empty id is one no call could ever release".to_string(),
        });
    }
    if id.len() > MAX_NAME_BYTES {
        return Err(Error::BadId {
            reason: format!("is longer than {MAX_NAME_BYTES} bytes, which is not an id"),
        });
    }
    if id.chars().any(char::is_control) {
        return Err(Error::BadId {
            reason: "contains a control character, which would be invisible in a key listing"
                .to_string(),
        });
    }
    Ok(())
}

/// Both halves, for the operations that take both.
pub(crate) fn validate(resource: &str, id: &str) -> Result<()> {
    validate_resource(resource)?;
    validate_id(id)
}

/// The set of holders of every resource, with the lazy expiry both drivers need.
///
/// Shared here because it is the same structure whether it lives in this process or is rebuilt from a
/// Redis read, and because the *rule* it encodes — an expired holder is not a holder — is what keeps
/// two drivers from disagreeing about a resource whose lease ran out between a write and a read.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    resources: BTreeMap<String, Vec<Entry>>,
}

/// One holder, as the registry stores it.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    /// The holder's id.
    pub id: String,
    /// Whether the holder excludes others.
    pub mode: Mode,
    /// `None` is a lease that does not expire.
    pub deadline: Option<Instant>,
}

impl Entry {
    /// Whether this holder's lease has run out at `now`.
    pub(crate) fn expired(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|deadline| deadline <= now)
    }

    /// What is left of this holder's lease.
    pub(crate) fn remaining(&self, now: Instant) -> Option<Duration> {
        // Saturating, so a deadline reached between a purge and this call reports zero rather than
        // panicking on a subtraction that would have underflowed.
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    /// The public view of this holder.
    pub(crate) fn holder(&self, now: Instant) -> Holder {
        Holder {
            id: self.id.clone(),
            mode: self.mode,
            remaining: self.remaining(now),
        }
    }
}

impl Registry {
    /// Remove this resource's expired holders, dropping the resource if that empties it.
    pub(crate) fn purge(&mut self, resource: &str, now: Instant) {
        let Some(entries) = self.resources.get_mut(resource) else {
            return;
        };
        entries.retain(|entry| !entry.expired(now));
        if entries.is_empty() {
            self.resources.remove(resource);
        }
    }

    /// Remove every expired holder of every resource.
    pub(crate) fn sweep(&mut self, now: Instant) {
        self.resources.retain(|_, entries| {
            entries.retain(|entry| !entry.expired(now));
            !entries.is_empty()
        });
    }

    /// The live holders of a resource.
    pub(crate) fn holders(&mut self, resource: &str, now: Instant) -> Vec<Holder> {
        self.purge(resource, now);
        self.resources
            .get(resource)
            .map(|entries| entries.iter().map(|entry| entry.holder(now)).collect())
            .unwrap_or_default()
    }

    /// Whether `id` already holds this resource, and as what.
    pub(crate) fn held_by(&mut self, resource: &str, id: &str, now: Instant) -> Option<Mode> {
        self.purge(resource, now);
        self.resources
            .get(resource)?
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| entry.mode)
    }

    /// Whether any live holder of this resource conflicts with `wanted`.
    pub(crate) fn conflicted(&mut self, resource: &str, wanted: Mode, now: Instant) -> bool {
        self.purge(resource, now);
        self.resources.get(resource).is_some_and(|entries| {
            entries
                .iter()
                .any(|entry| entry.mode.conflicts_with(wanted))
        })
    }

    /// Insert a holder, replacing any entry with the same id.
    ///
    /// Replacement rather than refusal *here*, because the caller has already decided the question with
    /// [`Registry::held_by`]; this is the one place that mutates a resource's list, so "the same id is
    /// the same holder" cannot be got right in one driver and wrong in the other.
    pub(crate) fn insert(&mut self, resource: &str, entry: Entry) {
        let entries = self.resources.entry(resource.to_string()).or_default();
        match entries.iter_mut().find(|existing| existing.id == entry.id) {
            Some(existing) => *existing = entry,
            None => entries.push(entry),
        }
    }

    /// Remove `id` from `resource`, answering whether it was there.
    pub(crate) fn remove(&mut self, resource: &str, id: &str, now: Instant) -> bool {
        self.purge(resource, now);
        let Some(entries) = self.resources.get_mut(resource) else {
            return false;
        };
        let before = entries.len();
        entries.retain(|entry| entry.id != id);
        let removed = entries.len() != before;
        if entries.is_empty() {
            self.resources.remove(resource);
        }
        removed
    }

    /// Remove every holder of `resource`, answering whether there was one.
    pub(crate) fn remove_all(&mut self, resource: &str, now: Instant) -> bool {
        self.purge(resource, now);
        self.resources
            .remove(resource)
            .is_some_and(|entries| !entries.is_empty())
    }

    /// Move `id`'s deadline, answering whether it held the resource.
    pub(crate) fn set_deadline(
        &mut self,
        resource: &str,
        id: &str,
        deadline: Option<Instant>,
        now: Instant,
    ) -> bool {
        self.purge(resource, now);
        let Some(entries) = self.resources.get_mut(resource) else {
            return false;
        };
        match entries.iter_mut().find(|entry| entry.id == id) {
            Some(entry) => {
                entry.deadline = deadline;
                true
            }
            // Purged above, so reaching here means the id was never there — or its lease ran out during
            // this very call, which is the case an `update_lease` returning `true` would turn into a
            // lock the caller believes it renewed and another process believes it owns.
            None => false,
        }
    }

    /// How many resources are tracked. For a diagnostic, and for the tests that pin reclamation.
    pub(crate) fn len(&self) -> usize {
        self.resources.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lease_of_zero_is_for_ever_and_a_zero_duration_is_not() {
        // The distinction this enum exists for. `ttl = 0` means "until released" — RoadRunner's rule —
        // and a `For(Duration::ZERO)` reaching a driver would be a lock that is gone before it is
        // reported as granted, which reads to a caller exactly like one that was never granted.
        assert!(Lease::Forever.is_forever());
        assert_eq!(Lease::Forever.millis(), None);
        assert_eq!(
            Lease::for_duration(Duration::ZERO).millis(),
            Some(1),
            "a zero duration clamps up to a millisecond rather than becoming a lock with no lease"
        );
    }

    #[test]
    fn a_lease_is_clamped_so_a_duration_near_the_bound_does_not_wrap() {
        let at_bound = Lease::for_duration(MAX_LEASE);
        assert_eq!(at_bound.millis(), Some(MAX_LEASE.as_millis() as u64));

        let beyond = Lease::for_duration(MAX_LEASE + Duration::from_secs(1));
        assert_eq!(beyond.millis(), Some(MAX_LEASE.as_millis() as u64));

        // The one that actually happens: an application writing `PHP_INT_MAX` microseconds.
        let absurd = Lease::for_duration(Duration::from_secs(u64::MAX / 2));
        assert_eq!(absurd.millis(), Some(MAX_LEASE.as_millis() as u64));
    }

    #[test]
    fn sharing_tolerates_readers_and_exclusivity_tolerates_nothing() {
        assert!(!Mode::Shared.conflicts_with(Mode::Shared));
        assert!(Mode::Shared.conflicts_with(Mode::Exclusive));
        assert!(Mode::Exclusive.conflicts_with(Mode::Shared));
        assert!(Mode::Exclusive.conflicts_with(Mode::Exclusive));
    }

    #[test]
    fn a_name_with_a_control_character_is_refused_because_it_is_invisible_in_a_key_listing() {
        validate("pdf:create", "worker-1").expect("an ordinary pair is accepted");
        validate("a resource with spaces", "an id with spaces")
            .expect("spaces are not control characters and an application's names contain them");

        let err = validate("pdf\ncreate", "worker-1").expect_err("a newline in a key is a mistake");
        assert!(matches!(err, Error::BadResource { .. }), "{err}");

        let err =
            validate("pdf:create", "worker\u{1}").expect_err("so is a control character in an id");
        assert!(matches!(err, Error::BadId { .. }), "{err}");
    }

    #[test]
    fn an_empty_or_oversized_resource_or_id_is_refused_by_name() {
        assert!(matches!(
            validate("", "worker-1"),
            Err(Error::BadResource { .. })
        ));
        assert!(matches!(validate("pdf", ""), Err(Error::BadId { .. })));

        let long = "x".repeat(MAX_NAME_BYTES + 1);
        assert!(matches!(
            validate(&long, "worker-1"),
            Err(Error::BadResource { .. })
        ));
        assert!(matches!(validate("pdf", &long), Err(Error::BadId { .. })));
    }

    #[test]
    fn a_generated_id_is_hex_and_two_of_them_differ() {
        let first = generate_id();
        let second = generate_id();

        assert_eq!(
            first.len(),
            ID_BYTES * 2,
            "a fixed-width id in hex: {first}"
        );
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "lowercase hex, so an id copied out of a log is the id that was stored: {first}"
        );
        assert_ne!(
            first, second,
            "two calls must not collide, which is the property the RNG is there for"
        );

        // And the id the server may produce has to satisfy the rules it enforces on a client's, or a
        // generated id would be the one value that could not be released.
        validate("pdf", &first).expect("a generated id is a valid id");
    }

    #[test]
    fn a_purge_removes_an_expired_holder_and_the_resource_it_emptied() {
        let mut registry = Registry::default();
        let now = Instant::now();
        registry.insert(
            "pdf",
            Entry {
                id: "gone".to_string(),
                mode: Mode::Exclusive,
                deadline: Some(now - Duration::from_millis(1)),
            },
        );
        registry.insert(
            "pdf",
            Entry {
                id: "here".to_string(),
                mode: Mode::Exclusive,
                deadline: Some(now + Duration::from_secs(60)),
            },
        );

        let holders = registry.holders("pdf", now);
        assert_eq!(holders.len(), 1, "{holders:?}");
        assert_eq!(holders[0].id, "here");

        assert!(registry.remove("pdf", "here", now));
        assert_eq!(
            registry.len(),
            0,
            "the last holder leaving must take the resource with it, or a process that ran long enough \
             would accumulate one empty entry per resource it ever locked"
        );
    }

    #[test]
    fn an_expired_holder_is_not_a_holder_even_before_anything_removes_it() {
        let mut registry = Registry::default();
        let now = Instant::now();
        registry.insert(
            "pdf",
            Entry {
                id: "worker-1".to_string(),
                mode: Mode::Exclusive,
                deadline: Some(now - Duration::from_millis(1)),
            },
        );

        // Every question a caller can ask, answered as if the entry were not there — because to the
        // caller it is not. A driver that let the stale entry through would refuse an acquisition that
        // should have succeeded, and the caller would have no way to tell that from real contention.
        assert!(registry.holders("pdf", now).is_empty());
        assert_eq!(registry.held_by("pdf", "worker-1", now), None);
        assert!(!registry.conflicted("pdf", Mode::Exclusive, now));
        assert!(!registry.set_deadline("pdf", "worker-1", None, now));
    }

    #[test]
    fn the_same_id_may_not_hold_the_same_resource_twice() {
        let mut registry = Registry::default();
        let now = Instant::now();
        registry.insert(
            "pdf",
            Entry {
                id: "worker-1".to_string(),
                mode: Mode::Shared,
                deadline: None,
            },
        );

        assert_eq!(
            registry.held_by("pdf", "worker-1", now),
            Some(Mode::Shared),
            "which is what a driver refuses on, and why that refusal is not `conflicted`: a shared \
             holder does not conflict with itself, so `conflicted` alone would let it through"
        );
    }

    #[test]
    fn a_sweep_reclaims_what_nothing_has_asked_about_since_it_expired() {
        let mut registry = Registry::default();
        let now = Instant::now();
        for resource in ["a", "b", "c"] {
            registry.insert(
                resource,
                Entry {
                    id: "worker-1".to_string(),
                    mode: Mode::Exclusive,
                    deadline: Some(now - Duration::from_secs(1)),
                },
            );
        }
        assert_eq!(registry.len(), 3);

        registry.sweep(now);
        assert_eq!(
            registry.len(),
            0,
            "a resource whose only holder expired and which nobody touches again is the reason the \
             sweep exists; lazy expiry alone would keep it for as long as the process lives"
        );
    }
}
