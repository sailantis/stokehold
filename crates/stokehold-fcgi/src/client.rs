//! The FastCGI client.
//!
//! Deliberate design points, all consequences of the hazards recorded in
//! `docs/plan.md`:
//!
//! - **One request in flight per connection.** `php-cgi` processes requests
//!   sequentially, so multiplexing would add a demux table and buy nothing. A
//!   client owns one connection and sends one request at a time.
//! - **`STDOUT` is returned raw.** The FastCGI front-end relays these bytes
//!   verbatim, so the client never parses headers or bodies on the hot path.
//! - **`STDERR` is never dropped.** It is accumulated and handed back with the
//!   response. A caller that discards it will eventually block a backend on a
//!   full pipe — the classic "all workers hung" bug.
//! - **Keep-alive is reported, not assumed.** `FCGI_KEEP_CONN` is a request; the
//!   backend may still close. The caller decides whether to reuse.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use crate::error::{FcgiError, Result};
use crate::message::{BeginRequest, EndRequest};
use crate::nvpair::{self, NameValuePairs};
use crate::protocol::{ProtocolStatus, RecordType};
use crate::record::{self, Header};
use crate::transport::{Endpoint, Stream};

/// The request id used for every real request.
///
/// With one request in flight and no multiplexing a constant id is correct, and
/// it keeps captured traffic readable.
pub const REQUEST_ID: u16 = 1;

/// `GET_VALUES` is not tied to a request, so it uses request id 0.
pub const MANAGEMENT_REQUEST_ID: u16 = 0;

/// What to send to a backend for one request.
#[derive(Debug, Clone, Default)]
pub struct Request {
    /// The CGI environment, sent as `PARAMS`.
    pub params: NameValuePairs,
    /// The raw request body, sent as `STDIN`.
    ///
    /// stokehold never interprets this: multipart and `$_FILES` handling belongs
    /// to the PHP SAPI, so the bytes are passed through untouched.
    pub body: Vec<u8>,
    /// Ask the backend to keep the connection open after responding.
    pub keep_conn: bool,
}

/// What a backend sent back.
#[derive(Debug, Clone, Default)]
pub struct Response {
    /// Raw `STDOUT` bytes: a `Status:`-prefixed header block, a blank line, then
    /// the body. Left unparsed so it can be relayed verbatim.
    pub stdout: Vec<u8>,
    /// `STDERR` bytes. Always collected; see the module docs.
    pub stderr: Vec<u8>,
    /// The `END_REQUEST` body.
    pub end: Option<EndRequest>,
    /// Whether the connection may be reused for another request.
    pub keep_conn: bool,
    /// How long the backend took to answer.
    pub duration: Duration,
}

impl Response {
    /// The application status reported by `END_REQUEST`, if any.
    pub fn app_status(&self) -> Option<u32> {
        self.end.map(|end| end.app_status)
    }

    /// The protocol status reported by `END_REQUEST`, if any.
    pub fn protocol_status(&self) -> Option<ProtocolStatus> {
        self.end.map(|end| end.protocol_status)
    }

    /// Fail if the backend did not complete the request.
    ///
    /// A non-zero *application* status is not an error here: PHP applications
    /// legitimately exit non-zero, and the HTTP status lives in the `Status:`
    /// header instead. Only the protocol status indicates a transport-level
    /// refusal such as `FCGI_OVERLOADED`.
    pub fn check_protocol(&self) -> Result<()> {
        match self.end {
            None => Err(FcgiError::IncompleteResponse {
                request_id: REQUEST_ID,
                complete: false,
            }),
            Some(end) if !end.protocol_status.is_complete() => {
                Err(FcgiError::BackendProtocolStatus {
                    status: end.protocol_status,
                })
            }
            Some(_) => Ok(()),
        }
    }

    /// Interpret `STDOUT` as `Status:` header + headers + body.
    ///
    /// Only needed when a caller wants the HTTP status rather than passing the
    /// bytes through, so the hot path never calls this.
    pub fn parse_status_headers(&self) -> (u16, Vec<(String, String)>) {
        parse_response_headers(&self.stdout)
    }
}

