//! Translating a request into what a worker receives: the borrowed CGI environment,
//! the `PARAMS`, and the JSON frame head. **Pure** — no process, no lock, no I/O —
//! which is why it is testable on its own, and why the borrowed path can be compared
//! byte-for-byte against the owning one.


use std::borrow::Cow;
use volucer_fcgi::{NameValuePairs, Request};
use volucer_worker::{BorrowedHeaders, Codec, Json, RequestHeader, RequestHeaderJson};

#[cfg(test)]
use super::params;


/// A CGI parameter as a borrowed string, lossily for the same reason the owned path is.
///
/// `from_utf8_lossy` returns `Cow::Borrowed` for the bytes both front-ends actually produce —
/// they build every value from a `&str` — so the common path copies nothing, and a value that
/// is somehow not UTF-8 still becomes the same replacement-character string the owned
/// translation would have made.
fn param_cow<'a>(params: &'a NameValuePairs, name: &str) -> Option<Cow<'a, str>> {
    params
        .iter()
        .find(|(key, _)| key.as_slice() == name.as_bytes())
        .map(|(_, value)| String::from_utf8_lossy(value))
}

/// The request, read back out of the CGI environment without copying it.
///
/// This is the pool's translation, and the counterpart of [`request_header`]: that one owns
/// everything, this one borrows everything. Both must produce the same frame, and the pool uses
/// this one because owning a `RequestHeader` only to serialize it is nine `String`s and two
/// `Vec`s per request that are dropped before the worker has answered.
///
/// Every field is a slice of the `PARAMS` pair it came from — including the header values, whose
/// bytes a CGI variable shares with the client's header — and the one thing that is *not* a slice,
/// the `HTTP_ACCEPT_ENCODING` → `Accept-Encoding` name, is transformed while it is being
/// serialized rather than into an owned `String` first. See [`CgiHeader`].
#[doc(hidden)]
pub struct BorrowedRequest<'a> {
    method: Cow<'a, str>,
    uri: Cow<'a, str>,
    protocol: Cow<'a, str>,
    remote_addr: Cow<'a, str>,
    script_filename: Cow<'a, str>,
    https: bool,
    headers: RequestHeaderParams<'a>,
}

impl<'a> BorrowedRequest<'a> {
    /// The frame's JSON head, borrowing this translation and the environment under it.
    ///
    /// `body_len` is left at zero because the channel stamps in the length of the body it is
    /// about to frame, so the declared length and the bytes sent cannot disagree.
    #[doc(hidden)]
    pub fn draft(&self) -> RequestHeaderJson<'_, RequestHeaderParams<'_>, CgiHeader<'_>> {
        RequestHeaderJson {
            method: &self.method,
            uri: &self.uri,
            protocol: &self.protocol,
            headers: BorrowedHeaders::new(self.headers.clone()),
            remote_addr: &self.remote_addr,
            body_len: 0,
            script_filename: &self.script_filename,
            https: self.https,
        }
    }
}

/// Serialize a request head into an owned buffer.
///
/// One allocation per request, and it buys the removal of nine: the head describes the request by
/// *borrowing* the CGI environment, so the alternative is not "no buffer" but "an owned
/// `RequestHeader`" — five `String`s, two per request header, plus a `Vec` for the pairs.
///
/// The caller has already stamped `body_len` in, because this function is on the pool's side of
/// the thread boundary and the body is on the channel's side of it.
pub(super) fn encode_head<'a>(
    head: &RequestHeaderJson<'a, RequestHeaderParams<'a>, CgiHeader<'a>>,
) -> Vec<u8> {
    Json::encode(head).unwrap_or_else(|err| {
        // Serializing `&str`s and `u64`s cannot fail, and a panic here would take the server down
        // for one request. An empty payload makes the worker answer with a protocol error, which is
        // reported as the backend failure it is.
        tracing::error!(%err, "the worker request head could not be serialized");
        Vec::new()
    })
}

