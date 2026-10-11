//! Serving files from the document root, without a backend.
//!
//! This is the layer that turns "serves PHP" into "can front a site". It sits
//! above the backends, not beside them: a request is answered here when the path
//! names a real file, and goes to PHP when it does not. That is exactly why an
//! ordinary application with a front controller keeps working — `/about` is not a
//! file, so it is still PHP's.
//!
//! # The two rules it must not break
//!
//! **A URL path is untrusted text**, and it must never name a file outside the
//! document root. `..` segments are refused before anything else happens, and
//! percent-decoding is deliberately *not* applied: the raw path decides which
//! file is served, so `%2e%2e%2f` cannot travel further than the literal string
//! `%2e%2e%2f`. PHP's router, which does decode, is reached through the front
//! controller and is a separate trust boundary.
//!
//! **No blocking call on the runtime thread.** Resolving a path means `stat` and
//! `open`, which are not quick on every filesystem, so the decision is made in a
//! single [`tokio::task::spawn_blocking`] closure that returns a [`Plan`]. Only
//! the bytes are handled on the runtime, through [`tokio::fs::File`], which runs
//! its reads on the blocking pool for us.
//!
//! # What is deliberately missing
//!
//! - **Zero-copy** (`sendfile`, `TransmitFile`). A file is streamed in 64 KiB
//!   blocks instead: a copy per block, and a real cost on large files. Doing
//!   better means `hyper`'s Unix-only `SendFile` body, which is a second code
//!   path rather than a faster one.
//! - **Compression on the fly.** Precompressed variants are served; nothing is
//!   compressed here, because doing it per request spends a worker thread on work
//!   the build should have done once.

use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use hyper::body::Body;
use hyper::header::{
    ACCEPT_ENCODING, ACCEPT_RANGES, ALLOW, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH,
    CONTENT_RANGE, CONTENT_TYPE, ETAG, IF_MATCH, IF_MODIFIED_SINCE, IF_NONE_MATCH, IF_RANGE,
    LAST_MODIFIED, LOCATION, RANGE, VARY,
};
use hyper::{HeaderMap, Method, Response, StatusCode};

use stokehold_core::config::StaticConfig;

use crate::error::HttpError;
use crate::listener::{body_of, ResponseBody};
/// The boundary a multi-range answer declares its parts with.
///
/// Fixed rather than random: a boundary only has to be something the parts do not
/// contain, and a part is a slice of a file this server chose, so a literal that
/// cannot appear in a `Content-Range` line is enough. A random one would cost a
/// source of randomness per request and buy nothing.
const MULTIPART_BOUNDARY: &str = "stokehold-boundary";

/// How much of a file to hold in memory at once when streaming it.
const STREAM_CHUNK: usize = 64 * 1024;

/// The static layer's configuration and document root.
#[derive(Debug, Clone)]
pub struct StaticFiles {
    pub config: StaticConfig,
    /// The root a request path is resolved against.
    pub document_root: PathBuf,
    /// The listener's own port, for a redirect when no `Host` header arrived.
    pub server_port: u16,
}

/// A response this layer produced. The body is boxed so a file can stream while
/// an error page is a few bytes.
pub type StaticResponse = Response<ResponseBody>;

/// Everything the layer needs to know about a request.
#[derive(Debug)]
pub struct Request<'a> {
    pub method: &'a Method,
    /// The raw request path, still percent-encoded.
    pub path: &'a str,
    pub headers: &'a HeaderMap,
    /// The `Host` header, used verbatim in a redirect.
    pub host: Option<&'a str>,
}

/// Answer a static request, or say it is not one.
///
/// `None` means "not a file: dispatch it". Every error here is a *complete*
/// answer — a 404, 403, 416 — because this layer exists so PHP never sees a
/// request for a file that is not there.
pub async fn respond(
    request: Request<'_>,
    files: &StaticFiles,
) -> Option<Result<StaticResponse, HttpError>> {
    if !files.config.enabled {
        return None;
    }

    // Only these are meaningful for a file. Anything else — `POST`, `PUT`,
    // `DELETE` — belongs to the application even when the path names a file,
    // because a framework may well route it.
    if !matches!(
        *request.method,
        Method::GET | Method::HEAD | Method::OPTIONS
    ) {
        return None;
    }

    let plan = decide(&request, files).await;
    Some(match plan {
        Plan::NotAFile => return None,
        Plan::NotFound => Ok(status_only(StatusCode::NOT_FOUND)),
        Plan::Forbidden => Ok(status_only(StatusCode::FORBIDDEN)),
        Plan::BadRequest(reason) => Err(HttpError::BadRequest(reason)),
        Plan::Redirect(location) => Ok(Response::builder()
            .status(StatusCode::MOVED_PERMANENTLY)
            .header(LOCATION, location)
            .body(body_of(Bytes::new()))
            .expect("a redirect is a valid response")),
        Plan::File {
            path,
            encoding,
            length,
            modified,
        } => answer(&request, files, path, encoding, length, modified).await,
    })
}

/// What the blocking half decided.
///
/// Returning a plan is what keeps every `stat` off the runtime thread while
/// leaving the bytes on it.
#[derive(Debug)]
enum Plan {
    /// Nothing here: the backend's business.
    NotAFile,
    NotFound,
    Forbidden,
    BadRequest(String),
    /// A directory URL without its trailing slash.
    Redirect(String),
    File {
        path: PathBuf,
        encoding: Option<&'static str>,
        length: u64,
        modified: Option<SystemTime>,
    },
}

/// The values the blocking closure needs, copied out of the request.
///
/// Owned strings rather than a shared `HeaderMap`, because the closure must be
/// `'static`. Only `Accept-Encoding` is read here: everything else a file's
/// answer depends on is decided after the blocking half has found the file.
#[derive(Debug, Default)]
struct Wanted {
    accept_encoding: Option<String>,
}

impl Wanted {
    fn from(request: &Request<'_>) -> Self {
        Wanted {
            accept_encoding: header(request.headers, ACCEPT_ENCODING).map(str::to_string),
        }
    }
}

/// Run the blocking decision, then the asynchronous answer.
async fn decide(request: &Request<'_>, files: &StaticFiles) -> Plan {
    let owned = Owned {
        path: request.path.to_string(),
        host: request.host.map(str::to_string),
        wanted: Wanted::from(request),
    };
    let files = files.clone();

    tokio::task::spawn_blocking(move || plan(&owned, &files))
        .await
        // A panic in the closure means a bug, but the honest answer to the
        // client is that the file is not available, not a 500 from the layer
        // that exists to serve files.
        .unwrap_or(Plan::NotFound)
}

/// What `decide` carries across the thread boundary.
#[derive(Debug)]
struct Owned {
    path: String,
    host: Option<String>,
    wanted: Wanted,
}

