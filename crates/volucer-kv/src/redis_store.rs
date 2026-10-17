//! The Redis driver: the same [`Store`] over a server another process owns.
//!
//! # Synchronous connections, deliberately
//!
//! `redis`'s `ConnectionManager` reconnects a dropped connection for you and is the obvious thing to
//! reach for — and it is **async**, so using it would mean either driving a `tokio` runtime from the
//! worker exchange threads, which is how one slow Redis stalls a whole pool, or moving the call path onto
//! a runtime, which `docs/plugins.md` §6.2 refuses. So this is the synchronous [`redis::Connection`], with
//! a pool the size of the worker pool and one reconnect-and-retry when a connection itself fails.
//!
//! The retry is a real feature rather than a nicety: Redis restarts, a network blips, a failover moves the
//! primary. A pool that could not recover would leave every worker unable to cache until the server was
//! restarted, which is a worse outage than the one that caused it.
//!
//! # Why the TTL is set in one command
//!
//! `SET key value PX millis` rather than `SET` followed by `EXPIRE`. Two commands would leave a window in
//! which the key exists with no deadline, and a process that died inside it would leave a key behind that
//! nothing ever expires — the same class of bug the `memory` driver's sweep-on-full avoids in its own way.
//! `PX` and not `EX`, because the caller's TTL is a `Duration` and rounding 900 ms to seconds would
//! silently make it either 0 or 1.
//!
//! # Why a key prefix per storage, and why it is not the caller's
//!
//! Two `[kv.*]` tables pointing at one Redis is the normal case — a session store and a fragment cache
//! with different lifetimes — and a `clear` on one must not empty the other. Redis's `FLUSHDB` would, so
//! `clear` walks this storage's prefix with `SCAN` and `DEL`s what it finds. `SCAN` and not `KEYS` because
//! `KEYS` blocks the server for the length of the whole keyspace, and a cache that stalls every other
//! client of the same Redis while clearing itself is a cache nobody should deploy.
//!
//! The prefix is **derived from the storage's name** rather than configured separately, because two names
//! for one thing is how a deployment comes to have a prefix that does not match the table it was written
//! under.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::{Error, Result, Store, Ttl};

/// How many connections a pool holds unless configured otherwise.
///
/// One per worker is the natural size — a call blocks the worker that asked — so sixteen covers a pool of
/// sixteen workers with none waiting, at the cost of sixteen sockets. A deployment with more workers than
/// this still works; its calls queue on the pool rather than on the server.
pub const DEFAULT_POOL_SIZE: usize = 16;

/// How many keys `SCAN` returns per round trip while clearing.
///
/// A hint, not a limit: Redis may return more or fewer. Large enough that clearing a hundred thousand keys
/// is a hundred round trips rather than ten thousand, small enough that one batch is not a burst of memory
/// in the server.
const SCAN_BATCH: usize = 1000;

/// A key-value store in Redis.
pub struct RedisStore {
    /// The URL, kept so a connection can be re-made when one fails.
    url: String,
    /// The pool, taken round-robin. The `Mutex` is held for one command, never across a retry of the whole
    /// operation.
    pool: Vec<std::sync::Mutex<redis::Connection>>,
    next: AtomicUsize,
    /// The prefix every key of this storage carries, so `clear` can scope itself to it.
    prefix: String,
}

impl std::fmt::Debug for RedisStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written because `redis::Connection` is not `Debug`. The URL is deliberately **not** printed:
        // it may carry a password, and a `Debug` that printed it would put a credential into whatever log
        // line or panic message happened to format this.
        f.debug_struct("RedisStore")
            .field("connections", &self.pool.len())
            .field("prefix", &self.prefix)
            .finish()
    }
}

impl RedisStore {
    /// Connect a pool to Redis, with every key of `storage` under its own prefix.
    ///
    /// Connects eagerly and `PING`s once, so an unreachable server — or a URL that happens to point at
    /// something which is not Redis — is a startup failure rather than a 500 on the first request that
    /// reads a cache. The worker pool follows the same rule for the same reason: a misconfiguration
    /// belongs where the configuration is read.
    pub fn connect(url: &str, pool_size: usize, storage: &str) -> Result<Self> {
        let pool_size = pool_size.max(1);
        let mut pool = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            pool.push(std::sync::Mutex::new(Self::dial(url)?));
        }

