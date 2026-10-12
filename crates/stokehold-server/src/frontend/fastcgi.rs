//! A minimal FastCGI *server*, for talking to nginx.
//!
//! The codec in `stokehold-fcgi` is symmetric, so this reuses the same records
//! and helpers the client uses. What is new here is the request lifecycle from
//! the server's side: reading `PARAMS` and `STDIN` until their terminators,
//! ignoring `ABORT_REQUEST`, and writing `STDOUT`, `STDERR` and `END_REQUEST`.
//!
//! Two properties are deliberate:
//!
//! - **Nothing is parsed that can be relayed.** A request becomes a
//!   `stokehold_fcgi::Request` (already-decoded name-value pairs plus a body)
//!   and a response goes back as `STDOUT` bytes, so the front controller's
//!   headers are never re-serialised.
//! - **`STDERR` is forwarded rather than logged away.** nginx shows it in the
//!   error log, which is where a developer looks first when PHP complains.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use stokehold_fcgi::{
    message::{BeginRequest, EndRequest},
    nvpair,
    protocol::{ProtocolStatus, RecordType},
    record, Request, Response,
};

use crate::error::{Result, ServerError};

/// What the front-end learned about a request.
#[derive(Debug, Clone)]
pub struct IncomingRequest {
    /// The CGI environment, verbatim from `PARAMS`.
    pub params: Vec<(Vec<u8>, Vec<u8>)>,
    /// The request body, verbatim from `STDIN`.
    pub body: Vec<u8>,
    /// Whether the client asked for keep-alive.
    pub keep_conn: bool,
    /// Whether the client aborted the request in flight.
    pub aborted: bool,
}

impl IncomingRequest {
    /// Look up a `PARAMS` value by name.
    ///
    /// Names are compared exactly, because that is how they arrive on the wire.
    pub fn param(&self, name: &str) -> Option<&[u8]> {
        self.params
            .iter()
            .find(|(key, _)| key.as_slice() == name.as_bytes())
            .map(|(_, value)| value.as_slice())
    }

    /// A `PARAMS` value as a lossy UTF-8 string.
    pub fn param_str(&self, name: &str) -> Option<String> {
        self.param(name)
            .map(|value| String::from_utf8_lossy(value).into_owned())
    }

    /// Convert into what a backend client expects.
    pub fn into_backend_request(self) -> Request {
        Request {
            params: self.params,
            body: self.body,
            keep_conn: self.keep_conn,
        }
    }
}

/// How a connection ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionOutcome {
    /// The peer closed cleanly between requests.
    Closed,
    /// The peer asked to keep the connection open and may send another request.
    KeepAlive,
    /// The peer aborted the request in flight.
    Aborted,
}

/// What arrived on a connection.
#[derive(Debug, Clone)]
pub enum NextRequest {
    /// A real request to serve.
    Request(IncomingRequest),
    /// A `GET_VALUES` probe. nginx uses it to decide whether this server is
    /// usable, so answering it keeps stokehold in rotation instead of marked
    /// down.
    GetValues(Vec<(Vec<u8>, Vec<u8>)>),
}

/// Read one request, or a management probe, from a connection.
///
/// Returns `Ok(None)` for a clean end of stream, which is how nginx closes an
/// idle keep-alive connection.
pub fn read_next<R: Read>(reader: &mut R) -> Result<Option<NextRequest>> {
    let begin = loop {
        let Some(record) = record::read_record(reader)? else {
            return Ok(None);
        };
        match record.record_type() {
            RecordType::BeginRequest => break record,
            RecordType::GetValues => {
                let names = nvpair::decode_pairs(&record.content).map_err(|source| {
                    ServerError::BadProtocol {
                        reason: format!("malformed GET_VALUES: {source}"),
                    }
                })?;
                return Ok(Some(NextRequest::GetValues(names)));
            }
            // Anything else before a BEGIN_REQUEST is unexpected, but skipping is
            // kinder than tearing the connection down.
            other => {
                tracing::debug!(record = ?other, "ignoring a record before BEGIN_REQUEST");
                continue;
            }
        }
    };

    let begin_request =
        BeginRequest::decode(&begin.content).map_err(|source| ServerError::BadProtocol {
            reason: format!("malformed BEGIN_REQUEST: {source}"),
        })?;

    let mut params = Vec::new();
    let mut body = Vec::new();

    loop {
        let Some(record) = record::read_record(reader)? else {
            return Err(ServerError::BadProtocol {
                reason: "the connection closed mid-request".to_string(),
            });
        };

        match record.record_type() {
            RecordType::Params => {
                if record.content.is_empty() {
                    continue;
                }
                params.extend(nvpair::decode_pairs(&record.content).map_err(|source| {
                    ServerError::BadProtocol {
                        reason: format!("malformed PARAMS: {source}"),
                    }
                })?);
            }
            RecordType::Stdin => {
                if record.content.is_empty() {
                    // The zero-length record terminates the body, which with the
                    // params terminator already seen ends the request.
                    break;
                }
                body.extend_from_slice(&record.content);
            }
            RecordType::AbortRequest => {
                // There is no partial response to abandon, so an abort is simply
                // a request that will not be served.
                return Ok(Some(NextRequest::Request(IncomingRequest {
                    params,
                    body,
                    keep_conn: false,
                    aborted: true,
                })));
            }
            other => {
                tracing::debug!(record = ?other, "ignoring an unexpected record");
                continue;
            }
        }
    }

    let _ = begin_request.role;
    Ok(Some(NextRequest::Request(IncomingRequest {
        params,
        body,
        keep_conn: begin_request.keep_conn,
        aborted: false,
    })))
}

