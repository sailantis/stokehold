//! Key-value storage: the store trait, and the in-memory driver.
//!
//! # What this is, and what it is not
//!
//! This is the server's side of a cache. It is **not** a PSR-16 implementation and does not try to
//! be one: PSR-16 is a *client* interface, and the half of it that matters — the serializer — lives
//! on the client by design. A value here is a `Vec<u8>` and nothing else is ever assumed about it.
//!
//! That is not fastidiousness. RoadRunner's KV is reached through a serialized PHP value, which
//! means the server holds the codec and therefore sees the plaintext of everything an application
//! stores. An application that wants to encrypt what it caches cannot, because the encryption would
//! have to happen below the serializer and the serializer is on the server. So the wire carries
//! **opaque bytes**, `docs/plugins.md` §6.2 says so, and this crate takes it seriously: there is no
//! `serde` anywhere in it, and [`Store::get`] cannot fail because a stored value was unreadable.
//!
//! # Keys are bytes, not `String`
//!
//! A key is `&[u8]`. PSR-16 restricts the character set, but that restriction is the *client's* to
//! enforce — and a server that took `&str` would reject an application that namespaces keys with
//! something exotic, for no reason a caller could act on.
//!
//! # The trait is synchronous, and that is the decision that shapes every driver
//!
//! A call arrives on a worker's exchange thread and is answered there. Making the store `async`
//! would mean either blocking on a runtime from a synchronous thread — which is how a single slow
//! Redis becomes a stalled worker pool — or moving the whole call path onto a runtime, which
//! `docs/plugins.md` §6.2 refuses because the exchange thread exists precisely to *not* be a
//! runtime task. So a slow backend blocks the worker that asked, and that is honest: the worker is
//! the thing waiting for its own answer, and no other worker is affected.
//!
//! The consequence to keep in view is the per-call deadline, still open as §10 item 9.
//!
//! # Errors are values, and "the store is full" is not an error to hide
//!
//! [`Error`] has three variants and no `Other(String)` catch-all, so a caller has to think about
//! each. The one worth arguing for is [`Error::Full`]: an unbounded in-memory map is how a cache
//! becomes an out-of-memory kill, and the two ways to bound it are to evict and to refuse. This
//! refuses and says so, because a refusal is visible in a log and a counter whereas an eviction is
//! a cache miss that looks like the application's bug.

pub mod memory;

use std::time::Duration;

pub use memory::MemoryStore;

/// What a key-value store can do.
///
/// Deliberately small. `docs/plugins.md` §6.2 lists the PSR-16 method set as what the *client*
/// exposes; three of those (`getMultiple`, `setMultiple`, `deleteMultiple`) are batches, and a
/// batch is not three more verbs — it is the same verb with more keys, which is why there is one
/// overridable method per direction rather than three more required ones.
pub trait Store: Send + Sync + std::fmt::Debug {
    /// The driver's name, as it is written in configuration. For logs and for `config.describe`.
    fn name(&self) -> &'static str;

    /// Fetch a value, if it is there and has not expired.
    ///
    /// Expiry is decided here rather than by a background thread alone, so a caller never sees a
    /// value that expired a moment ago. See [`MemoryStore`] for why a sweeper also exists and why
    /// it is not sufficient on its own.
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// Store a value.
    ///
    /// **`Some(Duration::ZERO)` deletes the key**, which is PSR-16's rule — "a negative or zero TTL
    /// MUST cause the item to be deleted" — and it is enforced here rather than left as the
    /// caller's job, because a caller that passed zero expecting an immediate expiry and got an
    /// immortal key would have a cache that grows for ever from one line of application code.
    ///
    /// `None` means no expiry at all.
    fn set(&self, key: &[u8], value: Vec<u8>, ttl: Option<Duration>) -> Result<()>;

    /// Remove a key, answering whether it was there.
    ///
    /// Worth returning: PSR-16's `delete` returns it, and "deleted" versus "was not there" is the
    /// difference between a cache and a lookup table for an application counting its own misses.
    fn delete(&self, key: &[u8]) -> Result<bool>;

    /// How long a key has left, or why it has none.
    ///
    /// A three-state answer rather than `Option<Duration>`, because two different states have no
    /// remaining time — absent, and present forever — and PSR-16's `getTtl` has to tell them apart.
    fn ttl(&self, key: &[u8]) -> Result<Ttl>;

    /// Remove every key **in this storage**.
    ///
    /// `clear` and not `flush`: a deployment with two storages must not have one call empty both.
    /// See [`MemoryStore`] for how the memory driver makes that true of a process-wide map.
    fn clear(&self) -> Result<()>;

