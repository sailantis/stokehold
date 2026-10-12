//! The channel: the request/response exchange on top of the frame codec.
//!
//! [`WorkerChannel`] owns both directions of a worker's pipe and implements the
//! exchange described in `docs/worker-protocol.md`. It is generic over
//! `Write`/`Read` so the tests can drive it over a `Cursor` — the same code path
//! the pool uses over real pipes.
//!
//! Two decisions are worth stating because they are the ones a reader would
//! otherwise expect to be different:
//!
//! - **`body_len` is authoritative *and* the last body frame is flagged.** The
//!   length lets the worker size its read before receiving a byte and lets a
//!   truncated body be detected instead of hanging; the flag is a cheap second
//!   check that catches a desynchronised stream. Neither alone is enough.
//! - **Unsolicited frames are collected, never discarded.** A worker's log line
//!   arriving in the middle of a response is normal, so the reader skips it and
//!   hands it back rather than failing the request.

use std::io::{BufReader, BufWriter, Read, Write};

use crate::error::{Result, WorkerError};
use crate::header::{flags, write_frame, Header, MessageType};
use crate::message::{
    Codec, Control, ErrorReport, HelloAck, Json, LogRecord, MetricRecord, RequestEnd,
    RequestHeader, ResponseHead,
};

/// The largest body slice sent in one frame.
///
/// Bounded so a large upload does not require the whole body in memory twice and
/// so a frame length stays far from the protocol's sanity limit.
pub const MAX_BODY_CHUNK: usize = 64 * 1024;

/// The capacity of [`BoundedChannel`]'s buffers.
///
/// Sized to hold a whole small request, a response head and an `End` frame — `MAX_BODY_CHUNK`
/// is twice this and the writer flushes rather than splitting a frame, so a full body chunk
/// cannot deadlock behind a full buffer (see [`BoundedChannel`]). The reader's buffer only
/// needs to cover a frame header plus a payload worth reading in one go; a larger buffer would
/// not remove a syscall, because a frame larger than it still takes several.
const BUFFER_CAPACITY: usize = 8 * 1024;

/// The largest response body that will be reserved up front from the head's declaration.
///
/// A response head arrives before its body and declares the body's length, so the body can be
/// sized once instead of regrowing. The reservation is capped because the declaration is the
/// worker's to make: without a cap, a worker claiming a gigabyte would make the server commit
/// it before sending a byte, which turns a bad length into memory pressure rather than an
/// error. Past the cap the body still grows to whatever actually arrives — this only decides
/// whether the growth is paid up front.
const BODY_RESERVE_LIMIT: usize = 1024 * 1024;

/// The writer half of a [`BoundedChannel`]: a buffered pipe that flushes per frame.
pub type BufferedWriter = BufWriter<Box<dyn Write + Send>>;

/// The reader half of a [`BoundedChannel`]: a buffered pipe shared by the exchange thread.
pub type BufferedReader = BufReader<Box<dyn Read + Send>>;

/// A [`WorkerChannel`] over a real pipe, with both directions buffered.
///
/// This exists because of what the channel does to an unbuffered pipe. Every frame is written
/// as a header and then a payload, and read as a header and then a payload, and each of those
/// calls lands on the operating system directly. On a pipe that makes a request cost two
/// `write(2)`s and at least two `read(2)`s for the frames plus one more for every payload,
/// where one of each would do — and each read is also a separate wakeup of the thread serving
/// the request, which is the expensive half on a pipe. `stokehold-fcgi`'s client assembles a
/// whole FastCGI request and issues "one write, one flush" for the same reason; the worker
/// channel is the fastest deployment option and was the one left counting on each call landing
/// on a syscall.
///
/// The writer is flushed after every frame, by [`write_frame`] rather than here, and the
/// channel is half-duplex: the worker answers only once the request is complete, so a frame
/// left in the buffer would be a deadlock rather than a latency win.
///
/// ```text
///   channel frame            unbuffered pipe   this type
///   REQUEST: header+payload       2 writes        1 write(2)
///   RESPONSE head+payload         3 reads         1 read(2)  (typical)
///   BODY:   header+payload
///   END:    header+payload
/// ```
///
/// It is deliberately **not** `Debug`: a `BufReader` over `ChildStdout` is, but deriving it
/// would be a statement about state we do not want printed.
pub struct BoundedChannel {
    channel: WorkerChannel<BufferedWriter, BufferedReader>,
}

impl BoundedChannel {
    /// Wrap a worker's stdin and stdout, buffered in both directions.
    pub fn new(writer: impl Write + Send + 'static, reader: impl Read + Send + 'static) -> Self {
        BoundedChannel {
            channel: WorkerChannel::new(
                BufWriter::with_capacity(BUFFER_CAPACITY, Box::new(writer)),
                BufReader::with_capacity(BUFFER_CAPACITY, Box::new(reader)),
            ),
        }
    }

    /// The request id that will be used next, forwarded from the inner channel.
    pub fn next_request_id(&self) -> u32 {
        self.channel.next_request_id()
    }

    /// Perform the handshake: send `HELLO`, require a matching `HELLO_ACK`.
    pub fn handshake(&mut self) -> Result<HelloAck> {
        self.channel.handshake()
    }

    /// Send a request and collect its whole response.
    pub fn request(&mut self, request: &RequestHeader, body: &[u8]) -> Result<WorkerResponse> {
        self.channel.request(request, body)
    }

    /// The same request, with the send and receive phases kept apart.
    ///
    /// The pool needs the stage, not the response: a failure while sending can be replayed
    /// for any method, one while reading only for an idempotent one. See
    /// [`WorkerChannel::request_staged`].
    pub fn request_staged(
        &mut self,
        request: &RequestHeader,
        body: &[u8],
        on_stage: impl FnMut(RequestStage),
    ) -> Result<WorkerResponse> {
        self.channel.request_staged(request, body, on_stage)
    }

    /// A request whose head was encoded on the caller's side of the thread boundary.
    ///
    /// The pool's path: it borrows the head out of the CGI environment, so it encodes there and
    /// sends the bytes rather than an owned header. See [`WorkerChannel::request_encoded`].
    pub fn request_encoded(
        &mut self,
        head: Vec<u8>,
        body: &[u8],
        on_stage: impl FnMut(RequestStage),
    ) -> Result<WorkerResponse> {
        self.channel.request_encoded(head, body, on_stage)
    }

    /// Write one frame from a payload that is already encoded.
    pub fn send_frame(
        &mut self,
        message_type: MessageType,
        request_id: u32,
        payload: &[u8],
        extra_flags: u16,
    ) -> Result<()> {
        self.channel
            .send_frame(message_type, request_id, payload, extra_flags)
    }

