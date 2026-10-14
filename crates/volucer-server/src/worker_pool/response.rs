//! Turning a worker's answer back into what the rest of volucer expects: the
//! `Status:`-prefixed header block, the telemetry lines on `stderr`, and the reason
//! phrase table. **Pure**, and deliberately the only place the translation happens,
//! so both front-ends stay identical.


use std::time::{Duration};
use volucer_fcgi::protocol::ProtocolStatus;
use volucer_fcgi::{EndRequest, Response};
use volucer_worker::codec::WorkerResponse;
use volucer_worker::{LogRecord};


/// Turn a worker response into what the rest of volucer expects.
///
/// The body is relayed verbatim, exactly as for the FastCGI backends, but the
/// header block has to be produced here: a worker hands back a status and a list
/// of headers, while a front-end relays a `Status:`-prefixed block. Translating
/// once, at the backend, is what keeps both front-ends identical for every
/// backend kind.
pub(super) fn render(response: WorkerResponse, duration: Duration) -> Response {
    let mut stdout = Vec::with_capacity(response.body.len() + 128);
    stdout.extend_from_slice(
        format!(
            "Status: {} {}\r\n",
            response.head.status,
            reason_phrase(response.head.status)
        )
        .as_bytes(),
    );
    for (name, value) in &response.head.headers {
        stdout.extend_from_slice(name.as_bytes());
        stdout.extend_from_slice(b": ");
        stdout.extend_from_slice(value.as_bytes());
        stdout.extend_from_slice(b"\r\n");
    }
    stdout.extend_from_slice(b"\r\n");
    stdout.extend_from_slice(&response.body);

    Response {
        stdout,
        stderr: render_logs(&response.logs),
        end: Some(EndRequest {
            // Negative would mean the application reported a status that cannot
            // be expressed here; clamping is the honest thing to do with a value
            // the protocol only defines for zero and above.
            app_status: response.end.app_status.max(0) as u32,
            protocol_status: ProtocolStatus::RequestComplete,
        }),
        // The pool, not this worker, owns reuse: a worker that asked to be
        // recycled is already replaced, so the backend as a whole is still
        // ready for the next request. Nothing reads this to decide whether the
        // *client* connection survives — that is the client's own keep-alive.
        keep_conn: true,
        duration,
    }
}

/// Format the telemetry a worker emitted while serving a request.
///
/// It goes to `STDERR` because that is where a front-end already puts whatever a
/// backend says out of band: nginx writes it to its error log, which is where a
/// developer looks first. Without this a worker's `emitLog()` would be read,
/// stored, and then silently dropped.
fn render_logs(logs: &[LogRecord]) -> Vec<u8> {
    if logs.is_empty() {
        return Vec::new();
    }
    let mut out = String::new();
    for record in logs {
        out.push_str(&format!("[{}] {}\n", record.level, record.message));
    }
    out.into_bytes()
}

/// The reason phrase for a status code.
///
/// A front-end relays the `Status:` line verbatim, and HTTP/1.0 clients exist
/// that read it, so a bare code is not quite enough. Only the codes PHP
/// applications actually produce are listed; anything else is sent without a
/// phrase, which every client accepts because HTTP/1.1 makes the phrase optional.
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        418 => "I'm a teapot",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => "",
    }
}


#[cfg(test)]
mod tests {
    use volucer_worker::codec::WorkerResponse;
    use super::*;
    use volucer_worker::{ResponseHead};

#[test]
fn a_response_is_rendered_as_a_status_header_block() {
    let response = WorkerResponse {
        head: ResponseHead {
            status: 404,
            headers: vec![
                ("Content-Type".to_string(), "text/plain".to_string()),
                ("Set-Cookie".to_string(), "a=1".to_string()),
                ("Set-Cookie".to_string(), "b=2".to_string()),
            ],
            body_len: 9,
        },
        body: b"not found".to_vec(),
        end: volucer_worker::RequestEnd {
            app_status: 1,
            peak_memory_bytes: 4096,
            duration_us: 250,
            keep_alive: false,
        },
        logs: vec![LogRecord {
            level: "warning".to_string(),
            message: "something happened".to_string(),
            request_id: 1,
        }],
        metrics: Vec::new(),
    };

    let rendered = render(response, Duration::from_millis(3));
    assert_eq!(
        rendered.stdout,
        b"Status: 404 Not Found\r\nContent-Type: text/plain\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\n\r\nnot found"
    );
    assert_eq!(rendered.app_status(), Some(1));
    assert!(rendered.check_protocol().is_ok());
    assert_eq!(
        rendered.stderr, b"[warning] something happened\n",
        "telemetry has to reach somewhere a developer looks"
    );
    // A worker that asked to be recycled does not make the backend unusable:
    // the pool replaces it.
    assert!(rendered.keep_conn);
}

#[test]
fn a_worker_with_no_telemetry_produces_no_stderr() {
    assert!(render(WorkerResponse::default(), Duration::ZERO)
        .stderr
        .is_empty());
}

#[test]
fn a_negative_application_status_is_clamped_rather_than_wrapped() {
    let mut response = WorkerResponse::default();
    response.end.app_status = -1;
    assert_eq!(render(response, Duration::ZERO).app_status(), Some(0));
}

#[test]
fn an_unknown_status_has_no_reason_phrase() {
    assert_eq!(reason_phrase(200), "OK");
    assert_eq!(reason_phrase(500), "Internal Server Error");
    assert_eq!(reason_phrase(599), "");
    let mut response = WorkerResponse::default();
    response.head.status = 599;
    assert!(render(response, Duration::ZERO)
        .stdout
        .starts_with(b"Status: 599 \r\n"));
}
}
