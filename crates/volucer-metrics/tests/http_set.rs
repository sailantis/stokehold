//! The `rr_http_*` set: that the names are the ones dashboards expect, and that the snapshot
//! arithmetic is right.
//!
//! The names are asserted literally. A test that read them back from the constants would pass
//! against a typo, and the whole point of these being RoadRunner's names is that a specific string
//! appears on a scrape — so the expected value here is the string an operator's dashboard queries.

use volucer_metrics::http::{self, PoolSnapshot, WorkerSnapshot};
use volucer_metrics::{Kind, Registry};

fn registry_with_the_set() -> Registry {
    let registry = Registry::new();
    http::describe(&registry);
    registry
}

/// The names, spelled out, because they are an interface with dashboards that already exist.
#[test]
fn the_metric_names_are_roadrunners() {
    let registry = registry_with_the_set();
    assert_eq!(
        registry.names(),
        vec![
            "rr_http_no_free_workers_total",
            "rr_http_request_duration_seconds",
            "rr_http_request_total",
            "rr_http_requests_queue",
            "rr_http_total_workers",
            "rr_http_uptime_seconds",
            "rr_http_worker_memory_bytes",
            "rr_http_worker_state",
            "rr_http_workers_invalid",
            "rr_http_workers_memory_bytes",
            "rr_http_workers_ready",
            "rr_http_workers_working",
        ]
    );
}

#[test]
fn describing_twice_is_a_no_op() {
    let registry = registry_with_the_set();
    let before = registry.names();
    http::describe(&registry);
    assert_eq!(before, registry.names());
}

#[test]
fn a_served_request_counts_and_times_itself_by_status() {
    let registry = registry_with_the_set();
    http::observe_request(&registry, 200, 0.012);
    http::observe_request(&registry, 200, 0.3);
    http::observe_request(&registry, 500, 0.9);

    let rendered = registry.render();
    assert!(
        rendered.contains("rr_http_request_total{status=\"200\"} 2"),
        "{rendered}"
    );
    assert!(
        rendered.contains("rr_http_request_total{status=\"500\"} 1"),
        "{rendered}"
    );
    assert!(
        rendered.contains("rr_http_request_duration_seconds_count{status=\"200\"} 2"),
        "{rendered}"
    );
    assert!(
        rendered.contains("rr_http_request_duration_seconds_count{status=\"500\"} 1"),
        "{rendered}"
    );
}

/// The state gauge is one series per worker *and state*, with a zero for the states a worker is
/// not in. A worker that stopped reporting would otherwise leave a gap rather than a zero, and a
/// stacked graph would silently shrink.
#[test]
fn worker_state_is_one_series_per_worker_and_state() {
    let registry = registry_with_the_set();
    http::set_pool(
        &registry,
        &PoolSnapshot {
            total_workers: 2,
            workers_ready: 1,
            workers_working: 1,
            workers_invalid: 0,
            workers: vec![
                WorkerSnapshot {
                    pid: 11,
                    state: http::state::READY,
                    memory_bytes: 1024,
                },
                WorkerSnapshot {
                    pid: 22,
                    state: http::state::WORKING,
                    memory_bytes: 2048,
                },
            ],
        },
    );

    let rendered = registry.render();
    assert!(
        rendered.contains("rr_http_worker_state{pid=\"11\",state=\"ready\"} 1"),
        "{rendered}"
    );
    assert!(
        rendered.contains("rr_http_worker_state{pid=\"11\",state=\"working\"} 0"),
        "the states a worker is not in are zero, not absent:\n{rendered}"
    );
    assert!(
        rendered.contains("rr_http_worker_state{pid=\"22\",state=\"working\"} 1"),
        "{rendered}"
    );
    assert!(
        rendered.contains("rr_http_worker_state{pid=\"22\",state=\"ready\"} 0"),
        "{rendered}"
    );
    assert_eq!(
        rendered.matches("rr_http_worker_state{").count(),
        8,
        "two workers times four states"
    );
}

#[test]
fn the_pool_gauges_and_the_memory_total_add_up() {
    let registry = registry_with_the_set();
    http::set_pool(
        &registry,
        &PoolSnapshot {
            total_workers: 3,
            workers_ready: 2,
            workers_working: 1,
            workers_invalid: 0,
            workers: vec![
                WorkerSnapshot {
                    pid: 1,
                    state: http::state::READY,
                    memory_bytes: 100,
                },
                WorkerSnapshot {
                    pid: 2,
                    state: http::state::READY,
                    memory_bytes: 200,
                },
                WorkerSnapshot {
                    pid: 3,
                    state: http::state::WORKING,
                    memory_bytes: 300,
                },
            ],
        },
    );

    let rendered = registry.render();
    assert!(rendered.contains("rr_http_total_workers 3"), "{rendered}");
    assert!(rendered.contains("rr_http_workers_ready 2"), "{rendered}");
    assert!(rendered.contains("rr_http_workers_working 1"), "{rendered}");
    assert!(
        rendered.contains("rr_http_workers_memory_bytes 600"),
        "the sum of the per-worker figures, not a separate number:\n{rendered}"
    );
    assert!(
        rendered.contains("rr_http_worker_memory_bytes{pid=\"3\"} 300"),
        "{rendered}"
    );
}

