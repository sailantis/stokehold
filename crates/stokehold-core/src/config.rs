//! Configuration model.
//!
//! The shape mirrors the design in `docs/plan.md`: one or two listeners, and a
//! set of named backends that a route selects between. Route-level selection is
//! intentionally absent until the dispatcher lands; today the config carries a
//! `default_backend` and the backend tables.

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
}

/// Listeners. Both are optional, but at least one must be configured.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// HTTP listener, e.g. `127.0.0.1:8080`.
    #[serde(default)]
    pub http: Option<String>,

    /// FastCGI listener, e.g. `127.0.0.1:9000` or `unix:/run/stokehold.sock`.
    #[serde(default)]
    pub fastcgi: Option<String>,
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

    /// Recycle a slot after this many requests, the equivalent of php-fpm's
    /// `pm.max_requests`.
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
}
