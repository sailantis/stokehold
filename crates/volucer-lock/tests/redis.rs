//! The Redis lock driver, against a **real** Redis.
//!
//! **The whole file is behind the `redis` feature**, so a build without it has no test to skip and no
//! `RedisLocks` to import. A file that compiled in both would need every test's body gated as well, which
//! is the same gate written many times and one of them eventually forgotten.
//!
//! # Why these are skipped rather than mocked
//!
//! Almost everything worth checking here is a property of the *server*: that a Lua script's check and
//! write are one operation, that `HSET`/`HDEL`/`HEXISTS` see each other's writes, that `TIME` answers
//! milliseconds, that a second connection sees the first one's lock. A mock would answer all of those
//! however its author imagined Redis behaves — and the properties that make a distributed lock work at
//! all are exactly the ones a mock is unable to disagree with.
//!
//! The one property a mock *could* test is the conversion of a flat `HGETALL` into holders, and that is
//! already a pure function with unit tests in the module.
//!
//! These run against whatever is at `REDIS_URL` — `redis://127.0.0.1:6379` by default — and **skip with a
//! printed reason** when nothing is there. Set `VOLUCER_E2E=1` where a skip should be a failure.
//!
//! # And why each test names its own resource
//!
//! These run against a server somebody may be using for something else. Each test locks a resource unique
//! to itself, so its key is `volucer:lock:test-<pid>-<name>` and no other test looks at it — and the
//! `release_all` some of them end with can then only remove its own.
//!
//! # Serial
//!
//! Redis itself is concurrent, and these are written to be safe in parallel — every assertion is scoped
//! to a key named after the test. They are run with `--test-threads=1` anyway, like the KV driver's,
//! because a shared server under a loaded CI machine is not where a flake should be discovered.

#![cfg(feature = "redis")]

use std::time::Duration;

use volucer_lock::{Lease, Locks, Mode, RedisLocks};

/// The backend to use, or `None` with a printed reason.
fn connect(test: &str) -> Option<RedisLocks> {
    let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    match RedisLocks::connect(&url, 2, Duration::from_secs(5)) {
        Ok(locks) => {
            // Start from nothing, in case an earlier run left a holder behind under this name.
            let _ = locks.release_all(&resource(test));
            Some(locks)
        }
        Err(err) => {
            let reason = format!("no Redis at {url}: {err}");
            if skip_is_a_failure() {
                panic!("VOLUCER_E2E is set, so a skipped test is a failure: {reason}");
            }
            eprintln!("skipping: {reason}");
            None
        }
    }
}

fn skip_is_a_failure() -> bool {
    match std::env::var("VOLUCER_E2E") {
        Ok(value) => !value.is_empty() && value != "0",
        Err(_) => false,
    }
}

/// A resource name unique to one test **and to one run of it**.
///
/// The pid alone is not enough: a lease from an earlier run is still live for its TTL, and Windows reuses
/// process ids aggressively, so a second run of the same test would find the first one's lock still held.
/// That is not hypothetical ? it is how the server's cross-process lock test failed the first time it ran
/// as part of the whole suite, after passing on its own.
fn resource(test: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("test-{}-{test}-{nanos}", std::process::id())
}

/// A lease long enough that a slow machine cannot expire it mid-test.
fn long() -> Lease {
    Lease::for_duration(Duration::from_secs(60))
}

#[test]
fn an_exclusive_lock_excludes_and_is_released_by_its_own_id() {
    let Some(locks) = connect("exclusive") else {
        return;
    };
    let resource = resource("exclusive");

    let held = locks
        .try_acquire(&resource, "worker-1", Mode::Exclusive, long())
        .expect("nothing holds it")
        .expect("so the acquisition succeeds");
    assert_eq!(held.id, "worker-1");
    assert_eq!(held.mode, Mode::Exclusive);

    // Somebody else: contention, which is `None` rather than an error.
    assert_eq!(
        locks
            .try_acquire(&resource, "worker-2", Mode::Exclusive, long())
            .expect("contention is an ordinary answer"),
        None
    );

    // And from a **second connection**, which is the property a fake cannot check: the lock is in the
    // server, so it is visible to a client that has never talked to the first one.
    let other = RedisLocks::connect(
        &std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string()),
        1,
        Duration::from_secs(5),
    )
    .expect("a second pool");
    assert_eq!(
        other
            .try_acquire(&resource, "worker-3", Mode::Exclusive, long())
            .expect("contention is an ordinary answer"),
        None,
        "a second process must see the first one's lock"
    );
    assert!(other.holders(&resource).expect("holders").len() == 1);

    assert!(locks.release(&resource, "worker-1").expect("release"));
    assert!(
        !locks.release(&resource, "worker-1").expect("release again"),
        "a second release is an answer, not an error"
    );
    assert!(!locks.exists(&resource).expect("exists"));
}

