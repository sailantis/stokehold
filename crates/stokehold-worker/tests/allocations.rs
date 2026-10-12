//! Allocation counts for the worker frame reader, pinned as a test.
//!
//! The worker channel is the hot path of the fastest deployment option — a resident
//! PHP worker over a pipe — so a per-frame allocation is paid on every response. Each
//! test asserts an exact count rather than a bound: a new allocation here is a decision,
//! and a test that names it is the cheapest way to notice.
//!
//! Same construction as `stokehold-fcgi`'s `tests/allocations.rs`: a counting
//! `#[global_allocator]`, thread-local so the parallel harness cannot pollute a count,
//! confined to its own test binary so nothing else in the suite pays for it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use stokehold_worker::header::{Header, MessageType};
use stokehold_worker::{Codec, Json};

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

/// A serialized frame: header, payload, and nothing else.
fn frame(message_type: MessageType, request_id: u32, payload: &[u8]) -> Vec<u8> {
    let header = Header::new(message_type, request_id, payload).expect("frame header");
    let mut out = header.encode().to_vec();
    out.extend_from_slice(payload);
    out
}

#[test]
fn reading_one_frame_allocates_only_its_payload() {
    // `Header::read_frame` is called once per frame. The payload has to be owned —
    // the caller dispatches on it after the header is gone — so one allocation for the
    // payload is the floor. Anything above it is overhead, and the `vec![0u8; len]` that
    // zeroes before the read is a `memset` over the whole payload that is worth knowing
    // about.
    let payload = vec![b'x'; 12 * 1024];
    let wire = frame(MessageType::Body, 1, &payload);

    let (count, bytes) = allocations(|| {
        let mut cursor = wire.as_slice();
        let (header, read) = Header::read_frame(&mut cursor)
            .expect("read the frame")
            .expect("a frame");
        assert_eq!(header.message_type, MessageType::Body);
        assert_eq!(read.len(), 12 * 1024);
    });

    // One allocation: the payload buffer. And it is the payload's size, which is what
    // makes the zero-fill visible — a `read`-into-uninitialised would not allocate
    // differently, but it would not touch those 12 KB twice.
    assert_eq!(count, 1, "exactly the payload buffer");
    assert!(
        bytes >= 12 * 1024,
        "the payload allocation is the payload: {bytes}"
    );
}

#[test]
fn a_small_frame_allocates_once_for_its_payload() {
    // A `Response` head is a few hundred bytes of JSON. Same shape, much smaller.
    let payload = Json::encode(&stokehold_worker::ResponseHead {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/html".to_string())],
        ..Default::default()
    })
    .expect("encode a head");
    let wire = frame(MessageType::Response, 1, &payload);

    let (count, _) = allocations(|| {
        let mut cursor = wire.as_slice();
        let (_, read) = Header::read_frame(&mut cursor).unwrap().unwrap();
        assert!(!read.is_empty());
    });
    assert_eq!(count, 1);
}

#[test]
fn reading_a_frame_with_an_empty_payload_allocates_nothing() {
    // The `End` frame of every response has no payload at all. `vec![0u8; 0]` does not
    // allocate, so this path is already free — worth pinning so a change to the buffer
    // handling cannot quietly make the most common frame in the protocol cost something.
    let wire = frame(MessageType::End, 1, b"");

    let (count, _) = allocations(|| {
        let mut cursor = wire.as_slice();
        let (header, read) = Header::read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(header.message_type, MessageType::End);
        assert!(read.is_empty());
    });
    assert_eq!(count, 0);
}

#[test]
fn a_truncated_frame_allocates_only_its_payload_buffer() {
    // The error path must not be more expensive than the success path in a way that
    // makes a malformed stream a lever. The payload buffer is allocated, then the read
    // fails; there is no second buffer for the error.
    let mut wire = frame(MessageType::Body, 1, &vec![b'x'; 4096]);
    wire.truncate(wire.len() - 100);

    let (count, _) = allocations(|| {
        let mut cursor = wire.as_slice();
        assert!(Header::read_frame(&mut cursor).is_err());
    });
    assert_eq!(count, 1);
}

#[test]
fn the_crc32c_check_adds_no_allocation() {
    // The checksum runs over the whole payload on every frame. It was a hand-written
    // bit-by-bit loop and is now the `crc32c` crate; whichever it is, it must not
    // allocate, because it runs over 12 KB on every response.
    let payload = vec![b'x'; 12 * 1024];
    let (count, _) = allocations(|| {
        let value = stokehold_worker::header::crc32c(&payload);
        assert_ne!(value, 0);
    });
    assert_eq!(count, 0);
}

#[test]
fn encoding_a_head_allocates_the_json_once() {
    // The server encodes a `RequestHeader` per request. `Json::encode` builds one
    // `String`, so one allocation is the floor; this pins that it stays that way rather
    // than growing with the number of headers.
    use stokehold_worker::RequestHeader;

    fn encoder_for(header_count: usize) -> usize {
        let header = RequestHeader {
            method: "GET".to_string(),
            uri: "/items".to_string(),
            headers: (0..header_count)
                .map(|index| (format!("X-Header-{index}"), format!("value-{index}")))
                .collect(),
            ..Default::default()
        };
        let (count, _) = allocations(|| {
            let encoded = Json::encode(&header).expect("encode");
            assert!(!encoded.is_empty());
        });
        count
    }

    let few = encoder_for(2);
    let many = encoder_for(40);

    // serde_json serializes into one growable buffer, so the count rises only by the
    // buffer's doublings — logarithmic, not one per header.
    assert!(
        many <= few + 8,
        "encoding grew by {} allocations for 38 more headers; a per-header copy would \
         be 38 more",
        many - few
    );
}
