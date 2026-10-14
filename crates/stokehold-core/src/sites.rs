//! Choosing a site from a `Host` header.
//!
//! This is the piece that makes vhosts work. Everything else about a request is
//! already decided per-process: `Routing` holds *one* document root, and the
//! router resolves `SCRIPT_FILENAME` from it before any backend is chosen. A
//! `host` matcher in the route table is therefore too late to change the root —
//! it can only pick a backend.
//!
//! So the host has to be resolved *before* the routing, and that is what this
//! module does. It returns configuration rather than a `Routing` on purpose:
//! `Routing` lives in `stokehold-fcgi`, which depends on this crate, so naming it
//! here would be a cycle. The front-end turns the answer into a `Routing` once, at
//! startup, and holds one per site.
//!
//! Deliberately free of the filesystem: every decision here is about strings the
//! configuration already contains, which is what makes it testable without a
//! fixture on disk.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::{Config, RouteConfig, ServeOnly, SiteConfig, StaticConfig};

/// The settings a request is served with, after a host has been resolved.
///
/// Borrowed from the configuration where possible, so building one per request
/// would be cheap — but the front-end builds them once at startup and keeps them,
/// because the `Routing` it derives from this must be prebuilt rather than
/// reconstructed on the hot path.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSite {
    /// The name this site is written under, or `"<default>"` for `[server]`.
    pub name: String,
    /// The directory this site's scripts and files resolve against.
    pub document_root: PathBuf,
    /// The front controller, or `None` for "serve only real files".
    pub front_controller: Option<String>,
    /// What this site serves.
    pub serve_only: ServeOnly,
    /// The static layer, with inheritance already applied.
    pub static_files: StaticConfig,
    /// The route table in effect for this site.
    pub routes: Vec<RouteConfig>,
    /// The backend a request matching no route goes to.
    pub default_backend: Option<String>,
}

/// The site table, with the `[server]` values it falls back to.
///
/// Built once from a validated [`Config`]. When `config.sites` is empty this
/// still produces exactly one site — the default one — so callers have a single
/// shape to handle and the single-site deployment is not a special case in the
/// request path.
#[derive(Debug, Clone)]
pub struct Sites {
    /// The default site, from `[server]`.
    default: ResolvedSite,
    /// Every configured site by name, so `stokehold check` can list them.
    named: BTreeMap<String, ResolvedSite>,
    /// Host pattern (lowercased) to site name, exact names first in the sense that
    /// an exact match is looked up before a wildcard one.
    exact: BTreeMap<String, String>,
    /// Wildcard patterns as `(suffix, site name)`, longest suffix first so that a
    /// `*.a.example.com` rule beats a `*.example.com` rule for the same host.
    wildcard: Vec<(String, String)>,
}

impl Sites {
    /// Resolve the site table, applying inheritance from `[server]`.
    ///
    /// `document_root` is the root the command line resolved, since the CLI
    /// absolutises it and a site must not be resolved against a relative path. A
    /// site always states its own root, so this is only the default site's.
    pub fn from_config(config: &Config, document_root: &Path) -> Self {
        Self::from_config_with(config, document_root, None)
    }

    /// [`Sites::from_config`], with the front controller the command line resolved.
    ///
    /// The flag describes the *default* site, so it is applied to that one and to sites
    /// that inherit: a site stating its own controller keeps it, which is what "a site
    /// states its own" means.
    pub fn from_config_with(
        config: &Config,
        document_root: &Path,
        front_controller_override: Option<&str>,
    ) -> Self {
        // Through `resolve_front_controller`, so the default (`index.php`) is applied
        // here rather than left to the CLI. Otherwise a site that omits the key would
        // get no controller while the single-site path got `index.php` — the same
        // deployment behaving two ways depending on which front-end read it.
        let default_front = config
            .server
            .resolve_front_controller(front_controller_override);
        let default_statics = config.server.statics();

        let default = ResolvedSite {
            name: "<default>".to_string(),
            document_root: document_root.to_path_buf(),
            front_controller: default_front.clone(),
            serve_only: config.server.serve_only,
            static_files: default_statics.clone(),
            routes: config.routes.clone(),
            default_backend: config.default_backend.clone(),
        };

        let mut named = BTreeMap::new();
        let mut exact = BTreeMap::new();
        let mut wildcard = Vec::new();

        for (name, site) in &config.sites {
            let resolved = resolve_site(
                name,
                site,
                default_front.as_deref(),
                config.server.serve_only,
                &default_statics,
                &config.routes,
                config.default_backend.as_deref(),
            );

            for host in &site.hosts {
                let host = host.trim().to_ascii_lowercase();
                match host.strip_prefix("*.") {
                    None => {
                        exact.insert(host, name.clone());
                    }
                    Some(suffix) => wildcard.push((suffix.to_string(), name.clone())),
                }
            }

            named.insert(name.clone(), resolved);
        }

        // Longest suffix first, so the most specific wildcard wins when two match.
        // `sort_by_key` on the reversed length is stable, so two patterns of equal
        // length keep declaration order — which for a `BTreeMap` means name order,
        // and that is at least predictable.
        wildcard.sort_by_key(|(suffix, _)| std::cmp::Reverse(suffix.len()));

        Sites {
            default,
            named,
            exact,
            wildcard,
        }
    }

