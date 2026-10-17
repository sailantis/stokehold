//! What the exposition format actually looks like, byte for byte.
//!
//! Golden strings rather than field-by-field assertions, because the thing being tested *is* the
//! text. A test that checked `contains("grpc")` would pass against a line Prometheus cannot parse,
//! and the format's rules — sorted families, cumulative buckets, `+Inf` last, the three-character
//! escape set — are exactly the kind that look satisfied by an eyeball and are not.

use volucer_metrics::{Kind, Registry};

#[test]
fn a_counter_renders_with_its_help_and_type() {
    let registry = Registry::new();
    registry.describe("rr_http_request_total", Kind::Counter, "Requests served");
    registry.counter_add("rr_http_request_total", &[("status", "200")], 3);

    assert_eq!(
        registry.render(),
        "# HELP rr_http_request_total Requests served\n\
         # TYPE rr_http_request_total counter\n\
         rr_http_request_total{status=\"200\"} 3\n"
    );
}

/// The order a caller passes labels in must not change the output, or one metric becomes two and
/// the series cap is spent twice.
#[test]
fn labels_are_sorted_so_call_order_does_not_matter() {
    let registry = Registry::new();
    registry.describe("rr_x", Kind::Gauge, "A gauge");

    registry.gauge_set("rr_x", &[("beta", "2"), ("alpha", "1")], 1.0);
    registry.gauge_set("rr_x", &[("alpha", "1"), ("beta", "2")], 2.0);

    assert_eq!(
        registry.render(),
        "# HELP rr_x A gauge\n\
         # TYPE rr_x gauge\n\
         rr_x{alpha=\"1\",beta=\"2\"} 2\n",
        "one series, not two, and the second write wins"
    );
}

#[test]
fn a_gauge_with_no_labels_has_no_braces() {
    let registry = Registry::new();
    registry.describe("rr_http_workers_ready", Kind::Gauge, "Idle workers");
    registry.gauge_set("rr_http_workers_ready", &[], 4.0);

    assert_eq!(
        registry.render(),
        "# HELP rr_http_workers_ready Idle workers\n\
         # TYPE rr_http_workers_ready gauge\n\
         rr_http_workers_ready 4\n"
    );
}

/// The cumulative form: a bucket counts everything at or below its bound, so every line is a
/// running total and `+Inf` equals the count. A test that recorded one observation per bucket would
/// pass against a non-cumulative implementation, which is a histogram Prometheus reads as wrong for
/// every quantile.
#[test]
fn histogram_buckets_are_cumulative_and_plus_inf_equals_the_count() {
    let registry = Registry::new();
    registry.describe_histogram(
        "rr_http_request_duration_seconds",
        "Request duration",
        &[0.1, 0.5, 1.0],
    );

    // One observation in each of the first two buckets, and one above every bound.
    for value in [0.05, 0.2, 5.0] {
        let _ = registry.observe("rr_http_request_duration_seconds", &[], value);
    }

    assert_eq!(
        registry.render(),
        "# HELP rr_http_request_duration_seconds Request duration\n\
         # TYPE rr_http_request_duration_seconds histogram\n\
         rr_http_request_duration_seconds_bucket{le=\"0.1\"} 1\n\
         rr_http_request_duration_seconds_bucket{le=\"0.5\"} 2\n\
         rr_http_request_duration_seconds_bucket{le=\"1\"} 2\n\
         rr_http_request_duration_seconds_bucket{le=\"+Inf\"} 3\n\
         rr_http_request_duration_seconds_sum 5.25\n\
         rr_http_request_duration_seconds_count 3\n",
        "0.05 in the first bucket, 0.2 also in the 0.5 bucket, 5.0 only under +Inf"
    );
}

