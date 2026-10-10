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
    let decoded = percent_decode(path);

    if routing.forbidden_extensions.iter().any(|ext| {
        decoded
            .to_ascii_lowercase()
            .ends_with(&ext.to_ascii_lowercase())
    }) {
        return Route::Forbidden;
    }

    let relative = decoded.trim_start_matches('/');
    let candidate = routing.document_root.join(relative);

    if candidate.is_file() && has_php_extension(relative) {
        return Route::Script {
            script_filename: candidate,
            script_name: format!("/{relative}"),
            path_info: String::new(),
        };
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

/// Find the longest leading path segment sequence that names an existing file.
fn split_at_script<'a>(relative: &'a str, routing: &Routing) -> Option<(String, &'a str)> {
    let mut boundaries: Vec<usize> = relative
        .match_indices('/')
        .map(|(index, _)| index)
        .collect();
    boundaries.push(relative.len());

    for boundary in boundaries {
        let head = &relative[..boundary];
        if head.is_empty() {
            continue;
        }
        let candidate = routing.document_root.join(head);
        if candidate.is_file() {
            let rest = relative[boundary..].trim_start_matches('/');
            return Some((head.to_string(), rest));
        }
    }
    None
}

fn has_php_extension(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".php") || lower.ends_with(".php3") || lower.ends_with(".phtml")
}

/// Percent-decode a URL path.
///
/// Invalid escapes are left as-is rather than rejected: a stray `%` in a path is
/// a request PHP can answer, not a reason to fail the whole request.
pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
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
    String::from_utf8_lossy(&out).into_owned()
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
pub fn build_params(info: &RequestInfo, routing: &Routing) -> Result<NameValuePairs> {
    let path = info.uri.split('?').next().unwrap_or(&info.uri).to_string();
    let route = route(&path, routing);

    let (script_filename, script_name, path_info) = match &route {
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
    let mut push = |name: &str, value: &str| {
        pairs.push((name.as_bytes().to_vec(), value.as_bytes().to_vec()));
    };

    push("GATEWAY_INTERFACE", "FastCGI/1.0");
    push("REQUEST_METHOD", &info.method);
    push("REQUEST_URI", &info.uri);
    push("QUERY_STRING", &info.query_string);
    push("SERVER_PROTOCOL", &info.protocol);
    push(
        "SERVER_SOFTWARE",
        concat!("stokehold/", env!("CARGO_PKG_VERSION")),
    );
    push("SERVER_NAME", &info.server_name);
    push("SERVER_PORT", &info.server_port.to_string());
    push("REMOTE_ADDR", &info.remote_addr);
    push(
        "DOCUMENT_ROOT",
        &routing.document_root.display().to_string(),
    );
    push("SCRIPT_FILENAME", &script_filename);
    push("SCRIPT_NAME", &script_name);
    push("REDIRECT_STATUS", "200");
    if !path_info.is_empty() && path_info != "/" {
        push("PATH_INFO", &path_info);
    }
    if info.https {
        push("HTTPS", "on");
    }

    // CONTENT_LENGTH is always sent when there is a body. See the module docs:
    // omitting it makes PHP drop the body silently.
    if info.body_len > 0 {
        push("CONTENT_LENGTH", &info.body_len.to_string());
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
            push("CONTENT_TYPE", value);
            continue;
        }
        let key = format!("HTTP_{}", name.to_ascii_uppercase().replace('-', "_"));
        push(&key, value);
    }

    // A POST with a body but no declared type would have no CONTENT_TYPE at all,
    // which PHP treats as a form-urlencoded body.
    if info.body_len > 0 && info.header("content-type").is_none() {
        push("CONTENT_TYPE", "application/x-www-form-urlencoded");
    }

    Ok(pairs)
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