    /// The default site: what `[server]` describes.
    pub fn default_site(&self) -> &ResolvedSite {
        &self.default
    }

    /// Every configured site by name, excluding the default.
    pub fn named(&self) -> &BTreeMap<String, ResolvedSite> {
        &self.named
    }

    /// The site that answers a `Host` header.
    ///
    /// The rule is the whole rule, so a reader of the file can predict it: an
    /// exact name wins, then the longest matching `*.` wildcard, then the default
    /// site. A host with a port, and a host in any case, matches the same as one
    /// without — the `Host` header is a client's word and neither a port nor a case
    /// is part of the name a site claims.
    pub fn resolve(&self, host: Option<&str>) -> &ResolvedSite {
        let Some(host) = host else {
            return &self.default;
        };
        self.resolve_str(host)
    }

    /// [`Sites::resolve`], taking the header directly.
    pub fn resolve_str(&self, host: &str) -> &ResolvedSite {
        let host = strip_port(host);
        if host.is_empty() {
            return &self.default;
        }
        let host = host.to_ascii_lowercase();

        if let Some(name) = self.exact.get(&host) {
            // A name in the table is a name in `named`, because both were filled
            // from the same map in the same pass.
            if let Some(site) = self.named.get(name.as_str()) {
                return site;
            }
        }

        for (suffix, name) in &self.wildcard {
            if crate::config::host_matches(&format!("*.{suffix}"), Some(host.as_str())) {
                if let Some(site) = self.named.get(name.as_str()) {
                    return site;
                }
            }
        }

        &self.default
    }
}

/// Apply the fallbacks that turn a [`SiteConfig`] into a [`ResolvedSite`].
///
/// One place, so a site that omits a key and a site that repeats the global value
/// cannot end up with different settings.
#[allow(clippy::too_many_arguments)]
fn resolve_site(
    name: &str,
    site: &SiteConfig,
    default_front: Option<&str>,
    default_serve_only: ServeOnly,
    default_statics: &StaticConfig,
    default_routes: &[RouteConfig],
    default_backend: Option<&str>,
) -> ResolvedSite {
    let front_controller = match &site.front_controller {
        Some(controller) => controller.as_path().map(str::to_string),
        None => default_front.map(str::to_string),
    };

    let static_files = match &site.static_files {
        // Resolved against the server's layer, not against nothing: a site naming
        // one key must keep the root's other settings.
        Some(setting) => setting.applied_to(default_statics),
        None => default_statics.clone(),
    };

    ResolvedSite {
        name: name.to_string(),
        document_root: site.document_root.clone(),
        front_controller,
        serve_only: site.serve_only.unwrap_or(default_serve_only),
        static_files,
        routes: site
            .routes
            .clone()
            .unwrap_or_else(|| default_routes.to_vec()),
        default_backend: site
            .default_backend
            .clone()
            .or_else(|| default_backend.map(str::to_string)),
    }
}

/// A `Host` header without its `:port`, handling a bracketed IPv6 literal.
///
/// A copy of the rule the router and the route table use, kept next to the site
/// lookup so the two cannot drift; `config::host_matches` does the pattern
/// comparison, and this only removes the port before it.
fn strip_port(host: &str) -> &str {
    let host = host.trim();
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match host.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') && port.chars().all(|c| c.is_ascii_digit()) => {
            name
        }
        _ => host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        let config = Config::from_toml(text).expect("config parses");
        config.validate().expect("config validates");
        config
    }

    const TWO_SITES: &str = r#"
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
hosts = ["example.com", "www.example.com", "*.shop.example.com"]
document_root = "/srv/example/public"
default_backend = "app"

[sites.corp]
hosts = ["corp.internal", "*.corp.internal", "*.deep.corp.internal"]
document_root = "/srv/corp"
front_controller = false

  routes = [
      { path_suffix = ".php", backend = "legacy" },
  ]
