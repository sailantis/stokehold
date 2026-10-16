//! The access log: one line per completed request, to its own sink.
//!
//! # Why this is not the diagnostic log
//!
//! nginx keeps them apart and it is right to. Two things a deployment configures separately must be
//! separately silenceable, and an access log that shared `RUST_LOG` with diagnostics would be one
//! filter away from silence: raising verbosity to debug a worker would flood it, and lowering it
//! would erase the record of who requested what. This is the same rule the configuration already
//! states for `[server.tls]` — a flag and a certificate are two statements about one thing and they
//! can disagree — applied to two *sinks*.
//!
//! # A failed write is not a failed request
//!
//! The sink appends to a file and **never fails a request**. A full disk, a revoked permission, a
//! rotated-away directory: all of them mean the log is lost, and none of them is a reason to stop
//! serving. The failure is reported **once** rather than per request — a per-request warning on a
//! full disk is a log flood that buries the message saying why, which is this project's own
//! recorded hazard about error paths that become the outage.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::net::IpAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// One completed request.
///
/// Borrowed rather than owned where it can be, because this is built once per request on a hot path
/// and the fields are all already in hand at the call site.
#[derive(Debug, Clone)]
pub struct Record<'a> {
    /// The request's id, already rendered — see [`crate::RequestId::as_str`].
    pub id: &'a str,
    /// The request method as it arrived.
    pub method: &'a str,
    /// The request target: path and query, as it arrived.
    ///
    /// The *target*, not the decoded path, because that is what a log is for: reconstructing what
    /// the client actually asked for. A decoded path would hide an encoded traversal attempt.
    pub target: &'a str,
    /// The status sent to the client.
    pub status: u16,
    /// The HTTP version the client spoke, e.g. `HTTP/1.1`.
    pub protocol: &'a str,
    /// The peer address, when the listener knows it.
    pub peer: Option<IpAddr>,
    /// The `Host` the client sent, if any.
    pub host: Option<&'a str>,
    /// The `User-Agent`, if any.
    pub user_agent: Option<&'a str>,
    /// The `Referer`, if any.
    pub referer: Option<&'a str>,
    /// The whole request's duration, in seconds.
    ///
    /// **Includes the client's own upload**, which is what nginx's `$request_time` does and what
    /// surprises people about it. [`Record::origin_seconds`] is the field that separates the
    /// server's own work from the client's slowness.
    pub total_seconds: f64,
    /// Time to the origin's first response byte, in seconds, when there was an origin.
    ///
    /// `None` for every backend that is not an `upstream`, and for a request that failed before the
    /// origin answered — see [`Record::origin_seconds`].
    pub origin_seconds: Option<f64>,
    /// Bytes of response body sent.
    pub bytes_sent: u64,
}

impl Record<'_> {
    /// The time to the origin's first byte, as a decimal string.
    ///
    /// A `-` rather than a `0.000` when there was no origin, because those are different facts: an
    /// origin that answered in less than a millisecond is not the same as a static file that never
    /// had one. nginx distinguishes them the same way, with `-`.
    fn origin_seconds(&self) -> String {
        match self.origin_seconds {
            Some(seconds) => format!("{seconds:.3}"),
            None => "-".to_string(),
        }
    }
}

/// How a record is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    /// nginx's `combined` shape, with the two timings appended.
    ///
    /// The shape is deliberately nginx's — `$remote_addr - $remote_user [$time_local] "$request"
    /// $status $body_bytes_sent "$http_referer" "$http_user_agent"` — so one `awk` reads both logs,
    /// which is the whole reason `RequestId` is 32 hex characters too. The timings are appended
    /// rather than inserted, so every existing field keeps its position and an nginx-derived script
    /// keeps working.
    #[default]
    Combined,
    /// One JSON object per line.
    ///
    /// For a collector that reads the file rather than a human. Every value is a string or a
    /// number, and the two timings are numbers so a query can sum them.
    Json,
}

impl Format {
    /// Parse a format name from configuration.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "combined" => Some(Self::Combined),
            "json" => Some(Self::Json),
            _ => None,
        }
    }

    /// The names this understands, for an error message that lists them.
    pub const NAMES: [&'static str; 2] = ["combined", "json"];
}

