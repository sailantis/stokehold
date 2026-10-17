//! The message payloads, and the JSON codec behind them.
//!
//! Payloads are JSON in version 1. That choice is behind [`Codec`] so the wire
//! framing never has to change if profiling ever justifies msgpack: the header,
//! the checksum and the framing are payload-agnostic by construction.
//!
//! Bodies are deliberately **not** JSON. A request's body travels in [`Body`]
//! frames as raw bytes, because base64 inside JSON inflates an upload by a third
//! and makes the runtime do work per byte on the hot path.
//!
//! The field names here are part of the protocol, because the PHP SDK decodes
//! the same JSON. Renaming a field is a protocol change.

use serde::{Deserialize, Serialize};

/// The server's opening frame: the protocol version, and the policies it wants used.
///
/// A dedicated struct rather than the `RequestHeader` this used to reuse. Two reasons: a
/// `RequestHeader` travels on *every* request, so a policy field there would add bytes to
/// the hot path, and a `HELLO` carrying request-shaped JSON read as though a request were
/// being described. The field names are part of the protocol, because the PHP SDK decodes
/// the same JSON.
///
/// `no_checksum` defaults to `false`, which is the fail-safe direction: a peer that does not
/// know the field keeps verifying checksums, and a server that says nothing means "required".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The protocol version the server speaks.
    ///
    /// Previously absent, because the reused `RequestHeader` had no such field and the SDK
    /// defaulted it to its own. Stating it makes a mismatched pair visible at the first frame.
    pub protocol_version: u8,
    /// Whether the worker may omit the per-frame checksum.
    #[serde(default)]
    pub no_checksum: bool,
}

/// The PHP and volucer versions a worker reports at handshake.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloAck {
    /// The worker's own protocol version, so a mismatched SDK is caught at
    /// handshake rather than on the first request.
    pub protocol_version: u8,
    /// `PHP_VERSION`.
    #[serde(default)]
    pub php_version: String,
    /// `PHP_SAPI`.
    #[serde(default)]
    pub sapi: String,
    /// The worker process id, for logging.
    #[serde(default)]
    pub pid: u32,
    /// What the worker's bootstrap reported, free-form and for logs only.
    #[serde(default)]
    pub runtime: String,
    /// Whether the worker agreed to omit the per-frame checksum.
    ///
    /// The worker's *answer*, not its preference: a server that requires checksums will not
    /// have offered, so an absent or `false` value means the worker is checksumming. Recorded
    /// so the pool can see what a worker actually agreed to rather than assuming it followed.
    #[serde(default)]
    pub no_checksum: bool,
}

/// What the server tells a worker when asking it to serve a request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestHeader {
    pub method: String,
    /// The full request target, e.g. `/index.php/foo?x=1`.
    pub uri: String,
    #[serde(default)]
    pub protocol: String,
    /// Request headers, preserving order and case as received.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Client address, for `REMOTE_ADDR`.
    #[serde(default)]
    pub remote_addr: String,
    /// How many body bytes follow in `Body` frames, so the worker can decide
    /// whether to buffer or stream before reading a single one.
    #[serde(default)]
    pub body_len: u64,
    /// The absolute script the server routed to.
    #[serde(default)]
    pub script_filename: String,
    /// Whether the request arrived over TLS.
    #[serde(default)]
    pub https: bool,
}

impl RequestHeader {
    /// Look up a header case-insensitively, the way HTTP requires.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The content type, if the client sent one.
    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }

    /// A borrowed view of this header, with `body_len` supplied by the caller.
    ///
    /// Every field is borrowed, so this allocates nothing. It exists so an owned
    /// `RequestHeader` can be framed without cloning it, and so the frame's shape has one
    /// definition: the `body_len` that matters is the length of the body about to be framed,
    /// which the caller holds and this type cannot see.
    pub fn draft(
        &self,
        body_len: u64,
    ) -> crate::codec::RequestHeaderJson<'_, crate::codec::HeaderPairs<'_>> {
        crate::codec::RequestHeaderJson {
            method: &self.method,
            uri: &self.uri,
            protocol: &self.protocol,
            headers: crate::codec::BorrowedHeaders::new(crate::codec::HeaderPairs(
                self.headers.iter(),
            )),
            remote_addr: &self.remote_addr,
            body_len,
            script_filename: &self.script_filename,
            https: self.https,
        }
    }
}

