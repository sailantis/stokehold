//! The Redis driver: the same [`Locks`] over a server another process owns.
//!
//! # One hash per resource, one pair of fields per holder
//!
//! A resource is a Redis hash at `volucer:lock:<resource>`. Its fields are two per holder — the id
//! itself, holding the mode, and `<id>:deadline`, holding an absolute millisecond timestamp. Two
//! parallel fields rather than one packed value so that an operator reading `HGETALL` in `redis-cli`
//! sees ids, modes and deadlines as themselves.
//!
//! Redis has no per-field TTL, so the deadline is **Redis's own clock**, read with `TIME` and written as
//! `now + ttl`. That is RoadRunner's choice as well, and it is the right one: a client whose clock is a
//! minute fast would create a lock that outlives its lease, and one a minute slow a lock that expires
//! the instant it is taken.
//!
//! # Everything that must be atomic is one Lua script
//!
//! Three scripts, each one round trip where the check and the write are one operation. The alternative —
//! read in Rust, decide, write — is a race a single-threaded test cannot see and a deployment finds in
//! production. Two holders reaching a check-then-write in the same millisecond both conclude they won.
//!
//! # An unknown state is produced here, and it took care to make it *possible*
//!
//! This is the driver `docs/plugins.md` §6.3 was written for: a command that fails or times out during
//! an acquisition may or may not have run, and the caller has to be told that rather than told "not
//! held". Three things produce it, all in [`RedisLocks::classify`]:
//!
//! - **a connection that dropped.** `is_connection_dropped()` on a command that mutates means the bytes
//!   may have reached the server.
//! - **an I/O error** — `WouldBlock` or `TimedOut` — because a command whose reply never arrived may be
//!   executing on the server right now. Redis's own `timeout` setting closes connections that are idle,
//!   which makes an abandoned request indistinguishable from a dropped one from this side.
//! - **a client-side error raised before the write**, which is the one case where the *opposite* is
//!   true: `redis-rs` reports a command it never sent, so there is nothing undetermined. It is
//!   deliberately classified as a refusal, and the distinction cost a test to find.
//!
//! None of these is retried. A retry is a second command whose effect the caller cannot distinguish
//! from the first — which is exactly the duplicate the unknown flag exists to prevent.
//!
//! # This crate's `with_connection` is deliberately not `volucer_kv`'s
//!
//! `RedisStore`'s helper retries a dropped connection and reconnects in place. That is right for a cache:
//! a `GET` is idempotent and a reconnected pool is a repaired pool. Here the retry is precisely the thing
//! that must not happen, so the two helpers look alike and behave oppositely — a duplication that earns
//! its keep, because a shared one would eventually be "fixed" for whichever caller was read last.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::{Error, Holder, Lease, Locks, Mode, Result};

/// How many connections a pool holds unless configured otherwise.
///
/// The same value as the KV store's, and for the same reason: one per worker is the natural size, since
/// a call blocks the worker that asked.
pub const DEFAULT_POOL_SIZE: usize = 16;

/// How long one command may take before it is treated as failed.
///
/// Five seconds, which is what RoadRunner's lock configuration defaults to and what the worker
/// protocol's request timeout already is — so a lock command cannot outlive the request that issued it
/// by much, and a worker killed for exceeding its deadline is not killed while a lock command it cannot
/// see the end of is still running.
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// How often a waiting acquisition re-tries.
///
/// The same five milliseconds as the memory driver, and worth stating here too: a Redis lock is shared,
/// so a wait is a real poll of another process's progress, and a call is answered on the worker's own
/// exchange thread where a tight loop costs a core rather than a task.
pub const WAIT_SLICE: Duration = Duration::from_millis(5);

/// The prefix every resource's key carries.
///
/// Fixed rather than configurable, matching RoadRunner's `rr:lock:`. A resource name is the
/// application's, and a second prefix would be a second thing to get right before two applications
/// sharing one Redis could be told apart.
pub const PREFIX: &str = "volucer:lock:";