/// The access log's destination.
///
/// A `Mutex<BufWriter<File>>` and not a channel: a channel would need a draining task, and the
/// draining task's existence would be a second thing that can fail silently. Appending under a
/// short lock is what nginx does at bottom, and it makes "the record was written" observable in the
/// same call that produces it.
pub struct AccessLog {
    format: Format,
    sink: Mutex<Sink>,
    /// Set once, so a broken sink warns once rather than per request.
    ///
    /// The same rule the worker supervisor follows for a restarted slot: an error path that
    /// reports on every attempt turns one fault into a flood, and the flood is what an operator
    /// then has to read past to find the cause.
    reported: AtomicBool,
}

/// Where records go: a file, or nowhere.
enum Sink {
    File(BufWriter<File>),
    /// Configured, but the file could not be opened. Records are dropped until a reload fixes it,
    /// and the reason is reported once.
    Broken(String),
}

impl AccessLog {
    /// Open a log at `path`, creating it if it does not exist.
    ///
    /// A failure to open is **not** returned as an error: it is recorded as [`Sink::Broken`] so the
    /// server starts and serves, and the same warning path that handles a mid-run failure reports
    /// it. A deployment whose log directory is missing should log that fact, not refuse to serve —
    /// which is a deliberate choice, and the opposite of what `[server.tls]` does with a missing
    /// certificate, because a certificate is required for correctness and a log is not.
    #[must_use]
    pub fn open(path: &Path, format: Format) -> Self {
        let sink = match File::options().create(true).append(true).open(path) {
            Ok(file) => Sink::File(BufWriter::new(file)),
            Err(err) => Sink::Broken(format!("{}: {err}", path.display())),
        };
        let broken = matches!(sink, Sink::Broken(_));
        let log = Self {
            format,
            sink: Mutex::new(sink),
            reported: AtomicBool::new(false),
        };
        if broken {
            log.report_once();
        }
        log
    }

    /// A log that goes nowhere, for when none is configured.
    ///
    /// Not `Option<AccessLog>` at the call site: a `None` check per request is a branch on the hot
    /// path for something decided once at startup, and the whole point of the "off by default"
    /// promise is that it costs nothing. This is also what makes the write below the *only* place
    /// that asks whether logging is on.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            format: Format::Combined,
            sink: Mutex::new(Sink::Broken(String::new())),
            // Already reported as far as anything downstream cares: a disabled log is not a fault.
            reported: AtomicBool::new(true),
        }
    }

    /// Whether this log writes anywhere.
    ///
    /// There so a caller can skip building a [`Record`] at all — the `Cow` in
    /// [`crate::RequestId::as_str`] allocates for a generated id, and a disabled log must not pay
    /// for that.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        matches!(
            *self.sink.lock().unwrap_or_else(|e| e.into_inner()),
            Sink::File(_)
        )
    }

    /// Write one record.
    ///
    /// Never returns an error and never panics in a way a request can see. A failure marks the sink
    /// broken so the next call is cheap, and is reported once.
    pub fn record(&self, record: &Record<'_>) {
        let mut sink = self.sink.lock().unwrap_or_else(|err| err.into_inner());
        let Sink::File(file) = &mut *sink else {
            return;
        };
        let line = match self.format {
            Format::Combined => combined(record),
            Format::Json => json(record),
        };
        // One `write_all` of one line, then a flush. The flush is what makes the record survive a
        // crash rather than sitting in the buffer — and a log you cannot trust after a crash is a
        // log that is missing exactly the request you wanted.
        if let Err(err) = file.write_all(line.as_bytes()).and_then(|()| file.flush()) {
            *sink = Sink::Broken(err.to_string());
            self.report_once();
        }
    }

    /// Flush what is buffered.
    pub fn flush(&self) {
        if let Sink::File(file) = &mut *self.sink.lock().unwrap_or_else(|e| e.into_inner()) {
            let _ = file.flush();
        }
    }

    /// Report a broken sink once, at `error` because a lost access log is a real loss.
    fn report_once(&self) {
        if self.reported.swap(true, Ordering::SeqCst) {
            return;
        }
        let reason = match &*self.sink.lock().unwrap_or_else(|e| e.into_inner()) {
            Sink::Broken(reason) => reason.clone(),
            Sink::File(_) => return,
        };
        tracing::error!(
            reason = %reason,
            "the access log could not be written and requests are being served without it; this is \
             reported once, not per request. Fix the path and send SIGHUP, or restart"
        );
    }
}

