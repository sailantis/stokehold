//! What translating one HTTP request into a CGI environment costs, measured rather than
//! inferred — and the measurement says **do not do the work**.
//!
//! The number this file exists to establish is `to_cgi`'s **own**, as distinct from the cost of
//! the `PARAMS` build that follows it. Those were conflated in an earlier note — the existing
//! `volucer-server/tests/request_header_allocations.rs` measures `request_header` and
//! `borrowed_request`, and the figure quoted for "the translation" there was really the *worker
//! request frame*, a different layer. The FastCGI path pays `to_cgi` and never pays that frame,
//! so its cost had never been measured at all.
//!
//! It is now. `RequestInfo` owns nine `String`s plus two per request header, all of them copies
//! of bytes still alive in the `Parts` on the ground, and the plan was to make it generic over a
//! lifetime with `Cow` fields so they could borrow. Measured on a Ryzen-class host, release
//! build, 200,000 iterations, across runs:
//!
//! | request headers | `RequestInfo` | environment build | `to_cgi` total |
//! |---|---|---|---|
//! | 5 | 663–714 ns | 2,196–2,342 ns | 2,900–3,900 ns |
//! | 15 | 1,297–1,449 ns | 2,331–2,407 ns | 4,200–4,900 ns |
//!
//! Read the columns, not the rows. `RequestInfo` grows by **~63–74 ns per request header** — four
//! allocations at roughly 16 ns — and is **23–34%** of the translation. The environment build,
//! which is where the *other* two allocations per header go, is flat and three to four times
//! larger.
//!
//! So the borrow work buys roughly **700 ns on a fifteen-header request**, out of a `GET /items`
//! that measures **617 µs** on the worker arm and ~362 µs on the FastCGI arms. That is **0.1%**,
//! for a change that makes `RequestInfo` generic, touches `build_params`/`build_params_for` and
//! every caller of both, and turns a struct of owned values into one that carries a lifetime
//! through the environment builder. The allocations are real and the count is honestly four per
//! header; the *time* is not there.
//!
//! Recorded rather than done, for the same reason hazard 41 recorded the checksum option: the
//! evidence is about the framing path, and no throughput number is claimed.
//!
//! The tests below therefore pin **the measurement and the reasoning**, not the shape of an
//! optimisation that is not going to happen.
//!
//! Own test binary, like every other `allocations.rs`: a `#[global_allocator]` is per binary.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::PathBuf;

use hyper::Request;
use volucer_fcgi::cgi::{build_params_for, route};
use volucer_fcgi::{RequestInfo, Routing};
use volucer_http::to_cgi;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
    static BYTES: Cell<usize> = const { Cell::new(0) };
}

fn record(size: usize) {
    let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
    let _ = BYTES.try_with(|bytes| bytes.set(bytes.get() + size));
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn allocations(body: impl FnOnce()) -> (usize, usize) {
    let _ = ALLOCATIONS.try_with(|count| count.set(0));
    let _ = BYTES.try_with(|bytes| bytes.set(0));
    body();
    (ALLOCATIONS.with(Cell::get), BYTES.with(Cell::get))
}

/// A request as a front-end hands it over, with `header_count` ordinary headers on it.
fn parts(header_count: usize) -> hyper::http::request::Parts {
    let mut builder = Request::builder()
        .method("GET")
        .uri("/items?page=2")
        .header("Host", "example.test")
        .header("Accept", "text/html")
        .header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64)")
        .header("Accept-Encoding", "gzip, deflate")
        .header("Connection", "keep-alive");

    for index in 0..header_count {
        builder = builder.header(format!("X-Request-{index}"), "value");
    }

    builder.body(()).expect("a valid request").into_parts().0
}

fn routing() -> Routing {
    Routing {
        document_root: PathBuf::from("."),
        front_controller: Some("index.php".to_string()),
        ..Routing::default()
    }
}

/// One translation, with the environment build included: what a request costs a FastCGI backend.
fn to_cgi_cost(header_count: usize) -> (usize, usize) {
    let parts = parts(header_count);
    let routing = routing();
    allocations(|| {
        let request = to_cgi(&parts, Vec::new(), &routing, 8080, "10.0.0.5").expect("routes");
        std::hint::black_box(&request);
    })
}

/// The same, with the body preallocated so the measurement is the translation.
fn to_cgi_with_body(header_count: usize, body: Vec<u8>) -> (usize, usize) {
    let parts = parts(header_count);
    let routing = routing();
    allocations(move || {
        let request = to_cgi(&parts, body, &routing, 8080, "10.0.0.5").expect("routes");
        std::hint::black_box(&request);
    })
}

/// The five headers `parts()` always carries.
const BROWSER_HEADERS: usize = 5;

/// A realistic full set: what a browser or an API client actually sends.
const REALISTIC_HEADERS: usize = 10;