/// The response head a worker sends before its body.
///
/// `Default` is hand-written rather than derived so it agrees with the serde
/// defaults below: an omitted field and a constructed default must not mean
/// different things.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseHead {
    /// HTTP status code.
    #[serde(default = "default_status")]
    pub status: u16,
    /// Response headers, in the order the application produced them.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// How many body bytes follow in `Body` frames.
    #[serde(default)]
    pub body_len: u64,
}

impl Default for ResponseHead {
    fn default() -> Self {
        ResponseHead {
            status: default_status(),
            headers: Vec::new(),
            body_len: 0,
        }
    }
}

fn default_status() -> u16 {
    200
}

/// The terminal message for a request: how it went, and what it cost.
///
/// `app_status` is the *application's* exit status and is **not** the HTTP
/// status. A worker that throws can report `app_status = 1` while the response
/// head still carries `status = 500`; conflating the two is how a protocol ends
/// up unable to express a failed request that produced a valid error page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestEnd {
    /// The application's own status. Non-zero is normal and not a protocol
    /// failure.
    #[serde(default)]
    pub app_status: i32,
    /// The worker's peak resident set in bytes, so the pool can recycle on a
    /// threshold without having to ask the operating system.
    #[serde(default)]
    pub peak_memory_bytes: u64,
    /// How long the application spent handling the request, in microseconds.
    /// Measured by the worker, so it excludes volucer's own overhead and makes
    /// the two comparable when profiling.
    #[serde(default)]
    pub duration_us: u64,
    /// Whether this worker is willing to serve another request. A worker that
    /// handled a fatal error reports `false` and is replaced instead of reused.
    #[serde(default = "default_true")]
    pub keep_alive: bool,
}

/// Hand-written for the same reason as [`ResponseHead::default`]: a constructed
/// default must not contradict the serde default for an omitted field.
impl Default for RequestEnd {
    fn default() -> Self {
        RequestEnd {
            app_status: 0,
            peak_memory_bytes: 0,
            duration_us: 0,
            keep_alive: default_true(),
        }
    }
}

fn default_true() -> bool {
    true
}

/// A log record emitted by a worker, outside any request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRecord {
    /// RFC 5424-ish level name: `emergency`, `alert`, `critical`, `error`,
    /// `warning`, `notice`, `info`, `debug`.
    #[serde(default)]
    pub level: String,
    #[serde(default)]
    pub message: String,
    /// The request this log line belongs to, when it happened inside one.
    #[serde(default)]
    pub request_id: u32,
}

/// A metric sample.
///
/// `PartialEq` only — an `f64` cannot be `Eq` (`NaN != NaN`), and a metric is a
/// measurement rather than an identifier, so exact equality is not a
/// meaningful claim to make about it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MetricRecord {
    pub name: String,
    /// Kept as `f64` because a metric that could only be an integer would be
    /// useless for latency and memory ratios.
    pub value: f64,
    #[serde(default)]
    pub unit: String,
}

/// A control message from the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Control {
    /// Stop accepting new work and exit once the current request finishes.
    Drain,
    /// Exit immediately; the current request is lost.
    Shutdown,
    /// Re-run the worker's bootstrap without exiting, for a boot cache.
    Reload,
    /// Ask the worker to report its memory usage.
    Report,
}