#[test]
fn readers_share_and_a_writer_excludes_them_across_connections() {
    let Some(locks) = connect("shared") else {
        return;
    };
    let resource = resource("shared");

    for reader in ["reader-1", "reader-2"] {
        locks
            .try_acquire(&resource, reader, Mode::Shared, long())
            .expect("nothing holds it")
            .unwrap_or_else(|| panic!("{reader} should have shared it"));
    }
    assert_eq!(locks.holders(&resource).expect("holders").len(), 2);

    assert_eq!(
        locks
            .try_acquire(&resource, "writer", Mode::Exclusive, long())
            .expect("contention is an ordinary answer"),
        None,
        "two readers must keep a writer out"
    );

    locks.release_all(&resource).expect("release all");
    locks
        .try_acquire(&resource, "writer", Mode::Exclusive, long())
        .expect("nothing holds it")
        .expect("so the writer gets it");
    assert_eq!(
        locks
            .try_acquire(&resource, "reader-1", Mode::Shared, long())
            .expect("contention is an ordinary answer"),
        None,
        "an exclusive holder must keep a reader out"
    );
    locks.release_all(&resource).expect("release all");
}

#[test]
fn an_id_that_already_holds_the_resource_is_refused_rather_than_renewed() {
    let Some(locks) = connect("already-held") else {
        return;
    };
    let resource = resource("already-held");

    locks
        .try_acquire(&resource, "worker-1", Mode::Shared, long())
        .expect("nothing holds it")
        .expect("acquired");

    // A shared holder does not *conflict* with itself, so a driver that only asked about conflicts would
    // grant this and the id would hold two leases it cannot release separately. Only the already-held
    // question catches it, which is why the script asks both.
    let err = locks
        .try_acquire(&resource, "worker-1", Mode::Shared, long())
        .expect_err("a second acquisition with the same id is a mistake");
    assert!(
        matches!(err, volucer_lock::Error::AlreadyHeld { .. }),
        "{err}"
    );
    assert!(err.to_string().contains("update_lease"), "{err}");

    locks.release_all(&resource).expect("release all");
}

#[test]
fn a_lease_that_expires_frees_the_resource_without_anybody_releasing_it() {
    let Some(locks) = connect("expiry") else {
        return;
    };
    let resource = resource("expiry");

    locks
        .try_acquire(
            &resource,
            "worker-1",
            Mode::Exclusive,
            Lease::for_duration(Duration::from_millis(150)),
        )
        .expect("nothing holds it")
        .expect("acquired");

    // The property a TTL lock exists for: a worker that died holding it does not wedge the resource.
    assert!(
        locks.exists(&resource).expect("exists"),
        "and it is held before the lease runs out"
    );
    std::thread::sleep(Duration::from_millis(300));

    assert!(
        !locks.exists(&resource).expect("exists"),
        "the lease has run out, so nothing holds it — and this is decided by Redis's clock, not this \
         process's"
    );
    assert!(
        locks.holders(&resource).expect("holders").is_empty(),
        "the expired holder is not reported even though the hash may still carry its fields"
    );
    locks
        .try_acquire(&resource, "worker-2", Mode::Exclusive, long())
        .expect("nothing holds it")
        .expect("so a second worker takes it without waiting for anybody to release");
}

#[test]
fn a_lease_of_zero_is_held_until_released() {
    let Some(locks) = connect("forever") else {
        return;
    };
    let resource = resource("forever");

    let held = locks
        .try_acquire(&resource, "worker-1", Mode::Exclusive, Lease::Forever)
        .expect("nothing holds it")
        .expect("acquired");
    assert_eq!(
        held.remaining, None,
        "`ttl = 0` is held until released — the opposite of what a zero TTL means to a KV value"
    );

    let holders = locks.holders(&resource).expect("holders");
    assert_eq!(holders.len(), 1);
    assert_eq!(
        holders[0].remaining, None,
        "and the sentinel survives the round trip through a string hash field: {holders:?}"
    );

    locks.release_all(&resource).expect("release all");
}

