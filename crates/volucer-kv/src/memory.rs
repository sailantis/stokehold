//! The in-memory store: sharded, bounded by bytes, with expiry decided on read and swept on full.
//!
//! # Why a store at all, when PHP already has an array
//!
//! Because the array is per *worker process*, and a worker pool is a set of independent processes.
//! Two requests that land on different workers share nothing, so an application cache in a PHP
//! static is a cache with a hit rate of `1 / workers` and no way to invalidate. This makes one
//! process's memory addressable from all of them, over the channel that is already open.
//!
//! That is also what `docs/plugins.md` §6.2 means by "useful on its own for single-node caching":
//! it needs no Redis, no new listener and no new dependency.
//!
//! # Sharded, because the callers are concurrent
//!
//! A single `Mutex<HashMap>` would serialise every KV call across every worker, which on a
//! deployment whose whole point is that requests do not queue is a strange thing to introduce. So
//! the map is split into [`SHARDS`] independently lockable pieces, chosen by hashing the key. Two
//! workers touching different keys almost never meet.
//!
//! The shard count is fixed rather than derived from the CPU count, and that is deliberate: a
//! `Vec<Mutex<_>>` sized by a *runtime* value cannot be indexed without a bounds check that the
//! compiler cannot elide, and a constant makes the index a mask. Sixteen locks is more than enough
//! to remove the contention that matters and small enough that the whole thing fits in cache lines.
//!
//! # Bounded by **bytes**, not by entry count
//!
//! An entry count is the obvious bound and the wrong one: a hundred thousand entries of one
//! kilobyte is a hundred megabytes, and a hundred thousand of a megabyte is a hundred gigabytes that
//! the kernel will take out of the process. So the accounting is the sum of the stored values'
//! lengths, and what it buys is that the number in the configuration is a number an operator can
//! reason about.
//!
//! **What the accounting does not include**, and the reason the default is not larger than it is:
//! the map's own overhead (a `Vec` key, its allocation, and the hash table's load factor) is
//! roughly two to three times the bytes counted here. So `max_bytes = 64 MiB` is a process that may
//! hold some 200 MiB of heap. That is a real multiplier and hiding it would make the setting a lie;
//! it is also why the figure is reported as a metric rather than only enforced.
//!
//! # Expiry: on read, and swept when full
//!
//! Two mechanisms, and neither alone is enough:
//!
//! 1. **On read**, which is exact and free: the caller's own lookup already has to find the entry,
//!    so comparing a deadline costs nothing, and a key that expired is removed as it is found. This
//!    is what makes "a caller never sees an expired value" true.
//! 2. **Swept when full**, which bounds the memory of keys nobody reads again. A key written with a
//!    TTL and never touched would otherwise hold its bytes for ever, and a cache whose memory is
//!    held by keys nobody asks for is the failure this whole file is about.
//!
//! The sweeper runs **because an insert was about to fail**, never on a timer. That is the whole
//! trick: the expensive operation happens exactly at the moment when the alternative is an error,
//! and a store under its bound never sweeps at all. It is rate-limited by [`SWEEP_INTERVAL`]
//! because the alternative — sweep on every refused insert — is quadratic in the pathological case
//! where the store is legitimately full of live entries.
//!
//! There is deliberately **no background thread**. A sweeper thread needs a shutdown handle, a
//! wake-up strategy and a place in the lifecycle, all to do work that the failing insert can do at
//! the moment it matters. §5's service bag would own it if there were one.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::{clamp_ttl, Error, Result, Store, Ttl};

/// How many independently lockable pieces the key space is split into.
///
/// A power of two so the shard index is a mask rather than a modulo, which matters because it is on
/// the path of every single call.
pub const SHARDS: usize = 16;

/// The longest a sweep may be deferred between two refusals.
///
/// A second. Long enough that a store that is genuinely full of live entries does not spend its time
/// walking itself, and short enough that a store full of dead ones recovers before an operator has
/// finished reading the log line about it.
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// How many bytes a store holds before it refuses, unless configured otherwise.
///
/// 64 MiB of counted values, which is the number in the configuration and *not* the process's heap;
/// see the module documentation for the multiplier. Chosen so that a deployment which never thinks
/// about this setting notices before the OOM killer does: at this size a leak is visible in `ps` long
/// before it is fatal, and an application caching more than this is one that should be on a driver
/// with a disk behind it.
///
/// **`volucer-core` writes this number a second time** as its own default for a `[kv.*]` table that
/// omits `max_bytes`, because a leaf crate that everything depends on cannot take a dependency on a
/// driver to name a default. Both copies are pinned by a test — `the_kv_default_bound_is_pinned_…`
/// there and `the_default_bound_is_the_one_the_configuration_writes` here — so a change to either
/// fails a test rather than making an omitted key mean something different from a named one.
pub const DEFAULT_MAX_BYTES: usize = 64 * 1024 * 1024;

