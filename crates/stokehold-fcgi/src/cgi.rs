//! The CGI/1.1 environment, and the routing that decides `SCRIPT_FILENAME`.
//!
//! This is the piece that decides whether PHP sees the request correctly. Two
//! things here are not optional and are easy to get subtly wrong:
//!
//! - **`CONTENT_LENGTH` must always be sent** when there is a body. Verified
//!   against php-cgi 8.3.31: without it PHP leaves `php://input` empty and never
//!   runs its request-body parser, so the raw bytes framed as `STDIN` are
//!   silently dropped while the response still looks successful.
//! - **`SCRIPT_FILENAME` and `PATH_INFO` must be split the way a front
//!   controller expects.** nginx does this with
//!   `try_files $uri /index.php$is_args$args`; getting it wrong means either a
//!   404 from PHP ("No input file specified") or a route that never matches.

use std::path::{Path, PathBuf};

use crate::error::{FcgiError, Result};
use crate::nvpair::NameValuePairs;

/// Routing configuration: where the document root is, and which script serves
/// URLs that do not map to a file.
#[derive(Debug, Clone)]
pub struct Routing {
    /// The document root, as PHP should see it.
    pub document_root: PathBuf,
    /// The front controller, relative to the document root, e.g. `index.php`.
    ///
    /// `None` means "serve only actual files", the classic
    /// `location ~ \.php$` behaviour.
    pub front_controller: Option<String>,
    /// Paths to reject outright, mirroring nginx's `forbid`.
    pub forbidden_extensions: Vec<String>,
}

impl Default for Routing {
    fn default() -> Self {
        Routing {
            document_root: PathBuf::from("."),
            front_controller: Some("index.php".to_string()),
            forbidden_extensions: vec![".htaccess".to_string()],
        }
    }
}

/// Where a request was routed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// A real PHP file under the document root.
    Script {
        /// Absolute path for `SCRIPT_FILENAME`.
        script_filename: PathBuf,
        /// The URL path of that script, for `SCRIPT_NAME`.
        script_name: String,
        /// Everything after the script, for `PATH_INFO`. Empty when unused.
        path_info: String,
    },
    /// Nothing matched, and no front controller is configured.
    NotFound,
    /// The path names a forbidden extension.
    Forbidden,
}

/// Resolve a URL path to a script, the way nginx plus a front controller does.
///
/// For `/index.php/foo/bar` the split is `SCRIPT_NAME = /index.php` and
/// `PATH_INFO = /foo/bar`. For `/about` with a front controller the split is
/// `SCRIPT_NAME = /index.php` and `PATH_INFO = /about`.
pub fn route(path: &str, routing: &Routing) -> Route {
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let decoded = percent_decode_cow(path);

    // Compared on bytes, case-insensitively, rather than through
    // `decoded.to_ascii_lowercase().ends_with(&ext.to_ascii_lowercase())`. That expression
    // allocated *twice* on every request — a copy of the whole path and a copy of every
    // configured extension — to answer a question about the last few bytes. It is the same
    // defect that `has_php_extension` below was fixed for, which this call site was missed
    // in.
    if routing.forbidden_extensions.iter().any(|ext| {
        let ext = ext.as_bytes();
        let bytes = decoded.as_bytes();
        bytes.len() >= ext.len() && bytes[bytes.len() - ext.len()..].eq_ignore_ascii_case(ext)
    }) {
        return Route::Forbidden;
    }

    let relative = decoded.trim_start_matches('/');

    // The extension is tested *before* the path is built and stat'ed. The two are pure
    // predicates so the answer is the same either way, but the order is what decides
    // whether a request pays a `stat` syscall at all: with a front controller configured,
    // a path naming no script is the common case, and testing for `.php` first means it
    // never reaches `is_file`.
    if has_php_extension(relative) {
        let candidate = routing.document_root.join(relative);
        if candidate.is_file() {
            return Route::Script {
                script_filename: candidate,
                script_name: format!("/{relative}"),
                path_info: String::new(),
            };
        }
    }

    // A path that starts with a real script is split at the script boundary.
    if let Some((script_relative, rest)) = split_at_script(relative, routing) {
        let script_filename = routing.document_root.join(&script_relative);
        if script_filename.is_file() {
            return Route::Script {
                script_filename,
                script_name: format!("/{script_relative}"),
                path_info: format!("/{rest}"),
            };
        }
    }

    match &routing.front_controller {
        Some(front) => {
            // A dotfile must not be handled by the application either. PHP is
            // the one interpreter that will happily emit a file it cannot parse,
            // so a front controller handed `/.env` as its PATH_INFO answers with
            // whatever is in it. Hiding one is the same decision the static layer
            // makes, for the same reason.
            if relative.split('/').any(|part| part.starts_with('.')) {
                return Route::NotFound;
            }
            let script_filename = routing.document_root.join(front);
            Route::Script {
                script_filename,
                script_name: format!("/{front}"),
                path_info: format!("/{relative}"),
            }
        }
        None => Route::NotFound,
    }
}

