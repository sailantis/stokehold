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

/// Top level configuration, deserialized from `volucer.toml`.
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

    /// This site's own certificate, presented by SNI for the site's hosts.
    ///
    /// Absent inherits `[server.tls]`. This is the only per-site TLS setting, because it is
    /// the only one that is a statement about the site rather than about the socket; see
    /// [`SiteTlsConfig`].
    #[serde(default)]
    pub tls: Option<SiteTlsConfig>,

    /// This site's certificates, obtained automatically.
    ///
    /// Absent inherits `[server.acme]`, which means the deployment's certificate covers this site's
    /// hosts too. Naming a table here gives the site its **own** certificate, for its own names,
    /// which is what a deployment with several unrelated hostnames wants — one certificate per
    /// hostname rather than one covering all of them.
    ///
    /// A site that states its own [`tls`](SiteConfig::tls) is left alone: naming a certificate is a
    /// decision not to have one issued, and honouring both would mean the site had two answers.
    #[serde(default)]
    pub acme: Option<SiteAcmeConfig>,
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
/// One function for the route table, and [`crate::hostmap::HostMap`] for the site and
/// certificate tables. The two are separate implementations of one rule — a map probe versus a
/// walk over the host's own labels — so they are pinned against each other by a test
/// (`this_table_and_host_matches_agree_on_every_shape` in the hostmap module) rather than
/// trusted to stay in step. That test found the divergence it was written for: this function
/// used to accept `a..example.com` for `*.example.com`, because an empty label is still a label
/// by this arithmetic.
///
/// The pattern is an exact name, compared case-insensitively with any `:port` stripped, or a
/// leading `*.` wildcard. `*.example.com` matches `a.example.com` and `a.b.example.com` but
/// *not* `example.com` itself: a wildcard names a subdomain, and treating it as also matching
/// the bare domain would silently widen every rule that used one.
pub fn host_matches(pattern: &str, host: Option<&str>) -> bool {
    let host = host.unwrap_or_default();
    let host = strip_port(host);
    if host.is_empty() {
        return false;
    }

    match pattern.strip_prefix("*.") {
        None => host.eq_ignore_ascii_case(pattern),
        Some(suffix) => {
            // The host must be longer than the suffix *plus* the separating dot, and that dot
            // must be there. Both halves are needed: without the length check, `.example.com`
            // would pass — it is exactly one byte longer than the suffix and that byte is a
            // dot — and the wildcard would be claimed by an empty label.
            let suffix_len = suffix.len();
            if host.len() <= suffix_len + 1 {
                return false;
            }
            let dot = host.len() - suffix_len - 1;
            if host.as_bytes()[dot] != b'.' {
                return false;
            }
            // And the label *before* that dot must be non-empty, which one length check cannot
            // express: `a..example.com` is longer than the suffix, and the byte before the
            // suffix really is a dot — but the label it separates is empty, so this is not a
            // subdomain of `example.com`. A name with a zero-length label is not a host name.
            if dot == 0 || host.as_bytes()[dot - 1] == b'.' {
                return false;
            }
            host[host.len() - suffix_len..].eq_ignore_ascii_case(suffix)
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
pub fn skip_stat(route: &RouteConfig, backend_interprets_path: bool) -> bool {
    // A backend that does not interpret the path never reads the file it names, so there is
    // nothing for the stat to protect. That covers two kinds for two different reasons: a worker
    // reads `script_filename` as metadata and routes on `REQUEST_URI`, and an upstream is handed
    // the path as a URL and does its own routing with it. See `BackendConfig::interprets_path`.
    if !backend_interprets_path {
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
///
/// One implementation, in [`crate::hostmap`], because `host_matches` below and that module's
/// lookup have to normalise a header the same way — a port stripped in one and not the other
/// would make a route and a site disagree about the same request.
use crate::hostmap::strip_port;

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
    ///
    /// With no `[server.tls]` this listener is plaintext. With `[server.tls]` present and
    /// no `https`, TLS wraps *this* listener, and `[server.tls].plain` decides whether it
    /// also answers cleartext on the same port.
    #[serde(default)]
    pub http: Option<String>,

    /// HTTPS listener, e.g. `0.0.0.0:443`.
    ///
    /// A second, independently bound listener that speaks TLS only. Setting it frees
    /// `http` to stay plaintext, which is what an `:80` and `:443` deployment needs — and
    /// what ACME's HTTP-01 challenge and an `:80` → `:443` redirect need, because those
    /// are the only parts of TLS that genuinely require a particular port.
    ///
    /// Requires `[server.tls]`: a TLS listener with no certificate is an error, not a
    /// listener that fails every handshake. Setting it together with `[server.tls].plain`
    /// is also an error — the port is already dedicated, so the same-port sniff would have
    /// nothing to decide.
    #[serde(default)]
    pub https: Option<String>,

    /// FastCGI listener, e.g. `127.0.0.1:9000` or `unix:/run/volucer.sock`.
    #[serde(default)]
    pub fastcgi: Option<String>,

    /// Directory that PHP scripts and static files are resolved against.
    ///
    /// A relative path is relative to the process's working directory, not to
    /// this file — the same rule [`WorkerConfig::working_dir`] follows. Prefer
    /// an absolute path: a deployment that sets its working directory elsewhere
    /// would otherwise resolve somewhere else silently. `volucer check` prints
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

    /// TLS settings. **Presence of this table is what turns TLS on.**
    ///
    /// No separate `enabled` boolean, deliberately: a flag and a certificate are two
    /// statements about one thing, and they can disagree. A table without both a
    /// certificate and a key is rejected instead.
    #[serde(default)]
    pub tls: Option<TlsConfig>,

    /// Certificate issuance. **Presence of this table is what turns ACME on.**
    ///
    /// Opt-in *in addition* to `[server.tls]`, which stays required: a deployment with no ACME
    /// behaves exactly as it did before this key existed. See [`AcmeConfig`] for why a fallback
    /// certificate is not optional.
    #[serde(default)]
    pub acme: Option<AcmeConfig>,

    /// Answer cleartext requests on `http` with a `301` to the `https://` equivalent.
    ///
    /// Off by default: it changes what an existing cleartext deployment does, and a
    /// redirect is a deployment's decision rather than a server's. Only meaningful when
    /// there is a plaintext listener to redirect *from* — with TLS on `http` and
    /// `plain = false`, that listener only ever answers TLS, so setting this is an error
    /// rather than a silently inert key.
    #[serde(default)]
    pub redirect_to_https: bool,
}

/// The front controller used when neither the config nor the command line says.
pub const DEFAULT_FRONT_CONTROLLER: &str = "index.php";

/// ALPN protocol names this server understands.
///
/// Checked by name rather than passed through, because an unknown identifier is accepted by
/// rustls and then never selected by anyone: the listener would look configured and answer
/// only whatever a client happened to offer. `h2` and `http/1.1` are the two it can actually
/// serve.
pub const KNOWN_ALPN: [&str; 2] = ["h2", "http/1.1"];

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

    /// Whether a plaintext listener exists to redirect *from*.
    ///
    /// Two shapes have one: `http` + `https`, and TLS on `http` with `plain = true`. With
    /// TLS on `http` alone, that socket never answers cleartext, so `redirect_to_https` has
    /// nothing to act on.
    pub fn has_plaintext_listener(&self) -> bool {
        let Some(tls) = &self.tls else {
            return false;
        };
        if self.https.is_some() {
            return self.http.is_some();
        }
        tls.plain
    }
}

/// TLS settings.
///
/// **The presence of this table is what turns TLS on.** There is deliberately no `enabled`
/// key: a flag and a certificate are two statements about the same thing, and two statements
/// can disagree — so a table without a usable certificate is an error instead.
///
/// A relative `certificate`/`key` is resolved against **the directory holding the config
/// file**, which is a deliberate exception to `document_root`'s rule (that one is relative to
/// the process's working directory). A certificate beside `volucer.toml` is the common
/// case, and resolving it against wherever the process happened to start would be a footgun
/// with a security consequence. `volucer check` prints the path it resolved.
///
/// **Both keys may be left out when an ACME authority will issue** — see [`AcmeConfig`]. The server
/// then generates a self-signed pair in memory for the first handshakes, and issuance replaces it.
/// The pair is not written to disk: it is a placeholder, so there is nothing to clean up and no
/// private key lying around for a certificate nothing ever trusted.
///
/// Absent is *only* allowed together with ACME, because otherwise the server would have nothing to
/// present and every handshake that is not an ACME challenge would fail — the mistake hazard 56 was
/// about, and the reason this table exists rather than an `enabled` flag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// PEM certificate chain, leaf first.
    ///
    /// Absent means "generate one", which requires an ACME authority to replace it.
    #[serde(default)]
    pub certificate: Option<PathBuf>,

    /// PEM private key (PKCS#8, PKCS#1 or SEC1).
    ///
    /// Absent means "generate one", which requires an ACME authority to replace it.
    #[serde(default)]
    pub key: Option<PathBuf>,

    /// Lowest protocol version accepted: `"1.2"` (the default) or `"1.3"`.
    ///
    /// Stated as a string rather than a number because `1.2` and `1.3` are names of
    /// protocol versions, not decimals, and `1.10` must never be expressible.
    #[serde(default)]
    pub min_version: MinTlsVersion,

    /// ALPN protocols to advertise, in preference order.
    ///
    /// `http/1.1` must not be dropped: a client that cannot do `h2` must still connect, and
    /// a listener that advertises only `h2` fails that handshake rather than downgrading.
    #[serde(default = "default_alpn")]
    pub alpn: Vec<String>,

    /// Also answer cleartext HTTP on the same port as TLS.
    ///
    /// Off by default, and it should stay off unless something needs it. It exists for a
    /// deployment behind an upstream proxy that forwards cleartext to `:443`, and for a
    /// migration period — not as a convenience. Accepting cleartext on a port that clients
    /// believe is encrypted is a security decision, so it is explicit, it is logged loudly at
    /// startup, and it is an error beside `https` (that port is already dedicated, so the
    /// sniff would have nothing to decide).
    ///
    /// A **detection, not a downgrade**: the first byte picks which of two paths a connection
    /// takes (a TLS record always begins `0x16`; a plaintext request begins an ASCII method
    /// letter). A client that offers TLS and fails the handshake still fails — there is no
    /// fallback to cleartext on a handshake error.
    #[serde(default)]
    pub plain: bool,
}

/// The `[sites.<name>.tls]` override.
///
/// Only the certificate and key can differ per site, because SNI is the whole reason a
/// per-site table exists: the name is claimed before any request is read, so a per-site
/// setting here can only be about *which certificate is presented*. `min_version`, `alpn`
/// and `plain` are properties of the socket the connection arrived on, so a per-site value
/// would describe something the socket does not do — setting one is an error rather than a
/// silently ignored key.
///
/// Both keys may be left out when the site has its own `[sites.<name>.acme]`, for the same reason
/// [`TlsConfig`] allows it: what is generated in memory is replaced by what is issued.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteTlsConfig {
    /// PEM certificate chain for this site's hosts, leaf first.
    #[serde(default)]
    pub certificate: Option<PathBuf>,

    /// PEM private key for this site's hosts.
    #[serde(default)]
    pub key: Option<PathBuf>,
}

/// `[server.acme]`: obtain this deployment's certificates automatically (RFC 8555).
///
/// # Why this is a table and not a boolean
///
/// Every key here has a value a deployment has to decide, and there is no safe default for any of
/// them except the authority — where the safe default is *staging*, spelled as an enum rather than
/// a `bool` so that `true` cannot quietly mean "issue a real certificate for a name somebody owns"
/// at a call site a reader skims. Presence of the table is what turns ACME on, the same rule
/// `[server.tls]` follows: a flag and a set of names are two statements about one thing, and they
/// can disagree.
///
/// # What it needs from `[server.tls]`
///
/// A **certificate to serve until an order completes** — and `[server.tls].certificate`/`.key` may
/// be left out for that, in which case one is generated in memory and never written down. This
/// replaces the earlier answer to open decision §9.5, which required the *operator* to name a
/// self-signed pair; see hazard 72. The invariant — a TLS listener needs something to present — is
/// what matters, and generating satisfies it without asking for work that is thrown away moments
/// later. The pair is a placeholder, so making it durable would leave a private key on disk for a
/// certificate nothing ever trusted.
///
/// So ACME does not contradict "a TLS listener needs a certificate" — it changes *which*
/// certificate, which is precisely what the reload phase made replaceable.
///
/// # Why this type is separate from [`SiteAcmeConfig`]
///
/// The global table's `domains` is **required** and a site's is **optional**, and that difference is
/// the whole reason there are two types. Deriving names from a site's `hosts` is what an operator
/// expects and is safe within one site; deriving across a whole deployment is exactly where a
/// wildcard host bites, because a wildcard cannot be a certificate name. One type with a
/// conditional rule would make "is `domains` allowed to be empty?" depend on where the table
/// appeared, which is the kind of rule a reader has to be told rather than see.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AcmeConfig {
    /// Whether this table issues at all.
    ///
    /// Defaults to `true`, so a table that does not mention it means what it always meant. `false`
    /// makes the table **defaults only**: its `authority`, `contact` and `cache` still apply to
    /// every site that inherits them, and nothing is ordered for `domains`.
    ///
    /// # Why a key rather than deleting the table
    ///
    /// Removing the table is how a deployment turns ACME off, and it is not always what an operator
    /// wants: staging a contact and an authority, or parking an order during an incident, would mean
    /// deleting settings that have to be typed back correctly afterwards. A key that says "not now"
    /// leaves them in place.
    ///
    /// # Why it is not inherited
    ///
    /// A site's `enabled` is its own. If a site inherited `false`, turning the deployment's orders
    /// off would silently stop every site's too — and the sites are the ones with unrelated
    /// hostnames, which is exactly the deployment that has more than one order. So an operator who
    /// wants silence everywhere says so wherever it is wanted.
    ///
    /// A **disabled server table still issues for a site that asks**, because a site's table is its
    /// own order and its own account; the deployment's `enabled` is a statement about the
    /// deployment's names, not about the deployment's settings.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Which authority to talk to.
    ///
    /// Defaults to staging, and production must be named. The reason is a rate limit as much as a
    /// safety one: a production account has weekly limits, and a first deployment that reached
    /// production by omission would spend them on its own mistakes.
    #[serde(default)]
    pub authority: Authority,

    /// The names to issue for.
    ///
    /// **Required here, and stated rather than derived.** The deployment-wide table covers hosts
    /// from several sites, and a site's hosts may be a wildcard pattern, which cannot be a name on a
    /// certificate — so a derivation that looked safe for one site would be wrong exactly for the
    /// deployments that need issuance most. A site's own table *may* omit this and derive from its
    /// own `hosts`; see [`SiteAcmeConfig::domains`].
    ///
    /// An empty list is an error rather than "derive from the sites": a mistake that would
    /// otherwise become an ACME request for an empty identifier set.
    #[serde(default)]
    pub domains: Vec<String>,

    /// A contact address for the account, e.g. `mailto:ops@example.com`.
    ///
    /// Optional in the protocol — Let's Encrypt does not require one — and **strongly recommended**
    /// here, because it is the only channel a CA has for warning that renewals are failing for this
    /// account. A deployment with no contact finds out from an expired certificate.
    ///
    /// This is also what the account is keyed by: `rustls-acme`'s directory cache names the account
    /// file after a hash of the contact list and the directory URL, so two tables that list the same
    /// contacts on the same authority **share one account**, and two that differ create two. The
    /// second is invisible in the configuration, so it is reported at startup.
    #[serde(default)]
    pub contact: Vec<String>,

    /// Where the account and issued certificates are cached.
    ///
    /// Relative paths resolve against the config file's directory, the same rule `certificate` and
    /// `key` follow (and deliberately **not** `document_root`'s rule, which is relative to the
    /// process's working directory).
    ///
    /// Not optional in effect: the engine requires a cache, because without one every restart
    /// re-issues and the rate limits are per-week. The default sits beside the configuration file,
    /// which is where an operator looks for it.
    #[serde(default = "default_acme_cache")]
    pub cache: PathBuf,
}