/// One stored value and its deadline.
#[derive(Debug)]
struct Entry {
    value: Vec<u8>,
    /// `None` means "no expiry", which is not the same as a deadline in the past.
    ///
    /// An `Option<Instant>` rather than a sentinel `Instant::MAX`: `Instant::now() + MAX_TTL` can
    /// overflow on platforms whose monotonic clock is 32-bit, and `checked_add` returning `None` for
    /// "no expiry" would then be indistinguishable from an overflow that should have been clamped.
    expires_at: Option<Instant>,
}

impl Entry {
    /// The bytes this entry accounts for.
    ///
    /// The value's length and nothing else — see the module documentation on what the accounting
    /// deliberately leaves out.
    fn bytes(&self) -> usize {
        self.value.len()
    }

    fn is_expired(&self, now: Instant) -> bool {
        self.expires_at.is_some_and(|at| now >= at)
    }
}

#[derive(Debug, Default)]
struct Shard {
    map: HashMap<Vec<u8>, Entry>,
}

/// A sharded, byte-bounded, expiring key-value store held in this process's memory.
#[derive(Debug)]
pub struct MemoryStore {
    shards: Box<[Mutex<Shard>]>,
    /// The bound, in counted value bytes.
    max_bytes: usize,
    /// The sum of every live entry's [`Entry::bytes`].
    ///
    /// An `AtomicUsize` updated **while the shard lock is held**, so the total is consistent with the
    /// shards taken together even though two shards can be written at once. What it is *not* is a
    /// reservation: two threads can each read the total, each find room for their own value, and
    /// together cross the bound by up to the sum of what they are storing. The bound is a safety net
    /// for a cache rather than an allocator, so an overshoot of a few kilobytes is the right trade
    /// against a CAS loop on the hot path — but it is a real property and stating it here is what
    /// stops somebody later finding it and calling it a bug.
    used_bytes: AtomicUsize,
    /// How many entries are live. Not a bound, but the number a metric wants: a store whose bytes are
    /// high and entries are low is holding large values, and one where both are high is holding many.
    entries: AtomicUsize,
    /// When a sweep last ran, so a run of refused inserts does not each walk the whole store.
    last_sweep: Mutex<Instant>,
}

impl Default for MemoryStore {
    fn default() -> Self {
        MemoryStore::with_max_bytes(DEFAULT_MAX_BYTES)
    }
}

