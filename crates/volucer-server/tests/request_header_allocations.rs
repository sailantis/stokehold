//! Allocation counts for the CGI-to-worker translation, pinned as a test.
//!
//! `request_header` is the last piece of the request path that rewrote the whole request: it
//! reads the CGI environment back out of the `PARAMS` pairs and builds an owned `RequestHeader`
//! — five `String`s, two per header, and a `Vec` for the pairs — which the codec then borrows
//! and drops. `borrowed_request` is the same translation with nothing owned: every field is a
//! slice of the environment, and the one value that is not (the `HTTP_ACCEPT_ENCODING` →
//! `Accept-Encoding` name) is rendered into a stack buffer while the JSON is being written.
//!
//! The other two allocation files say what the request costs *after* this point —
//! `volucer-fcgi`'s covers the environment build, `volucer-worker`'s covers the wire — so
//! this one closes the gap.
//!
//! Both paths are measured here, through the real channel over a `Cursor`, so the comparison is
//! two implementations of one request on one host rather than two builds of the server.
//!
//! Own test binary, like every other `allocations.rs`: a `#[global_allocator]` is per binary.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use volucer_fcgi::{NameValuePairs, Request};
use volucer_server::worker_pool::{borrowed_request, request_header};
use volucer_worker::{Codec, Header, Json, MessageType, RequestEnd, ResponseHead, WorkerChannel};

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

/// The environment a real request arrives with.
fn params(header_count: usize) -> NameValuePairs {
    let mut pairs: NameValuePairs = Vec::with_capacity(32);
    let mut push = |name: &str, value: &str| {
        pairs.push((name.as_bytes().to_vec(), value.as_bytes().to_vec()));
    };

    push("GATEWAY_INTERFACE", "FastCGI/1.0");
    push("REQUEST_METHOD", "GET");
    push("REQUEST_URI", "/items");
    push("QUERY_STRING", "");
    push("SERVER_PROTOCOL", "HTTP/1.1");
    push("SERVER_SOFTWARE", "volucer/0.1.0");
    push("SERVER_NAME", "localhost");
    push("SERVER_PORT", "80");
    push("REMOTE_ADDR", "127.0.0.1");
    push("DOCUMENT_ROOT", "/srv/app/public");
    push("SCRIPT_FILENAME", "/srv/app/public/index.php");
    push("SCRIPT_NAME", "/index.php");
    push("REDIRECT_STATUS", "200");
    push("HTTP_HOST", "localhost");
    push("HTTP_ACCEPT", "text/html");
    push("HTTP_ACCEPT_ENCODING", "gzip, deflate");
    push("HTTP_CONNECTION", "keep-alive");
    for index in 0..header_count {
        push(&format!("HTTP_X_HEADER_{index}"), "value");
    }

    pairs
}

/// The bytes a worker sends back: a head, a body, and the end.
fn reply(request_id: u32) -> Vec<u8> {
    let mut out = Vec::new();
    push_frame(
        &mut out,
        MessageType::Response,
        request_id,
        &Json::encode(&ResponseHead {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
            body_len: 5,
        })
        .unwrap(),
    );

    // The last body frame is flagged, which the channel requires: the flag is the second
    // check that catches a desynchronised stream, `body_len` being the first.
    let mut body_header =
        Header::new(MessageType::Body, request_id, b"hello").expect("frame header");
    body_header.flags |= volucer_worker::flags::LAST;
    out.extend_from_slice(&body_header.encode());
    out.extend_from_slice(b"hello");

    push_frame(
        &mut out,
        MessageType::End,
        request_id,
        &Json::encode(&RequestEnd::default()).unwrap(),
    );
    out
}

/// Append one frame: header, then payload.
fn push_frame(out: &mut Vec<u8>, message_type: MessageType, request_id: u32, payload: &[u8]) {
    let header = Header::new(message_type, request_id, payload).expect("frame header");
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(payload);
}