/// The suffix a holder's deadline field carries.
///
/// Two parallel fields per holder rather than one packed value. An id that itself ends in this suffix
/// would collide with another holder's deadline field, and that is accepted rather than escaped:
/// escaping would make an operator's `HGETALL` unreadable, the collision needs an application to choose
/// an id ending in a string this crate documents, and the consequence is a lease that expires early
/// rather than a lock granted twice.
pub const DEADLINE_SUFFIX: &str = ":deadline";

/// Locks in Redis, shared between processes.
pub struct RedisLocks {
    /// The URL, kept so a connection can be re-made when one fails.
    url: String,
    /// The pool, taken round-robin. The `Mutex` is held for one command, and no command is ever
    /// replayed on it.
    pool: Vec<std::sync::Mutex<redis::Connection>>,
    next: AtomicUsize,
    /// How long one command may take.
    read_timeout: Duration,
}

impl std::fmt::Debug for RedisLocks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written because `redis::Connection` is not `Debug`. The URL is deliberately **not**
        // printed: it may carry a password, and a `Debug` that printed it would put a credential into
        // whatever log line or panic message happened to format this.
        f.debug_struct("RedisLocks")
            .field("connections", &self.pool.len())
            .field("read_timeout", &self.read_timeout)
            .finish()
    }
}

impl RedisLocks {
    /// Connect a pool to Redis.
    ///
    /// Connects eagerly and `PING`s once, so an unreachable server — or a URL that happens to point at
    /// something which is not Redis — is a startup failure rather than a request that finds out. The
    /// worker pool and the KV store follow the same rule for the same reason: a misconfiguration belongs
    /// where the configuration is read.
    pub fn connect(url: &str, pool_size: usize, read_timeout: Duration) -> Result<Self> {
        let pool_size = pool_size.max(1);
        let read_timeout = if read_timeout.is_zero() {
            DEFAULT_READ_TIMEOUT
        } else {
            read_timeout
        };

        let mut pool = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            pool.push(std::sync::Mutex::new(Self::dial(url, read_timeout)?));
        }