/// Split a FastCGI `STDOUT` payload into its status and headers.
///
/// A response without a `Status:` header is `200` per the specification.
pub fn parse_response_headers(stdout: &[u8]) -> (u16, Vec<(String, String)>) {
    let text = String::from_utf8_lossy(stdout);
    let head = match text.find("\r\n\r\n") {
        Some(index) => &text[..index],
        None => match text.find("\n\n") {
            Some(index) => &text[..index],
            None => &text[..],
        },
    };

    let mut status = 200u16;
    let mut headers = Vec::new();
    for (index, line) in head.split('\n').enumerate() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        // Only a `Status:` on the first line is the response status; anywhere
        // else it is an ordinary header.
        if index == 0 && name.eq_ignore_ascii_case("status") {
            status = value
                .split_whitespace()
                .next()
                .and_then(|code| code.parse().ok())
                .unwrap_or(200);
            continue;
        }
        headers.push((name.trim().to_string(), value.to_string()));
    }
    (status, headers)
}

/// Split a FastCGI `STDOUT` payload into its header block and its body.
///
/// The inverse of [`parse_response_headers`], and used by anything that has to
/// speak a different protocol on the other side: the front-end needs the body
/// alone, because it writes its own header block.
///
/// A payload with no blank line is all body, which is what the specification's
/// "no headers" case looks like.
pub fn split_response_body(stdout: &[u8]) -> &[u8] {
    // Located on the bytes rather than on a lossy `String`, because the body is
    // arbitrary application bytes and a lossy conversion would corrupt it.
    let crlf = find_subslice(stdout, b"\r\n\r\n").map(|index| (index, 4));
    let lf = find_subslice(stdout, b"\n\n").map(|index| (index, 2));

    match (crlf, lf) {
        (Some((index, len)), Some((other, _))) if index <= other => &stdout[index + len..],
        (Some((index, len)), None) => &stdout[index + len..],
        (_, Some((index, len))) => &stdout[index + len..],
        (None, None) => &[],
    }
}

/// The offset of `needle` in `haystack`, if present.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// A client bound to one backend connection.
///
/// Generic over the transport so the protocol can be tested without a socket.
#[derive(Debug)]
pub struct Client<T = Stream> {
    stream: T,
    /// Whether the last request asked for keep-alive.
    keep_conn_requested: bool,
    /// Set once reuse is known to be unsafe.
    closed_by_backend: bool,
}

impl Client<Stream> {
    /// Connect to a backend.
    pub fn connect(endpoint: &Endpoint, timeout: Option<Duration>) -> Result<Self> {
        Ok(Client::from_stream(endpoint.connect(timeout)?))
    }
}

impl<T> Client<T> {
    /// Whether the connection is still usable.
    pub fn is_reusable(&self) -> bool {
        !self.closed_by_backend && self.keep_conn_requested
    }
}

impl<T: Read + Write> Client<T> {
    /// Wrap an already-connected transport, or a test double.
    pub fn from_stream(stream: T) -> Self {
        Client {
            stream,
            keep_conn_requested: false,
            closed_by_backend: false,
        }
    }

    /// Send one request and read its response.
    pub fn request(&mut self, request: &Request) -> Result<Response> {
        let started = Instant::now();
        self.keep_conn_requested = request.keep_conn;
        self.send(request)?;
        let mut response = self.receive()?;
        response.duration = started.elapsed();
        if !response.keep_conn {
            self.closed_by_backend = true;
        }
        Ok(response)
    }

    fn send(&mut self, request: &Request) -> Result<()> {
        let begin = BeginRequest::responder(request.keep_conn);
        let header = Header::new(RecordType::BeginRequest, REQUEST_ID, 8);
        header.write(&mut self.stream)?;
        self.stream.write_all(&begin.encode())?;

        let params = nvpair::encode_pairs(
            request
                .params
                .iter()
                .map(|(name, value)| (name.as_slice(), value.as_slice())),
        );
        record::write_records(
            &mut self.stream,
            RecordType::Params,
            REQUEST_ID,
            &params,
            true,
        )?;
        record::write_records(
            &mut self.stream,
            RecordType::Stdin,
            REQUEST_ID,
            &request.body,
            true,
        )?;
        Ok(())
    }

