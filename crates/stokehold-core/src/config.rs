//! Configuration model.
//!
//! The shape mirrors the design in `docs/plan.md`: one or two listeners, a set
//! of named backends, a route table selecting between them, and an optional
//! static-content layer in front of the whole thing.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const DEFAULT_WORKERS: u32 = 0; // 0 = number of logical CPUs

fn default_workers() -> u32 {
    DEFAULT_WORKERS
}

/// Top level configuration, deserialized from `stokehold.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,

    /// Named backends. A route (or the default) refers to one of these.
    #[serde(default)]
    pub backends: BTreeMap<String, BackendConfig>,

    /// Backend used when a request does not match a route.
    #[serde(default)]
    pub default_backend: Option<String>,

    /// Route table, in the order written. See [`RouteConfig`] for the
    /// precedence rule.
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
}

/// A route entry, selecting a backend for matching requests.
///
/// Both matchers are optional, but at least one must be present — a route with
/// neither is just `default_backend` written the long way, and it would make the
/// precedence rule meaningless. Constraints within one route are **all** of
/// ("and"), not any of.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    /// Match a request whose path starts with this string, e.g. `/api`.
    #[serde(default)]
    pub path_prefix: Option<String>,

    /// Match a request whose `Host` header is exactly this, ignoring the
    /// `:port` suffix. Case-insensitive.
    #[serde(default)]
    pub host: Option<String>,

    /// The backend to dispatch to.
    pub backend: String,
}

impl RouteConfig {
    /// How specific this route is: the number of constraints it states.
    ///
    /// Used to pick a winner when several routes match, so a route naming a host
    /// and a path beats one naming only the path.
    pub fn specificity(&self) -> usize {
        self.host.is_some() as usize + self.path_prefix.is_some() as usize
    }

    /// Whether this route's constraints all match the given host and path.
    pub fn matches(&self, host: Option<&str>, path: &str) -> bool {
        if let Some(wanted) = &self.path_prefix {
            if !path.starts_with(wanted.as_str()) {
                return false;
            }
        }
        if let Some(wanted) = &self.host {
            let host = host.unwrap_or_default();
            // A Host header may carry a port; the route names a host.
            let host = host.split(':').next().unwrap_or(host);
            if !host.eq_ignore_ascii_case(wanted) {
                return false;
            }
        }
        true
    }
}

impl Config {
    /// The backend that should serve this request, if a route matches it.
    ///
    /// The most specific matching route wins (see [`RouteConfig::specificity`]);
    /// ties are broken by order of declaration, so the earlier entry wins. This
    /// is deliberately the whole rule: anything more clever becomes
    /// unguessable, and a reader of the file should be able to predict it.
    pub fn route_backend(&self, host: Option<&str>, path: &str) -> Option<&str> {
        self.routes
            .iter()
            .filter(|route| route.matches(host, path))
            .max_by_key(|route| route.specificity())
            .map(|route| route.backend.as_str())
    }
}

/// Server-wide settings: the listeners, plus how a request path becomes a script.
///
/// Both listeners are optional, but at least one must be configured.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// HTTP listener, e.g. `127.0.0.1:8080`.
    #[serde(default)]
    pub http: Option<String>,

    /// FastCGI listener, e.g. `127.0.0.1:9000` or `unix:/run/stokehold.sock`.
    #[serde(default)]
    pub fastcgi: Option<String>,

    /// Directory that PHP scripts and static files are resolved against.
    ///
    /// A relative path is relative to the process's working directory, not to
    /// this file — the same rule [`WorkerConfig::working_dir`] follows. Prefer
    /// an absolute path: a deployment that sets its working directory elsewhere
    /// would otherwise resolve somewhere else silently. `stokehold check` prints
    /// the path it actually resolved.
    ///
    /// Unset means the `serve` command line decides, and failing that `.`.
    #[serde(default)]
    pub document_root: Option<PathBuf>,

    /// Front controller, relative to the document root, e.g. `index.php`.
    ///
    /// This is what makes a front controller work: a path naming no file under
    /// the document root is given to this script with `PATH_INFO` set, instead
    /// of answering 404.
    ///
    /// Set it to the empty string to serve only real `.php` files, matching
    /// nginx's `location ~ \.php$`. Unset means the `serve` command line
    /// decides, and failing that `index.php`.
    #[serde(default)]
    pub front_controller: Option<String>,

    /// Serving files from the document root, without a backend.
    #[serde(default)]
    pub static_files: StaticConfig,
}

