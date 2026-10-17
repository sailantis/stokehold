//! The Redis driver, against a **real** Redis.
//!
//! **The whole file is behind the `redis` feature**, so a build without it has no test to skip and no
//! `RedisStore` to import. A file that compiled in both would need every test's body gated as well, which
//! is the same gate written many times and one of them eventually forgotten.
//!
//! # Why these are skipped rather than mocked
//!
//! A mock of the Redis protocol would test this crate's idea of Redis, which is the one thing that does
//! not need testing. Every property worth checking here is a property of the *server*: that `SCAN MATCH`
//! scopes to a prefix, that `PTTL` distinguishes "no key" from "no expiry", that `DEL` reports whether it
//! removed anything, that `SET ... PX` sets a deadline rather than only a value. A fake would answer all
//! of those however its author imagined Redis behaves.
//!
//! So these run against whatever is at `REDIS_URL` — `redis://127.0.0.1:6379` by default — and **skip with
//! a printed reason** when nothing is there, exactly as the worker-pool tests do for PHP. Set
//! `VOLUCER_E2E=1` where a skip should be a failure.
//!
//! # And why each test names its own storage
//!
//! These run against a server somebody may be using for something else, and a stray `clear` must not empty
//! it. Each test uses a storage name unique to itself, so `RedisStore::prefix_for` puts its keys where no
//! other test looks — and `clear`, which is scoped to that prefix, can then only remove its own.

#![cfg(feature = "redis")]

use std::time::Duration;