impl MemoryStore {
    /// A store bounded at `max_bytes` of counted value bytes.
    pub fn with_max_bytes(max_bytes: usize) -> Self {
        let shards = (0..SHARDS)
            .map(|_| Mutex::new(Shard::default()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        MemoryStore {
            shards,
            max_bytes,
            used_bytes: AtomicUsize::new(0),
            entries: AtomicUsize::new(0),
            // In the past, so the first insert that finds the store full is allowed to sweep. A
            // store that started out unable to reclaim would refuse for its first second of life.
            last_sweep: Mutex::new(Instant::now() - SWEEP_INTERVAL),
        }
    }

    /// The bound, in bytes.
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// The bytes currently accounted for.
    pub fn used_bytes(&self) -> usize {
        self.used_bytes.load(Ordering::Relaxed)
    }

    /// How many entries are live, including any that have expired but not yet been found.
    ///
    /// "Including" is not a defect: an expired entry still holds its bytes until something looks at
    /// it, and a count that hid them would disagree with [`MemoryStore::used_bytes`], which is the
    /// number the bound is enforced against.
    pub fn len(&self) -> usize {
        self.entries.load(Ordering::Relaxed)
    }

    /// Whether the store holds nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The shard a key belongs to, chosen by a hash of the key's bytes.
    fn shard_of(&self, key: &[u8]) -> &Mutex<Shard> {
        // The `DefaultHasher` from `std::collections` rather than a hand-rolled FNV: this hash decides
        // only *which lock* is taken, so an adversary who can pick keys colliding into one shard can
        // make the store slower and not make it wrong — and SipHash is already here with no
        // dependency of its own. A hash with a worse distribution would be a worse decision for no
        // gain.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        let index = (hasher.finish() as usize) & (SHARDS - 1);
        &self.shards[index]
    }

    /// Make room for `needed` more bytes, sweeping if that is what it takes.
    ///
    /// Answers whether there is room *now*. A `false` is the caller's cue to return [`Error::Full`],
    /// and it is returned on the fast path — under the bound, or too soon after the last sweep —
    /// without touching any shard.
    fn make_room(&self, needed: usize) -> bool {
        if self.used_bytes() + needed <= self.max_bytes {
            return true;
        }

        let now = Instant::now();
        {
            let mut last = self
                .last_sweep
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if now.duration_since(*last) < SWEEP_INTERVAL {
                // Sweeping again would walk the store to find the same nothing. Refusing is the
                // honest answer, and the one the operator's log line is for.
                return false;
            }
            *last = now;
        }

        // The lock is released before the sweep, deliberately: the sweep takes every shard's lock in
        // turn, and holding `last_sweep` across it would make the *next* refused insert wait for a
        // sweep it is about to be told not to do.
        let before = self.used_bytes();
        self.sweep_expired(now);
        let after = self.used_bytes();
        tracing::debug!(
            driver = "memory",
            freed_bytes = before.saturating_sub(after),
            "the memory store was full, so expired entries were swept"
        );
        after + needed <= self.max_bytes
    }

    /// Remove every expired entry, in every shard.
    ///
    /// Takes each shard's lock in turn, and accounts for what it removes: the byte total is what the
    /// bound is enforced against, so a sweep that freed entries without subtracting their bytes would
    /// leave the store permanently unable to store anything — full for ever, of nothing.
    ///
    /// The arithmetic is done from the entries the sweep actually removed rather than from a count or
    /// a running total, so it cannot drift: `retain` gives no handle on what it dropped, so the bytes
    /// are summed on the way out instead.
    fn sweep_expired(&self, now: Instant) {
        let mut freed_bytes = 0usize;
        let mut freed_entries = 0usize;

        for shard in self.shards.iter() {
            let mut shard = shard
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut freed_here = 0usize;
            let mut removed_here = 0usize;
            shard.map.retain(|_, entry| {
                if entry.is_expired(now) {
                    freed_here += entry.bytes();
                    removed_here += 1;
                    false
                } else {
                    true
                }
            });
            freed_bytes += freed_here;
            freed_entries += removed_here;
        }

        // Outside the loop, so the atomics are touched once per sweep rather than once per shard: two
        // workers sweeping concurrently would otherwise interleave their subtractions with their own
        // reads and the total could go below what is actually held.
        if freed_entries > 0 {
            self.used_bytes.fetch_sub(freed_bytes, Ordering::Relaxed);
            self.entries.fetch_sub(freed_entries, Ordering::Relaxed);
        }
    }

    /// Run `f` with the lock held on the shard `key` belongs to.
    fn with_shard<T>(&self, key: &[u8], f: impl FnOnce(&mut Shard) -> T) -> T {
        let mut shard = self.shard_of(key).lock().unwrap_or_else(|poisoned| {
            // A poisoned shard means a panic happened while its lock was held. The memory driver's own
            // code cannot panic while holding one — every operation is a hash-map call — so this is a
            // panic in a *caller's* closure, and the map is still a valid map. Recovering is right:
            // refusing every future operation on this shard would turn one application bug into a
            // process that can no longer cache.
            poisoned.into_inner()
        });
        f(&mut shard)
    }
}

impl Store for MemoryStore {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let now = Instant::now();
        self.with_shard(key, |shard| {
            let Some(entry) = shard.map.get(key) else {
                return Ok(None);
            };
            if entry.is_expired(now) {
                // Removed here rather than by the sweeper, because the lookup has already found it:
                // this is the mechanism that makes "a caller never sees an expired value" true, and
                // it costs one comparison on a path that was going to happen anyway.
                if let Some(entry) = shard.map.remove(key) {
                    self.release(entry.bytes());
                }
                return Ok(None);
            }
            Ok(Some(entry.value.clone()))
        })
    }

    fn set(&self, key: &[u8], value: Vec<u8>, ttl: Option<Duration>) -> Result<()> {
        // PSR-16's rule, enforced here rather than trusted to the client: a zero or negative TTL means
        // "delete". See the trait's documentation for why this is not the caller's job.
        if ttl.is_some_and(|ttl| ttl.is_zero()) {
            self.delete(key)?;
            return Ok(());
        }

        let ttl = clamp_ttl(ttl);
        let expires_at = ttl.map(|ttl| Instant::now() + ttl);
        let needed = value.len();

        // The bound is checked *before* the shard lock, so an insert that cannot fit does not hold a
        // shard while it decides. `make_room` may sweep, which takes the locks itself — taking the
        // target shard's first and then sweeping would be the deadlock this ordering avoids.
        if !self.make_room(needed) {
            return Err(Error::Full {
                max: self.max_bytes,
            });
        }

        self.with_shard(key, |shard| {
            let replaced = shard.map.insert(key.to_vec(), Entry { value, expires_at });
            // The accounting is adjusted by the *difference* rather than by adding the new value, so
            // an overwrite does not double-count. This is the one line whose absence would make the
            // store refuse inserts it has room for, and it is why a test asserts that overwriting the
            // same key many times does not move the byte total.
            match replaced {
                Some(old) => {
                    let old_bytes = old.bytes();
                    self.used_bytes.fetch_sub(old_bytes, Ordering::Relaxed);
                    self.used_bytes.fetch_add(needed, Ordering::Relaxed);
                }
                None => {
                    self.used_bytes.fetch_add(needed, Ordering::Relaxed);
                    self.entries.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(())
        })
    }

    fn delete(&self, key: &[u8]) -> Result<bool> {
        self.with_shard(key, |shard| match shard.map.remove(key) {
            Some(entry) => {
                self.release(entry.bytes());
                Ok(true)
            }
            None => Ok(false),
        })
    }

    fn ttl(&self, key: &[u8]) -> Result<Ttl> {
        let now = Instant::now();
        self.with_shard(key, |shard| match shard.map.get(key) {
            None => Ok(Ttl::Missing),
            Some(entry) if entry.is_expired(now) => {
                if let Some(entry) = shard.map.remove(key) {
                    self.release(entry.bytes());
                }
                Ok(Ttl::Missing)
            }
            Some(entry) => Ok(match entry.expires_at {
                None => Ttl::Forever,
                Some(at) => Ttl::In(at.saturating_duration_since(now)),
            }),
        })
    }

    fn clear(&self) -> Result<()> {
        // Every shard of *this* store, and no other: the map belongs to this value, so a deployment
        // with two `[kv.*]` tables cannot have one call empty both. That is what makes `clear` the
        // right name — a `flush` would be a promise this cannot keep if a driver ever shared storage.
        for shard in self.shards.iter() {
            let mut shard = shard
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            shard.map.clear();
        }
        // Zeroed rather than decremented per entry: the shards are all empty, so the exact answer is
        // known and arithmetic that has to be applied N times is arithmetic that can drift.
        self.used_bytes.store(0, Ordering::Relaxed);
        self.entries.store(0, Ordering::Relaxed);
        Ok(())
    }

    fn get_many(&self, keys: &[&[u8]]) -> Result<Vec<Option<Vec<u8>>>> {
        // The default implementation — a loop — is kept. Overriding it would only add a way for the
        // batch and the single-key path to disagree, and in memory the loop *is* the batch.
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            out.push(self.get(key)?);
        }
        Ok(out)
    }
}

impl MemoryStore {
    /// Account for bytes and an entry leaving the store.
    ///
    /// One place, called from the three paths that remove something — [`Store::delete`], the lazy
    /// expiry in [`Store::get`] and the lazy expiry in [`Store::ttl`] — so that a fourth cannot be
    /// added without meeting it.
    fn release(&self, bytes: usize) {
        self.used_bytes.fetch_sub(bytes, Ordering::Relaxed);
        self.entries.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> MemoryStore {
        MemoryStore::with_max_bytes(DEFAULT_MAX_BYTES)
    }

    #[test]
    fn the_default_bound_is_the_one_the_configuration_writes() {
        // `volucer-core` writes this number a second time as its own default for a `[kv.*]` table
        // that omits `max_bytes`, because it is the leaf crate and cannot depend on a driver to
        // name a default. Both copies are pinned, here and in `config.rs`, so editing one fails a
        // test instead of making an omitted key mean a different bound from a named one.
        assert_eq!(DEFAULT_MAX_BYTES, 64 * 1024 * 1024);
        assert_eq!(
            MemoryStore::default().max_bytes(),
            DEFAULT_MAX_BYTES,
            "a store built with no bound must get the configured default, or every driver that \
             constructs one differently would need a test of its own"
        );
    }

    #[test]
    fn a_value_comes_back_exactly_as_it_went_in() {
        let store = store();
        // Bytes rather than text, because the wire carries opaque bytes and a store that assumed
        // UTF-8 would corrupt exactly the case the design exists for: an application that encrypts
        // what it caches.
        let value = vec![0x00, 0xff, 0xfe, b'\n', 0x80];
        store.set(b"key", value.clone(), None).unwrap();
        assert_eq!(store.get(b"key").unwrap(), Some(value));
    }

    #[test]
    fn a_missing_key_is_none_and_not_an_error() {
        let store = store();
        assert_eq!(store.get(b"absent").unwrap(), None);
        assert!(!store.delete(b"absent").unwrap(), "and nothing was there");
        assert_eq!(store.ttl(b"absent").unwrap(), Ttl::Missing);
    }

    #[test]
    fn a_zero_ttl_deletes_the_key_as_psr_16_requires() {
        let store = store();
        store.set(b"key", b"value".to_vec(), None).unwrap();
        store
            .set(b"key", b"ignored".to_vec(), Some(Duration::ZERO))
            .unwrap();

        assert_eq!(
            store.get(b"key").unwrap(),
            None,
            "a zero TTL must delete rather than store with an immediate expiry: an application \
             passing 0 to mean \"do not keep this\" must not get an immortal key"
        );
        assert_eq!(store.len(), 0, "and the entry must be accounted for");
        assert_eq!(store.used_bytes(), 0);
    }

    #[test]
    fn an_expired_key_is_gone_on_the_first_read_and_its_bytes_are_back() {
        let store = store();
        store
            .set(b"key", vec![0u8; 100], Some(Duration::from_millis(1)))
            .unwrap();
        assert_eq!(store.used_bytes(), 100);

        std::thread::sleep(Duration::from_millis(20));

        assert_eq!(store.get(b"key").unwrap(), None, "and not the old value");
        assert_eq!(
            store.used_bytes(),
            0,
            "the bytes must come back when the entry is found expired, or the store shrinks for \
             ever and eventually refuses inserts it has room for"
        );
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn ttl_distinguishes_absent_from_never_expiring() {
        let store = store();
        assert_eq!(store.ttl(b"absent").unwrap(), Ttl::Missing);

        store.set(b"forever", b"v".to_vec(), None).unwrap();
        assert_eq!(store.ttl(b"forever").unwrap(), Ttl::Forever);

        store
            .set(b"soon", b"v".to_vec(), Some(Duration::from_secs(60)))
            .unwrap();
        match store.ttl(b"soon").unwrap() {
            Ttl::In(left) => assert!(
                left <= Duration::from_secs(60) && left > Duration::from_secs(30),
                "the remaining time should be most of the minute that was set: {left:?}"
            ),
            other => panic!("expected a countable TTL, got {other:?}"),
        }
    }

    #[test]
    fn deleting_reports_whether_anything_was_there() {
        let store = store();
        store.set(b"key", b"v".to_vec(), None).unwrap();
        assert!(store.delete(b"key").unwrap(), "it was there");
        assert!(!store.delete(b"key").unwrap(), "and now it is not");
    }

    #[test]
    fn clear_empties_the_store_and_its_accounting() {
        let store = store();
        for index in 0..100u32 {
            store
                .set(format!("key{index}").as_bytes(), vec![0u8; 10], None)
                .unwrap();
        }
        assert_eq!(store.len(), 100);
        assert_eq!(store.used_bytes(), 1000);

        store.clear().unwrap();
        assert!(store.is_empty());
        assert_eq!(store.used_bytes(), 0);
        assert_eq!(store.get(b"key0").unwrap(), None);
    }

    #[test]
    fn a_batch_is_positional_so_a_missing_key_keeps_its_slot() {
        let store = store();
        store.set(b"a", b"1".to_vec(), None).unwrap();
        store.set(b"c", b"3".to_vec(), None).unwrap();

        let got = store
            .get_many(&[b"a".as_slice(), b"b".as_slice(), b"c".as_slice()])
            .unwrap();
        assert_eq!(
            got,
            vec![Some(b"1".to_vec()), None, Some(b"3".to_vec())],
            "the absent key is a hole in its own position, not a shorter vector: a caller pairing \
             keys with values by index is the only thing a batch is for"
        );

        store
            .set_many(
                &[
                    (b"x".to_vec(), b"24".to_vec()),
                    (b"y".to_vec(), b"25".to_vec()),
                ],
                None,
            )
            .unwrap();
        assert_eq!(store.get(b"x").unwrap(), Some(b"24".to_vec()));
        assert_eq!(store.get(b"y").unwrap(), Some(b"25".to_vec()));
    }

    #[test]
    fn overwriting_a_key_does_not_grow_the_accounting() {
        // The single line whose absence would make the store refuse inserts it has room for, and the
        // failure would look like a leak rather than like an arithmetic bug.
        let store = store();
        for _ in 0..1000 {
            store.set(b"key", vec![0u8; 100], None).unwrap();
        }
        assert_eq!(store.used_bytes(), 100);
        assert_eq!(store.len(), 1);

        // And shrinking it gives the difference back rather than nothing.
        store.set(b"key", vec![0u8; 10], None).unwrap();
        assert_eq!(store.used_bytes(), 10);
    }

    #[test]
    fn a_store_at_its_bound_refuses_rather_than_growing_without_end() {
        let store = MemoryStore::with_max_bytes(1000);
        store.set(b"a", vec![0u8; 600], None).unwrap();
        store.set(b"b", vec![0u8; 400], None).unwrap();
        assert_eq!(store.used_bytes(), 1000);

        match store.set(b"c", vec![0u8; 1], None) {
            Err(Error::Full { max }) => assert_eq!(
                max, 1000,
                "the refusal must carry the bound, so an operator knows what to raise"
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }

        // And what is already stored is untouched: refusing to add is not a licence to evict.
        assert_eq!(store.get(b"a").unwrap(), Some(vec![0u8; 600]));
        assert_eq!(store.get(b"b").unwrap(), Some(vec![0u8; 400]));
    }

    #[test]
    fn a_full_store_sweeps_what_nothing_has_read_and_then_has_room() {
        // The sweeper's whole reason for existing: a key written with a TTL and never read again would
        // hold its bytes for ever, and this is the only path that reclaims them. Nothing below reads a
        // key — that is the point, since a read would do the reclamation itself and the test would then
        // pass with no sweeper at all.
        //
        // Two stores rather than one, because the two halves need different clocks and asserting both
        // against one store couples them: a 1 ms TTL on Windows can be expired before the next call,
        // so "nothing has expired yet" is not a fact a test can rely on from the writing side.
        let sweepable = MemoryStore::with_max_bytes(1000);
        for index in 0..10u32 {
            sweepable
                .set(
                    format!("key{index}").as_bytes(),
                    vec![0u8; 100],
                    Some(Duration::from_millis(1)),
                )
                .unwrap();
        }
        assert_eq!(sweepable.used_bytes(), 1000);

        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            sweepable.used_bytes(),
            1000,
            "the bytes are still accounted for: nothing has looked at the map"
        );
        assert!(
            sweepable.make_room(1),
            "the sweep is the only thing that can reclaim these, and it must"
        );
        assert_eq!(sweepable.used_bytes(), 0, "and the accounting must follow");
        assert_eq!(sweepable.len(), 0);
    }

    #[test]
    fn a_full_store_with_nothing_expired_refuses_rather_than_sweeping_for_ever() {
        let store = MemoryStore::with_max_bytes(1000);
        for index in 0..10u32 {
            store
                .set(
                    format!("key{index}").as_bytes(),
                    vec![0u8; 100],
                    // An hour, so the entries are certainly live when the assertion runs.
                    Some(Duration::from_secs(3600)),
                )
                .unwrap();
        }

        assert!(
            !store.make_room(1),
            "a sweep was allowed to run and found nothing to reclaim, so there is no room — and \
             saying so is the whole point, because the alternative is a store that refuses inserts \
             it has room for or one that evicts live data"
        );
        assert_eq!(store.used_bytes(), 1000, "and nothing was taken");
    }

    #[test]
    fn a_sweep_does_not_reclaim_a_key_that_has_not_expired() {
        // The other direction, and the one that matters more: a sweep that dropped live entries would
        // be a cache that loses data whenever it fills, which is worse than one that refuses inserts.
        let store = MemoryStore::with_max_bytes(1000);
        for index in 0..5u32 {
            store
                .set(
                    format!("live{index}").as_bytes(),
                    vec![0u8; 100],
                    // Absurd, but it is a deadline and not "no expiry", which is the case being tested.
                    Some(Duration::from_secs(3600)),
                )
                .unwrap();
        }
        for index in 0..5u32 {
            store
                .set(
                    format!("dead{index}").as_bytes(),
                    vec![0u8; 100],
                    Some(Duration::from_millis(1)),
                )
                .unwrap();
        }
        std::thread::sleep(Duration::from_millis(20));

        assert!(store.make_room(1), "the dead half is reclaimable");
        assert_eq!(store.used_bytes(), 500, "and only the dead half went");
        assert_eq!(store.len(), 5);
        for index in 0..5u32 {
            assert_eq!(
                store.get(format!("live{index}").as_bytes()).unwrap(),
                Some(vec![0u8; 100]),
                "a live entry must survive a sweep"
            );
        }
    }

    #[test]
    fn a_sweep_is_rate_limited_so_a_full_store_does_not_walk_itself_on_every_insert() {
        let store = MemoryStore::with_max_bytes(100);
        store.set(b"live", vec![0u8; 100], None).unwrap();

        // The first refusal may sweep, and finds only live entries.
        assert!(!store.make_room(1));
        // The second must not sweep again, which is observable because the sweep would have taken
        // `last_sweep` forward and a store that swept anyway would be doing O(n) work per refused
        // insert. Asserted through the interval rather than through timing, which would be a flaky
        // test on a loaded machine.
        let first = *store.last_sweep.lock().unwrap();
        assert!(!store.make_room(1));
        let second = *store.last_sweep.lock().unwrap();
        assert_eq!(
            first, second,
            "the second refusal came inside the interval, so it must not have swept"
        );
    }

    #[test]
    fn the_bound_is_bytes_and_not_entry_count() {
        // The decision stated as a test: four small values and one large one are both three entries,
        // and only one of them should be refused by a byte bound.
        let small = MemoryStore::with_max_bytes(40);
        for index in 0..4u32 {
            small
                .set(format!("k{index}").as_bytes(), vec![0u8; 10], None)
                .unwrap();
        }
        assert!(matches!(
            small.set(b"fifth", vec![0u8; 10], None),
            Err(Error::Full { .. })
        ));
        assert_eq!(small.len(), 4, "four entries, each counted");

        let large = MemoryStore::with_max_bytes(40);
        assert!(matches!(
            large.set(b"one", vec![0u8; 100], None),
            Err(Error::Full { .. })
        ));
        assert_eq!(
            large.len(),
            0,
            "one entry that is too large is refused, where an entry-count bound would have taken it"
        );
    }

    #[test]
    fn keys_are_compared_as_bytes_so_an_invalid_utf8_key_is_a_key_like_any_other() {
        let store = store();
        let key = &[0xff, 0xfe, 0x00, 0x80][..];
        store.set(key, b"v".to_vec(), None).unwrap();
        assert_eq!(store.get(key).unwrap(), Some(b"v".to_vec()));
        assert_eq!(store.get(&[0xff, 0xfe, 0x00][..]).unwrap(), None);
    }

    #[test]
    fn keys_spread_across_shards_rather_than_piling_into_one() {
        let store = store();
        for index in 0..SHARDS * 8 {
            store
                .set(format!("key{index}").as_bytes(), b"v".to_vec(), None)
                .unwrap();
        }
        let occupied = store
            .shards
            .iter()
            .filter(|shard| !shard.lock().unwrap().map.is_empty())
            .count();
        assert!(
            occupied > SHARDS / 2,
            "16 shards exist to avoid one lock per call; {occupied} of {SHARDS} in use means the \
             hash or the index is not spreading keys"
        );
    }
}
