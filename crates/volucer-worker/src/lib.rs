//! The volucer worker protocol: framed transport for persistent PHP workers.
//!
//! Worker mode boots a PHP application once and serves many requests from it —
//! the value proposition of RoadRunner and Swoole, without embedding PHP. The
//! channel is the child process's **stdin/stdout pipe**: portable, no port per
//! worker, and no socket exhaustion.
//!
//! Why a bespoke protocol rather than goridge or FastCGI:
//!
//! - **FastCGI cannot carry this.** It is server-driven and half-duplex: the
//!   worker can only answer the request it was given, so it has no way to push a
//!   log line, a metric or a "recycle me" notice. Worker mode is exactly the
//!   case where the worker needs to talk first.
//! - **goridge is a framing codec, not a protocol.** Adopting its 12-byte header
//!   saves the easy part while importing a versioning commitment; adopting its
//!   semantics means becoming a RoadRunner clone. This crate models the same
//!   proven header shape and owns a message set.
//!
//! The wire format is specified in `docs/worker-protocol.md` and pinned by the
//! tests here. The two rules that prevent the classic bugs living in a worker
//! protocol — output rebinding and shutdown-by-closing-stdin — are described
//! there too, and the PHP SDK (`sailantis/volucer-php-sdk`) implements them.
//!
//! ```text
//!   volucer ──REQUEST──►  php worker (stdin)
//!             ◄─RESPONSE─  … ──BODY──►  … ──END──  (stdout)
//!             ◄─LOG / METRIC / EVENT─              (unsolicited)
//! ```
//!
//! Everything is symmetric: [`Header::read_frame`] reads what
//! [`write_frame`] wrote, so the same codec serves both ends.

pub mod codec;
pub mod error;
pub mod header;
pub mod message;

pub use codec::{
    BorrowedHeaders, BoundedChannel, BufferedReader, BufferedWriter, CallHandler,
    RequestHeaderJson, RequestStage, WorkerChannel, MAX_BODY_CHUNK,
};
pub use error::{Result, WorkerError};
pub use header::{
    crc32c, flags, write_frame, Checksum, Header, MessageType, HEADER_LEN, MAX_PAYLOAD_LEN,
    PROTOCOL_VERSION,
};
pub use message::{
    Call, Codec, Control, ErrorReport, Hello, HelloAck, Json, LogRecord, MetricRecord, RequestEnd,
    RequestHeader, ResponseHead, Return,
};

/// The request id a worker uses for unsolicited frames.
pub const UNSOLICITED: u32 = 0;

/// The first request id the server allocates.
///
/// Starting at 1 rather than 0 keeps `0` unambiguously "not a request", which is
/// what makes a stray correlation bug visible instead of silently matching.
pub const FIRST_REQUEST_ID: u32 = 1;

/// The first call id the server allocates, counting **down** from `u32::MAX`.
///
/// The opposite end of the range from request ids, for the same reason `0` is reserved: a call
/// mistaken for a request — or the reverse — is then a visible correlation failure rather than a
/// silent match. The two ranges meet only after four billion of one or the other, which is a
/// collision a running process will not see.
pub const FIRST_CALL_ID: u32 = u32::MAX;

/// The midpoint of the id space: at or above this, an id is a call's rather than a request's.
///
/// A *threshold* rather than a comparison against [`FIRST_CALL_ID`], because that would only recognise
/// the first call and refuse the rest. Requests count up from 1 and calls count down from `u32::MAX`, so
/// the half each owns is decided by which side of the midpoint an id falls on — and the two are
/// disjoint for four billion of either, which is longer than a process runs.
pub const FIRST_CALL_ID_HALF: u32 = u32::MAX / 2;