    /// Fetch several keys at once.
    ///
    /// A default that loops, so a driver with no batch primitive does not have to write one. It is
    /// always callable and it is overridable, which is the right shape: on Redis the loop is one
    /// round trip per key and worth replacing, and in memory a loop over a map *is* the batch.
    ///
    /// The answer is positional: `out[i]` describes `keys[i]`. A missing key is `None` in its own
    /// slot rather than absent from the vector, because pairing keys with values by position is the
    /// only thing a batch is for.
    fn get_many(&self, keys: &[&[u8]]) -> Result<Vec<Option<Vec<u8>>>> {
        keys.iter().map(|key| self.get(key)).collect()
    }

    /// Store several values at once, all with the same TTL.
    ///
    /// One TTL for the batch rather than one each, because that is what PSR-16's `setMultiple`
    /// takes and because the alternative is a second vector whose length must agree with the first.
    fn set_many(&self, keys: &[(&[u8], Vec<u8>)], ttl: Option<Duration>) -> Result<()> {
        for (key, value) in keys {
            self.set(key, value.clone(), ttl)?;
        }
        Ok(())
    }
}

/// How long a key has left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ttl {
    /// There is no such key, which is not the same as a key that never expires.
    Missing,
    /// The key is there and will not expire on its own.
    Forever,
    /// The key is there and has this much left.
    In(Duration),
}

impl Ttl {
    /// Whether there is a value under this key.
    ///
    /// The question `has` asks, and the reason it is here rather than a `has` method on the trait:
    /// "is this there" and "how long has it got" are one lookup with two answers, and a trait method
    /// per phrasing would make every driver implement the same match twice.
    pub fn is_present(self) -> bool {
        !matches!(self, Ttl::Missing)
    }
}

/// What can go wrong.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The store is at its bound and will not take another key.
    ///
    /// Carries the bound, because an operator seeing this needs to know whether to raise a limit or
    /// to move to a driver with a disk behind it, and "the cache is full" without a number answers
    /// neither.
    #[error(
        "the in-memory store is full at {max} entries; raise `max_entries` or use a driver with a \
         disk behind it"
    )]
    Full { max: usize },

    /// The key is not one this store will accept.
    ///
    /// A separate variant because it is the *caller's* mistake and therefore actionable, where a
    /// backend failure is not.
    #[error("the key {reason}")]
    BadKey { reason: String },

    /// The backing store failed. Never produced by the memory driver.
    #[error("the {driver} store failed: {reason}")]
    Store {
        driver: &'static str,
        reason: String,
    },
}

/// The result of a store operation.
pub type Result<T> = std::result::Result<T, Error>;

/// The longest a TTL may be.
///
/// Ten years, and a bound rather than a policy: what it prevents is `Duration::MAX` — which an
/// application reaches by writing `PHP_INT_MAX` seconds — becoming a negative number of
/// milliseconds, or a saturating add, in a driver that stores a deadline as an integer. A key that
/// genuinely needs to outlive the process's own uptime is a key that does not need a TTL.
pub const MAX_TTL: Duration = Duration::from_secs(10 * 365 * 24 * 60 * 60);

/// Clamp a TTL into the range a driver can represent.
///
/// Shared here rather than written per driver, because the off-by-one it prevents is the same one
/// every time: a TTL of exactly [`MAX_TTL`] must survive unchanged, and a value above it must be
/// clamped rather than wrapped.
pub fn clamp_ttl(ttl: Option<Duration>) -> Option<Duration> {
    ttl.map(|ttl| ttl.min(MAX_TTL))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ttl_exactly_at_the_bound_survives_and_one_beyond_it_is_clamped() {
        assert_eq!(clamp_ttl(Some(MAX_TTL)), Some(MAX_TTL));
        assert_eq!(
            clamp_ttl(Some(MAX_TTL + Duration::from_secs(1))),
            Some(MAX_TTL),
            "a TTL beyond the bound must clamp rather than wrap, which on a driver storing \
             milliseconds as an integer would be a deadline in the past"
        );
        // The one that actually happens: an application writing `PHP_INT_MAX` seconds.
        assert_eq!(
            clamp_ttl(Some(Duration::from_secs(u64::MAX / 2))),
            Some(MAX_TTL)
        );
        assert_eq!(clamp_ttl(None), None, "no expiry stays no expiry");
    }

    #[test]
    fn missing_and_forever_are_different_answers() {
        // The distinction PSR-16's `getTtl` cannot express as an `Option`: two states have no
        // remaining time, and only one of them has a value behind it.
        assert!(!Ttl::Missing.is_present());
        assert!(Ttl::Forever.is_present());
        assert!(Ttl::In(Duration::from_secs(1)).is_present());
    }
}