#[test]
fn update_lease_replaces_the_deadline_rather_than_adding_to_it() {
    let Some(locks) = connect("update") else {
        return;
    };
    let resource = resource("update");

    locks
        .try_acquire(
            &resource,
            "worker-1",
            Mode::Exclusive,
            Lease::for_duration(Duration::from_secs(30)),
        )
        .expect("nothing holds it")
        .expect("acquired");

    assert!(locks
        .update_lease(
            &resource,
            "worker-1",
            Lease::for_duration(Duration::from_secs(2))
        )
        .expect("update"));

    let remaining = locks.holders(&resource).expect("holders")[0]
        .remaining
        .expect("a deadline");
    assert!(
        remaining <= Duration::from_secs(2),
        "the new deadline is two seconds from now, not thirty plus two: {remaining:?}"
    );
    assert!(
        remaining > Duration::from_millis(1500),
        "and it is a whole two seconds, not a fraction of one: {remaining:?}"
    );

    assert!(
        !locks
            .update_lease(&resource, "worker-2", long())
            .expect("update"),
        "an id that holds nothing gets `false`, which is the answer a caller must act on"
    );

    locks.release_all(&resource).expect("release all");
}

#[test]
fn a_waiting_acquisition_takes_the_resource_as_soon_as_the_holder_releases_it() {
    // Wrapped in a `Some` so a skipped test reads the same way as every other one here, rather than
    // `Arc::new` on a value that has to be unwrapped before the skip can be decided.
    let Some(locks) = connect("wait").map(std::sync::Arc::new) else {
        return;
    };
    let resource = resource("wait");

    locks
        .try_acquire(&resource, "worker-1", Mode::Exclusive, long())
        .expect("nothing holds it")
        .expect("acquired");

    let contender = std::sync::Arc::clone(&locks);
    let target = resource.clone();
    let waiting = std::thread::spawn(move || {
        contender
            .acquire(
                &target,
                "worker-2",
                Mode::Exclusive,
                long(),
                Duration::from_secs(5),
            )
            .expect("waiting is not an error")
    });

    // Long enough that the contender has made more than one attempt: the point is that it does not
    // succeed on the first one and does not give up.
    std::thread::sleep(Duration::from_millis(100));
    locks.release(&resource, "worker-1").expect("release");

    let acquired = waiting.join().expect("the contender thread must not panic");
    assert_eq!(
        acquired.map(|holder| holder.id),
        Some("worker-2".to_string()),
        "the contender must have acquired it as soon as it was free"
    );
    locks.release_all(&resource).expect("release all");
}

#[test]
fn release_all_takes_the_resource_from_everyone_and_says_whether_anybody_held_it() {
    let Some(locks) = connect("force") else {
        return;
    };
    let resource = resource("force");

    assert!(
        !locks.release_all(&resource).expect("release all"),
        "a resource nobody holds reports nothing released"
    );

    for reader in ["reader-1", "reader-2"] {
        locks
            .try_acquire(&resource, reader, Mode::Shared, long())
            .expect("nothing holds it")
            .expect("acquired");
    }
    assert_eq!(locks.holders(&resource).expect("holders").len(), 2);

    assert!(locks.release_all(&resource).expect("release all"));
    assert!(locks.holders(&resource).expect("holders").is_empty());
    assert!(
        !locks.release_all(&resource).expect("release all again"),
        "the second call answers `false` rather than pretending to have released something"
    );
}

#[test]
fn the_two_fields_of_a_holder_are_removed_together() {
    let Some(locks) = connect("fields") else {
        return;
    };
    let resource = resource("fields");

    locks
        .try_acquire(&resource, "worker-1", Mode::Exclusive, long())
        .expect("nothing holds it")
        .expect("acquired");
    locks.release(&resource, "worker-1").expect("release");

    // A release that removed the mode field and left the deadline behind would leave a hash whose only
    // entry is a deadline — and a *second* acquisition would then find `HLEN` non-zero forever, which is
    // a key nothing ever cleans up.
    let client = redis::Client::open(
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string()),
    )
    .expect("a client");
    let mut connection = client.get_connection().expect("a connection");
    let length: i64 = redis::cmd("HLEN")
        .arg(RedisLocks::key(&resource))
        .query(&mut connection)
        .expect("HLEN");
    assert_eq!(
        length, 0,
        "the script drops the key entirely when its last holder leaves"
    );

    // And the id is usable again, which is what a leftover field would have prevented.
    locks
        .try_acquire(&resource, "worker-1", Mode::Exclusive, long())
        .expect("nothing holds it")
        .expect("the same id may re-acquire once it has released");
    locks.release_all(&resource).expect("release all");
}
