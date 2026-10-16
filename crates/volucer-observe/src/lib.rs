//! Request identity and trace context: the two things a request log and a trace share.
//!
//! This crate exists because volucer has **two front-ends** — an HTTP listener and a FastCGI
//! listener for a deployment behind nginx — and a request id that only one of them assigned would
//! be worse than none at all: the same deployment would log one id for a direct request and a
//! different one for the same request proxied. That is the reasoning `volucer-watch` already states
//! for the reloaded state table, and it applies here for the same reason. It is deliberately *not*
//! in `volucer-core`, which is configuration and shared types; an id is neither.
//!
//! # What this is, before it is anything else
//!
//! Two small values and the rules for reading them off the wire:
//!
//! - [`RequestId`] — 32 hex characters when this server generates one, which is exactly nginx's
//!   `$request_id` (`ngx_http_variables.c`: 16 random bytes, `ngx_hex_dump`ed). The length is not
//!   arbitrary; matching it means a log line written by this server and one written by an nginx in
//!   front of it have the same shape, so one `awk` reads both.
//! - [`TraceContext`] — the W3C `traceparent` header, parsed and formatted by hand.
//!
//! **Neither needs OpenTelemetry**, and that is the point of the split. A structured request line
//! and a propagatable trace context are a header convention and a random number; only *exporting*
//! spans needs a tracing framework. Building the two cheap things first is also what makes the
//! framework worth adding later, because there will be spans to export rather than an exporter
//! wired to nothing.
//!
//! # Adopting a client's id does not re-render it
//!
//! [`RequestId::parse`] accepts an inbound id and [`RequestId::as_str`] returns it **exactly as it
//! arrived**. Folding it into 16 bytes and printing that would defeat the entire purpose of
//! adopting one: the client is looking for *its* string in *our* log, and a normalised value is not
//! findable. So the type holds either the 16 bytes it generated or the string it was handed, and
//! the two render differently on purpose.
//!
//! # Randomness has a fallback, and it is not a security claim
//!
//! A request id is **not** a token: nothing is authorised by it, and guessing one reveals nothing.
//! So when the system RNG is unavailable this crate degrades to a time-and-counter value and warns
//! once, rather than failing the request. A server that refuses traffic because it could not mint a
//! log identifier has its priorities backwards. The trace id is treated the same way, and for the
//! same reason — W3C requires it to be unpredictable only to stop a hostile client *predicting* a
//! trace to inject into, which is a different threat model from an RNG outage on the server.

use std::borrow::Cow;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// The header an inbound request id is read from, and the one the response echoes.
///
/// Lowercase, because HTTP/1.1 header names are case-insensitive and `hyper`'s `HeaderName` is
/// normalised — so one spelling here is what a comparison elsewhere can rely on.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// The W3C trace context header.
pub const TRACEPARENT_HEADER: &str = "traceparent";

/// The longest request id this will adopt from a client.
///
/// A cap rather than a preference: the value ends up in a log line, so an unbounded one is an
/// unbounded log line, and adopting a client's own id means the client chooses how much of the log
/// it owns. 64 fits a UUID twice over with room for a prefix.
const MAX_REQUEST_ID_LEN: usize = 64;

/// A `traceparent` of version `00` is exactly this long.
///
/// `2 + 1 + 32 + 1 + 16 + 1 + 2`. The only longer value the specification allows is a *later*
/// version, and this crate understands one version — see [`TraceContext::parse`].
const TRACEPARENT_LEN: usize = 55;

/// A request identifier: 16 random bytes this server minted, or a client's own string adopted
/// verbatim.
///
/// Not `Copy`, because the adopted form owns its text. That is the honest cost of not re-rendering
/// what a client sent — see the crate docs.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum RequestId {
    /// Minted here: 16 random bytes, rendered as 32 lowercase hex characters.
    Generated([u8; 16]),
    /// Taken from the client's own header, kept exactly as it arrived.
    Adopted(Box<str>),
}