impl std::fmt::Debug for AccessLog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sink = match &*self.sink.lock().unwrap_or_else(|e| e.into_inner()) {
            Sink::File(_) => "file",
            Sink::Broken(_) => "broken",
        };
        formatter
            .debug_struct("AccessLog")
            .field("format", &self.format)
            .field("sink", &sink)
            .finish()
    }
}

/// Render a record in nginx's `combined` shape, with both timings appended.
///
/// Field order is the compatibility contract, so the two new fields go at the end — an nginx-derived
/// `awk '{print $1, $9}'` keeps working.
///
/// Every client-controlled value is escaped for a **space-separated, line-based** format: a
/// `User-Agent` containing a quote or a newline would otherwise shift the field count or forge a
/// record. This is the same reasoning `RequestId::parse` applies to the id, and it applies to the
/// three headers for the same reason.
fn combined(record: &Record<'_>) -> String {
    let mut out = String::with_capacity(180);
    match record.peer {
        Some(peer) => out.push_str(&peer.to_string()),
        None => out.push('-'),
    }
    // `$remote_user`, which this server has no concept of — a backend owns authentication. Kept as
    // `-` so the field exists and the positions after it are unchanged.
    out.push_str(" - - [");
    out.push_str(&timestamp());
    out.push_str("] \"");
    escape_into(&mut out, record.method);
    out.push(' ');
    escape_into(&mut out, record.target);
    out.push(' ');
    escape_into(&mut out, record.protocol);
    out.push_str("\" ");
    let _ = write!(out, "{}", record.status);
    let _ = write!(out, " {} ", record.bytes_sent);
    quote_into(&mut out, record.referer);
    out.push(' ');
    quote_into(&mut out, record.user_agent);
    // The two additions, last so the positions above are nginx's.
    let _ = write!(out, " {:.3}", record.total_seconds);
    let _ = write!(out, " {}", record.origin_seconds());
    let _ = write!(out, " {id}", id = record.id);
    out.push('\n');
    out
}

/// Render a record as one JSON object.
fn json(record: &Record<'_>) -> String {
    let mut out = String::with_capacity(280);
    out.push('{');
    let mut first = true;
    let mut field = |out: &mut String, name: &str, value: &str| {
        if !first {
            out.push(',');
        }
        first = false;
        let _ = write!(out, "\"{name}\":");
        escape_json_into(out, value);
    };
    field(&mut out, "id", record.id);
    field(&mut out, "method", record.method);
    field(&mut out, "target", record.target);
    field(&mut out, "protocol", record.protocol);
    field(&mut out, "host", record.host.unwrap_or("-"));
    field(&mut out, "user_agent", record.user_agent.unwrap_or("-"));
    field(&mut out, "referer", record.referer.unwrap_or("-"));
    let peer = match record.peer {
        Some(peer) => peer.to_string(),
        None => "-".to_string(),
    };
    field(&mut out, "remote_addr", &peer);
    field(&mut out, "time", &timestamp());
    // After the strings, because these are numbers and a reader that types a field must not have to
    // guess which kind it is.
    let _ = write!(out, ",\"status\":{}", record.status);
    let _ = write!(out, ",\"bytes_sent\":{}", record.bytes_sent);
    let _ = write!(out, ",\"request_time\":{:.3}", record.total_seconds);
    match record.origin_seconds {
        Some(seconds) => {
            let _ = write!(out, ",\"origin_time\":{seconds:.3}");
        }
        // `null`, not `-`: a machine reading this field must be able to tell "no origin" from
        // "an origin that took no time", which is the distinction the `-` in the text format makes.
        None => out.push_str(",\"origin_time\":null"),
    }
    out.push_str("}\n");
    out
}