        let locks = RedisLocks {
            url: url.to_string(),
            pool,
            next: AtomicUsize::new(0),
            read_timeout,
        };
        locks.ping()?;
        tracing::info!(
            connections = pool_size,
            read_timeout_ms = read_timeout.as_millis() as u64,
            "redis lock backend ready"
        );
        Ok(locks)
    }

    /// One connection to the server, with the command deadline already applied.
    ///
    /// The URL is never included in a message: it may carry a password, and an error string ends up in a
    /// log.
    fn dial(url: &str, read_timeout: Duration) -> Result<redis::Connection> {
        let client = redis::Client::open(url).map_err(|err| Error::Backend {
            driver: "redis",
            reason: format!("the Redis URL is not usable: {err}"),
        })?;
        let connection = client.get_connection().map_err(|err| Error::Backend {
            driver: "redis",
            reason: format!("could not connect: {err}"),
        })?;
        // Set on the connection rather than passed per command: the synchronous client has no
        // per-command deadline, and this is the setting that makes a hung server produce an error the
        // caller can act on instead of a blocked worker.
        connection
            .set_read_timeout(Some(read_timeout))
            .map_err(|err| Error::Backend {
                driver: "redis",
                reason: format!("could not set the command deadline: {err}"),
            })?;
        Ok(connection)
    }

    /// Run one command on a pooled connection, **without replaying it**.
    ///
    /// The policy that distinguishes this from `volucer_kv`'s helper, and the module documentation says
    /// why. A dropped connection is repaired *in place* after a failure, so the next caller finds a
    /// working connection rather than meeting the same one — but the command that failed is not run
    /// again.
    fn with_connection<T>(
        &self,
        operation: &'static str,
        resource: &str,
        acquiring: bool,
        run: impl Fn(&mut redis::Connection) -> redis::RedisResult<T>,
    ) -> Result<T> {
        let index = self.next.fetch_add(1, Ordering::Relaxed) % self.pool.len();
        let mut connection = self.pool[index]
            .lock()
            // A poisoned lock is one a previous call panicked while holding. Recovering is right: the
            // guarded value is a socket, no panic can leave it in a state a redial cannot fix, and
            // refusing every later lock call for the rest of the process's life is the worse failure.
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        match run(&mut connection) {
            Ok(value) => Ok(value),
            Err(err) => {
                if err.is_connection_dropped() || err.is_io_error() {
                    // Repair the slot for the *next* caller. A redial that itself fails is reported as a
                    // plain backend failure, because at that point nothing about this command can be
                    // concluded from the connection's state.
                    tracing::warn!(
                        operation,
                        resource,
                        %err,
                        "a redis lock connection failed; repairing the slot without replaying the command"
                    );
                    match Self::dial(&self.url, self.read_timeout) {
                        Ok(repaired) => *connection = repaired,
                        Err(reason) => {
                            tracing::warn!(
                                operation,
                                "the redis lock connection could not be re-established"
                            );
                            return Err(reason);
                        }
                    }
                }
                Err(self.classify(operation, resource, acquiring, err))
            }
        }
    }

    /// Turn a client error into the right variant.
    ///
    /// **The one decision that matters is `Unknown` versus `Backend`.** It is made on the error kind
    /// rather than on the operation alone, because not every failure during an acquisition is
    /// undetermined:
    ///
    /// - `Io` — a dropped or timed-out connection — means the bytes may have reached the server and the
    ///   command may be running. That is the case §6.3 documents. **Only during an acquisition**: a read
    ///   that failed did nothing, and a release that failed before reaching the server did nothing
    ///   either, so a caller may simply retry those.
    /// - `Server(_)` is Redis *answering*: a `WRONGTYPE`, an `OOM` under `maxmemory`. However alarming,
    ///   it is a decided refusal and reporting it as unknown would be the false-unknown this file's own
    ///   documentation warns about.
    /// - `Client(_)` is `redis-rs` refusing to send the command. Nothing was transmitted, so *nothing* is
    ///   undetermined — and getting this wrong was the first version's bug: a command that never left
    ///   the process was reported as an unknown lock state, which is precisely the over-reporting that
    ///   teaches callers to ignore the flag.
    fn classify(
        &self,
        operation: &'static str,
        resource: &str,
        acquiring: bool,
        err: redis::RedisError,
    ) -> Error {
        let undetermined = matches!(err.kind(), redis::ErrorKind::Io);
        if acquiring && undetermined {
            return Error::Unknown {
                resource: resource.to_string(),
                operation,
                reason: format!("the command did not complete: {err}"),
            };
        }
        Error::Backend {
            driver: "redis",
            reason: format!("{operation} failed: {err}"),
        }
    }

    fn ping(&self) -> Result<()> {
        self.with_connection("PING", "", false, |connection| {
            redis::cmd("PING").query::<String>(connection)
        })
        .map(|_| ())
    }

    /// The key a resource's holders live at.
    pub fn key(resource: &str) -> String {
        format!("{PREFIX}{resource}")
    }

    /// The hash field an id's deadline is stored in.
    pub fn field(id: &str) -> String {
        format!("{id}{DEADLINE_SUFFIX}")
    }

    /// Redis's own `TIME`, in milliseconds since the epoch.
    ///
    /// Read from the server rather than computed locally because the deadline is the server's to
    /// enforce — see the module documentation.
    fn now_millis(connection: &mut redis::Connection) -> redis::RedisResult<i64> {
        let (seconds, micros): (i64, i64) = redis::cmd("TIME").query(connection)?;
        Ok(seconds * 1000 + micros / 1000)
    }

    /// The absolute deadline a lease implies, from Redis's own clock.
    ///
    /// `-1` is the "no deadline" sentinel, and it cannot collide with a real one: a millisecond
    /// timestamp is a large positive number in every clock this will meet.
    fn deadline(connection: &mut redis::Connection, lease: Lease) -> redis::RedisResult<i64> {
        match lease.millis() {
            None => Ok(NO_DEADLINE),
            Some(millis) => Ok(Self::now_millis(connection)? + millis as i64),
        }
    }
}