/// Find the longest leading path segment sequence that names an existing script.
///
/// The `has_php_extension` check is the whole point. Without it this matches any
/// file, and php-cgi does not verify that a script is PHP: given a file it cannot
/// parse it emits the file verbatim with a 200. So a front-controller deployment
/// would hand `/config.json` and `/.env` to PHP and return their contents.
/// Requiring the extension means PHP is only ever asked to run something it is
/// meant to run, and every other path falls through to the front controller as
/// application input.
///
/// Boundaries are walked rather than collected into a `Vec`, and each candidate is
/// tested on the bytes: the collection allocated once per request, and the old
/// `has_php_extension(&head)` lowercased the candidate prefix, so a path of *n* segments
/// allocated *n* times with each allocation proportional to its prefix. Asking the
/// *suffixes* first means only a boundary that can actually be a PHP script goes on to
/// build a path and call `is_file`, which is the expensive part.
fn split_at_script<'a>(relative: &'a str, routing: &Routing) -> Option<(String, &'a str)> {
    let bytes = relative.as_bytes();
    for (boundary, _) in relative.match_indices('/') {
        let head = &relative[..boundary];
        if head.is_empty() {
            continue;
        }
        if !php_extension_of(&bytes[..boundary]) {
            continue;
        }
        let candidate = routing.document_root.join(head);
        if candidate.is_file() {
            let rest = relative[boundary..].trim_start_matches('/');
            return Some((head.to_string(), rest));
        }
    }

    // The whole path as one candidate, which is the bare `/index.php` case.
    if !relative.is_empty() && php_extension_of(bytes) {
        let candidate = routing.document_root.join(relative);
        if candidate.is_file() {
            return Some((relative.to_string(), ""));
        }
    }
    None
}

/// Whether a path names something PHP is expected to execute.
///
/// Shared with the static layer, which must not serve one of these as a file:
/// serving `index.php` from disk sends its *source* to the client.
///
/// Case-insensitive without allocating, which is why this does not lowercase the whole
/// path: it used to call `to_ascii_lowercase()` and allocate a copy of the path on every
/// request, to answer a question about its last five bytes.
///
/// The comparison is on a byte slice, not on a `&str` suffix, because a byte offset taken
/// from the end of a `&str` can land inside a multi-byte character and slicing there
/// panics. A path with non-ASCII bytes in it must still be answerable, so callers pass
/// the bytes and no boundary is ever needed.
pub fn has_php_extension(path: &str) -> bool {
    php_extension_of(path.as_bytes())
}

/// PHP extensions this server will execute, as ASCII bytes for a case-insensitive
/// suffix comparison.
const PHP_EXTENSIONS: [&[u8]; 3] = [b".php", b".php3", b".phtml"];

/// Whether a byte slice ends with one of [`PHP_EXTENSIONS`], case-insensitively.
fn php_extension_of(bytes: &[u8]) -> bool {
    PHP_EXTENSIONS
        .iter()
        .any(|ext| bytes.len() >= ext.len() && bytes[bytes.len() - ext.len()..].eq_ignore_ascii_case(ext))
}

/// Percent-decode a URL path.
///
/// Invalid escapes are left as-is rather than rejected: a stray `%` in a path is
/// a request PHP can answer, not a reason to fail the whole request.
pub fn percent_decode(input: &str) -> String {
    percent_decode_cow(input).into_owned()
}

