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

    /// Sites, by name. Empty means one site: what `[server]` describes.
    ///
    /// A site exists because a `host` in the route table is matched *after*
    /// `SCRIPT_FILENAME` is resolved, so a route can choose a backend but never
    /// a document root. Hosting two applications in one process therefore needs
    /// the decision made earlier, and that is what this table is for.
    #[serde(default)]
    pub sites: BTreeMap<String, SiteConfig>,
}

/// One site: a document root, and optionally its own routing and static layer.
///
/// Every key except `hosts` and `document_root` falls back to the `[server]`
/// value, so a site that only sets those two inherits the rest — the usual
/// shape when several sites share one application.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SiteConfig {
    /// The `Host` names this site answers to.
    ///
    /// An exact name, case-insensitively, with any `:port` ignored — or a
    /// leading `*.` wildcard, which matches one or more labels (`*.example.com`
    /// matches `a.example.com` and `a.b.example.com`).
    pub hosts: Vec<String>,

    /// Directory that this site's scripts and files resolve against.
    ///
    /// Absolutised at startup for the same reason `server.document_root` is:
    /// `php-cgi` runs in a persistent pool and `chdir()`s into the directory of
    /// the script it executed, so a relative root resolves against the previous
    /// request's directory from the second request onwards.
    pub document_root: PathBuf,

    /// This site's front controller; see [`ServerConfig::front_controller`].
    #[serde(default)]
    pub front_controller: Option<FrontController>,

    /// Backend used when a request to this site matches no route.
    #[serde(default)]
    pub default_backend: Option<String>,

    /// This site's route table, replacing the global one when present.
    #[serde(default)]
    pub routes: Option<Vec<RouteConfig>>,

    /// This site's static layer. Absent inherits the `[server]` one.
    #[serde(default)]
    pub static_files: Option<StaticsSetting>,

    /// What this site answers with; see [`ServeOnly`].
    ///
    /// Absent inherits the `[server]` value, which itself defaults to
    /// `ServeOnly::All`.
    #[serde(default)]
    pub serve_only: Option<ServeOnly>,
}

/// What a deployment serves.
///
/// The name is the rule: *serve only* this. Absent means "serve everything",
/// which is static files first and then a backend — the behaviour that existed
/// before this key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServeOnly {
    /// Static files first, then the backend. The default.
    #[default]
    All,
    /// The backend only: a path naming a file is dispatched like any other.
    Backend,
    /// Files only: a path naming no file is a 404, and no backend is consulted.
    Static,
}

impl ServeOnly {
    /// Whether the static layer runs for this site.
    pub fn serves_files(self) -> bool {
        !matches!(self, ServeOnly::Backend)
    }

    /// Whether a path that names no file may reach a backend.
    pub fn serves_backend(self) -> bool {
        !matches!(self, ServeOnly::Static)
    }

    /// The name as it is written in configuration, for logs and diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            ServeOnly::All => "all",
            ServeOnly::Backend => "backend",
            ServeOnly::Static => "static",
        }
    }
}

impl std::fmt::Display for ServeOnly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A front controller as configuration spells it.
///
/// A string is the script's path below the document root. `false` means "serve
/// only real `.php` files", which the global key can only express as an empty
/// string on the command line — TOML cannot tell `front_controller = ""` from an
/// absent key, so the file reserves the meaning and a *flag* supplies it. A site
/// has no flag of its own, so it needs a spelling, and a boolean is one that
/// cannot be confused with a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FrontController {
    /// A relative path below the document root, e.g. `index.php`.
    Path(String),
    /// `false`: no front controller, so only real scripts are served.
    FilesOnly(bool),
}

impl FrontController {
    /// The controller as a path, or `None` for "serve only real files".
    pub fn as_path(&self) -> Option<&str> {
        match self {
            FrontController::Path(path) => Some(path.as_str()),
            FrontController::FilesOnly(_) => None,
        }
    }
}

/// A static-file setting as configuration spells it.
///
/// A table tunes the layer; `false` turns it off and `true` turns it on with the
/// defaults. The bare boolean exists because "off" is otherwise
/// `static_files = { enabled = false }`, and the short form is what a reader
/// reaches for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StaticsSetting {
    /// The key was not written. On, by default — see `Default` — but *not* an explicit
    /// request, which is what lets a FastCGI-only deployment leave it out and still validate.
    Unspecified,
    /// A tuning table, naming only the keys it changes.
    Table(StaticOverrides),
    /// `false` disables the layer; `true` enables it with the defaults.
    Switch(bool),
}

impl Default for StaticsSetting {
    /// Absent means the layer is **on**, with the defaults.
    ///
    /// It used to be off, so that upgrading did not change an existing deployment: a path
    /// that had reached the front controller still did. There are no deployments yet, and
    /// the cost of the old default was that every new site had to be told to serve its own
    /// assets. Turning it on only changes the outcome for a path that names a real,
    /// non-script file — `/about` still reaches the front controller — so a front-controller
    /// application is unaffected either way.
    ///
    /// `Unspecified` rather than `Switch(true)`, because the two have to be told apart:
    /// static content is served over HTTP only, so *asking* for it with no HTTP listener is a
    /// mistake worth reporting, while never mentioning it is not asking for anything. A
    /// FastCGI-only deployment that leaves the key out must still validate.
    fn default() -> Self {
        StaticsSetting::Unspecified
    }
}

impl StaticsSetting {
    /// Whether a deployment actually asked for the static layer, as opposed to getting it by
    /// default. Only an explicit request can be contradicted by the rest of the file.
    pub fn is_explicit(&self) -> bool {
        !matches!(self, StaticsSetting::Unspecified)
    }

    /// Whether the key is present and says "on".
    pub fn asks_for_statics(&self) -> bool {
        match self {
            StaticsSetting::Unspecified => false,
            StaticsSetting::Switch(enabled) => *enabled,
            StaticsSetting::Table(overrides) => overrides.enabled == Some(true),
        }
    }

    /// The setting applied over `base`.
    ///
    /// A site resolves its static layer against the server's rather than against
    /// nothing, so a site naming one key keeps the root's other settings — an
    /// application that overrides `precompressed` must not silently reset
    /// `cache_control` to the default for its whole document root.
    pub fn applied_to(&self, base: &StaticConfig) -> StaticConfig {
        match self {
            // The key was not written, so the layer is the one already in effect. That the
            // *default* is on is expressed by `ServerConfig::statics` seeding the base with
            // `enabled: true`, not by this arm — which is what keeps "absent" and "written
            // true" distinguishable for validation.
            StaticsSetting::Unspecified => base.clone(),
            StaticsSetting::Table(overrides) => overrides.apply(base),
            // A bare boolean says one thing about `enabled` and nothing about the
            // rest, so the rest is inherited — which is what makes
            // `static_files = false` a usable opt-out for a site whose root serves
            // files with settings worth keeping.
            StaticsSetting::Switch(enabled) => StaticConfig {
                enabled: *enabled,
                ..base.clone()
            },
        }
    }