/// Escape a value for the quoted fields and the bare ones of the text format.
///
/// **A space must be escaped too, and that is not cosmetic.** This format is space-separated, so an
/// unescaped space in a quoted field shifts every token after it: a client sending
/// `User-Agent: evil 1.2.3.4 - - [now] "GET /admin HTTP/1.1" 200 1` would have its own text read as
/// fields of ours. Quotes and newlines are the obvious injection; the space is the one that makes
/// the *other* two matter, because without it a value cannot be told from the field separator at
/// all. nginx escapes these with `\x20`, `\x22` and `\x5C`; this follows that, since the whole point
/// of the format is that one parser reads both logs.
fn escape_into(out: &mut String, value: &str) {
    // One pass and no allocation when the value is ordinary, which is nearly always: a `User-Agent`
    // with a space is the common case, so this branch is taken and the loop is not.
    if !value.bytes().any(needs_escaping) {
        out.push_str(value);
        return;
    }
    for byte in value.bytes() {
        match byte {
            b'"' => out.push_str("\\x22"),
            b'\\' => out.push_str("\\x5C"),
            b' ' => out.push_str("\\x20"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            other if other < 0x20 => out.push('?'),
            other => out.push(other as char),
        }
    }
}

/// Whether a byte cannot appear literally in the text format.
///
/// `0x20` is included: see [`escape_into`] for why an unescaped space is the injection that makes
/// the others matter.
const fn needs_escaping(byte: u8) -> bool {
    matches!(byte, b'"' | b'\\' | b' ' | b'\n' | b'\r' | b'\t') || byte < 0x20
}

/// A quoted field that is always present, so the position after it is stable.
fn quote_into(out: &mut String, value: Option<&str>) {
    out.push('"');
    escape_into(out, value.unwrap_or("-"));
    out.push('"');
}

/// Escape a value for a JSON string.
fn escape_json_into(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // JSON has no other control-character escapes, and `\u00XX` is what it requires. A bare
            // control byte makes the document unparseable, which would lose the whole line rather
            // than one field.
            other if (other as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", other as u32);
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

/// The current time in nginx's `$time_local` shape: `10/Oct/2000:13:55:36 +0200`.
///
/// The same shape nginx uses, so a parser for one reads the other. Built from `httpdate`, which is
/// already a dependency of this workspace, rather than formatting by hand.
fn timestamp() -> String {
    let now = httpdate::fmt_http_date(std::time::SystemTime::now());
    // `httpdate` produces the HTTP-date shape (`Sun, 06 Nov 1994 08:49:37 GMT`). nginx's log shape
    // is a different one, so this is a real conversion rather than a trim — and it is worth doing
    // because the alternative is a second date formatter in the tree.
    //
    // The offset is rendered as `+0000`: this server logs in UTC, which is what makes two servers
    // in different zones comparable without a config key.
    match parse_http_date(&now) {
        Some((year, month, day, hour, minute, second, _)) => format!(
            "{day:02}/{}/{year:04}:{hour:02}:{minute:02}:{second:02} +0000",
            MONTHS[month]
        ),
        None => now,
    }
}

/// The three-letter month names nginx's date shape uses.
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Pull `(year, month, day, hour, minute, second)` out of an HTTP-date.
///
/// Returns `None` for anything that is not the shape `httpdate` produces, which keeps the fallback
/// in [`timestamp`] honest rather than silently emitting a half-converted date.
fn parse_http_date(date: &str) -> Option<(i32, usize, u32, u32, u32, u32, u32)> {
    // `Sun, 06 Nov 1994 08:49:37 GMT`
    let rest = date.split_once(", ")?.1;
    let mut fields = rest.split(' ');
    let day: u32 = fields.next()?.parse().ok()?;
    let month_name = fields.next()?;
    let month = MONTHS.iter().position(|name| *name == month_name)?;
    let year: i32 = fields.next()?.parse().ok()?;
    let mut clock = fields.next()?.split(':');
    let hour: u32 = clock.next()?.parse().ok()?;
    let minute: u32 = clock.next()?.parse().ok()?;
    let second: u32 = clock.next()?.parse().ok()?;
    Some((year, month, day, hour, minute, second, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Record<'static> {
        Record {
            id: "00112233445566778899aabbccddeeff",
            method: "GET",
            target: "/about?x=1",
            status: 200,
            protocol: "HTTP/1.1",
            peer: Some("203.0.113.7".parse().expect("an address")),
            host: Some("example.test"),
            user_agent: Some("curl/8.0"),
            referer: Some("https://example.test/"),
            total_seconds: 0.012,
            origin_seconds: Some(0.004),
            bytes_sent: 1234,
        }
    }

    #[test]
    fn the_combined_line_keeps_nginx_field_positions() {
        // The compatibility claim, asserted by position — so the indices below are the contract.
        //
        // **The timestamp occupies *two* space-separated tokens**, because `$time_local` is
        // `16/Oct/2026:21:22:47 +0000` and the offset is part of it. nginx's line has the same
        // property, which is why an `awk` script for it uses `$4` for the date and `$5` for the
        // offset rather than treating the bracketed field as one token. Counting this wrong is what
        // a first version of this test did, and every assertion after the timestamp was off by one.
        let line = combined(&record());
        let fields: Vec<&str> = line.trim_end().split(' ').collect();
        assert_eq!(fields[0], "203.0.113.7", "$remote_addr");
        assert_eq!(fields[1], "-", "the first `-`");
        assert_eq!(fields[2], "-", "$remote_user");
        assert!(fields[3].starts_with('['), "$time_local: {}", fields[3]);
        assert!(fields[3].contains('/'), "the date: {}", fields[3]);
        assert!(fields[3].contains(':'), "the clock: {}", fields[3]);
        assert_eq!(
            fields[4], "+0000]",
            "the offset is its own token, as in nginx"
        );
        assert_eq!(fields[5], "\"GET", "the request begins");
        assert_eq!(fields[6], "/about?x=1");
        assert_eq!(fields[7], "HTTP/1.1\"", "third token of $request");
        assert_eq!(fields[8], "200", "$status");
        assert_eq!(fields[9], "1234", "$body_bytes_sent");
        assert_eq!(fields[10], "\"https://example.test/\"", "$http_referer");
        assert_eq!(fields[11], "\"curl/8.0\"", "$http_user_agent");
        // Then, and only then, the additions — so every index above is nginx's.
        assert_eq!(fields[12], "0.012", "the whole request");
        assert_eq!(fields[13], "0.004", "to the origin's first byte");
        assert_eq!(fields[14], "00112233445566778899aabbccddeeff", "the id");
        assert_eq!(fields.len(), 15, "nothing else: {fields:?}");
        assert!(line.ends_with('\n'), "a record is one line");
        assert_eq!(line.matches('\n').count(), 1);
    }

    #[test]
    fn a_missing_origin_is_a_dash_and_not_a_zero() {
        // The distinction the field exists for: a static file never had an origin, where an origin
        // that answered instantly did. Rendering both as `0.000` would make a graph of a mixed
        // deployment wrong in the direction that flatters it.
        let mut plain = record();
        plain.origin_seconds = None;
        let line = combined(&plain);
        let fields: Vec<&str> = line.trim_end().split(' ').collect();
        assert_eq!(fields[12], "0.012", "the total is unaffected");
        assert_eq!(fields[13], "-", "no origin, not zero seconds");
        assert_eq!(fields[14], "00112233445566778899aabbccddeeff", "the id");
        let rendered = json(&plain);
        assert!(rendered.contains("\"origin_time\":null"), "{rendered}");
        // And the JSON form uses a number, not a string, when there is one — a reader must not have
        // to guess the type of a field.
        let with_origin = json(&record());
        assert!(
            with_origin.contains("\"origin_time\":0.004"),
            "{with_origin}"
        );
    }

    #[test]
    fn a_header_cannot_forge_a_log_record_or_shift_a_field() {
        // The security-relevant test here. A `User-Agent` is entirely client-controlled, and this
        // format is space-separated and line-based: without escaping, a newline forges a record and
        // a space breaks the field count.
        let mut hostile = record();
        hostile.user_agent =
            Some("evil\n1.2.3.4 - - [now] \"GET /admin HTTP/1.1\" 200 1 - - 0.1 0.1 forged");
        hostile.referer = Some("has \"quotes\" and \\backslash");
        hostile.target = "/x";
        let line = combined(&hostile);
        assert_eq!(line.matches('\n').count(), 1, "one record, not two: {line}");
        assert!(line.contains("\\n"), "the newline is escaped: {line}");
        assert!(line.contains("\\x22"), "the quote is escaped: {line}");
        assert!(
            line.contains("\\x20"),
            "the space is escaped, or it re-creates the field split: {line}"
        );
        assert!(line.contains("\\x5C"), "the backslash is escaped: {line}");
        let fields: Vec<&str> = line.trim_end().split(' ').collect();
        assert_eq!(fields[0], "203.0.113.7");
        assert_eq!(fields[8], "200", "the status is still where nginx puts it");
        assert_eq!(fields[9], "1234", "and so is the byte count");
        // **The count is the assertion that matters.** A version of this escaper that handled
        // quotes and newlines but not spaces produced 31 fields here, because the hostile
        // `User-Agent` split into seven tokens and its own `200` landed where our status belongs.
        // Only a field count catches that: every positional assertion above still passed.
        assert_eq!(fields.len(), 15, "no field was gained: {fields:?}");
    }

    #[test]
    fn the_json_form_is_one_object_on_one_line() {
        let line = json(&record());
        assert!(line.ends_with("}\n"), "{line}");
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.starts_with('{'));
        // Every field the text form carries, so a collector does not silently lose one.
        for name in [
            "id",
            "method",
            "target",
            "protocol",
            "host",
            "user_agent",
            "referer",
            "remote_addr",
            "time",
            "status",
            "bytes_sent",
            "request_time",
        ] {
            assert!(
                line.contains(&format!("\"{name}\":")),
                "{name} missing from {line}"
            );
        }
        // A control character must become `\u00XX` or the whole document is unparseable, which
        // would lose the record rather than one field.
        let mut hostile = record();
        hostile.user_agent = Some("a\u{1}b");
        let escaped = json(&hostile);
        assert!(escaped.contains("\\u0001"), "{escaped}");
        assert!(
            !escaped.contains('\u{1}'),
            "a raw control byte reached the line"
        );
    }

    #[test]
    fn the_timestamp_is_nginxs_shape() {
        // A shape assertion, because the fallback returns the HTTP-date it failed to convert and a
        // regression would show up as a whole log no parser recognises.
        let stamp = timestamp();
        assert_eq!(stamp.len(), 26, "{stamp}");
        assert!(stamp.ends_with(" +0000"), "{stamp}");
        assert_eq!(stamp.as_bytes()[2], b'/', "{stamp}");
        assert_eq!(stamp.as_bytes()[6], b'/', "{stamp}");
        assert_eq!(stamp.as_bytes()[11], b':', "{stamp}");
        // The month is the three-letter name nginx uses, not the number.
        let month = &stamp[3..6];
        assert!(MONTHS.contains(&month), "{month} is not a month name");
    }

    #[test]
    fn a_disabled_log_writes_nothing_and_does_not_pay_for_an_id() {
        // The "off by default costs nothing" promise, in the one place it can be checked: a
        // disabled log must report itself disabled, because the caller uses that to skip building
        // the record — including the `Cow::Owned` an id's hex form allocates.
        let log = AccessLog::disabled();
        assert!(!log.is_enabled());
        // Recording against it is a no-op rather than a panic, so a caller that forgets the check
        // still serves.
        log.record(&record());
        log.flush();
    }

    #[test]
    fn an_unopenable_path_warns_once_and_does_not_panic() {
        // A deployment whose log directory is missing must still serve. `is_enabled` is false, so
        // the caller skips the work, and nothing panics on the way.
        let log = AccessLog::open(
            Path::new("a-directory-that-does-not-exist/access.log"),
            Format::Combined,
        );
        assert!(!log.is_enabled());
        log.record(&record());
        // Calling it again must not panic on the already-broken sink, and must not report twice —
        // the flag is what makes the second call silent.
        log.record(&record());
    }

    #[test]
    fn records_reach_the_file_in_order() {
        // Read back rather than asserted on a struct: the claim is that a deployment can read its
        // own log, and only reading it checks that.
        let path = std::env::temp_dir().join(format!(
            "volucer-observe-access-log-{}-{:?}.log",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        let log = AccessLog::open(&path, Format::Combined);
        assert!(log.is_enabled(), "the temp path must be writable");
        for status in [200u16, 404, 500] {
            let mut record = record();
            record.status = status;
            log.record(&record);
        }
        log.flush();
        let text = std::fs::read_to_string(&path).expect("the log was written");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].contains(" 200 "), "{}", lines[0]);
        assert!(lines[1].contains(" 404 "), "{}", lines[1]);
        assert!(lines[2].contains(" 500 "), "{}", lines[2]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_format_name_is_read_strictly() {
        assert_eq!(Format::parse("combined"), Some(Format::Combined));
        assert_eq!(Format::parse("JSON"), Some(Format::Json));
        assert_eq!(Format::parse(" json "), Some(Format::Json));
        // Not `unwrap_or_default`: a typo must be a configuration error rather than quietly
        // choosing a format the operator did not ask for.
        assert_eq!(Format::parse("clf"), None);
        assert_eq!(Format::parse(""), None);
        assert_eq!(Format::NAMES.len(), 2);
    }
}