"#;

    #[test]
    fn an_empty_site_table_is_one_default_site() {
        // The single-site deployment must not be a special case in the request
        // path, so it is the same shape with one entry in it.
        let config = config("[server]\nhttp = \"127.0.0.1:8080\"\n");
        let sites = Sites::from_config(&config, Path::new("/srv/app"));

        assert!(sites.named().is_empty());
        let site = sites.resolve(Some("anything.test"));
        assert_eq!(site.name, "<default>");
        assert_eq!(site.document_root, PathBuf::from("/srv/app"));
        assert_eq!(site.front_controller.as_deref(), Some("index.php"));
        assert_eq!(site.serve_only, ServeOnly::All);
    }

    #[test]
    fn an_exact_host_wins_then_the_longest_wildcard_then_the_default() {
        let config = config(TWO_SITES);
        let sites = Sites::from_config(&config, Path::new("/srv/default"));

        assert_eq!(sites.resolve_str("example.com").name, "example_com");
        assert_eq!(sites.resolve_str("www.example.com").name, "example_com");
        assert_eq!(sites.resolve_str("a.shop.example.com").name, "example_com");
        assert_eq!(sites.resolve_str("corp.internal").name, "corp");
        assert_eq!(sites.resolve_str("a.corp.internal").name, "corp");
        // The longest wildcard wins: `*.deep.corp.internal` beats `*.corp.internal`.
        assert_eq!(sites.resolve_str("x.deep.corp.internal").name, "corp");

        // Unlisted, so the default site answers.
        assert_eq!(sites.resolve_str("other.test").name, "<default>");
        assert_eq!(sites.resolve(None).name, "<default>");
    }

    #[test]
    fn a_host_is_matched_without_its_port_and_without_regard_to_case() {
        let config = config(TWO_SITES);
        let sites = Sites::from_config(&config, Path::new("/srv/default"));

        assert_eq!(sites.resolve_str("EXAMPLE.com:8080").name, "example_com");
        assert_eq!(sites.resolve_str("corp.internal:443").name, "corp");
        assert_eq!(sites.resolve_str("[::1]:8080").name, "<default>");
        assert_eq!(sites.resolve_str("").name, "<default>");
        // `*.shop.example.com` is a subdomain rule, so the bare name it hangs off
        // is not a match — it is a different site's or the default's.
        assert_eq!(sites.resolve_str("shop.example.com").name, "<default>");
    }

    #[test]
    fn a_site_inherits_every_key_it_omits() {
        let config = config(TWO_SITES);
        let sites = Sites::from_config(&config, Path::new("/srv/default"));

        // `example_com` states only hosts, root and default_backend, so the front
        // controller, serve_only, statics and routes come from `[server]`.
        let example = sites.resolve_str("example.com");
        assert_eq!(example.front_controller.as_deref(), Some("index.php"));
        assert_eq!(example.serve_only, ServeOnly::All);
        assert!(example.routes.is_empty());
        assert_eq!(
            example.static_files,
            config.server.statics(),
            "the inherited static layer must be the server's, not the defaults"
        );

        // `corp` states a boolean front controller and its own routes.
        let corp = sites.resolve_str("corp.internal");
        assert_eq!(corp.front_controller, None, "`false` means only real files");
        assert_eq!(corp.routes.len(), 1);
        assert_eq!(corp.routes[0].path_suffix.as_deref(), Some(".php"));
        assert_eq!(corp.default_backend.as_deref(), Some("app"));
    }

    #[test]
    fn a_site_static_setting_is_merged_key_by_key() {
        // A site naming one key keeps the root's other settings. All-or-nothing
        // inheritance would silently reset `cache_control` to the default here.
        let text = r#"
[server]
http = "127.0.0.1:8080"
document_root = "/srv/default"

[server.static_files]
enabled = true
cache_control = "public, max-age=60"

[sites.a]
hosts = ["a.test"]
document_root = "/srv/a"
static_files = { precompressed = true }
"#;
        let config = config(text);
        let sites = Sites::from_config(&config, Path::new("/srv/default"));
        let site = sites.resolve_str("a.test");

        assert!(site.static_files.precompressed, "the site's own key");
        assert!(
            site.static_files.enabled,
            "the root's enabled must survive the merge"
        );
        assert_eq!(
            site.static_files.cache_control, "public, max-age=60",
            "and so must the root's cache_control"
        );
    }

    #[test]
    fn a_site_can_turn_the_static_layer_off_while_the_root_has_it_on() {
        // The opt-out has to work in the direction that matters: a root serving
        // files, one application site that must not.
        let text = r#"
[server]
http = "127.0.0.1:8080"
document_root = "/srv/default"
static_files = { enabled = true }

[sites.api]
hosts = ["api.test"]
document_root = "/srv/api"
static_files = false
"#;
        let config = config(text);
        let sites = Sites::from_config(&config, Path::new("/srv/default"));

        assert!(sites.default_site().static_files.enabled);
        assert!(
            !sites.resolve_str("api.test").static_files.enabled,
            "a site must be able to opt out of a layer the root turned on"
        );
    }

    #[test]
    fn the_default_site_carries_the_servers_route_table() {
        // Routes on `[server]` apply to the default site, not to every site: a site
        // that omits `routes` inherits them, which is what makes one application on
        // several hosts one configuration.
        let text = r#"
default_backend = "app"

[server]
http = "127.0.0.1:8080"
document_root = "/srv/default"

[backends.app]
kind = "worker"
command = "php worker.php"

[backends.admin]
kind = "worker"
command = "php worker.php"

[[routes]]
path_prefix = "/admin"
backend = "admin"

[sites.a]
hosts = ["a.test"]
document_root = "/srv/a"
"#;
        let config = config(text);
        let sites = Sites::from_config(&config, Path::new("/srv/default"));

        assert_eq!(sites.default_site().routes.len(), 1);
        assert_eq!(
            sites.resolve_str("a.test").routes.len(),
            1,
            "a site that omits routes inherits the server's"
        );
    }
}