    /// The setting applied over the defaults, which is what a deployment with no
    /// inherited value gets.
    pub fn resolved(&self) -> StaticConfig {
        self.applied_to(&StaticConfig::default())
    }
}

/// The keys a `static_files` table states, each optional.
///
/// `Option` per key rather than a [`StaticConfig`] with defaults, because "did not
/// mention it" and "mentioned it and chose the default" are different answers when
/// the table is an override. Without that distinction a site setting
/// `precompressed = true` would also assert `enabled = false` and
/// `cache_control = "…"` — the defaults — over whatever the server had said.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticOverrides {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub precompressed: Option<bool>,
    #[serde(default)]
    pub directory_index: Option<bool>,
    #[serde(default)]
    pub dotfiles: Option<Dotfiles>,
    #[serde(default)]
    pub cache_control: Option<String>,
    #[serde(default)]
    pub max_multirange_bytes: Option<u64>,
}

impl StaticOverrides {
    /// The base settings with these overrides applied.
    ///
    /// `enabled` is the field whose default differs from [`StaticOverrides::default`]: a
    /// site that tunes only `precompressed` gets the layer it was already getting, while
    /// `static_files = false` is one key that says only "off". Both come out of this one
    /// function so the two spellings cannot disagree.
    pub fn apply(&self, base: &StaticConfig) -> StaticConfig {
        StaticConfig {
            enabled: self.enabled.unwrap_or(base.enabled),
            precompressed: self.precompressed.unwrap_or(base.precompressed),
            directory_index: self.directory_index.unwrap_or(base.directory_index),
            dotfiles: self.dotfiles.unwrap_or(base.dotfiles),
            cache_control: self
                .cache_control
                .clone()
                .unwrap_or_else(|| base.cache_control.clone()),
            max_multirange_bytes: self
                .max_multirange_bytes
                .unwrap_or(base.max_multirange_bytes),
        }
    }
}

/// A route entry, selecting a backend for matching requests.
///
/// Both matchers are optional, but at least one must be present — a route with
/// neither is just `default_backend` written the long way, and it would make the
/// precedence rule meaningless. Constraints within one route are **all** of
/// ("and"), not any of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    /// Match a request whose path starts with this string, e.g. `/api`.
    #[serde(default)]
    pub path_prefix: Option<String>,

    /// Match a request whose path ends with this string, e.g. `.php`.
    ///
    /// Compared case-insensitively, deliberately: the router decides what a
    /// script is case-insensitively, so a case-sensitive suffix here would let
    /// `/upload.PHP` be recognised as a script by one layer and miss its route in
    /// the other. Two answers to "is this PHP" is the drift the static layer
    /// already guards against by sharing `has_php_extension`.
    #[serde(default)]
    pub path_suffix: Option<String>,

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
        self.host.is_some() as usize
            + self.path_prefix.is_some() as usize
            + self.path_suffix.is_some() as usize
    }

    /// Whether this route's host constraint is an exact name rather than a
    /// wildcard.
    ///
    /// An exact host is the more specific claim about the same request, so it wins
    /// a tie against a wildcard that also matches — see [`Config::route_backend`].
    pub fn has_exact_host(&self) -> bool {
        self.host
            .as_deref()
            .is_some_and(|host| !host.starts_with("*."))
    }

    /// Whether this route's constraints all match the given host and path.
    pub fn matches(&self, host: Option<&str>, path: &str) -> bool {
        if let Some(wanted) = &self.path_prefix {
            if !path.starts_with(wanted.as_str()) {
                return false;
            }
        }
        if let Some(wanted) = &self.path_suffix {
            // Byte-wise and case-insensitive, like `has_php_extension`, rather than
            // `path.to_ascii_lowercase().ends_with(...)`, which would allocate a copy
            // of the whole path per request to answer a question about its last few
            // bytes. The comparison is on bytes because a byte offset taken from the
            // end of a `&str` can land inside a multi-byte character and slicing
            // there panics, so a path with non-ASCII in it must still be answerable.
            let wanted = wanted.as_bytes();
            let bytes = path.as_bytes();
            if bytes.len() < wanted.len()
                || !bytes[bytes.len() - wanted.len()..].eq_ignore_ascii_case(wanted)
            {
                return false;
            }
        }
        if let Some(wanted) = &self.host {
            if !host_matches(wanted, host) {
                return false;
            }
        }
        true
    }
}

/// Whether a `Host` header matches a configured host pattern.
///
/// One function for the route table and the site table, so the two cannot
/// disagree about what a wildcard means — a route that matched a host the site
/// resolver did not, or the reverse, would be a bug nobody could reproduce from
/// reading either table alone.
///
/// The pattern is an exact name, compared case-insensitively with any `:port`
/// stripped, or a leading `*.` wildcard. `*.example.com` matches
/// `a.example.com` and `a.b.example.com` but *not* `example.com` itself: a
/// wildcard names a subdomain, and treating it as also matching the bare domain
/// would silently widen every rule that used one.
pub fn host_matches(pattern: &str, host: Option<&str>) -> bool {
    let host = host.unwrap_or_default();
    let host = strip_port(host);
    if host.is_empty() {
        return false;
    }

    match pattern.strip_prefix("*.") {
        None => host.eq_ignore_ascii_case(pattern),
        Some(suffix) => {
            // The host must be longer than the suffix *plus* the separating dot, and
            // that dot must be there. Both halves are needed: without the length
            // check, `.example.com` would pass — it is exactly one byte longer than
            // the suffix and that byte is a dot — and the wildcard would be claimed
            // by an empty label.
            let suffix_len = suffix.len();
            host.len() > suffix_len + 1
                && host.as_bytes()[host.len() - suffix_len - 1] == b'.'
                && host[host.len() - suffix_len..].eq_ignore_ascii_case(suffix)
        }
    }
}

/// Pick a backend from a route table, or `None` when nothing matches.
///
/// Free rather than a method on [`Config`] because a site carries its own table, and
/// the two must be selected by the same rule — a site whose routes were matched even
/// slightly differently from the server's would be a bug nobody could reproduce from
/// reading either table alone.
pub fn select_backend<'a>(
    routes: &'a [RouteConfig],
    host: Option<&str>,
    path: &str,
) -> Option<&'a str> {
    select_route(routes, host, path).map(|route| route.backend.as_str())
}