        let store = RedisStore {
            url: url.to_string(),
            pool,
            next: AtomicUsize::new(0),
            prefix: Self::prefix_for(storage),
        };
        store.ping()?;
        tracing::info!(
            connections = pool_size,
            prefix = %store.prefix,
            "redis store ready"
        );
        Ok(store)
    }

    /// One connection to the server, or why not.
    ///
    /// The URL is not included in the message: it may carry a password, and an error string ends up in
    /// logs.
    fn dial(url: &str) -> Result<redis::Connection> {
        let client = redis::Client::open(url).map_err(|err| Error::Store {
            driver: "redis",
            reason: format!("the Redis URL is not usable: {err}"),
        })?;
        client.get_connection().map_err(|err| Error::Store {
            driver: "redis",
            reason: format!("could not connect: {err}"),
        })
    }

    /// The prefix a storage's keys live under.
    ///
    /// Public because it answers "which keys are mine", and an operator clearing a stale cache by hand
    /// needs the same string the server would have used.
    pub fn prefix_for(storage: &str) -> String {
        format!("volucer:kv:{storage}:")
    }

    /// The key as Redis sees it: this storage's prefix, then the caller's bytes.
    ///
    /// The caller's bytes are used as they are, not escaped. A key containing `:` — which is what PSR-16
    /// uses for namespacing, so an application's keys do — is then readable in `MONITOR`, and the prefix is
    /// the only part whose structure this side needs. Escaping the rest would make an operator's view of a
    /// live server unreadable for no gain.
    fn redis_key(&self, key: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.prefix.len() + key.len());
        out.extend_from_slice(self.prefix.as_bytes());
        out.extend_from_slice(key);
        out
    }

    /// Run one command on a pooled connection, reconnecting once if the connection itself failed.
    ///
    /// **The retry is only for connection errors**, and that distinction is what makes it safe rather than
    /// a double-write bug: an error from Redis itself — a `WRONGTYPE` against a key somebody else created,
    /// an `OOM` under `maxmemory` — is returned as it is, because reconnecting would not change the answer.
    /// The client's own classifiers for "this socket is no longer usable" are what make the retry a repair
    /// rather than a guess.
    ///
    /// The reconnection happens **in place**, so the pool keeps its size and the next caller on this slot
    /// finds a working connection rather than meeting the same failure.
    fn with_connection<T>(
        &self,
        what: &str,
        run: impl Fn(&mut redis::Connection) -> redis::RedisResult<T>,
    ) -> Result<T> {
        let index = self.next.fetch_add(1, Ordering::Relaxed) % self.pool.len();
        let mut connection = self.pool[index]
            .lock()
            // A poisoned lock is one a previous call panicked while holding. Recovering is right: the
            // connection is a socket whose state does not depend on that panic, and refusing this slot for
            // ever would shrink the pool by one every time until there were none.
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        match run(&mut connection) {
            Ok(value) => Ok(value),
            Err(err) if err.is_connection_dropped() || err.is_io_error() => {
                tracing::warn!(command = what, %err, "a redis connection dropped; reconnecting");
                *connection = Self::dial(&self.url)?;
                run(&mut connection).map_err(|err| self.failure(what, err))
            }
            Err(err) => Err(self.failure(what, err)),
        }
    }

    fn ping(&self) -> Result<()> {
        self.with_connection("PING", |connection| {
            redis::cmd("PING").query::<String>(connection)
        })
        .map(|_| ())
    }

    /// One place that turns a client error into the store error, so no method can forget the driver name.
    fn failure(&self, what: &str, err: redis::RedisError) -> Error {
        Error::Store {
            driver: "redis",
            reason: format!("{what} failed: {err}"),
        }
    }

    /// The remaining time on a key, in the three states the trait needs.
    ///
    /// `PTTL` rather than `TTL`: the second has one-second resolution, so a key with 600 ms left would
    /// report `0` — the same answer as a key that does not exist, which is the whole reason [`Ttl`] has
    /// three states rather than two.
    fn remaining(&self, redis_key: &[u8]) -> Result<Ttl> {
        let millis: i64 = self.with_connection("PTTL", |connection| {
            redis::cmd("PTTL").arg(redis_key).query(connection)
        })?;

        Ok(match millis {
            // Redis's own answers, documented as values rather than errors: -2 for "no such key" and -1 for
            // "no expiry". Matched rather than treated as a failure, because they *are* the answers.
            -2 => Ttl::Missing,
            -1 => Ttl::Forever,
            // Rounded *up*, so a key with 1 ms left is `In(1ms)` rather than `In(0)`: a caller comparing
            // against zero is then never told that a live key has expired.
            _ => Ttl::In(Duration::from_millis(millis.max(1) as u64)),
        })
    }
}