/// Percent-decode a URL path, borrowing it when there is nothing to decode.
///
/// Most requests contain no `%` at all, and copying the whole path to discover that is an
/// allocation per request for nothing. `Cow` makes that case free and leaves the decoding
/// case exactly as it was.
///
/// The scan and the copy are separate passes on purpose: a `Vec` that is only allocated
/// once a `%` is found means the common path never touches the allocator, rather than
/// allocating and then throwing the buffer away.
pub fn percent_decode_cow(input: &str) -> std::borrow::Cow<'_, str> {
    let bytes = input.as_bytes();
    if !bytes.contains(&b'%') {
        return std::borrow::Cow::Borrowed(input);
    }

    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &input[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }

    // `String::from_utf8` takes the buffer by value, so a decoded path that is valid UTF-8 —
    // every `%XX` sequence that spells a character — keeps the allocation it already made
    // instead of copying out of it. Only a genuinely broken escape (a `%` that is not valid
    // UTF-8 once decoded, which the spec does not really allow but this must not panic on)
    // pays for a second pass.
    match String::from_utf8(out) {
        Ok(text) => std::borrow::Cow::Owned(text),
        Err(err) => std::borrow::Cow::Owned(String::from_utf8_lossy(err.as_bytes()).into_owned()),
    }
}

/// The parts of a request needed to build the environment.
#[derive(Debug, Clone)]
pub struct RequestInfo {
    pub method: String,
    /// The raw request target, e.g. `/index.php/foo?x=1`.
    pub uri: String,
    pub query_string: String,
    pub protocol: String,
    pub headers: Vec<(String, String)>,
    pub remote_addr: String,
    pub server_name: String,
    pub server_port: u16,
    pub body_len: usize,
    pub https: bool,
}

impl Default for RequestInfo {
    fn default() -> Self {
        RequestInfo {
            method: "GET".to_string(),
            uri: "/".to_string(),
            query_string: String::new(),
            protocol: "HTTP/1.1".to_string(),
            headers: Vec::new(),
            remote_addr: "127.0.0.1".to_string(),
            server_name: "localhost".to_string(),
            server_port: 80,
            body_len: 0,
            https: false,
        }
    }
}