/// Read one request, treating a management probe as an error.
///
/// Kept for callers that only serve requests; the listener uses [`read_next`] so
/// it can answer probes.
pub fn read_request<R: Read>(reader: &mut R) -> Result<Option<IncomingRequest>> {
    match read_next(reader)? {
        None => Ok(None),
        Some(NextRequest::Request(request)) => Ok(Some(request)),
        Some(NextRequest::GetValues(_)) => Err(ServerError::UnsupportedManagementRecord),
    }
}

/// Write a backend response back to the FastCGI client.
///
/// `stdout` is relayed verbatim: it is already `Status:`-header + headers +
/// body, which is exactly what the client expects.
pub fn write_response<W: Write>(
    writer: &mut W,
    request_id: u16,
    stdout: &[u8],
    stderr: &[u8],
    keep_conn: bool,
) -> Result<()> {
    record::write_records(writer, RecordType::Stdout, request_id, stdout, true)?;
    if !stderr.is_empty() {
        record::write_records(writer, RecordType::Stderr, request_id, stderr, true)?;
    }

    // The application status is 0 because the backend already reported any
    // application-level outcome inside its response; the client sees that as the
    // `Status:` line.
    let end = EndRequest::complete();
    let header = record::Header::new(RecordType::EndRequest, request_id, 8);
    header.write(writer)?;
    writer.write_all(&end.encode())?;
    writer.flush()?;

    let _ = keep_conn;
    Ok(())
}

/// Write an error that happened before the backend produced a response.
///
/// nginx surfaces this as a 502, which is the correct outcome: the gateway is
/// what failed.
pub fn write_error<W: Write>(
    writer: &mut W,
    request_id: u16,
    message: &str,
    reason: ServerErrorKind,
) -> Result<()> {
    let body = format!(
        "Status: 502 Bad Gateway\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nstokehold: {message}\r\n"
    );
    record::write_records(
        writer,
        RecordType::Stdout,
        request_id,
        body.as_bytes(),
        true,
    )?;
    record::write_records(
        writer,
        RecordType::Stderr,
        request_id,
        message.as_bytes(),
        true,
    )?;

    let end = EndRequest {
        app_status: 0,
        protocol_status: reason.protocol_status(),
    };
    let header = record::Header::new(RecordType::EndRequest, request_id, 8);
    header.write(writer)?;
    writer.write_all(&end.encode())?;
    writer.flush()?;
    Ok(())
}

/// Why a request could not be served, for the `END_REQUEST` status.
#[derive(Debug, Clone, Copy)]
pub enum ServerErrorKind {
    /// The backend failed; a 502 is the right answer.
    Backend,
    /// Too many concurrent requests for this server.
    Overloaded,
    /// Multiplexed connections are not supported.
    NoMultiplexing,
}

impl ServerErrorKind {
    fn protocol_status(self) -> ProtocolStatus {
        match self {
            ServerErrorKind::Backend => ProtocolStatus::RequestComplete,
            ServerErrorKind::Overloaded => ProtocolStatus::Overloaded,
            ServerErrorKind::NoMultiplexing => ProtocolStatus::CantMpxConn,
        }
    }
}

/// Answer a `GET_VALUES` probe.
///
/// nginx uses this to decide whether a server is usable, so answering it keeps
/// stokehold in rotation instead of being marked down.
pub fn write_get_values<W: Write>(writer: &mut W, values: &[(&str, &str)]) -> Result<()> {
    let payload = nvpair::encode_pairs(
        values
            .iter()
            .map(|(name, value)| (name.as_bytes(), value.as_bytes())),
    );
    record::write_records(
        writer,
        RecordType::GetValuesResult,
        stokehold_fcgi::MANAGEMENT_REQUEST_ID,
        &payload,
        false,
    )?;
    writer.flush()?;
    Ok(())
}