/// [`select_backend`], keeping the route so its other properties are available.
pub fn select_route<'a>(
    routes: &'a [RouteConfig],
    host: Option<&str>,
    path: &str,
) -> Option<&'a RouteConfig> {
    routes
        .iter()
        .filter(|route| route.matches(host, path))
        .max_by_key(|route| (route.specificity(), route.has_exact_host()))
}

/// Whether a route proves its path names a script, so the caller need not `stat` it.
///
/// `php-cgi` checks that a script exists and answers `No input file specified` when it
/// does not, so *existence* is genuinely its business. What it does **not** do is check
/// that the file is PHP: handed `config.json` it emits it verbatim with a 200, which is
/// hazard 19. So the `stat` in the router is doing two jobs, and only one of them is
/// removable.
///
/// The removable one is removable exactly when the pattern already pins a script
/// extension: then the file can only be PHP, and its absence is PHP's own 404. That is a
/// static property of the configuration, so it is decided once at startup rather than
/// per request — and never written by hand, because the decision belongs with the code
/// that knows why it is safe.
pub fn skip_stat(route: &RouteConfig, backend_is_worker: bool) -> bool {
    // A worker never interprets the path: it reads `script_filename` as metadata and
    // routes on `REQUEST_URI`, so there is no file for the stat to protect.
    if backend_is_worker {
        return true;
    }
    // A suffix that pins a script extension, e.g. `.php`. Compared with the same
    // case-insensitive rule `has_php_extension` uses, so the two cannot disagree about
    // `/upload.PHP`.
    route
        .path_suffix
        .as_deref()
        .is_some_and(is_script_extension)
}

/// Whether a `path_suffix` names a script extension, and nothing but one.
///
/// `".php"` and `"/entry.php"` do; `".json"` and `".css"` do not. A prefix says nothing
/// about the extension at all, which is why only a suffix can make this claim.
fn is_script_extension(suffix: &str) -> bool {
    // The last `.`-separated part of the suffix is the extension it pins, so a suffix
    // carrying a path (`/api/entry.php`) is judged by its end rather than rejected.
    match suffix.rsplit_once('.') {
        Some((_, extension)) => {
            const SCRIPT_EXTENSIONS: [&str; 3] = ["php", "php3", "phtml"];
            SCRIPT_EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        }
        None => false,
    }
}

/// A `Host` header without its `:port`, handling a bracketed IPv6 literal.
fn strip_port(host: &str) -> &str {
    let host = host.trim();
    if let Some(rest) = host.strip_prefix('[') {
        // `[::1]:8080` and `[::1]`.
        return rest.split(']').next().unwrap_or(rest);
    }
    // A bare `::1` has several colons and no port; only the last one separates a
    // port when there is exactly one colon.
    match host.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') && port.chars().all(|c| c.is_ascii_digit()) => {
            name
        }
        _ => host,
    }
}

impl Config {
    /// The backend that should serve this request, if a route matches it.
    ///
    /// The most specific matching route wins, and ties are broken by declaration
    /// order so the earlier entry wins. "Most specific" is stated rather than
    /// weighted, because a table whose winner could not be predicted by reading
    /// the file would be worse than one that is merely simple:
    ///
    /// 1. A route stating more constraints beats one stating fewer, so
    ///    `host + path_suffix` beats a bare `path_suffix`.
    /// 2. Among routes stating the same number, an exact `host` beats a `*.`
    ///    wildcard, since an exact name is the more specific claim about the same
    ///    request.
    /// 3. Anything still tied is settled by declaration order.
    pub fn route_backend(&self, host: Option<&str>, path: &str) -> Option<&str> {
        select_backend(&self.routes, host, path)
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
    ///
    /// A table tunes the layer; `false` turns it off and `true` turns it on with the
    /// defaults. Off by default, so an existing deployment is not changed by
    /// upgrading.
    #[serde(default)]
    pub static_files: StaticsSetting,

    /// What this deployment serves; see [`ServeOnly`]. Defaults to everything.
    #[serde(default)]
    pub serve_only: ServeOnly,
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

    /// The static layer's settings, with any bare boolean resolved.
    ///
    /// The layer is **on** unless a deployment says otherwise, so an absent key resolves to
    /// `enabled = true`. Only a path that names a real, non-script file changes behaviour:
    /// a front-controller application answers every unmatched path exactly as it did when the
    /// default was off.
    pub fn statics(&self) -> StaticConfig {
        self.static_files.applied_to(&StaticConfig {
            enabled: true,
            ..StaticConfig::default()
        })
    }
}

/// How files under the document root are served.
///
/// The HTTP front-end only. A file goes to a backend when nothing here matches,
/// which is what keeps a front controller working: with `index.php` configured,
/// a path that names no file is PHP's business, not this layer's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// A backend is one of three shapes, tagged by `kind` in TOML.
///
/// There was a fourth, `spawn_fpm`, which stokehold would have started itself. It was
/// never implemented — `Dispatcher` refused it — while `stokehold version` advertised it
/// and the preflight check warned that it would not start, so the key looked supported
/// and could not work. It is gone rather than built: `docs/plan.md` §9 says not to
/// reimplement what php-fpm already does, and `external` attaches to the php-fpm that
/// systemd already supervises, which is the better version of the same feature.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackendConfig {
    /// Persistent PHP process speaking stokehold's own framed protocol over pipes.
    Worker(WorkerConfig),
    /// An already-running php-fpm (systemd-managed). Linux/macOS default.
    External(ExternalConfig),
    /// N `php-cgi` processes on N ports. The only option on Windows.
    SpawnCgi(SpawnCgiConfig),
}

impl BackendConfig {
    pub fn kind(&self) -> &'static str {
        match self {
            BackendConfig::Worker(_) => "worker",
            BackendConfig::External(_) => "external",
            BackendConfig::SpawnCgi(_) => "spawn_cgi",
        }
    }

    /// True when the backend resets state per request in PHP's own SAPI.
    pub fn resets_per_request(&self) -> bool {
        !matches!(self, BackendConfig::Worker(_))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

    /// Whether the per-frame `crc32c` is verified.
    ///
    /// Defaults to [`ChecksumMode::Required`], which is what every stokehold release
    /// before this option did, so a configuration that does not mention it is unchanged on
    /// the wire. Setting `optional` lets a worker that also agrees omit the checksum; the
    /// negotiation is in the handshake, so a worker that does not understand the offer
    /// simply keeps checksumming — see [`ChecksumMode`].
    #[serde(default)]
    pub checksum: ChecksumMode,

    /// How the server and a worker are connected.
    ///
    /// Defaults to the platform's better transport: [`WorkerTransport::Socket`] on Windows
    /// and [`WorkerTransport::Pipe`] everywhere else. The split is measured rather than
    /// assumed. PHP's stream layer takes **~8 ms median (15 ms p95)** to return from a
    /// blocking read on a Windows pipe, against **~0.09 ms** for the same read on a socket;
    /// on a request path that was **21× on the whole `/items` route** (15.24 ms vs 0.72 ms
    /// median, 9 runs of 1000 requests each). A Rust process reading the identical pipe
    /// returns in 0.010 ms, and RoadRunner — a different server with a different protocol —
    /// measured the same 15 ms, so the cost is PHP's pipe read on Windows rather than
    /// anything stokehold does. On Linux the ordering is the other way round and much
    /// smaller, so the pipe stays the default there. See `docs/plan.md` hazard 46.
    #[serde(default)]
    pub transport: WorkerTransport,
}