/// Turn a FastCGI request into the worker frame's head, borrowing the environment.
///
/// The same translation as [`request_header`], with the same defaults for a missing variable,
/// and it exists so the pool does not build the owned `RequestHeader` at all.
///
/// `pub` and `#[doc(hidden)]` so `tests/request_header_allocations.rs` can measure it beside
/// `request_header`; it is not re-exported and is not part of the interface.
#[doc(hidden)]
pub fn borrowed_request(request: &Request) -> BorrowedRequest<'_> {
    let params = &request.params;

    BorrowedRequest {
        method: param_cow(params, "REQUEST_METHOD").unwrap_or(Cow::Borrowed("GET")),
        uri: param_cow(params, "REQUEST_URI").unwrap_or(Cow::Borrowed("/")),
        protocol: param_cow(params, "SERVER_PROTOCOL").unwrap_or(Cow::Borrowed("HTTP/1.1")),
        remote_addr: param_cow(params, "REMOTE_ADDR").unwrap_or(Cow::Borrowed("")),
        script_filename: param_cow(params, "SCRIPT_FILENAME").unwrap_or(Cow::Borrowed("")),
        https: match params.iter().find(|(key, _)| key.as_slice() == b"HTTPS") {
            // Present means the request arrived over TLS; CGI uses "on" and "off", and
            // the front-end only writes the former.
            Some((_, value)) => !value.eq_ignore_ascii_case(b"off") && !value.is_empty(),
            None => false,
        },
        headers: RequestHeaderParams::new(params),
    }
}

/// The request's headers, borrowed out of the CGI environment.
///
/// A CGI variable's value *is* the client's header value — `to_cgi` put those bytes there — so
/// the value needs no copy at all. The name does need one transformation, and that is deferred to
/// [`CgiHeader`], which does it into a stack buffer while the JSON is being written.
///
/// `CONTENT_TYPE` is appended last because it does not carry the `HTTP_` prefix, exactly as
/// [`headers_from_params`] appends it. `CONTENT_LENGTH` is deliberately absent from both.
#[derive(Clone)]
#[doc(hidden)]
pub struct RequestHeaderParams<'a> {
    params: std::slice::Iter<'a, (Vec<u8>, Vec<u8>)>,
    content_type: Option<&'a [u8]>,
    content_type_yielded: bool,
}

impl<'a> RequestHeaderParams<'a> {
    fn new(params: &'a NameValuePairs) -> Self {
        Self {
            params: params.iter(),
            content_type: params
                .iter()
                .find(|(key, _)| key.as_slice() == b"CONTENT_TYPE")
                .map(|(_, value)| value.as_slice()),
            content_type_yielded: false,
        }
    }
}

impl<'a> Iterator for RequestHeaderParams<'a> {
    type Item = CgiHeader<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        for (name, value) in self.params.by_ref() {
            let Ok(name) = std::str::from_utf8(name) else {
                continue;
            };
            // Not a request header — `DOCUMENT_ROOT`, `SCRIPT_FILENAME`, `HTTPS` and the rest
            // are all environment variables with their own meaning.
            let Some(suffix) = name.strip_prefix("HTTP_") else {
                continue;
            };
            // A bare `HTTP_` prefix names nothing, and `headers_from_params` drops it rather
            // than producing a header with an empty name. The two translations have to agree
            // byte for byte, so this drops it too.
            if suffix.is_empty() {
                continue;
            }
            return Some(CgiHeader {
                name,
                value: String::from_utf8_lossy(value),
            });
        }

        if std::mem::replace(&mut self.content_type_yielded, true) {
            return None;
        }
        self.content_type.map(|value| CgiHeader {
            name: "CONTENT_TYPE",
            value: String::from_utf8_lossy(value),
        })
    }
}

/// One request header, named the way the CGI environment names it.
///
/// Serializes as a two-element array, byte for byte like the `(&str, &str)` pairs the owned path
/// produces, so the worker cannot tell which translation it was sent.
#[doc(hidden)]
pub struct CgiHeader<'a> {
    name: &'a str,
    value: Cow<'a, str>,
}

impl serde::Serialize for CgiHeader<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        (HttpHeaderName(self.name), self.value.as_ref()).serialize(serializer)
    }
}

impl<'a> serde::Serialize for HttpHeaderName<'a> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        // The name is transformed here rather than before, which is what keeps it off the
        // heap: a header name is short, so it fits the stack buffer, and the common case
        // allocates nothing at all. Anything else — a name too long, or one that is not
        // ASCII, where the character-wise case folding `header_name` does differs from the
        // byte-wise kind — falls back to the owning form.
        let suffix = self.0.strip_prefix("HTTP_").unwrap_or(self.0);
        let mut stack = [0u8; HTTP_HEADER_NAME_CAPACITY];
        match render_http_header_name(suffix, &mut stack) {
            Some(name) => serializer.serialize_str(name),
            None => serializer.serialize_str(&header_name(suffix)),
        }
    }
}