#[test]
fn the_wall_clock_cost_of_a_realistic_request_is_reported_and_loosely_bounded() {
    // Five headers is a curl. A browser or an API client sends ten to fifteen, and this is where
    // the per-header cost is actually paid, so the realistic figure is the one worth reporting.
    //
    // The threshold is deliberately loose because **wall clock is a debug-build number here**: an
    // unoptimised `to_cgi` is roughly ten times the release figure, and a tight bound would make
    // `cargo test` fail on a build nobody benchmarks. Assert the bound that catches a real
    // regression — a translation that starts taking microseconds per *header* — and print the
    // figure, which is the part that is evidence. Run with `--release` for the number to compare
    // against the module documentation.
    let parts = parts(REALISTIC_HEADERS);
    let routing = routing();
    for _ in 0..200 {
        std::hint::black_box(
            to_cgi(&parts, Vec::new(), &routing, 8080, "10.0.0.5").expect("routes"),
        );
    }

    const ITERATIONS: u32 = 20_000;
    let started = std::time::Instant::now();
    for _ in 0..ITERATIONS {
        std::hint::black_box(
            to_cgi(&parts, Vec::new(), &routing, 8080, "10.0.0.5").expect("routes"),
        );
    }
    let each = started.elapsed().as_nanos() as f64 / f64::from(ITERATIONS);

    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    println!(
        "to_cgi ({profile}): {each:.0} ns per translation for {} headers, empty body",
        BROWSER_HEADERS + REALISTIC_HEADERS
    );

    assert!(
        each < 500_000.0,
        "a translation taking {each:.0} ns in the {profile} profile would be worth rewriting on \
         its own; the release figure is a few microseconds"
    );
}

#[test]
fn translating_a_request_costs_four_allocations_per_header() {
    // Measured, not guessed. With five headers a translation costs 64 allocations and 2,375
    // bytes; the numbers move by four allocations per request header:
    //
    //   request headers   allocations   bytes
    //   5                  64           2,375
    //   15                106           3,897
    //   25                148           8,895
    //
    // Four per header, and it is worth naming where they go, because none of them is necessary:
    // two are `RequestInfo::{headers}`'s `String`s (`name.to_string()` and
    // `String::from_utf8_lossy(value.as_bytes()).into_owned()`), and two are the `PARAMS` pair the
    // environment build makes *from those Strings*. So the header is copied twice, into two
    // different owned pairs, before anything is written to the wire.
    //
    // The per-header figure is taken across a wide span so that `Vec` doublings — which are real
    // but happen once per power of two, not once per header — average out rather than landing on
    // whichever count the test happens to use.
    let (few, few_bytes) = to_cgi_cost(0);
    let (many, many_bytes) = to_cgi_cost(100);

    let per_header = (many - few) as f64 / 100.0;
    let bytes_per_header = (many_bytes - few_bytes) / 100;

    println!(
        "to_cgi: {few} allocations / {few_bytes} bytes with {BROWSER_HEADERS} headers, \
         {many} / {many_bytes} with 105; {per_header:.2} allocations and about \
         {bytes_per_header} bytes per extra header"
    );

    assert!(
        (3.9..=4.3).contains(&per_header),
        "four allocations per request header is the measured cost — two owned `String`s and the \
         two `Vec<u8>`s of the PARAMS pair built from them; got {per_header:.2}. If this moved, \
         either a header stopped being copied or the environment build changed shape"
    );
    assert!(
        bytes_per_header > 32,
        "a header's own bytes are copied into four places, so more than thirty-two bytes per \
         header is expected; got {bytes_per_header}"
    );
}

#[test]
fn the_fixed_cost_of_a_translation_is_the_environment_and_nothing_hidden() {
    // With no extra request headers, the translation is the fixed set: the nine `String`s of
    // `RequestInfo` (`method`, `uri`, `query_string`, `protocol`, `remote_addr`, `server_name`,
    // and the `Vec` of headers), the request's `headers` vector, and the `PARAMS` written for
    // `SERVER_*`, `SCRIPT_*` and the rest. Pinning the total is what makes a regression in any
    // one of them visible; the per-header test above cannot see a fixed cost that changed.
    let (none, none_bytes) = to_cgi_cost(0);

    println!("to_cgi fixed cost: {none} allocations / {none_bytes} bytes");

    assert!(
        (55..=70).contains(&none),
        "the fixed cost was measured at 64 allocations for a browser-shaped request with five \
         headers; got {none}. A fixed cost that grew is the interesting failure — it is paid by \
         every request regardless of how many headers arrive"
    );
    assert!(
        (2000..=2800).contains(&none_bytes),
        "and about 2,375 bytes; got {none_bytes}"
    );
}