    fn receive(&mut self) -> Result<Response> {
        let mut response = Response::default();
        loop {
            let Some(record) = record::read_record(&mut self.stream)? else {
                return Err(FcgiError::IncompleteResponse {
                    request_id: REQUEST_ID,
                    complete: true,
                });
            };

            match record.header.record_type {
                RecordType::Stdout => {
                    if !record.content.is_empty() {
                        response.stdout.extend_from_slice(&record.content);
                    }
                }
                RecordType::Stderr => {
                    if !record.content.is_empty() {
                        response.stderr.extend_from_slice(&record.content);
                    }
                }
                RecordType::EndRequest => {
                    response.end = Some(EndRequest::decode(&record.content)?);
                    break;
                }
                // A backend may send a management record or a type we did not
                // ask for; ignore it rather than failing the request.
                _ => continue,
            }
        }

        response.keep_conn = self.keep_conn_requested && !self.closed_by_backend;
        Ok(response)
    }

    /// Read backend variables with `GET_VALUES`.
    ///
    /// This is the mechanism `php-fpm` exposes for health and status polling, so
    /// it doubles as the liveness probe for a slot.
    pub fn get_values<'a, I>(&mut self, names: I) -> Result<NameValuePairs>
    where
        I: IntoIterator<Item = &'a [u8]>,
    {
        let payload = nvpair::encode_pairs(names.into_iter().map(|name| (name, &b""[..])));
        record::write_records(
            &mut self.stream,
            RecordType::GetValues,
            MANAGEMENT_REQUEST_ID,
            &payload,
            false,
        )?;

        loop {
            let Some(record) = record::read_record(&mut self.stream)? else {
                return Err(FcgiError::IncompleteResponse {
                    request_id: MANAGEMENT_REQUEST_ID,
                    complete: false,
                });
            };
            match record.header.record_type {
                RecordType::GetValuesResult => return nvpair::decode_pairs(&record.content),
                // An unsupported name is answered with FCGI_UNKNOWN_TYPE, which
                // tells us this backend cannot be probed.
                RecordType::UnknownType => {
                    return Err(FcgiError::UnexpectedRecordType {
                        expected: RecordType::GetValuesResult,
                        actual: RecordType::UnknownType,
                    })
                }
                _ => continue,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A test double with independent input and output streams.
    ///
    /// A bare `Cursor` cannot be used for both directions: writing would advance
    /// the same position the reads depend on.
    #[derive(Debug, Default)]
    struct Loopback {
        out: Vec<u8>,
        input: Cursor<Vec<u8>>,
    }

    impl Loopback {
        fn replying(reply: Vec<u8>) -> Self {
            Loopback {
                out: Vec::new(),
                input: Cursor::new(reply),
            }
        }

        fn written(&self) -> &[u8] {
            &self.out
        }
    }

    impl Read for Loopback {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for Loopback {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            std::io::Write::write(&mut self.out, buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn framed(record_type: RecordType, content: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let header = Header::new(record_type, REQUEST_ID, content.len() as u16);
        header.write(&mut out).unwrap();
        out.extend_from_slice(content);
        out.extend(std::iter::repeat(0u8).take(header.padding_length as usize));
        out
    }

    fn end_record() -> Vec<u8> {
        framed(RecordType::EndRequest, &EndRequest::complete().encode())
    }

    fn client_for(reply: Vec<u8>) -> Client<Loopback> {
        Client::from_stream(Loopback::replying(reply))
    }

    #[test]
    fn receives_stdout_stderr_and_end() {
        let mut reply = framed(RecordType::Stdout, b"Status: 200 OK\r\n\r\nhello");
        reply.extend(framed(RecordType::Stderr, b"a warning"));
        reply.extend(end_record());

        let mut client = client_for(reply);
        let response = client.receive().unwrap();
        assert_eq!(response.stdout, b"Status: 200 OK\r\n\r\nhello");
        assert_eq!(response.stderr, b"a warning");
        assert_eq!(
            response.protocol_status(),
            Some(ProtocolStatus::RequestComplete)
        );
        response.check_protocol().unwrap();
    }

    #[test]
    fn empty_stream_records_are_ignored_but_end_is_required() {
        let mut reply = framed(RecordType::Stdout, b"body");
        reply.extend(framed(RecordType::Stdout, b""));
        assert!(client_for(reply).receive().is_err(), "missing END_REQUEST");

        let mut reply = framed(RecordType::Stdout, b"");
        reply.extend(end_record());
        let response = client_for(reply).receive().unwrap();
        assert!(response.stdout.is_empty());
        response.check_protocol().unwrap();
    }

    #[test]
    fn a_truncated_response_reports_incomplete_rather_than_hanging() {
        let reply = framed(RecordType::Stdout, b"partial");
        let err = client_for(reply).receive().unwrap_err();
        assert!(
            matches!(err, FcgiError::IncompleteResponse { complete: true, .. }),
            "{err}"
        );
    }

    #[test]
    fn an_overloaded_backend_surfaces_as_a_protocol_error() {
        let mut reply = framed(RecordType::Stdout, b"");
        reply.extend(framed(
            RecordType::EndRequest,
            &EndRequest {
                app_status: 0,
                protocol_status: ProtocolStatus::Overloaded,
            }
            .encode(),
        ));

        let response = client_for(reply).receive().unwrap();
        let err = response.check_protocol().unwrap_err();
        assert!(err.to_string().contains("Overloaded"), "{err}");
    }

    #[test]
    fn a_non_zero_application_status_is_not_a_protocol_failure() {
        let mut reply = framed(RecordType::Stdout, b"Status: 500\r\n\r\nboom");
        reply.extend(framed(
            RecordType::EndRequest,
            &EndRequest {
                app_status: 255,
                protocol_status: ProtocolStatus::RequestComplete,
            }
            .encode(),
        ));

        let response = client_for(reply).receive().unwrap();
        response.check_protocol().unwrap();
        assert_eq!(response.app_status(), Some(255));
        assert_eq!(response.parse_status_headers().0, 500);
    }

    #[test]
    fn sends_begin_params_and_terminated_stdin_records() {
        let mut reply = framed(RecordType::Stdout, b"ok");
        reply.extend(end_record());

        let mut client = client_for(reply);
        let request = Request {
            params: vec![(b"REQUEST_METHOD".to_vec(), b"GET".to_vec())],
            body: b"payload".to_vec(),
            keep_conn: true,
        };
        let response = client.request(&request).unwrap();
        assert_eq!(response.stdout, b"ok");
        assert!(response.keep_conn, "keep-alive requested and not refused");
        assert!(client.is_reusable());

        // Decode exactly what the client wrote.
        let written = client.stream.written().to_vec();
        let mut cursor = written.as_slice();
        let begin = record::read_record(&mut cursor).unwrap().unwrap();
        assert_eq!(begin.record_type(), RecordType::BeginRequest);
        assert_eq!(begin.request_id(), REQUEST_ID);
        assert!(BeginRequest::decode(&begin.content).unwrap().keep_conn);

        let params = record::read_record(&mut cursor).unwrap().unwrap();
        assert_eq!(params.record_type(), RecordType::Params);
        let pairs = nvpair::decode_pairs(&params.content).unwrap();
        assert_eq!(pairs, vec![(b"REQUEST_METHOD".to_vec(), b"GET".to_vec())]);

        // The PARAMS stream terminator, then the body and its terminator.
        assert!(record::read_record(&mut cursor)
            .unwrap()
            .unwrap()
            .content
            .is_empty());
        let stdin = record::read_record(&mut cursor).unwrap().unwrap();
        assert_eq!(stdin.record_type(), RecordType::Stdin);
        assert_eq!(stdin.content, b"payload");
        assert!(record::read_record(&mut cursor)
            .unwrap()
            .unwrap()
            .content
            .is_empty());
    }

    #[test]
    fn a_request_without_keep_conn_marks_the_connection_unusable() {
        let mut reply = framed(RecordType::Stdout, b"ok");
        reply.extend(end_record());

        let mut client = client_for(reply);
        let request = Request {
            keep_conn: false,
            ..Request::default()
        };
        let response = client.request(&request).unwrap();
        assert!(!response.keep_conn);
        assert!(!client.is_reusable(), "must not be reused after a close");
    }

    #[test]
    fn parses_status_and_headers() {
        let stdout = b"Status: 404 Not Found\r\nContent-Type: text/html\r\nX-Test: 1\r\n\r\nbody";
        let (status, headers) = parse_response_headers(stdout);
        assert_eq!(status, 404);
        assert_eq!(headers.len(), 2);
        assert_eq!(
            headers[0],
            ("Content-Type".to_string(), "text/html".to_string())
        );
        assert_eq!(headers[1], ("X-Test".to_string(), "1".to_string()));
    }

    #[test]
    fn a_response_without_a_status_header_is_200() {
        let (status, headers) = parse_response_headers(b"Content-Type: text/plain\r\n\r\nhi");
        assert_eq!(status, 200);
        assert_eq!(headers.len(), 1);
    }

    #[test]
    fn parses_headers_terminated_by_a_bare_line_feed() {
        let (status, headers) = parse_response_headers(b"Status: 201 Created\n\nbody");
        assert_eq!(status, 201);
        assert!(headers.is_empty());
    }

    #[test]
    fn headers_only_apply_when_status_is_first() {
        // A `Status:` header that is not the first line is an ordinary header.
        let (status, headers) =
            parse_response_headers(b"Content-Type: text/plain\r\nStatus: 404\r\n\r\n");
        assert_eq!(status, 200);
        assert_eq!(headers.len(), 2);
    }

    #[test]
    fn a_malformed_status_line_falls_back_to_200() {
        let (status, _) = parse_response_headers(b"Status: nonsense\r\n\r\n");
        assert_eq!(status, 200);
    }

    #[test]
    fn a_response_without_a_blank_line_is_still_parsed() {
        let (status, headers) = parse_response_headers(b"Status: 204 No Content\r\nX-A: b");
        assert_eq!(status, 204);
        assert_eq!(headers.len(), 1);
    }

    #[test]
    fn get_values_decodes_a_result_record() {
        let payload = nvpair::encode_pairs([
            (&b"max_connections"[..], &b"10"[..]),
            (&b"total_processes"[..], &b"3"[..]),
        ]);
        let mut reply = Vec::new();
        let header = Header::new(
            RecordType::GetValuesResult,
            MANAGEMENT_REQUEST_ID,
            payload.len() as u16,
        );
        header.write(&mut reply).unwrap();
        reply.extend_from_slice(&payload);

        let mut client = client_for(reply);
        let values = client.get_values([&b"max_connections"[..]]).unwrap();
        assert_eq!(values[0].0, b"max_connections");
        assert_eq!(values[0].1, b"10");

        let written = client.stream.written().to_vec();
        let record = record::read_record(&mut written.as_slice())
            .unwrap()
            .unwrap();
        assert_eq!(record.record_type(), RecordType::GetValues);
        assert_eq!(record.request_id(), MANAGEMENT_REQUEST_ID);
    }

    #[test]
    fn get_values_reports_an_unsupported_backend() {
        let mut reply = Vec::new();
        let header = Header::new(RecordType::UnknownType, MANAGEMENT_REQUEST_ID, 1);
        header.write(&mut reply).unwrap();
        reply.push(RecordType::GetValues.as_u8());

        let mut client = client_for(reply);
        assert!(client.get_values([&b"anything"[..]]).is_err());
    }

    #[test]
    fn request_ids_are_constant_with_no_multiplexing() {
        assert_eq!(REQUEST_ID, 1);
        assert_eq!(MANAGEMENT_REQUEST_ID, 0);
    }
}