/// A value exactly on a bound lands in that bound's bucket, because `le` is *less than or equal*.
/// An implementation using `<` would put 0.5 in the 1.0 bucket, and nothing would say so.
#[test]
fn a_value_exactly_on_a_bound_is_in_that_bucket() {
    let registry = Registry::new();
    registry.describe_histogram("rr_h", "Bounds", &[0.5, 1.0]);
    let _ = registry.observe("rr_h", &[], 0.5);

    let rendered = registry.render();
    assert!(
        rendered.contains("rr_h_bucket{le=\"0.5\"} 1"),
        "0.5 belongs in le=0.5, not le=1:\n{rendered}"
    );
    assert!(
        rendered.contains("rr_h_bucket{le=\"1\"} 1"),
        "and therefore also in the cumulative le=1:\n{rendered}"
    );
}

#[test]
fn an_observation_above_every_bound_says_so() {
    let registry = Registry::new();
    registry.describe_histogram("rr_h", "Bounds", &[0.1]);

    assert!(registry.observe("rr_h", &[], 0.05), "inside the bound");
    assert!(
        !registry.observe("rr_h", &[], 9.0),
        "above every bound is counted by +Inf alone, and the caller is told"
    );
}

/// Only backslash, quote and newline are escapes — and a **tab is not one**, which is the assertion
/// that makes this a test of the format rather than of string escaping in general.
///
/// A JSON escaper would emit `\t` here. Prometheus would then read a literal backslash and a `t`,
/// so the label value on the graph would not be the path the application meant, and nothing would
/// report it: both strings are valid label values.
#[test]
fn label_values_escape_exactly_three_characters() {
    let registry = Registry::new();
    registry.describe("rr_x", Kind::Gauge, "A gauge");
    registry.gauge_set("rr_x", &[("path", "a\"b\\c\nd\te")], 1.0);

    let rendered = registry.render();
    // Built rather than written as a raw literal, because the expected value contains both a
    // literal backslash and an escaped one, and a mistake between them would be invisible in a
    // diff. Written out, this is: rr_x{path="a\"b\\c\nd<TAB>e"} 1
    let expected = "rr_x{path=\"a\\\"b\\\\c\\nd\te\"} 1";
    assert!(
        rendered.contains(expected),
        "backslash, quote and newline escaped; tab left alone.\n\
         expected a line containing: {expected}\n\
         got:\n{rendered}"
    );
}

/// A quote is *not* special in `# HELP`, so escaping it there would put a backslash in front of a
/// character Prometheus reads literally.
#[test]
fn help_text_escapes_no_quotes() {
    let registry = Registry::new();
    registry.describe("rr_x", Kind::Gauge, "the \"value\" of a\nthing");
    registry.gauge_set("rr_x", &[], 1.0);

    assert!(
        registry
            .render()
            .contains("# HELP rr_x the \"value\" of a\\nthing\n"),
        "quote untouched, newline escaped:\n{}",
        registry.render()
    );
}

#[test]
fn special_floats_are_spelled_the_way_prometheus_reads_them() {
    let registry = Registry::new();
    registry.describe("rr_inf", Kind::Gauge, "Positive infinity");
    registry.describe("rr_neg", Kind::Gauge, "Negative infinity");
    registry.describe("rr_nan", Kind::Gauge, "Not a number");
    registry.gauge_set("rr_inf", &[], f64::INFINITY);
    registry.gauge_set("rr_neg", &[], f64::NEG_INFINITY);
    registry.gauge_set("rr_nan", &[], f64::NAN);

    let rendered = registry.render();
    assert!(rendered.contains("rr_inf +Inf\n"), "{rendered}");
    assert!(rendered.contains("rr_neg -Inf\n"), "{rendered}");
    assert!(rendered.contains("rr_nan NaN\n"), "{rendered}");
}

/// A name nobody described is refused, because it has no `# TYPE` and would render as an untyped
/// bare line — which Prometheus accepts and a human cannot read.
#[test]
fn an_undeclared_metric_is_refused_rather_than_created() {
    let registry = Registry::new();
    registry.counter_add("rr_never_described", &[], 1);

    assert_eq!(registry.render(), "");
    assert_eq!(
        registry.refused_series(),
        vec![("rr_never_described:undeclared".to_string(), 1)],
        "and the refusal is countable, and says which kind it was"
    );
}