/// The decision, made off the runtime thread.
fn plan(owned: &Owned, files: &StaticFiles) -> Plan {
    let path = owned.path.as_str();
    if path.contains('\0') {
        return Plan::BadRequest("the path contains a NUL byte".to_string());
    }

    let resolved = match resolve_under_root(&files.document_root, path) {
        Resolution::Path(path) => path,
        // A dotfile is reported as missing rather than forbidden, matching how
        // nginx hides one: whether a file exists is not a client's business.
        Resolution::Dotfile => return Plan::NotFound,
        // Reported rather than clamped: a client that asked for
        // `../../etc/passwd` should not be told the path simply does not exist.
        Resolution::Outside => return Plan::Forbidden,
    };

    if files.config.directory_index {
        if path.ends_with('/') {
            let index = resolved.join("index.html");
            if !index.is_file() {
                // A directory with no index is not a file, so PHP gets it — an
                // application may well have a route for it.
                return Plan::NotAFile;
            }
            return describe(index, owned, files);
        }
        // A directory without its trailing slash: `docs` becomes `docs/`, so
        // relative links inside the page resolve against it.
        if resolved.is_dir() && resolved.join("index.html").is_file() {
            return Plan::Redirect(directory_location(owned, files.server_port));
        }
    }

    if !resolved.is_file() {
        return Plan::NotAFile;
    }

    // A file PHP is meant to execute is never served from disk: sending
    // `index.php` as a file discloses its source, and `<?php …` is a valid
    // response body, so nothing downstream would notice. It goes to the
    // backend, which is also what makes `/index.php/foo` work. This is why
    // `has_php_extension` is shared with the routing layer rather than
    // duplicated: two lists of "what is a script" would drift, and the one that
    // drifted would be the one that serves source code.
    if stokehold_fcgi::cgi::has_php_extension(path) {
        return Plan::NotAFile;
    }

    describe(resolved, owned, files)
}

/// Turn a file that is known to exist into a `Plan::File`.
fn describe(path: PathBuf, owned: &Owned, files: &StaticFiles) -> Plan {
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        // It was there a moment ago and is not now. A 404 is the correct and
        // unremarkable answer to a request racing a deployment.
        Err(_) => return Plan::NotFound,
    };

    let (path, encoding, length) = choose_encoding(&path, metadata.len(), &owned.wanted, files);
    Plan::File {
        path,
        encoding,
        length,
        modified: metadata.modified().ok(),
    }
}

/// Resolve a URL path to a file under the document root.
#[derive(Debug)]
enum Resolution {
    Path(PathBuf),
    /// A dotfile component, which the configuration refuses.
    Dotfile,
    /// A path that tries to leave the document root.
    Outside,
}

/// Join a request path onto the document root, refusing escapes.
///
/// Only `Normal` components are accepted, and nothing is percent-decoded: the raw
/// text decides which file is served, so an encoded `..` is a file name that
/// happens not to exist rather than a way out of the root.
fn resolve_under_root(root: &Path, path: &str) -> Resolution {
    let mut out = root.to_path_buf();
    let mut saw_dotfile = false;

    for component in Path::new(path).components() {
        match component {
            Component::Normal(segment) => {
                if segment.to_string_lossy().starts_with('.') {
                    saw_dotfile = true;
                }
                out.push(segment);
            }
            // The leading `/`, which is a root marker rather than a segment.
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => return Resolution::Outside,
        }
    }

    if saw_dotfile {
        return Resolution::Dotfile;
    }
    Resolution::Path(out)
}

/// The `Location` for a directory requested without its trailing slash.
fn directory_location(owned: &Owned, server_port: u16) -> String {
    // `Host` is the client's own, and a client follows the redirect, so it is
    // used verbatim — it already carries a port when there is one to carry.
    match &owned.host {
        Some(host) if !host.is_empty() => format!("//{host}{}/", owned.path),
        _ => {
            let port = if server_port == 80 {
                String::new()
            } else {
                format!(":{server_port}")
            };
            format!("//localhost{port}{}/", owned.path)
        }
    }
}

/// Serve one file: validators, ranges, precompressed variants.
async fn answer(
    request: &Request<'_>,
    files: &StaticFiles,
    path: PathBuf,
    encoding: Option<&'static str>,
    length: u64,
    modified: Option<SystemTime>,
) -> Result<StaticResponse, HttpError> {
    let mut headers = HeaderMap::new();

    // A `Last-Modified` that cannot be formatted would be worse than none: a
    // client would compare against a value it cannot parse.
    if let Some(modified) = modified {
        if let Ok(value) = httpdate::fmt_http_date(modified).parse() {
            headers.insert(LAST_MODIFIED, value);
        }
    }

    // The validator, from the same two facts a conditional request compares
    // against: the length and the modification time. Strong rather than weak,
    // because the bytes really are the ones the length describes.
    let tag = format!(
        "\"{:x}-{:x}\"",
        length,
        modified
            .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|since| since.as_secs())
            .unwrap_or(0)
    );
    headers.insert(ETAG, tag.parse().expect("a quoted etag"));
    headers.insert(
        CONTENT_TYPE,
        content_type(&path).parse().expect("a literal"),
    );
    headers.insert(ACCEPT_RANGES, "bytes".parse().expect("a literal"));
    headers.insert(
        CACHE_CONTROL,
        files
            .config
            .cache_control
            .parse()
            .expect("validated at startup"),
    );
    if let Some(encoding) = encoding {
        headers.insert(CONTENT_ENCODING, encoding.parse().expect("a literal"));
        // The same URL answers differently by encoding, so a cache must key on
        // it. Without this a proxy that saw the gzip answer could hand it to a
        // client that cannot read it.
        headers.insert(VARY, "accept-encoding".parse().expect("a literal"));
    }

    // Conditional before anything is sent: a client that already has the file
    // should not receive a byte of it, a range overlapping its copy included.
    let precondition = conditional(request, &headers, &tag, modified);
    if let Precondition::Answer(response) = precondition {
        return Ok(response);
    }

    if matches!(*request.method, Method::HEAD | Method::OPTIONS) {
        return head_response(request, &headers, length);
    }

    // A failed `If-Range` is not an error: it means "you have an old copy, take
    // the whole thing", so the range is ignored rather than refused.
    let range = match precondition {
        Precondition::IgnoreRange => None,
        _ => request
            .headers
            .get(RANGE)
            .and_then(|value| value.to_str().ok())
            .map(|value| parse_ranges(value, length)),
    };

    match range {
        None | Some(Ranges::Ignore) => stream(&path, 0, length, &headers, StatusCode::OK).await,
        Some(Ranges::Unsatisfiable) => Ok(build(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(CONTENT_RANGE, format!("bytes */{length}"))
            .body(body_of(Bytes::new()))
            .expect("headers are valid")),
        Some(Ranges::One(range)) => {
            let mut headers = headers.clone();
            headers.insert(
                CONTENT_RANGE,
                range.content_range(length).parse().expect("a literal"),
            );
            stream(
                &path,
                range.start,
                range.len(),
                &headers,
                StatusCode::PARTIAL_CONTENT,
            )
            .await
        }
        Some(Ranges::Many(ranges)) => {
            multi_range(
                &path,
                ranges,
                length,
                &headers,
                files.config.max_multirange_bytes,
            )
            .await
        }
    }
}