/// A call the server makes to a running worker.
///
/// **The request is the method and its parameters, and the server never interprets either.** A call
/// is `"<namespace>.<verb>"` — `kv.get`, `lock.acquire`, `metrics.observe` — and the params are JSON
/// the namespace's handler understands. Keeping the payload opaque here is what lets the frame layer
/// stay ignorant of what a KV store is: the exchange thread hands the call to a handler and writes
/// back whatever comes out.
///
/// `name` rather than `method` because `method` is already the HTTP verb on [`RequestHeader`], and two
/// fields called `method` in one protocol is how a reader comes to believe a call carries an HTTP
/// request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Call {
    /// `"<namespace>.<verb>"`, e.g. `kv.get`.
    pub name: String,
    /// The parameters, as the namespace's handler expects them. JSON, and opaque here.
    #[serde(default)]
    pub params: serde_json::Value,
    /// The `traceparent` this call belongs to, so a span can continue across the call.
    ///
    /// Carried from the start rather than added later, because it costs a field now and is a protocol
    /// change once anything is deployed. `docs/plugins.md` §6.5 is where that argument lives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
}

/// The worker's answer to a [`Call`].
///
/// **An error is a successful `Return` with `ok: false`**, not an `ERROR` frame. The distinction
/// matters: an `ERROR` frame describes a protocol failure and can be unrecoverable, while a call that
/// could not be served — an unknown method, a backend that timed out, a lock whose state is unknown —
/// is an ordinary answer to an ordinary question. Sending `ERROR` for those would make a missing KV
/// entry look like a broken worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Return {
    /// Whether the call was served.
    pub ok: bool,
    /// The result, when `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Why it was not, when not `ok`. Free-form, and read by a human.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Whether the failure left the call's effect in an unknown state.
    ///
    /// **A third answer, not a boolean pair.** `ok: false` with `unknown: false` means nothing
    /// happened — an unknown method, a value that is not there. `ok: false` with `unknown: true` means
    /// the call may or may not have taken effect: a lock acquired before the reply was lost, a write
    /// that was sent and not acknowledged. The caller must resolve that by asking, and an API that
    /// collapsed the two would make "the write failed" and "the write may have happened" the same
    /// answer — which is how a retry duplicates a side effect. This is the ambiguity `docs/plugins.md`
    /// §6.3 records RoadRunner's own lock implementation as having, modelled explicitly rather than
    /// left to a comment.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unknown: bool,
}

impl Return {
    /// A served call.
    pub fn ok(result: serde_json::Value) -> Self {
        Return {
            ok: true,
            result: Some(result),
            error: None,
            unknown: false,
        }
    }

    /// A call that did not happen.
    pub fn refused(reason: impl Into<String>) -> Self {
        Return {
            ok: false,
            result: None,
            error: Some(reason.into()),
            unknown: false,
        }
    }

    /// A call whose effect cannot be determined.
    pub fn unknown_state(reason: impl Into<String>) -> Self {
        Return {
            ok: false,
            result: None,
            error: Some(reason.into()),
            unknown: true,
        }
    }
}

/// A description of a failure, used in `Error` frames.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorReport {
    #[serde(default)]
    pub message: String,
    /// The request the failure belongs to, when it is not the frame's own id.
    #[serde(default)]
    pub request_id: u32,
    /// Whether the worker considers itself usable afterwards.
    #[serde(default)]
    pub recoverable: bool,
}

/// A payload encoder. JSON today; the indirection is the extension point.
pub trait Codec {
    fn encode<T: Serialize>(value: &T) -> crate::Result<Vec<u8>>;
    /// The same encoding, into a buffer the caller owns and will reuse.
    ///
    /// The request frame is encoded once per request, and the channel that sends it lives
    /// for many requests, so a `Vec` per request is pure churn: the bytes are handed to
    /// [`write_frame`] and dropped immediately. The buffer is cleared, not shrunk, so the
    /// capacity outlives the request that needed it.
    fn encode_into<T: Serialize + ?Sized>(out: &mut Vec<u8>, value: &T) -> crate::Result<()>;
    fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> crate::Result<T>;
}

/// The JSON codec, which is what version 1 negotiates.
#[derive(Debug, Clone, Copy, Default)]
pub struct Json;