    /// Ask a worker for its health without serving a request.
    pub fn ping(&mut self) -> Result<()> {
        self.channel.ping()
    }

    /// Send a control message and wait for its acknowledgement.
    pub fn control(&mut self, control: &Control) -> Result<()> {
        self.channel.control(control)
    }
}

/// A borrowed `RequestHeader`, with `body_len` supplied by the caller.
///
/// Exists so the request frame can be serialized without cloning the header, and so the header
/// can be *built* without owning anything: every field is borrowed from wherever it came from,
/// and `headers` is an iterator of borrowed pairs rather than a `Vec` of owned `String`s. Field
/// names and order match [`RequestHeader`] exactly — that is a protocol requirement, not a style
/// preference, because the PHP SDK decodes this JSON — so the two must be changed together, and
/// the derive is deliberately still what decides the field names.
/// `the_borrowed_request_frame_is_byte_identical_to_the_owned_encoding` is what enforces it.
///
/// `I` and `P` are generic because the server reads the request headers out of the CGI
/// environment and can produce them on the fly: a `&[(String, String)]` would force it to collect
/// owned pairs into a `Vec` first, which is the allocation this type exists to avoid. The explicit
/// bound replaces serde's inferred `I: Serialize`, which an iterator does not implement.
#[derive(serde::Serialize)]
#[serde(bound(serialize = "I: Iterator<Item = P> + Clone, P: serde::Serialize"))]
pub struct RequestHeaderJson<'a, I, P = (&'a str, &'a str)>
where
    I: Iterator<Item = P> + Clone,
    P: serde::Serialize,
{
    pub method: &'a str,
    pub uri: &'a str,
    pub protocol: &'a str,
    pub headers: BorrowedHeaders<'a, I>,
    pub remote_addr: &'a str,
    pub body_len: u64,
    pub script_filename: &'a str,
    pub https: bool,
}

/// The request's headers, as a sequence of serializable pairs.
///
/// Wrapped rather than serialized directly because serde has no built-in way to say "serialize
/// this iterator as an array of two-element arrays": the iterator is not `Serialize`, and
/// `collect` would be the allocation the wrapper exists to avoid. `Clone` is part of the contract
/// — serializing a sequence of unknown length asks for the iterator once, so cloning is what makes
/// the borrow in `Serialize::serialize` enough.
pub struct BorrowedHeaders<'a, I>(I, std::marker::PhantomData<&'a ()>);

impl<'a, I> BorrowedHeaders<'a, I> {
    /// Wrap an iterator of header pairs for serialization.
    pub fn new(pairs: I) -> Self {
        BorrowedHeaders(pairs, std::marker::PhantomData)
    }
}

impl<'a, I, P> serde::Serialize for BorrowedHeaders<'a, I>
where
    I: Iterator<Item = P> + Clone,
    P: serde::Serialize,
{
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut sequence = serializer.serialize_seq(None)?;
        for pair in self.0.clone() {
            sequence.serialize_element(&pair)?;
        }
        sequence.end()
    }
}

/// A `RequestHeader`'s own header pairs, borrowed as `&str`.
///
/// Exists so [`RequestHeader::draft`](crate::RequestHeader::draft) can name the iterator it hands
/// to the codec: a `Map<Iter, fn(..)>` would name the same thing through a function pointer, and a
/// `Clone` iterator adapter names it without one.
#[derive(Clone)]
pub struct HeaderPairs<'a>(pub std::slice::Iter<'a, (String, String)>);

impl<'a> Iterator for HeaderPairs<'a> {
    type Item = (&'a str, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        self.0
            .next()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }
}

/// Which half of a request `WorkerChannel::request_staged` reached.
///
/// The distinction exists for one reason: a caller that may retry a non-idempotent
/// request needs to know whether the request could have been delivered. A failure
/// while [`RequestStage::Sending`] proves it could not; one while
/// [`RequestStage::Reading`] means it may have been.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestStage {
    /// The `REQUEST` frame and its body were being written.
    Sending,
    /// The request was written in full; the response was being read.
    Reading,
}

/// One decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub header: Header,
    pub payload: Vec<u8>,
}

/// A completed request: head, body and terminal status.
#[derive(Debug, Clone, Default)]
pub struct WorkerResponse {
    pub head: ResponseHead,
    /// The response body, reassembled from `Body` frames. The pool relays this
    /// into a FastCGI `STDOUT` stream without re-parsing it.
    pub body: Vec<u8>,
    pub end: RequestEnd,
    /// Log lines and metrics the worker emitted while serving this request.
    pub logs: Vec<LogRecord>,
    /// Metrics the worker emitted while serving this request.
    pub metrics: Vec<MetricRecord>,
}

impl WorkerResponse {
    /// The HTTP status the application chose.
    pub fn status(&self) -> u16 {
        self.head.status
    }

    /// Whether the worker is willing to serve another request.
    pub fn keep_alive(&self) -> bool {
        self.end.keep_alive
    }
}

/// Both directions of a worker's pipe.
#[derive(Debug)]
pub struct WorkerChannel<W: Write, R: Read> {
    writer: W,
    reader: R,
    next_request_id: u32,
    logs: Vec<LogRecord>,
    metrics: Vec<MetricRecord>,
    /// Where the request frame's JSON is built.
    ///
    /// Belongs to the channel because the channel outlives a request, so a sequence of requests
    /// costs one allocation for the head rather than a `Vec` per request. Cleared, never shrunk,
    /// so the capacity that made the last request free is kept.
    payload: Vec<u8>,
    /// Where each reply frame is read, so the reader reuses one buffer instead of allocating —
    /// and zeroing — a `Vec` per frame.
    ///
    /// Belongs to the channel for the same reason `payload` does. A frame's payload is copied
    /// out of it (into the body, or decoded) before the next read, because the next read
    /// overwrites it.
    scratch: Vec<u8>,
}

impl<W: Write, R: Read> WorkerChannel<W, R> {
    /// Wrap a writer (the worker's stdin) and a reader (its stdout).
    pub fn new(writer: W, reader: R) -> Self {
        WorkerChannel {
            writer,
            reader,
            next_request_id: crate::FIRST_REQUEST_ID,
            logs: Vec::new(),
            metrics: Vec::new(),
            payload: Vec::new(),
            scratch: Vec::new(),
        }
    }