/// The headers-only answer, for the methods that take no body.
///
/// `HEAD` is suppressed by `hyper` while keeping the length it is told, and
/// `OPTIONS` describes the resource without sending it. Both are the headers
/// alone, and neither is a range request.
fn head_response(
    request: &Request<'_>,
    headers: &HeaderMap,
    length: u64,
) -> Result<StaticResponse, HttpError> {
    let mut builder = build(StatusCode::OK);
    for (name, value) in headers.iter() {
        builder = builder.header(name, value);
    }
    if request.method == Method::OPTIONS {
        builder = builder.header(ALLOW, "GET, HEAD, OPTIONS");
    }
    builder
        .header(CONTENT_LENGTH, length)
        .body(body_of(Bytes::new()))
        .map_err(|err| HttpError::BadGateway(err.to_string()))
}

/// What the precondition header fields decided.
enum Precondition {
    /// Send the representation, honouring any range.
    Proceed,
    /// Send this instead: a 304, or a 412 for a failed `If-Match`.
    Answer(StaticResponse),
    /// Ignore any `Range`, because a failed `If-Range` means the client is
    /// working from an older copy and should have the whole representation.
    IgnoreRange,
}

/// If the client already has this exact version, answer without the bytes.
fn conditional(
    request: &Request<'_>,
    headers: &HeaderMap,
    tag: &str,
    modified: Option<SystemTime>,
) -> Precondition {
    let header = |name| {
        request
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
    };

    // `If-Match` is a precondition rather than a cache check: it exists so a
    // client can be sure it is not acting on a version it has never seen. This
    // layer only reads, so a mismatch is a 412 rather than a 304.
    if let Some(value) = header(IF_MATCH) {
        if value.trim() != "*" && !etag_matches(value, tag) {
            return Precondition::Answer(precondition_failed(headers));
        }
    }

    // An entity tag, when present, *is* the validator — `If-Modified-Since` is
    // then ignored, because a stale date must not override a tag the client
    // actually sent.
    if let Some(value) = header(IF_NONE_MATCH) {
        return if etag_matches(value, tag) {
            Precondition::Answer(not_modified(headers))
        } else {
            Precondition::Proceed
        };
    }

    if let (Some(since), Some(modified)) = (header(IF_MODIFIED_SINCE), modified) {
        if let Ok(since) = httpdate::parse_http_date(since) {
            // Truncated to whole seconds, because the header carries no
            // sub-second precision. Comparing the raw `SystemTime` would report
            // the file as newer than the date the client was sent, and the client
            // would re-download an unchanged file for ever.
            if truncate_to_seconds(modified) <= since {
                return Precondition::Answer(not_modified(headers));
            }
        }
    }

    // `If-Range` decides whether a range may be honoured at all. The comparison
    // does **not** fall back from one kind of validator to the other: a client
    // that sent an entity tag did not send a date, so treating the file's own
    // `Last-Modified` as though the client had sent it would honour a range on a
    // copy the client may have had for weeks.
    if request.headers.contains_key(RANGE) {
        if let Some(value) = header(IF_RANGE) {
            let value = value.trim();
            let current = if looks_like_entity_tag(value) {
                etag_matches(value, tag)
            } else {
                httpdate::parse_http_date(value).ok().is_some_and(|sent| {
                    modified.is_some_and(|modified| truncate_to_seconds(modified) == sent)
                })
            };
            if !current {
                return Precondition::IgnoreRange;
            }
        }
    }

    Precondition::Proceed
}

/// Whether an `If-Range` value is an entity tag rather than a date.
///
/// An entity tag is quoted, optionally with the `W/` weakness prefix; a date is
/// not. There is no third form, so anything unquoted is read as a date.
fn looks_like_entity_tag(value: &str) -> bool {
    value.strip_prefix("W/").unwrap_or(value).starts_with('"')
}
fn truncate_to_seconds(time: SystemTime) -> SystemTime {
    let seconds = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds)
}

/// Whether an `If-Match`/`If-None-Match`/`If-Range` value names this entity tag.
///
/// Strong comparison only, and only a tag the client sent verbatim: weak
/// comparison would accept `W/"…"` for a strong tag, which is the distinction the
/// `W/` prefix exists to make.
fn etag_matches(value: &str, tag: &str) -> bool {
    value.split(',').any(|candidate| candidate.trim() == tag)
}

fn not_modified(headers: &HeaderMap) -> StaticResponse {
    let mut response = build(StatusCode::NOT_MODIFIED);
    for (name, value) in headers.iter() {
        // A 304 must not carry an encoding-specific `Vary` it does not depend
        // on... it does, so everything but the body headers is kept.
        response = response.header(name, value);
    }
    // Deliberately no `Content-Length`: a 304 has no body, and a length would
    // have to be zero to be truthful — which is how a client ends up believing
    // the resource is empty.
    response
        .body(body_of(Bytes::new()))
        .expect("headers are valid")
}

fn precondition_failed(headers: &HeaderMap) -> StaticResponse {
    let mut response = build(StatusCode::PRECONDITION_FAILED);
    if let Some(value) = headers.get(ETAG) {
        response = response.header(ETAG, value);
    }
    response
        .body(body_of(Bytes::new()))
        .expect("headers are valid")
}

/// A status with no headers and no body.
fn status_only(status: StatusCode) -> StaticResponse {
    build(status)
        .body(body_of(Bytes::new()))
        .expect("a status alone is a valid response")
}

fn build(status: StatusCode) -> hyper::http::response::Builder {
    Response::builder().status(status)
}