impl RequestId {
    /// Mint a new id.
    ///
    /// Infallible by design: see the crate docs. A request id is not a security token, so an RNG
    /// failure degrades rather than refusing to serve.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0u8; 16];
        fill(&mut bytes);
        Self::Generated(bytes)
    }

    /// An id from raw bytes.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self::Generated(bytes)
    }

    /// An id from a client's header, or `None` if it is not one this server will adopt.
    ///
    /// **Strict on purpose.** The value goes into a log line, so anything that is not a plain
    /// identifier is refused rather than escaped: a header containing a newline would otherwise let
    /// a client forge whole log records, and one containing a space would make the field count of a
    /// space-separated format wrong. Refusing costs a fresh id; escaping costs a rule that every
    /// future reader of the log has to know.
    ///
    /// The accepted alphabet is what real request ids are made of — a UUID's hyphens, hex, and the
    /// underscore and dot a few gateways use — between 1 and [`MAX_REQUEST_ID_LEN`] characters.
    /// A leading or trailing `-` or `.` is refused because such a value is indistinguishable from a
    /// truncated one, and a truncated id is worse than a fresh one: it would correlate two
    /// different requests.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        if value.is_empty() || value.len() > MAX_REQUEST_ID_LEN {
            return None;
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return None;
        }
        if value.starts_with(['-', '.']) || value.ends_with(['-', '.']) {
            return None;
        }
        Some(Self::Adopted(value.into()))
    }

    /// The id as it should appear in a log line or on a response header.
    ///
    /// For [`RequestId::Generated`] this is the 32-character hex form; for
    /// [`RequestId::Adopted`] it is the client's own string, unchanged.
    #[must_use]
    pub fn as_str(&self) -> Cow<'_, str> {
        match self {
            Self::Generated(bytes) => {
                let mut out = String::with_capacity(32);
                push_hex(&mut out, bytes);
                Cow::Owned(out)
            }
            Self::Adopted(text) => Cow::Borrowed(text),
        }
    }

    /// The rendered form, written into an existing `String`.
    ///
    /// There so a caller formatting a log line does not allocate twice for one id. An adopted id is
    /// appended as-is, for the reason in the crate docs.
    pub fn write_str(&self, out: &mut String) {
        match self {
            Self::Generated(bytes) => push_hex(out, bytes),
            Self::Adopted(text) => out.push_str(text),
        }
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.as_str().as_bytes() {
            // A character-wise write, because the adopted form is text rather than bytes to
            // re-encode. Each byte is ASCII by `parse`'s rule or hex by `push_hex`.
            formatter.write_str(&(*byte as char).to_string())?;
        }
        Ok(())
    }
}

impl fmt::Debug for RequestId {
    /// The rendered form, not the struct.
    ///
    /// A `Debug` that printed `Generated([71, 3, ...])` would make every log line and every test
    /// failure harder to read than the thing it describes, and would be a second rendering of the
    /// same value to keep in step.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.as_str())
    }
}

/// A parsed or generated W3C trace context.
///
/// `trace-id` identifies the whole trace and is shared by every service on the path; `span-id`
/// identifies *this* hop. A proxy that continues a trace keeps the trace id and mints a new span
/// id, which is why [`TraceContext::continuing`] exists separately from [`TraceContext::parse`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TraceContext {
    trace_id: [u8; 16],
    span_id: [u8; 8],
    sampled: bool,
}

impl TraceContext {
    /// Begin a new trace, with a fresh trace id and span id.
    ///
    /// `sampled` is a parameter rather than a default because the flag is a *statement* to every
    /// downstream service about whether this request was recorded. Defaulting it would be this
    /// crate claiming to know the deployment's sampling policy, which it does not.
    #[must_use]
    pub fn new(sampled: bool) -> Self {
        Self::continuing(random_trace_id(), sampled)
    }

