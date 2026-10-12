//! Allocation counts for the whole request-send path, one header per request pinned.
//!
//! `stokehold-server`'s `allocations.rs` measures the deep clones that *carried* the header
//! around — the part whose cost was "one `#[derive(Clone)]` of a `RequestHeader`" and could
//! be stated without a live exchange. This file measures the other half: what it costs to
//! actually put a request on the wire, through the real [`WorkerChannel`] over a `Cursor`,
//! with a canned reply.
//!
//! Together they say the whole story, because the clones were pure overhead: the codec
//! borrows what it serializes, so once the header reaches the codec the wire cost is one
//! allocation per *request*, not per header. The slope is what makes a reintroduced
//! `..header.clone()` visible here rather than only in a sibling crate.
//!
//! Own test binary, like every other `allocations.rs`: a `#[global_allocator]` is per binary.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use stokehold_worker::{
    Codec, Header, Json, MessageType, RequestEnd, RequestHeader, ResponseHead, WorkerChannel,
};

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

/// Append one frame: header, then payload.
fn push_frame(out: &mut Vec<u8>, message_type: MessageType, request_id: u32, payload: &[u8]) {
    let header = Header::new(message_type, request_id, payload).expect("frame header");
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(payload);
}

/// The bytes a worker sends back for one request: a head, a body, and the end.
fn reply(request_id: u32) -> Vec<u8> {
    let mut out = Vec::new();

    let head = Json::encode(&ResponseHead {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
        body_len: 5,
    })
    .unwrap();
    push_frame(&mut out, MessageType::Response, request_id, &head);

    let mut header = Header::new(MessageType::Body, request_id, b"hello").unwrap();
    header.flags |= stokehold_worker::flags::LAST;
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(b"hello");

    let end = Json::encode(&RequestEnd::default()).unwrap();
    push_frame(&mut out, MessageType::End, request_id, &end);
    out
}

fn request_with(header_count: usize) -> RequestHeader {
    RequestHeader {
        method: "GET".to_string(),
        uri: "/items".to_string(),
        protocol: "HTTP/1.1".to_string(),
        headers: (0..header_count)
            .map(|index| (format!("X-Header-{index}"), format!("value-{index}")))
            .collect(),
        remote_addr: "127.0.0.1".to_string(),
        body_len: 0,
        script_filename: "/srv/app/public/index.php".to_string(),
        https: false,
    }
}

#[test]
fn a_request_over_the_wire_costs_one_allocation_and_not_one_per_header() {
    // `WorkerChannel::request` takes the header by reference and serializes a borrowed view
    // of it. So the only allocations are the JSON payload and the frame the writer copies
    // into — neither of which grows with the header count except by buffer doubling.
    //
    // The response side is included deliberately: this measures the whole round trip, which
    // is what a request actually costs, and it is the number that would move if a clone came
    // back.
    fn round_trip_cost(header_count: usize) -> usize {
        let bytes = reply(1);
        let mut channel = WorkerChannel::new(Vec::new(), std::io::Cursor::new(bytes));
        let header = request_with(header_count);

        let (count, _) = allocations(|| {
            let response = channel.request(&header, b"").expect("a canned reply");
            assert_eq!(response.body, b"hello");
        });
        count
    }

    let few = round_trip_cost(2);
    let many = round_trip_cost(40);

    // A clone of the header would be 76 more allocations here for the 38 extra headers
    // (`stokehold-server`'s `allocations.rs` pins that slope). Buffer doubling and the
    // decoded response are all that is allowed to grow.
    // Measured growth is 3 allocations for 38 more headers — the JSON payload buffer
    // doubling as it grows, not a copy of anything. A clone of the header here would be 76.
    //
    // The bound is kept tight rather than set to a round 50 so it stays a statement about
    // this code: any change that makes the per-request work scale with the header count
    // fails it, and buffer doubling stays well clear of it.
    let growth = many - few;
    assert!(
        growth <= 4,
        "the round trip grew by {growth} allocations for 38 more headers; only the JSON \
         buffer's doublings should grow, so anything near 76 means the header is being \
         copied again"
    );
}