/// `[sites.<name>.acme]`: this site's certificates, obtained automatically.
///
/// The same table as [`AcmeConfig`] with one difference and one addition:
///
/// - `authority`, `contact` and `cache` are all **optional**, so a site inherits the deployment's
///   values and states only what is its own. Inheriting the cache is what makes the account shared:
///   the account file is keyed by contact and authority, not by site.
/// - `domains` may be **omitted**, in which case it is derived from the site's own
///   [`hosts`](SiteConfig::hosts), dropping any wildcard pattern.
///
/// The site's certificate **replaces** the deployment's for the hosts it names. A site that states
/// its own [`tls`](SiteConfig::tls) is left alone entirely — naming a certificate is a decision not
/// to have one issued.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SiteAcmeConfig {
    /// Whether this site's table issues; absent means `true`.
    ///
    /// **Not inherited from `[server.acme]`**, which is the one place this table departs from the
    /// usual rule; see [`AcmeConfig::enabled`]. `false` keeps the table and its settings and issues
    /// nothing for this site, so the site falls back to the deployment's certificate.
    #[serde(default)]
    pub enabled: Option<bool>,

    /// Which authority to talk to; absent inherits `[server.acme]`.
    #[serde(default)]
    pub authority: Option<Authority>,

    /// The names to issue for; absent or empty derives them from this site's `hosts`.
    ///
    /// Deriving drops every wildcard pattern, because `*.example.com` cannot appear on a
    /// certificate — the CA offers `dns-01` for that and this server does not speak it. What is
    /// dropped is **logged at startup**, since a certificate that does not cover a name the site
    /// answers to is a browser warning waiting for someone to notice.
    ///
    /// A derivation that leaves nothing is an **error** rather than an empty order, and the error
    /// quotes the wildcards and says to list the real names.
    #[serde(default)]
    pub domains: Vec<String>,

    /// A contact address for the account; absent inherits `[server.acme]`.
    #[serde(default)]
    pub contact: Option<Vec<String>>,

    /// Where the account and issued certificates are cached; absent inherits `[server.acme]`.
    #[serde(default)]
    pub cache: Option<PathBuf>,
}

impl SiteAcmeConfig {
    /// Whether this table's settings are honoured at all.
    ///
    /// Separate from [`SiteAcmeConfig::issues`] because the two are different questions: a disabled
    /// site still **inherits** the deployment's `authority` and `cache` for its own use, and only
    /// stops ordering. Conflating them would make `enabled = false` mean "ignore this table", which
    /// is what deleting it means.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// Whether this site orders a certificate of its own.
    ///
    /// Also false when the site names its own certificate — see [`SiteConfig::tls`] — because naming
    /// one is a decision not to have one issued. `Config::validate` refuses the two together, so a
    /// deployment cannot reach that state, but the readers downstream do not have to know that.
    pub fn issues(&self) -> bool {
        self.is_enabled()
    }

    /// The names to issue for: the site's own, or derived from its hosts.
    ///
    /// Returns the names and the wildcard patterns that were dropped, so a caller can report them
    /// rather than let the omission pass unmentioned.
    pub fn resolve_domains(&self, hosts: &[String]) -> (Vec<String>, Vec<String>) {
        if !self.domains.is_empty() {
            return (self.domains.clone(), Vec::new());
        }
        let mut names = Vec::new();
        let mut dropped = Vec::new();
        for host in hosts {
            // A wildcard is a *pattern*, not a name, and `validate_acme_domain` refuses it. Split
            // rather than filtered, so the caller can say which ones were left out.
            if host.contains('*') {
                dropped.push(host.clone());
                continue;
            }
            if !names.contains(host) {
                names.push(host.clone());
            }
        }
        (names, dropped)
    }

    /// This site's contact list: its own, or the deployment's.
    ///
    /// Inheriting rather than defaulting is deliberate, and it is what makes the **account**
    /// shared: `rustls-acme` names the account file after a hash of the contact list and the
    /// directory URL, so a site that inherits creates no second account.
    pub fn resolved_contact<'a>(&'a self, global: &'a Option<AcmeConfig>) -> &'a [String] {
        match &self.contact {
            Some(contact) => contact,
            None => global.as_ref().map_or(&[], |acme| acme.contact.as_slice()),
        }
    }

    /// This site's cache directory: its own, or the deployment's.
    ///
    /// Inheriting is what lets several sites' certificates live in one directory. The file names
    /// are hashes of `(domains, directory_url)` for a certificate and `(contact, directory_url)`
    /// for the account, so two sites never collide on a certificate and *do* share an account when
    /// their contacts match.
    pub fn resolved_cache(&self, global: &Option<AcmeConfig>) -> PathBuf {
        match &self.cache {
            Some(cache) => cache.clone(),
            None => global
                .as_ref()
                .map_or_else(default_acme_cache, |acme| acme.cache.clone()),
        }
    }

    /// This site's authority: its own, or the deployment's.
    pub fn resolved_authority(&self, global: &Option<AcmeConfig>) -> Authority {
        match self.authority {
            Some(authority) => authority,
            None => global
                .as_ref()
                .map_or(Authority::default(), |acme| acme.authority),
        }
    }
}

/// The rules every ACME table shares, whoever wrote it.
///
/// Factored out so the deployment's table and a site's table cannot drift: the names, the contact
/// format and the cache are the same questions in both, and the errors name the table they belong
/// to through `label`.
fn validate_acme_table(
    label: &str,
    domains: &[String],
    contact: &[String],
    cache: &Path,
) -> Result<()> {
    for domain in domains {
        validate_acme_domain(domain).map_err(|err| match err {
            Error::Config(message) => Error::Config(message.replace("server.acme", label)),
            other => other,
        })?;
    }

    // Duplicates are not harmful to the order — the CA ignores them — but they are always a
    // mistake, and the mistake they usually are is a copy-paste that left one name out. Saying so
    // is cheaper than the operator discovering the missing name from a browser warning.
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for (index, domain) in domains.iter().enumerate() {
        if let Some(first) = seen.insert(domain.as_str(), index) {
            return Err(Error::Config(format!(
                "{label}.domains lists {domain:?} twice (positions {first} and {index}); the CA \
                 would ignore the repeat, so this is nearly always a name that was meant to be \
                 here instead"
            )));
        }
    }

    for address in contact {
        // `mailto:` is the only scheme Let's Encrypt accepts, and a bare address — the mistake an
        // operator makes because every other tool takes one — would be sent as an unknown URI
        // scheme and rejected by the CA. Catching it here means the error names the fix.
        if !address.starts_with("mailto:") {
            return Err(Error::Config(format!(
                "{label}.contact {address:?} must be a `mailto:` URI, e.g. \
                 \"mailto:ops@example.com\""
            )));
        }
        if address.len() <= "mailto:".len() {
            return Err(Error::Config(format!(
                "{label}.contact is an empty `mailto:` URI; give an address"
            )));
        }
    }

    if cache.as_os_str().is_empty() {
        return Err(Error::Config(format!(
            "{label}.cache is empty; name a directory, or remove the key to get the default \
             beside this file"
        )));
    }

    Ok(())
}

/// Which ACME authority to use.
///
/// A named enum rather than a `bool`, and the reason is the consequence of getting it wrong: a real
/// certificate for a name somebody owns, at a rate limit that is per-week and per-account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    /// Let's Encrypt staging: the real protocol, certificates no browser trusts, and generous
    /// limits. The default.
    #[default]
    Staging,
    /// Let's Encrypt production. Reached only by naming it.
    Production,
}

impl Authority {
    /// The name as it is written in configuration, for logs and diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Authority::Staging => "staging",
            Authority::Production => "production",
        }
    }
}

/// The default ACME cache directory, beside the configuration file.
fn default_acme_cache() -> PathBuf {
    PathBuf::from("acme-cache")
}

/// Validate one ACME domain name.
///
/// Stricter than `validate_host_pattern` on purpose, because the two rules are about different
/// things. A *host pattern* may be a wildcard (`*.example.com`) and is matched against a `Host`
/// header, port and all. An ACME *identifier* is a name that goes on a certificate, so:
///
/// - a wildcard is refused, because TLS-ALPN-01 cannot validate one (RFC 8737 requires the
///   identifier to equal the SNI name, and `*.example.com` is not an SNI name a client sends) and
///   the crate's flat domain list could not express one anyway;
/// - a port is refused, because it is not part of a name;
/// - an empty label is refused, because `a..b` is a typo that a CA rejects after a round trip.
///
/// The last is the reason this exists at all: every rule here is something the CA would reject
/// *after* an order had been started, and an error that arrives before the network is free.
fn validate_acme_domain(domain: &str) -> Result<()> {
    if domain.is_empty() {
        return Err(Error::Config(
            "server.acme.domains contains an empty name".to_string(),
        ));
    }
    if domain.starts_with("*.") || domain.contains('*') {
        return Err(Error::Config(format!(
            "server.acme.domains {domain:?} is a wildcard; the TLS-ALPN-01 challenge proves control \
             of one exact name, so a wildcard cannot be validated this way — list the names \
             individually, or use a DNS-01 client instead"
        )));
    }
    if domain.contains(':') {
        return Err(Error::Config(format!(
            "server.acme.domains {domain:?} contains a port; a certificate names a host, not an \
             address"
        )));
    }
    if domain.starts_with('.') || domain.ends_with('.') || domain.contains("..") {
        return Err(Error::Config(format!(
            "server.acme.domains {domain:?} has an empty label; check the name"
        )));
    }
    if !domain.contains('.') {
        // Not fatal for a CA — a single-label name is valid in some setups — but it is a mistake
        // in every deployment shape this server is for, and the failure it leads to (the CA cannot
        // resolve it) arrives after a round trip.
        return Err(Error::Config(format!(
            "server.acme.domains {domain:?} has no dot; a certificate cannot be issued for a \
             single-label name, so this is probably a typo"
        )));
    }
    Ok(())
}

/// The lowest TLS protocol version accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum MinTlsVersion {
    /// TLS 1.2 — the default. TLS 1.3 clients still negotiate 1.3; this is a floor.
    #[default]
    #[serde(rename = "1.2")]
    V1_2,
    /// TLS 1.3 only.
    #[serde(rename = "1.3")]
    V1_3,
}

/// ALPN default: HTTP/2 preferred, HTTP/1.1 always offered.
fn default_alpn() -> Vec<String> {
    vec!["h2".to_string(), "http/1.1".to_string()]
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
/// There was a fourth, `spawn_fpm`, which volucer would have started itself. It was
/// never implemented — `Dispatcher` refused it — while `volucer version` advertised it
/// and the preflight check warned that it would not start, so the key looked supported
/// and could not work. It is gone rather than built: `docs/plan.md` §9 says not to
/// reimplement what php-fpm already does, and `external` attaches to the php-fpm that
/// systemd already supervises, which is the better version of the same feature.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackendConfig {
    /// Persistent PHP process speaking volucer's own framed protocol over pipes.
    Worker(WorkerConfig),
    /// An already-running php-fpm (systemd-managed). Linux/macOS default.
    External(ExternalConfig),
    /// N `php-cgi` processes on N ports. The only option on Windows.
    SpawnCgi(SpawnCgiConfig),
    /// An HTTP origin volucer proxies to, for a runtime that owns its own HTTP loop.
    ///
    /// Named by role rather than by language: a Node, Python or Go application served by an
    /// application server of its own is the same thing to volucer, which only has to hand it an
    /// HTTP request and relay the answer. This is a *fourth* shape rather than a variation on the
    /// three above because it shares none of their machinery: there is no CGI environment, no
    /// FastCGI record and no volucer protocol, only hyper's client.
    Upstream(UpstreamConfig),
}