impl RequestInfo {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Build the FastCGI `PARAMS` for a request.
///
/// The variable set follows what a FastCGI client must send for php-fpm to
/// populate `$_SERVER`, `$_GET`, `$_POST` and `$_COOKIE` correctly.
///
/// Routing happens here. A caller that has *already* routed the request — the HTTP
/// front-end does, so it can turn "no script" into a 404 rather than a 502 — should pass
/// its answer to [`build_params_for`] instead: this would otherwise route a second time,
/// which walks the path and calls `is_file` again for nothing.
pub fn build_params(info: &RequestInfo, routing: &Routing) -> Result<NameValuePairs> {
    let path = info.uri.split('?').next().unwrap_or(&info.uri);
    let route = route(path, routing);
    build_params_for(info, routing, &route)
}

/// Build the FastCGI `PARAMS` for a request whose route is already known.
///
/// Identical to [`build_params`] except that it does not route: the caller supplies the
/// [`Route`] it resolved. This exists because the HTTP front-end has to route before it
/// builds the environment — a path that names no script is a 404 there, not a 502 — and
/// having both do it meant the router ran twice per request, `is_file` syscalls included.
pub fn build_params_for(
    info: &RequestInfo,
    routing: &Routing,
    route: &Route,
) -> Result<NameValuePairs> {
    let (script_filename, script_name, path_info) = match route {
        Route::Script {
            script_filename,
            script_name,
            path_info,
        } => (
            script_filename.display().to_string(),
            script_name.clone(),
            path_info.clone(),
        ),
        Route::NotFound => {
            return Err(FcgiError::BadRequest(format!(
                "no script matches {} and no front controller is configured",
                info.uri
            )))
        }
        Route::Forbidden => {
            return Err(FcgiError::BadRequest(format!(
                "{} names a forbidden path",
                info.uri
            )))
        }
    };

    let mut pairs: NameValuePairs = Vec::with_capacity(32);

    push_pair(&mut pairs, "GATEWAY_INTERFACE", "FastCGI/1.0");
    push_pair(&mut pairs, "REQUEST_METHOD", &info.method);
    push_pair(&mut pairs, "REQUEST_URI", &info.uri);
    push_pair(&mut pairs, "QUERY_STRING", &info.query_string);
    push_pair(&mut pairs, "SERVER_PROTOCOL", &info.protocol);
    push_pair(
        &mut pairs,
        "SERVER_SOFTWARE",
        concat!("stokehold/", env!("CARGO_PKG_VERSION")),
    );
    push_pair(&mut pairs, "SERVER_NAME", &info.server_name);
    push_pair(&mut pairs, "SERVER_PORT", &info.server_port.to_string());
    push_pair(&mut pairs, "REMOTE_ADDR", &info.remote_addr);
    push_pair(
        &mut pairs,
        "DOCUMENT_ROOT",
        &routing.document_root.display().to_string(),
    );
    push_pair(&mut pairs, "SCRIPT_FILENAME", &script_filename);
    push_pair(&mut pairs, "SCRIPT_NAME", &script_name);
    push_pair(&mut pairs, "REDIRECT_STATUS", "200");
    if !path_info.is_empty() && path_info != "/" {
        push_pair(&mut pairs, "PATH_INFO", &path_info);
    }
    if info.https {
        push_pair(&mut pairs, "HTTPS", "on");
    }

    // CONTENT_LENGTH is always sent when there is a body. See the module docs:
    // omitting it makes PHP drop the body silently.
    if info.body_len > 0 {
        push_pair(&mut pairs, "CONTENT_LENGTH", &info.body_len.to_string());
    }

    // The request's own headers become HTTP_*, except Content-Type and
    // Content-Length, which keep their CGI names.
    for (name, value) in &info.headers {
        if name.eq_ignore_ascii_case("content-length") {
            // Prefer the length of the body we are actually framing, so a
            // mismatched client header cannot make PHP read the wrong amount.
            continue;
        }
        if name.eq_ignore_ascii_case("content-type") {
            push_pair(&mut pairs, "CONTENT_TYPE", value);
            continue;
        }
        push_http_header(&mut pairs, name, value);
    }

    // A POST with a body but no declared type would have no CONTENT_TYPE at all,
    // which PHP treats as a form-urlencoded body.
    if info.body_len > 0 && info.header("content-type").is_none() {
        push_pair(
            &mut pairs,
            "CONTENT_TYPE",
            "application/x-www-form-urlencoded",
        );
    }

    Ok(pairs)
}

/// Append one name-value pair.
fn push_pair(pairs: &mut NameValuePairs, name: &str, value: &str) {
    pairs.push((name.as_bytes().to_vec(), value.as_bytes().to_vec()));
}

/// Append one request header under its CGI name: `HTTP_` plus the name uppercased with
/// `-` replaced by `_`.
///
/// Built straight into the pair's own buffer rather than through
/// `format!("HTTP_{}", name.to_ascii_uppercase().replace('-', "_"))`. That expression was
/// three intermediate `String`s per request header — an uppercase copy, a replaced copy
/// and the formatted result — and then the pair's own two. Six allocations per header, for
/// a name that is a fixed prefix, a case fold and a byte substitution, none of which needs
/// an intermediate at all. The allocation counter in `tests/allocations.rs` is what showed
/// the size of it.
fn push_http_header(pairs: &mut NameValuePairs, name: &str, value: &str) {
    let mut key = Vec::with_capacity(5 + name.len());
    key.extend_from_slice(b"HTTP_");
    for byte in name.bytes() {
        // `to_ascii_uppercase` on a byte, not on a `String`: the name is ASCII by the time
        // it gets here (it arrived as a header name), and a non-ASCII byte is passed through
        // unchanged exactly as `str::to_ascii_uppercase` would leave it.
        key.push(if byte == b'-' {
            b'_'
        } else {
            byte.to_ascii_uppercase()
        });
    }
    pairs.push((key, value.as_bytes().to_vec()));
}

/// Convenience: the script PHP would execute for a URL path.
pub fn script_for(path: &str, routing: &Routing) -> Option<PathBuf> {
    match route(path, routing) {
        Route::Script {
            script_filename, ..
        } => Some(script_filename),
        _ => None,
    }
}

/// True when `path` is a PHP file that PHP will agree to execute.
///
/// php-fpm's `security.limit_extensions` defaults to `.php`, so a matching
/// extension is not a stylistic choice: a script without one is refused with
/// "Access denied".
pub fn is_executable_script(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("php"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a temporary document root with the given files.
    fn fixture(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "stokehold-cgi-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create the fixture root");
        for (relative, contents) in files {
            let path = root.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create a fixture directory");
            }
            std::fs::write(&path, contents).expect("write a fixture file");
        }
        root
    }