/// The longest header name rendered without allocating.
///
/// Long enough for any real name — the longest registered request header is
/// `Access-Control-Request-Headers` at 30 bytes — and short enough to be free on the stack.
const HTTP_HEADER_NAME_CAPACITY: usize = 128;

/// `ACCEPT_ENCODING` → `Accept-Encoding`, into a caller-supplied buffer.
///
/// Returns `None` when the name does not fit or is not ASCII, and the caller then owns it. The
/// rendering can never be longer than the suffix: one byte of `_` becomes one byte of `-`, and
/// the case fold is in place.
fn render_http_header_name<'b>(
    suffix: &str,
    out: &'b mut [u8; HTTP_HEADER_NAME_CAPACITY],
) -> Option<&'b str> {
    if !suffix.is_ascii() || suffix.len() > out.len() {
        return None;
    }
    let mut written = 0;
    for (index, segment) in suffix.split('_').enumerate() {
        if index > 0 {
            out[written] = b'-';
            written += 1;
        }
        for (position, byte) in segment.bytes().enumerate() {
            out[written] = if position == 0 {
                byte.to_ascii_uppercase()
            } else {
                byte.to_ascii_lowercase()
            };
            written += 1;
        }
    }
    std::str::from_utf8(&out[..written]).ok()
}

/// A CGI header name, rendered as the HTTP name it stands for.
struct HttpHeaderName<'a>(&'a str);

/// Turn a FastCGI request into a worker `REQUEST` header.
///
/// The translation goes through the CGI parameters rather than the original HTTP
/// request, and that is the better direction: the front-end has already resolved
/// the path against the document root, so `SCRIPT_FILENAME` here is the file it
/// decided to run, not a second guess at it. It also means the pool needs no
/// routing of its own — one source of truth for what the request is.
///
/// The pool does **not** call this: [`borrowed_request`] is the same translation without the
/// ownership. This is kept because it is the readable statement of what the frame contains, it is
/// what the unit tests assert against, and `tests/request_header_allocations.rs` counts what it
/// costs, which is the number the borrowed path is measured against. `pub` and
/// `#[doc(hidden)]` so that test can reach it; it is not re-exported from the crate root.
#[doc(hidden)]
pub fn request_header(request: &Request) -> RequestHeader {
    let params = &request.params;

    RequestHeader {
        method: text_param(params, "REQUEST_METHOD").unwrap_or_else(|| "GET".to_string()),
        uri: text_param(params, "REQUEST_URI").unwrap_or_else(|| "/".to_string()),
        protocol: text_param(params, "SERVER_PROTOCOL").unwrap_or_else(|| "HTTP/1.1".to_string()),
        headers: headers_from_params(params),
        remote_addr: text_param(params, "REMOTE_ADDR").unwrap_or_default(),
        // Overwritten by `WorkerChannel::request` from the body it frames, so the
        // declared length and the bytes sent cannot disagree.
        body_len: 0,
        script_filename: text_param(params, "SCRIPT_FILENAME").unwrap_or_default(),
        https: match params.iter().find(|(key, _)| key.as_slice() == b"HTTPS") {
            // Present means the request arrived over TLS; CGI uses "on" and
            // "off", and the front-end only writes the former.
            Some((_, value)) => !value.eq_ignore_ascii_case(b"off") && !value.is_empty(),
            None => false,
        },
    }
}

/// A CGI parameter as a string.
pub(super) fn text_param(params: &NameValuePairs, name: &str) -> Option<String> {
    params
        .iter()
        .find(|(key, _)| key.as_slice() == name.as_bytes())
        .map(|(_, value)| String::from_utf8_lossy(value).into_owned())
}