impl Codec for Json {
    fn encode<T: Serialize>(value: &T) -> crate::Result<Vec<u8>> {
        Ok(serde_json::to_vec(value)?)
    }

    fn encode_into<T: Serialize + ?Sized>(out: &mut Vec<u8>, value: &T) -> crate::Result<()> {
        out.clear();
        serde_json::to_writer(std::io::Cursor::new(&mut *out), value)?;
        Ok(())
    }

    fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> crate::Result<T> {
        Ok(serde_json::from_slice(bytes)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_header_round_trips_and_finds_headers_case_insensitively() {
        let header = RequestHeader {
            method: "POST".to_string(),
            uri: "/index.php?x=1".to_string(),
            protocol: "HTTP/1.1".to_string(),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            remote_addr: "127.0.0.1".to_string(),
            body_len: 17,
            script_filename: "/srv/app/public/index.php".to_string(),
            https: true,
        };

        let encoded = Json::encode(&header).unwrap();
        let decoded: RequestHeader = Json::decode(&encoded).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(decoded.header("content-type"), Some("application/json"));
        assert_eq!(decoded.content_type(), Some("application/json"));
        assert!(decoded.header("absent").is_none());
    }

    #[test]
    fn a_missing_content_type_is_none_not_an_empty_string() {
        let header = RequestHeader::default();
        assert_eq!(header.content_type(), None);
    }

    #[test]
    fn a_response_without_a_status_defaults_to_200() {
        let decoded: ResponseHead = Json::decode(b"{}").unwrap();
        assert_eq!(decoded.status, 200);
        assert!(decoded.headers.is_empty());
    }

    #[test]
    fn a_missing_keep_alive_means_keep_alive() {
        // The permissive default is deliberate: a worker that omits the field
        // is not asking to be replaced.
        let end: RequestEnd = Json::decode(b"{}").unwrap();
        assert!(end.keep_alive);
        assert_eq!(end.app_status, 0);
    }

    #[test]
    fn a_non_zero_app_status_survives_the_round_trip() {
        let end = RequestEnd {
            app_status: -1,
            peak_memory_bytes: 1024,
            duration_us: 42,
            keep_alive: false,
        };
        let decoded: RequestEnd = Json::decode(&Json::encode(&end).unwrap()).unwrap();
        assert_eq!(decoded, end);
    }

    #[test]
    fn control_messages_are_tagged_by_kind() {
        let encoded = Json::encode(&Control::Drain).unwrap();
        assert_eq!(String::from_utf8(encoded).unwrap(), r#"{"kind":"drain"}"#);
        let decoded: Control = Json::decode(br#"{"kind":"reload"}"#).unwrap();
        assert_eq!(decoded, Control::Reload);
    }

    #[test]
    fn a_log_record_round_trips() {
        let record = LogRecord {
            level: "warning".to_string(),
            message: "deprecated call".to_string(),
            request_id: 9,
        };
        let decoded: LogRecord = Json::decode(&Json::encode(&record).unwrap()).unwrap();
        assert_eq!(decoded, record);
    }

    #[test]
    fn a_metric_keeps_fractional_values() {
        let metric = MetricRecord {
            name: "latency_ms".to_string(),
            value: 1.5,
            unit: "ms".to_string(),
        };
        let decoded: MetricRecord = Json::decode(&Json::encode(&metric).unwrap()).unwrap();
        assert_eq!(decoded.value, 1.5);
    }

    #[test]
    fn invalid_json_is_an_error_not_a_panic() {
        let err = Json::decode::<RequestHeader>(b"{not json").unwrap_err();
        assert!(matches!(err, crate::WorkerError::Json(_)), "{err}");
        assert!(err.is_fatal());
    }

    #[test]
    fn a_hello_ack_only_needs_the_version() {
        let ack: HelloAck = Json::decode(b"{\"protocol_version\":1}").unwrap();
        assert_eq!(ack.protocol_version, 1);
        assert!(ack.php_version.is_empty());
    }
}