    /// Continue an existing trace with a new span id for this hop.
    ///
    /// This is what a proxy does with an inbound `traceparent`: the trace is the client's, the span
    /// is ours. Keeping the *client's* span id instead would make two different hops claim to be
    /// the same span, which is what breaks a waterfall diagram.
    #[must_use]
    pub fn continuing(trace_id: [u8; 16], sampled: bool) -> Self {
        Self {
            trace_id,
            span_id: random_span_id(),
            sampled,
        }
    }

    /// Parse a `traceparent` header value.
    ///
    /// **Version `00` only**, and that is a decision rather than an omission: the specification
    /// says a receiver must reject a value whose version it does not understand when the value is
    /// longer than the version-`00` form. Ignoring an unknown version starts a new trace, which
    /// loses a link — a cosmetic loss — where guessing at the layout of a future format risks
    /// reading a *different* field as the trace id and joining a trace that is not ours.
    ///
    /// Returns the client's own context, including its span id. A caller propagating onward wants
    /// [`TraceContext::continuing`] with [`TraceContext::trace_id`] — see that method for why.
    ///
    /// `None` for anything malformed, including the all-zero trace or span id the specification
    /// forbids, an `ff` version, and trailing data. A caller must treat `None` as "start a new
    /// trace", never as an error: a client's broken tracing header is not a broken request.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        if value.len() != TRACEPARENT_LEN {
            return None;
        }
        let bytes = value.as_bytes();
        // Fixed positions, checked before slicing so a `-` in the wrong place is a `None` rather
        // than a panic.
        //
        // **These three checks are also what makes the fixed slicing below safe on a non-ASCII
        // value**, and that is worth stating because it is not obvious and an `is_ascii()` guard
        // looked necessary until a falsification showed it was not. A `str` slice panics if either
        // end lands inside a character, and every boundary used here is `3`, `35`, `36`, `52`, `53`
        // or `55`: bytes `2`, `35` and `52` are proven to be `-` *by these checks*, so they are
        // each a whole character, and `3`, `36` and `53` immediately follow one; `55` is the end.
        // A value containing a multi-byte character therefore either fails one of these three
        // comparisons or reaches `parse_hex_array`, which rejects any byte that is not lowercase
        // hex. The guard was removed rather than kept as decoration — a check nothing can fail is a
        // claim about the code that is not true.
        if bytes[2] != b'-' || bytes[35] != b'-' || bytes[52] != b'-' {
            return None;
        }
        if &bytes[0..2] != b"00" {
            return None;
        }
        let trace_id = parse_hex_array::<16>(&value[3..35])?;
        let span_id = parse_hex_array::<8>(&value[36..52])?;
        if trace_id == [0u8; 16] || span_id == [0u8; 8] {
            return None;
        }
        let flags = parse_hex_array::<1>(&value[53..55])?;
        // Bit 0 is `sampled`. The other seven bits are reserved and are carried no further: this
        // crate does not know what a future revision means by them, and pretending to would be the
        // same mistake as accepting an unknown version.
        Some(Self {
            trace_id,
            span_id,
            sampled: flags[0] & 0x01 == 0x01,
        })
    }

    /// The `traceparent` value to send onward.
    #[must_use]
    pub fn header(&self) -> String {
        let mut out = String::with_capacity(TRACEPARENT_LEN);
        out.push_str("00-");
        push_hex(&mut out, &self.trace_id);
        out.push('-');
        push_hex(&mut out, &self.span_id);
        out.push('-');
        out.push_str(if self.sampled { "01" } else { "00" });
        out
    }

    /// The trace id, shared by every service on the path.
    #[must_use]
    pub fn trace_id(&self) -> [u8; 16] {
        self.trace_id
    }

    /// This hop's span id.
    #[must_use]
    pub fn span_id(&self) -> &[u8; 8] {
        &self.span_id
    }

    /// Whether the originating service recorded this request.
    #[must_use]
    pub fn sampled(&self) -> bool {
        self.sampled
    }

    /// The trace id as lowercase hex, for a structured log field.
    ///
    /// Trace ids are conventionally logged in this form — the same 32 characters a collector shows
    /// — so a value copied out of a log line can be pasted into a trace query unchanged.
    #[must_use]
    pub fn trace_id_hex(&self) -> String {
        let mut out = String::with_capacity(32);
        push_hex(&mut out, &self.trace_id);
        out
    }

    /// The span id as lowercase hex.
    #[must_use]
    pub fn span_id_hex(&self) -> String {
        let mut out = String::with_capacity(16);
        push_hex(&mut out, &self.span_id);
        out
    }
}