/// Rebuild HTTP headers from the CGI parameters.
///
/// `CONTENT_LENGTH` is deliberately *not* turned back into a header: `body_len`
/// is authoritative, and two sources for one number is how they drift apart.
/// Everything else the front-end wrote round-trips, because it built these
/// variables from the client's headers in the first place.
fn headers_from_params(params: &NameValuePairs) -> Vec<(String, String)> {
    let mut headers = Vec::with_capacity(params.len());
    for (name, value) in params {
        let Ok(name) = std::str::from_utf8(name) else {
            continue;
        };
        let Some(suffix) = name.strip_prefix("HTTP_") else {
            continue;
        };
        if suffix.is_empty() {
            continue;
        }
        headers.push((
            header_name(suffix),
            String::from_utf8_lossy(value).into_owned(),
        ));
    }

    // `Content-Type` keeps its CGI name, which is exactly how it arrives.
    if let Some((_, value)) = params
        .iter()
        .find(|(key, _)| key.as_slice() == b"CONTENT_TYPE")
    {
        headers.push((
            "Content-Type".to_string(),
            String::from_utf8_lossy(value).into_owned(),
        ));
    }

    headers
}

/// `ACCEPT_ENCODING` → `Accept-Encoding`.
///
/// Cosmetic, but worth doing: these names end up in application logs and in
/// whatever a handler does with them, and `HTTP_X_FORWARDED_FOR` reconstructed by
/// the SDK means either spelling works for lookup.
fn header_name(cgi_suffix: &str) -> String {
    let mut name = String::with_capacity(cgi_suffix.len());
    for (index, segment) in cgi_suffix.split('_').enumerate() {
        if index > 0 {
            name.push('-');
        }
        let mut chars = segment.chars();
        if let Some(first) = chars.next() {
            name.extend(first.to_uppercase());
            name.extend(chars.flat_map(char::to_lowercase));
        }
    }
    name
}