/// One request over the real channel, with the owned translation and an owned header.
fn owned_cost(header_count: usize) -> (usize, usize) {
    let request = Request {
        params: params(header_count),
        body: Vec::new(),
        keep_conn: true,
    };
    let mut channel = WorkerChannel::new(Vec::new(), std::io::Cursor::new(reply(1)));

    allocations(|| {
        let header = request_header(&request);
        let response = channel
            .request(&header, &request.body)
            .expect("a canned reply");
        assert_eq!(response.body, b"hello");
    })
}

/// The same request, with the borrowed translation and no owned header at all.
fn borrowed_cost(header_count: usize) -> (usize, usize) {
    let request = Request {
        params: params(header_count),
        body: Vec::new(),
        keep_conn: true,
    };
    let mut channel = WorkerChannel::new(Vec::new(), std::io::Cursor::new(reply(1)));

    allocations(|| {
        // Exactly what the pool does: read the head out of the environment, encode it, and send
        // the bytes. The encode is the single buffer the head now costs.
        let head = Json::encode(&borrowed_request(&request).draft()).expect("encode");
        let response = channel
            .request_encoded(head, &request.body, |_| {})
            .expect("a canned reply");
        assert_eq!(response.body, b"hello");
    })
}

#[test]
fn the_borrowed_translation_does_not_grow_with_the_header_count() {
    // The shape, pinned. Measured on one host, one binary, both paths driven through the real
    // channel with a canned reply, so the number is the whole request:
    //
    //   headers   owned   borrowed
    //   0         28      12
    //   10        49      13
    //   20        70      14
    //
    // The owned translation costs two allocations per request header — the name `String` and the
    // value `String` — plus a `Vec` doubling, which is the 21 asserted below for ten more headers.
    // The borrowed one costs **none**: its only growth is the head's JSON buffer crossing a power
    // of two, so ten more headers buy one allocation and twenty buy two.
    let (owned_few, _) = owned_cost(10);
    let (owned_many, _) = owned_cost(20);
    let (borrowed_few, _) = borrowed_cost(10);
    let (borrowed_many, _) = borrowed_cost(20);

    assert_eq!(
        owned_many - owned_few,
        21,
        "the owned translation should cost two allocations per header plus a vector doubling; if \
         this moved, either a header stopped costing two or the pairs vector changed shape"
    );
    assert!(
        borrowed_many - borrowed_few <= 2,
        "the borrowed translation grew by {} for ten more headers; it must not scale with the \
         header count at all — a `String` per header is what it exists to remove",
        borrowed_many - borrowed_few
    );
    assert!(
        borrowed_many <= 15,
        "a whole borrowed request costs {borrowed_many} allocations for twenty headers, including \
         the response decode; it was 14"
    );
    assert!(
        owned_many - borrowed_many >= 50,
        "the borrowed path should be at least fifty allocations cheaper per request at twenty \
         headers; owned {owned_many}, borrowed {borrowed_many}"
    );
}

/// The translation itself, without the channel around it: what one request's head costs.
fn translation_only_owned(header_count: usize) -> usize {
    let request = Request {
        params: params(header_count),
        body: Vec::new(),
        keep_conn: true,
    };
    allocations(|| {
        std::hint::black_box(request_header(&request));
    })
    .0
}

fn translation_only_borrowed(header_count: usize) -> usize {
    let request = Request {
        params: params(header_count),
        body: Vec::new(),
        keep_conn: true,
    };
    allocations(|| {
        std::hint::black_box(borrowed_request(&request).draft());
    })
    .0
}

#[test]
fn building_the_frame_head_costs_nothing_for_a_borrowed_request() {
    // The same claim with the channel and the response decode held out of it, so the number is the
    // translation and nothing else. The borrowed head is a struct of slices, so it is built on the
    // stack and costs zero allocations at any header count — and the test says exactly that, rather
    // than "few".
    //
    // The owned translation is measured beside it for contrast: twelve allocations with no request
    // headers at all, growing by two per header, which is the cost this change removes.
    let owned = translation_only_owned(20);
    let borrowed = translation_only_borrowed(20);

    assert_eq!(
        borrowed, 0,
        "the borrowed head should allocate nothing; it borrows every field out of the CGI \
         environment and renders the one transformed name into a stack buffer"
    );
    assert!(
        owned >= 40,
        "the owned translation should cost at least two allocations per header; it was {owned}"
    );
}