impl fmt::Debug for TraceContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.header())
    }
}

/// A fresh trace id, never all zero.
fn random_trace_id() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    fill(&mut bytes);
    // All-zero ids are invalid per the specification, and a random source could in principle
    // produce one. Cheap to rule out, and a zero id is dropped by every collector.
    if bytes == [0u8; 16] {
        bytes[15] = 1;
    }
    bytes
}

/// A fresh span id, never all zero.
fn random_span_id() -> [u8; 8] {
    let mut bytes = [0u8; 8];
    fill(&mut bytes);
    if bytes == [0u8; 8] {
        bytes[7] = 1;
    }
    bytes
}

/// Fill `dest` from the system RNG, falling back to [`fallback_fill`].
///
/// The fallback is **predictable**, and the crate docs say why that is acceptable for these two
/// values. What it must not be is *repeated*: a counter is mixed in so two requests in the same
/// nanosecond cannot collide, which a bare timestamp would allow under load.
fn fill(dest: &mut [u8]) {
    if getrandom::fill(dest).is_ok() {
        return;
    }
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            "the system random number source is unavailable; request ids and trace ids will be \
             predictable until it recovers. This affects log correlation only — nothing is \
             authorised by these values"
        );
    });
    fallback_fill(dest);
}

/// The no-RNG path: a monotonic counter spread across the whole word.
///
/// Split out from [`fill`] rather than written inline, so the one test that can exercise it does
/// not have to reimplement it. A test that copied this body would be a test of the copy — and the
/// copy would keep passing after the real one was changed, which is precisely the hazard this
/// project has already recorded twice.
fn fallback_fill(dest: &mut [u8]) {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64);
    let mut seed = nanos ^ sequence.rotate_left(17);
    for slot in dest.iter_mut() {
        // SplitMix64's finaliser, which is what spreads a counter's low bits across the whole word
        // — a raw counter would put almost all of its movement in one byte.
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut mixed = seed;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        mixed ^= mixed >> 31;
        *slot = (mixed >> 24) as u8;
    }
}

/// One lowercase hex digit.
const fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'a' + nibble - 10) as char,
    }
}

/// Append `bytes` as lowercase hex.
fn push_hex(out: &mut String, bytes: &[u8]) {
    for byte in bytes {
        out.push(hex_digit(byte >> 4));
        out.push(hex_digit(byte & 0x0f));
    }
}

/// Read exactly `N` bytes from `2 * N` hex characters.
///
/// `None` on any non-hex character, on a wrong length, and on the uppercase the specification does
/// not allow in a `traceparent`.
fn parse_hex_array<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    let bytes = text.as_bytes();
    for (index, slot) in out.iter_mut().enumerate() {
        let high = hex_value(bytes[index * 2])?;
        let low = hex_value(bytes[index * 2 + 1])?;
        *slot = (high << 4) | low;
    }
    Some(out)
}