#[cfg(test)]
mod tests {
    use super::*;

#[test]
fn cgi_header_names_are_rebuilt_in_canonical_form() {
    assert_eq!(header_name("ACCEPT_ENCODING"), "Accept-Encoding");
    assert_eq!(header_name("HOST"), "Host");
    assert_eq!(header_name("X_FORWARDED_FOR"), "X-Forwarded-For");
    assert_eq!(header_name("USER_AGENT"), "User-Agent");
}

/// The stack-buffer rendering must agree with the owning one, name for name.
///
/// `render_http_header_name` exists so a request header's name is never heap-allocated, and it
/// is a second implementation of what `header_name` does — which is exactly the kind of pair
/// that drifts. It also has to answer `None` rather than truncate for a name it cannot render,
/// because a silently shortened header name would reach the application as a different header.
#[test]
fn the_stack_buffer_rendering_agrees_with_the_owning_one() {
    let names = [
        "HTTP_",
        "HTTP_HOST",
        "HTTP_ACCEPT_ENCODING",
        "HTTP_X_FORWARDED_FOR",
        "HTTP_CONTENT_TYPE",
        "HTTP_A",
        "HTTP_A_B_C_D",
        "HTTP_USER_AGENT",
        // Already canonical, so the fold is a no-op.
        "HTTP_Host",
    ];
    for name in names {
        let suffix = name.strip_prefix("HTTP_").unwrap_or(name);
        let mut stack = [0u8; HTTP_HEADER_NAME_CAPACITY];
        let rendered = render_http_header_name(suffix, &mut stack)
            .unwrap_or_else(|| panic!("{name} did not fit the stack buffer"));
        assert_eq!(rendered, header_name(suffix), "{name}");
    }

    // A name that cannot fit is refused rather than truncated: the rendered name can never be
    // longer than its suffix, so the capacity is the only limit.
    let long = "A".repeat(HTTP_HEADER_NAME_CAPACITY + 1);
    let mut stack = [0u8; HTTP_HEADER_NAME_CAPACITY];
    assert!(render_http_header_name(&long, &mut stack).is_none());

    // Non-ASCII would case-fold differently byte-wise than character-wise, so it goes to the
    // owning path rather than being folded here.
    let mut stack = [0u8; HTTP_HEADER_NAME_CAPACITY];
    assert!(render_http_header_name("CAFÉ", &mut stack).is_none());
}

#[test]
fn a_request_header_is_translated_from_the_cgi_parameters() {
    let request = Request {
        params: params(&[
            ("REQUEST_METHOD", "POST"),
            ("REQUEST_URI", "/index.php/foo?x=1"),
            ("SERVER_PROTOCOL", "HTTP/1.1"),
            ("REMOTE_ADDR", "127.0.0.1"),
            ("SCRIPT_FILENAME", "/srv/app/index.php"),
            ("HTTP_HOST", "example.test"),
            ("HTTP_ACCEPT_ENCODING", "gzip"),
            ("CONTENT_TYPE", "application/json"),
            ("CONTENT_LENGTH", "12"),
            ("HTTPS", "on"),
        ]),
        body: Vec::new(),
        keep_conn: true,
    };

    let header = request_header(&request);
    assert_eq!(header.method, "POST");
    assert_eq!(header.uri, "/index.php/foo?x=1");
    assert_eq!(header.protocol, "HTTP/1.1");
    assert_eq!(header.remote_addr, "127.0.0.1");
    assert_eq!(header.script_filename, "/srv/app/index.php");
    assert!(header.https);
    assert_eq!(
        header.body_len, 0,
        "the channel decides this, not the CGI env"
    );
    assert_eq!(header.header("host"), Some("example.test"));
    assert_eq!(header.header("Accept-Encoding"), Some("gzip"));
    assert_eq!(header.header("content-type"), Some("application/json"));
    // `CONTENT_LENGTH` is the body's business, and the body is what says so.
    assert_eq!(header.header("content-length"), None);
}

#[test]
fn the_https_flag_needs_a_truthy_value() {
    let off = Request {
        params: params(&[("HTTPS", "off")]),
        ..Request::default()
    };
    assert!(!request_header(&off).https);

    let absent = Request::default();
    assert!(!request_header(&absent).https);
}

/// The two translations of one request must put the same bytes on the wire.
///
/// The pool sends the borrowed one; `request_header` is the readable statement of what the
/// frame contains. Two implementations of one protocol rule is how they drift, so this
/// compares the encodings of the *whole* frame head — every field, not a selection — for
/// environments chosen to cover the corners: no request headers, the ordinary set, a header
/// whose value is not UTF-8, a variable that is not a request header at all, a bare `HTTP_`
/// prefix, and `CONTENT_TYPE`, which keeps its CGI name.
#[test]
fn the_borrowed_translation_encodes_identically_to_the_owned_one() {
    let environments: Vec<NameValuePairs> = vec![
        // No request headers, so both paths are the fixed fields plus `CONTENT_TYPE`.
        params(&[("REQUEST_METHOD", "POST"), ("CONTENT_TYPE", "application/json")]),
        // The ordinary page: several headers, one with a multi-word name, one that is not
        // a request header, and `CONTENT_TYPE` arriving in its CGI spelling.
        params(&[
            ("REQUEST_METHOD", "POST"),
            ("REQUEST_URI", "/index.php/foo?x=1"),
            ("SERVER_PROTOCOL", "HTTP/1.1"),
            ("REMOTE_ADDR", "127.0.0.1"),
            ("SCRIPT_FILENAME", "/srv/app/index.php"),
            ("DOCUMENT_ROOT", "/srv/app"),
            ("PATH_INFO", "/foo"),
            ("HTTP_HOST", "example.test"),
            ("HTTP_ACCEPT_ENCODING", "gzip, deflate"),
            ("HTTP_X_FORWARDED_FOR", "10.0.0.1"),
            ("CONTENT_TYPE", "application/json"),
            ("CONTENT_LENGTH", "12"),
        ]),
        // Corners: a bare `HTTP_` prefix (which names nothing and must be dropped by both),
        // a non-UTF-8 value (which both must render lossily the same way), and no
        // `CONTENT_TYPE` at all.
        vec![
            (b"REQUEST_METHOD".to_vec(), b"GET".to_vec()),
            (b"HTTP_".to_vec(), b"empty-name".to_vec()),
            (b"HTTP_X_BYTES".to_vec(), vec![0x80, 0xff, b'x']),
        ],
    ];

    for environment in environments {
        let request = Request {
            params: environment,
            body: b"a body".to_vec(),
            keep_conn: true,
        };

        // `body_len` is zero on both: the channel stamps it in from the body it frames.
        let owned = Json::encode(&request_header(&request).draft(0)).expect("encode");
        let borrowed =
            Json::encode(&borrowed_request(&request).draft()).expect("encode");

        assert_eq!(
            String::from_utf8_lossy(&borrowed),
            String::from_utf8_lossy(&owned),
            "the borrowed head must encode identically to the owned one"
        );
        assert_eq!(borrowed, owned, "and byte for byte, not merely printed alike");
    }
}
}