/// The point of the cap: a label drawn from a request path must not be able to make the registry
/// grow without bound.
#[test]
fn the_series_cap_is_enforced_and_counted() {
    let registry = Registry::with_series_cap(3);
    registry.describe("rr_bounded", Kind::Counter, "Bounded");

    for index in 0..10 {
        registry.counter_add("rr_bounded", &[("path", &format!("/{index}"))], 1);
    }

    assert_eq!(
        registry.names(),
        vec!["rr_bounded".to_string()],
        "the family exists"
    );
    let rendered = registry.render();
    assert_eq!(
        rendered.matches("rr_bounded{").count(),
        3,
        "exactly three series, the cap, and no more:\n{rendered}"
    );
    assert_eq!(
        registry.refused_series(),
        vec![("rr_bounded:at-cap".to_string(), 7)],
        "seven refused, and the reason is at-cap rather than undeclared"
    );
}

/// A family with no series is omitted rather than described, so `/metrics` does not advertise a
/// metric that does not exist.
#[test]
fn a_described_but_unused_metric_is_not_rendered() {
    let registry = Registry::new();
    registry.describe("rr_unused", Kind::Counter, "Nothing writes this");

    assert_eq!(registry.render(), "");
    assert_eq!(registry.names(), vec!["rr_unused".to_string()]);
}

/// Two scrapes of an unchanged process are byte-identical, which is what makes a diff of two
/// scrapes readable and a golden test possible.
#[test]
fn rendering_is_deterministic_and_sorted_by_family_name() {
    let registry = Registry::new();
    for name in ["rr_zeta", "rr_alpha", "rr_middle"] {
        registry.describe(name, Kind::Counter, "A counter");
        registry.counter_add(name, &[("b", "2"), ("a", "1")], 1);
    }

    let first = registry.render();
    assert_eq!(first, registry.render(), "the same registry, twice");

    let order: Vec<&str> = first
        .lines()
        .filter(|line| line.starts_with("# TYPE"))
        .collect();
    assert_eq!(
        order,
        vec![
            "# TYPE rr_alpha counter",
            "# TYPE rr_middle counter",
            "# TYPE rr_zeta counter",
        ],
        "families come out sorted, so an unchanged process diffs clean:\n{first}"
    );
}

/// A second registration with a conflicting kind is warned and ignored rather than replacing the
/// first, because replacing would drop the series that were already counting.
#[test]
fn a_conflicting_reregistration_does_not_replace_the_family() {
    let registry = Registry::new();
    registry.describe("rr_x", Kind::Counter, "A counter");
    registry.counter_add("rr_x", &[], 7);

    registry.describe("rr_x", Kind::Gauge, "Actually a gauge");

    assert!(
        registry.render().contains("rr_x 7\n"),
        "the counter and its value survive:\n{}",
        registry.render()
    );
    assert!(registry.render().contains("# TYPE rr_x counter\n"));
}

/// A metric used at the wrong kind is a call-site mistake, and it must not silently become a
/// metric that exists and never moves.
#[test]
fn using_a_counter_as_a_histogram_is_refused() {
    let registry = Registry::new();
    registry.describe("rr_x", Kind::Counter, "A counter");

    assert!(!registry.observe("rr_x", &[], 1.0));
    assert_eq!(
        registry.refused_series(),
        vec![("rr_x:wrong-kind".to_string(), 1)]
    );
}

/// Empty bounds are legal and mean every observation lands in `+Inf` — a caller who wants only a
/// sum and a count.
#[test]
fn a_histogram_with_no_bounds_counts_everything_in_plus_inf() {
    let registry = Registry::new();
    registry.describe_histogram("rr_only_sum", "Sum only", &[]);
    let _ = registry.observe("rr_only_sum", &[], 2.0);
    let _ = registry.observe("rr_only_sum", &[], 3.0);

    assert_eq!(
        registry.render(),
        "# HELP rr_only_sum Sum only\n\
         # TYPE rr_only_sum histogram\n\
         rr_only_sum_bucket{le=\"+Inf\"} 2\n\
         rr_only_sum_sum 5\n\
         rr_only_sum_count 2\n"
    );
}