/// How a worker process is connected to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerTransport {
    /// The worker's own `stdin`/`stdout`. Portable, needs nothing from the worker, and
    /// the fastest option on Linux and macOS.
    Pipe,
    /// A loopback TCP socket, for Windows, where PHP's pipe reads are slow. The server
    /// listens on an ephemeral loopback port and passes it to the worker as
    /// `STOKEHOLD_WORKER_SOCKET`; the worker connects back. The transport is
    /// indistinguishable to the protocol, which is why nothing above this setting changes.
    Socket,
}

impl Default for WorkerTransport {
    /// Windows gets a socket, everything else gets a pipe.
    ///
    /// A hand-written `Default` rather than `#[default]` on a variant, because the better
    /// transport is a property of the platform and not of the enum. Getting this wrong is
    /// expensive in the quiet way: a Windows deployment that leaves the setting out used to
    /// pay 15 ms of scheduler latency per request and look healthy doing it — every request
    /// answered, every status 200, no log line anywhere.
    fn default() -> Self {
        if cfg!(windows) {
            WorkerTransport::Socket
        } else {
            WorkerTransport::Pipe
        }
    }
}

impl WorkerTransport {
    /// True when the transport is the worker's own stdio.
    pub fn is_pipe(self) -> bool {
        matches!(self, WorkerTransport::Pipe)
    }

    /// The name as it is written in configuration, for logs and diagnostics.
    ///
    /// Worth logging even though it is usually the default: the transport is invisible in
    /// the protocol, so a deployment that ends up on the wrong one behaves identically
    /// except for how fast it is.
    pub fn as_str(self) -> &'static str {
        match self {
            WorkerTransport::Pipe => "pipe",
            WorkerTransport::Socket => "socket",
        }
    }
}

impl std::fmt::Display for WorkerTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The environment variable carrying the address a socket-transport worker must connect to.
///
/// The server listens before starting the worker, so the address is known up front and the
/// worker is told rather than having to discover it.
pub const WORKER_SOCKET_ENV: &str = "STOKEHOLD_WORKER_SOCKET";

/// How strictly the worker protocol's per-frame `crc32c` is enforced.
///
/// A configuration-level mirror of the protocol's `Checksum` policy. It lives here
/// rather than importing the worker crate's enum so that the configuration model stays
/// free of the protocol crate, which is what lets `stokehold-core` be the leaf that every
/// other crate depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumMode {
    /// Every frame must carry a verified checksum, and one that claims otherwise is
    /// refused. The default, and the only mode an older worker can be driven in.
    #[default]
    Required,
    /// The handshake may agree to omit the checksum. A frame that still carries one is
    /// verified; only a frame that declares it has none is read without hashing it.
    Optional,
}