use volucer_kv::{RedisStore, Store, Ttl};
/// The server to use, or `None` with a printed reason.
fn connect(storage: &str) -> Option<RedisStore> {
    let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    match RedisStore::connect(&url, 2, storage) {
        Ok(store) => Some(store),
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

/// A storage name unique to one test, so its keys cannot collide with another's.
fn storage(test: &str) -> String {
    format!("test-{}-{test}", std::process::id())
}

#[test]
fn a_value_round_trips_through_a_real_server() {
    let Some(store) = connect(&storage("roundtrip")) else {
        return;
    };
    store.clear().expect("the storage starts empty");

    // Bytes, not text: the whole point of the wire carrying opaque bytes is that an application's
    // compressed or encrypted value survives, and Redis returns exactly what it was given.
    let value: Vec<u8> = (0..=255u8).collect();
    store.set(b"key", value.clone(), None).expect("stored");

    assert_eq!(store.get(b"key").expect("read"), Some(value));
    assert_eq!(store.get(b"absent").expect("read"), None);
}

#[test]
fn a_zero_ttl_deletes_and_a_positive_one_expires() {
    let Some(store) = connect(&storage("ttl")) else {
        return;
    };
    store.clear().expect("empty");

    // PSR-16's rule, and the two drivers must agree about it: an application that moves from `memory` to
    // `redis` without changing its code must not change behaviour.
    store.set(b"key", b"v".to_vec(), None).expect("stored");
    store
        .set(b"key", b"ignored".to_vec(), Some(Duration::ZERO))
        .expect("set with a zero TTL");
    assert_eq!(
        store.get(b"key").expect("read"),
        None,
        "a zero TTL must delete rather than store an immortal key"
    );

    store
        .set(b"soon", b"v".to_vec(), Some(Duration::from_millis(150)))
        .expect("stored");
    assert!(
        store.get(b"soon").expect("read").is_some(),
        "still there immediately after"
    );
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        store.get(b"soon").expect("read"),
        None,
        "and it is the *server* expiring it, not this crate pretending"
    );
}

#[test]
fn ttl_distinguishes_absent_from_never_expiring() {
    let Some(store) = connect(&storage("ttl-states")) else {
        return;
    };
    store.clear().expect("empty");

    assert_eq!(
        store.ttl(b"absent").expect("read"),
        Ttl::Missing,
        "Redis's -2 is reported as missing, not as a failure"
    );

    store.set(b"forever", b"v".to_vec(), None).expect("stored");
    assert_eq!(store.ttl(b"forever").expect("read"), Ttl::Forever);

    // Under a second, which is the case `TTL` would report as zero — indistinguishable from absent, and
    // the whole reason `PTTL` is used.
    store
        .set(b"briefly", b"v".to_vec(), Some(Duration::from_millis(600)))
        .expect("stored");
    match store.ttl(b"briefly").expect("read") {
        Ttl::In(left) => assert!(
            left > Duration::ZERO && left <= Duration::from_millis(600),
            "a sub-second TTL must be reported as a positive remainder: {left:?}"
        ),
        other => panic!("expected a remainder, got {other:?}"),
    }
}

#[test]
fn delete_reports_whether_anything_was_there() {
    let Some(store) = connect(&storage("delete")) else {
        return;
    };
    store.clear().expect("empty");

    store.set(b"key", b"v".to_vec(), None).expect("stored");
    assert!(store.delete(b"key").expect("delete"));
    assert!(
        !store.delete(b"key").expect("delete"),
        "DEL's count is what makes this answer possible"
    );
}

#[test]
fn a_batch_is_positional_and_missing_keys_keep_their_slot() {
    let Some(store) = connect(&storage("batch")) else {
        return;
    };
    store.clear().expect("empty");

    store.set(b"a", b"1".to_vec(), None).expect("stored");
    store.set(b"c", b"3".to_vec(), None).expect("stored");

    // `MGET` rather than the trait's default loop, and the property that makes a batch a batch: the answer
    // lines up with the keys, holes included.
    let got = store
        .get_many(&[b"a".as_slice(), b"b".as_slice(), b"c".as_slice()])
        .expect("read");
    assert_eq!(
        got,
        vec![Some(b"1".to_vec()), None, Some(b"3".to_vec())],
        "a missing key is a hole in its own position, not a shorter vector"
    );
}

#[test]
fn clear_is_scoped_to_one_storage_and_leaves_a_sibling_alone() {
    // The property the per-storage prefix exists for, and the one a `FLUSHDB` would fail. Two storages on
    // one server must not be able to empty each other, or a deployment with a session store and a fragment
    // cache would lose one whenever the other was cleared.
    let Some(sessions) = connect(&storage("scoped-sessions")) else {
        return;
    };
    let Some(cache) = connect(&storage("scoped-cache")) else {
        return;
    };
    sessions.clear().expect("empty");
    cache.clear().expect("empty");

    // The same key name in both, which is what makes this a test of the prefix rather than of the key.
    sessions
        .set(b"k", b"session".to_vec(), None)
        .expect("stored");
    cache.set(b"k", b"cache".to_vec(), None).expect("stored");

    assert_eq!(sessions.get(b"k").expect("read"), Some(b"session".to_vec()));
    assert_eq!(cache.get(b"k").expect("read"), Some(b"cache".to_vec()));

    sessions.clear().expect("cleared");

    assert_eq!(
        sessions.get(b"k").expect("read"),
        None,
        "clearing one storage must empty it"
    );
    assert_eq!(
        cache.get(b"k").expect("read"),
        Some(b"cache".to_vec()),
        "and must not touch the other, which holds the same key name"
    );

    cache.clear().expect("tidy up");
}

#[test]
fn clear_removes_more_than_one_scan_batch() {
    // `SCAN` returns a cursor, and the loop exists because one call does not return the whole keyspace. A
    // `clear` that made a single call would look correct on a small test and leave keys behind in
    // production, where the keyspace is far larger than the `COUNT` hint — so this writes more keys than the
    // driver's own batch size and checks that none survive.
    let Some(store) = connect(&storage("clear-many")) else {
        return;
    };
    store.clear().expect("empty");

    // 1200, above the driver's `SCAN_BATCH` of 1000: the second round trip is what is under test.
    for index in 0..1200u32 {
        store
            .set(format!("key{index}").as_bytes(), b"v".to_vec(), None)
            .expect("stored");
    }

    store.clear().expect("cleared");

    for index in [0u32, 999, 1000, 1199] {
        assert_eq!(
            store.get(format!("key{index}").as_bytes()).expect("read"),
            None,
            "key{index} should have been removed by the clear"
        );
    }
}

#[test]
fn an_unreachable_server_is_a_connect_failure_rather_than_a_later_surprise() {
    // The startup rule the worker pool also follows: a deployment's misconfiguration belongs where the
    // configuration is read, not on the first request that happens to read a cache.
    //
    // A port that cannot have a Redis on it rather than a plausible one, so the test cannot pass by finding
    // somebody else's server.
    let err = RedisStore::connect("redis://127.0.0.1:1", 1, "unreachable")
        .expect_err("port 1 is not a Redis");
    let message = err.to_string();
    assert!(message.contains("redis"), "{message}");
    assert!(
        !message.contains("127.0.0.1:1"),
        "the URL must not be echoed, because it may carry a password: {message}"
    );
}

#[test]
fn a_url_that_is_not_a_url_is_refused_by_name() {
    let err = RedisStore::connect("this is not a URL", 1, "bad-url").expect_err("must be refused");
    assert!(
        err.to_string().contains("not usable"),
        "the message should say the URL is the problem: {err}"
    );
}