/// The front controller used when neither the config nor the command line says.
pub const DEFAULT_FRONT_CONTROLLER: &str = "index.php";

impl ServerConfig {
    /// The effective front controller, or `None` for "serve only real files".
    ///
    /// The command line wins over this file, because a flag is the more specific
    /// statement, and an empty value on either side means "no front controller".
    pub fn resolve_front_controller(&self, from_command_line: Option<&str>) -> Option<String> {
        let value = from_command_line
            .or(self.front_controller.as_deref())
            .unwrap_or(DEFAULT_FRONT_CONTROLLER);
        let value = value.trim();
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    }
}

/// How files under the document root are served.
///
/// The HTTP front-end only. A file goes to a backend when nothing here matches,
/// which is what keeps a front controller working: with `index.php` configured,
/// a path that names no file is PHP's business, not this layer's.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticConfig {
    /// Serve files at all. Off by default, so an existing deployment is not
    /// changed by upgrading: a request that used to reach the front controller
    /// still does.
    #[serde(default)]
    pub enabled: bool,

    /// Serve `file.gz` or `file.br` when the client accepts that encoding.
    ///
    /// Opt-in because it assumes the build produced those files, and silently
    /// serving a stale one is worse than not serving it at all.
    #[serde(default)]
    pub precompressed: bool,

    /// Serve `index.html` for a directory, with a redirect when the URL has no
    /// trailing slash. When off, a directory is not a file and so goes to PHP —
    /// which is what an application with no static build wants.
    #[serde(default = "default_true")]
    pub directory_index: bool,

    /// What to do with a dotfile such as `.git/config` or `.env`.
    #[serde(default)]
    pub dotfiles: Dotfiles,

    /// `Cache-Control` for files served from here.
    ///
    /// One value for everything, deliberately: a per-extension policy is a
    /// guess about someone else's build, and a wrong `immutable` is invisible
    /// until assets go stale.
    #[serde(default = "default_cache_control")]
    pub cache_control: String,

    /// Refuse a multi-range request larger than this many bytes.
    ///
    /// A multipart range answer is assembled in memory, and `bytes=0-,0-,0-,…`
    /// is a cheap way to ask a server to allocate. Single ranges and `Range:
    /// bytes=0-` are unaffected: they are answered by streaming.
    #[serde(default = "default_max_multirange")]
    pub max_multirange_bytes: u64,
}

impl Default for StaticConfig {
    fn default() -> Self {
        StaticConfig {
            enabled: false,
            precompressed: false,
            directory_index: true,
            dotfiles: Dotfiles::Deny,
            cache_control: default_cache_control(),
            max_multirange_bytes: default_max_multirange(),
        }
    }
}

/// What to do when a request path contains a dotfile component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dotfiles {
    /// 404, matching how nginx treats a hidden file by default. Not 403: which
    /// files exist is not something a client needs to learn.
    #[default]
    Deny,
    /// Treat them like any other file. Only sensible when the document root
    /// holds nothing but published assets.
    Allow,
}

fn default_true() -> bool {
    true
}

fn default_cache_control() -> String {
    "public, max-age=3600".to_string()
}

fn default_max_multirange() -> u64 {
    1024 * 1024
}

/// A backend is one of four shapes, tagged by `kind` in TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackendConfig {
    /// Persistent PHP process speaking stokehold's own framed protocol over pipes.
    Worker(WorkerConfig),
    /// An already-running php-fpm (systemd-managed). Linux/macOS default.
    External(ExternalConfig),
    /// stokehold starts php-fpm itself; php-fpm still manages its own children.
    SpawnFpm(SpawnFpmConfig),
    /// N `php-cgi` processes on N ports. The only option on Windows.
    SpawnCgi(SpawnCgiConfig),
}