fn header(headers: &HeaderMap, name: hyper::header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// The `.br`/`.gz` variant to serve instead of the file, when there is one.
///
/// Only an *existing* variant is chosen, so a missing `file.gz` falls back to the
/// original rather than a 404 — the file the client asked for does exist.
fn choose_encoding(
    path: &Path,
    length: u64,
    wanted: &Wanted,
    files: &StaticFiles,
) -> (PathBuf, Option<&'static str>, u64) {
    if !files.config.precompressed {
        return (path.to_path_buf(), None, length);
    }
    let Some(accept) = wanted.accept_encoding.as_deref() else {
        return (path.to_path_buf(), None, length);
    };

    // Brotli first: it is smaller than gzip for text, so when the client accepts
    // both and both exist, it is the better answer.
    for (suffix, encoding) in [(".br", "br"), (".gz", "gzip")] {
        if !accepts_encoding(accept, encoding) {
            continue;
        }
        let candidate = PathBuf::from(format!("{}{suffix}", path.display()));
        if let Ok(metadata) = std::fs::metadata(&candidate) {
            if metadata.is_file() {
                return (candidate, Some(encoding), metadata.len());
            }
        }
    }

    (path.to_path_buf(), None, length)
}

/// Whether an `Accept-Encoding` value offers this encoding with a non-zero
/// quality.
fn accepts_encoding(accept: &str, encoding: &str) -> bool {
    for part in accept.split(',') {
        let mut fields = part.split(';');
        let name = fields.next().unwrap_or_default().trim();
        if !name.eq_ignore_ascii_case(encoding) {
            continue;
        }
        let quality = fields
            .find_map(|field| {
                field
                    .trim()
                    .strip_prefix("q=")
                    .and_then(|value| value.parse::<f32>().ok())
            })
            .unwrap_or(1.0);
        return quality > 0.0;
    }
    false
}

/// One byte range, with an inclusive end like the header it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ByteRange {
    start: u64,
    end: u64,
}

impl ByteRange {
    fn len(&self) -> u64 {
        self.end - self.start + 1
    }

    fn content_range(&self, total: u64) -> String {
        format!("bytes {}-{}/{}", self.start, self.end, total)
    }
}

/// What a `Range` header asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ranges {
    One(ByteRange),
    Many(Vec<ByteRange>),
    /// A range that names no byte at all, which is a 416.
    Unsatisfiable,
    /// Nothing usable was asked for, so the whole representation is sent.
    ///
    /// RFC 9110 is explicit that a server which cannot honour a range sends the
    /// whole thing: a missing feature is not a client error.
    Ignore,
}

/// Parse `Range: bytes=…`.
fn parse_ranges(value: &str, length: u64) -> Ranges {
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return Ranges::Ignore;
    };

    let mut ranges = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let Some((start, end)) = part.split_once('-') else {
            return Ranges::Ignore;
        };

        let range = match (start.trim(), end.trim()) {
            // `bytes=-500`: the last 500 bytes, which is what a player seeking to
            // the end of a media file asks for.
            ("", suffix) => match suffix.parse::<u64>() {
                // `bytes=-0` names no byte, which is unsatisfiable rather than
                // malformed: the client asked for a suffix of nothing.
                Ok(0) => return Ranges::Unsatisfiable,
                Ok(suffix) => ByteRange {
                    start: length.saturating_sub(suffix.min(length)),
                    end: length.saturating_sub(1),
                },
                Err(_) => return Ranges::Ignore,
            },
            (start, "") => match start.parse::<u64>() {
                Ok(start) if start < length => ByteRange {
                    start,
                    end: length - 1,
                },
                Ok(_) => return Ranges::Unsatisfiable,
                Err(_) => return Ranges::Ignore,
            },
            (start, end) => match (start.parse::<u64>(), end.parse::<u64>()) {
                (Ok(start), Ok(end)) if start <= end && start < length => ByteRange {
                    start,
                    end: end.min(length - 1),
                },
                // `bytes=100-50` is malformed rather than unsatisfiable, and a
                // malformed range is ignored.
                (Ok(start), Ok(end)) if start > end => return Ranges::Ignore,
                (Ok(_), Ok(_)) => return Ranges::Unsatisfiable,
                _ => return Ranges::Ignore,
            },
        };
        ranges.push(range);
    }

    match ranges.len() {
        0 => Ranges::Ignore,
        1 => Ranges::One(ranges[0]),
        _ => Ranges::Many(ranges),
    }
}

/// Stream a slice of a file as the response body.
async fn stream(
    path: &Path,
    offset: u64,
    length: u64,
    headers: &HeaderMap,
    status: StatusCode,
) -> Result<StaticResponse, HttpError> {
    let mut builder = build(status);
    for (name, value) in headers.iter() {
        builder = builder.header(name, value);
    }
    builder = builder.header(CONTENT_LENGTH, length);

    builder
        .body(stream_body(path, offset, length).await?)
        .map_err(|err| HttpError::BadGateway(err.to_string()))
}

/// The body for a slice of a file.
///
/// `File::from_std` is why this is not behind `spawn_blocking`: opening with
/// `std::fs` is quick, and the handle is handed to a `tokio` file so the reads
/// themselves run on the blocking pool rather than on a runtime thread.
async fn stream_body(path: &Path, offset: u64, length: u64) -> Result<ResponseBody, HttpError> {
    use tokio::io::AsyncSeekExt;

    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        // Vanished between the `stat` and the `open`. The headers already say
        // 200, so the honest answer is an empty body rather than a truncated one;
        // a client sees a short read and retries.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(body_of(Bytes::new())),
        Err(err) => {
            return Err(HttpError::BadGateway(format!(
                "cannot open {}: {err}",
                path.display()
            )))
        }
    };

    let mut file = tokio::fs::File::from_std(file);
    if offset > 0 {
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|err| HttpError::BadGateway(err.to_string()))?;
    }

    Ok(BoxBody::new(FileBody {
        file,
        remaining: length,
    }))
}

/// A file streamed in fixed-size blocks, never buffered whole.
///
/// Hand-written rather than assembled from `http_body_util`'s adapters, because
/// the natural adapters (`StreamBody` over `ReaderStream`) need a `Stream`, and
/// the only way to get one without a new dependency is `futures` — a large tree
/// for a body that is forty lines. `poll_read_buf` does the one thing that is
/// easy to get wrong: filling a `BytesMut` without copying through a `Vec`.
struct FileBody {
    file: tokio::fs::File,
    /// How many bytes are still owed. `take` at the source would do the same,
    /// but keeping the count here means the size hint is exact.
    remaining: u64,
}