    #[test]
    fn routes_a_direct_script() {
        let root = fixture("direct", &[("index.php", "<?php"), ("about.php", "<?php")]);
        let routing = Routing {
            document_root: root.clone(),
            front_controller: None,
            ..Routing::default()
        };

        match route("/about.php", &routing) {
            Route::Script {
                script_name,
                path_info,
                ..
            } => {
                assert_eq!(script_name, "/about.php");
                assert!(path_info.is_empty());
            }
            other => panic!("expected a script, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn routes_a_path_through_the_front_controller() {
        let root = fixture("front", &[("index.php", "<?php")]);
        let routing = Routing {
            document_root: root.clone(),
            front_controller: Some("index.php".to_string()),
            ..Routing::default()
        };

        // This is `/about` reaching index.php with PATH_INFO, the nginx pattern.
        match route("/about", &routing) {
            Route::Script {
                script_name,
                path_info,
                ..
            } => {
                assert_eq!(script_name, "/index.php");
                assert_eq!(path_info, "/about");
            }
            other => panic!("expected the front controller, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn splits_script_name_from_path_info() {
        let root = fixture("split", &[("index.php", "<?php")]);
        let routing = Routing {
            document_root: root.clone(),
            front_controller: Some("index.php".to_string()),
            ..Routing::default()
        };

        match route("/index.php/foo/bar", &routing) {
            Route::Script {
                script_name,
                path_info,
                ..
            } => {
                assert_eq!(script_name, "/index.php");
                assert_eq!(path_info, "/foo/bar");
            }
            other => panic!("expected a split, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_extension_is_matched_case_insensitively_without_lowercasing() {
        // The suffix check used to lowercase the whole path, which allocated a copy of
        // it on every request to ask about its last five bytes. The predicate must not
        // change: PHP itself accepts an upper-case extension.
        assert!(has_php_extension("/index.php"));
        assert!(has_php_extension("/INDEX.PHP"));
        assert!(has_php_extension("/Index.Php3"));
        assert!(has_php_extension("index.phtml"));
        assert!(!has_php_extension("/index.php.bak"));
        assert!(!has_php_extension("/index.ph"));
        assert!(!has_php_extension(""));
        // A path may carry non-ASCII bytes, and the suffix comparison must not slice
        // at a byte offset that is not a character boundary — the reason this compares
        // bytes rather than a `&str`.
        assert!(has_php_extension("/caf\u{e9}/index.php"));
        assert!(!has_php_extension("/caf\u{e9}"));
    }

    #[test]
    fn a_non_ascii_path_does_not_panic_the_router() {
        // The naive optimisation of this function was `path[path.len() - 5..]`, which
        // panics when the offset lands inside a multi-byte character. This path has one
        // exactly there.
        let root = fixture("nonascii", &[("index.php", "<?php")]);
        let routing = Routing {
            document_root: root.clone(),
            front_controller: Some("index.php".to_string()),
            ..Routing::default()
        };

        for path in ["/caf\u{e9}", "/\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}", "/a\u{1f600}b"] {
            match route(path, &routing) {
                Route::Script { script_name, .. } => assert_eq!(script_name, "/index.php"),
                other => panic!("{path}: expected the front controller, got {other:?}"),
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_script_deeper_in_the_path_still_wins_over_the_front_controller() {
        // The routing rewrite changed how boundaries are walked, so pin the ordering it
        // is supposed to preserve: the *first* boundary that names an existing script wins,
        // which is the longest leading sequence.
        let root = fixture(
            "deep",
            &[
                ("index.php", "<?php"),
                ("admin/index.php", "<?php"),
                ("admin/tools/report.php", "<?php"),
            ],
        );
        let routing = Routing {
            document_root: root.clone(),
            front_controller: Some("index.php".to_string()),
            ..Routing::default()
        };

        for (path, script, info) in [
            ("/admin/index.php/settings", "/admin/index.php", "/settings"),
            (
                "/admin/tools/report.php/today",
                "/admin/tools/report.php",
                "/today",
            ),
            // No real script, so the front controller takes it with the path as input.
            ("/admin/missing/page", "/index.php", "/admin/missing/page"),
        ] {
            match route(path, &routing) {
                Route::Script {
                    script_name,
                    path_info,
                    ..
                } => {
                    assert_eq!(script_name, script, "{path}");
                    assert_eq!(path_info, info, "{path}");
                }
                other => panic!("{path}: expected a script, got {other:?}"),
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A dotfile does not reach the application either, only the static layer.
    ///
    /// `/.env` travels as PATH_INFO otherwise, and the front controller answers
    /// with the file's contents.
    #[test]
    fn a_dotfile_does_not_reach_the_application() {
        let root = fixture("dotapp", &[("index.php", "<?php")]);
        let routing = Routing {
            document_root: root.clone(),
            front_controller: Some("index.php".to_string()),
            ..Routing::default()
        };

        for path in ["/.env", "/.git/config", "/sub/.hidden", "/.well-known"] {
            assert_eq!(route(path, &routing), Route::NotFound, "{path}");
        }
        // An ordinary path still reaches the front controller.
        match route("/about", &routing) {
            Route::Script { script_name, .. } => assert_eq!(script_name, "/index.php"),
            other => panic!("expected the front controller, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Only a script may be handed to PHP as the script.
    ///
    /// php-cgi does not check that a script is PHP: given a file it cannot parse
    /// it emits it verbatim with a 200. So a route that matches any existing file
    /// turns the document root into a file server by accident — verified by
    /// hand, `GET /.env` returned the secrets in it even with static file serving
    /// switched off.
    #[test]
    fn a_non_php_file_is_never_routed_to_php_as_a_script() {
        let root = fixture(
            "nonphp",
            &[
                ("index.php", "<?php"),
                ("assets/app.css", "body{margin:0}"),
                ("config.json", "{\"secret\":1}"),
                (".env", "SECRET=1"),
            ],
        );
        let routing = Routing {
            document_root: root.clone(),
            front_controller: Some("index.php".to_string()),
            ..Routing::default()
        };

        // A stylesheet and a JSON file must go to the front controller as
        // *input*, never be used as the script themselves.
        for path in ["/assets/app.css", "/config.json"] {
            match route(path, &routing) {
                Route::Script { script_name, .. } => {
                    assert_eq!(script_name, "/index.php", "{path} must not be the script");
                }
                other => panic!("{path}: expected the front controller, got {other:?}"),
            }
        }

        // A dotfile never reaches PHP at all.
        assert_eq!(route("/.env", &routing), Route::NotFound);

        // PATH_INFO still works after a real script.
        match route("/index.php/foo", &routing) {
            Route::Script {
                script_name,
                path_info,
                ..
            } => {
                assert_eq!(script_name, "/index.php");
                assert_eq!(path_info, "/foo");
            }
            other => panic!("expected a split, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_query_string_is_not_part_of_the_path() {
        let root = fixture("query", &[("index.php", "<?php")]);
        let routing = Routing {
            document_root: root.clone(),
            front_controller: Some("index.php".to_string()),
            ..Routing::default()
        };

        match route("/admin?tab=users", &routing) {
            Route::Script { path_info, .. } => assert_eq!(path_info, "/admin"),
            other => panic!("expected the front controller, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reports_not_found_without_a_front_controller() {
        let root = fixture("notfound", &[("index.php", "<?php")]);
        let routing = Routing {
            document_root: root.clone(),
            front_controller: None,
            ..Routing::default()
        };
        assert_eq!(route("/nope", &routing), Route::NotFound);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn forbids_configured_extensions() {
        let routing = Routing::default();
        assert_eq!(route("/.htaccess", &routing), Route::Forbidden);
        assert_eq!(route("/config/.htaccess", &routing), Route::Forbidden);
    }

    #[test]
    fn decodes_percent_escapes_and_leaves_invalid_ones_alone() {
        assert_eq!(percent_decode("/a%20b"), "/a b");
        assert_eq!(percent_decode("/caf%C3%A9"), "/café");
        assert_eq!(percent_decode("/100%"), "/100%");
        assert_eq!(percent_decode("/%zz"), "/%zz");
    }

    #[test]
    fn always_sends_content_length_when_there_is_a_body() {
        let routing = Routing::default();
        let info = RequestInfo {
            method: "POST".to_string(),
            uri: "/index.php".to_string(),
            body_len: 42,
            ..RequestInfo::default()
        };
        let params = build_params(&info, &routing).expect("params build");
        let lookup = |name: &str| {
            params
                .iter()
                .find(|(key, _)| key.as_slice() == name.as_bytes())
                .map(|(_, value)| String::from_utf8_lossy(value).into_owned())
        };
        assert_eq!(lookup("CONTENT_LENGTH").as_deref(), Some("42"));
    }

    #[test]
    fn omits_content_length_for_a_bodyless_request() {
        let routing = Routing::default();
        let info = RequestInfo {
            uri: "/index.php".to_string(),
            ..RequestInfo::default()
        };
        let params = build_params(&info, &routing).expect("params build");
        assert!(
            !params
                .iter()
                .any(|(key, _)| key.as_slice() == b"CONTENT_LENGTH"),
            "a GET must not declare a body"
        );
    }

    #[test]
    fn a_client_content_length_header_cannot_override_the_framed_body() {
        let routing = Routing::default();
        let info = RequestInfo {
            method: "POST".to_string(),
            uri: "/index.php".to_string(),
            body_len: 7,
            headers: vec![("Content-Length".to_string(), "9999".to_string())],
            ..RequestInfo::default()
        };
        let params = build_params(&info, &routing).expect("params build");
        let lengths: Vec<String> = params
            .iter()
            .filter(|(key, _)| key.as_slice() == b"CONTENT_LENGTH")
            .map(|(_, value)| String::from_utf8_lossy(value).into_owned())
            .collect();
        assert_eq!(lengths, vec!["7".to_string()]);
    }

    #[test]
    fn maps_request_headers_to_the_http_variables() {
        let routing = Routing::default();
        let info = RequestInfo {
            uri: "/index.php".to_string(),
            headers: vec![
                ("Accept-Language".to_string(), "de-DE".to_string()),
                ("Host".to_string(), "example.test".to_string()),
                ("Content-Type".to_string(), "application/json".to_string()),
            ],
            body_len: 2,
            ..RequestInfo::default()
        };
        let params = build_params(&info, &routing).expect("params build");
        let lookup = |name: &str| {
            params
                .iter()
                .find(|(key, _)| key.as_slice() == name.as_bytes())
                .map(|(_, value)| String::from_utf8_lossy(value).into_owned())
        };
        assert_eq!(lookup("HTTP_ACCEPT_LANGUAGE").as_deref(), Some("de-DE"));
        assert_eq!(lookup("HTTP_HOST").as_deref(), Some("example.test"));
        assert_eq!(lookup("CONTENT_TYPE").as_deref(), Some("application/json"));
        // Content-Type keeps its CGI name rather than becoming HTTP_CONTENT_TYPE.
        assert_eq!(lookup("HTTP_CONTENT_TYPE"), None);
    }

    #[test]
    fn a_precomputed_route_builds_the_same_environment_as_building_it_fresh() {
        // `build_params_for` exists so the HTTP front-end does not route twice — it has to
        // route first, because "no script" is a 404 there and not a 502. This pins the two
        // entry points to each other, so the optimisation cannot drift from the original.
        let root = fixture(
            "precomputed",
            &[("index.php", "<?php"), ("admin/index.php", "<?php")],
        );
        let routing = Routing {
            document_root: root.clone(),
            front_controller: Some("index.php".to_string()),
            ..Routing::default()
        };

        for (uri, headers) in [
            ("/index.php", Vec::new()),
            ("/admin/index.php/settings", Vec::new()),
            ("/items", Vec::new()),
            (
                "/items",
                vec![
                    ("Host".to_string(), "example.test".to_string()),
                    ("Accept".to_string(), "*/*".to_string()),
                ],
            ),
        ] {
            let info = RequestInfo {
                uri: uri.to_string(),
                headers,
                ..RequestInfo::default()
            };
            let fresh = build_params(&info, &routing).expect("build_params");
            let known = route(info.uri.split('?').next().unwrap_or(&info.uri), &routing);
            let precomputed = build_params_for(&info, &routing, &known).expect("build_params_for");
            assert_eq!(fresh, precomputed, "{uri}");
        }

        // A path with no script is still the same error through both, so the front-end's
        // 404 decision and the builder cannot disagree about what is routable.
        let routing_no_front = Routing {
            document_root: root.clone(),
            front_controller: None,
            ..Routing::default()
        };
        let info = RequestInfo {
            uri: "/nothing".to_string(),
            ..RequestInfo::default()
        };
        let known = route("/nothing", &routing_no_front);
        assert!(matches!(known, Route::NotFound));
        assert!(build_params(&info, &routing_no_front).is_err());
        assert!(build_params_for(&info, &routing_no_front, &known).is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The CGI name of a request variable, as `format!` used to produce it.
    ///
    /// The reference expression, kept here so the in-place version can be checked against
    /// it. It is not what the code does any more: it allocates three `String`s per header.
    fn reference_http_key(name: &str) -> String {
        format!("HTTP_{}", name.to_ascii_uppercase().replace('-', "_"))
    }

    #[test]
    fn the_in_place_cgi_name_matches_the_formatting_it_replaced() {
        // The rewrite of the key construction is only worth having if it is the same
        // function. This compares the two directly across the shapes a header name takes:
        // short, long, already upper-case, mixed, dotted, underscored, numeric, and a name
        // long enough to force the `Vec` to grow.
        let names = [
            "Host",
            "Accept",
            "accept-language",
            "ACCEPT-LANGUAGE",
            "X-Forwarded-For",
            "X-Forwarded-Proto",
            "If-None-Match",
            "Range",
            "X-Custom-Header-With-A-Fairly-Long-Name-After-The-Prefix",
            "X-2",
            "X-A_B",
            "x-a.b",
        ];

        for name in names {
            let mut pairs: NameValuePairs = Vec::new();
            push_http_header(&mut pairs, name, "v");
            let produced = String::from_utf8(pairs[0].0.clone()).expect("ASCII");
            assert_eq!(
                produced,
                reference_http_key(name),
                "the key for {name:?} must be byte-for-byte what the format produced"
            );
        }
    }

    #[test]
    fn a_body_without_a_declared_type_defaults_to_form_urlencoded() {
        let routing = Routing::default();
        let info = RequestInfo {
            method: "POST".to_string(),
            uri: "/index.php".to_string(),
            body_len: 5,
            ..RequestInfo::default()
        };
        let params = build_params(&info, &routing).expect("params build");
        assert!(params
            .iter()
            .any(|(key, value)| key.as_slice() == b"CONTENT_TYPE"
                && value.as_slice() == b"application/x-www-form-urlencoded"));
    }

    #[test]
    fn sets_path_info_only_when_there_is_one() {
        let root = fixture("pathinfo", &[("index.php", "<?php")]);
        let routing = Routing {
            document_root: root.clone(),
            front_controller: Some("index.php".to_string()),
            ..Routing::default()
        };

        let info = RequestInfo {
            uri: "/index.php".to_string(),
            ..RequestInfo::default()
        };
        let params = build_params(&info, &routing).expect("params build");
        assert!(!params.iter().any(|(key, _)| key.as_slice() == b"PATH_INFO"));

        let info = RequestInfo {
            uri: "/index.php/deep".to_string(),
            ..RequestInfo::default()
        };
        let params = build_params(&info, &routing).expect("params build");
        assert!(params
            .iter()
            .any(|(key, value)| key.as_slice() == b"PATH_INFO" && value.as_slice() == b"/deep"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_a_forbidden_path_instead_of_routing_it() {
        let routing = Routing::default();
        let info = RequestInfo {
            uri: "/.htaccess".to_string(),
            ..RequestInfo::default()
        };
        assert!(build_params(&info, &routing).is_err());
    }

    #[test]
    fn marks_https_when_the_request_is_secure() {
        let routing = Routing::default();
        let info = RequestInfo {
            uri: "/index.php".to_string(),
            https: true,
            ..RequestInfo::default()
        };
        let params = build_params(&info, &routing).expect("params build");
        assert!(params
            .iter()
            .any(|(key, value)| key.as_slice() == b"HTTPS" && value.as_slice() == b"on"));
    }

    #[test]
    fn only_php_extensioned_files_are_executable() {
        assert!(is_executable_script(Path::new("/srv/index.php")));
        assert!(is_executable_script(Path::new("/srv/index.PHP")));
        assert!(!is_executable_script(Path::new("/srv/index.txt")));
        assert!(!is_executable_script(Path::new("/srv/index")));
    }
}
