//! Allocation counts for the response parser, pinned as a test.
//!
//! The parser used to allocate two `String`s per header — around twenty
//! allocations for a page that sends ten headers. Wall clock cannot show that: the
//! parse is a few microseconds either way, and the difference is inside the noise of
//! a 2 ms request. An allocation counter can, and this is it.
//!
//! Each test asserts an exact count, not an upper bound. A new allocation in this
//! code is a decision, and a test that says which one is the cheapest way to notice.
//!
//! The allocator here is why this lives in its own integration-test file: a
//! `#[global_allocator]` is per binary, and each integration-test file is its own
//! binary, so nothing else in the suite pays for the counting.
//!
//! The counters are **thread-local**, so these tests are correct under the default
//! parallel test harness: another test allocating on another thread is not counted,
//! and neither is the harness's own bookkeeping. That was not the first version, and
//! the failure it produced was instructive — a count of 23 where the answer is 1 is
//! someone else's twenty-two allocations.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use stokehold_fcgi::{
    build_params, parse_response, parse_response_headers, split_response_body, RequestInfo,
    Response, Routing,
};

thread_local! {
    /// Allocations made by this thread that have not been observed yet.
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
    /// Bytes allocated by this thread that have not been observed yet.
    static BYTES: Cell<usize> = const { Cell::new(0) };
}

/// Record one allocation of `size` bytes on the current thread.
///
/// `try_with` rather than `with`: this runs inside `alloc`, including during the
/// window where a thread's TLS is being destroyed, and panicking there would abort.
/// A count missed at thread teardown cannot affect a test.
fn record(size: usize) {
    let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
    let _ = BYTES.try_with(|bytes| bytes.set(bytes.get() + size));
}

/// A `System` allocator that counts calls.
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

/// Run `body` and report how many allocations it made on this thread, and of how
/// many bytes.
fn allocations(body: impl FnOnce()) -> (usize, usize) {
    // Zero first, so work that happened before the measurement cannot be counted, and
    // a panic inside `body` cannot leave a stale count behind for the next test.
    let _ = ALLOCATIONS.try_with(|count| count.set(0));
    let _ = BYTES.try_with(|bytes| bytes.set(0));
    body();
    let count = ALLOCATIONS.with(Cell::get);
    let bytes = BYTES.with(Cell::get);
    (count, bytes)
}

/// A realistic `STDOUT` payload: the shape azera's page has, with a `Status:`, a
/// `Content-Type`, a dozen ordinary headers, and a body far larger than the header
/// block.
fn realistic_stdout() -> Vec<u8> {
    let mut stdout = Vec::with_capacity(13 * 1024);
    stdout.extend_from_slice(b"Status: 200 OK\r\n");
    stdout.extend_from_slice(b"Content-Type: text/html; charset=UTF-8\r\n");
    for index in 0..12 {
        stdout.extend_from_slice(format!("X-Bench-Header-{index}: value-{index}\r\n").as_bytes());
    }
    stdout.extend_from_slice(b"\r\n");
    stdout.extend_from_slice(&vec![b'x'; 12 * 1024]);
    stdout
}

#[test]
fn parsing_a_response_allocates_the_header_list_and_nothing_else() {
    let stdout = realistic_stdout();

    let (count, bytes) = allocations(|| {
        let (status, headers, body) = parse_response(&stdout);
        // Touch everything so nothing can be optimised away.
        assert_eq!(status, 200);
        assert_eq!(headers.len(), 13);
        assert_eq!(body.len(), 12 * 1024);
    });

    // One allocation: the `Vec` of borrowed header pairs. Its capacity is
    // `head.len() / 16`, so the bytes counted are that vector's storage — not the
    // 12 KB body, which is never copied.
    assert_eq!(count, 1, "the parse should allocate once, for the header list");
    assert!(
        bytes < 1024,
        "the header list is a few hundred bytes, not the payload: {bytes}"
    );
}