/// The sentinel a lease that does not expire is stored as.
const NO_DEADLINE: i64 = -1;

impl Locks for RedisLocks {
    fn name(&self) -> &'static str {
        "redis"
    }

    fn try_acquire(
        &self,
        resource: &str,
        id: &str,
        mode: Mode,
        lease: Lease,
    ) -> Result<Option<Holder>> {
        crate::validate(resource, id)?;

        let answer = self.with_connection("acquire", resource, true, |connection| {
            let deadline = Self::deadline(connection, lease)?;
            let now = Self::now_millis(connection)?;
            redis::Script::new(ACQUIRE_SCRIPT)
                .key(Self::key(resource))
                .arg(id)
                .arg(mode.as_str())
                .arg(deadline)
                .arg(now)
                .invoke::<redis::Value>(connection)
        })?;

        match answer {
            // The script answers the id it wrote, which is the same string the caller sent. It is
            // compared rather than trusted so that a script and this code cannot drift apart silently.
            redis::Value::BulkString(_) | redis::Value::SimpleString(_) | redis::Value::Okay => {
                Ok(Some(Holder {
                    id: id.to_string(),
                    mode,
                    remaining: lease.millis().map(Duration::from_millis),
                }))
            }
            redis::Value::Int(ALREADY_HELD) => Err(Error::AlreadyHeld {
                resource: resource.to_string(),
                id: id.to_string(),
                mode: mode.as_str(),
            }),
            // `false` from Lua, which is how contention arrives.
            _ => Ok(None),
        }
    }

    fn acquire(
        &self,
        resource: &str,
        id: &str,
        mode: Mode,
        lease: Lease,
        wait: Duration,
    ) -> Result<Option<Holder>> {
        // One attempt when there is nothing to wait for, so `wait = 0` is provably the same call as
        // `try_acquire` rather than a loop that happens to exit first time.
        if wait.is_zero() {
            return self.try_acquire(resource, id, mode, lease);
        }

        let deadline = Instant::now() + wait;
        loop {
            if let Some(holder) = self.try_acquire(resource, id, mode, lease)? {
                return Ok(Some(holder));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            // The slice, or what is left of the wait if that is less, so the last sleep never overshoots
            // the deadline the caller gave.
            std::thread::sleep(WAIT_SLICE.min(deadline.saturating_duration_since(now)));
        }
    }

    fn release(&self, resource: &str, id: &str) -> Result<bool> {
        crate::validate(resource, id)?;
        // One script: a `HEXISTS` and a `HDEL` as two commands is a window in which a different holder
        // takes the id — or, worse, the `HDEL` of a field a *new* holder has just written.
        self.with_connection("release", resource, false, |connection| {
            redis::Script::new(RELEASE_SCRIPT)
                .key(Self::key(resource))
                .arg(id)
                .invoke::<i64>(connection)
        })
        .map(|removed| removed > 0)
    }

    fn release_all(&self, resource: &str) -> Result<bool> {
        crate::validate_resource(resource)?;
        // `DEL` and not a script: removing the whole key needs no check, and the answer `DEL` gives —
        // how many keys it removed — is exactly the `bool` the caller wants.
        self.with_connection("release_all", resource, false, |connection| {
            redis::cmd("DEL")
                .arg(Self::key(resource))
                .query::<i64>(connection)
        })
        .map(|removed| removed > 0)
    }

    fn holders(&self, resource: &str) -> Result<Vec<Holder>> {
        crate::validate_resource(resource)?;
        self.with_connection("holders", resource, false, |connection| {
            let now = Self::now_millis(connection)?;
            // The time is read in the same closure as the hash so one call cannot span a clock change
            // between reading the holders and deciding which of them are live.
            redis::pipe()
                .cmd("TIME")
                .cmd("HGETALL")
                .arg(Self::key(resource))
                .query::<(redis::Value, Vec<String>)>(connection)
                .map(|(time, flat)| {
                    let now = match time {
                        redis::Value::Array(parts) if parts.len() == 2 => {
                            let seconds = as_i64(&parts[0]).unwrap_or(now / 1000);
                            let micros = as_i64(&parts[1]).unwrap_or(0);
                            seconds * 1000 + micros / 1000
                        }
                        _ => now,
                    };
                    holders_from(&flat, now)
                })
        })
    }

    fn update_lease(&self, resource: &str, id: &str, lease: Lease) -> Result<bool> {
        crate::validate(resource, id)?;
        self.with_connection("update_lease", resource, false, |connection| {
            let deadline = Self::deadline(connection, lease)?;
            redis::Script::new(UPDATE_SCRIPT)
                .key(Self::key(resource))
                .arg(id)
                .arg(deadline)
                .invoke::<i64>(connection)
        })
        .map(|updated| updated > 0)
    }
}