/// Whether a backend resets state between requests in the application's own runtime.
///
/// Three answers, not two, and the third is the one that would be got wrong by a boolean: an
/// upstream is neither PHP's SAPI resetting per request nor an application-scoped PHP worker — it
/// is somebody else's runtime, whose request scope is its own business and not something this
/// side can describe. `check` prints exactly that rather than claiming one of the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestScope {
    /// PHP's SAPI rebuilds it for every request (CGI, php-fpm, `php-cgi`).
    PhpSapi,
    /// The application lives across requests and clears its own (the worker protocol).
    ApplicationScoped,
    /// A runtime of its own, which volucer does not reach into.
    OriginScoped,
}

impl RequestScope {
    /// The phrase `volucer check` prints for this answer.
    ///
    /// Owned by the type rather than written at the call site, so the answer and its description
    /// cannot drift apart — and so the three cases are one value instead of a `bool` that would
    /// have to be reinterpreted wherever it is printed.
    pub fn describe(self) -> &'static str {
        match self {
            RequestScope::PhpSapi => "yes (PHP SAPI)",
            RequestScope::ApplicationScoped => "no (application-scoped worker)",
            RequestScope::OriginScoped => "not applicable (the origin owns its request scope)",
        }
    }
}

impl BackendConfig {
    pub fn kind(&self) -> &'static str {
        match self {
            BackendConfig::Worker(_) => "worker",
            BackendConfig::External(_) => "external",
            BackendConfig::SpawnCgi(_) => "spawn_cgi",
            BackendConfig::Upstream(_) => "upstream",
        }
    }

    /// True when the backend resets state per request in PHP's own SAPI.
    pub fn resets_per_request(&self) -> bool {
        matches!(self.request_scope(), RequestScope::PhpSapi)
    }

    /// Where a request's state is scoped for this backend.
    pub fn request_scope(&self) -> RequestScope {
        match self {
            BackendConfig::Worker(_) => RequestScope::ApplicationScoped,
            BackendConfig::External(_) | BackendConfig::SpawnCgi(_) => RequestScope::PhpSapi,
            BackendConfig::Upstream(_) => RequestScope::OriginScoped,
        }
    }

    /// Whether this backend interprets the path as a file to execute.
    ///
    /// The question a route's `skip_stat` turns on, and it is *not* the complement of
    /// [`Self::resets_per_request`]: a worker does not reset per request and an upstream does not
    /// either, but only the worker is reached through a CGI environment for which the path is
    /// metadata. An upstream is handed the path as the request's URL, and does its own routing with
    /// it, so volucer stat-ing it first would check a file nothing reads.
    pub fn interprets_path(&self) -> bool {
        matches!(
            self,
            BackendConfig::External(_) | BackendConfig::SpawnCgi(_)
        )
    }

    /// The parsed origin, for an `upstream` backend.
    ///
    /// `None` for the three CGI-shaped kinds. Parsed rather than stored because the configuration
    /// model deserialises straight from TOML: a field holding a parsed value would have to be
    /// filled in by hand after every load, and a reload reads the file again. This is called at
    /// startup, at `check`, and once per backend when its client is built — never per request.
    ///
    /// The error is returned rather than logged so the caller decides: `validate_backend` turns it
    /// into a configuration error, `preflight` into a warning.
    pub fn origin(&self) -> Option<std::result::Result<OriginSpec, String>> {
        match self {
            BackendConfig::Upstream(upstream) => {
                Some(OriginSpec::parse(&upstream.origin, upstream.tls.clone()))
            }
            _ => None,
        }
    }
}

/// An HTTP origin, reached over the network or a unix socket.
///
/// Parsed by [`OriginSpec::parse`], which lives here so that the *configuration model* can
/// describe and check an origin without the client crate's parser — the same reason
/// [`ChecksumMode`] mirrors the worker protocol's policy instead of importing it. `volucer-core`
/// is the leaf every other crate depends on, so it cannot depend on `volucer-upstream`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    /// Where the origin is: `http://host:port`, `https://host:port`, or `unix:/path/to.sock`.
    pub origin: String,

    /// How to trust an `https:` origin. An error beside any other scheme, since there is no
    /// handshake for it to describe.
    #[serde(default)]
    pub tls: Option<UpstreamTlsConfig>,
}

/// How an `https:` origin is verified.
///
/// Two knobs and no third. There is deliberately **no** `verify = false`: an unverified TLS hop is
/// strictly worse than a plaintext one, because it looks encrypted and checks nothing, and this
/// project already refuses the equivalent shape on the serving side (`[server.tls]` will not put a
/// self-signed certificate on a wire "for a certificate nothing ever trusted"). A deployment that
/// wants that shape writes `http://` on a loopback port and says so out loud.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamTlsConfig {
    /// PEM file holding the roots to verify the origin against. Absent means the public roots.
    #[serde(default)]
    pub ca: Option<PathBuf>,

    /// The name to verify against, when it is not the host in `origin`. The common case is
    /// dialling an IP or a service address while the certificate names a host.
    #[serde(default)]
    pub sni: Option<String>,
}

/// A parsed `origin`, in the configuration model's own terms.
///
/// `volucer-upstream`'s `Origin` is hyper-typed — a `Uri`, an `Authority` — and a configuration
/// model cannot carry one without depending on `hyper`, which this crate deliberately does not.
/// The reverse is fine and is the direction used: this parses, validates and describes, and the
/// client crate builds its `Uri` from it once, at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginSpec {
    /// `host:port` over the network, plaintext or TLS.
    ///
    /// Three fields rather than one string because the three have different rules and the
    /// difference is not reconstructible from the whole: `sni` is what the certificate is verified
    /// against, `host` is what the `Host` header carries, and the two are *meant* to differ when a
    /// deployment dials a service address.
    Network {
        /// `"http"` or `"https"`.
        scheme: &'static str,
        /// The host name or bracketed IPv6 literal, without the port.
        host: String,
        /// The port, defaulted from the scheme when the origin names none.
        port: u16,
        /// How to verify an `https:` origin. Always `Some` for `https`, always `None` for `http`.
        tls: Option<UpstreamTlsConfig>,
    },
    /// A unix socket on this host.
    ///
    /// Only produced on a platform that has one. On Windows the parse refuses `unix:` by name
    /// rather than accepting a path nothing can dial — the same rule `Endpoint` uses.
    #[cfg(unix)]
    Socket(PathBuf),
}

impl OriginSpec {
    /// Parse an `origin` string, together with the `[backends.<n>.tls]` table that belongs to it.
    ///
    /// One constructor rather than two, and the TLS table is a *parameter* rather than something
    /// attached afterwards, because the two are only meaningful together: a `tls` table beside a
    /// plaintext origin describes a handshake that will never happen, so an origin parsed without
    /// it could not be checked for that at all.
    ///
    /// Strict, and every refusal is a shape that would otherwise be *silently* read as something
    /// else: a bare `host:port` would be a guess between a path and an address, a base path and
    /// credentials would look honoured when nothing consumes them, an unknown scheme would be
    /// treated as plaintext, and a `tls` table beside `http` would look like a verified hop.
    pub fn parse(spec: &str, tls: Option<UpstreamTlsConfig>) -> std::result::Result<Self, String> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err("it is empty".to_string());
        }

        if let Some(path) = spec.strip_prefix("unix:") {
            if path.is_empty() {
                return Err("the unix socket path is empty".to_string());
            }
            if tls.is_some() {
                return Err(
                    "a [backends.<n>.tls] table is set beside a `unix:` origin, which has no \
                     handshake for it to describe; remove it"
                        .to_string(),
                );
            }
            #[cfg(unix)]
            {
                if path.contains('\0') {
                    return Err("the unix socket path contains a NUL byte".to_string());
                }
                if path.len() > UNIX_SOCKET_PATH_MAX {
                    return Err(format!(
                        "the unix socket path is {} bytes and the platform allows \
                         {UNIX_SOCKET_PATH_MAX}",
                        path.len()
                    ));
                }
                return Ok(OriginSpec::Socket(PathBuf::from(path)));
            }
            #[cfg(not(unix))]
            {
                return Err(
                    "unix sockets are not available on this platform, and a Windows named \
                            pipe cannot be used here: the HTTP client has no pipe transport, so an \
                            origin must be reached over TCP on Windows"
                        .to_string(),
                );
            }
        }

        let (scheme, rest) = spec.split_once("://").ok_or_else(|| {
            format!(
                "{spec:?} has no scheme; write `http://127.0.0.1:3000` (a bare `host:port` would \
                 be neither plaintext nor TLS for certain, so it is not guessed at)"
            )
        })?;
        let scheme: &'static str = match scheme {
            "http" => "http",
            "https" => "https",
            other => {
                return Err(format!(
                    "{other:?} is not a scheme an origin can be reached with; use `http`, `https` \
                     or `unix`"
                ))
            }
        };

        // Everything before the first `/` is authority. A path, a query or a fragment after it
        // would have to be joined with the request's own, and a prefix joined without saying so is
        // worse than one that is refused — a route prefix belongs in the route table, where it is.
        let (authority, tail) = match rest.find(['/', '?', '#']) {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, ""),
        };
        // Checked before the path, so `http:///api` is reported as having no host rather than as
        // carrying a path — the missing host is the mistake that makes the rest unreadable.
        if authority.is_empty() {
            return Err("it has no host; write `http://127.0.0.1:3000`".to_string());
        }
        if !tail.is_empty() && tail != "/" {
            return Err(format!(
                "it carries the path {tail:?}; an origin is only an address, and a route prefix \
                 belongs in the route table (`path_prefix = ...`)"
            ));
        }
        if authority.contains('@') {
            return Err(
                "it carries credentials, which an origin has no use for: volucer forwards \
                        the client's credentials, never an origin's own"
                    .to_string(),
            );
        }

        let (host, port) = split_authority(authority, scheme)?;
        match scheme {
            // Absent means the public roots, which is the right default for a publicly-trusted
            // origin and the wrong one for an internal service — which is exactly why it is the
            // documented default rather than something inferred.
            "https" => Ok(OriginSpec::Network {
                scheme,
                host,
                port,
                tls: Some(tls.unwrap_or_default()),
            }),
            _ => {
                if tls.is_some() {
                    return Err(format!(
                        "a [backends.<n>.tls] table is set beside the `{scheme}` origin {spec:?}, \
                         which has no handshake for it to describe; remove it, or make the origin \
                         `https://`"
                    ));
                }
                Ok(OriginSpec::Network {
                    scheme,
                    host,
                    port,
                    tls: None,
                })
            }
        }
    }

    /// The scheme, as it is written in configuration.
    pub fn scheme(&self) -> &'static str {
        match self {
            OriginSpec::Network { scheme, .. } => scheme,
            #[cfg(unix)]
            OriginSpec::Socket(_) => "unix",
        }
    }

    /// The `host:port` the origin is dialled at, when there is one.
    ///
    /// The port is the resolved one, so a caller reporting "nothing is listening on
    /// 127.0.0.1:80" from `http://127.0.0.1` names the port that was actually chosen rather than
    /// the empty string the file contains.
    pub fn host_port(&self) -> Option<(String, u16)> {
        match self {
            OriginSpec::Network { host, port, .. } => Some((host.clone(), *port)),
            #[cfg(unix)]
            OriginSpec::Socket(_) => None,
        }
    }

    /// The socket path, for a `unix:` origin.
    pub fn socket_path(&self) -> Option<&Path> {
        match self {
            OriginSpec::Network { .. } => None,
            #[cfg(unix)]
            OriginSpec::Socket(path) => Some(path),
        }
    }

    /// How to verify this origin.
    ///
    /// `None` for plaintext and for a socket, `Some` for `https` — and the field is *not* optional
    /// in the `https` case, which is what makes "verification is not optional" a property of the
    /// type rather than a rule somebody has to remember.
    pub fn tls(&self) -> Option<&UpstreamTlsConfig> {
        match self {
            OriginSpec::Network { tls, .. } => tls.as_ref(),
            #[cfg(unix)]
            OriginSpec::Socket(_) => None,
        }
    }

    /// Whether this origin is reached over TLS.
    pub fn is_tls(&self) -> bool {
        self.tls().is_some()
    }

    /// Whether the origin is on this host rather than across a network.
    ///
    /// Used to decide whether plaintext is worth warning about: a loopback origin is this
    /// deployment talking to itself and has nothing to protect, while a plaintext hop to another
    /// host is the one place an operator may not have realised there is no encryption.
    pub fn is_local(&self) -> bool {
        match self {
            OriginSpec::Network { host, .. } => {
                let bare = host.trim_start_matches('[').trim_end_matches(']');
                bare.eq_ignore_ascii_case("localhost")
                    || bare == "::1"
                    || bare
                        .parse::<std::net::Ipv4Addr>()
                        .is_ok_and(|ip| ip.is_loopback())
            }
            #[cfg(unix)]
            OriginSpec::Socket(_) => true,
        }
    }
}