    /// Push everything written so far through to the writer.
    ///
    /// A no-op for an unbuffered writer; the meaningful call for a [`BufWriter`]. The channel
    /// never flushes on its own — [`write_frame`] does it after each frame, because whether a
    /// frame may be batched depends on what follows it, which only the caller knows.
    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush()?;
        Ok(())
    }

    /// The request id that will be used next.
    pub fn next_request_id(&self) -> u32 {
        self.next_request_id
    }

    /// Log lines seen so far, including those collected during requests.
    pub fn take_logs(&mut self) -> Vec<LogRecord> {
        std::mem::take(&mut self.logs)
    }

    /// Metric samples seen so far.
    pub fn take_metrics(&mut self) -> Vec<MetricRecord> {
        std::mem::take(&mut self.metrics)
    }

    /// Log lines this worker has emitted and not yet taken.
    pub fn logs(&self) -> &[LogRecord] {
        &self.logs
    }

    /// Metric samples this worker has emitted and not yet taken.
    pub fn metrics(&self) -> &[MetricRecord] {
        &self.metrics
    }

    /// Perform the handshake: send `HELLO`, require a matching `HELLO_ACK`.
    ///
    /// The version check happens here, at boot, rather than on the first request
    /// — a mismatched SDK is a configuration problem and should look like one.
    pub fn handshake(&mut self) -> Result<HelloAck> {
        let hello = RequestHeader::default();
        self.send_json(MessageType::Hello, crate::UNSOLICITED, &hello, 0)?;

        let (header, payload) = self.expect(MessageType::HelloAck, "handshake")?;
        let ack: HelloAck = Json::decode(&payload)?;
        if ack.protocol_version != crate::PROTOCOL_VERSION {
            return Err(WorkerError::UnsupportedVersion {
                found: ack.protocol_version,
                expected: crate::PROTOCOL_VERSION,
            });
        }
        tracing::debug!(
            frame = header.message_type as u8,
            php = %ack.php_version,
            sapi = %ack.sapi,
            pid = ack.pid,
            "worker handshake complete"
        );
        Ok(ack)
    }

    /// Send a request and collect its whole response.
    ///
    /// Blocking by design: the pool gives each worker a thread so a slow
    /// application cannot stall a listener, and the pipe read itself has no
    /// portable timeout — the pool enforces one by killing the worker, which
    /// unblocks the read on every platform.
    pub fn request(&mut self, request: &RequestHeader, body: &[u8]) -> Result<WorkerResponse> {
        self.request_staged(request, body, |_| {})
    }

    /// Allocate a request id, reserving 0 for unsolicited frames.
    ///
    /// Wraps rather than overflowing; the wrap skips [`crate::UNSOLICITED`] so an id is
    /// never spent on the value that means "not a request". Named `take_` rather than
    /// `next_` so it does not read like the [`WorkerChannel::next_request_id`] accessor it
    /// sits next to — this one advances, that one only looks.
    fn take_request_id(&mut self) -> u32 {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        if self.next_request_id == crate::UNSOLICITED {
            self.next_request_id = crate::FIRST_REQUEST_ID;
        }
        request_id
    }

    /// Write the request frame, declaring `body_len` for the body frames that follow.
    ///
    /// The length is stamped in here, from the bytes that are about to be framed, so the declared
    /// length and the bytes sent cannot disagree. That used to be done with
    /// `RequestHeader { body_len, ..request.clone() }`, which deep-cloned five `String`s and two
    /// more per header on every request to change one integer; the borrowed view in
    /// [`RequestHeaderJson`] serializes to byte-identical JSON without copying anything, and into
    /// this channel's own buffer rather than a `Vec` per request.
    fn send_request<I, P>(&mut self, head: RequestHeaderJson<'_, I, P>, body: &[u8]) -> Result<u32>
    where
        I: Iterator<Item = P> + Clone,
        P: serde::Serialize,
    {
        let request_id = self.take_request_id();
        // Taken out of `self` for the duration of the write, so `self` can still be borrowed
        // mutably to write the frame while the buffer is in hand.
        let mut payload = std::mem::take(&mut self.payload);
        payload.clear();
        let head = RequestHeaderJson { body_len: body.len() as u64, ..head };

        let mut outcome = Json::encode_into(&mut payload, &head);
        if outcome.is_ok() {
            outcome = self.send_frame(MessageType::Request, request_id, &payload, 0);
        }
        if outcome.is_ok() {
            outcome = self.send_body(request_id, body);
        }

        // Home on every path, including a failure: a failed request must not cost the next one
        // its capacity.
        self.payload = payload;
        outcome.map(|()| request_id)
    }

    /// Send a request whose head was encoded on the caller's side of the thread boundary.
    ///
    /// This is how the pool reaches the channel, and it is decided by where the two halves live.
    /// The pool's head *borrows* the CGI environment — that is the point of `borrowed_request`,
    /// which exists so a request does not build nine `String`s that are dropped unread — and a
    /// borrow cannot travel to the exchange thread, because on a timeout the thread outlives the
    /// call that made it. So the head arrives already encoded, as bytes.
    ///
    /// That costs one `Vec` per request, so the head's allocation moves rather than disappears.
    /// It is still worth doing, because the exchange already copied the body into a `Vec` to move
    /// it across: the head is now carried in that same copy, and the request pays one buffer where
    /// the owned translation paid eleven.
    ///
    /// The JSON goes straight out as the `REQUEST` payload — no framing, no re-serialization — and
    /// the worker cannot tell which side of the process boundary encoded it.
    pub fn request_encoded(
        &mut self,
        head: Vec<u8>,
        body: &[u8],
        mut on_stage: impl FnMut(RequestStage),
    ) -> Result<WorkerResponse> {
        let request_id = self.take_request_id();

        // Everything up to the last byte of the body is the send phase. The callbacks are
        // reported before returning, so a failure that happens *during* the write is still
        // attributed to `Sending`.
        on_stage(RequestStage::Sending);
        self.send_frame(MessageType::Request, request_id, &head, 0)?;
        self.send_body(request_id, body)?;

        on_stage(RequestStage::Reading);
        self.collect_response(request_id)
    }

    /// The same request, with the send and receive phases kept apart.
    ///
    /// The two failure modes look nearly identical on the wire but prove different
    /// things, and a caller that may retry the request has to know which happened:
    ///
    /// - A failure in [`RequestStage::Sending`] means the `REQUEST` frame or one of
    ///   its body frames could not be written. Nothing was delivered, so replaying
    ///   the request cannot execute it twice — for *any* method.
    /// - A failure in [`RequestStage::Reading`] means the request was written in
    ///   full and the answer was not read. The application may be running it right
    ///   now, so a replay is a second execution and only an idempotent method can
    ///   accept that.
    ///
    /// [`WorkerChannel::request`] is the version for callers that cannot retry and
    /// do not care which phase failed.
    pub fn request_staged(
        &mut self,
        request: &RequestHeader,
        body: &[u8],
        mut on_stage: impl FnMut(RequestStage),
    ) -> Result<WorkerResponse> {
        // Everything up to the last byte of the body is the send phase. The callbacks are
        // reported before returning, so a failure that happens *during* the write is still
        // attributed to `Sending`.
        on_stage(RequestStage::Sending);
        let request_id = self.send_request(request.draft(body.len() as u64), body)?;

        on_stage(RequestStage::Reading);
        self.collect_response(request_id)
    }

    /// Ask a worker for its health without serving a request.
    ///
    /// Telemetry cannot reach the match below: `expect_any` absorbs log and
    /// metric frames before returning, so the only thing that arrives here is a
    /// real `PONG` — or a genuine mistake.
    pub fn ping(&mut self) -> Result<()> {
        self.send_json(MessageType::Ping, crate::UNSOLICITED, &(), 0)?;
        let (header, _payload) = self.expect_any("a ping answer")?;
        match header.message_type {
            MessageType::Pong => Ok(()),
            other => Err(WorkerError::NotA {
                expected: MessageType::Pong.name(),
                actual: other,
            }),
        }
    }

    /// Ask a worker to stop accepting work and exit when it is idle.
    pub fn drain(&mut self) -> Result<()> {
        self.control(&Control::Drain)
    }

    /// Ask a worker to exit immediately.
    pub fn shutdown(&mut self) -> Result<()> {
        self.control(&Control::Shutdown)
    }

    /// Send a control message and wait for its acknowledgement.
    ///
    /// As in [`WorkerChannel::ping`], telemetry is absorbed before it can reach
    /// this match.
    pub fn control(&mut self, control: &Control) -> Result<()> {
        self.send_json(MessageType::Control, crate::UNSOLICITED, control, 0)?;
        let (header, payload) = self.expect_any("a control acknowledgement")?;
        match header.message_type {
            MessageType::ControlAck => Ok(()),
            MessageType::Error => {
                let report: ErrorReport = Json::decode(&payload)?;
                Err(WorkerError::Remote {
                    message: report.message,
                })
            }
            other => Err(WorkerError::NotA {
                expected: MessageType::ControlAck.name(),
                actual: other,
            }),
        }
    }

    /// Read frames until the request is finished.
    ///
    /// The frame buffer is taken out of `self` for the duration, as `send_request` takes the
    /// payload buffer and for the same reason: the loop needs `&mut self` (for `absorb_log`)
    /// while the bytes it is reading live in a buffer owned by `self`. It goes home on every
    /// path, so a request that fails does not cost the next one its capacity.
    fn collect_response(&mut self, request_id: u32) -> Result<WorkerResponse> {
        let mut frame_buffer = std::mem::take(&mut self.scratch);
        let outcome = self.read_response(request_id, &mut frame_buffer);
        self.scratch = frame_buffer;
        outcome
    }

    /// The read loop of [`WorkerChannel::collect_response`].
    fn read_response(
        &mut self,
        request_id: u32,
        frame_buffer: &mut Vec<u8>,
    ) -> Result<WorkerResponse> {
        let mut response = WorkerResponse::default();
        let mut have_head = false;
        let mut body = Vec::new();
        let mut expected_body = 0u64;
        let mut seen_last_body = false;
        // Unsolicited telemetry accumulates in the channel's lifetime buffers as
        // it arrives; remembering where this request started separates "the logs
        // this request produced" from "the logs this worker has ever produced".
        let log_mark = self.logs.len();
        let metric_mark = self.metrics.len();

        loop {
            // Reuses one buffer for every frame, so a response costs no per-frame allocation
            // and no per-frame zero-fill. The bytes are consumed (copied into the body, or
            // decoded) before the next read overwrites them.
            let Some((header, len)) = Header::read_into(&mut self.reader, frame_buffer)? else {
                return Err(WorkerError::Closed(format!(
                    "the worker closed its output while answering request {request_id}"
                )));
            };
            let payload = &frame_buffer[..len];

            match header.message_type {
                MessageType::Response => {
                    if header.request_id != request_id {
                        return Err(WorkerError::WrongRequestId {
                            expected: request_id,
                            actual: header.request_id,
                        });
                    }
                    if have_head {
                        return Err(WorkerError::Remote {
                            message: format!("request {request_id} was answered twice"),
                        });
                    }
                    let head: ResponseHead = Json::decode(payload)?;
                    // The head arrives before the body, so the body can be sized once instead
                    // of regrowing frame by frame. The declaration is checked against what
                    // actually arrives below, and the reservation is capped, so a worker that
                    // declares a length it does not send cannot make this allocate it.
                    let declared = usize::try_from(head.body_len).unwrap_or(usize::MAX);
                    if declared <= BODY_RESERVE_LIMIT {
                        body.reserve(declared);
                    }
                    expected_body = head.body_len;
                    response.head = head;
                    have_head = true;
                }
                MessageType::Body => {
                    if header.request_id != request_id {
                        return Err(WorkerError::WrongRequestId {
                            expected: request_id,
                            actual: header.request_id,
                        });
                    }
                    if !have_head {
                        return Err(WorkerError::Remote {
                            message: format!("request {request_id} sent a body before its head"),
                        });
                    }
                    body.extend_from_slice(payload);
                    if header.flags & flags::LAST != 0 {
                        seen_last_body = true;
                    }
                }
                MessageType::End => {
                    if header.request_id != request_id {
                        return Err(WorkerError::WrongRequestId {
                            expected: request_id,
                            actual: header.request_id,
                        });
                    }
                    response.end = Json::decode(payload)?;
                    break;
                }
                MessageType::Log => self.absorb_log(payload),
                MessageType::Metric => self.absorb_metric(payload),
                MessageType::Event => {
                    tracing::trace!(request_id = header.request_id, "worker sent an event");
                }
                MessageType::Error => {
                    let report: ErrorReport = Json::decode(payload)?;
                    if report.recoverable {
                        tracing::warn!(%report.message, "worker reported a recoverable error");
                    } else {
                        return Err(WorkerError::Remote {
                            message: report.message,
                        });
                    }
                }
                other => {
                    return Err(WorkerError::NotA {
                        expected: "a response frame",
                        actual: other,
                    })
                }
            }
        }

        if !have_head {
            return Err(WorkerError::Remote {
                message: format!("request {request_id} ended without a response head"),
            });
        }
        // Two independent checks, because either one catching a bug is cheaper
        // than debugging a truncated response in production.
        if body.len() as u64 != expected_body {
            return Err(WorkerError::Remote {
                message: format!(
                    "request {request_id} declared {} body bytes but sent {}",
                    expected_body,
                    body.len()
                ),
            });
        }
        if expected_body > 0 && !seen_last_body {
            return Err(WorkerError::Remote {
                message: format!("request {request_id} did not flag its last body frame"),
            });
        }

        response.body = body;
        // Cloned rather than moved: the channel's buffers are documented as the
        // lifetime view (`take_logs`), so a caller that wants everything the
        // worker ever said keeps getting it, while a caller that wants this
        // request's telemetry finds it on the response.
        response.logs = self.logs[log_mark..].to_vec();
        response.metrics = self.metrics[metric_mark..].to_vec();
        Ok(response)
    }

    /// Send the body as `Body` frames, flagging the last one.
    fn send_body(&mut self, request_id: u32, body: &[u8]) -> Result<()> {
        if body.is_empty() {
            return Ok(());
        }

        let mut chunks = body.chunks(MAX_BODY_CHUNK).peekable();
        while let Some(chunk) = chunks.next() {
            let last = chunks.peek().is_none();
            let mut header = Header::new(MessageType::Body, request_id, chunk)?;
            if last {
                header.flags |= flags::LAST;
            }
            write_frame(&mut self.writer, &header, chunk)?;
        }
        Ok(())
    }

    /// Serialize `value` and write it as one frame.
    ///
    /// The counterpart of [`WorkerChannel::send_frame`] for a caller that holds a value rather
    /// than bytes. The pool relays a control frame it built elsewhere through the other one.
    pub fn send_json<T: serde::Serialize>(
        &mut self,
        message_type: MessageType,
        request_id: u32,
        value: &T,
        extra_flags: u16,
    ) -> Result<()> {
        let payload = Json::encode(value)?;
        self.send_frame(message_type, request_id, &payload, extra_flags)
    }

    /// Write one frame from a payload that is already encoded.
    ///
    /// The counterpart of [`WorkerChannel::send_json`] for a caller that holds
    /// bytes rather than a value — the pool relays a serialized control frame it
    /// built elsewhere, and re-encoding it here would mean this crate's server
    /// side needed a serializer of its own.
    pub fn send_frame(
        &mut self,
        message_type: MessageType,
        request_id: u32,
        payload: &[u8],
        extra_flags: u16,
    ) -> Result<()> {
        let mut header = Header::new(message_type, request_id, payload)?;
        header.flags |= extra_flags;
        write_frame(&mut self.writer, &header, payload)
    }

    /// Read one frame, tolerating nothing: a malformed frame is an error.
    pub fn read_frame(&mut self) -> Result<Option<Frame>> {
        let Some((header, payload)) = Header::read_frame(&mut self.reader)? else {
            return Ok(None);
        };
        Ok(Some(Frame { header, payload }))
    }

    /// Read one frame, requiring a specific type.
    fn expect(&mut self, expected: MessageType, what: &'static str) -> Result<(Header, Vec<u8>)> {
        let (header, payload) = self.expect_any(what)?;
        if header.message_type != expected {
            return Err(WorkerError::NotA {
                expected: expected.name(),
                actual: header.message_type,
            });
        }
        Ok((header, payload))
    }

    /// Read one frame, absorbing telemetry and refusing a clean close.
    fn expect_any(&mut self, what: &'static str) -> Result<(Header, Vec<u8>)> {
        loop {
            let Some(frame) = self.read_frame()? else {
                return Err(WorkerError::Closed(format!(
                    "the worker closed its output while sending {what}"
                )));
            };
            match frame.header.message_type {
                MessageType::Log => self.absorb_log(&frame.payload),
                MessageType::Metric => self.absorb_metric(&frame.payload),
                _ => return Ok((frame.header, frame.payload)),
            }
        }
    }

    fn absorb_log(&mut self, payload: &[u8]) {
        match Json::decode::<LogRecord>(payload) {
            Ok(record) => {
                tracing::debug!(level = %record.level, "worker: {}", record.message);
                self.logs.push(record);
            }
            // A log line is not worth failing a request over.
            Err(err) => tracing::warn!(%err, "ignoring an unreadable worker log record"),
        }
    }

    fn absorb_metric(&mut self, payload: &[u8]) {
        match Json::decode::<MetricRecord>(payload) {
            Ok(record) => self.metrics.push(record),
            Err(err) => tracing::warn!(%err, "ignoring an unreadable worker metric"),
        }
    }
}