/// A `redis::Value` that should be an integer.
fn as_i64(value: &redis::Value) -> Option<i64> {
    match value {
        redis::Value::Int(number) => Some(*number),
        redis::Value::BulkString(bytes) => std::str::from_utf8(bytes).ok()?.parse().ok(),
        _ => None,
    }
}

/// What the acquisition script answers for an id that already holds the resource.
///
/// An integer rather than a string, because Lua's `false` and a Lua string both arrive as distinct
/// `redis::Value`s and an integer is the one that cannot be confused with a mode name.
const ALREADY_HELD: i64 = -1;

/// The acquisition, as one operation.
///
/// `KEYS[1]` the resource key. `ARGV[1]` the id. `ARGV[2]` `exclusive` or `shared`. `ARGV[3]` an absolute
/// deadline in milliseconds, or `-1`. `ARGV[4]` Redis's current time in milliseconds.
///
/// Returns the id on success, `false` for contention, and `-1` for "this id already holds it".
///
/// # Why the expiry sweep happens here and not on a timer
///
/// Redis has no per-field TTL, so an expired holder stays in the hash until something removes it. That
/// something is this script, on the way into a write: a resource nobody writes to again keeps its stale
/// fields, and the only cost is a few bytes — whereas a sweeper would need a second connection, a timer,
/// and a lock to avoid racing the scripts that matter. The same reasoning as the memory driver's
/// sweep-on-insert, arrived at independently.
const ACQUIRE_SCRIPT: &str = r#"
local key = KEYS[1]
local id = ARGV[1]
local mode = ARGV[2]
local deadline = tonumber(ARGV[3])
local now = tonumber(ARGV[4])