/// A later snapshot replaces the earlier one rather than accumulating, which is the whole
/// difference between a gauge and a counter and the mistake that makes a worker count climb for
/// ever.
#[test]
fn a_second_snapshot_replaces_the_first() {
    let registry = registry_with_the_set();
    let one = PoolSnapshot {
        total_workers: 1,
        workers_ready: 1,
        workers_working: 0,
        workers_invalid: 0,
        workers: vec![WorkerSnapshot {
            pid: 9,
            state: http::state::READY,
            memory_bytes: 10,
        }],
    };
    http::set_pool(&registry, &one);
    http::set_pool(
        &registry,
        &PoolSnapshot {
            total_workers: 4,
            workers_ready: 0,
            workers_working: 4,
            workers_invalid: 0,
            workers: Vec::new(),
        },
    );

    let rendered = registry.render();
    assert!(rendered.contains("rr_http_total_workers 4"), "{rendered}");
    assert!(rendered.contains("rr_http_workers_working 4"), "{rendered}");
    assert!(rendered.contains("rr_http_workers_ready 0"), "{rendered}");
}

/// A request that waited for a worker is counted, because "how often was the pool exhausted" is the
/// question a `workers_ready` graph cannot answer on its own.
#[test]
fn a_request_with_no_free_worker_is_counted() {
    let registry = registry_with_the_set();
    http::count_no_free_workers(&registry);
    http::count_no_free_workers(&registry);
    http::set_requests_queue(&registry, 7);
    http::set_uptime_seconds(&registry, 42.5);

    let rendered = registry.render();
    assert!(
        rendered.contains("rr_http_no_free_workers_total 2"),
        "{rendered}"
    );
    assert!(rendered.contains("rr_http_requests_queue 7"), "{rendered}");
    assert!(
        rendered.contains("rr_http_uptime_seconds 42.5"),
        "{rendered}"
    );
}

/// The duration ladder has to reach past the pool's own `exec_ttl`, or every request that timed out
/// lands in `+Inf` together with every other slow one and cannot be told from it.
#[test]
fn the_duration_ladder_reaches_past_a_plausible_timeout() {
    let last = http::DURATION_BUCKETS[http::DURATION_BUCKETS.len() - 1];
    assert!(
        last >= 60.0,
        "the largest bound is {last}, which a 30 s exec_ttl would overflow"
    );
}

/// The kinds are what the set declares, checked against a family that actually has a series —
/// because a family with none is omitted from the exposition entirely, so asserting on
/// `rr_http_total_workers` here would be testing the omission rather than the type.
#[test]
fn the_set_describes_its_kinds() {
    let registry = registry_with_the_set();
    http::observe_request(&registry, 200, 0.001);
    http::set_uptime_seconds(&registry, 1.0);
    let rendered = registry.render();

    assert!(
        rendered.contains("# TYPE rr_http_request_total counter\n"),
        "{rendered}"
    );
    assert!(
        rendered.contains("# TYPE rr_http_request_duration_seconds histogram\n"),
        "{rendered}"
    );
    assert!(
        rendered.contains("# TYPE rr_http_uptime_seconds gauge\n"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("rr_http_total_workers"),
        "a gauge nobody has written yet is not advertised; it appears on the first snapshot:\n{rendered}"
    );
}

/// Nothing in this set takes a label from the request, which is what keeps the registry's series
/// cap a backstop rather than a load-bearing limit. A path or a host in a label would make the
/// number of series a function of what clients ask for.
#[test]
fn no_metric_in_the_set_is_labelled_from_the_request() {
    let registry = registry_with_the_set();
    http::observe_request(&registry, 404, 0.001);
    http::set_pool(
        &registry,
        &PoolSnapshot {
            total_workers: 1,
            workers_ready: 1,
            workers_working: 0,
            workers_invalid: 0,
            workers: vec![WorkerSnapshot {
                pid: 5,
                state: http::state::READY,
                memory_bytes: 1,
            }],
        },
    );

    let rendered = registry.render();
    for forbidden in ["path", "uri", "host", "method", "site"] {
        assert!(
            !rendered.contains(&format!("{forbidden}=")),
            "a {forbidden} label would make the series count depend on the request:\n{rendered}"
        );
    }
    // And the two labels that *are* used are the closed ones.
    assert!(rendered.contains("status=\"404\""));
    assert!(rendered.contains("state=\"ready\""));
}

/// `Kind` is re-exported from the crate root and used by the set's `describe`, so a wrong variant
/// would be caught at compile time — this pins that the enum is public and complete.
#[test]
fn every_kind_the_set_uses_is_public() {
    assert_eq!(Kind::Counter.as_str(), "counter");
    assert_eq!(Kind::Gauge.as_str(), "gauge");
    assert_eq!(Kind::Histogram.as_str(), "histogram");
}