impl Body for FileBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<std::result::Result<hyper::body::Frame<Bytes>, std::io::Error>>>
    {
        let this = self.get_mut();
        if this.remaining == 0 {
            return std::task::Poll::Ready(None);
        }

        // Sized to what is left, so the last block of a large file does not
        // allocate 64 KiB to send three bytes.
        let want = (STREAM_CHUNK as u64).min(this.remaining) as usize;
        let mut buffer = bytes::BytesMut::with_capacity(want);

        match tokio_util::io::poll_read_buf(std::pin::Pin::new(&mut this.file), cx, &mut buffer) {
            std::task::Poll::Ready(Ok(0)) => {
                // A short file: the metadata said one thing and the read says
                // another, which is a deployment in progress. Stopping is the
                // only honest option — the framing already promised `length`.
                this.remaining = 0;
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Ready(Ok(read)) => {
                this.remaining -= read as u64;
                std::task::Poll::Ready(Some(Ok(hyper::body::Frame::data(
                    buffer.split_to(read).freeze(),
                ))))
            }
            std::task::Poll::Ready(Err(err)) => {
                // Reported once, and not retried: a client that got a 200 with a
                // truncated body learns the truth from the connection closing.
                this.remaining = 0;
                std::task::Poll::Ready(Some(Err(err)))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        // Exact, which is what lets `hyper` frame the response with a
        // `Content-Length` rather than chunked encoding.
        hyper::body::SizeHint::with_exact(self.remaining)
    }
}

/// Assemble a multi-range answer.
///
/// The parts are buffered, which is why the configuration has a size limit for
/// them: each part carries its own header, and a client is free to ask for
/// hundreds of tiny slices of a very large file. The limit is checked before a
/// byte is read.
async fn multi_range(
    path: &Path,
    ranges: Vec<ByteRange>,
    total: u64,
    headers: &HeaderMap,
    limit: u64,
) -> Result<StaticResponse, HttpError> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    // The framing as well as the payload: a boundary line, a header and two
    // CRLFs per part, plus the closing boundary.
    let framing = ranges.len() as u64 * 256 + 64;
    let payload: u64 = ranges.iter().map(ByteRange::len).sum();
    if framing + payload > limit {
        // Refused rather than answered: the client can ask for the parts
        // individually, which streams rather than buffering.
        return Err(HttpError::BadRequest(format!(
            "a multi-range request for {payload} bytes exceeds the {limit} byte limit; \
             request the ranges one at a time"
        )));
    }

    let mut file =
        tokio::fs::File::from_std(std::fs::File::open(path).map_err(|err| {
            HttpError::BadGateway(format!("cannot open {}: {err}", path.display()))
        })?);

    let mut out: Vec<u8> = Vec::with_capacity((framing + payload) as usize);
    let mut buffer = vec![0u8; STREAM_CHUNK];
    for range in &ranges {
        out.extend_from_slice(format!("--{MULTIPART_BOUNDARY}\r\n").as_bytes());
        out.extend_from_slice(format!("Content-Type: {}\r\n", content_type(path)).as_bytes());
        out.extend_from_slice(
            format!("Content-Range: {}\r\n\r\n", range.content_range(total)).as_bytes(),
        );

        file.seek(std::io::SeekFrom::Start(range.start))
            .await
            .map_err(|err| HttpError::BadGateway(err.to_string()))?;
        let mut remaining = range.len();
        while remaining > 0 {
            let want = (buffer.len() as u64).min(remaining) as usize;
            let read = file
                .read(&mut buffer[..want])
                .await
                .map_err(|err| HttpError::BadGateway(err.to_string()))?;
            if read == 0 {
                break;
            }
            out.extend_from_slice(&buffer[..read]);
            remaining -= read as u64;
        }
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{MULTIPART_BOUNDARY}--\r\n").as_bytes());

    let mut builder = build(StatusCode::PARTIAL_CONTENT).header(
        CONTENT_TYPE,
        format!("multipart/byteranges; boundary={MULTIPART_BOUNDARY}"),
    );
    for (name, value) in headers.iter() {
        // The single-part headers describe one range, which is wrong for a
        // multipart answer, so only the validators and the caching are carried.
        if name == CONTENT_TYPE || name == CONTENT_RANGE {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder = builder.header(CONTENT_LENGTH, out.len());

    builder
        .body(body_of(Bytes::from(out)))
        .map_err(|err| HttpError::BadGateway(err.to_string()))
}

/// The content type for a file, by extension.
///
/// A short table of what a web build actually contains, rather than a dependency
/// with a thousand entries. Anything not listed is `application/octet-stream`,
/// which is honest: the client asked the server, and the server does not know.
fn content_type(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    match extension.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "xml" => "application/xml",
        "txt" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "br" => "application/brotli",
        _ => "application/octet-stream",
    }
}

/// A `Host` header split into name and port.
pub fn split_host(host: &str, fallback: u16) -> (String, u16) {
    match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() => match port.parse() {
            Ok(port) => (name.to_string(), port),
            Err(_) => (host.to_string(), fallback),
        },
        _ => (host.to_string(), fallback),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use std::fs;

    fn fixture(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "stokehold-static-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create the fixture root");
        for (relative, contents) in files {
            let path = root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create a fixture directory");
            }
            fs::write(&path, contents).expect("write a fixture file");
        }
        root
    }

    fn layer(root: &Path, enabled: bool) -> StaticFiles {
        StaticFiles {
            config: StaticConfig {
                enabled,
                ..StaticConfig::default()
            },
            document_root: root.to_path_buf(),
            server_port: 8080,
        }
    }

    fn no_headers() -> HeaderMap {
        HeaderMap::new()
    }

    /// Run an async test body on a runtime built here.
    ///
    /// `#[tokio::test]` is not available — the `macros` feature does not resolve
    /// on this machine — and building the runtime explicitly keeps the async
    /// boundary visible in the tests as well as in the production code.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a tokio runtime")
            .block_on(future)
    }

    /// A request as the blocking half sees it, with no headers set.
    fn owned(path: &str, headers: &HeaderMap) -> Owned {
        Owned {
            path: path.to_string(),
            host: Some("example.test".to_string()),
            wanted: Wanted {
                accept_encoding: header(headers, ACCEPT_ENCODING).map(str::to_string),
            },
        }
    }

    fn request<'a>(method: &'a Method, path: &'a str, headers: &'a HeaderMap) -> Request<'a> {
        Request {
            method,
            path,
            headers,
            host: Some("example.test"),
        }
    }

    fn name_of(plan: &Plan) -> &'static str {
        match plan {
            Plan::NotAFile => "not a file",
            Plan::NotFound => "not found",
            Plan::Forbidden => "forbidden",
            Plan::BadRequest(_) => "bad request",
            Plan::Redirect(_) => "redirect",
            Plan::File { .. } => "a file",
        }
    }

    #[test]
    fn static_serving_is_off_unless_it_is_configured() {
        assert!(!StaticConfig::default().enabled);
    }

    #[test]
    fn a_dotfile_is_denied() {
        let root = Path::new("/srv/app");
        assert!(matches!(
            resolve_under_root(root, "/.env"),
            Resolution::Dotfile
        ));
        assert!(matches!(
            resolve_under_root(root, "/.git/config"),
            Resolution::Dotfile
        ));
        assert!(matches!(
            resolve_under_root(root, "/assets/app.js"),
            Resolution::Path(_)
        ));
        // A leading dot alone is the whole rule, so `.well-known` is denied too.
        // An operator who publishes there turns dotfiles on, which is the only
        // way the rule stays predictable from the configuration file.
        assert!(matches!(
            resolve_under_root(root, "/.well-known/acme-challenge/token"),
            Resolution::Dotfile
        ));
    }

    #[test]
    fn a_parent_traversal_cannot_leave_the_root() {
        let root = Path::new("/srv/app");
        for path in ["/../etc/passwd", "/assets/../../etc/passwd"] {
            assert!(
                matches!(resolve_under_root(root, path), Resolution::Outside),
                "{path} must be refused"
            );
        }
        // The percent-encoded form is deliberately *not* decoded. It cannot
        // escape the root, so the only question is which refusal it earns: the
        // directory name starts with a dot, so the dotfile rule catches it.
        match resolve_under_root(root, "/..%2f..%2fetc/passwd") {
            Resolution::Dotfile => {}
            Resolution::Path(found) => assert!(found.starts_with(root), "{}", found.display()),
            Resolution::Outside => panic!("an encoded escape must not be treated as traversal"),
        }
    }

    #[test]
    fn a_suffix_range_takes_the_end_of_the_file() {
        assert_eq!(
            parse_ranges("bytes=-500", 1000),
            Ranges::One(ByteRange {
                start: 500,
                end: 999
            })
        );
        // A suffix longer than the file is the whole file, not an error.
        assert_eq!(
            parse_ranges("bytes=-5000", 1000),
            Ranges::One(ByteRange { start: 0, end: 999 })
        );
    }

    #[test]
    fn an_open_ended_range_reaches_the_last_byte() {
        assert_eq!(
            parse_ranges("bytes=100-", 1000),
            Ranges::One(ByteRange {
                start: 100,
                end: 999
            })
        );
        assert_eq!(
            parse_ranges("bytes=0-", 1000),
            Ranges::One(ByteRange { start: 0, end: 999 })
        );
    }

    #[test]
    fn an_end_past_the_file_is_clamped_not_refused() {
        assert_eq!(
            parse_ranges("bytes=0-9999999", 1000),
            Ranges::One(ByteRange { start: 0, end: 999 })
        );
    }

    #[test]
    fn a_range_that_names_no_byte_is_unsatisfiable() {
        assert_eq!(parse_ranges("bytes=1000-", 1000), Ranges::Unsatisfiable);
        assert_eq!(parse_ranges("bytes=-0", 1000), Ranges::Unsatisfiable);
        assert_eq!(parse_ranges("bytes=5000-6000", 1000), Ranges::Unsatisfiable);
    }

    #[test]
    fn a_malformed_range_is_ignored_so_the_whole_file_is_sent() {
        assert_eq!(parse_ranges("items=0-10", 1000), Ranges::Ignore);
        assert_eq!(parse_ranges("bytes=100-50", 1000), Ranges::Ignore);
        assert_eq!(parse_ranges("", 1000), Ranges::Ignore);
        assert_eq!(parse_ranges("bytes=abc", 1000), Ranges::Ignore);
        assert_eq!(parse_ranges("bytes=abc-def", 1000), Ranges::Ignore);
    }

    #[test]
    fn several_ranges_are_reported_in_order() {
        assert_eq!(
            parse_ranges("bytes=0-99, 500-599", 1000),
            Ranges::Many(vec![
                ByteRange { start: 0, end: 99 },
                ByteRange {
                    start: 500,
                    end: 599
                },
            ])
        );
    }

    #[test]
    fn a_content_range_says_what_was_sent_out_of_what_exists() {
        let range = ByteRange {
            start: 100,
            end: 199,
        };
        assert_eq!(range.len(), 100);
        assert_eq!(range.content_range(1000), "bytes 100-199/1000");
    }

    #[test]
    fn a_multi_range_answer_is_buffered_so_it_is_capped() {
        block_on(async {
            let root = fixture("multilimit", &[("app.txt", &"x".repeat(4096))]);
            let files = StaticFiles {
                config: StaticConfig {
                    enabled: true,
                    // Below the two parts below, so the refusal is observable
                    // without a large file.
                    max_multirange_bytes: 1024,
                    ..StaticConfig::default()
                },
                document_root: root.clone(),
                server_port: 8080,
            };

            let mut headers = HeaderMap::new();
            headers.insert(RANGE, "bytes=0-2047,2048-4095".parse().unwrap());
            let answer = respond(request(&Method::GET, "/app.txt", &headers), &files)
                .await
                .expect("static serving is on");
            let err = answer.expect_err("2048 bytes of parts exceed a 1024 byte cap");
            assert_eq!(err.status(), 400, "{err}");

            // Under the cap, the same request is answered.
            let mut headers = HeaderMap::new();
            headers.insert(RANGE, "bytes=0-99,200-299".parse().unwrap());
            let answer = respond(request(&Method::GET, "/app.txt", &headers), &files)
                .await
                .unwrap()
                .expect("two small parts are under the cap");
            assert_eq!(answer.status(), StatusCode::PARTIAL_CONTENT);
            assert!(
                answer
                    .headers()
                    .get(CONTENT_TYPE)
                    .is_some_and(|value| value.to_str().unwrap().starts_with("multipart/")),
                "a multi-range answer declares its own type"
            );

            let _ = fs::remove_dir_all(&root);
        });
    }

    #[test]
    fn a_tag_is_matched_strongly() {
        assert!(etag_matches("\"abc\"", "\"abc\""));
        assert!(etag_matches("\"abc\", \"def\"", "\"def\""));
        // A weak tag is a different validator, not an equivalent one.
        assert!(!etag_matches("W/\"abc\"", "\"abc\""));
        assert!(!etag_matches("\"abc\"", "\"def\""));
    }

    #[test]
    fn an_if_range_value_is_read_as_the_kind_of_validator_it_is() {
        // A quoted value is an entity tag; a date is not. Without this
        // distinction a mismatched tag would be compared as a date and then
        // silently honoured against the file's own Last-Modified.
        assert!(looks_like_entity_tag("\"abc\""));
        assert!(looks_like_entity_tag("W/\"abc\""));
        assert!(!looks_like_entity_tag("Wed, 21 Oct 2015 07:28:00 GMT"));
        assert!(!looks_like_entity_tag("*"));
    }

    #[test]
    fn a_stale_entity_tag_makes_a_range_be_ignored_rather_than_honoured() {
        let mut headers = HeaderMap::new();
        headers.insert(IF_RANGE, "\"stale\"".parse().unwrap());
        headers.insert(RANGE, "bytes=2-5".parse().unwrap());

        match conditional(
            &request(&Method::GET, "/a.txt", &headers),
            &HeaderMap::new(),
            "\"current\"",
            None,
        ) {
            Precondition::IgnoreRange => {}
            other => panic!(
                "a mismatched tag must ignore the range: {}",
                name_of_precondition(&other)
            ),
        }

        // The matching tag honours it.
        let mut headers = HeaderMap::new();
        headers.insert(IF_RANGE, "\"current\"".parse().unwrap());
        headers.insert(RANGE, "bytes=2-5".parse().unwrap());
        assert!(matches!(
            conditional(
                &request(&Method::GET, "/a.txt", &headers),
                &HeaderMap::new(),
                "\"current\"",
                None
            ),
            Precondition::Proceed
        ));
    }

    #[test]
    fn a_matching_if_none_match_is_a_304_and_a_mismatched_one_is_not() {
        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, "\"current\"".parse().unwrap());
        match conditional(
            &request(&Method::GET, "/a.txt", &headers),
            &HeaderMap::new(),
            "\"current\"",
            None,
        ) {
            Precondition::Answer(response) => {
                assert_eq!(response.status(), StatusCode::NOT_MODIFIED)
            }
            other => panic!("expected a 304, got {}", name_of_precondition(&other)),
        }

        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, "\"other\"".parse().unwrap());
        assert!(matches!(
            conditional(
                &request(&Method::GET, "/a.txt", &headers),
                &HeaderMap::new(),
                "\"current\"",
                None
            ),
            Precondition::Proceed
        ));
    }

    #[test]
    fn a_failed_if_match_is_a_412_because_the_representations_disagree() {
        let mut headers = HeaderMap::new();
        headers.insert(IF_MATCH, "\"other\"".parse().unwrap());
        match conditional(
            &request(&Method::GET, "/a.txt", &headers),
            &HeaderMap::new(),
            "\"current\"",
            None,
        ) {
            Precondition::Answer(response) => {
                assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED)
            }
            other => panic!("expected a 412, got {}", name_of_precondition(&other)),
        }
    }

    fn name_of_precondition(precondition: &Precondition) -> &'static str {
        match precondition {
            Precondition::Proceed => "proceed",
            Precondition::Answer(_) => "an answer",
            Precondition::IgnoreRange => "ignore the range",
        }
    }

    #[test]
    fn an_encoding_is_accepted_only_with_a_non_zero_quality() {
        assert!(accepts_encoding("gzip", "gzip"));
        assert!(accepts_encoding("br, gzip", "gzip"));
        assert!(accepts_encoding("gzip;q=0.5", "gzip"));
        assert!(!accepts_encoding("gzip;q=0", "gzip"));
        assert!(!accepts_encoding("br", "gzip"));
        assert!(!accepts_encoding("identity", "gzip"));
        // A wildcard does not name the encoding, so it is not accepted: serving
        // a precompressed variant requires the client to have asked for it.
        assert!(!accepts_encoding("*", "gzip"));
    }

    #[test]
    fn content_types_cover_what_a_build_produces() {
        assert_eq!(
            content_type(Path::new("a/b/app.js")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("style.CSS")),
            "text/css; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("index.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(content_type(Path::new("font.woff2")), "font/woff2");
        assert_eq!(content_type(Path::new("app.wasm")), "application/wasm");
        // Honest rather than guessed.
        assert_eq!(
            content_type(Path::new("archive.7z")),
            "application/octet-stream"
        );
        assert_eq!(
            content_type(Path::new("LICENSE")),
            "application/octet-stream"
        );
    }

    #[test]
    fn a_host_header_can_be_split_without_assuming_a_port() {
        assert_eq!(
            split_host("example.test", 8080),
            ("example.test".into(), 8080)
        );
        assert_eq!(
            split_host("example.test:443", 8080),
            ("example.test".into(), 443)
        );
        assert_eq!(
            split_host("example.test:", 8080),
            ("example.test:".into(), 8080)
        );
    }

    #[test]
    fn a_directory_url_redirects_to_its_trailing_slash() {
        let headers = no_headers();
        let subject = owned("/docs", &headers);
        assert_eq!(directory_location(&subject, 8080), "//example.test/docs/");

        // Without a `Host`, the listener's own port is the honest answer.
        let mut bare = owned("/docs", &headers);
        bare.host = None;
        assert_eq!(directory_location(&bare, 8080), "//localhost:8080/docs/");
        assert_eq!(directory_location(&bare, 80), "//localhost/docs/");
    }

    #[test]
    fn only_a_path_that_names_a_file_is_served_here() {
        let root = fixture(
            "plan",
            &[("index.html", "home"), ("assets/logo.svg", "<svg/>")],
        );
        let files = layer(&root, true);
        let headers = no_headers();

        // Not a file: PHP's business, which is how a front controller survives.
        for path in ["/about", "/api/users"] {
            let outcome = plan(&owned(path, &headers), &files);
            assert_eq!(name_of(&outcome), "not a file", "{path}");
        }

        // A real file: this layer's.
        let outcome = plan(&owned("/assets/logo.svg", &headers), &files);
        assert_eq!(name_of(&outcome), "a file");

        // A directory with an index is served through the index.
        match plan(&owned("/assets/", &headers), &files) {
            Plan::NotAFile => {}
            Plan::File { path, .. } => assert!(path.ends_with("index.html"), "{}", path.display()),
            other => panic!("unexpected: {}", name_of(&other)),
        }

        // A directory with no index is not a file, so PHP gets it.
        fs::create_dir_all(root.join("empty")).expect("create the directory");
        let outcome = plan(&owned("/empty/", &headers), &files);
        assert_eq!(name_of(&outcome), "not a file");

        // And the refusals.
        let outcome = plan(&owned("/../secret", &headers), &files);
        assert_eq!(name_of(&outcome), "forbidden");
        let outcome = plan(&owned("/.env", &headers), &files);
        assert_eq!(name_of(&outcome), "not found");

        let _ = fs::remove_dir_all(&root);
    }

    /// A file PHP is meant to execute must never come from this layer.
    ///
    /// This is a source-disclosure hole when it is wrong, and a silent one: PHP
    /// source is a perfectly valid response body, so the only symptom is that
    /// the client received the program instead of the program's output. The file
    /// is written here on purpose — the earlier version of the test above only
    /// asserted `not a file` for `/index.php` in a fixture that never *had* an
    /// `index.php`, so it passed for the wrong reason and hid exactly this.
    #[test]
    fn a_php_script_is_never_served_as_a_file() {
        let root = fixture(
            "phpguard",
            &[
                ("index.php", "<?php echo 'secret';"),
                ("app.php", "<?php echo 'secret';"),
                ("legacy.phtml", "<?php echo 'secret';"),
                ("old.php3", "<?php echo 'secret';"),
                ("public/style.css", "body{}"),
            ],
        );
        let files = layer(&root, true);
        let headers = no_headers();

        for path in ["/index.php", "/app.php", "/legacy.phtml", "/old.php3"] {
            let outcome = plan(&owned(path, &headers), &files);
            assert_eq!(name_of(&outcome), "not a file", "{path} must go to PHP");
        }

        // Extension matching is case-insensitive, like the routing layer's.
        assert_eq!(
            name_of(&plan(&owned("/INDEX.PHP", &headers), &files)),
            "not a file"
        );

        // A plain file with a misleading name is still a file.
        assert_eq!(
            name_of(&plan(&owned("/public/style.css", &headers), &files)),
            "a file"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_directory_without_a_slash_redirects_rather_than_serving() {
        let root = fixture("dir", &[("docs/index.html", "docs")]);
        let files = layer(&root, true);
        let headers = no_headers();

        match plan(&owned("/docs", &headers), &files) {
            Plan::Redirect(location) => assert_eq!(location, "//example.test/docs/"),
            other => panic!("expected a redirect, got {}", name_of(&other)),
        }

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_directory_route_goes_to_php_when_directory_indexes_are_off() {
        let root = fixture("noindex", &[("docs/index.html", "docs")]);
        let files = StaticFiles {
            config: StaticConfig {
                enabled: true,
                directory_index: false,
                ..StaticConfig::default()
            },
            document_root: root.clone(),
            server_port: 8080,
        };
        let headers = no_headers();

        // Without the feature the path is not a file at all, so there is nothing
        // to redirect to and nothing to serve.
        let outcome = plan(&owned("/docs", &headers), &files);
        assert_eq!(name_of(&outcome), "not a file");
        let outcome = plan(&owned("/docs/", &headers), &files);
        assert_eq!(name_of(&outcome), "not a file");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_head_request_is_answered_without_a_body() {
        block_on(async {
            let root = fixture("head", &[("app.css", "body { }")]);
            let files = layer(&root, true);
            let headers = no_headers();
            let method = Method::HEAD;

            let response = respond(request(&method, "/app.css", &headers), &files)
                .await
                .expect("static serving is on")
                .expect("a head of a file is not an error");
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers().get(CONTENT_LENGTH).unwrap(),
                "8",
                "a HEAD keeps the length it would have sent"
            );
            assert!(
                response.headers().contains_key(ETAG),
                "a HEAD still describes the resource"
            );
            assert_eq!(
                response.body().size_hint().exact(),
                Some(0),
                "and has no body"
            );

            let _ = fs::remove_dir_all(&root);
        });
    }

    #[test]
    fn a_conditional_request_gets_a_304_without_the_bytes() {
        block_on(async {
            let root = fixture("cond", &[("app.css", "body { }")]);
            let files = layer(&root, true);

            let mut headers = HeaderMap::new();
            let first = respond(request(&Method::GET, "/app.css", &headers), &files)
                .await
                .unwrap()
                .unwrap();
            let tag = first
                .headers()
                .get(ETAG)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();

            headers.insert(IF_NONE_MATCH, tag.parse().unwrap());
            let second = respond(request(&Method::GET, "/app.css", &headers), &files)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
            assert_eq!(second.body().size_hint().exact(), Some(0));
            assert!(
                !second.headers().contains_key(CONTENT_LENGTH),
                "a 304 must not claim a length"
            );

            let _ = fs::remove_dir_all(&root);
        });
    }

    #[test]
    fn a_range_request_gets_a_206_with_exactly_those_bytes() {
        block_on(async {
            let root = fixture("range", &[("app.txt", "0123456789")]);
            let files = layer(&root, true);

            let mut headers = HeaderMap::new();
            headers.insert(RANGE, "bytes=2-5".parse().unwrap());
            let response = respond(request(&Method::GET, "/app.txt", &headers), &files)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(
                response.headers().get(CONTENT_RANGE).unwrap(),
                "bytes 2-5/10"
            );
            assert_eq!(response.headers().get(CONTENT_LENGTH).unwrap(), "4");

            let body = response
                .into_body()
                .collect()
                .await
                .expect("the body reads")
                .to_bytes();
            assert_eq!(&body[..], b"2345");

            let _ = fs::remove_dir_all(&root);
        });
    }

    #[test]
    fn a_range_beyond_the_file_is_a_416_that_says_how_long_it_is() {
        block_on(async {
            let root = fixture("unsat", &[("app.txt", "0123456789")]);
            let files = layer(&root, true);

            let mut headers = HeaderMap::new();
            headers.insert(RANGE, "bytes=100-200".parse().unwrap());
            let response = respond(request(&Method::GET, "/app.txt", &headers), &files)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
            assert_eq!(
                response.headers().get(CONTENT_RANGE).unwrap(),
                "bytes */10",
                "a 416 tells the client what would have worked"
            );

            let _ = fs::remove_dir_all(&root);
        });
    }

    #[test]
    fn a_precompressed_variant_is_chosen_only_when_it_exists_and_is_accepted() {
        block_on(async {
            let root = fixture(
                "encoding",
                &[
                    ("app.js", "plain"),
                    ("app.js.gz", "gzipped"),
                    ("app.js.br", "brotli"),
                ],
            );
            let files = StaticFiles {
                config: StaticConfig {
                    enabled: true,
                    precompressed: true,
                    ..StaticConfig::default()
                },
                document_root: root.clone(),
                server_port: 8080,
            };
            let plain = layer(&root, true);

            async fn pick(files: &StaticFiles, accept: &str) -> (String, Option<String>, usize) {
                let mut headers = HeaderMap::new();
                headers.insert(ACCEPT_ENCODING, accept.parse().unwrap());
                let response = respond(request(&Method::GET, "/app.js", &headers), files)
                    .await
                    .expect("static serving is on")
                    .expect("the file exists");
                let encoding = response
                    .headers()
                    .get(CONTENT_ENCODING)
                    .map(|value| value.to_str().unwrap().to_string());
                let body = response
                    .into_body()
                    .collect()
                    .await
                    .expect("the body reads")
                    .to_bytes();
                (
                    String::from_utf8_lossy(&body).into_owned(),
                    encoding,
                    body.len(),
                )
            }

            // Brotli wins when both are on offer, because it is smaller.
            let (body, encoding, _) = pick(&files, "gzip, br").await;
            assert_eq!(body, "brotli");
            assert_eq!(encoding.as_deref(), Some("br"));

            let (body, encoding, _) = pick(&files, "gzip").await;
            assert_eq!(body, "gzipped");
            assert_eq!(encoding.as_deref(), Some("gzip"));

            // A client that wants none of them gets the original.
            let (body, encoding, _) = pick(&files, "identity").await;
            assert_eq!(body, "plain");
            assert_eq!(encoding, None);

            // And with the feature off, always the original.
            let (body, encoding, _) = pick(&plain, "gzip, br").await;
            assert_eq!(body, "plain");
            assert_eq!(encoding, None);

            let _ = fs::remove_dir_all(&root);
        });
    }

    #[test]
    fn a_variant_that_is_not_there_falls_back_rather_than_404ing() {
        block_on(async {
            let root = fixture("novariant", &[("app.js", "plain")]);
            let files = StaticFiles {
                config: StaticConfig {
                    enabled: true,
                    precompressed: true,
                    ..StaticConfig::default()
                },
                document_root: root.clone(),
                server_port: 8080,
            };

            let mut headers = HeaderMap::new();
            headers.insert(ACCEPT_ENCODING, "gzip, br".parse().unwrap());
            let response = respond(request(&Method::GET, "/app.js", &headers), &files)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(!response.headers().contains_key(CONTENT_ENCODING));

            let _ = fs::remove_dir_all(&root);
        });
    }
}
