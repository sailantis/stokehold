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
use stokehold_worker::{Codec, Json, WorkerChannel};

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

/// A complete reply: head, `frames` body frames of `chunk` bytes, and the end.
fn reply(request_id: u32, body_len: usize, chunk: usize) -> Vec<u8> {
    use stokehold_worker::RequestEnd;

    let mut out = Vec::new();

    let head = Json::encode(&stokehold_worker::ResponseHead {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/html".to_string())],
        body_len: body_len as u64,
    })
    .expect("encode a head");
    out.extend_from_slice(&frame(MessageType::Response, request_id, &head));

    let mut sent = 0usize;
    while sent < body_len {
        let this = chunk.min(body_len - sent);
        let payload = vec![b'x'; this];
        let mut header = Header::new(MessageType::Body, request_id, &payload).expect("body header");
        if sent + this == body_len {
            header.flags |= stokehold_worker::flags::LAST;
        }
        out.extend_from_slice(&header.encode());
        out.extend_from_slice(&payload);
        sent += this;
    }

    let end = Json::encode(&RequestEnd::default()).expect("encode the end");
    out.extend_from_slice(&frame(MessageType::End, request_id, &end));

    out
}

/// The whole read path, through the real channel, with a canned reply.
fn read_cost(body_len: usize, chunk: usize) -> usize {
    let bytes = reply(1, body_len, chunk);
    let mut channel = WorkerChannel::new(Vec::new(), std::io::Cursor::new(bytes));
    let (count, _) = allocations(|| {
        let response = channel
            .request_encoded(Vec::new(), b"", |_| {})
            .expect("a canned reply");
        assert_eq!(response.body.len(), body_len);
    });
    count
}

#[test]
fn reading_a_reply_does_not_grow_with_the_number_of_frames() {
    // A response body arrives in `MAX_BODY_CHUNK` frames, so a large response is many
    // frames. Before the frame buffer was reused, each frame cost its own allocation and
    // its own zero-fill: a 1 MB body in 64 KiB frames was 27 allocations. The point of
    // reusing the buffer is that the frame *count* stops mattering.
    //
    // The body itself still has to be assembled, and its declaration lets it be sized once
    // rather than regrown frame by frame — so the count should not grow with the frame count
    // either.
    let small = read_cost(50 * 1024, 64 * 1024);
    let large = read_cost(1024 * 1024, 64 * 1024);

    let growth = large - small;
    assert!(
        growth <= 2,
        "reading a 1 MB / 16-frame reply cost {growth} allocations more than a 50 KB / \
         1-frame reply; a per-frame buffer would be 15 more, so this means the frame buffer \
         is being allocated per frame again"
    );
}

#[test]
fn reading_a_reply_is_a_flat_cost_for_any_body_size() {
    // A cold channel pays for the frame buffer once; after that the cost must be flat in the
    // body size, which is the claim this pins.
    for (body_len, chunk) in [
        (11usize, 64 * 1024),
        (2048, 64 * 1024),
        (50 * 1024, 64 * 1024),
        (200 * 1024, 64 * 1024),
        (1024 * 1024, 64 * 1024),
    ] {
        assert_eq!(
            read_cost(body_len, chunk),
            7,
            "a {body_len}-byte reply in {chunk}-byte frames should cost the same as any \
             other size: four for the head, one for the body, one for the end, one for the \
             frame buffer the channel does not have yet"
        );
    }
}


#[test]
fn a_warm_channel_does_not_pay_for_the_frame_buffer_again() {
    // The frame buffer belongs to the channel, so it is allocated once and reused by every
    // request after. This compares the first request on a channel with the second by serving
    // both from one reply stream (request ids 1 and 2), so the only difference between them is
    // the buffer the first request had to grow.
    //
    // The body is still allocated per request — a response body is handed to the caller inside
    // the `WorkerResponse`, so its allocation moves rather than disappears. That is why this
    // asserts a *difference*, not a zero: the frame buffer is what should stop being paid.
    let body_len = 2048usize;
    let mut wire = reply(1, body_len, 64 * 1024);
    wire.extend_from_slice(&reply(2, body_len, 64 * 1024));

    let mut channel = WorkerChannel::new(Vec::new(), std::io::Cursor::new(wire));

    let (first, _) = allocations(|| {
        channel.request_encoded(Vec::new(), b"", |_| {}).expect("the first reply");
    });
    let (second, _) = allocations(|| {
        channel.request_encoded(Vec::new(), b"", |_| {}).expect("the second reply");
    });

    assert!(
        first > second,
        "the first request cost {first} and the second {second}; the second should cost \
         less, because the frame buffer it needs already exists"
    );
    // The steady-state number is what matters: every request after the first costs this.
    assert_eq!(
        second, 5,
        "a warm channel should cost five allocations — the response head and its two Strings, \
         the body buffer, and the End — and {second} was measured"
    );
    // The first request pays for the frame buffer, which grows in steps: the response-head
    // frame, then the body frame. So the difference is the buffer, not one allocation.
    assert_eq!(
        first - second,
        2,
        "the cold request should pay exactly for the frame buffer's growth ({first} vs \
         {second})"
    );
}

#[test]
fn a_body_larger_than_the_reserve_limit_still_reads_correctly() {
    // The reservation from the head's declaration is capped so a worker cannot make the
    // server commit an arbitrary amount of memory before sending a byte. Past the cap the
    // body must still assemble correctly — the cap decides whether the growth is prepaid,
    // not whether it happens.
    let body_len = 3 * 1024 * 1024;
    let bytes = reply(1, body_len, 64 * 1024);
    let mut channel = WorkerChannel::new(Vec::new(), std::io::Cursor::new(bytes));

    let (count, _) = allocations(|| {
        let response = channel.request_encoded(Vec::new(), b"", |_| {}).expect("reply");
        assert_eq!(response.body.len(), body_len);
        assert!(response.body.iter().all(|byte| *byte == b'x'));
    });

    assert!(
        count < 40,
        "a 3 MB body past the reserve cap cost {count} allocations; growth from the cap \
         should be logarithmic, and a per-frame buffer would be ~48"
    );
}