/// The names a `GET_VALUES` probe asked about.
pub type ValueNames = Vec<(Vec<u8>, Vec<u8>)>;

/// Read a management record if one is pending, without consuming a request.
///
/// Kept separate from [`read_request`] because a probe is not a request and must
/// not be answered with an `END_REQUEST`.
pub fn read_management_request<R: Read>(reader: &mut R) -> Result<Option<ValueNames>> {
    let Some(record) = record::read_record(reader)? else {
        return Ok(None);
    };
    if record.record_type() != RecordType::GetValues {
        return Err(ServerError::BadProtocol {
            reason: format!(
                "expected a GET_VALUES record, received {:?}",
                record.record_type()
            ),
        });
    }
    Ok(Some(nvpair::decode_pairs(&record.content).map_err(
        |source| ServerError::BadProtocol {
            reason: format!("malformed GET_VALUES: {source}"),
        },
    )?))
}

/// How long a request took, for logging.
#[derive(Debug)]
pub struct Timing {
    pub started: Instant,
}

impl Timing {
    pub fn start() -> Self {
        Timing {
            started: Instant::now(),
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

/// Turn a backend response into the parts the front-end needs.
pub fn split_response(response: &Response) -> (&[u8], &[u8], u16) {
    (
        &response.stdout,
        &response.stderr,
        stokehold_fcgi::REQUEST_ID,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn framed(record_type: RecordType, request_id: u16, content: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let header = record::Header::new(record_type, request_id, content.len() as u16);
        header.write(&mut out).unwrap();
        out.extend_from_slice(content);
        out.extend(std::iter::repeat_n(0u8, header.padding_length as usize));
        out
    }

    fn begin(keep_conn: bool) -> Vec<u8> {
        let request = BeginRequest::responder(keep_conn);
        let mut out = Vec::new();
        let header = record::Header::new(RecordType::BeginRequest, 1, 8);
        header.write(&mut out).unwrap();
        out.extend_from_slice(&request.encode());
        out
    }

    fn params(pairs: &[(&str, &str)]) -> Vec<u8> {
        let payload = nvpair::encode_pairs(
            pairs
                .iter()
                .map(|(name, value)| (name.as_bytes(), value.as_bytes())),
        );
        let mut out = framed(RecordType::Params, 1, &payload);
        // The zero-length PARAMS record terminates the stream.
        out.extend(framed(RecordType::Params, 1, b""));
        out
    }

    fn stdin(body: &[u8]) -> Vec<u8> {
        let mut out = framed(RecordType::Stdin, 1, body);
        out.extend(framed(RecordType::Stdin, 1, b""));
        out
    }

    /// Read the next record, skipping the zero-length records that terminate a
    /// stream type. Real servers emit them, so the tests must tolerate them
    /// exactly as the client does.
    fn next_non_empty<R: Read>(reader: &mut R) -> Option<record::Record> {
        loop {
            let record = record::read_record(reader).ok()??;
            if record.content.is_empty() {
                continue;
            }
            return Some(record);
        }
    }

    /// Read the next record of any kind, terminators included.
    fn next_record<R: Read>(reader: &mut R) -> Option<record::Record> {
        record::read_record(reader).ok()?
    }

    #[test]
    fn reads_a_complete_request() {
        let mut wire = begin(true);
        wire.extend(params(&[
            ("REQUEST_METHOD", "POST"),
            ("SCRIPT_FILENAME", "/srv/index.php"),
        ]));
        wire.extend(stdin(b"a=1"));

        let request = read_request(&mut wire.as_slice()).unwrap().unwrap();
        assert_eq!(request.param_str("REQUEST_METHOD").as_deref(), Some("POST"));
        assert_eq!(
            request.param_str("SCRIPT_FILENAME").as_deref(),
            Some("/srv/index.php")
        );
        assert_eq!(request.body, b"a=1");
        assert!(request.keep_conn);
    }

    #[test]
    fn reads_a_request_split_across_several_params_records() {
        let mut wire = begin(false);
        // Two PARAMS records carrying one pair each, then the terminator.
        //
        // nginx does *not* do this: `ngx_http_fastcgi_create_request` sums every pair and emits
        // the whole environment as one record (verified in nginx 1.30.5). This shape is what the
        // front-end tolerates, not what it receives, and the split is at a *pair* boundary
        // because a pair straddling two records is not reassembled — see hazard 38.
        let first = nvpair::encode_pairs([(&b"REQUEST_METHOD"[..], &b"GET"[..])]);
        let second = nvpair::encode_pairs([(&b"QUERY_STRING"[..], &b"x=1"[..])]);
        wire.extend(framed(RecordType::Params, 1, &first));
        wire.extend(framed(RecordType::Params, 1, &second));
        wire.extend(framed(RecordType::Params, 1, b""));
        wire.extend(stdin(b""));

        let request = read_request(&mut wire.as_slice()).unwrap().unwrap();
        assert_eq!(request.params.len(), 2);
        assert_eq!(request.param_str("QUERY_STRING").as_deref(), Some("x=1"));
        assert!(!request.keep_conn);
    }

    #[test]
    fn reads_a_body_split_across_several_stdin_records() {
        let mut wire = begin(true);
        wire.extend(params(&[("REQUEST_METHOD", "POST")]));
        wire.extend(framed(RecordType::Stdin, 1, b"part-one"));
        wire.extend(framed(RecordType::Stdin, 1, b"part-two"));
        wire.extend(framed(RecordType::Stdin, 1, b""));

        let request = read_request(&mut wire.as_slice()).unwrap().unwrap();
        assert_eq!(request.body, b"part-onepart-two");
    }

    #[test]
    fn a_clean_end_of_stream_is_not_an_error() {
        assert!(read_request(&mut [].as_slice()).unwrap().is_none());
    }

    #[test]
    fn a_connection_closing_mid_request_is_an_error() {
        let mut wire = begin(true);
        wire.extend(params(&[("REQUEST_METHOD", "GET")]));
        // No STDIN terminator: the connection died.
        assert!(read_request(&mut wire.as_slice()).is_err());
    }

    #[test]
    fn a_malformed_begin_request_is_reported() {
        let mut wire = Vec::new();
        let header = record::Header::new(RecordType::BeginRequest, 1, 2);
        header.write(&mut wire).unwrap();
        wire.extend_from_slice(&[0, 9]);
        wire.extend(std::iter::repeat_n(0u8, header.padding_length as usize));
        assert!(read_request(&mut wire.as_slice()).is_err());
    }

    #[test]
    fn writes_a_stdout_stderr_and_end_sequence() {
        let mut wire = Vec::new();
        write_response(
            &mut wire,
            7,
            b"Status: 200 OK\r\n\r\nhi",
            b"a warning",
            true,
        )
        .unwrap();

        let mut cursor = wire.as_slice();
        let stdout = next_non_empty(&mut cursor).unwrap();
        assert_eq!(stdout.record_type(), RecordType::Stdout);
        assert_eq!(stdout.request_id(), 7);
        assert_eq!(stdout.content, b"Status: 200 OK\r\n\r\nhi");

        let stderr = next_non_empty(&mut cursor).unwrap();
        assert_eq!(stderr.record_type(), RecordType::Stderr);
        assert_eq!(stderr.content, b"a warning");

        let end = next_non_empty(&mut cursor).unwrap();
        assert_eq!(end.record_type(), RecordType::EndRequest);
        assert!(EndRequest::decode(&end.content)
            .unwrap()
            .protocol_status
            .is_complete());
    }

    #[test]
    fn omits_the_stderr_record_when_there_is_nothing_to_report() {
        let mut wire = Vec::new();
        write_response(&mut wire, 1, b"body", b"", true).unwrap();

        let mut cursor = wire.as_slice();
        assert_eq!(
            next_non_empty(&mut cursor).unwrap().record_type(),
            RecordType::Stdout
        );
        // Straight to END_REQUEST: no STDERR record is written at all, and the
        // END_REQUEST itself is a single non-stream record.
        assert_eq!(
            next_non_empty(&mut cursor).unwrap().record_type(),
            RecordType::EndRequest
        );
        assert!(next_record(&mut cursor).is_none(), "nothing follows");
    }

    #[test]
    fn an_error_response_is_a_502_with_a_reason() {
        let mut wire = Vec::new();
        write_error(
            &mut wire,
            3,
            "the backend is down",
            ServerErrorKind::Backend,
        )
        .unwrap();

        let mut cursor = wire.as_slice();
        let stdout = next_non_empty(&mut cursor).unwrap();
        let text = String::from_utf8_lossy(&stdout.content);
        assert!(text.contains("502 Bad Gateway"), "{text}");
        assert!(text.contains("the backend is down"), "{text}");

        let stderr = next_non_empty(&mut cursor).unwrap();
        assert_eq!(stderr.content, b"the backend is down");
        assert_eq!(stderr.request_id(), 3);

        let end = next_non_empty(&mut cursor).unwrap();
        let end = EndRequest::decode(&end.content).unwrap();
        // The backend failed, but the request itself completed.
        assert!(end.protocol_status.is_complete());
    }

    #[test]
    fn an_overloaded_server_says_so_in_the_protocol_status() {
        let mut wire = Vec::new();
        write_error(&mut wire, 1, "busy", ServerErrorKind::Overloaded).unwrap();
        let mut cursor = wire.as_slice();
        let _ = next_non_empty(&mut cursor).unwrap();
        let _ = next_non_empty(&mut cursor).unwrap();
        let end = next_non_empty(&mut cursor).unwrap();
        assert_eq!(
            EndRequest::decode(&end.content).unwrap().protocol_status,
            ProtocolStatus::Overloaded
        );
    }

    #[test]
    fn a_multiplexed_connection_is_refused_rather_than_hung() {
        let mut wire = Vec::new();
        write_error(
            &mut wire,
            1,
            "no multiplexing",
            ServerErrorKind::NoMultiplexing,
        )
        .unwrap();
        let mut cursor = wire.as_slice();
        let _ = next_non_empty(&mut cursor).unwrap();
        let _ = next_non_empty(&mut cursor).unwrap();
        let end = next_non_empty(&mut cursor).unwrap();
        assert_eq!(
            EndRequest::decode(&end.content).unwrap().protocol_status,
            ProtocolStatus::CantMpxConn
        );
    }

    #[test]
    fn answers_a_get_values_probe() {
        let mut wire = Vec::new();
        write_get_values(&mut wire, &[("max_connections", "10")]).unwrap();

        let mut cursor = wire.as_slice();
        let result = next_non_empty(&mut cursor).unwrap();
        assert_eq!(result.record_type(), RecordType::GetValuesResult);
        assert_eq!(result.request_id(), stokehold_fcgi::MANAGEMENT_REQUEST_ID);
        let pairs = nvpair::decode_pairs(&result.content).unwrap();
        assert_eq!(pairs[0].0, b"max_connections");
        assert_eq!(pairs[0].1, b"10");
    }

    #[test]
    fn reads_a_management_request() {
        let payload = nvpair::encode_pairs([(&b"max_connections"[..], &b""[..])]);
        let wire = framed(RecordType::GetValues, 0, &payload);
        let names = read_management_request(&mut wire.as_slice())
            .unwrap()
            .unwrap();
        assert_eq!(names[0].0, b"max_connections");

        // A non-management record is a protocol error in this position.
        let wire = framed(RecordType::Stdin, 1, b"x");
        assert!(read_management_request(&mut wire.as_slice()).is_err());
    }

    #[test]
    fn converts_into_a_backend_request_without_losing_anything() {
        let mut wire = begin(true);
        wire.extend(params(&[("REQUEST_METHOD", "GET")]));
        wire.extend(stdin(b""));
        let incoming = read_request(&mut wire.as_slice()).unwrap().unwrap();

        let backend = incoming.into_backend_request();
        assert_eq!(backend.body, b"");
        assert!(backend.keep_conn);
        assert_eq!(backend.params[0].0, b"REQUEST_METHOD");
    }

    #[test]
    fn a_request_before_begin_is_skipped_rather_than_fatal() {
        // Some clients emit a management record first; skipping keeps the
        // connection usable.
        let mut wire = framed(RecordType::Data, 0, b"ignored");
        wire.extend(begin(false));
        wire.extend(params(&[("REQUEST_METHOD", "GET")]));
        wire.extend(stdin(b""));

        let request = read_request(&mut wire.as_slice()).unwrap().unwrap();
        assert_eq!(request.param_str("REQUEST_METHOD").as_deref(), Some("GET"));
    }

    #[test]
    fn split_response_reports_the_constant_request_id() {
        let response = Response {
            stdout: b"body".to_vec(),
            stderr: b"log".to_vec(),
            ..Response::default()
        };
        let (stdout, stderr, request_id) = split_response(&response);
        assert_eq!(stdout, b"body");
        assert_eq!(stderr, b"log");
        assert_eq!(request_id, stokehold_fcgi::REQUEST_ID);
    }

    #[test]
    fn timing_measures_something_positive() {
        let timing = Timing::start();
        std::thread::sleep(Duration::from_millis(1));
        assert!(timing.elapsed() >= Duration::from_millis(1));
    }
}