#[test]
fn a_parsed_header_is_borrowed_from_the_payload() {
    // The point of the borrowed header list: no copy of the header text exists
    // anywhere after the parse. With `(String, String)` this would be at least twice
    // per header instead of once for the whole list.
    let stdout = realistic_stdout();
    let (count, _) = allocations(|| {
        let (_, headers, _) = parse_response(&stdout);
        for (name, _) in &headers {
            // Each name really is a slice of the payload: present verbatim in it.
            assert!(
                stdout.windows(name.len()).any(|w| w == name.as_bytes()),
                "{name} is not in the payload"
            );
        }
    });
    assert_eq!(count, 1);
}

#[test]
fn splitting_the_body_allocates_nothing_at_all() {
    let stdout = realistic_stdout();
    let (count, _) = allocations(|| {
        assert_eq!(split_response_body(&stdout).len(), 12 * 1024);
    });
    assert_eq!(count, 0);
}

#[test]
fn the_largest_body_still_allocates_once() {
    // The point of locating the header end on the bytes: the cost of the parse does
    // not scale with the body, and no part of the body is ever copied.
    let mut stdout = Vec::with_capacity(256 * 1024);
    stdout.extend_from_slice(b"Status: 200\r\nContent-Type: text/html\r\n\r\n");
    stdout.extend_from_slice(&vec![b'y'; 256 * 1024]);

    let (count, bytes) = allocations(|| {
        let (_, headers, body) = parse_response(&stdout);
        assert_eq!(headers.len(), 1);
        assert_eq!(body.len(), 256 * 1024);
    });
    assert_eq!(count, 1);
    assert!(bytes < 1024, "the body must never be copied: {bytes}");
}

#[test]
fn a_response_with_no_headers_allocates_nothing() {
    // `Vec::with_capacity(0)` does not allocate, and the body is a slice, so the
    // shortest possible response costs nothing to parse.
    let (count, _) = allocations(|| {
        let (status, headers, body) = parse_response(b"\r\n");
        assert_eq!(status, 200);
        assert!(headers.is_empty());
        assert!(body.is_empty());
    });
    assert_eq!(count, 0);
}

#[test]
fn a_header_that_is_not_ascii_costs_no_allocation() {
    // The non-ASCII header is skipped, and skipping must not itself allocate. This is
    // the case that used to pay for a lossy conversion of the whole payload.
    let (count, _) = allocations(|| {
        let (_, headers) = parse_response_headers(b"Status: 200\r\nX-\xFF: v\r\nX-A: b\r\n\r\n");
        assert_eq!(headers.len(), 1);
    });
    assert_eq!(count, 1);
}

#[test]
fn a_body_that_is_not_utf8_costs_no_allocation() {
    // A binary body that contains a `\r\n\r\n` of its own: neither the terminator
    // search nor the parse may copy it or be confused by it.
    let mut stdout = b"Status: 200\r\nContent-Type: application/octet-stream\r\n\r\n".to_vec();
    stdout.extend_from_slice(&[0xFF, 0xFE, 0x00, 0x0D, 0x0A, 0x0D, 0x0A, 0xC3, 0x28]);

    let (count, _) = allocations(|| {
        let (status, headers, body) = parse_response(&stdout);
        assert_eq!(status, 200);
        assert_eq!(headers.len(), 1);
        assert_eq!(body.len(), 9);
    });
    assert_eq!(count, 1);
}

#[test]
fn the_response_object_parses_allocating_the_same_one_time() {
    // `Response::parse` is the entry point the HTTP front-end uses, so its count must
    // be the same as the free function's, not one per header.
    let response = Response {
        stdout: realistic_stdout(),
        ..Response::default()
    };

    let (count, _) = allocations(|| {
        let (status, headers, body) = response.parse();
        assert_eq!(status, 200);
        assert_eq!(headers.len(), 13);
        assert_eq!(body.len(), 12 * 1024);
    });
    assert_eq!(count, 1);
}