/// Split `host[:port]`, refusing the shapes that cannot be dialled.
///
/// A bracketed IPv6 literal keeps its brackets: they are what distinguishes the colons in the
/// address from the one before the port, so stripping them here would make the address ambiguous
/// to everything downstream. Everything else is `SocketAddr`'s rule, taken from `parse_listener`
/// so an origin and a listener cannot disagree about what a `host:port` is.
fn split_authority(authority: &str, scheme: &str) -> std::result::Result<(String, u16), String> {
    let default_port = if scheme == "https" { 443 } else { 80 };

    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or_else(|| format!("{authority:?} opens an IPv6 literal without closing it"))?;
        host.parse::<std::net::Ipv6Addr>()
            .map_err(|source| format!("{host:?} is not an IPv6 address: {source}"))?;
        let port = match after {
            "" => default_port,
            _ => after
                .strip_prefix(':')
                .ok_or_else(|| {
                    format!(
                        "{authority:?} has something after the IPv6 literal that is \
                                        not a `:port`"
                    )
                })?
                .parse::<u16>()
                .map_err(|source| format!("{authority:?} has no usable port: {source}"))?,
        };
        return Ok((format!("[{host}]"), port));
    }

    match authority.rsplit_once(':') {
        Some((host, port)) => {
            if host.is_empty() {
                return Err(format!("{authority:?} names a port with no host"));
            }
            if host.contains(':') {
                return Err(format!(
                    "{authority:?} has a bare IPv6 address; write it in brackets, e.g. \
                     `[{host}]:{port}`"
                ));
            }
            let port = port
                .parse::<u16>()
                .map_err(|source| format!("{authority:?} has no usable port: {source}"))?;
            Ok((host.to_string(), port))
        }
        None => Ok((authority.to_string(), default_port)),
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
    /// Defaults to [`ChecksumMode::Required`], which is what every volucer release
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
    /// anything volucer does. On Linux the ordering is the other way round and much
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
    /// `VOLUCER_WORKER_SOCKET`; the worker connects back. The transport is
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
pub const WORKER_SOCKET_ENV: &str = "VOLUCER_WORKER_SOCKET";

/// How strictly the worker protocol's per-frame `crc32c` is enforced.
///
/// A configuration-level mirror of the protocol's `Checksum` policy. It lives here
/// rather than importing the worker crate's enum so that the configuration model stays
/// free of the protocol crate, which is what lets `volucer-core` be the leaf that every
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
        if self.server.http.is_none()
            && self.server.fastcgi.is_none()
            && self.server.https.is_none()
        {
            return Err(Error::Config(
                "no listener configured: set server.http, server.https and/or server.fastcgi"
                    .to_string(),
            ));
        }

        if let Some(http) = &self.server.http {
            parse_listener(http).map_err(|e| Error::Config(format!("server.http: {e}")))?;
        }
        if let Some(fastcgi) = &self.server.fastcgi {
            parse_listener(fastcgi).map_err(|e| Error::Config(format!("server.fastcgi: {e}")))?;
        }
        if let Some(https) = &self.server.https {
            parse_listener(https).map_err(|e| Error::Config(format!("server.https: {e}")))?;
        }

        self.validate_tls()?;

        for (name, backend) in &self.backends {
            self.validate_backend(name, backend)?;
        }

        // An upstream is only reachable over HTTP: there is no CGI environment to build for it and
        // the FastCGI front-end has no way to hand it a request. A deployment that configures one
        // and no HTTP listener has configured something that cannot receive a request at all, and
        // the failure is otherwise silent — the backend starts, the sockets bind, and nothing ever
        // reaches the origin. An error rather than a warning, unlike the static-layer check below,
        // because there is no *second* thing the upstream could be for.
        if self.server.http.is_none() {
            if let Some((name, _)) = self
                .backends
                .iter()
                .find(|(_, backend)| matches!(backend, BackendConfig::Upstream(_)))
            {
                return Err(Error::Config(format!(
                    "backends.{name} is an upstream, which is only reached over HTTP, but no \
                     server.http listener is configured; set server.http, or use a FastCGI \
                     backend for this application"
                )));
            }
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
                 `volucer serve --front-controller ''` to mean \"only real files\"."
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
        self.validate_acme()?;

        Ok(())
    }

    /// Validate the TLS settings, and the listener arrangements they have to agree with.
    ///
    /// Every rule here exists because the alternative is a configuration that parses, reports
    /// nothing, and then does something the file does not say — a listener that accepts and
    /// fails every handshake, or a per-site key that silently has no effect.
    fn validate_tls(&self) -> Result<()> {
        let Some(tls) = &self.server.tls else {
            // No `[server.tls]`: TLS is not configured, so a listener that needs it is the
            // mistake to report. Saying so by name is the difference between "why does my
            // https listener fail every handshake" and a config error.
            if self.server.https.is_some() {
                return Err(Error::Config(
                    "server.https is set but [server.tls] is not; an HTTPS listener needs a \
                     certificate, so add [server.tls] with a certificate and key"
                        .to_string(),
                ));
            }
            if self.sites.values().any(|site| site.tls.is_some()) {
                let name = self
                    .sites
                    .iter()
                    .find(|(_, site)| site.tls.is_some())
                    .map(|(name, _)| name.clone())
                    .unwrap_or_default();
                return Err(Error::Config(format!(
                    "sites.{name}.tls is set but [server.tls] is not; a per-site certificate \
                     overrides the server's, so the server needs one first"
                )));
            }
            if self.server.redirect_to_https {
                return Err(Error::Config(
                    "server.redirect_to_https is set but [server.tls] is not, so there is no \
                     HTTPS endpoint to redirect to"
                        .to_string(),
                ));
            }
            return Ok(());
        };

        // Every TLS key is a *pair*, and a pair with one half is a mistake rather than a partial
        // configuration: the other half has no meaning and the failure would be a missing file
        // named by a key the operator thought they had filled in. Absent **both** is the ACME case
        // and is checked against `validate_acme` below, because whether "generate one" is allowed
        // depends on something this function cannot see on its own.
        match (&tls.certificate, &tls.key) {
            (Some(certificate), Some(key)) => {
                if certificate.as_os_str().is_empty() {
                    return Err(Error::Config(
                        "server.tls.certificate is empty; name the PEM file".to_string(),
                    ));
                }
                if key.as_os_str().is_empty() {
                    return Err(Error::Config(
                        "server.tls.key is empty; name the PEM file".to_string(),
                    ));
                }
            }
            (None, None) => {}
            (Some(_), None) => {
                return Err(Error::Config(
                    "server.tls.certificate is set but server.tls.key is not; name both, or \
                     leave both out to have a placeholder generated (which needs [server.acme] \
                     to replace it)"
                        .to_string(),
                ));
            }
            (None, Some(_)) => {
                return Err(Error::Config(
                    "server.tls.key is set but server.tls.certificate is not; name both, or \
                     leave both out to have a placeholder generated (which needs [server.acme] \
                     to replace it)"
                        .to_string(),
                ));
            }
        }

        for protocol in &tls.alpn {
            if protocol.is_empty() {
                return Err(Error::Config(
                    "server.tls.alpn contains an empty protocol; name one, or remove the key to \
                     get the default"
                        .to_string(),
                ));
            }
            if !KNOWN_ALPN.contains(&protocol.as_str()) {
                return Err(Error::Config(format!(
                    "server.tls.alpn {protocol:?} is not a known protocol; expected one of \
                     {KNOWN_ALPN:?}"
                )));
            }
        }
        if tls.alpn.iter().any(|p| p == "h2") && !tls.alpn.iter().any(|p| p == "http/1.1") {
            // Not fatal — a deployment may genuinely want h2 only — but it is nearly always a
            // mistake with an invisible cost: a client that cannot do h2 fails the handshake
            // rather than being served HTTP/1.1.
            tracing::warn!(
                "server.tls.alpn offers \"h2\" without \"http/1.1\"; a client that cannot do \
                 HTTP/2 will fail the handshake rather than being served HTTP/1.1"
            );
        }

        match (&self.server.http, &self.server.https) {
            (None, None) => {
                return Err(Error::Config(
                    "[server.tls] is set but no listener carries it: set server.http (TLS on \
                     that port) and/or server.https"
                        .to_string(),
                ));
            }
            (Some(http), Some(https)) if http == https => {
                return Err(Error::Config(format!(
                    "server.http and server.https are both {http:?}; they are two listeners, so \
                     they need two addresses"
                )));
            }
            (Some(_), Some(_)) if tls.plain => {
                return Err(Error::Config(
                    "[server.tls].plain is set together with server.https; the plaintext \
                     listener is already server.http, so the sniff has nothing to decide \
                     — remove one of them"
                        .to_string(),
                ));
            }
            _ => {}
        }
        // A site's own table may only change which certificate is presented, because the name
        // is claimed before any request exists. Anything else here would describe something the
        // socket does not do, so it is refused rather than ignored.
        for (name, site) in &self.sites {
            let Some(names) = &site.tls else { continue };
            match (&names.certificate, &names.key) {
                (Some(certificate), Some(key)) => {
                    if certificate.as_os_str().is_empty() {
                        return Err(Error::Config(format!(
                            "sites.{name}.tls.certificate is empty; name the PEM file, or remove \
                             the table to inherit [server.tls]"
                        )));
                    }
                    if key.as_os_str().is_empty() {
                        return Err(Error::Config(format!(
                            "sites.{name}.tls.key is empty; name the PEM file, or remove the \
                             table to inherit [server.tls]"
                        )));
                    }
                }
                (None, None) => {}
                (Some(_), None) => {
                    return Err(Error::Config(format!(
                        "sites.{name}.tls.certificate is set but sites.{name}.tls.key is not; \
                         name both, or leave both out to have a placeholder generated (which \
                         needs an ACME table for this site to replace it)"
                    )));
                }
                (None, Some(_)) => {
                    return Err(Error::Config(format!(
                        "sites.{name}.tls.key is set but sites.{name}.tls.certificate is not; \
                         name both, or leave both out to have a placeholder generated (which \
                         needs an ACME table for this site to replace it)"
                    )));
                }
            }
        }

        // A redirect needs a plaintext listener to redirect *from*. With TLS on `http` and
        // `plain` off, that socket never answers cleartext, so the key could never fire.
        if self.server.redirect_to_https && !self.server.has_plaintext_listener() {
            return Err(Error::Config(
                "server.redirect_to_https is set but nothing serves cleartext: with TLS on \
                 server.http, set [server.tls].plain = true, or add a server.https listener \
                 so server.http stays plaintext"
                    .to_string(),
            ));
        }

        Ok(())
    }

    /// Validate `[server.acme]`, and the arrangement it has to agree with.
    ///
    /// Every rule here exists because the alternative is a deployment that reports nothing and then
    /// behaves in a way the file does not say. Two of them are about silence, which is the failure
    /// mode ACME has that nothing else in this server does: issuance that never succeeds is
    /// indistinguishable from issuance that is working, until a certificate expires.
    /// Validate the ACME tables — the deployment's and each site's — and the arrangement they have
    /// to agree with.
    ///
    /// Every rule here exists because the alternative is a deployment that reports nothing and then
    /// behaves in a way the file does not say. Two of them are about silence, which is the failure
    /// mode ACME has that nothing else in this server does: issuance that never succeeds is
    /// indistinguishable from issuance that is working, until a certificate expires.
    fn validate_acme(&self) -> Result<()> {
        // "Will something be issued" rather than "is a table present": `enabled = false` keeps a
        // table for its settings, and every reader below has to agree that it issues nothing. The
        // difference is not cosmetic — a disabled table that still counted would demand a
        // `[server.tls]` for a deployment that never orders, and would suppress the error for an
        // absent pair that nothing will replace.
        let server_issues = self.server.acme.as_ref().is_some_and(|acme| acme.enabled);
        let site_issues: Vec<(&String, &SiteConfig, &SiteAcmeConfig)> = self
            .sites
            .iter()
            .filter_map(|(name, site)| {
                let acme = site.acme.as_ref()?;
                match acme.issues() {
                    true => Some((name, site, acme)),
                    false => None,
                }
            })
            .collect();
        let any_acme = server_issues || !site_issues.is_empty();

        // TLS has to be on at all, because TLS-ALPN-01 is validated on the TLS port: with no
        // `[server.tls]` there is no TLS listener for the CA to connect to. This holds for a
        // site-level table too, since a per-site certificate is still presented by the server's
        // listener.
        if any_acme && self.server.tls.is_none() {
            return Err(Error::Config(
                "an ACME table is set but [server.tls] is not; the TLS-ALPN-01 challenge is \
                 validated on the TLS port, so add [server.tls] (its certificate and key may be \
                 left out, and one will be generated)"
                    .to_string(),
            ));
        }

        // A **generated** placeholder is only honest when something will replace it. Without that,
        // a listener with nothing to serve fails every handshake that is not a challenge — hazard
        // 56's sibling — and the failure appears at connect time rather than at startup.
        if !any_acme {
            if let Some(tls) = &self.server.tls {
                if tls.certificate.is_none() && tls.key.is_none() {
                    return Err(Error::Config(
                        "server.tls.certificate and server.tls.key are both absent, and no ACME \
                         table will issue a certificate to replace a generated one; name a pair, \
                         or add [server.acme]"
                            .to_string(),
                    ));
                }
            }
        }

        if let Some(acme) = &self.server.acme {
            // Validated even when disabled, because the settings are still in force: a site that
            // inherits them inherits into a table that has to be well formed, and a bad `cache` or
            // `contact` would be reported at whatever point the site finally used it. The domains
            // rule is the exception — a disabled table issues for nothing, so an empty list is not
            // an error there, and refusing it would make "turn it off" impossible to write for a
            // deployment that never stated any.
            if acme.enabled && acme.domains.is_empty() {
                return Err(Error::Config(
                    "server.acme.domains is empty, so there is nothing to issue for; list the \
                     names the certificate must cover, or set server.acme.enabled = false"
                        .to_string(),
                ));
            }
            if acme.enabled {
                validate_acme_table("server.acme", &acme.domains, &acme.contact, &acme.cache)?;
            }
        }

        let mut claimed_names: BTreeMap<String, String> = BTreeMap::new();
        if let Some(acme) = &self.server.acme {
            if acme.enabled {
                for domain in &acme.domains {
                    claimed_names.insert(domain.clone(), "server.acme".to_string());
                }
            }
        }

        for (name, site) in &self.sites {
            let Some(acme) = &site.acme else { continue };
            let label = format!("sites.{name}.acme");

            // A site that names its own certificate has decided not to have one issued, so it
            // orders nothing and there is no second answer to refuse. Mechanically the two already
            // work together: `CertificateSet` gives such a site no generated placeholder, and an
            // issued certificate overwrites the named entry for the names it covers. Naming a pair
            // *and* asking for one is therefore how a deployment migrates from a certificate
            // another authority issued, serving it until ACME replaces it — which is exactly what
            // `[server.tls]` plus `[server.acme]` has always allowed. The two levels agree.
            //
            // An *absent* pair beside a site ACME table is the other half of the feature: the site
            // starts on a generated placeholder. Half a pair is still half a pair, and
            // `validate_tls` reports that.
            if !acme.issues() {
                continue;
            }

            let (names, dropped) = acme.resolve_domains(&site.hosts);
            if names.is_empty() {
                // The derivation found nothing it could put on a certificate. Reported with the
                // patterns that were left out, because "which names did I lose?" is the operator's
                // next question and the answer belongs in this message rather than in a log line.
                return Err(Error::Config(format!(
                    "{label} has no domains and none of sites.{name}.hosts can go on a certificate \
                     ({}); a wildcard cannot be a certificate name, so list the real names in \
                     {label}.domains",
                    dropped.join(", ")
                )));
            }
            for pattern in &dropped {
                // Logged rather than an error: the certificate simply does not cover that pattern,
                // which the operator may well have intended. It is worth saying out loud because
                // the symptom is a browser warning on a name this site answers to.
                tracing::warn!(
                    site = name,
                    pattern = pattern.as_str(),
                    "this site's ACME table derives its domains from hosts, and a wildcard cannot \
                     be a certificate name: the site answers to that pattern and the certificate \
                     will not cover it"
                );
            }
            let cache = acme.resolved_cache(&self.server.acme);
            validate_acme_table(
                &label,
                &names,
                acme.resolved_contact(&self.server.acme),
                &cache,
            )?;

            // A name claimed by two ACME tables cannot be issued twice, and the failure it leads to
            // is invisible. A validation handshake carries only the SNI name, so the deployment's
            // source answers first and returns the *deployment's* challenge certificate — whose
            // `acmeIdentifier` commits to the deployment's key authorization while the site's
            // server is being asked for the site's. The site's order then fails validation on every
            // attempt, and all it can do is retry.
            //
            // Refused rather than warned about because a warning here is indistinguishable from the
            // noise of a first deployment, and because the two ways out are both one edit: drop the
            // name from the deployment's list, or state the site's names so they no longer overlap.
            for domain in &names {
                if let Some(other) = claimed_names.insert(domain.clone(), label.clone()) {
                    return Err(Error::Config(format!(
                        "{domain:?} is issued for by both {other} and {label}; one name belongs to \
                         one order, so remove it from one of them (a site that states its own \
                         `domains` no longer overlaps the deployment's)"
                    )));
                }
            }
        }

        Ok(())
    }

    /// The site table: host patterns, duplicates, and per-site settings.    ///
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
            BackendConfig::Upstream(u) => {
                // Parsing is also the validation of the `[backends.<n>.tls]` table: the origin and
                // the table are only meaningful together, so `OriginSpec::parse` takes the table and
                // refuses one that has no handshake to describe.
                let origin = OriginSpec::parse(&u.origin, u.tls.clone())
                    .map_err(|reason| Error::Config(format!("backends.{name}.origin: {reason}")))?;
                // The scheme decides whether a `tls` table belongs at all, and the parse above has
                // already refused the combination that does not. `origin` is kept in scope rather
                // than discarded so the rule is readable here: `sni` and `ca` are only reachable
                // for a `https:` origin.

                if let Some(tls) = origin.tls() {
                    if let Some(ca) = &tls.ca {
                        if ca.as_os_str().is_empty() {
                            return bad(format!(
                                "backends.{name}.tls.ca is empty; name the PEM file, or remove \
                                 the key to use the public roots"
                            ));
                        }
                    }
                    if let Some(sni) = &tls.sni {
                        // The verification name is checked at startup, not at connect time: a typo
                        // in `sni` is a configuration mistake, and a configuration mistake belongs
                        // where the file is read rather than in a 502 on the first request.
                        let sni = sni.trim();
                        if sni.is_empty() {
                            return bad(format!(
                                "backends.{name}.tls.sni is empty; name the host the certificate \
                                 carries, or remove the key to verify against the origin's own host"
                            ));
                        }
                        // `host:port` and a trailing dot are both refused by this, which is right:
                        // a verification name is a DNS name, and a port is not part of one.
                        if sni.parse::<std::net::IpAddr>().is_err() && !is_dns_name(sni) {
                            return bad(format!(
                                "backends.{name}.tls.sni {sni:?} is not a host name or IP address"
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Whether `name` is a shape a certificate can be verified against.
///
/// Deliberately stricter than DNS allows: no leading or trailing dot, no empty label, and no
/// characters outside letters, digits, `-` and `_`. A name that is *more* permissive than this
/// would be accepted here and then fail rustls' own `ServerName` parse at startup, which reports
/// the failure from a place that cannot say which key it came from.
fn is_dns_name(name: &str) -> bool {
    // A single trailing dot is a valid absolute name in DNS but rustls' `ServerName` refuses it,
    // and a certificate's SAN never carries it either, so refusing it here says so once.
    if name.is_empty() || name.len() > 253 || name.ends_with('.') || name.starts_with('.') {
        return false;
    }
    name.split('.').all(|label| {
        if label.is_empty() || label.len() > 63 {
            return false;
        }
        // A label may not begin or end with a hyphen — DNS's own rule, and the reason a bare `-`
        // is not a host name. Enforced rather than left to `ServerName`, so the error names the
        // key it came from.
        let mut bytes = label.bytes();
        let first = bytes.next().expect("a non-empty label has a first byte");
        let last = label.as_bytes()[label.len() - 1];
        let edge = |b: u8| b.is_ascii_alphanumeric();
        if !edge(first) || !edge(last) {
            return false;
        }
        bytes.all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
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
                 component, e.g. `\\\\\\\\.\\\\pipe\\\\volucer-cgi`"
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
             `unix:/run/volucer/cgi` or `\\\\.\\pipe\\volucer-cgi`"
        );
        #[cfg(not(windows))]
        return format!("{spec:?} must start with `unix:`, e.g. `unix:/run/volucer/cgi`");
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

    /// A config with ACME, as a deployment would write it.
    fn acme_config(acme_body: &str) -> Config {
        let text = format!(
            r#"
[server]
http = "0.0.0.0:443"
document_root = "/srv/app"

[server.tls]
certificate = "fallback.crt"
key = "fallback.key"

[server.acme]
{acme_body}
"#
        );
        Config::from_toml(&text).expect("the fixture parses")
    }

    /// ACME is off unless the table is there, which is what keeps it additive.
    ///
    /// The claim the whole phase rests on: a deployment with no `[server.acme]` behaves exactly as
    /// it did before the key existed. A default that turned it on — or a required key — would make
    /// every existing configuration either change behaviour or fail to parse, and neither is
    /// visible from the ACME tests themselves.
    #[test]
    fn acme_is_absent_until_the_table_is_written() {
        let text = r#"
[server]
http = "0.0.0.0:443"
document_root = "/srv/app"

[server.tls]
certificate = "fallback.crt"
key = "fallback.key"
"#;
        let config = Config::from_toml(text).expect("an existing config still parses");
        assert!(
            config.server.acme.is_none(),
            "ACME must be off when the table is absent, or it is not additive"
        );
        config.validate().expect("and it still validates");
    }

    /// Production must be named; everything else defaults to staging.
    ///
    /// The reason this is a test rather than a comment: getting it wrong issues a real certificate
    /// for a name somebody owns, against a rate limit that is per-week and per-account.
    #[test]
    fn acme_defaults_to_staging_and_production_is_named() {
        let default = acme_config(r#"domains = ["example.com"]"#);
        assert_eq!(
            default.server.acme.as_ref().expect("present").authority,
            Authority::Staging,
            "an authority that is not named must be staging"
        );

        let named = acme_config(
            r#"
authority = "production"
domains = ["example.com"]
"#,
        );
        assert_eq!(
            named.server.acme.as_ref().expect("present").authority,
            Authority::Production
        );
        assert_eq!(Authority::Production.as_str(), "production");
        assert_eq!(Authority::default().as_str(), "staging");
    }

    /// An ACME table with no `[server.tls]` is refused, and the error names the fix.
    ///
    /// A certificate-less ACME deployment is **allowed**, and this is the reversal.
    ///
    /// The earlier answer to open decision §9.5 was "no": name a fallback pair. That answered the
    /// literal question and required the operator to hand-make a certificate that is replaced
    /// moments later — hazard 72. What is actually needed is that the listener has *something* to
    /// present, and generating one satisfies that without asking for work that is thrown away.
    ///
    /// Asserted here rather than left to `serve`, because the interesting part is not that a
    /// placeholder is generated but that the configuration is *accepted*: the refusal this replaces
    /// was the only thing standing between an operator and a one-line ACME config.
    #[test]
    fn acme_needs_no_named_certificate_because_one_is_generated() {
        let text = r#"
[server]
http = "0.0.0.0:443"
document_root = "/srv/app"

[server.tls]

[server.acme]
domains = ["example.com"]
"#;
        let config = Config::from_toml(text).expect("it parses");
        config
            .validate()
            .expect("a pair that is generated is all the listener needs");

        let tls = config.server.tls.as_ref().expect("[server.tls] is present");
        assert!(
            tls.certificate.is_none() && tls.key.is_none(),
            "the point of the case is that neither is named"
        );
    }

    /// A site's `domains` may be left out, and then it follows the site's `hosts` — without the
    /// wildcard, which cannot go on a certificate.
    ///
    /// The answer to "one is writing host and the other domain": they are not the same thing, so
    /// rather than renaming either, the site's table *derives* from the one an operator has already
    /// written. The wildcard is the whole reason this is a rule rather than a default — `*.x` is a
    /// host pattern, and a CA has no `dns-01` here, so it must be dropped **and** reported.
    #[test]
    fn a_site_domain_list_is_derived_from_its_hosts_without_the_wildcard() {
        let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[sites.corp]
hosts = ["corp.internal", "*.corp.internal"]
document_root = "/srv/corp"

[sites.corp.acme]
"#;
        let config = Config::from_toml(text).expect("parses");
        config
            .validate()
            .expect("a derivation that leaves a real name is valid");
        let site = config.sites.get("corp").expect("site present");
        let acme = site.acme.as_ref().expect("the site has its own table");
        let (names, dropped) = acme.resolve_domains(&site.hosts);
        assert_eq!(
            names,
            vec!["corp.internal".to_string()],
            "the exact name is kept and the wildcard is not"
        );
        assert_eq!(
            dropped,
            vec!["*.corp.internal".to_string()],
            "what was left out is named, because the certificate will not cover it"
        );
    }

    /// A derivation that leaves nothing is an **error**, not an empty order.
    ///
    /// The case that makes the rule worth having: a site whose hosts are all wildcards has no name
    /// that can go on a certificate, and the alternative — silently issuing for an empty identifier
    /// set — is an ACME request for nothing at all, reported by the CA in wording that names no site.
    #[test]
    fn a_site_whose_hosts_are_all_wildcards_must_state_its_domains() {
        let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[sites.corp]
hosts = ["*.corp.internal"]
document_root = "/srv/corp"

[sites.corp.acme]
"#;
        let config = Config::from_toml(text).expect("parses");
        let err = config.validate().expect_err("nothing can be issued for");
        let message = err.to_string();
        assert!(
            message.contains("*.corp.internal") && message.contains("sites.corp.acme.domains"),
            "the error must name the wildcard and where to put the real names: {message}"
        );
    }

    /// A site's table inherits the deployment's contact and cache, which is what shares the account.
    ///
    /// `rustls-acme` names the account file after a hash of the contact list and the directory URL, so
    /// inheriting is not tidiness: it is the difference between one account for the deployment and
    /// one per site. The cache is where the certificate files live, and inheriting keeps them in one
    /// directory — safe because a certificate's file name hashes its own domain set.
    #[test]
    fn a_site_inherits_the_contact_and_cache_so_the_account_is_shared() {
        let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[server.acme]
domains = ["example.com"]
contact = ["mailto:ops@example.com"]
cache = "shared-cache"

[sites.corp]
hosts = ["corp.internal"]
document_root = "/srv/corp"

[sites.corp.acme]
domains = ["corp.internal"]
"#;
        let config = Config::from_toml(text).expect("parses");
        config
            .validate()
            .expect("a site that inherits the contact is valid");

        let site = config.sites.get("corp").expect("site present");
        let acme = site.acme.as_ref().expect("site table");
        assert_eq!(
            acme.resolved_contact(&config.server.acme),
            ["mailto:ops@example.com"],
            "the contact is inherited, which is what makes the account shared"
        );
        assert_eq!(
            acme.resolved_cache(&config.server.acme),
            PathBuf::from("shared-cache"),
            "the cache is inherited, so several sites' certificates live in one directory"
        );
        assert_eq!(
            acme.resolved_authority(&config.server.acme),
            Authority::Staging,
            "and the authority, since a site has no reason to differ by default"
        );
    }

    /// A site may override what it must, and only that.
    #[test]
    fn a_site_may_state_its_own_authority_and_contact() {
        let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[server.acme]
domains = ["example.com"]
contact = ["mailto:ops@example.com"]

[sites.corp]
hosts = ["corp.internal"]
document_root = "/srv/corp"

[sites.corp.acme]
domains = ["corp.internal"]
contact = ["mailto:corp@example.com"]
authority = "production"
cache = "corp-cache"
"#;
        let config = Config::from_toml(text).expect("parses");
        config.validate().expect("a site may override all three");

        let site = config.sites.get("corp").expect("site present");
        let acme = site.acme.as_ref().expect("site table");
        assert_eq!(
            acme.resolved_contact(&config.server.acme),
            ["mailto:corp@example.com"]
        );
        assert_eq!(
            acme.resolved_cache(&config.server.acme),
            PathBuf::from("corp-cache")
        );
        assert_eq!(
            acme.resolved_authority(&config.server.acme),
            Authority::Production
        );
    }

    /// A site's own `domains` is used as written, and nothing is reported as dropped.
    #[test]
    fn a_site_that_states_its_domains_keeps_them_verbatim() {
        let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[sites.corp]
hosts = ["corp.internal", "www.corp.internal"]
document_root = "/srv/corp"

[sites.corp.acme]
domains = ["corp.internal"]
"#;
        let config = Config::from_toml(text).expect("parses");
        config.validate().expect("valid");

        let site = config.sites.get("corp").expect("site present");
        let acme = site.acme.as_ref().expect("site table");
        let (names, dropped) = acme.resolve_domains(&site.hosts);
        assert_eq!(names, vec!["corp.internal".to_string()]);
        assert!(
            dropped.is_empty(),
            "nothing was derived, so nothing was left out"
        );
    }

    /// A site's derived names are held to the same rules as a stated list.
    ///
    /// The point of factoring the rules: a derivation that produced a name the CA would refuse must
    /// fail here, naming the site's table, and not only in the deployment-wide case. A single-label
    /// host is the example because it is a *legal* `hosts` entry and an illegal certificate name —
    /// so this is exactly the gap between the two words the derivation has to bridge.
    #[test]
    fn a_derived_name_is_checked_like_a_stated_one() {
        let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[server.acme]
domains = ["example.com"]

[sites.corp]
hosts = ["corp"]
document_root = "/srv/corp"

[sites.corp.acme]
"#;
        let config = Config::from_toml(text).expect("parses");
        let err = config
            .validate()
            .expect_err("a single-label name cannot be issued for");
        let message = err.to_string();
        assert!(
            message.contains("sites.corp.acme"),
            "the error must name the site's table, not the deployment's: {message}"
        );
        assert!(
            message.contains("no dot"),
            "and it must be the certificate rule that fired, not the host rule: {message}"
        );
    }

    /// Each domain rule fails on the thing it names, and the message says why.
    ///
    /// Table-driven because the rules are independent — one case per rule, each asserting the
    /// *reason* rather than only that validation failed. A test that asserted "some error" for all
    /// of them would pass against a single over-broad rule.
    #[test]
    fn an_acme_domain_that_cannot_be_validated_is_refused_with_its_reason() {
        let cases: &[(&str, &[&str])] = &[
            (
                r#"domains = ["*.example.com"]"#,
                &["wildcard", "TLS-ALPN-01"],
            ),
            (r#"domains = ["example.com:443"]"#, &["port"]),
            (r#"domains = ["a..b.com"]"#, &["empty label"]),
            (r#"domains = ["localhost"]"#, &["no dot"]),
            (r#"domains = [""]"#, &["empty"]),
        ];

        for (body, expected) in cases {
            let config = acme_config(body);
            let err = config
                .validate()
                .expect_err(&format!("{body} must be refused"));
            let message = err.to_string();
            for fragment in *expected {
                assert!(
                    message.contains(fragment),
                    "the error for {body} must mention {fragment:?}: {message}"
                );
            }
        }

        // And a valid list passes, so the rules above are not refusing everything.
        acme_config(r#"domains = ["example.com", "www.example.com"]"#)
            .validate()
            .expect("a plain list of names must validate");
    }

    /// An empty domain list is refused rather than becoming an order for nothing.
    #[test]
    fn acme_with_no_domains_is_refused() {
        let config = acme_config(r#"contact = ["mailto:ops@example.com"]"#);
        let err = config.validate().expect_err("nothing to issue for");
        assert!(
            err.to_string().contains("domains is empty"),
            "the error must name the key: {err}"
        );
    }

    /// A duplicate name is refused, and the message points at the likely cause.
    ///
    /// The CA would ignore the repeat, so this is never *harmful* — which is exactly why it needs
    /// catching: the mistake it usually represents is a name that was meant to be there instead.
    #[test]
    fn a_duplicate_acme_domain_is_refused_with_both_positions() {
        let config = acme_config(r#"domains = ["example.com", "www.example.com", "example.com"]"#);
        let err = config
            .validate()
            .expect_err("a duplicate name is a mistake");
        let message = err.to_string();
        assert!(
            message.contains("twice") && message.contains("positions 0 and 2"),
            "the error must say where the duplicate is, so the reader can find it: {message}"
        );
    }

    /// A contact without `mailto:` is refused, because the CA would reject it after a round trip.
    ///
    /// The bare address is the mistake worth catching: every other tool takes `ops@example.com`, so
    /// an operator will write that, and the CA would see an unknown URI scheme.
    #[test]
    fn an_acme_contact_must_be_a_mailto_uri() {
        let bare = acme_config(
            r#"
domains = ["example.com"]
contact = ["ops@example.com"]
"#,
        );
        let err = bare
            .validate()
            .expect_err("a bare address is not a URI the CA accepts");
        let message = err.to_string();
        assert!(
            message.contains("mailto:") && message.contains("ops@example.com"),
            "the error must show the fix: {message}"
        );

        let empty = acme_config(
            r#"
domains = ["example.com"]
contact = ["mailto:"]
"#,
        );
        assert!(
            empty.validate().is_err(),
            "an empty mailto: URI must be refused"
        );

        // And a proper one passes, including several.
        acme_config(
            r#"
domains = ["example.com"]
contact = ["mailto:ops@example.com", "mailto:oncall@example.com"]
"#,
        )
        .validate()
        .expect("well-formed contacts must validate");
    }

    /// The cache defaults beside the configuration file rather than to a relative path a reader
    /// cannot predict.
    #[test]
    fn the_acme_cache_defaults_to_a_named_directory() {
        let config = acme_config(r#"domains = ["example.com"]"#);
        let acme = config.server.acme.as_ref().expect("present");
        assert_eq!(
            acme.cache,
            PathBuf::from("acme-cache"),
            "the default must be a name an operator can find, not the working directory"
        );
        assert!(
            !acme.cache.is_absolute(),
            "a relative default is what makes it resolve against the config file, which is the \
             rule `certificate` and `key` already follow"
        );
    }

    /// An unknown key under `[server.acme]` is refused rather than ignored.
    ///
    /// `deny_unknown_fields` on the table, asserted here because the alternative is the failure
    /// this project keeps refusing: a key that looks like it does something and does not. A typo
    /// such as `domain = [...]` (singular) would otherwise leave `domains` empty — which is caught
    /// too, but the error would name the wrong key.
    #[test]
    fn an_unknown_key_under_server_acme_is_refused() {
        let text = r#"
[server]
http = "0.0.0.0:443"
document_root = "/srv/app"

[server.tls]
certificate = "fallback.crt"
key = "fallback.key"

[server.acme]
domain = ["example.com"]
"#;
        let err = Config::from_toml(text).expect_err("a misspelt key is not a key");
        assert!(
            err.to_string().contains("domain"),
            "the error must name the key it did not recognise: {err}"
        );
    }

    #[test]
    fn parses_a_complete_config() {
        let text = r#"
default_backend = "fpm"

[server]
http = "127.0.0.1:8080"
fastcgi = "unix:/run/volucer.sock"

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
socket = "unix:/run/volucer/cgi"
"#;
        let config = Config::from_toml(text).expect("config parses");
        match &config.backends["cgi"] {
            BackendConfig::SpawnCgi(cgi) => {
                assert_eq!(cgi.socket.as_deref(), Some("unix:/run/volucer/cgi"));
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
        let err = validate_cgi_socket("/run/volucer/cgi").expect_err("no unix: prefix");
        assert!(err.contains("unix:"), "{err}");

        let err = validate_cgi_socket("unix:").expect_err("empty path");
        assert!(err.contains("empty"), "{err}");

        let err = validate_cgi_socket("unix:relative/cgi").expect_err("relative path");
        assert!(err.contains("absolute"), "{err}");

        if cfg!(unix) {
            validate_cgi_socket("unix:/run/volucer/cgi").expect("a plain absolute path is valid");
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
            cgi_socket_path("unix:/run/volucer/cgi", 0),
            PathBuf::from("/run/volucer/cgi.0.sock")
        );
        assert_eq!(
            cgi_socket_path("unix:/run/volucer/cgi", 11),
            PathBuf::from("/run/volucer/cgi.11.sock")
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
        let off = Config::from_toml("[server]\nhttp = \"127.0.0.1:8080\"\nstatic_files = false\n")
            .expect("config parses");
        assert!(!off.server.statics().enabled);
    }

    /// A FastCGI-only deployment never serves static content, and `volucer` refuses to
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
        for written in [
            "static_files = true\n",
            "static_files = { enabled = true }\n",
        ] {
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

    // ---- TLS -------------------------------------------------------------------------

    /// A config with TLS on `http`, so a test can vary one key at a time.
    fn tls_config(extra: &str) -> String {
        format!(
            r#"
[server]
http = "127.0.0.1:443"

[server.tls]
certificate = "certs/default.crt"
key = "certs/default.key"
{extra}
"#
        )
    }

    #[test]
    fn a_tls_table_with_both_paths_is_accepted() {
        let config = Config::from_toml(&tls_config("")).expect("parses");
        config.validate().expect("a certificate and key are enough");

        let tls = config.server.tls.as_ref().expect("tls present");
        assert_eq!(
            tls.min_version,
            MinTlsVersion::V1_2,
            "1.2 is the floor by default"
        );
        assert_eq!(tls.alpn, ["h2", "http/1.1"], "http/1.1 must not be dropped");
        assert!(!tls.plain, "accepting cleartext on a TLS port is opt-in");
        assert!(
            !config.server.redirect_to_https,
            "a redirect is a deployment's decision"
        );
    }

    #[test]
    fn a_half_a_certificate_pair_is_refused_rather_than_defaulted() {
        // Both keys are optional now, so the *absence* of `key` no longer fails to parse — it has to
        // be caught by validation instead. That is the right layer for the half-pair case either
        // way: "TLS on, a certificate, no key" is a table an operator very nearly finished, and the
        // error has to say which half is missing rather than that some field is required.
        let text = r#"
[server]
http = "127.0.0.1:443"

[server.tls]
certificate = "certs/default.crt"
"#;
        let config = Config::from_toml(text).expect("both keys are optional, so this parses");
        let err = config.validate().expect_err("half a pair is refused");
        let message = err.to_string();
        assert!(
            message.contains("server.tls.key") && message.contains("name both"),
            "the error must name the missing half and the two ways out: {message}"
        );
    }

    /// A generated placeholder is only allowed when something will replace it.
    ///
    /// The rule that makes the optional pair safe rather than convenient. Without it, a
    /// `[server.tls]` with no certificate and no ACME table would start, generate one, and serve a
    /// self-signed certificate for ever — a listener that looks healthy and is trusted by nobody,
    /// which is hazard 56's failure with the error moved from startup to the client's browser.
    #[test]
    fn a_pair_left_out_needs_an_acme_table_to_replace_it() {
        let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]
"#;
        let config = Config::from_toml(text).expect("parses");
        let err = config
            .validate()
            .expect_err("nothing would replace a generated placeholder");
        let message = err.to_string();
        assert!(
            message.contains("server.tls.certificate") && message.contains("[server.acme]"),
            "the error must name what is absent and what would make it legal: {message}"
        );
    }

    /// A site's own certificate beside an ACME table is **allowed**, matching the server level.
    ///
    /// This reverses the rule this test used to pin. The two levels disagreed: `[server.tls]` with a
    /// pair plus `[server.acme]` was accepted, while the same shape at site level was refused as "two
    /// answers to one question". But it is not two answers — it is one answer now and another when the
    /// order completes, which is what the machinery already does: `CertificateSet` gives such a site
    /// no generated placeholder, and an issued certificate overwrites the named entry for the names it
    /// covers. It is also how a deployment migrates from a certificate issued elsewhere, which the
    /// server level has always allowed. The rule that should hold at both levels is the absence of an
    /// ACME table, not the absence of a certificate.
    #[test]
    fn a_site_may_name_a_certificate_and_ask_for_one_at_the_same_time() {
        let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]
certificate = "certs/default.crt"
key = "certs/default.key"

[sites.corp]
hosts = ["corp.internal"]
document_root = "/srv/corp"

[sites.corp.tls]
certificate = "certs/corp.crt"
key = "certs/corp.key"

[sites.corp.acme]
domains = ["corp.internal"]
"#;
        let config = Config::from_toml(text).expect("parses");
        config.validate().expect(
            "the named pair serves until the order replaces it, as it does at server level",
        );
    }

    /// A site that names a certificate with no ACME table beside it is not an order, and must be real.
    ///
    /// The half of the old rule that survives: the refusal was never about the pair, it was about the
    /// pair being mistaken for an order. With a table present the pair is a placeholder-to-be-replaced;
    /// without one it is the certificate, and has to exist.
    #[test]
    fn a_site_that_names_a_certificate_without_acme_is_not_an_order() {
        let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]
certificate = "certs/default.crt"
key = "certs/default.key"

[sites.corp]
hosts = ["corp.internal"]
document_root = "/srv/corp"

[sites.corp.tls]
certificate = "certs/corp.crt"
key = "certs/corp.key"
"#;
        let config = Config::from_toml(text).expect("parses");
        config
            .validate()
            .expect("a site with its own pair and no ACME table is an ordinary deployment");
        assert!(
            config.sites["corp"].acme.is_none(),
            "which is to say: no order is built for it"
        );
    }

    /// A **disabled** site table does not count as something that will replace a generated
    /// placeholder.
    ///
    /// The invariant from hazard 72 has to reach `enabled`, or it is a hole: a deployment would be
    /// allowed to leave the pair out on the strength of a table that orders nothing, and then serve a
    /// self-signed certificate for ever — the port up, every handshake completing, and no client
    /// trusting any of them.
    #[test]
    fn a_disabled_site_table_cannot_replace_a_generated_placeholder() {
        let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[sites.corp]
hosts = ["corp.example.com"]
document_root = "/srv/corp"

[sites.corp.acme]
enabled = false
"#;
        let config = Config::from_toml(text).expect("parses");
        let err = config
            .validate()
            .expect_err("a table that issues nothing replaces nothing");
        let message = err.to_string();
        assert!(
            message.contains("server.tls.certificate"),
            "the error must name what is absent: {message}"
        );
    }

    /// A name in two ACME tables is refused, naming both.
    ///
    /// The failure this prevents cannot be seen in the configuration and cannot be recovered from at
    /// runtime. A validation handshake carries only the SNI name, so the deployment's challenge source
    /// answers first — with a challenge certificate whose `acmeIdentifier` commits to the
    /// *deployment's* key authorization, while the CA is asking this server for the *site's*. The
    /// site's order fails validation on every attempt and can only retry.
    #[test]
    fn a_name_issued_for_by_two_tables_is_refused_naming_both() {
        let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[server.acme]
domains = ["corp.internal", "example.com"]
contact = ["mailto:ops@example.com"]

[sites.corp]
hosts = ["corp.internal"]
document_root = "/srv/corp"

[sites.corp.acme]
"#;
        let config = Config::from_toml(text).expect("parses");
        let err = config
            .validate()
            .expect_err("one name belongs to one order");
        let message = err.to_string();
        assert!(
            message.contains("server.acme") && message.contains("sites.corp.acme"),
            "the error must name both tables, so the reader knows which to edit: {message}"
        );
        assert!(
            message.contains("corp.internal"),
            "and the name they both claim: {message}"
        );
    }

    /// A site that states its own `domains` is the way out of that refusal, and it works.
    ///
    /// Nothing changed in the derivation to achieve this — stating the names was always a complete
    /// replacement rather than a merge. The point of the test is that the *refusal* is escapable by
    /// the edit the error message suggests, which is what makes it a good error rather than a wall.
    #[test]
    fn a_site_that_states_its_own_domains_no_longer_overlaps_the_deployment() {
        let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[server.acme]
domains = ["corp.internal", "example.com"]
contact = ["mailto:ops@example.com"]

[sites.corp]
hosts = ["corp.internal"]
document_root = "/srv/corp"

[sites.corp.acme]
domains = ["corp.example.com"]
"#;
        let config = Config::from_toml(text).expect("parses");
        config
            .validate()
            .expect("the site asks for a name the deployment does not");
    }

    /// A deployment-wide table that only stages settings issues nothing, so an empty `domains` is
    /// legal on it — and a site's table still issues.
    #[test]
    fn a_disabled_server_table_is_defaults_only() {
        let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[server.acme]
enabled = false
contact = ["mailto:ops@example.com"]
authority = "production"

[sites.corp]
hosts = ["corp.example.com"]
document_root = "/srv/corp"

[sites.corp.acme]
"#;
        let config = Config::from_toml(text).expect("parses");
        config
            .validate()
            .expect("a disabled table stages contact and authority without ordering");

        let server = config.server.acme.as_ref().expect("the table is kept");
        assert!(
            server.domains.is_empty(),
            "and it names no domains, since it issues for none"
        );
        assert!(
            !server.enabled,
            "the switch is what makes the empty list legal rather than an error"
        );

        // The site still issues — the flag is not inherited — and it inherits the *settings*.
        let site = config.sites["corp"].acme.as_ref().expect("site table");
        assert!(
            site.issues(),
            "a disabled deployment does not silence a site"
        );
        assert_eq!(
            site.resolved_authority(&config.server.acme),
            Authority::Production,
            "but the deployment's authority is still what the site talks to"
        );
        assert_eq!(
            site.resolved_contact(&config.server.acme),
            ["mailto:ops@example.com"],
            "and its contact, which is what puts several sites on one account"
        );
    }

    /// A disabled site table keeps its settings and orders nothing.
    #[test]
    fn a_disabled_site_table_stops_the_order_and_keeps_the_settings() {
        let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]
certificate = "certs/default.crt"
key = "certs/default.key"

[sites.corp]
hosts = ["corp.example.com"]
document_root = "/srv/corp"

[sites.corp.acme]
enabled = false
domains = ["corp.example.com"]
"#;
        let config = Config::from_toml(text).expect("parses");
        config
            .validate()
            .expect("a disabled site table is not an order, so nothing has to replace anything");

        let site = config.sites["corp"]
            .acme
            .as_ref()
            .expect("the table is kept");
        assert!(!site.issues(), "nothing is ordered");
        let (names, _) = site.resolve_domains(&config.sites["corp"].hosts);
        assert_eq!(
            names,
            ["corp.example.com"],
            "and the settings are still there, which is the point of a switch over a deletion"
        );
    }

    /// `enabled` is not inherited, and that is the rule rather than an oversight.
    ///
    /// A site that inherited `false` would go quiet when the *deployment's* orders were turned off,
    /// and the deployments with several unrelated hostnames — the ones with several orders — are
    /// exactly the ones that cannot afford it. So the flag is per table, and silence has to be said
    /// where it is wanted.
    #[test]
    fn a_disabled_server_table_does_not_silence_a_site() {
        let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[server.acme]
enabled = false

[sites.corp]
hosts = ["corp.example.com"]
document_root = "/srv/corp"

[sites.corp.acme]
"#;
        let config = Config::from_toml(text).expect("parses");
        config.validate().expect("the site is still an order");

        assert!(
            !config.server.acme.as_ref().expect("table").enabled,
            "the deployment is off"
        );
        assert!(
            config.sites["corp"].acme.as_ref().expect("table").issues(),
            "and the site is not, because the flag belongs to the table that states it"
        );
    }

    /// A disabled table with a *bad* setting is still refused, because a site may inherit it.
    ///
    /// The settings do not stop being in force when ordering does: a site that omits `contact` gets
    /// the deployment's, and an account registered with a malformed one fails at the CA after a round
    /// trip. So the format checks run either way, and only the `domains` rule is skipped.
    #[test]
    fn a_disabled_table_still_has_its_settings_checked() {
        let text = r#"
[server]
http = "127.0.0.1:443"
document_root = "/srv/app"

[server.tls]

[server.acme]
enabled = false
contact = ["ops@example.com"]

[sites.corp]
hosts = ["corp.example.com"]
document_root = "/srv/corp"

[sites.corp.acme]
"#;
        let config = Config::from_toml(text).expect("parses");
        let err = config
            .validate()
            .expect_err("a bare address would be sent as an unknown URI scheme");
        assert!(
            err.to_string().contains("mailto:"),
            "the error must name the fix: {err}"
        );
    }

    #[test]
    fn https_without_a_tls_table_is_refused_by_name() {
        let text = r#"
[server]
http = "127.0.0.1:80"
https = "127.0.0.1:443"
"#;
        let config = Config::from_toml(text).expect("parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("server.https"), "{err}");
        assert!(err.to_string().contains("[server.tls]"), "{err}");
    }

    #[test]
    fn a_tls_table_with_no_listener_carrying_it_is_refused() {
        let text = r#"
[server]
fastcgi = "127.0.0.1:9000"

[server.tls]
certificate = "c.crt"
key = "c.key"
"#;
        let config = Config::from_toml(text).expect("parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("no listener carries it"), "{err}");
    }

    #[test]
    fn plain_and_https_together_are_refused() {
        // With a dedicated HTTPS port, cleartext already has a home: the `http` listener.
        // The sniff would have nothing to decide, so this is a contradiction rather than a
        // harmless redundancy.
        let text = r#"
[server]
http = "127.0.0.1:80"
https = "127.0.0.1:443"

[server.tls]
certificate = "c.crt"
key = "c.key"
plain = true
"#;
        let config = Config::from_toml(text).expect("parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("plain"), "{err}");
        assert!(err.to_string().contains("server.https"), "{err}");
    }

    #[test]
    fn http_and_https_on_the_same_address_are_refused() {
        let text = r#"
[server]
http = "127.0.0.1:443"
https = "127.0.0.1:443"

[server.tls]
certificate = "c.crt"
key = "c.key"
"#;
        let config = Config::from_toml(text).expect("parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("two listeners"), "{err}");
    }

    #[test]
    fn an_unknown_alpn_protocol_is_refused_by_name() {
        // Accepted by rustls and then never selected by anyone, which would leave a
        // listener that looks configured and answers only what a client offered.
        let config = Config::from_toml(&tls_config(r#"alpn = ["h2", "spdy/3"]"#)).expect("parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("spdy/3"), "{err}");
    }

    #[test]
    fn h2_without_http11_is_allowed_but_warned_about() {
        // Not fatal: a deployment may deliberately serve only HTTP/2. It is nearly always
        // a mistake though, because a client that cannot do h2 fails the handshake rather
        // than being served HTTP/1.1 — so it validates and the warning is the signal.
        let config = Config::from_toml(&tls_config(r#"alpn = ["h2"]"#)).expect("parses");
        config
            .validate()
            .expect("h2 alone is a deliberate choice, not an error");
    }

    #[test]
    fn a_per_site_tls_table_needs_the_server_one_first() {
        let text = r#"
[server]
http = "127.0.0.1:443"

[sites.shop]
hosts = ["shop.test"]
document_root = "/srv/shop"

[sites.shop.tls]
certificate = "shop.crt"
key = "shop.key"
"#;
        let config = Config::from_toml(text).expect("parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("shop.tls"), "{err}");
        assert!(err.to_string().contains("server.tls"), "{err}");
    }

    #[test]
    fn a_per_site_certificate_overrides_the_servers() {
        let text = r#"
[server]
http = "127.0.0.1:443"

[server.tls]
certificate = "default.crt"
key = "default.key"

[sites.shop]
hosts = ["shop.test"]
document_root = "/srv/shop"

[sites.shop.tls]
certificate = "shop.crt"
key = "shop.key"
"#;
        let config = Config::from_toml(text).expect("parses");
        config
            .validate()
            .expect("a per-site certificate is the SNI case");

        let site = config.sites.get("shop").expect("site present");
        assert_eq!(
            site.tls.as_ref().and_then(|t| t.certificate.as_deref()),
            Some(Path::new("shop.crt")),
            "the site states its own certificate"
        );
    }

    #[test]
    fn a_per_site_handshake_key_is_refused_rather_than_ignored() {
        // `min_version` describes the socket, and SNI cannot change what the socket did
        // before the request existed. Accepting it would let a site claim something the
        // listener does not do.
        let text = r#"
[server]
http = "127.0.0.1:443"

[server.tls]
certificate = "default.crt"
key = "default.key"

[sites.shop]
hosts = ["shop.test"]
document_root = "/srv/shop"

[sites.shop.tls]
certificate = "shop.crt"
key = "shop.key"
min_version = "1.3"
"#;
        let err = Config::from_toml(text).expect_err("unknown key in the site tls table");
        assert!(err.to_string().contains("min_version"), "{err}");
    }

    #[test]
    fn a_redirect_needs_something_serving_cleartext() {
        // With TLS on `http` and `plain` off, that socket never answers cleartext, so the
        // key could never fire — refused rather than silently inert.
        let text = r#"
[server]
http = "127.0.0.1:443"
redirect_to_https = true

[server.tls]
certificate = "c.crt"
key = "c.key"
"#;
        let config = Config::from_toml(text).expect("parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("redirect_to_https"), "{err}");
        assert!(err.to_string().contains("cleartext"), "{err}");
    }

    #[test]
    fn a_redirect_is_accepted_when_a_plaintext_listener_exists() {
        // Two shapes qualify: a dedicated `https` listener beside a plain `http` one, or
        // TLS on `http` with `plain = true`.
        let with_https = r#"
[server]
http = "127.0.0.1:80"
https = "127.0.0.1:443"
redirect_to_https = true

[server.tls]
certificate = "c.crt"
key = "c.key"
"#;
        Config::from_toml(with_https)
            .expect("parses")
            .validate()
            .expect("a dedicated https listener leaves http plaintext");

        let with_plain = r#"
[server]
http = "127.0.0.1:443"
redirect_to_https = true

[server.tls]
certificate = "c.crt"
key = "c.key"
plain = true
"#;
        Config::from_toml(with_plain)
            .expect("parses")
            .validate()
            .expect("plain = true gives the redirect something to fire on");
    }

    #[test]
    fn redirect_without_tls_at_all_is_refused() {
        let text = r#"
[server]
http = "127.0.0.1:8080"
redirect_to_https = true
"#;
        let config = Config::from_toml(text).expect("parses");
        let err = config.validate().expect_err("must fail");
        assert!(err.to_string().contains("redirect_to_https"), "{err}");
    }

    #[test]
    fn a_plaintext_deployment_is_unchanged_by_any_of_this() {
        // The control: no `[server.tls]`, no `https`, no redirect — exactly the config
        // shape that existed before TLS, and it must still validate and carry no TLS.
        let config = Config::from_toml("[server]\nhttp = \"127.0.0.1:8080\"\n").expect("parses");
        config.validate().expect("valid");
        assert!(config.server.tls.is_none());
        assert!(config.server.https.is_none());
        assert!(!config.server.redirect_to_https);
    }

    #[test]
    fn has_plaintext_listener_matches_the_documented_matrix() {
        let bare = Config::from_toml("[server]\nhttp = \"127.0.0.1:8080\"\n").expect("parses");
        assert!(!bare.server.has_plaintext_listener());

        let tls_only = Config::from_toml(&tls_config("")).expect("parses");
        assert!(
            !tls_only.server.has_plaintext_listener(),
            "h2-only TLS on http"
        );

        let tls_plain = Config::from_toml(&tls_config("plain = true")).expect("parses");
        assert!(tls_plain.server.has_plaintext_listener());

        let both = Config::from_toml(
            r#"
[server]
http = "127.0.0.1:80"
https = "127.0.0.1:443"

[server.tls]
certificate = "c.crt"
key = "c.key"
"#,
        )
        .expect("parses");
        assert!(both.server.has_plaintext_listener());

        let https_only = Config::from_toml(
            r#"
[server]
https = "127.0.0.1:443"

[server.tls]
certificate = "c.crt"
key = "c.key"
"#,
        )
        .expect("parses");
        assert!(!https_only.server.has_plaintext_listener(), "TLS only");
        https_only
            .validate()
            .expect("a TLS-only deployment is valid");
    }

    // -----------------------------------------------------------------------
    // The upstream backend's configuration surface.
    // -----------------------------------------------------------------------

    /// A config with one upstream backend, for the tests below.
    fn upstream_config(backend_body: &str) -> String {
        format!(
            r#"
default_backend = "node"
[server]
http = "127.0.0.1:8080"
[backends.node]
kind = "upstream"
{backend_body}
"#
        )
    }

    #[test]
    fn an_upstream_backend_parses_and_names_itself() {
        let config = Config::from_toml(&upstream_config("origin = \"http://127.0.0.1:3000\""))
            .expect("parses");
        config.validate().expect("validates");
        let backend = &config.backends["node"];
        assert_eq!(backend.kind(), "upstream");
        // The third answer, which is the reason `request_scope` is an enum: neither PHP's SAPI nor
        // an application-scoped worker describes somebody else's runtime.
        assert_eq!(backend.request_scope(), RequestScope::OriginScoped);
        assert!(!backend.resets_per_request());
        // And the question the route table's `stat` turns on, which is *not* the negation of the
        // line above: a worker and an upstream both skip it, for two different reasons.
        assert!(!backend.interprets_path());
    }

    #[test]
    fn an_upstream_is_restart_only_like_every_other_backend() {
        // A URL looks reloadable in a way a `command` does not, which is why the README says so and
        // why the restart-only list names backends as a class. Pinned here so a later change that
        // made an origin reloadable has to decide about this deliberately: the *only* thing
        // `preflight` has to say about a loopback origin is nothing.
        let warnings = crate::preflight::inspect(
            &Config::from_toml(&upstream_config("origin = \"http://127.0.0.1:3000\""))
                .expect("parses"),
            std::path::Path::new("."),
        );
        assert!(
            !warnings.iter().any(|w| w.message.contains("backends.node")),
            "a loopback origin raises nothing of its own: {warnings:?}"
        );
    }

    /// Every origin shape that is accepted, and that the parse resolves what it should.
    ///
    /// A named row type rather than a tuple: three fields of different shapes in a slice literal
    /// is exactly the thing a reader has to count on their fingers.
    #[test]
    fn the_four_origin_shapes_are_accepted_and_resolved() {
        struct Case {
            spec: &'static str,
            scheme: &'static str,
            host_port: Option<(&'static str, u16)>,
            tls: bool,
        }
        let cases: &[Case] = &[
            Case {
                spec: "http://127.0.0.1:3000",
                scheme: "http",
                host_port: Some(("127.0.0.1", 3000)),
                tls: false,
            },
            Case {
                spec: "http://[::1]:3000",
                scheme: "http",
                host_port: Some(("[::1]", 3000)),
                tls: false,
            },
            // No port: the scheme's default, resolved once here rather than at dial time, so an
            // error message names the port that was actually chosen.
            Case {
                spec: "http://origin.test",
                scheme: "http",
                host_port: Some(("origin.test", 80)),
                tls: false,
            },
            Case {
                spec: "https://origin.test",
                scheme: "https",
                host_port: Some(("origin.test", 443)),
                tls: true,
            },
            Case {
                spec: "https://10.0.0.5:8443",
                scheme: "https",
                host_port: Some(("10.0.0.5", 8443)),
                tls: true,
            },
        ];
        for case in cases {
            let spec = case.spec;
            let origin = OriginSpec::parse(spec, None)
                .unwrap_or_else(|err| panic!("{spec} must parse: {err}"));
            assert_eq!(origin.scheme(), case.scheme, "{spec}");
            assert_eq!(
                origin.host_port(),
                case.host_port.map(|(h, p)| (h.to_string(), p)),
                "{spec}"
            );
            assert_eq!(origin.is_tls(), case.tls, "{spec}");
        }

        #[cfg(unix)]
        {
            let origin = OriginSpec::parse("unix:/run/node.sock", None).expect("parses");
            assert_eq!(origin.scheme(), "unix");
            assert_eq!(origin.host_port(), None);
            assert_eq!(
                origin.socket_path(),
                Some(std::path::Path::new("/run/node.sock"))
            );
            // A socket leaves the host, so there is nothing for a plaintext warning to be about —
            // which is the same answer `is_local` gives a loopback address and the opposite of what
            // it gives `http://10.0.0.5`.
            assert!(origin.is_local());
        }
        assert!(OriginSpec::parse("http://127.0.0.1:3000", None)
            .expect("parses")
            .is_local());
        assert!(OriginSpec::parse("http://localhost:3000", None)
            .expect("parses")
            .is_local());
        assert!(!OriginSpec::parse("http://10.0.0.5:3000", None)
            .expect("parses")
            .is_local());
    }

    /// The shapes that are refused, each for the reason it names.
    ///
    /// Table-driven because the rules are independent: one case per rule, each asserting the
    /// *reason*. A test that asserted "some error" for all of them would pass against one
    /// over-broad rule.
    #[test]
    fn an_origin_that_cannot_be_used_is_refused_with_its_reason() {
        // Built rather than written out, because a userinfo-looking literal is exactly what a
        // secret scanner is for — and a test whose input depends on a scanner's opinion is a test
        // that can silently change meaning. The parse only has to see an `@` before the path.
        let with_userinfo = format!("http://{}@{}", "user", "127.0.0.1:3000");
        let cases: Vec<(&str, &[&str])> = vec![
            // A bare `host:port` is the dangerous one: read as plaintext it would be a silent
            // downgrade, so it is refused rather than guessed at.
            ("127.0.0.1:3000", &["no scheme"]),
            ("", &["empty"]),
            ("http://", &["no host"]),
            ("http:///api", &["no host"]),
            ("http://:3000", &["no host"]),
            ("ftp://127.0.0.1:3000", &["ftp", "unix"]),
            // A base path would have to be joined with the request's own, and the join rule is not
            // guessable — so it is refused with the alternative rather than silently dropped.
            ("http://127.0.0.1:3000/app", &["/app", "route table"]),
            ("http://127.0.0.1:3000/?x=1", &["route table"]),
            (with_userinfo.as_str(), &["credentials"]),
            // Brackets are what disambiguate an IPv6 literal's colons from the port's, so a bare
            // one is refused with the shape to write instead.
            ("http://::1:3000", &["brackets"]),
            ("http://[::1:3000", &["without closing"]),
            ("http://origin.test:notaport", &["port"]),
        ];
        for (spec, expected) in cases {
            let err = OriginSpec::parse(spec, None)
                .err()
                .unwrap_or_else(|| panic!("{spec} must be refused"));
            for needle in expected.iter() {
                assert!(
                    err.contains(needle),
                    "{spec}: {err:?} must mention {needle:?}"
                );
            }
        }
    }

    #[test]
    fn a_tls_table_beside_a_plaintext_or_socket_origin_is_refused() {
        // The one thing worse than no verification is verification the operator believes in: a
        // `[backends.n.tls]` table beside `http://` or `unix:` would describe a handshake that
        // never happens, and it would be silently ignored if the parse did not take it.
        for origin in ["http://127.0.0.1:3000", "unix:/run/node.sock"] {
            let spec = format!("{origin:?}");
            let text = upstream_config(&format!(
                "origin = {spec}\n[backends.node.tls]\nca = \"ca.pem\"\n"
            ));
            let config = Config::from_toml(&text).expect("parses");
            let err = config.validate().expect_err("must fail");
            let message = err.to_string();
            assert!(message.contains("backends.node"), "{origin}: {message}");
            assert!(
                message.contains("handshake"),
                "the error must say why the table has no meaning: {origin}: {message}"
            );
        }
    }

    #[test]
    fn an_https_origin_accepts_its_tls_table() {
        let text = r#"
default_backend = "node"
[server]
http = "127.0.0.1:8080"
[backends.node]
kind = "upstream"
origin = "https://node.internal:8443"
[backends.node.tls]
ca = "/etc/ssl/internal-ca.pem"
sni = "node.internal"
"#;
        let config = Config::from_toml(text).expect("parses");
        config.validate().expect("validates");
        let origin = config.backends["node"]
            .origin()
            .expect("an upstream has an origin")
            .expect("the origin parses");
        let tls = origin.tls().expect("https has tls settings");
        assert_eq!(tls.sni.as_deref(), Some("node.internal"));
        assert_eq!(
            tls.ca.as_deref(),
            Some(std::path::Path::new("/etc/ssl/internal-ca.pem"))
        );
    }

    #[test]
    fn an_empty_tls_key_is_refused_rather_than_read_as_absent() {
        // TOML cannot tell `""` from a missing key, and an empty CA path would otherwise become
        // "the public roots" — a silent change of what the hop is verified against.
        let cases: &[(&str, &str)] = &[
            ("ca = \"\"", "tls.ca is empty"),
            ("sni = \"\"", "tls.sni is empty"),
        ];
        for (body, expected) in cases {
            let text = upstream_config(&format!(
                "origin = \"https://node.internal:8443\"\n[backends.node.tls]\n{body}\n"
            ));
            let config = Config::from_toml(&text).expect("parses");
            let err = config.validate().expect_err("must fail");
            assert!(err.to_string().contains(expected), "{body}: {err}");
        }
    }

    #[test]
    fn an_sni_that_is_not_a_name_is_refused_at_validation() {
        // A verification name is a DNS name; a port or a scheme inside one is a mistake worth
        // naming where the file is read, rather than a handshake error on the first request.
        for bad in [
            "node.internal:8443",
            "https://node.internal",
            "node..internal",
            "-",
        ] {
            let text = upstream_config(&format!(
                "origin = \"https://10.0.0.5:8443\"\n[backends.node.tls]\nsni = {bad:?}\n"
            ));
            let config = Config::from_toml(&text).expect("parses");
            let err = config.validate().expect_err("must fail");
            assert!(err.to_string().contains("not a host name"), "{bad}: {err}");
        }
        // A bare IP is a name a certificate can carry, since a certificate may hold an address.
        let text = upstream_config(
            "origin = \"https://10.0.0.5:8443\"\n[backends.node.tls]\nsni = \"10.0.0.5\"\n",
        );
        Config::from_toml(&text)
            .expect("parses")
            .validate()
            .expect("an IP is a usable verification name");
    }

    #[test]
    fn an_upstream_needs_an_http_listener() {
        // There is no CGI environment to build for an upstream and no way for the FastCGI
        // front-end to hand it a request, so without an HTTP listener the backend starts, the
        // sockets bind, and nothing ever reaches the origin. Silent otherwise, so it is an error.
        let text = r#"
default_backend = "node"
[server]
fastcgi = "127.0.0.1:9080"
[backends.node]
kind = "upstream"
origin = "http://127.0.0.1:3000"
"#;
        let config = Config::from_toml(text).expect("parses");
        let err = config.validate().expect_err("must fail");
        let message = err.to_string();
        assert!(message.contains("backends.node"), "{message}");
        assert!(message.contains("server.http"), "{message}");
    }

    #[test]
    fn the_three_silent_readers_answer_correctly_for_an_upstream() {
        // Three places accepted the new kind and described it wrongly, none of them a compile
        // error. Two are pure functions and are pinned here; the third is `check`'s output and is
        // asserted at the CLI level, where it is actually printed.
        let config = Config::from_toml(&upstream_config("origin = \"http://127.0.0.1:3000\""))
            .expect("parses");
        let backend = &config.backends["node"];

        // 1. What `check` prints for the per-request-reset line. Neither of the two strings that
        //    existed before is true of an upstream.
        let line = backend.request_scope().describe();
        assert!(line.contains("not applicable"), "{line}");
        assert!(!line.contains("PHP SAPI"), "{line}");
        assert!(!line.contains("application-scoped"), "{line}");

        // 2. Whether a route to it skips the router's `stat`. The path is the origin's business —
        //    there is no script for volucer to resolve — so it must.
        assert!(
            !backend.interprets_path(),
            "an upstream routes on the URL, so the path is not a file to stat"
        );
    }

    #[test]
    fn an_upstream_and_a_worker_skip_the_stat_for_different_reasons() {
        // The rule `skip_stat` asks about, stated once for all three shapes: the answer is not
        // "is it a worker", because an upstream reaches the same answer from the other side.
        let config = Config::from_toml(&upstream_config("origin = \"http://127.0.0.1:3000\""))
            .expect("parses");
        let worker = Config::from_toml(
            r#"
[server]
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
"#,
        )
        .expect("parses");
        let cgi = Config::from_toml(
            r#"
[server]
http = "127.0.0.1:8080"
[backends.cgi]
kind = "spawn_cgi"
binary = "/usr/bin/php-cgi"
"#,
        )
        .expect("parses");

        let prefix = |config: &Config, name: &str| {
            let route = RouteConfig {
                host: None,
                path_prefix: Some("/legacy".to_string()),
                path_suffix: None,
                backend: name.to_string(),
            };
            skip_stat(&route, config.backends[name].interprets_path())
        };

        assert!(
            prefix(&config, "node"),
            "an upstream does not read the path"
        );
        assert!(prefix(&worker, "app"), "a worker does not read the path");
        assert!(
            !prefix(&cgi, "cgi"),
            "php-cgi emits the file it is pointed at, so the stat is the guard"
        );
    }

    #[test]
    fn the_removed_keys_of_other_backends_are_still_refused() {
        // `deny_unknown_fields`, checked for the new arm specifically: a `kind = "upstream"` file
        // carrying `command` or `workers` is a copy-paste from another backend, and it should be a
        // parse error rather than a key that is silently ignored.
        for key in [
            "command = \"php worker.php\"",
            "workers = 4",
            "binary = \"/usr/bin/php\"",
        ] {
            let text = upstream_config(&format!("origin = \"http://127.0.0.1:3000\"\n{key}"));
            assert!(
                Config::from_toml(&text).is_err(),
                "{key} must not be accepted by an upstream backend"
            );
        }
    }
}