/// One hex digit's value, rejecting the uppercase a `traceparent` may not contain.
const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_request_id_is_nginx_shaped() {
        // The length is the compatibility claim: 32 hex characters is exactly what nginx's
        // `$request_id` renders, so one `awk` reads both logs. A change that made this 36 (a UUID)
        // would silently break that and no functional test would notice.
        let id = RequestId::generate();
        let text = id.to_string();
        assert_eq!(text.len(), 32, "{text}");
        assert!(
            text.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "{text} is not hex"
        );
        assert_eq!(text, format!("{id}"));
        assert_eq!(text, format!("{id:?}"), "Debug must be the rendered form");
        assert_eq!(id.as_str(), text, "as_str and Display must agree");
    }

    #[test]
    fn two_generated_ids_differ() {
        // A `generate` that returned a constant would pass every other test in this file.
        assert_ne!(RequestId::generate(), RequestId::generate());
        assert_ne!(
            TraceContext::new(true).trace_id(),
            TraceContext::new(true).trace_id()
        );
    }

    #[test]
    fn the_bytes_are_the_hex_that_is_printed() {
        // The two renderings must be one value, or a log line and a test fixture disagree.
        let id = RequestId::from_bytes([
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ]);
        assert_eq!(id.to_string(), "00112233445566778899aabbccddeeff");
        let mut written = String::new();
        id.write_str(&mut written);
        assert_eq!(written, id.to_string());
    }

    #[test]
    fn an_adopted_request_id_comes_back_exactly_as_it_arrived() {
        // **The test that gives adoption its point.** A client looking for its own id in our log
        // needs the same string back: a UUID normalised into 32 hex characters is not findable, and
        // that is what an earlier version of this type did. Asserted on the exact text, because
        // asserting the length is what let that bug through.
        for value in [
            "00112233445566778899aabbccddeeff",
            "3f2504e0-4f89-11d3-9a0c-0305e82c3301",
            "ABC-123",
            "req_42",
            "trace.span.7",
        ] {
            let parsed = RequestId::parse(value).unwrap_or_else(|| panic!("{value} refused"));
            assert_eq!(parsed.to_string(), value, "not verbatim");
            assert_eq!(parsed.as_str(), value);
            let mut written = String::new();
            parsed.write_str(&mut written);
            assert_eq!(written, value);
        }
    }

    #[test]
    fn a_generated_and_an_adopted_id_are_told_apart() {
        // The variant decides how the id renders, so a `parse` that quietly produced a `Generated`
        // would re-render a client's id and the test above would still pass.
        let adopted = RequestId::parse("ABC-123").expect("a legal id");
        assert!(matches!(adopted, RequestId::Adopted(_)), "{adopted:?}");
        assert!(
            matches!(RequestId::generate(), RequestId::Generated(_)),
            "generate must not adopt"
        );
    }

    #[test]
    fn a_header_that_is_not_an_identifier_is_refused_rather_than_escaped() {
        // The security-relevant test in this file. `\n` in an adopted id would let a client forge
        // log records; a space would break a space-separated field count; an empty value would
        // produce an empty field. All are refused, so the front-end generates its own instead.
        for value in [
            "bad\n2026-01-01T00:00:00Z ERROR forged log record",
            "bad\r\ntrailing",
            "bad value",
            "",
            "semi;colon",
            "quote\"inside",
            "-leading-hyphen",
            ".leading-dot",
            "trailing-",
            "trailing.",
            &"x".repeat(MAX_REQUEST_ID_LEN + 1),
        ] {
            assert!(
                RequestId::parse(value).is_none(),
                "{value:?} was adopted, and it is not an identifier"
            );
        }
        // The cap is a boundary, not an off-by-one refusal of the longest legal id.
        assert!(RequestId::parse(&"x".repeat(MAX_REQUEST_ID_LEN)).is_some());
    }

    #[test]
    fn a_traceparent_round_trips_through_the_header_form() {
        let context = TraceContext::new(true);
        let header = context.header();
        assert_eq!(header.len(), TRACEPARENT_LEN);
        assert_eq!(
            TraceContext::parse(&header).expect("its own output parses"),
            context
        );
        // The flag survives both ways, or a proxy would silently unsample everything behind it.
        let unsampled = TraceContext::new(false);
        assert!(!TraceContext::parse(&unsampled.header()).unwrap().sampled());
        assert!(TraceContext::parse(&context.header()).unwrap().sampled());
        assert_eq!(context.trace_id_hex().len(), 32);
        assert_eq!(context.span_id_hex().len(), 16);
        // The hex accessors and the header must be one value, or a log line and a trace query
        // disagree about which trace this was.
        assert_eq!(context.header()[3..35], context.trace_id_hex());
        assert_eq!(context.header()[36..52], context.span_id_hex());
    }

    #[test]
    fn the_specifications_own_example_parses() {
        // Straight out of the W3C recommendation, so this is checked against the specification
        // rather than against this crate's own output — which is the failure mode of a round-trip
        // test written alone.
        let parsed = TraceContext::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
            .expect("the specification's example");
        assert_eq!(parsed.trace_id_hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(parsed.span_id_hex(), "00f067aa0ba902b7");
        assert!(parsed.sampled());
    }

    #[test]
    fn a_malformed_traceparent_is_none_and_never_a_panic() {
        // `None` means "start a new trace", so this is the whole contract with the front-end: a
        // client's broken tracing header must not become a client's broken request.
        for value in [
            "",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7", // no flags
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
            "00_4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01", // wrong separator
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01", // upper case
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01", // unknown version
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01", // zero trace id
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01", // zero span id
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-zz", // not hex
            // Exactly 55 **bytes** with a multi-byte character inside the span-id field: the char
            // count is 53, so this is what a length check alone would let through. It must be a
            // `None` and not a panic — see the comment on the ASCII check.
            "00-4bf92f3577b34da6a3ce929d0e0e4736-—067aa0ba902b7-01",
        ] {
            assert!(
                TraceContext::parse(value).is_none(),
                "{value:?} was accepted"
            );
        }
    }

    #[test]
    fn only_the_low_flag_bit_is_read() {
        // `03` sets a reserved bit. The sampled flag must still be read correctly rather than the
        // whole byte being compared, which would report "not sampled" for a sampled request.
        let parsed = TraceContext::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-03")
            .expect("a reserved bit is not a malformed value");
        assert!(parsed.sampled(), "bit 0 is set, so this is sampled");
        let clear = TraceContext::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-02")
            .expect("a reserved bit is not a malformed value");
        assert!(!clear.sampled(), "bit 0 is clear");
    }

    #[test]
    fn continuing_keeps_the_trace_and_changes_the_span() {
        // The distinction the whole type exists for: a proxy continues the client's *trace* and
        // contributes its own *span*. Keeping the client's span id would make two hops claim to be
        // one, which is what corrupts a waterfall.
        let client =
            TraceContext::parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").unwrap();
        let ours = TraceContext::continuing(client.trace_id(), client.sampled());
        assert_eq!(ours.trace_id(), client.trace_id());
        assert_ne!(ours.span_id(), client.span_id());
        assert!(ours.sampled(), "the client's decision is respected");
        assert_eq!(
            ours.header()[3..35],
            client.header()[3..35],
            "the trace id is the same field of the same header"
        );
    }

    #[test]
    fn a_new_trace_never_has_an_all_zero_id() {
        // The specification forbids the zero id and every collector drops it, and a random source
        // is allowed to produce it. Asserted over many draws because one draw proves nothing.
        for _ in 0..64 {
            let context = TraceContext::new(true);
            assert_ne!(context.trace_id(), [0u8; 16]);
            assert_ne!(*context.span_id(), [0u8; 8]);
        }
    }

    #[test]
    fn the_randomness_fallback_still_produces_distinct_values() {
        // Reachable only when the system RNG fails, which a test cannot arrange portably — so the
        // real function is called directly rather than a copy of it being written here.
        let mut first = [0u8; 16];
        let mut second = [0u8; 16];
        fallback_fill(&mut first);
        fallback_fill(&mut second);
        assert_ne!(first, second, "the same nanosecond must not collide");
        assert_ne!(first, [0u8; 16]);
    }
}