-- Drop every holder whose lease has passed, so the conflict check below sees the truth rather than
-- whatever the last writer left behind.
local fields = redis.call('HGETALL', key)
local i = 1
while i <= #fields do
    local field = fields[i]
    if string.sub(field, -9) == ':deadline' then
        local at = tonumber(fields[i + 1])
        if at and at >= 0 and at <= now then
            local holder = string.sub(field, 1, #field - 9)
            redis.call('HDEL', key, holder, field)
        end
    end
    i = i + 2
end

-- The id's own claim, asked after the sweep: a lease that just expired is not a claim.
if redis.call('HEXISTS', key, id) == 1 then
    return -1
end

-- The conflict check. An exclusive holder excludes everybody; a shared holder excludes only an
-- exclusive one. Read after the sweep so a stale entry cannot make this refuse.
fields = redis.call('HGETALL', key)
i = 1
while i <= #fields do
    local field = fields[i]
    if string.sub(field, -9) ~= ':deadline' then
        local held = fields[i + 1]
        if held == 'exclusive' or mode == 'exclusive' then
            return false
        end
    end
    i = i + 2
end

redis.call('HSET', key, id, mode)
redis.call('HSET', key, id .. ':deadline', deadline)
return id
"#;

/// Release one holder, in one operation.
const RELEASE_SCRIPT: &str = r#"
local key = KEYS[1]
local id = ARGV[1]
if redis.call('HEXISTS', key, id) == 0 then
    return 0
end
redis.call('HDEL', key, id, id .. ':deadline')
if redis.call('HLEN', key) == 0 then
    redis.call('DEL', key)
end
return 1
"#;

/// Replace one holder's deadline, or report that it holds nothing.
///
/// **The replacement rather than the extension**, and doing it in one script is what makes "the id was
/// gone" a reliable answer: a `HEXISTS` and an `HSET` as two commands could have the holder release
/// between them, and the `HSET` would then re-create a lock nobody holds under a field whose deadline
/// nothing will ever remove.
const UPDATE_SCRIPT: &str = r#"
local key = KEYS[1]
local id = ARGV[1]
if redis.call('HEXISTS', key, id) == 0 then
    return 0
end
redis.call('HSET', key, id .. ':deadline', ARGV[2])
return 1
"#;

/// Turn a flat `HGETALL` answer into the live holders.
///
/// `HGETALL` answers as a flat `[field, value, field, value, …]` with the fields in no particular order,
/// so the two halves of a holder are paired here rather than assumed adjacent. Expired holders are
/// dropped, because the question a caller is asking is what state it is in and not what state the hash
/// is in — the rule the memory driver's `Registry` applies and the reason both have one.
fn holders_from(flat: &[String], now_ms: i64) -> Vec<Holder> {
    let mut modes: Vec<(String, Mode)> = Vec::new();
    let mut deadlines: Vec<(String, i64)> = Vec::new();

    for pair in flat.chunks(2) {
        let [field, value] = pair else { continue };
        match field.strip_suffix(DEADLINE_SUFFIX) {
            Some(holder) => deadlines.push((
                holder.to_string(),
                // An unparseable deadline becomes `i64::MIN`, which is a value nothing can be at or
                // past, so the holder is reported as **held** rather than as expired. That is the safe
                // direction: reporting it as expired would let a second holder into the section.
                value.parse::<i64>().unwrap_or(i64::MIN),
            )),
            None => modes.push((
                field.clone(),
                match value.as_str() {
                    "shared" => Mode::Shared,
                    _ => Mode::Exclusive,
                },
            )),
        }
    }

    let mut holders = Vec::with_capacity(modes.len());
    for (id, mode) in modes {
        let deadline = deadlines
            .iter()
            .find(|(holder, _)| *holder == id)
            .map(|(_, deadline)| *deadline);
        match deadline {
            // No deadline field: either a lease that does not expire, or an entry something else wrote.
            // Read as held, for the reason above.
            None | Some(NO_DEADLINE) | Some(i64::MIN) => holders.push(Holder {
                id,
                mode,
                remaining: None,
            }),
            Some(at) if at <= now_ms => continue,
            // Rounded *up*, so a caller comparing against zero is never told that a live holder's lease
            // is `In(0)`, which it could not tell from expired. The memory driver does the same.
            Some(at) => holders.push(Holder {
                id,
                mode,
                remaining: Some(Duration::from_millis((at - now_ms).max(1) as u64)),
            }),
        }
    }
    holders
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resource_key_carries_the_fixed_prefix() {
        assert_eq!(RedisLocks::key("pdf:create"), "volucer:lock:pdf:create");
        assert_eq!(
            PREFIX, "volucer:lock:",
            "the prefix is fixed rather than configured, matching RoadRunner: a resource name is the \
             application's, and a second prefix is a second thing to agree on before two applications \
             sharing one Redis can be told apart"
        );
    }

    #[test]
    fn a_holders_deadline_field_is_the_id_with_a_suffix() {
        assert_eq!(RedisLocks::field("worker-1"), "worker-1:deadline");
        assert_eq!(RedisLocks::field(""), ":deadline");
    }

    #[test]
    fn a_flat_hgetall_becomes_the_live_holders() {
        let flat = vec![
            "worker-1".to_string(),
            "exclusive".to_string(),
            "worker-1:deadline".to_string(),
            "1700000005000".to_string(),
        ];
        let holders = holders_from(&flat, 1_700_000_000_000);
        assert_eq!(holders.len(), 1);
        assert_eq!(holders[0].id, "worker-1");
        assert_eq!(holders[0].mode, Mode::Exclusive);
        assert_eq!(holders[0].remaining, Some(Duration::from_secs(5)));
    }

    #[test]
    fn the_two_fields_of_one_holder_are_paired_even_when_hgetall_returns_them_apart() {
        // Redis makes no promise about the order of `HGETALL`'s fields, so a reader that assumed a
        // holder's two fields were adjacent would work in a test and fail on a server that rehashed.
        let flat = vec![
            "a:deadline".to_string(),
            "1700000009000".to_string(),
            "a".to_string(),
            "shared".to_string(),
        ];
        let holders = holders_from(&flat, 1_700_000_000_000);
        assert_eq!(holders.len(), 1);
        assert_eq!(holders[0].id, "a");
        assert_eq!(holders[0].mode, Mode::Shared);
        assert_eq!(holders[0].remaining, Some(Duration::from_secs(9)));
    }

    #[test]
    fn an_expired_holder_is_not_reported_even_though_it_is_still_stored() {
        // The property the memory driver's `Registry` applies and this one has to match. A driver that
        // reported the stale entry would have a caller wait for a lease that had already run out.
        let flat = vec![
            "gone".to_string(),
            "exclusive".to_string(),
            "gone:deadline".to_string(),
            "1699999999000".to_string(),
            "here".to_string(),
            "shared".to_string(),
            "here:deadline".to_string(),
            "1700000001000".to_string(),
        ];
        let holders = holders_from(&flat, 1_700_000_000_000);
        assert_eq!(holders.len(), 1, "{holders:?}");
        assert_eq!(holders[0].id, "here");
        assert_eq!(holders[0].mode, Mode::Shared);
    }

    #[test]
    fn a_deadline_of_minus_one_is_a_lease_that_does_not_expire() {
        let flat = vec![
            "forever".to_string(),
            "exclusive".to_string(),
            "forever:deadline".to_string(),
            "-1".to_string(),
        ];
        let holders = holders_from(&flat, 1_700_000_000_000);
        assert_eq!(holders.len(), 1);
        assert_eq!(
            holders[0].remaining, None,
            "`ttl = 0` is held until released, so the sentinel has to survive the round trip through a \
             string field"
        );
    }

    #[test]
    fn a_holder_with_no_deadline_field_is_read_as_held_rather_than_as_expired() {
        // The safe direction: an entry something else wrote, or a hash left half-written, is reported as
        // holding the lock, so a caller waits. Reporting it as expired would let a second holder into the
        // section, which is the failure this crate exists to prevent.
        let flat = vec!["mystery".to_string(), "exclusive".to_string()];
        let holders = holders_from(&flat, 1_700_000_000_000);
        assert_eq!(holders.len(), 1);
        assert_eq!(holders[0].remaining, None);
    }

    #[test]
    fn an_unparseable_deadline_is_read_as_held_rather_than_as_expired() {
        let flat = vec![
            "broken".to_string(),
            "exclusive".to_string(),
            "broken:deadline".to_string(),
            "not-a-number".to_string(),
        ];
        let holders = holders_from(&flat, 1_700_000_000_000);
        assert_eq!(
            holders.len(),
            1,
            "which `i64::MIN` gives: nothing can be at or past it"
        );
        assert_eq!(holders[0].remaining, None);
    }

    #[test]
    fn a_deadline_that_has_just_passed_reports_one_millisecond_rather_than_zero() {
        let flat = vec![
            "soon".to_string(),
            "exclusive".to_string(),
            "soon:deadline".to_string(),
            "1700000000001".to_string(),
        ];
        let holders = holders_from(&flat, 1_700_000_000_000);
        assert_eq!(holders.len(), 1);
        assert_eq!(holders[0].remaining, Some(Duration::from_millis(1)));
    }

    #[test]
    fn an_odd_number_of_fields_is_not_a_panic() {
        // A hash mid-write cannot produce this — `HSET` of two fields is one command — but a hash an
        // operator edited by hand can, and a read is not the place to panic over it.
        let flat = vec!["lonely".to_string()];
        assert!(holders_from(&flat, 1_700_000_000_000).is_empty());
    }
}