#[test]
fn routing_a_path_allocates_only_the_script_strings() {
    // `route` runs on every request, in both listeners, before anything else. It used to
    // allocate thirteen times: the percent-decoded copy of the whole path (even when it
    // contained no `%` at all, which is nearly every request), a lowercase copy of the path
    // and of every configured forbidden extension, and the `SCRIPT_NAME`/`PATH_INFO` strings
    // for whichever answer it returned.
    //
    // What is left is the two `format!`s that become `SCRIPT_NAME` and `PATH_INFO`. Pinning
    // the count here makes the next step a decision rather than a guess.
    let root = std::env::temp_dir().join(format!("stokehold-route-alloc-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&root);
    std::fs::write(root.join("index.php"), "<?php").expect("write a fixture script");
    let routing = Routing {
        document_root: root.clone(),
        front_controller: Some("index.php".to_string()),
        ..Routing::default()
    };

    let (count, bytes) = allocations(|| {
        let route = stokehold_fcgi::route("/items", &routing);
        match route {
            stokehold_fcgi::Route::Script {
                script_name,
                path_info,
                ..
            } => {
                assert_eq!(script_name, "/index.php");
                assert_eq!(path_info, "/items");
            }
            other => panic!("expected the front controller, got {other:?}"),
        }
    });

    // Measured: six allocations. The categories are the owned path `PathBuf::join` has to
    // build, the `SCRIPT_NAME` and `PATH_INFO` strings, and the growth of `format!`'s
    // buffer — which is `std`'s business, so the count is recorded rather than asserted
    // exactly. The bound is what matters: it fails if routing ever goes back to copying the
    // path to decode it or lowercasing it to match an extension.
    println!("route(/items): {count} allocations, {bytes} bytes");
    assert!(
        count <= 6,
        "routing a path should be a borrowed decode, one joined path and two format!s, \
         not {count} allocations"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn matching_a_forbidden_extension_does_not_scale_with_the_configured_count() {
    // The slope is the assertion that actually catches the regression. The guard used to be
    // `decoded.to_ascii_lowercase().ends_with(&ext.to_ascii_lowercase())` inside `any`, so a
    // deployment with *n* forbidden extensions allocated *n* copies of the extension per
    // request, plus one copy of the whole path. Comparing bytes makes it constant.
    fn match_cost(extension_count: usize) -> usize {
        let root = std::env::temp_dir().join(format!(
            "stokehold-route-ext-{extension_count}-{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&root);
        let routing = Routing {
            document_root: root.clone(),
            front_controller: None,
            // The first entry is the one that matches, so the walk cannot shortcut.
            forbidden_extensions: (0..extension_count)
                .map(|index| {
                    if index == 0 {
                        ".htaccess".to_string()
                    } else {
                        format!(".forbidden-{index}")
                    }
                })
                .collect(),
        };

        let (count, _) = allocations(|| {
            assert_eq!(
                stokehold_fcgi::route("/.htAccess", &routing),
                stokehold_fcgi::Route::Forbidden,
                "matched case-insensitively"
            );
        });
        let _ = std::fs::remove_dir_all(&root);
        count
    }

    let one = match_cost(1);
    let twenty = match_cost(20);

    assert_eq!(
        one,
        0,
        "a forbidden match needs no allocation at all: the path is not copied and neither \
         is the extension"
    );
    assert_eq!(
        twenty, one,
        "nineteen more forbidden extensions must cost nothing per request"
    );
}

#[test]
fn a_path_with_no_escapes_is_not_copied_to_decode_it() {
    // The `Cow` that makes the common case free. Nearly every request path has no `%`, and
    // copying the whole path to discover that was an allocation per request.
    let (count, _) = allocations(|| {
        let decoded = stokehold_fcgi::percent_decode_cow("/items/42?page=1");
        assert!(matches!(decoded, std::borrow::Cow::Borrowed(_)));
    });
    assert_eq!(count, 0);

    // An escaped path still decodes exactly as before, and into a single buffer:
    // `String::from_utf8` takes the vector by value rather than copying out of it.
    let (count, _) = allocations(|| {
        let decoded = stokehold_fcgi::percent_decode_cow("/caf%C3%A9");
        assert_eq!(decoded, "/café");
    });
    assert_eq!(count, 1, "one buffer, reused by String::from_utf8");
}

#[test]
fn a_forbidden_extension_is_matched_without_lowercasing_the_path() {
    // The guard that used to allocate twice per request: a lowercase copy of the path and a
    // lowercase copy of every configured extension. Both are gone; what remains is the
    // matched `Route`, which carries no strings at all.
    let routing = stokehold_fcgi::Routing::default();
    let (count, bytes) = allocations(|| {
        assert_eq!(
            stokehold_fcgi::route("/.htaccess", &routing),
            stokehold_fcgi::Route::Forbidden
        );
    });
    assert_eq!(count, 0, "{bytes} bytes");
}

#[test]
fn neither_delegating_entry_point_allocates_on_its_own() {
    let stdout = realistic_stdout();

    let (body_count, _) = allocations(|| {
        assert_eq!(split_response_body(&stdout).len(), 12 * 1024);
    });
    assert_eq!(body_count, 0);

    let (headers_count, _) = allocations(|| {
        assert_eq!(parse_response_headers(&stdout).1.len(), 13);
    });
    assert_eq!(headers_count, 1);
}

#[test]
fn building_the_environment_allocates_about_twice_per_pair() {
    // `build_params` is the convenience entry point: it allocates the list and then each
    // name and value, which is two allocations per pair. This test states that cost
    // rather than hiding it, because it is the number the pool's own reusable-buffer
    // version has to beat — and, more importantly, it is the number that *changes* when
    // the name/value construction changes, which is what makes it a regression test.
    //
    // The pair count is deliberately not asserted: this measures the *slope*, so adding
    // a variable to the environment cannot fail the test spuriously.
    fn allocations_for(header_count: usize) -> usize {
        let root = std::env::temp_dir().join(format!("stokehold-alloc-{header_count}"));
        let _ = std::fs::create_dir_all(&root);
        std::fs::write(root.join("index.php"), "<?php").expect("write a fixture script");

        let mut info = RequestInfo {
            uri: "/items".to_string(),
            ..RequestInfo::default()
        };
        info.headers = (0..header_count)
            .map(|index| (format!("X-Header-{index}"), format!("value-{index}")))
            .collect();
        let routing = Routing {
            document_root: root.clone(),
            front_controller: Some("index.php".to_string()),
            ..Routing::default()
        };

        let (count, _) = allocations(|| {
            let params = build_params(&info, &routing).expect("params build");
            assert!(!params.is_empty());
        });
        let _ = std::fs::remove_dir_all(&root);
        count
    }

    let few = allocations_for(2);
    let many = allocations_for(40);

    // Thirty-eight more headers cost two allocations each, and no more than that: the
    // routing and the fixed variables must not add anything proportional to the count.
    // A regression here is exactly what a per-header `to_ascii_lowercase` or a collected
    // boundary `Vec` would look like — one extra allocation per header, not per pair.
    let expected_growth = (40 - 2) * 2;
    assert!(
        many <= few + expected_growth + 4,
        "environment cost grew by {} for 38 more headers, expected about {expected_growth}",
        many - few
    );
}

/// A two-direction transport double: writes into `out`, reads from `input`.
///
/// A single `Cursor` cannot be both, which the unit tests call out and which this test
/// learned the same way — writing advanced the read position, and the client reported
/// `UnsupportedVersion(120)` on its own bytes.
#[derive(Default)]
struct Loopback {
    out: Vec<u8>,
    input: std::io::Cursor<Vec<u8>>,
}

impl std::io::Read for Loopback {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        std::io::Read::read(&mut self.input, buf)
    }
}

impl std::io::Write for Loopback {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.out.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The bytes a backend sends for a response: `STDOUT` records then `END_REQUEST`.
fn framed_reply(stdout: &[u8]) -> Vec<u8> {
    use stokehold_fcgi::{EndRequest, Header, RecordType};
    let mut reply = Vec::new();
    for chunk in stdout.chunks(65535) {
        let header = Header::new(RecordType::Stdout, 1, chunk.len() as u16);
        reply.extend_from_slice(&header.encode());
        reply.extend_from_slice(chunk);
        reply.extend(std::iter::repeat_n(0u8, header.padding_length as usize));
    }
    let end = EndRequest::complete();
    let header = Header::new(RecordType::EndRequest, 1, end.encode().len() as u16);
    reply.extend_from_slice(&header.encode());
    reply.extend_from_slice(&end.encode());
    reply
}

#[test]
fn the_request_builder_reuses_its_buffers_across_requests() {
    // The measured claim is *comparative* rather than an absolute count, because the
    // absolute one is dominated by the response accumulating into a fresh `Vec` (a few
    // doublings of a 12 KB payload) and by the test double's own write buffer — neither
    // of which is what this is about. What would show up sharply is a per-header cost:
    // a `String` per name and per value. So run the same request with four headers and
    // with sixty-four, from a cold client each time, and require the two counts to be
    // the same.
    use stokehold_fcgi::{Client, Request};

    /// One request from a cold client, counting the allocations it costs.
    fn cold_request_allocations(header_count: usize) -> usize {
        let stdout = realistic_stdout();
        let mut client: Client<Loopback> = Client::from_stream(Loopback {
            out: Vec::new(),
            input: std::io::Cursor::new(framed_reply(&stdout)),
        });
        let request = Request {
            params: (0..header_count)
                .map(|index| {
                    (
                        format!("HTTP_X_HEADER_{index}").into_bytes(),
                        format!("value-number-{index}").into_bytes(),
                    )
                })
                .collect(),
            body: Vec::new(),
            keep_conn: true,
        };

        let (count, _) = allocations(|| {
            let response = client.request(&request).expect("the request");
            assert_eq!(response.stdout.len(), stdout.len());
        });
        count
    }

    let few = cold_request_allocations(4);
    let many = cold_request_allocations(64);

    // Sixty more headers would be a hundred and twenty more allocations if each name
    // and value were copied into its own `String`. The environment is instead encoded
    // into one reused buffer, so the growth is only the buffer's own doublings — a
    // handful, logarithmic in the environment size, not linear in the header count.
    assert!(
        many <= few + 8,
        "allocation count must not grow with the header count: {few} with 4 headers, \
         {many} with 64 (naive per-header copying would be ~120 more)"
    );
    // And it must be far below what one allocation per name/value would cost.
    assert!(
        many < 64,
        "{many} allocations for 64 headers is too close to one per header"
    );
}

/// Counts the calls a transport makes that become syscalls.
///
/// `Client::receive` reads one FastCGI record at a time — a header and then a content
/// payload — and each of those calls goes straight to the socket. On loopback TCP or a unix
/// socket that is one `read(2)` per field of every record, and the same shape of waste that
/// `stokehold-worker`'s channel had. The counts are thread-local so the parallel harness
/// cannot pollute them.
#[derive(Default)]
struct CountingLoopback {
    inner: Loopback,
}

thread_local! {
    static READS: Cell<usize> = const { Cell::new(0) };
    static WRITES: Cell<usize> = const { Cell::new(0) };
}

impl std::io::Read for CountingLoopback {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        READS.with(|count| count.set(count.get() + 1));
        std::io::Read::read(&mut self.inner, buf)
    }
}

impl std::io::Write for CountingLoopback {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        WRITES.with(|count| count.set(count.get() + 1));
        std::io::Write::write(&mut self.inner, buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(&mut self.inner)
    }
}

#[test]
fn reading_a_response_does_not_spend_a_syscall_per_record_field() {
    // A response is a `STDOUT` record and an `END_REQUEST` record, each framed as a header
    // followed by a content payload, and `read_record` asks for the two separately. Written
    // straight to the socket that is four reads for the smallest possible answer, plus one per
    // record beyond. The transport is read through a buffer, so the first read brings the whole
    // reply in and the remaining fields are slices of it.
    use stokehold_fcgi::{Client, Request};

    let stdout = b"Status: 200 OK\r\nContent-Type: text/html\r\n\r\nhello";
    let mut client: Client<CountingLoopback> = Client::from_stream(CountingLoopback {
        inner: Loopback {
            out: Vec::new(),
            input: std::io::Cursor::new(framed_reply(stdout)),
        },
    });
    let request = Request {
        params: vec![(b"REQUEST_METHOD".to_vec(), b"GET".to_vec())],
        body: Vec::new(),
        keep_conn: true,
    };

    let _ = READS.try_with(|count| count.set(0));
    let _ = WRITES.try_with(|count| count.set(0));
    let response = client.request(&request).expect("the request");
    assert_eq!(response.stdout, stdout);

    let reads = READS.with(Cell::get);
    let writes = WRITES.with(Cell::get);

    // The write side is already one call: the client assembles the whole request and issues
    // "one write, one flush". Pinned so it cannot regress into eight.
    assert_eq!(writes, 1, "the request is still written in one call");

    // The read side used to be four — two records × (header + content) — and each was a
    // `read(2)` the previous one could have answered. One fill covers the whole reply, and the
    // buffer stops at exactly this: a body too large for it costs a second fill per buffer,
    // which is the kernel's chunking rather than the framing being charged for again.
    assert_eq!(
        reads, 1,
        "a two-record reply costs {reads} reads; the whole reply arrives in one segment"
    );
}

#[test]
fn the_fastcgi_read_count_does_not_grow_with_the_number_of_records() {
    // The property that matters: reads must scale with the size of the answer, not with the
    // number of records it is split into. A 12 KB page sent as many small records — which is
    // what a FastCGI backend that flushes often produces — must not cost a read per record.
    use stokehold_fcgi::{Client, Request};

    fn reads_for_stdout(stdout: &[u8], records: usize) -> usize {
        let mut reply = Vec::new();
        for chunk in stdout.chunks(stdout.len().div_ceil(records)) {
            let header = stokehold_fcgi::Header::new(stokehold_fcgi::RecordType::Stdout, 1, chunk.len() as u16);
            reply.extend_from_slice(&header.encode());
            reply.extend_from_slice(chunk);
            reply.extend(std::iter::repeat_n(0u8, header.padding_length as usize));
        }
        let end = stokehold_fcgi::EndRequest::complete();
        let header = stokehold_fcgi::Header::new(
            stokehold_fcgi::RecordType::EndRequest,
            1,
            end.encode().len() as u16,
        );
        reply.extend_from_slice(&header.encode());
        reply.extend_from_slice(&end.encode());

        let mut client: Client<CountingLoopback> = Client::from_stream(CountingLoopback {
            inner: Loopback {
                out: Vec::new(),
                input: std::io::Cursor::new(reply),
            },
        });
        let request = Request {
            params: vec![(b"REQUEST_METHOD".to_vec(), b"GET".to_vec())],
            body: Vec::new(),
            keep_conn: true,
        };

        let _ = READS.try_with(|count| count.set(0));
        let response = client.request(&request).expect("the request");
        assert_eq!(response.stdout, stdout);
        READS.with(Cell::get)
    }

    let stdout = vec![b'x'; 12 * 1024];
    let few = reads_for_stdout(&stdout, 1);
    let many = reads_for_stdout(&stdout, 40);

    // Both are four pages of 4 KB and change nothing about how much has to cross the socket, so
    // both cost about the same. Unbuffered this would be 40 extra reads for the 40-record case:
    // two per record instead of one per fill.
    assert!(
        many <= few + 2,
        "reads grew from {few} to {many} for the same 12 KB split into 40 records instead of \
         one; reads must track the size of the answer, not the number of records"
    );
}