impl BackendConfig {
    pub fn kind(&self) -> &'static str {
        match self {
            BackendConfig::Worker(_) => "worker",
            BackendConfig::External(_) => "external",
            BackendConfig::SpawnFpm(_) => "spawn_fpm",
            BackendConfig::SpawnCgi(_) => "spawn_cgi",
        }
    }

    /// True when the backend resets state per request in PHP's own SAPI.
    pub fn resets_per_request(&self) -> bool {
        !matches!(self, BackendConfig::Worker(_))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerConfig {
    /// Command line that starts the persistent worker, e.g. `php worker.php`.
    pub command: String,

    /// Number of worker processes. `0` means one per logical CPU.
    #[serde(default = "default_workers")]
    pub workers: u32,

    /// Recycle a worker once its resident set exceeds this many megabytes.
    #[serde(default)]
    pub memory_limit_mb: Option<u64>,

    /// Recycle a worker after this many requests.
    ///
    /// **`0` means "never recycle"**, matching php-fpm's `pm.max_requests = 0` and
    /// RoadRunner's `max_jobs: 0`. That is the opposite of what a bare
    /// `served >= max` comparison would do, and the difference is not academic: a
    /// php-fpm pool config carrying `pm.max_requests = 0` is the common production
    /// case, and copying it here used to destroy the worker after every request —
    /// one application boot per request, which is the deployment model this backend
    /// exists to avoid. Absent and zero therefore both mean "no limit".
    #[serde(default)]
    pub max_requests: Option<u64>,

    /// Working directory for the worker process.
    #[serde(default)]
    pub working_dir: Option<PathBuf>,

    /// Extra environment variables for the worker process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalConfig {
    /// php-fpm listener: `unix:/run/php/php8.3-fpm.sock` or `127.0.0.1:9000`.
    pub socket: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnFpmConfig {
    /// php-fpm binary.
    pub binary: PathBuf,

    /// php-fpm configuration file to pass with `--fpm-config`.
    #[serde(default)]
    pub config: Option<PathBuf>,

    /// Listener php-fpm will bind, used by stokehold to reach it.
    #[serde(default)]
    pub socket: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnCgiConfig {
    /// `php-cgi` binary.
    pub binary: PathBuf,

    /// Number of processes, and therefore number of ports. `0` means one per logical CPU.
    #[serde(default = "default_workers")]
    pub workers: u32,

    /// Host each process binds to. Loopback only on Windows.
    #[serde(default = "default_bind_host")]
    pub bind_host: String,

    /// First port to assign. Each worker gets the next free port from here.
    #[serde(default = "default_base_port")]
    pub base_port: u16,

    /// Bind unix sockets instead of TCP, where the platform has them.
    ///
    /// This is a path **prefix**, not a finished path: slot `i` binds
    /// `<prefix>.<i>.sock`, for the same reason each TCP slot needs its own port
    /// — `php-cgi` processes one request at a time, and one socket per process is
    /// what lets a slot be recycled, counted and reasoned about on its own.
    ///
    /// `php-cgi -b` accepts an `AF_UNIX` path on any non-Windows platform because
    /// `fcgi_listen` (`main/fastcgi.c`) only falls back to TCP when the argument
    /// contains a `:` or is all digits. On Windows the same code path is a named
    /// pipe, which is a different feature, so this option is rejected there at
    /// validation time rather than accepted and ignored.
    ///
    /// `bind_host` and `base_port` are unused when this is set.
    #[serde(default)]
    pub socket: Option<String>,

    /// Recycle a slot after this many requests, the equivalent of php-fpm's
    /// `pm.max_requests`. **`0` means never recycle**, as it does there.
    #[serde(default)]
    pub max_requests: Option<u64>,
}

fn default_bind_host() -> String {
    "127.0.0.1".to_string()
}

fn default_base_port() -> u16 {
    19_001
}

impl Config {
    /// Parse configuration from TOML text.
    pub fn from_toml(text: &str) -> Result<Self> {
        Ok(toml::from_str(text)?)
    }

    /// Read and parse a configuration file.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml(&text)
    }

    /// Reject configurations that cannot work, with actionable messages.
    pub fn validate(&self) -> Result<()> {
        if self.server.http.is_none() && self.server.fastcgi.is_none() {
            return Err(Error::Config(
                "no listener configured: set server.http and/or server.fastcgi".to_string(),
            ));
        }

        if let Some(http) = &self.server.http {
            parse_listener(http).map_err(|e| Error::Config(format!("server.http: {e}")))?;
        }
        if let Some(fastcgi) = &self.server.fastcgi {
            parse_listener(fastcgi).map_err(|e| Error::Config(format!("server.fastcgi: {e}")))?;
        }

        for (name, backend) in &self.backends {
            self.validate_backend(name, backend)?;
        }

        if self.server.static_files.enabled && self.server.http.is_none() {
            return Err(Error::Config(
                "server.static_files is enabled but no server.http listener is configured; \
                 static content is served over HTTP, and a FastCGI client does its own"
                    .to_string(),
            ));
        }
        if self
            .server
            .document_root
            .as_ref()
            .is_some_and(|root| root.as_os_str().is_empty())
        {
            return Err(Error::Config(
                "server.document_root is empty; set a directory or remove the key".to_string(),
            ));
        }
        if self
            .server
            .front_controller
            .as_ref()
            .is_some_and(|front| front.trim().is_empty())
        {
            return Err(Error::Config(
                "server.front_controller is empty, which is not how \"serve only real files\" is \
                 written here — TOML has no way to tell an empty string from an absent key, so \
                 the meaning is reserved. Remove the key to get the default, or use \
                 `stokehold serve --front-controller ''` to mean \"only real files\"."
                    .to_string(),
            ));
        }
        if self.server.front_controller.as_ref().is_some_and(|front| {
            let front = front.trim_start();
            front.starts_with('/')
                || front.contains('\\')
                || Path::new(front)
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
        }) {
            return Err(Error::Config(
                "server.front_controller must be a relative path below the document root, e.g. \
                 \"index.php\" or \"public/index.php\" — an absolute path, a `..` component or a \
                 backslash would escape it"
                    .to_string(),
            ));
        }
        if self.server.static_files.cache_control.trim().is_empty() {
            return Err(Error::Config(
                "server.static_files.cache_control is empty; set a value or remove the key"
                    .to_string(),
            ));
        }
        if self.server.static_files.max_multirange_bytes == 0 {
            return Err(Error::Config(
                "server.static_files.max_multirange_bytes is zero, which refuses every \
                 multi-range request; set a value or remove the key"
                    .to_string(),
            ));
        }

        match &self.default_backend {
            None => {
                if !self.backends.is_empty() {
                    return Err(Error::Config(
                        "backends are defined but default_backend is not set".to_string(),
                    ));
                }
            }
            Some(name) if !self.backends.contains_key(name) => {
                return Err(Error::Config(format!(
                    "default_backend {name:?} does not match any configured backend"
                )));
            }
            Some(_) => {}
        }

        for (index, route) in self.routes.iter().enumerate() {
            if !self.backends.contains_key(&route.backend) {
                return Err(Error::Config(format!(
                    "routes[{index}].backend {:?} does not match any configured backend",
                    route.backend
                )));
            }
            if route.path_prefix.is_none() && route.host.is_none() {
                return Err(Error::Config(format!(
                    "routes[{index}] has neither path_prefix nor host, so it would match every \
                     request; set default_backend instead"
                )));
            }
            if let Some(prefix) = &route.path_prefix {
                if !prefix.starts_with('/') {
                    return Err(Error::Config(format!(
                        "routes[{index}].path_prefix {prefix:?} must start with '/'"
                    )));
                }
            }
            if let Some(host) = &route.host {
                if host.is_empty() || host.contains(':') {
                    return Err(Error::Config(format!(
                        "routes[{index}].host {host:?} must be a bare host name without a port"
                    )));
                }
            }
        }

        Ok(())
    }

    fn validate_backend(&self, name: &str, backend: &BackendConfig) -> Result<()> {
        let bad = |msg: String| Err(Error::Config(format!("backends.{name}: {msg}")));
        match backend {
            BackendConfig::Worker(w) => {
                if w.command.trim().is_empty() {
                    return bad("worker command must not be empty".to_string());
                }
            }
            BackendConfig::External(e) => {
                parse_listener(&e.socket)
                    .map_err(|err| Error::Config(format!("backends.{name}.socket: {err}")))?;
            }
            BackendConfig::SpawnFpm(f) => {
                if let Some(socket) = &f.socket {
                    parse_listener(socket)
                        .map_err(|err| Error::Config(format!("backends.{name}.socket: {err}")))?;
                }
            }
            BackendConfig::SpawnCgi(c) => {
                if let Some(socket) = &c.socket {
                    validate_cgi_socket(socket)
                        .map_err(|err| Error::Config(format!("backends.{name}.socket: {err}")))?;
                }
                if c.bind_host != "127.0.0.1" && c.bind_host != "::1" {
                    return bad(format!(
                        "bind_host {:?} is not loopback; php-cgi should never be exposed",
                        c.bind_host
                    ));
                }
            }
        }
        Ok(())
    }
}

/// A parsed listener endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listener {
    /// `host:port`
    Tcp(SocketAddr),
    /// `unix:/path/to.sock`
    Unix(PathBuf),
}

/// The longest `sun_path` a unix socket may bind, in bytes.
///
/// `sockaddr_un` is 110 bytes on Linux with a 2-byte `sun_family`, so 108 usable
/// for the path including its NUL terminator; 107 is the portable maximum. The
/// kernel truncates anything longer rather than failing, which would bind a path
/// nobody asked for, so the check is explicit.
pub const UNIX_SOCKET_PATH_MAX: usize = 107;

/// Validate a `spawn_cgi` `socket` prefix.
///
/// The value is a prefix rather than a finished path because the pool appends
/// `.<slot>.sock` — see [`SpawnCgiConfig::socket`]. The per-slot suffix is
/// accounted for here so a prefix that only fits because of a low slot number is
/// rejected at startup rather than at the twelfth worker.
pub fn validate_cgi_socket(spec: &str) -> std::result::Result<(), String> {
    let path = spec.strip_prefix("unix:").ok_or_else(|| {
        format!("{spec:?} must start with `unix:`, e.g. `unix:/run/stokehold/cgi`")
    })?;
    if path.is_empty() {
        return Err("unix socket path is empty".to_string());
    }
    if !path.starts_with('/') {
        return Err(format!(
            "unix socket path {path:?} must be absolute; php-cgi's working directory is not ours"
        ));
    }
    if path.contains('\0') {
        return Err("unix socket path contains a NUL byte".to_string());
    }
    if cfg!(not(unix)) {
        return Err(format!(
            "unix sockets are not available on this platform, so {spec:?} cannot be used; \
             php-cgi's `-b` binds a named pipe here, which this backend does not drive — \
             drop `socket` to use loopback TCP"
        ));
    }
    // `.<slot>.sock`: a dot, up to 10 digits of `u32`, then ".sock".
    let worst_case = path.len() + 1 + 10 + 5;
    if worst_case > UNIX_SOCKET_PATH_MAX {
        return Err(format!(
            "unix socket path {path:?} is too long: with the per-slot suffix a worker would need \
             up to {worst_case} bytes and the platform allows {UNIX_SOCKET_PATH_MAX}"
        ));
    }
    Ok(())
}

/// The socket path slot `index` binds, given a `unix:` prefix.
pub fn cgi_socket_path(prefix: &str, index: u32) -> PathBuf {
    let base = prefix.strip_prefix("unix:").unwrap_or(prefix);
    PathBuf::from(format!("{base}.{index}.sock"))
}

/// Parse a listener endpoint from either `host:port` or `unix:/path`.
pub fn parse_listener(spec: &str) -> std::result::Result<Listener, String> {
    let spec = spec.trim();
    if let Some(path) = spec.strip_prefix("unix:") {
        if path.is_empty() {
            return Err("unix socket path is empty".to_string());
        }
        return Ok(Listener::Unix(PathBuf::from(path)));
    }
    if spec.is_empty() {
        return Err("listener is empty".to_string());
    }
    spec.parse::<SocketAddr>()
        .map(Listener::Tcp)
        .map_err(|e| format!("{spec:?} is neither host:port nor unix:/path ({e})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_complete_config() {
        let text = r#"
default_backend = "fpm"

[server]
http = "127.0.0.1:8080"
fastcgi = "unix:/run/stokehold.sock"

[backends.fpm]
kind = "external"
socket = "unix:/run/php/php8.3-fpm.sock"

[backends.win]
kind = "spawn_cgi"
binary = 'C:\php-8.3.31\php-cgi.exe'
workers = 8

[backends.app]
kind = "worker"
command = "php worker.php"
memory_limit_mb = 256
"#;
        let config = Config::from_toml(text).expect("config parses");
        config.validate().expect("config validates");

        assert_eq!(config.default_backend.as_deref(), Some("fpm"));
        assert_eq!(config.backends.len(), 3);

        let fpm = &config.backends["fpm"];
        assert_eq!(fpm.kind(), "external");
        assert!(fpm.resets_per_request());

        let app = &config.backends["app"];
        assert_eq!(app.kind(), "worker");
        assert!(!app.resets_per_request());

        match &config.backends["win"] {
            BackendConfig::SpawnCgi(cgi) => {
                assert_eq!(cgi.workers, 8);
                assert_eq!(cgi.base_port, 19_001);
                assert_eq!(cgi.bind_host, "127.0.0.1");
            }
            other => panic!("expected spawn_cgi, got {other:?}"),
        }
    }

    #[test]
    fn rejects_config_without_a_listener() {
        let config = Config::from_toml("").expect("empty config parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("no listener configured"), "{err}");
    }

    #[test]
    fn rejects_unknown_backend_as_default() {
        let text = r#"
default_backend = "nope"
[server]
http = "127.0.0.1:8080"
"#;
        let config = Config::from_toml(text).expect("config parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("default_backend"), "{err}");
    }

    #[test]
    fn rejects_non_loopback_php_cgi_bind() {
        let text = r#"
default_backend = "win"
[server]
http = "127.0.0.1:8080"
[backends.win]
kind = "spawn_cgi"
binary = "php-cgi"
bind_host = "0.0.0.0"
"#;
        let config = Config::from_toml(text).expect("config parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("loopback"), "{err}");
    }

    #[test]
    fn parses_listener_forms() {
        assert_eq!(
            parse_listener("127.0.0.1:9000"),
            Ok(Listener::Tcp("127.0.0.1:9000".parse().unwrap()))
        );
        assert_eq!(
            parse_listener("unix:/run/php.sock"),
            Ok(Listener::Unix(PathBuf::from("/run/php.sock")))
        );
        assert!(parse_listener("unix:").is_err());
        assert!(parse_listener("").is_err());
        assert!(parse_listener("nonsense").is_err());
    }

    #[test]
    fn parses_a_spawn_cgi_unix_socket() {
        let text = r#"
default_backend = "cgi"
[server]
http = "127.0.0.1:8080"
[backends.cgi]
kind = "spawn_cgi"
binary = "php-cgi"
socket = "unix:/run/stokehold/cgi"
"#;
        let config = Config::from_toml(text).expect("config parses");
        match &config.backends["cgi"] {
            BackendConfig::SpawnCgi(cgi) => {
                assert_eq!(cgi.socket.as_deref(), Some("unix:/run/stokehold/cgi"));
                // Defaults still present, so a TCP fallback stays expressible.
                assert_eq!(cgi.base_port, 19_001);
            }
            other => panic!("expected spawn_cgi, got {other:?}"),
        }
    }

    #[test]
    fn a_cgi_socket_prefix_is_validated() {
        // On Windows the platform check fires first; elsewhere these are the path
        // checks, so assert only on what must hold everywhere.
        let err = validate_cgi_socket("/run/stokehold/cgi").expect_err("no unix: prefix");
        assert!(err.contains("unix:"), "{err}");

        let err = validate_cgi_socket("unix:").expect_err("empty path");
        assert!(err.contains("empty"), "{err}");

        let err = validate_cgi_socket("unix:relative/cgi").expect_err("relative path");
        assert!(err.contains("absolute"), "{err}");

        if cfg!(unix) {
            validate_cgi_socket("unix:/run/stokehold/cgi").expect("a plain absolute path is valid");
            // A path that looks finished is still just a prefix, which is the
            // documented contract — it binds `/run/php.sock.0.sock`.
            validate_cgi_socket("unix:/run/php.sock").expect("a finished-looking path is a prefix");
        }
    }

    #[test]
    fn a_socket_path_that_only_fits_before_the_suffix_is_rejected() {
        // 107 is the portable `sun_path` maximum; the check must account for the
        // `.<slot>.sock` the pool appends, or slot 0 binds and slot 11 fails.
        let fits = format!("unix:{}", "a".repeat(UNIX_SOCKET_PATH_MAX - "..sock".len() - 1));
        let too_long = format!("unix:{}", "a".repeat(UNIX_SOCKET_PATH_MAX));
        if cfg!(unix) {
            validate_cgi_socket(&fits).expect("a path with room for the suffix is accepted");
            assert!(
                validate_cgi_socket(&too_long).is_err(),
                "a path that leaves no room for the suffix must be rejected"
            );
        }
    }

    #[test]
    fn cgi_socket_path_appends_the_slot() {
        assert_eq!(
            cgi_socket_path("unix:/run/stokehold/cgi", 0),
            PathBuf::from("/run/stokehold/cgi.0.sock")
        );
        assert_eq!(
            cgi_socket_path("unix:/run/stokehold/cgi", 11),
            PathBuf::from("/run/stokehold/cgi.11.sock")
        );
    }

    fn routed_config() -> Config {
        let text = r#"
default_backend = "app"

[server]
http = "127.0.0.1:8080"

[backends.app]
kind = "worker"
command = "php worker.php"

[backends.legacy]
kind = "worker"
command = "php legacy.php"

[[routes]]
path_prefix = "/api"
backend = "legacy"

[[routes]]
host = "admin.example"
path_prefix = "/"
backend = "legacy"
"#;
        let config = Config::from_toml(text).expect("config parses");
        config.validate().expect("config validates");
        config
    }

    #[test]
    fn a_route_selects_a_backend_by_path_prefix() {
        let config = routed_config();
        assert_eq!(config.route_backend(None, "/api/users"), Some("legacy"));
        assert_eq!(config.route_backend(None, "/other"), None);
        // A prefix match is textual, matching how nginx `location` behaves.
        assert_eq!(config.route_backend(None, "/apix"), Some("legacy"));
    }

    #[test]
    fn a_host_route_ignores_the_port_and_the_case() {
        let config = routed_config();
        assert_eq!(
            config.route_backend(Some("Admin.Example:8080"), "/"),
            Some("legacy")
        );
        assert_eq!(
            config.route_backend(Some("admin.example"), "/x"),
            Some("legacy")
        );
        assert_eq!(config.route_backend(Some("public.example"), "/x"), None);
    }

    #[test]
    fn the_most_specific_matching_route_wins() {
        // Both routes match `admin.example/api`; the one that states two
        // constraints beats the one that states a path alone.
        let text = r#"
default_backend = "app"

[server]
http = "127.0.0.1:8080"

[backends.app]
kind = "worker"
command = "php worker.php"

[backends.path_only]
kind = "worker"
command = "php path.php"

[backends.both]
kind = "worker"
command = "php both.php"

[[routes]]
path_prefix = "/api"
backend = "path_only"

[[routes]]
host = "admin.example"
path_prefix = "/"
backend = "both"
"#;
        let config = Config::from_toml(text).expect("config parses");
        config.validate().expect("config validates");
        assert_eq!(
            config.route_backend(Some("admin.example"), "/api/x"),
            Some("both")
        );
    }

    #[test]
    fn rejects_a_route_naming_an_unknown_backend() {
        let text = r#"
default_backend = "app"
[server]
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
[[routes]]
path_prefix = "/api"
backend = "missing"
"#;
        let config = Config::from_toml(text).expect("config parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("routes[0].backend"), "{err}");
    }

    #[test]
    fn rejects_a_route_that_matches_everything() {
        let text = r#"
default_backend = "app"
[server]
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
[[routes]]
backend = "app"
"#;
        let config = Config::from_toml(text).expect("config parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("neither"), "{err}");
    }

    #[test]
    fn rejects_a_path_prefix_without_a_leading_slash() {
        let text = r#"
default_backend = "app"
[server]
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
[[routes]]
path_prefix = "api"
backend = "app"
"#;
        let config = Config::from_toml(text).expect("config parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("must start with"), "{err}");
    }

    #[test]
    fn static_files_are_off_unless_asked_for() {
        let config =
            Config::from_toml("[server]\nhttp = \"127.0.0.1:8080\"\n").expect("config parses");
        config.validate().expect("a listener alone is enough");
        assert!(
            !config.server.static_files.enabled,
            "an upgrade must not start serving files an operator never asked for"
        );
        // The defaults are still populated, so enabling it needs one key.
        assert_eq!(config.server.static_files.dotfiles, Dotfiles::Deny);
        assert!(config.server.static_files.directory_index);
        assert_eq!(config.server.static_files.max_multirange_bytes, 1024 * 1024);
    }

    #[test]
    fn parses_a_static_section() {
        let text = r#"
[server]
http = "127.0.0.1:8080"
[server.static_files]
enabled = true
precompressed = true
directory_index = false
dotfiles = "allow"
cache_control = "public, max-age=31536000, immutable"
max_multirange_bytes = 4096
"#;
        let config = Config::from_toml(text).expect("config parses");
        config.validate().expect("config validates");

        let statics = &config.server.static_files;
        assert!(statics.enabled);
        assert!(statics.precompressed);
        assert!(!statics.directory_index);
        assert_eq!(statics.dotfiles, Dotfiles::Allow);
        assert_eq!(statics.cache_control, "public, max-age=31536000, immutable");
        assert_eq!(statics.max_multirange_bytes, 4096);
    }

    #[test]
    fn rejects_static_content_with_nowhere_to_serve_it() {
        let text = r#"
[server]
fastcgi = "127.0.0.1:9000"
[server.static_files]
enabled = true
"#;
        let config = Config::from_toml(text).expect("config parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("no server.http"), "{err}");
    }

    #[test]
    fn rejects_an_empty_cache_control_and_a_zero_multirange_limit() {
        let text = r#"
[server]
http = "127.0.0.1:8080"
[server.static_files]
enabled = true
cache_control = "  "
"#;
        let config = Config::from_toml(text).expect("config parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("cache_control"), "{err}");

        let text = r#"
[server]
http = "127.0.0.1:8080"
[server.static_files]
enabled = true
max_multirange_bytes = 0
"#;
        let config = Config::from_toml(text).expect("config parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("max_multirange_bytes"), "{err}");
    }

    #[test]
    fn an_unknown_static_key_is_rejected_rather_than_ignored() {
        let text = r#"
[server]
http = "127.0.0.1:8080"
[server.static_files]
enable = true
"#;
        // A typo that silently leaves static serving off would be a confusing
        // bug to diagnose from the outside, which is why the section denies
        // unknown fields like the rest of the model.
        assert!(Config::from_toml(text).is_err());
    }

    #[test]
    fn a_deployment_can_be_described_entirely_in_the_file() {
        // The point of these keys: a systemd unit or a container image should not
        // have to carry a command line to say where the application lives.
        let text = r#"
default_backend = "app"
[server]
http = "127.0.0.1:8080"
document_root = "./public"
front_controller = "public/index.php"
[backends.app]
kind = "worker"
command = "php worker.php"
"#;
        let config = Config::from_toml(text).expect("config parses");
        config.validate().expect("config validates");
        assert_eq!(
            config.server.document_root.as_deref(),
            Some(Path::new("./public"))
        );
        assert_eq!(
            config.server.resolve_front_controller(None).as_deref(),
            Some("public/index.php")
        );
    }

    #[test]
    fn the_command_line_front_controller_wins_over_the_file() {
        let text = r#"
[server]
http = "127.0.0.1:8080"
front_controller = "index.php"
"#;
        let config = Config::from_toml(text).expect("config parses");

        assert_eq!(
            config
                .server
                .resolve_front_controller(Some("app.php"))
                .as_deref(),
            Some("app.php")
        );
        // An empty flag is not "unset": it is the operator asking for files only.
        assert_eq!(config.server.resolve_front_controller(Some("   ")), None);
    }

    #[test]
    fn the_front_controller_defaults_to_index_php() {
        let text = r#"
[server]
http = "127.0.0.1:8080"
"#;
        let config = Config::from_toml(text).expect("config parses");
        assert_eq!(
            config.server.resolve_front_controller(None).as_deref(),
            Some("index.php")
        );
    }

    #[test]
    fn rejects_an_empty_config_file_paths_and_a_front_controller_above_the_root() {
        // An empty string in TOML is indistinguishable from an absent key, so
        // "only real .php files" must be said on the command line, not here.
        let text = r#"
[server]
http = "127.0.0.1:8080"
front_controller = ""
"#;
        let config = Config::from_toml(text).expect("config parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("front_controller"), "{err}");

        let text = r#"
[server]
http = "127.0.0.1:8080"
document_root = ""
"#;
        let config = Config::from_toml(text).expect("config parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("document_root"), "{err}");

        for front in ["/etc/passwd", "../index.php", "a\\b.php"] {
            let text =
                format!("[server]\nhttp = \"127.0.0.1:8080\"\nfront_controller = {front:?}\n");
            let config = Config::from_toml(&text).expect("config parses");
            let err = config.validate().expect_err("must fail");
            assert!(
                err.to_string().contains("below the document root"),
                "{front}: {err}"
            );
        }
    }
}