impl ChecksumMode {
    /// True when the checksum is enforced, for logging and tests.
    pub fn is_required(self) -> bool {
        matches!(self, ChecksumMode::Required)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalConfig {
    /// php-fpm listener: `unix:/run/php/php8.3-fpm.sock` or `127.0.0.1:9000`.
    pub socket: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

        // Static content is served over HTTP, and a FastCGI client does its own — so asking
        // for it with no HTTP listener is a mistake worth reporting. But *asking* is the
        // operative word: the layer is on unless a deployment says otherwise, and a
        // FastCGI-only config that never mentioned it is not asking for anything. Rejecting
        // that would make the default unshippable, so only an explicit request is refused —
        // which is why `StaticsSetting` has an `Unspecified` variant to tell them apart.
        let asked_for_statics = self.server.static_files.asks_for_statics()
            || self.sites.values().any(|site| {
                site.static_files
                    .as_ref()
                    .is_some_and(StaticsSetting::asks_for_statics)
            });
        if asked_for_statics && self.server.http.is_none() {
            return Err(Error::Config(
                "static_files is enabled but no server.http listener is configured; \
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
        if self.server.statics().cache_control.trim().is_empty() {
            return Err(Error::Config(
                "server.static_files.cache_control is empty; set a value or remove the key"
                    .to_string(),
            ));
        }
        if self.server.statics().max_multirange_bytes == 0 {
            return Err(Error::Config(
                "server.static_files.max_multirange_bytes is zero, which refuses every \
                 multi-range request; set a value or remove the key"
                    .to_string(),
            ));
        }
        // `serve_only = "backend"` and an enabled static layer are contradictory:
        // one says the static layer never runs, the other says it must. Which wins
        // would otherwise depend on which check read the config, so it is refused.
        if self.server.serve_only == ServeOnly::Backend && self.server.statics().enabled {
            return Err(Error::Config(
                "server.serve_only is \"backend\", which skips the static layer, but \
                 server.static_files is enabled; write one of them"
                    .to_string(),
            ));
        }

        match &self.default_backend {
            None => {
                // Backends without a `default_backend` is a typo: nothing could
                // select them. No backends at all is the static-only deployment,
                // which `Dispatcher` accepts and the CLI warns about.
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
            validate_route(route, "routes", index, &self.backends)?;
        }

        self.validate_sites()?;

        Ok(())
    }

    /// Validate the site table: host patterns, duplicates, and per-site settings.
    ///
    /// Separate from `validate` only because it is long; the two are one rule set
    /// and are always called together.
    fn validate_sites(&self) -> Result<()> {
        // Host patterns, and the duplicate check. A host claimed by two sites is
        // the one mistake a reader cannot see from either site alone: one of them
        // would silently never be reached, and which one depends on map order.
        let mut claimed: BTreeMap<String, &str> = BTreeMap::new();
        for (name, site) in &self.sites {
            if site.hosts.is_empty() {
                return Err(Error::Config(format!(
                    "sites.{name}.hosts is empty, so this site would never be reached; list at \
                     least one host name"
                )));
            }
            if site.document_root.as_os_str().is_empty() {
                return Err(Error::Config(format!(
                    "sites.{name}.document_root is empty; set the directory holding this site"
                )));
            }

            for host in &site.hosts {
                validate_host_pattern(host, &format!("sites.{name}.hosts"))?;
                let key = host.trim().to_ascii_lowercase();
                if let Some(existing) = claimed.insert(key, name) {
                    return Err(Error::Config(format!(
                        "sites.{existing} and sites.{name} both claim the host {host:?}; the first \
                         one declared would be the only one ever reached"
                    )));
                }
            }

            if let Some(front) = &site.front_controller {
                validate_front_controller(front, &format!("sites.{name}.front_controller"))?;
            }

            if let Some(routes) = &site.routes {
                for (index, route) in routes.iter().enumerate() {
                    validate_route(
                        route,
                        &format!("sites.{name}.routes"),
                        index,
                        &self.backends,
                    )?;
                }
            }

            if let Some(backend) = &site.default_backend {
                if !self.backends.contains_key(backend) {
                    return Err(Error::Config(format!(
                        "sites.{name}.default_backend {backend:?} does not match any configured \
                         backend"
                    )));
                }
            }

            if let Some(statics) = &site.static_files {
                let resolved = statics.applied_to(&self.server.statics());
                validate_statics(&resolved, &format!("sites.{name}.static_files"))?;
            }
            // `static_files = false` and `serve_only = "static"` are both a way of
            // saying "files only", so writing them together is a contradiction and
            // writing `false` beside `"backend"` is a double negative. Rejecting is
            // what keeps the resolved answer a single statement.
            if let Some(StaticsSetting::Switch(enabled)) = &site.static_files {
                match (enabled, site.serve_only) {
                    (true, Some(ServeOnly::Backend)) => {
                        return Err(Error::Config(format!(
                            "sites.{name}: static_files = true cannot be combined with \
                             serve_only = \"backend\", which skips the static layer"
                        )))
                    }
                    (false, Some(ServeOnly::Static)) => {
                        return Err(Error::Config(format!(
                            "sites.{name}: static_files = false cannot be combined with \
                             serve_only = \"static\"; write one of them"
                        )))
                    }
                    _ => {}
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

/// Validate one route, whichever table it came from.
///
/// `where_` is the table's name as a reader would write it (`routes` or
/// `sites.<name>.routes`), so a message names the entry that owns the mistake
/// rather than an index into something the reader cannot see.
fn validate_route(
    route: &RouteConfig,
    where_: &str,
    index: usize,
    backends: &BTreeMap<String, BackendConfig>,
) -> Result<()> {
    if !backends.contains_key(&route.backend) {
        return Err(Error::Config(format!(
            "{where_}[{index}].backend {:?} does not match any configured backend",
            route.backend
        )));
    }
    if route.path_prefix.is_none() && route.path_suffix.is_none() && route.host.is_none() {
        return Err(Error::Config(format!(
            "{where_}[{index}] has no path_prefix, path_suffix or host, so it would match every \
             request; set default_backend instead"
        )));
    }
    if let Some(prefix) = &route.path_prefix {
        if !prefix.starts_with('/') {
            return Err(Error::Config(format!(
                "{where_}[{index}].path_prefix {prefix:?} must start with '/'"
            )));
        }
    }
    if let Some(suffix) = &route.path_suffix {
        // A suffix that does not start with `.` or `/` matches far more than it
        // looks like it does — `"php"` matches `/notphp` — so it is refused rather
        // than left to be discovered as a routing surprise.
        if !(suffix.starts_with('.') || suffix.starts_with('/')) {
            return Err(Error::Config(format!(
                "{where_}[{index}].path_suffix {suffix:?} must start with '.' or '/', so that it \
                 cannot match an unrelated path by accident"
            )));
        }
    }
    if let Some(host) = &route.host {
        validate_host_pattern(host, &format!("{where_}[{index}].host"))?;
    }
    Ok(())
}

/// Validate a host pattern, as `*.example.com` or a bare name.
fn validate_host_pattern(pattern: &str, where_: &str) -> Result<()> {
    if pattern.is_empty() {
        return Err(Error::Config(format!(
            "{where_} is empty; name a host, or remove the route"
        )));
    }
    // The port is not part of the pattern: a `Host` header carries one, but the
    // name a site or route claims does not, and accepting one would make
    // `a.test:8080` and `a.test` two different sites that look the same.
    let name = pattern.strip_prefix("*.").unwrap_or(pattern);
    if name.is_empty() || name.contains(':') || name.contains('/') {
        return Err(Error::Config(format!(
            "{where_} {pattern:?} must be a bare host name without a port, optionally with a \
             leading \"*.\""
        )));
    }
    // `*.` must leave at least one label, or the wildcard claims the whole of a
    // top-level domain.
    if pattern.starts_with("*.") && !name.contains('.') {
        return Err(Error::Config(format!(
            "{where_} {pattern:?} would match every host under {name:?}; a wildcard needs at \
             least two labels after it, e.g. \"*.example.com\""
        )));
    }
    Ok(())
}

/// Validate a front controller, as a relative path below the document root.
fn validate_front_controller(controller: &FrontController, where_: &str) -> Result<()> {
    let front = match controller {
        // `false` means "only real files", which is what the key being absent
        // already means in a site table — so it is redundant but not wrong, and
        // saying it explicitly is how a reader checks the deployment's intent.
        FrontController::FilesOnly(false) => return Ok(()),
        // `true` is not a path and has no sensible reading: it would have to mean
        // some particular script, and guessing which one is exactly the ambiguity
        // the empty-string rule exists to avoid.
        FrontController::FilesOnly(true) => {
            return Err(Error::Config(format!(
                "{where_} = true is not a path; name the script, e.g. \"index.php\", or write \
                 `false` to serve only real files"
            )))
        }
        FrontController::Path(path) => path.as_str(),
    };
    if front.trim().is_empty() {
        return Err(Error::Config(format!(
            "{where_} is empty; write `false` to serve only real files, or name a script"
        )));
    }
    let front = front.trim_start();
    if front.starts_with('/')
        || front.contains('\\')
        || Path::new(front)
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(Error::Config(format!(
            "{where_} must be a relative path below the document root, e.g. \"index.php\" or \
             \"public/index.php\" — an absolute path, a `..` component or a backslash would \
             escape it, and `false` means \"only real files\""
        )));
    }
    Ok(())
}

/// Validate the resolvable parts of a static-file setting.
fn validate_statics(statics: &StaticConfig, where_: &str) -> Result<()> {
    if statics.cache_control.trim().is_empty() {
        return Err(Error::Config(format!(
            "{where_}.cache_control is empty; set a value or remove the key"
        )));
    }
    if statics.max_multirange_bytes == 0 {
        return Err(Error::Config(format!(
            "{where_}.max_multirange_bytes is zero, which refuses every multi-range request; \
             set a value or remove the key"
        )));
    }
    Ok(())
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
///
/// On Windows the same setting means a **named pipe** rather than a unix socket,
/// because that is what `php-cgi -b` binds there. A pipe name carries no path
/// length limit worth enforcing, so only the name itself is checked.
pub fn validate_cgi_socket(spec: &str) -> std::result::Result<(), String> {
    #[cfg(windows)]
    if let Some(name) = spec.strip_prefix(r"\\.\pipe\") {
        if name.is_empty() {
            return Err(format!("{spec:?} has an empty pipe name"));
        }
        if name.contains('\\') {
            return Err(format!(
                "pipe name {name:?} may not contain a backslash; a pipe's name is a single \
                 component, e.g. `\\\\\\\\.\\\\pipe\\\\stokehold-cgi`"
            ));
        }
        if spec.contains('\0') {
            return Err("pipe name contains a NUL byte".to_string());
        }
        return Ok(());
    }

    let path = spec.strip_prefix("unix:").ok_or_else(|| {
        #[cfg(windows)]
        return format!(
            "{spec:?} must start with `unix:` or `\\\\.\\pipe\\`, e.g. \
             `unix:/run/stokehold/cgi` or `\\\\.\\pipe\\stokehold-cgi`"
        );
        #[cfg(not(windows))]
        return format!("{spec:?} must start with `unix:`, e.g. `unix:/run/stokehold/cgi`");
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
            "unix sockets are not available on this platform, so {spec:?} cannot be used"
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

/// The socket or pipe name slot `index` binds, given a prefix.
///
/// A unix socket gets `.<index>.sock` appended because a path names a file. A
/// Windows pipe name is a namespace rather than a path, and `php-cgi`'s own
/// instances are already unlimited (`PIPE_UNLIMITED_INSTANCES`), so the slot's
/// name is qualified with the index instead, keeping one pipe per slot.
pub fn cgi_socket_path(prefix: &str, index: u32) -> PathBuf {
    #[cfg(windows)]
    if let Some(name) = prefix.strip_prefix(r"\\.\pipe\") {
        return PathBuf::from(format!(r"\\.\pipe\{name}.{index}"));
    }
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
    fn the_checksum_is_required_unless_the_configuration_says_otherwise() {
        // The default matters more than the option: a configuration written before this
        // setting existed must keep the old wire behaviour, which is `required`.
        let text = r#"
default_backend = "app"
[server]
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
"#;
        let config = Config::from_toml(text).expect("config parses");
        match &config.backends["app"] {
            BackendConfig::Worker(w) => assert_eq!(w.checksum, ChecksumMode::Required),
            other => panic!("expected worker, got {other:?}"),
        }

        let text = r#"
default_backend = "app"
[server]
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
checksum = "optional"
"#;
        let config = Config::from_toml(text).expect("config parses");
        match &config.backends["app"] {
            BackendConfig::Worker(w) => {
                assert_eq!(w.checksum, ChecksumMode::Optional);
                assert!(!w.checksum.is_required());
            }
            other => panic!("expected worker, got {other:?}"),
        }
    }

    #[test]
    fn a_worker_uses_the_better_transport_for_the_platform() {
        // The default is the whole point of the option: a deployment that never mentions
        // `transport` must still get the transport that platform is fast on. On Windows that
        // is the socket — PHP reads a pipe there in ~8 ms median against ~0.09 ms on a
        // socket, which was 21× on the whole route. The assertion is platform-conditional
        // rather than a fixed value because the correct answer is.
        let text = r#"
default_backend = "app"
[server]
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
"#;
        let config = Config::from_toml(text).expect("config parses");
        let expected = if cfg!(windows) {
            WorkerTransport::Socket
        } else {
            WorkerTransport::Pipe
        };
        match &config.backends["app"] {
            BackendConfig::Worker(w) => {
                assert_eq!(w.transport, expected);
                assert_eq!(w.transport.is_pipe(), !cfg!(windows));
            }
            other => panic!("expected worker, got {other:?}"),
        }
    }

    #[test]
    fn the_transport_can_always_be_stated_explicitly() {
        // The platform default has to be overridable in both directions, or the setting is
        // only half a setting: a Windows deployment may want the pipe, and a Linux one may
        // want the socket to compare the two on its own hardware.
        for (written, expected) in [
            ("pipe", WorkerTransport::Pipe),
            ("socket", WorkerTransport::Socket),
        ] {
            let text = format!(
                r#"
default_backend = "app"
[server]
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
transport = "{written}"
"#
            );
            let config = Config::from_toml(&text).expect("config parses");
            match &config.backends["app"] {
                BackendConfig::Worker(w) => assert_eq!(w.transport, expected, "{written}"),
                other => panic!("expected worker, got {other:?}"),
            }
        }
        assert_eq!(WorkerTransport::Pipe.to_string(), "pipe");
        assert_eq!(WorkerTransport::Socket.to_string(), "socket");
    }

    #[test]
    fn an_unknown_checksum_mode_is_refused_rather_than_defaulted() {
        // The setting decides whether integrity is checked, so a typo — `checksum = "opt"`,
        // say, or the `none` a reader might guess at — must not silently mean `required`.
        // Guessing either way is wrong, so the field is denied rather than defaulted.
        let text = r#"
default_backend = "app"
[server]
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
checksum = "none"
"#;
        let err = Config::from_toml(text).expect_err("must not be accepted");
        let message = err.to_string();
        assert!(message.contains("unknown variant `none`"), "{message}");
        assert!(
            message.contains("expected `required` or `optional`"),
            "the error must name the two valid modes: {message}"
        );
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
        //
        // The paths have to be absolute, or they never reach the length check —
        // they are rejected as relative first, and a test that exercises the
        // length rule has to get past that. The leading `/` is part of the path
        // like any other byte.
        let suffix = ".".len() + 10 + ".sock".len();
        let fits = format!("unix:/{}", "a".repeat(UNIX_SOCKET_PATH_MAX - suffix - 1));
        let too_long = format!("unix:/{}", "a".repeat(UNIX_SOCKET_PATH_MAX));

        if cfg!(unix) {
            // The boundary in both directions. `fits` is exactly the longest
            // prefix that still leaves room for a worst-case slot suffix, so one
            // byte more must be rejected.
            assert_eq!(
                fits.len() - "unix:".len() + suffix,
                UNIX_SOCKET_PATH_MAX,
                "the `fits` fixture must sit exactly on the boundary"
            );
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
        assert!(
            err.to_string().contains("path_prefix, path_suffix or host"),
            "{err}"
        );
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
    fn static_files_are_on_unless_a_deployment_says_otherwise() {
        let config =
            Config::from_toml("[server]\nhttp = \"127.0.0.1:8080\"\n").expect("config parses");
        config.validate().expect("a listener alone is enough");
        let statics = config.server.statics();
        assert!(
            statics.enabled,
            "a site serves its own assets unless it says not to"
        );
        // A front controller is unaffected by the default: `/about` still reaches it, because
        // what changed is only the answer for a path that names a real, non-script file.
        assert_eq!(statics.dotfiles, Dotfiles::Deny);
        assert!(statics.directory_index);
        assert_eq!(statics.max_multirange_bytes, 1024 * 1024);

        // And turning it off is one key.
        let off = Config::from_toml(
            "[server]\nhttp = \"127.0.0.1:8080\"\nstatic_files = false\n",
        )
        .expect("config parses");
        assert!(!off.server.statics().enabled);
    }

    /// A FastCGI-only deployment never serves static content, and `stokehold` refuses to
    /// start one that asks for it. Leaving the key out is not asking.
    #[test]
    fn a_fastcgi_only_deployment_may_leave_the_static_key_out() {
        let text = r#"
[server]
fastcgi = "127.0.0.1:9000"
"#;
        let config = Config::from_toml(text).expect("config parses");
        config
            .validate()
            .expect("the default must not make a FastCGI-only config unshippable");

        // But an explicit request with no HTTP listener is a mistake worth reporting.
        for written in ["static_files = true\n", "static_files = { enabled = true }\n"] {
            let text = format!("[server]\nfastcgi = \"127.0.0.1:9000\"\n{written}");
            let config = Config::from_toml(&text).expect("config parses");
            let err = config.validate().expect_err("must fail");
            assert!(err.to_string().contains("static"), "{written}: {err}");
        }
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

        let statics = config.server.statics();
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

    /// A two-site configuration, which is the shape this whole key exists for.
    fn two_sites() -> Config {
        let text = r#"
default_backend = "app"

[server]
http = "127.0.0.1:8080"
document_root = "/srv/default"
front_controller = "index.php"

[backends.app]
kind = "worker"
command = "php worker.php"

[backends.legacy]
kind = "spawn_cgi"
binary = 'C:\php\php-cgi.exe'

[sites.example_com]
hosts = ["example.com", "www.example.com"]
document_root = "/srv/example/public"
front_controller = "index.php"
default_backend = "app"

[sites.corp]
hosts = ["corp.internal", "*.corp.internal"]
document_root = "/srv/corp"
front_controller = false
default_backend = "legacy"
serve_only = "backend"
static_files = { precompressed = true }

  routes = [
      { path_suffix = ".php", backend = "legacy" },
  ]
"#;
        let config = Config::from_toml(text).expect("config parses");
        config.validate().expect("config validates");
        config
    }

    #[test]
    fn parses_a_site_table() {
        let config = two_sites();
        assert_eq!(config.sites.len(), 2);

        let example = &config.sites["example_com"];
        assert_eq!(example.hosts, ["example.com", "www.example.com"]);
        assert_eq!(example.document_root, PathBuf::from("/srv/example/public"));
        assert_eq!(example.default_backend.as_deref(), Some("app"));

        let corp = &config.sites["corp"];
        assert_eq!(corp.serve_only, Some(ServeOnly::Backend));
        // A boolean front controller is "only real files", and it round-trips as
        // `None` path rather than an empty string, which TOML cannot express.
        assert_eq!(
            corp.front_controller
                .as_ref()
                .and_then(FrontController::as_path),
            None
        );
        assert_eq!(corp.routes.as_ref().map(Vec::len), Some(1));
        assert_eq!(
            corp.static_files.as_ref().map(StaticsSetting::resolved),
            Some(StaticConfig {
                precompressed: true,
                ..StaticConfig::default()
            })
        );
    }

    #[test]
    fn a_bare_boolean_static_setting_is_the_short_form_of_enabled() {
        // The global key takes the same bare boolean a site does, so the two
        // spellings cannot diverge — which is the point: `false` and
        // `{ enabled = false }` must not need different words.
        for (written, enabled) in [("false", false), ("true", true)] {
            let text = format!("[server]\nhttp = \"127.0.0.1:8080\"\nstatic_files = {written}\n");
            let config = Config::from_toml(&text).expect("config parses");
            assert_eq!(
                config.server.statics().enabled,
                enabled,
                "static_files = {written}"
            );
        }

        // A table is the same setting spelled at length, so the two must agree.
        let text = "[server]\nhttp = \"127.0.0.1:8080\"\n\
                    static_files = { enabled = false }\n";
        let table = Config::from_toml(text).expect("config parses");
        let short =
            Config::from_toml("[server]\nhttp = \"127.0.0.1:8080\"\nstatic_files = false\n")
                .expect("config parses");
        assert_eq!(table.server.statics(), short.server.statics());

        // And the real thing: a site carrying the bare boolean.
        let text = r#"
[server]
http = "127.0.0.1:8080"

[sites.a]
hosts = ["a.test"]
document_root = "/srv/a"
static_files = false
"#;
        let config = Config::from_toml(text).expect("config parses");
        let site = &config.sites["a"];
        assert_eq!(
            site.static_files.as_ref().map(StaticsSetting::resolved),
            Some(StaticConfig {
                enabled: false,
                ..StaticConfig::default()
            })
        );
        config.validate().expect("files-free site still validates");
    }

    #[test]
    fn a_wildcard_host_matches_subdomains_but_not_the_bare_domain() {
        // A wildcard names a subdomain. Matching the bare domain too would
        // silently widen every rule that used one, which is the kind of widening a
        // reader cannot see from the pattern alone.
        assert!(host_matches("*.example.com", Some("a.example.com")));
        assert!(host_matches("*.example.com", Some("a.b.example.com")));
        assert!(host_matches("*.example.com", Some("A.Example.COM:8080")));
        assert!(!host_matches("*.example.com", Some("example.com")));
        assert!(!host_matches("*.example.com", Some(".example.com")));
        assert!(!host_matches("*.example.com", Some("notexample.com")));

        // An exact name is unchanged, including the port and case handling the
        // route table already had.
        assert!(host_matches("example.com", Some("EXAMPLE.com:8080")));
        assert!(!host_matches("example.com", Some("www.example.com")));
        // An IPv6 literal has several colons and no port, so the last one must not
        // be mistaken for a port separator.
        assert!(host_matches("::1", Some("::1")));
        assert!(host_matches("::1", Some("[::1]:8080")));
    }

    #[test]
    fn a_path_suffix_matches_case_insensitively_like_the_router_does() {
        // The router decides what a script is case-insensitively, so the route
        // table must agree or `/upload.PHP` would be a script to one layer and miss
        // its route in the other.
        let config = routed_suffix_config();
        for path in ["/upload.php", "/upload.PHP", "/a/b/x.PhP"] {
            assert_eq!(
                config.route_backend(None, path),
                Some("legacy"),
                "{path} must reach the .php route"
            );
        }
        assert_eq!(config.route_backend(None, "/upload.phtml"), None);
        // A suffix is a suffix: it does not require a segment boundary, which is
        // what a reader of `ends_with` expects.
        assert_eq!(config.route_backend(None, "/notphp"), None);
        assert_eq!(config.route_backend(None, "/xmy.php"), Some("legacy"));
    }

    fn routed_suffix_config() -> Config {
        let text = r#"
default_backend = "app"

[server]
http = "127.0.0.1:8080"

[backends.app]
kind = "worker"
command = "php worker.php"

[backends.legacy]
kind = "spawn_cgi"
binary = 'C:\php\php-cgi.exe'

[[routes]]
path_suffix = ".php"
backend = "legacy"
"#;
        let config = Config::from_toml(text).expect("config parses");
        config.validate().expect("config validates");
        config
    }

    #[test]
    fn an_exact_host_beats_a_wildcard_at_the_same_specificity() {
        // Both routes state one constraint and both match `admin.example.com`; the
        // exact name is the more specific claim, so it wins regardless of order.
        let text = r#"
default_backend = "app"

[server]
http = "127.0.0.1:8080"

[backends.app]
kind = "worker"
command = "php worker.php"

[backends.wild]
kind = "worker"
command = "php worker.php"

[backends.exact]
kind = "worker"
command = "php worker.php"

# Declared first, so order alone would pick it — which is what the test is about.
[[routes]]
host = "*.example.com"
backend = "wild"

[[routes]]
host = "admin.example.com"
backend = "exact"
"#;
        let config = Config::from_toml(text).expect("config parses");
        config.validate().expect("config validates");
        assert_eq!(
            config.route_backend(Some("admin.example.com"), "/"),
            Some("exact")
        );
        assert_eq!(
            config.route_backend(Some("other.example.com"), "/"),
            Some("wild")
        );
    }

    #[test]
    fn rejects_a_path_suffix_that_would_match_by_accident() {
        // `path_suffix = "php"` matches `/notphp`, which is not what it looks like
        // it does, so it is refused rather than left as a routing surprise.
        for suffix in ["php", "*php", "php/"] {
            let text = format!(
                r#"
default_backend = "app"
[server]
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"

[[routes]]
path_suffix = {suffix:?}
backend = "app"
"#
            );
            let config = Config::from_toml(&text).expect("config parses");
            let err = config.validate().expect_err("must fail");
            assert!(err.to_string().contains("path_suffix"), "{suffix}: {err}");
        }
    }

    #[test]
    fn rejects_a_site_that_would_never_be_reached() {
        let base = r#"
[server]
http = "127.0.0.1:8080"
"#;

        // No hosts.
        let text = format!("{base}\n[sites.a]\nhosts = []\ndocument_root = \"/srv/a\"\n");
        let config = Config::from_toml(&text).expect("parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("hosts is empty"), "{err}");

        // Nothing on disk to serve.
        let text = format!("{base}\n[sites.a]\nhosts = [\"a.test\"]\ndocument_root = \"\"\n");
        let config = Config::from_toml(&text).expect("parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("document_root is empty"), "{err}");

        // Two sites claiming one host: which one wins is map order, so neither the
        // file nor a reader can predict it.
        let text = format!(
            "{base}\n[sites.a]\nhosts = [\"same.test\"]\ndocument_root = \"/srv/a\"\n\
             [sites.b]\nhosts = [\"SAME.test\"]\ndocument_root = \"/srv/b\"\n"
        );
        let config = Config::from_toml(&text).expect("parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("both claim"), "{err}");
    }

    #[test]
    fn rejects_a_site_setting_that_contradicts_another() {
        // `static_files = true` and `serve_only = "backend"` cannot both be
        // honoured, and `false` beside `"static"` is a double negative. Refusing
        // keeps the resolved answer a single statement.
        let base = r#"
[server]
http = "127.0.0.1:8080"
"#;
        for (statics, serve) in [("true", "\"backend\""), ("false", "\"static\"")] {
            let text = format!(
                "{base}\n[sites.a]\nhosts = [\"a.test\"]\ndocument_root = \"/srv/a\"\n\
                 static_files = {statics}\nserve_only = {serve}\n"
            );
            let config = Config::from_toml(&text).expect("parses");
            let err = config.validate().expect_err("must fail");
            assert!(
                err.to_string().contains("static_files"),
                "{statics}/{serve}: {err}"
            );
        }
    }

    #[test]
    fn rejects_a_site_naming_an_unknown_backend_or_escaping_its_root() {
        let base = r#"
default_backend = "app"
[server]
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
"#;
        let text = format!(
            "{base}\n[sites.a]\nhosts = [\"a.test\"]\ndocument_root = \"/srv/a\"\n\
             default_backend = \"nope\"\n"
        );
        let config = Config::from_toml(&text).expect("parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("default_backend"), "{err}");

        // A site's front controller is validated by the same rule as the global
        // one, so it cannot escape the site's root either.
        for front in ["/etc/passwd", "../index.php"] {
            let text = format!(
                "{base}\n[sites.a]\nhosts = [\"a.test\"]\ndocument_root = \"/srv/a\"\n\
                 front_controller = {front:?}\n"
            );
            let config = Config::from_toml(&text).expect("parses");
            let err = config.validate().expect_err("must fail");
            assert!(
                err.to_string().contains("below the document root"),
                "{front}: {err}"
            );
        }
    }

    #[test]
    fn rejects_a_wildcard_that_would_claim_a_whole_top_level_domain() {
        let text = r#"
[server]
http = "127.0.0.1:8080"

[sites.a]
hosts = ["*.com"]
document_root = "/srv/a"
"#;
        let config = Config::from_toml(text).expect("parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("at least two labels"), "{err}");
    }

    #[test]
    fn a_front_controller_of_false_means_only_real_files() {
        // The site spelling of `--front-controller ''`, which the global key can
        // only express on the command line.
        let text = r#"
[server]
http = "127.0.0.1:8080"

[sites.a]
hosts = ["a.test"]
document_root = "/srv/a"
front_controller = false
"#;
        let config = Config::from_toml(text).expect("parses");
        config.validate().expect("validates");
        let controller = config.sites["a"].front_controller.as_ref().unwrap();
        assert_eq!(controller.as_path(), None);
        assert_eq!(controller, &FrontController::FilesOnly(false));

        // `true` is accepted as a value but is not a path, so it is refused as
        // meaningless rather than silently meaning "yes, index.php".
        let text = r#"
[server]
http = "127.0.0.1:8080"

[sites.a]
hosts = ["a.test"]
document_root = "/srv/a"
front_controller = true
"#;
        let config = Config::from_toml(text).expect("parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("front_controller"), "{err}");
    }
}