#[test]
fn building_request_info_is_a_small_share_of_the_translation() {
    // The number that decides the borrow work, and it says no. See the module documentation for
    // the table; the assertion here pins the *ratio*, which is the part that does not move between
    // hosts, rather than the nanoseconds, which do.
    //
    // `RequestInfo`'s growth is real — four allocations per header — but it is charged against an
    // environment build that is flat, three times larger, and itself four allocations per header.
    // Borrowing the nine `String`s removes roughly a third of the translation and about 0.1% of a
    // request.
    let routing = routing();
    let route = route("/items", &routing);

    let info = |parts: &hyper::http::request::Parts| RequestInfo {
        method: parts.method.as_str().to_string(),
        uri: parts
            .uri
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/")
            .to_string(),
        query_string: parts.uri.query().unwrap_or("").to_string(),
        protocol: "HTTP/1.1".to_string(),
        headers: parts
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect(),
        remote_addr: "10.0.0.5".to_string(),
        server_name: "example.test".to_string(),
        server_port: 8080,
        body_len: 0,
        https: false,
    };

    const ITERATIONS: u32 = if cfg!(debug_assertions) {
        20_000
    } else {
        200_000
    };

    let measure = |extra: usize| -> (f64, f64, f64) {
        let parts = parts(extra);
        for _ in 0..200 {
            std::hint::black_box(
                build_params_for(&info(&parts), &routing, &route).expect("params"),
            );
        }

        let started = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            std::hint::black_box(info(&parts));
        }
        let info_ns = started.elapsed().as_nanos() as f64 / f64::from(ITERATIONS);

        let prebuilt = info(&parts);
        let started = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            std::hint::black_box(build_params_for(&prebuilt, &routing, &route).expect("params"));
        }
        let env_ns = started.elapsed().as_nanos() as f64 / f64::from(ITERATIONS);

        let started = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            std::hint::black_box(
                to_cgi(&parts, Vec::new(), &routing, 8080, "10.0.0.5").expect("routes"),
            );
        }
        let total_ns = started.elapsed().as_nanos() as f64 / f64::from(ITERATIONS);

        (info_ns, env_ns, total_ns)
    };

    let (info_few, env_few, total_few) = measure(0);
    let (info_many, env_many, total_many) = measure(REALISTIC_HEADERS);
    let few = BROWSER_HEADERS;
    let many = BROWSER_HEADERS + REALISTIC_HEADERS;

    let per_header = (info_many - info_few) / REALISTIC_HEADERS as f64;
    println!(
        "to_cgi, {few} headers: {info_few:.0} ns RequestInfo ({:.0}%), {env_few:.0} ns env, \
         {total_few:.0} ns total",
        100.0 * info_few / total_few
    );
    println!(
        "to_cgi, {many} headers: {info_many:.0} ns RequestInfo ({:.0}%), {env_many:.0} ns env, \
         {total_many:.0} ns total",
        100.0 * info_many / total_many
    );
    println!("RequestInfo grows by {per_header:.0} ns per header, four allocations");

    assert!(
        info_few + env_few <= total_few + 500.0 && info_many + env_many <= total_many + 500.0,
        "the two halves must not exceed the whole by more than noise"
    );

    // A sanity floor on the measurement itself: if these were near zero the optimiser would have
    // elided the loops, and a figure of zero is not a measurement. `black_box` is outside the
    // allocation counter, so the counts above and the times here agree only by construction.
    assert!(
        info_few > 50.0 && env_few > 500.0 && total_few > 1_000.0,
        "suspiciously fast: RequestInfo {info_few:.0} ns, env {env_few:.0} ns, total \
         {total_few:.0} ns"
    );

    // The claim: `RequestInfo` is a minority of the translation, so making it generic buys a
    // third of a figure that is itself a rounding error on a request. If this ever exceeds half,
    // the borrow work has become the obvious next step and this test is the thing that will say
    // so.
    //
    // Only asserted in a release build: an unoptimised build changes the relative weight of the
    // two halves — `to_string()` inlines nothing, and the environment's `Vec` work is relatively
    // cheaper — so the *ratio* is a release-build quantity even where the allocation counts are
    // not. The figure is printed either way.
    if !cfg!(debug_assertions) {
        assert!(
            info_many / total_many < 0.5,
            "`RequestInfo` is {:.0}% of the translation at {many} headers; at that point borrowing \
             its fields is no longer a rounding error and the reasoning in this file needs \
             revisiting",
            100.0 * info_many / total_many
        );
    }

    // And the absolute one, which is what makes it a decision rather than a preference: the whole
    // translation is a few microseconds against a request measured in hundreds. Loose, for the
    // same debug-build reason.
    assert!(
        total_many < 500_000.0,
        "the whole translation measured {total_many:.0} ns, which would be worth rewriting on \
         its own"
    );
}

#[test]
fn the_body_is_moved_not_copied() {
    // The body is taken by value, so a 64 KiB upload must not cost a copy here. Worth pinning:
    // if it ever became `&[u8]` and was cloned, that would be 64 KiB per request that the
    // per-header measurement would attribute to the translation.
    const BODY: usize = 64 * 1024;

    let (_none, none_bytes) = to_cgi_cost(0);
    // The buffer is allocated *outside* the measurement, so the translation is not charged for
    // it; a copy here would show up as the difference.
    let body = vec![b'x'; BODY];
    let (count, bytes) = to_cgi_with_body(0, body);

    assert!(
        bytes < none_bytes + 4096,
        "the body's own buffer is moved, so the translation must not copy it: {bytes} bytes \
         against {none_bytes} with no body at all"
    );
    assert!(
        count >= 55,
        "and it must still do the translation's own work; got {count} allocations"
    );
}
