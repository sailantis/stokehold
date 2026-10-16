//! How many times one request reaches the operating system, counted through the real codec.
//!
//! A frame is written as a header and then a payload, and read as a header and then a payload.
//! Over a `Cursor` that is free. Over the `ChildStdin`/`ChildStdout` the pool actually uses,
//! each of those is a separate `write(2)` or `read(2)`, and each read is also a separate wakeup
//! of the exchange thread serving the request — which is the expensive half on a pipe.
//!
//! `volucer-fcgi`'s client assembles a whole FastCGI request and issues "one write, one
//! flush" for exactly this reason. The worker channel, which carries the fastest deployment
//! option, was left counting on each codec call landing on a syscall.
//!
//! The counters wrap the real codec, so these measure the production call pattern rather than a
//! re-statement of it. No `#[global_allocator]` here, so no separate binary is needed, but the
//! counters are thread-local so the parallel harness cannot pollute them.

use std::cell::Cell;
use std::io::{self, Cursor, Read, Write};

use volucer_worker::{
    BoundedChannel, Codec, Header, Json, MessageType, RequestEnd, RequestHeader, ResponseHead,
    WorkerChannel,
};

thread_local! {
    static WRITES: Cell<usize> = const { Cell::new(0) };
    static FLUSHES: Cell<usize> = const { Cell::new(0) };
    static READS: Cell<usize> = const { Cell::new(0) };
}

/// Counts the calls that become syscalls, and the flushes separately.
///
/// `flush` is counted apart from `write` on purpose: on a `ChildStdin` a flush is a no-op, so
/// folding the two together would overstate the cost of a buffer. `write` and `read` on a pipe
/// are one-to-one with `write(2)` and `read(2)`.
struct Counting<IO>(IO);

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        WRITES.with(|count| count.set(count.get() + 1));
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        FLUSHES.with(|count| count.set(count.get() + 1));
        self.0.flush()
    }
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        READS.with(|count| count.set(count.get() + 1));
        self.0.read(buf)
    }
}

/// Writes, flushes and reads, in that order.
type Totals = (usize, usize, usize);

fn totals() -> Totals {
    (
        WRITES.with(Cell::get),
        FLUSHES.with(Cell::get),
        READS.with(Cell::get),
    )
}

fn count_start() {
    let _ = WRITES.try_with(|count| count.set(0));
    let _ = FLUSHES.try_with(|count| count.set(0));
    let _ = READS.try_with(|count| count.set(0));
}

fn push_frame(out: &mut Vec<u8>, message_type: MessageType, request_id: u32, payload: &[u8]) {
    let mut header = Header::new(message_type, request_id, payload).expect("frame header");
    if message_type == MessageType::Body {
        header.flags |= volucer_worker::flags::LAST;
    }
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(payload);
}

/// The bytes a worker sends back for one request: a head, a body, and the end.
///
/// Three frames, each of which is a header plus a payload on the wire.
fn reply(request_id: u32) -> Vec<u8> {
    let mut out = Vec::new();

    let head = Json::encode(&ResponseHead {
        status: 200,
        headers: vec![("Content-Type".to_string(), "text/html".to_string())],
        body_len: 5,
    })
    .unwrap();
    push_frame(&mut out, MessageType::Response, request_id, &head);
    push_frame(&mut out, MessageType::Body, request_id, b"hello");
    push_frame(
        &mut out,
        MessageType::End,
        request_id,
        &Json::encode(&RequestEnd::default()).unwrap(),
    );

    out
}

fn request() -> RequestHeader {
    RequestHeader {
        method: "GET".to_string(),
        uri: "/items".to_string(),
        protocol: "HTTP/1.1".to_string(),
        headers: vec![("Accept".to_string(), "text/html".to_string())],
        remote_addr: "127.0.0.1".to_string(),
        body_len: 0,
        script_filename: "/srv/app/public/index.php".to_string(),
        https: false,
    }
}

/// One request through the channel as the pool builds it now.
fn bounded() -> Totals {
    let mut channel = BoundedChannel::new(Counting(Vec::new()), Counting(Cursor::new(reply(1))));
    let header = request();

    count_start();
    let response = channel.request(&header, b"").expect("a canned reply");
    assert_eq!(response.body, b"hello");
    totals()
}

/// One request through the same channel without buffers, which is what the pool used to build.
fn unbuffered() -> Totals {
    let mut channel = WorkerChannel::new(Counting(Vec::new()), Counting(Cursor::new(reply(1))));
    let header = request();

    count_start();
    let response = channel.request(&header, b"").expect("a canned reply");
    assert_eq!(response.body, b"hello");
    totals()
}

#[test]
fn a_full_response_is_read_in_one_syscall_and_not_one_per_field() {
    // Three frames come back — a head, a body and the end — and each is a header followed by a
    // payload, so an unbuffered reader has to enter the kernel six times to hear one small
    // answer. Each of those is also a wakeup of the exchange thread. Buffered, the first read
    // fills the buffer and the rest is a slice of what is already there.
    let (writes, flushes, reads) = unbuffered();
    assert_eq!(
        (writes, flushes, reads),
        (2, 1, 6),
        "the unbuffered baseline changed; the reason this type exists needs re-stating"
    );

    let (writes, flushes, reads) = bounded();
    assert_eq!(
        writes, 1,
        "the request's header and payload share one write"
    );
    assert_eq!(flushes, 1, "one frame means one flush");
    assert_eq!(
        reads, 1,
        "the whole reply arrived in the buffer behind the first read"
    );
}

#[test]
fn the_read_count_does_not_grow_with_the_number_of_frames() {
    // The point of a buffer is to decouple the syscall count from the frame count, so this is
    // the property to pin: three frames must not cost three reads plus two payload reads. The
    // buffer is a fixed size, so a large answer does cost more reads — that is the buffer being
    // refilled, not the framing being charged for.
    fn reads_for(body_len: usize) -> usize {
        let mut out = Vec::new();
        let head = Json::encode(&ResponseHead {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/html".to_string())],
            body_len: body_len as u64,
        })
        .unwrap();
        push_frame(&mut out, MessageType::Response, 1, &head);
        push_frame(&mut out, MessageType::Body, 1, &vec![b'x'; body_len]);
        push_frame(
            &mut out,
            MessageType::End,
            1,
            &Json::encode(&RequestEnd::default()).unwrap(),
        );

        let mut channel = BoundedChannel::new(Counting(Vec::new()), Counting(Cursor::new(out)));
        let header = request();
        count_start();
        let response = channel.request(&header, b"").expect("a canned reply");
        assert_eq!(response.body.len(), body_len);
        READS.with(Cell::get)
    }

    // A 5-byte body and a 100 KB one differ in how many times the buffer has to be refilled for
    // the payload, and in nothing else. The three frames themselves add no reads at all.
    let small = reads_for(5);
    let large = reads_for(100 * 1024);

    assert_eq!(small, 1, "the whole small reply fits in one buffer fill");
    assert!(
        large <= 100 * 1024 / 8192 + 2,
        "a large body may only cost one read per buffer refill; {large} reads for 100 KB means \
         the framing is being charged for again"
    );
}