impl Store for RedisStore {
    fn name(&self) -> &'static str {
        "redis"
    }

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let redis_key = self.redis_key(key);
        self.with_connection("GET", |connection| {
            redis::cmd("GET").arg(&redis_key).query(connection)
        })
    }

    fn set(&self, key: &[u8], value: Vec<u8>, ttl: Option<Duration>) -> Result<()> {
        // PSR-16's rule, enforced here rather than trusted to the client, exactly as the memory driver does
        // it. The two drivers must agree about what a zero TTL means, or an application that moves from one
        // to the other changes behaviour without changing its code.
        if ttl.is_some_and(|ttl| ttl.is_zero()) {
            self.delete(key)?;
            return Ok(());
        }

        let redis_key = self.redis_key(key);
        let ttl = crate::clamp_ttl(ttl);
        self.with_connection("SET", |connection| {
            let mut command = redis::cmd("SET");
            command.arg(&redis_key).arg(&value);
            // One command, so there is no window in which the key exists with no deadline — see the module
            // documentation.
            if let Some(ttl) = ttl {
                command.arg("PX").arg(ttl.as_millis() as u64);
            }
            command.query::<()>(connection)
        })
    }

    fn delete(&self, key: &[u8]) -> Result<bool> {
        let redis_key = self.redis_key(key);
        let removed: i64 = self.with_connection("DEL", |connection| {
            redis::cmd("DEL").arg(&redis_key).query(connection)
        })?;
        // `DEL` answers how many keys it removed, which for one key is 1 or 0 — the answer the trait
        // promises, and the reason it is not `()`: "deleted" and "was not there" are different answers to an
        // application counting its own misses.
        Ok(removed > 0)
    }

    fn ttl(&self, key: &[u8]) -> Result<Ttl> {
        let redis_key = self.redis_key(key);
        self.remaining(&redis_key)
    }

    fn clear(&self) -> Result<()> {
        // Scoped to this storage's prefix, because two `[kv.*]` tables may share one Redis and a `FLUSHDB`
        // would empty both. `SCAN` rather than `KEYS` so the server is never blocked for the length of its
        // keyspace — see the module documentation.
        let pattern = format!("{}*", self.prefix);
        let mut cursor = 0u64;
        let mut removed = 0usize;

        loop {
            let (next, keys): (u64, Vec<Vec<u8>>) = self.with_connection("SCAN", |connection| {
                redis::cmd("SCAN")
                    .arg(cursor)
                    .arg("MATCH")
                    .arg(&pattern)
                    .arg("COUNT")
                    .arg(SCAN_BATCH)
                    .query(connection)
            })?;

            if !keys.is_empty() {
                let deleted: i64 = self.with_connection("DEL", |connection| {
                    redis::cmd("DEL").arg(&keys).query(connection)
                })?;
                removed += deleted.max(0) as usize;
            }

            // `SCAN` returns zero as the next cursor when it is done, which is the only termination
            // condition. Redis guarantees the cursor does not repeat, which is what makes the loop finite
            // even while the keyspace changes underneath it.
            cursor = next;
            if cursor == 0 {
                break;
            }
        }

        tracing::debug!(prefix = %self.prefix, removed, "cleared a redis storage");
        Ok(())
    }

    fn get_many(&self, keys: &[&[u8]]) -> Result<Vec<Option<Vec<u8>>>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        // One `MGET` rather than the default loop, which is what the trait's own documentation says an
        // override is for: on Redis the loop is one round trip per key. `MGET`'s answer is in the order of
        // its arguments, which is the positional contract the trait promises.
        let encoded: Vec<Vec<u8>> = keys.iter().map(|key| self.redis_key(key)).collect();
        self.with_connection("MGET", |connection| {
            redis::cmd("MGET").arg(&encoded).query(connection)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store with no connections, for the tests that only exercise the prefixing.
    ///
    /// [`RedisStore::redis_key`] is pure and takes no connection, so this exercises the real function
    /// rather than a stub — and it means the two cheapest properties are checked without a Redis running.
    fn unconnected(storage: &str) -> RedisStore {
        RedisStore {
            url: String::new(),
            pool: Vec::new(),
            next: AtomicUsize::new(0),
            prefix: RedisStore::prefix_for(storage),
        }
    }

    #[test]
    fn a_storages_prefix_is_derived_from_its_name_and_distinct_per_storage() {
        // The property `clear`'s scoping rests on. Two storages whose prefixes collided would make one
        // `clear` empty the other, silently — the keys would simply be gone.
        let sessions = RedisStore::prefix_for("sessions");
        let cache = RedisStore::prefix_for("cache");
        assert_ne!(sessions, cache);
        assert!(
            sessions.starts_with("volucer:kv:sessions:"),
            "{sessions} should name the feature and the storage"
        );
        assert!(
            !cache.starts_with(&sessions),
            "neither prefix may be a prefix of the other, or a SCAN for one would match the other's keys"
        );
    }

    #[test]
    fn the_key_a_storage_writes_is_its_prefix_then_the_callers_bytes() {
        let store = unconnected("sessions");

        assert_eq!(
            store.redis_key(b"user:1"),
            b"volucer:kv:sessions:user:1".to_vec(),
            "an application's own namespacing survives unchanged"
        );

        let mut expected = b"volucer:kv:sessions:".to_vec();
        expected.extend_from_slice(&[0xff, 0x00]);
        assert_eq!(
            store.redis_key(&[0xff, 0x00]),
            expected,
            "a key with bytes above 0x7f is not escaped: the prefix is the only part whose structure \
             this side needs, and escaping the rest would make an operator's view of a live server \
             unreadable for no gain"
        );
    }
}