/// Writes the low-level helpers tests need.
pub fn write_hello_ack<W: Write>(writer: &mut W, ack: &HelloAck) -> Result<()> {
    let payload = Json::encode(ack)?;
    let header = Header::new(MessageType::HelloAck, crate::UNSOLICITED, &payload)?;
    write_frame(writer, &header, &payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{crc32c, HEADER_LEN, PROTOCOL_VERSION};
    use crate::message::HelloAck;

    /// Builds the byte stream a worker would send, frame by frame.
    ///
    /// Driving the real codec over a `Cursor` rather than a mock is the point:
    /// these are the same code paths the pool runs over a live pipe.
    #[derive(Default)]
    struct WorkerScript {
        bytes: Vec<u8>,
    }

    impl WorkerScript {
        fn json<T: serde::Serialize>(
            &mut self,
            message_type: MessageType,
            request_id: u32,
            value: &T,
        ) -> &mut Self {
            let payload = Json::encode(value).unwrap();
            let header = Header::new(message_type, request_id, &payload).unwrap();
            write_frame(&mut self.bytes, &header, &payload).unwrap();
            self
        }

        fn body(&mut self, request_id: u32, chunk: &[u8], last: bool) -> &mut Self {
            let mut header = Header::new(MessageType::Body, request_id, chunk).unwrap();
            if last {
                header.flags |= flags::LAST;
            }
            write_frame(&mut self.bytes, &header, chunk).unwrap();
            self
        }

        /// A response head declaring `body_len` bytes.
        fn head(&mut self, request_id: u32, status: u16, body_len: u64) -> &mut Self {
            self.json(
                MessageType::Response,
                request_id,
                &ResponseHead {
                    status,
                    headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
                    body_len,
                },
            )
        }

        fn channel(&self) -> WorkerChannel<Vec<u8>, std::io::Cursor<Vec<u8>>> {
            WorkerChannel::new(Vec::new(), std::io::Cursor::new(self.bytes.clone()))
        }
    }

    /// Decode the frames a channel wrote, so tests can assert on the request side.
    fn sent_frames(bytes: &[u8]) -> Vec<(Header, Vec<u8>)> {
        let mut cursor = std::io::Cursor::new(bytes);
        let mut frames = Vec::new();
        while let Some(frame) = Header::read_frame(&mut cursor).unwrap() {
            frames.push(frame);
        }
        frames
    }

    /// A worker script that answers `request_id` with exactly `body`.
    fn echoing(body: &str) -> WorkerScript {
        let mut script = WorkerScript::default();
        script
            .head(1, 200, body.len() as u64)
            .body(1, body.as_bytes(), true)
            .json(MessageType::End, 1, &RequestEnd::default());
        script
    }

    #[test]
    fn a_request_and_its_response_round_trip_over_one_channel() {
        let script = echoing("hello");
        let mut channel = script.channel();

        let response = channel
            .request(&RequestHeader::default(), b"")
            .expect("a well-formed exchange must succeed");

        assert_eq!(response.status(), 200);
        assert_eq!(response.body, b"hello");
        assert!(response.keep_alive());
        assert_eq!(response.head.body_len, 5);
        assert_eq!(
            response.head.headers,
            vec![("Content-Type".to_string(), "text/plain".to_string())]
        );
    }

    #[test]
    fn the_first_request_id_is_one_so_zero_stays_reserved() {
        let mut channel = echoing("x").channel();
        assert_eq!(channel.next_request_id(), crate::FIRST_REQUEST_ID);
        assert_eq!(channel.next_request_id(), 1);
        channel.request(&RequestHeader::default(), b"").unwrap();
        assert_eq!(channel.next_request_id(), 2);
    }

    #[test]
    fn the_request_frame_carries_the_declared_body_length() {
        let mut script = WorkerScript::default();
        script
            .head(1, 204, 0)
            .json(MessageType::End, 1, &RequestEnd::default());
        let mut channel = script.channel();

        channel
            .request(&RequestHeader::default(), b"0123456789")
            .unwrap();

        let frames = sent_frames(&channel.writer);
        assert_eq!(frames[0].0.message_type, MessageType::Request);
        assert_eq!(frames[0].0.request_id, 1);
        let sent: RequestHeader = Json::decode(&frames[0].1).unwrap();
        assert_eq!(sent.body_len, 10);
    }

    #[test]
    fn the_borrowed_request_frame_is_byte_identical_to_the_owned_encoding() {
        // `send_request` serializes a *borrowed* view of the header rather than cloning it
        // to stamp in `body_len`, so the two encodings have to agree exactly. Field names,
        // order and defaults are protocol — the PHP SDK decodes this JSON.
        //
        // Comparing bytes rather than round-tripping is the whole point: every field of
        // `RequestHeader` except `method` and `uri` carries a serde default, so a field
        // missing from the borrowed view would not fail `Json::decode::<RequestHeader>` at
        // all. It would just arrive empty, which is exactly the kind of silence a
        // round-trip test cannot catch.
        let request = RequestHeader {
            method: "POST".to_string(),
            uri: "/items?page=2".to_string(),
            protocol: "HTTP/1.1".to_string(),
            headers: vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("X-Trace".to_string(), "abc".to_string()),
            ],
            remote_addr: "10.0.0.7".to_string(),
            body_len: 0,
            script_filename: "/srv/app/public/index.php".to_string(),
            https: true,
        };
        let body = b"{\"a\":1}";
        let expected = Json::encode(&RequestHeader {
            body_len: body.len() as u64,
            ..request.clone()
        })
        .unwrap();

        let mut script = WorkerScript::default();
        script
            .head(1, 200, 0)
            .json(MessageType::End, 1, &RequestEnd::default());
        let mut channel = script.channel();

        channel.request(&request, body).unwrap();

        let frames = sent_frames(&channel.writer);
        assert_eq!(
            frames[0].1, expected,
            "the borrowed frame must encode identically to the cloned one"
        );
    }

    #[test]
    fn an_empty_request_body_sends_no_body_frames() {
        let mut script = WorkerScript::default();
        script
            .head(1, 200, 0)
            .json(MessageType::End, 1, &RequestEnd::default());
        let mut channel = script.channel();

        channel.request(&RequestHeader::default(), b"").unwrap();

        let types: Vec<MessageType> = sent_frames(&channel.writer)
            .into_iter()
            .map(|(header, _)| header.message_type)
            .collect();
        assert_eq!(types, vec![MessageType::Request]);
    }

    #[test]
    fn a_staged_request_reports_the_send_phase_before_the_read_phase() {
        // The send/read split is what lets the pool decide whether a failed
        // request may be replayed, so the order and the count are both load
        // bearing: exactly one call per phase, send first. A codec that reported
        // both after the fact, or read first, would make every failure look
        // replayable and could re-run a POST.
        let mut script = WorkerScript::default();
        script
            .head(1, 200, 0)
            .json(MessageType::End, 1, &RequestEnd::default());
        let mut channel = script.channel();

        let mut stages = Vec::new();
        channel
            .request_staged(&RequestHeader::default(), b"", |stage| stages.push(stage))
            .unwrap();

        assert_eq!(stages, vec![RequestStage::Sending, RequestStage::Reading]);
    }

    #[test]
    fn a_staged_request_that_fails_while_sending_is_reported_as_such() {
        // A body large enough to need several frames, against a writer that reports
        // the pipe closed: the write of the first frame fails, and the stage must say
        // so rather than reporting the read phase. That distinction is the
        // difference between "this POST was never delivered, retry is safe" and
        // "this POST may have run already, do not retry".
        let body = vec![b'z'; MAX_BODY_CHUNK * 4];

        let mut channel = WorkerChannel::new(ClosedWriter, std::io::Cursor::new(Vec::new()));
        let mut stages = Vec::new();
        let result = channel.request_staged(&RequestHeader::default(), &body, |stage| {
            stages.push(stage)
        });

        assert!(result.is_err(), "a write to a closed pipe must fail");
        assert_eq!(
            stages,
            vec![RequestStage::Sending],
            "the failure happened while sending, so the read phase was never reached"
        );
    }

    /// A writer that always reports the pipe as closed.
    #[derive(Debug)]
    struct ClosedWriter;

    impl std::io::Write for ClosedWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_large_request_body_is_chunked_and_the_last_chunk_is_flagged() {
        let body = vec![b'z'; MAX_BODY_CHUNK * 2 + 5];
        let mut script = WorkerScript::default();
        script
            .head(1, 200, 0)
            .json(MessageType::End, 1, &RequestEnd::default());
        let mut channel = script.channel();

        channel.request(&RequestHeader::default(), &body).unwrap();

        let frames = sent_frames(&channel.writer);
        let bodies: Vec<&(Header, Vec<u8>)> = frames
            .iter()
            .filter(|(header, _)| header.message_type == MessageType::Body)
            .collect();
        assert_eq!(bodies.len(), 3, "a 128 KiB + 5 byte body is three chunks");
        assert_eq!(bodies[0].1.len(), MAX_BODY_CHUNK);
        assert_eq!(bodies[1].1.len(), MAX_BODY_CHUNK);
        assert_eq!(bodies[2].1.len(), 5);
        assert_eq!(bodies[0].0.flags & flags::LAST, 0);
        assert_eq!(bodies[1].0.flags & flags::LAST, 0);
        assert_ne!(
            bodies[2].0.flags & flags::LAST,
            0,
            "the last chunk is flagged"
        );

        let reassembled: Vec<u8> = bodies.iter().flat_map(|(_, c)| c.clone()).collect();
        assert_eq!(reassembled, body);
    }

    #[test]
    fn a_declared_body_length_that_does_not_match_is_rejected() {
        let mut script = WorkerScript::default();
        script
            .head(1, 200, 10) // claims ten bytes…
            .body(1, b"four", true) // …sends four
            .json(MessageType::End, 1, &RequestEnd::default());
        let mut channel = script.channel();

        let err = channel.request(&RequestHeader::default(), b"").unwrap_err();
        assert!(err.to_string().contains("declared 10"), "{err}");
        assert!(err.to_string().contains("sent 4"), "{err}");
    }

    #[test]
    fn a_body_whose_last_frame_is_unflagged_is_rejected() {
        let mut script = WorkerScript::default();
        script
            .head(1, 200, 4)
            .body(1, b"four", false) // the right length, but never flagged
            .json(MessageType::End, 1, &RequestEnd::default());
        let mut channel = script.channel();

        let err = channel.request(&RequestHeader::default(), b"").unwrap_err();
        assert!(err.to_string().contains("last body frame"), "{err}");
    }

    #[test]
    fn a_body_before_the_head_is_rejected() {
        let mut script = WorkerScript::default();
        script.body(1, b"premature", false).head(1, 200, 0).json(
            MessageType::End,
            1,
            &RequestEnd::default(),
        );
        let mut channel = script.channel();

        let err = channel.request(&RequestHeader::default(), b"").unwrap_err();
        assert!(err.to_string().contains("before its head"), "{err}");
    }

    #[test]
    fn a_response_head_sent_twice_is_rejected() {
        let mut script = WorkerScript::default();
        script
            .head(1, 200, 0)
            .head(1, 500, 0)
            .json(MessageType::End, 1, &RequestEnd::default());
        let mut channel = script.channel();

        let err = channel.request(&RequestHeader::default(), b"").unwrap_err();
        assert!(err.to_string().contains("answered twice"), "{err}");
    }

    #[test]
    fn an_end_without_a_head_is_rejected() {
        let mut script = WorkerScript::default();
        script.json(MessageType::End, 1, &RequestEnd::default());
        let mut channel = script.channel();

        let err = channel.request(&RequestHeader::default(), b"").unwrap_err();
        assert!(err.to_string().contains("without a response head"), "{err}");
    }

    #[test]
    fn a_wrong_request_id_is_rejected_but_leaves_the_stream_usable() {
        let mut script = WorkerScript::default();
        script
            .head(999, 200, 0)
            .json(MessageType::End, 999, &RequestEnd::default());
        let mut channel = script.channel();

        let err = channel.request(&RequestHeader::default(), b"").unwrap_err();
        assert!(
            matches!(
                err,
                WorkerError::WrongRequestId {
                    expected: 1,
                    actual: 999
                }
            ),
            "{err}"
        );
        // A correlation bug is a bad request, not a broken pipe: the pool must
        // not discard a worker over it.
        assert!(!err.is_fatal());
    }

    #[test]
    fn a_worker_that_closes_mid_response_is_reported_as_closed() {
        let mut channel = WorkerScript::default().channel();

        let err = channel.request(&RequestHeader::default(), b"").unwrap_err();
        assert!(matches!(err, WorkerError::Closed(_)), "{err}");
        assert!(err.is_fatal(), "a closed worker is not reusable");
    }

    #[test]
    fn telemetry_inside_a_response_is_collected_not_treated_as_an_error() {
        let mut script = WorkerScript::default();
        script
            .json(
                MessageType::Log,
                crate::UNSOLICITED,
                &LogRecord {
                    level: "warning".to_string(),
                    message: "deprecated call".to_string(),
                    request_id: 1,
                },
            )
            .head(1, 200, 2)
            .json(
                MessageType::Metric,
                crate::UNSOLICITED,
                &MetricRecord {
                    name: "db_ms".to_string(),
                    value: 1.5,
                    unit: "ms".to_string(),
                },
            )
            .body(1, b"ok", true)
            .json(MessageType::End, 1, &RequestEnd::default());
        let mut channel = script.channel();

        let response = channel
            .request(&RequestHeader::default(), b"")
            .expect("a log line must never fail a request");

        assert_eq!(response.body, b"ok");
        assert_eq!(response.logs.len(), 1);
        assert_eq!(response.logs[0].message, "deprecated call");
        assert_eq!(response.metrics.len(), 1);
        assert_eq!(response.metrics[0].value, 1.5);

        // The same records are also available once the response is consumed.
        assert_eq!(channel.take_logs().len(), 1);
        assert_eq!(channel.take_metrics().len(), 1);
        assert!(channel.take_logs().is_empty(), "take empties the buffer");
    }

    #[test]
    fn a_non_recoverable_error_fails_the_request() {
        let mut script = WorkerScript::default();
        script.head(1, 500, 0).json(
            MessageType::Error,
            1,
            &ErrorReport {
                message: "fatal: out of memory".to_string(),
                request_id: 1,
                recoverable: false,
            },
        );
        let mut channel = script.channel();

        let err = channel.request(&RequestHeader::default(), b"").unwrap_err();
        assert!(matches!(err, WorkerError::Remote { .. }), "{err}");
        assert!(err.to_string().contains("out of memory"), "{err}");
        assert!(!err.is_fatal());
    }

    #[test]
    fn a_recoverable_error_is_a_warning_and_the_request_continues() {
        let mut script = WorkerScript::default();
        script
            .head(1, 200, 2)
            .json(
                MessageType::Error,
                1,
                &ErrorReport {
                    message: "a notice".to_string(),
                    request_id: 1,
                    recoverable: true,
                },
            )
            .body(1, b"ok", true)
            .json(MessageType::End, 1, &RequestEnd::default());
        let mut channel = script.channel();

        let response = channel.request(&RequestHeader::default(), b"").unwrap();
        assert_eq!(response.body, b"ok");
    }

    #[test]
    fn an_unexpected_frame_type_fails_the_request() {
        let mut script = WorkerScript::default();
        script.json(MessageType::Pong, crate::UNSOLICITED, &());
        let mut channel = script.channel();

        let err = channel.request(&RequestHeader::default(), b"").unwrap_err();
        assert!(matches!(err, WorkerError::NotA { .. }), "{err}");
        assert!(err.to_string().contains("a response frame"), "{err}");
    }

    #[test]
    fn request_ids_wrap_without_ever_using_the_reserved_zero() {
        let mut script = WorkerScript::default();
        script
            .head(u32::MAX, 200, 0)
            .json(MessageType::End, u32::MAX, &RequestEnd::default());
        let mut channel = script.channel();
        channel.next_request_id = u32::MAX;

        channel
            .request(&RequestHeader::default(), b"")
            .expect("the wrapped id must still correlate");

        assert_eq!(
            channel.next_request_id(),
            crate::FIRST_REQUEST_ID,
            "the wrap must skip the reserved zero"
        );
        let frames = sent_frames(&channel.writer);
        assert_eq!(frames[0].0.request_id, u32::MAX);
    }

    #[test]
    fn the_handshake_accepts_a_matching_hello_ack() {
        let mut script = WorkerScript::default();
        script.json(
            MessageType::HelloAck,
            crate::UNSOLICITED,
            &HelloAck {
                protocol_version: PROTOCOL_VERSION,
                php_version: "8.3.31".to_string(),
                sapi: "cli".to_string(),
                pid: 4242,
                runtime: "php-cli".to_string(),
            },
        );
        let mut channel = script.channel();

        let ack = channel.handshake().unwrap();
        assert_eq!(ack.pid, 4242);
        assert_eq!(ack.php_version, "8.3.31");

        let frames = sent_frames(&channel.writer);
        assert_eq!(frames[0].0.message_type, MessageType::Hello);
        assert_eq!(frames[0].0.request_id, crate::UNSOLICITED);
    }

    #[test]
    fn the_handshake_rejects_a_mismatched_version_at_boot_not_first_request() {
        let mut script = WorkerScript::default();
        script.json(
            MessageType::HelloAck,
            crate::UNSOLICITED,
            &HelloAck {
                protocol_version: 9,
                ..HelloAck::default()
            },
        );
        let mut channel = script.channel();

        let err = channel.handshake().unwrap_err();
        assert!(
            matches!(
                err,
                WorkerError::UnsupportedVersion {
                    found: 9,
                    expected: 1
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn the_handshake_names_what_it_expected_when_the_worker_sends_the_wrong_frame() {
        let mut script = WorkerScript::default();
        script.json(
            MessageType::Response,
            crate::UNSOLICITED,
            &ResponseHead::default(),
        );
        let mut channel = script.channel();

        let err = channel.handshake().unwrap_err();
        assert!(err.to_string().contains("HELLO_ACK"), "{err}");
    }

    #[test]
    fn control_waits_for_an_acknowledgement() {
        let mut script = WorkerScript::default();
        script.json(MessageType::ControlAck, crate::UNSOLICITED, &());
        let mut channel = script.channel();

        channel.control(&Control::Reload).unwrap();
        let frames = sent_frames(&channel.writer);
        assert_eq!(frames[0].0.message_type, MessageType::Control);
        let sent: Control = Json::decode(&frames[0].1).unwrap();
        assert_eq!(sent, Control::Reload);
    }

    #[test]
    fn drain_and_shutdown_send_their_own_control_kinds() {
        let mut script = WorkerScript::default();
        script
            .json(MessageType::ControlAck, crate::UNSOLICITED, &())
            .json(MessageType::ControlAck, crate::UNSOLICITED, &());
        let mut channel = script.channel();

        channel.drain().unwrap();
        channel.shutdown().unwrap();

        let kinds: Vec<Control> = sent_frames(&channel.writer)
            .into_iter()
            .map(|(_, payload)| Json::decode(&payload).unwrap())
            .collect();
        assert_eq!(kinds, vec![Control::Drain, Control::Shutdown]);
    }

    #[test]
    fn an_error_answering_a_control_becomes_a_remote_failure() {
        let mut script = WorkerScript::default();
        script.json(
            MessageType::Error,
            crate::UNSOLICITED,
            &ErrorReport {
                message: "cannot drain".to_string(),
                request_id: 0,
                recoverable: false,
            },
        );
        let mut channel = script.channel();

        let err = channel.drain().unwrap_err();
        assert!(err.to_string().contains("cannot drain"), "{err}");
    }

    #[test]
    fn ping_accepts_a_pong() {
        let mut script = WorkerScript::default();
        script.json(MessageType::Pong, crate::UNSOLICITED, &());
        let mut channel = script.channel();

        channel.ping().unwrap();
        let frames = sent_frames(&channel.writer);
        assert_eq!(frames[0].0.message_type, MessageType::Ping);
    }

    #[test]
    fn telemetry_before_a_pong_is_absorbed() {
        let mut script = WorkerScript::default();
        script
            .json(
                MessageType::Log,
                crate::UNSOLICITED,
                &LogRecord {
                    level: "info".to_string(),
                    message: "still alive".to_string(),
                    request_id: 0,
                },
            )
            .json(MessageType::Pong, crate::UNSOLICITED, &());
        let mut channel = script.channel();

        channel
            .ping()
            .expect("a log line must not fail a health check");
        assert_eq!(channel.take_logs().len(), 1);
    }

    #[test]
    fn ping_refuses_anything_that_is_not_a_pong() {
        let mut script = WorkerScript::default();
        script.head(crate::UNSOLICITED, 200, 0);
        let mut channel = script.channel();

        let err = channel.ping().unwrap_err();
        // The error names the type it wanted by its wire name, so a log line
        // reading "not a valid PONG" is unambiguous.
        assert!(err.to_string().contains("not a valid PONG"), "{err}");
    }

    #[test]
    fn a_corrupted_frame_on_the_worker_side_is_fatal() {
        let mut bytes = {
            let mut script = WorkerScript::default();
            script.head(1, 200, 2);
            script.bytes
        };
        // Flip a payload bit; the checksum must catch it.
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        let mut channel = WorkerChannel::new(Vec::new(), std::io::Cursor::new(bytes));

        let err = channel.request(&RequestHeader::default(), b"").unwrap_err();
        assert!(matches!(err, WorkerError::ChecksumMismatch { .. }), "{err}");
        assert!(err.is_fatal(), "framing has desynchronised");
    }

    #[test]
    fn the_codec_reports_its_own_constants() {
        // Pinned here as well as in the wire-format integration test, because a
        // silent change to either value is a protocol break.
        assert_eq!(HEADER_LEN, 16);
        assert_eq!(MAX_BODY_CHUNK, 64 * 1024);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }
}
