//! The `volucer` command line interface.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::{Parser, Subcommand};

use volucer_core::config::{Config, MinTlsVersion};
use volucer_core::php;
use volucer_core::sites::Sites;
use volucer_fcgi::cgi::Routing;
use volucer_http::{HttpLimits, RunningHttp};
use volucer_server::{Contention, Dispatcher, FastCgiListener, Limits, ServerError};

#[derive(Parser, Debug)]
#[command(
    name = "volucer",
    version,
    about = "Cross-platform PHP application server: persistent workers and FastCGI",
    long_about = None
)]
struct Cli {
    /// Increase log verbosity (`-v`, `-vv`).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the configured listeners and backends.
    Serve {
        /// Path to `volucer.toml`.
        #[arg(short, long, default_value = "volucer.toml")]
        config: PathBuf,

        /// Document root that PHP scripts are resolved against.
        ///
        /// Overrides `server.document_root`; without either, `.`.
        #[arg(long)]
        document_root: Option<PathBuf>,

        /// Front controller, relative to the document root.
        ///
        /// Overrides `server.front_controller`; without either, `index.php`.
        /// Set it empty to serve only actual files, matching nginx's
        /// `location ~ \.php$`.
        #[arg(long)]
        front_controller: Option<String>,

        /// Maximum concurrent client connections.
        #[arg(long, default_value_t = 512)]
        max_connections: usize,
    },

    /// Validate the configuration and report what was found on this host.
    Check {
        /// Path to `volucer.toml`.
        #[arg(short, long, default_value = "volucer.toml")]
        config: PathBuf,

        /// Document root to check, overriding `server.document_root`.
        #[arg(long)]
        document_root: Option<PathBuf>,

        /// Extra directories to search for PHP binaries.
        #[arg(long = "php-dir")]
        php_dirs: Vec<PathBuf>,
    },

    /// Print the version, the PHP floor, and the protocols this build speaks.
    Version,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let result = match cli.command {
        Command::Serve {
            config,
            document_root,
            front_controller,
            max_connections,
        } => serve(
            &config,
            document_root.as_deref(),
            front_controller.as_deref(),
            max_connections,
        ),
        Command::Check {
            config,
            document_root,
            php_dirs,
        } => check(&config, document_root.as_deref(), &php_dirs).map_err(ServerError::from),
        Command::Version => {
            version();
            Ok(())
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("volucer: {err}");
            let mut source = std::error::Error::source(&err);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}

/// `version` prints what is running, and what that build can do.
///
/// The PHP floor is here because it is a build fact an operator has to be able
/// to check from a shell: a worker below it starts and then cannot be asked for
/// a per-request peak. See the README in `sailantis/volucer-php-sdk`.
fn version() {
    println!("volucer {}", env!("CARGO_PKG_VERSION"));
    println!("  front-ends:       HTTP/1.1, HTTP/2 (cleartext), FastCGI, TLS (rustls)");
    println!(
        "  worker protocol:  {}",
        volucer_server::WORKER_PROTOCOL_VERSION
    );
    println!(
        "  php floor:        {}",
        volucer_server::MINIMUM_PHP_VERSION
    );
    println!("  backends:         worker, external fpm, spawn_cgi");
    // ACME is advertised only as *available*, because whether it is in use is a property of the
    // configuration rather than of the build — `volucer check` is where a deployment's own answer
    // is printed. Saying "TLS-ALPN-01" rather than "ACME" matters: it is the one challenge this
    // build can perform, and an operator comparing against another server needs to know that.
    println!("  certificates:     ACME (RFC 8555), the tls-alpn-01 challenge");
}

fn init_tracing(verbosity: u8) {
    use tracing_subscriber::EnvFilter;

    let fallback = match verbosity {
        0 => "volucer=info",
        1 => "volucer=debug",
        _ => "volucer=trace",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(fallback));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

fn load_and_validate(path: &Path) -> Result<Config, ServerError> {
    let config = Config::load(path)?;
    config.validate()?;
    tracing::info!(path = %path.display(), "configuration loaded");
    Ok(config)
}

fn routing_for(document_root: &Path, front_controller: Option<&str>) -> Routing {
    Routing {
        document_root: document_root.to_path_buf(),
        front_controller: front_controller.map(str::to_string),
        ..Routing::default()
    }
}

/// The document root in effect: the flag, else the file, else the working
/// directory.
///
/// The order matters and is stated once, here, so `serve` and `check` cannot
/// disagree about which root will be used — `check` reports on the deployment
/// `serve` would actually start.
fn resolve_document_root(flag: Option<&Path>, config: &Config) -> PathBuf {
    flag.map(Path::to_path_buf)
        .or_else(|| config.server.document_root.clone())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Absolutise the document root, because a relative one silently breaks PHP.
///
/// `SCRIPT_FILENAME` is built by joining this root, and `php-cgi` runs in a
/// persistent pool: it `chdir()`s into the directory of the script it executed.
/// A relative root therefore resolves against the *previous* request's
/// directory from the second request onwards, and the answer is `No input file
/// specified` — verified against php-cgi 8.3.31, where the same path succeeds
/// once and fails every time after. An absolute root has no such dependency.
///
/// Best-effort: if the working directory cannot be read there is nothing better
/// to offer than the path as written, and the preflight warning for a root that
/// does not exist already covers the case where something is wrong.
fn absolute_document_root(root: &Path) -> PathBuf {
    if root.is_absolute() {
        return root.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(root))
        .unwrap_or_else(|_| root.to_path_buf())
}

/// Resolve a TLS path against the directory holding the config file.
///
/// Deliberately **not** the document-root rule, which is relative to the process's working
/// directory. A certificate beside `volucer.toml` is the common case, and resolving it
/// against wherever the process happened to start is a footgun with a security consequence:
/// the server would start with whatever certificate it found, or fail to start at all, for a
/// reason invisible in the file. An absolute path is used as written.
fn resolve_tls_path(path: &Path, config_path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match config_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(path),
        // A bare filename such as `volucer.toml` has an empty parent, which means the
        // working directory — the same answer `Path::parent` gives and the one a reader
        // expects.
        _ => path.to_path_buf(),
    }
}

/// Absolute certificate and key for the server's own TLS settings.
fn resolved_server_tls(config: &Config, config_path: &Path) -> Option<TlsPaths> {
    let tls = config.server.tls.as_ref()?;
    Some(TlsPaths {
        certificate: resolve_tls_path(&tls.certificate, config_path),
        key: resolve_tls_path(&tls.key, config_path),
    })
}

/// Resolved certificate and key paths.
struct TlsPaths {
    certificate: PathBuf,
    key: PathBuf,
}

/// Every `(site name, paths)` that states its own certificate, resolved.
fn resolved_site_tls(config: &Config, config_path: &Path) -> Vec<(String, TlsPaths)> {
    config
        .sites
        .iter()
        .filter_map(|(name, site)| {
            let tls = site.tls.as_ref()?;
            Some((
                name.clone(),
                TlsPaths {
                    certificate: resolve_tls_path(&tls.certificate, config_path),
                    key: resolve_tls_path(&tls.key, config_path),
                },
            ))
        })
        .collect()
}

/// How long a certificate is valid for, and whether that is a problem.
///
/// The direct mitigation for ACME's failure mode, which is **silence**: issuance that never
/// succeeds is indistinguishable from issuance that is working, so the only symptom a deployment
/// gets is a certificate that expires. Reading the days left and saying so is what turns weeks of
/// quiet failure into one line an operator sees.
///
/// A day count rather than a timestamp because that is the unit the decision is made in: 41 days
/// means "renewals are working", 3 means "something has been failing for a while".
fn certificate_lifetime(path: &Path) -> Option<CertificateLifetime> {
    let pem = std::fs::read(path).ok()?;
    let (_, pem) = x509_parser::pem::parse_x509_pem(&pem).ok()?;
    let certificate = pem.parse_x509().ok()?;
    let validity = certificate.validity();
    let expiry = validity.not_after.timestamp();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Some(CertificateLifetime {
        days_left: (expiry - now).div_euclid(86_400),
        expired: expiry <= now,
    })
}

/// What `check` reports about one certificate's lifetime.
struct CertificateLifetime {
    /// Whole days until `not_after`. Negative when it has already passed.
    days_left: i64,
    /// Whether `not_after` is in the past.
    expired: bool,
}

impl CertificateLifetime {
    /// The report, with the warning it deserves.
    ///
    /// The thresholds are chosen against how automation actually fails rather than against a
    /// round number. A renewal is attempted at a third of the way through validity, so anything
    /// under a third means *at least one* attempt has not taken effect yet — worth knowing, not yet
    /// an emergency. Under a week means several have failed in a row.
    fn report(&self, label: &str) -> String {
        if self.expired {
            return format!(
                "{label}: EXPIRED — every client is being shown a certificate warning, and \
                 no ACME renewal has succeeded"
            );
        }
        match self.days_left {
            0..=7 => format!(
                "{label}: {} day(s) left — several renewals in a row have failed; the CA will \
                 also be warning the account contact",
                self.days_left
            ),
            8..=30 => format!(
                "{label}: {} day(s) left — an ACME renewal should have happened by now",
                self.days_left
            ),
            _ => format!("{label}: {} day(s) left", self.days_left),
        }
    }
}

/// Install an ACME-issued certificate into the process's serving table.
///
/// The engine's sink, and the only bridge from issuance to what a handshake resolves against.
///
/// It reaches the **process's one resolver**, not a listener's, for the reason `reload-shared`
/// already established one layer down: an `:80`/`:443` deployment has two TLS-speaking listeners
/// sharing one table, so a certificate installed per listener would appear on one port and not the
/// other — the same one-protocol-only failure, one layer up.
///
/// The resolver arrives through a cell rather than as a field because the two are mutually
/// dependent and only one order can be written: the resolver is composed with the engine's
/// challenge source, so the engine has to exist first, and the engine needs somewhere to install
/// what it issues. A cell breaks the cycle without a lock on any path that matters — nothing writes
/// it after startup, and the only read is on an issuance, which happens seconds or weeks later.
struct ResolverSink {
    resolver: Arc<std::sync::OnceLock<Arc<volucer_http::SniResolver>>>,
}

impl volucer_acme::CertificateSink for ResolverSink {
    fn install(&self, pem: &[u8], domains: &[String]) -> Result<(), volucer_acme::AcmeError> {
        let Some(resolver) = self.resolver.get() else {
            // Only reachable if an issuance completed between the engine starting and the
            // resolver being installed, which `serve` does not allow — but a missing resolver is a
            // certificate installed nowhere, and that is the failure this whole path exists to
            // prevent, so it is an error rather than a silent success.
            tracing::error!(
                "an issued certificate arrived before the serving resolver was installed"
            );
            return Err(volucer_acme::AcmeError::State);
        };

        // `rustls-acme` hands over the certificate and key as *one* PEM blob — the shape its cache
        // stores — so the split is by PEM label rather than by position: a chain may have any
        // number of certificates followed by exactly one key, and "first is the cert, second is
        // the key" would break the moment a chain had an intermediate.
        let (cert_pem, key_pem) = split_pem_pair(pem);
        match resolver.install_issued(cert_pem, key_pem, domains) {
            Ok(replaced) => {
                tracing::info!(
                    domains = ?domains,
                    named_before = replaced.named(),
                    "the issued certificate is now being served"
                );
                Ok(())
            }
            Err(err) => {
                tracing::error!(%err, "an issued certificate could not be installed");
                Err(volucer_acme::AcmeError::State)
            }
        }
    }
}

impl std::fmt::Debug for ResolverSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolverSink")
            .field("installed", &self.resolver.get().is_some())
            .finish_non_exhaustive()
    }
}

/// Split one PEM blob into its certificate chain and its private key.
///
/// By PEM *label* rather than by position, because that is what the two actually are and a chain is
/// not a fixed length: the blob is the chain (leaf, then any intermediates) followed by the key, so
/// a reader that took "the second block" would hand an intermediate over as the key.
///
/// A blob with no key yields an empty key half rather than a guess: it cannot be installed either
/// way, and the loader names the reason (`KeyError::Empty`) instead of a parser failing on an
/// intermediate.
fn split_pem_pair(pem: &[u8]) -> (&[u8], &[u8]) {
    const KEY_MARKERS: [&str; 4] = [
        "-----BEGIN PRIVATE KEY-----",
        "-----BEGIN RSA PRIVATE KEY-----",
        "-----BEGIN EC PRIVATE KEY-----",
        "-----BEGIN ENCRYPTED PRIVATE KEY-----",
    ];

    match KEY_MARKERS
        .iter()
        .filter_map(|marker| find_bytes(pem, marker.as_bytes()))
        .min()
    {
        Some(at) => (&pem[..at], &pem[at..]),
        // No key marker: the whole blob is the chain, and the key half is empty so the loader
        // reports a missing key rather than the chain being silently truncated.
        None => (pem, &[]),
    }
}

/// The offset of `needle` in `haystack`, or `None`.
///
/// `windows` rather than a dependency: this runs once per issuance, on a few kilobytes.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Run the configured listeners until interrupted.
///
/// Both front-ends share one dispatcher, so a request gets the same backend
/// whichever protocol carried it. The FastCGI listener runs on this thread; the
/// HTTP listener needs a `tokio` runtime, so it runs on one of its own. That is
/// the async boundary made concrete: the runtime exists for the HTTP front-end,
/// and nothing else in the program depends on it.
fn serve(
    path: &Path,
    document_root_flag: Option<&Path>,
    front_controller_flag: Option<&str>,
    max_connections: usize,
) -> Result<(), ServerError> {
    let config = load_and_validate(path)?;
    let document_root = absolute_document_root(&resolve_document_root(document_root_flag, &config));
    let front_controller = config
        .server
        .resolve_front_controller(front_controller_flag);

    // The command line describes the *default* site. A site states its own root, so a
    // flag that named one would silently mean "the default site only" — the kind of
    // setting this file already refuses to give two meanings (see the empty-string rule
    // on `front_controller`).
    if !config.sites.is_empty() {
        if document_root_flag.is_some() {
            return Err(ServerError::Misconfigured {
                backend: "<cli>".to_string(),
                reason: "--document-root cannot be combined with [sites]: each site states its \
                         own document_root"
                    .to_string(),
            });
        }
        if front_controller_flag.is_some() {
            return Err(ServerError::Misconfigured {
                backend: "<cli>".to_string(),
                reason: "--front-controller cannot be combined with [sites]: each site states its \
                         own front_controller"
                    .to_string(),
            });
        }
    }

    // One routing, derived once, from the same table the sites are. The command line's
    // controller is folded in here rather than afterwards so the default site is
    // described in one place.
    let sites = Arc::new(Sites::from_config_with(
        &config,
        &document_root,
        front_controller.as_deref(),
    ));
    let http_address = config.server.http.clone();
    let fastcgi_address = config.server.fastcgi.clone();
    if http_address.is_none() && fastcgi_address.is_none() {
        return Err(ServerError::NoListener);
    }

    // Backends start before a listener binds: a misconfigured backend should be
    // reported before a port is occupied. One dispatcher serves both front-ends,
    // shared without a lock because each backend slot synchronises internally —
    // so two requests to two workers run at the same time.
    let dispatcher = Dispatcher::from_config(
        &config,
        routing_for(&document_root, front_controller.as_deref()),
    )?;
    for name in dispatcher.backend_names() {
        tracing::info!(backend = %name, "backend ready");
    }
    // A static-only deployment answers a path that names no file with a 500 and
    // nothing else in the log, so the reason is stated once here rather than left
    // to be inferred from a request that failed for no visible cause.
    if !dispatcher.has_backends() {
        tracing::warn!(
            "no backends are configured: only paths naming a real file will be answered, and \
             every other path will be a 500"
        );
    }
    let dispatcher = Arc::new(dispatcher);

    println!("volucer");
    println!("  document root:    {}", document_root.display());
    println!(
        "  front controller: {}",
        front_controller
            .as_deref()
            .unwrap_or("(none: only real .php files are served)")
    );
    println!("  backends:         {}", config.backends.len());
    if !config.routes.is_empty() {
        println!("  routes:           {}", config.routes.len());
    }
    if !config.sites.is_empty() {
        println!("  sites:            {}", config.sites.len());
        for (name, site) in &config.sites {
            println!("    {name}: {}", site.hosts.join(", "));
        }
    }
    if config.server.statics().enabled {
        println!("  static files:     serving from the document root");
    }

    // Reported before the listeners start, not after: a document root that is
    // not there is the mistake that produces a 404 for every static file and a
    // front controller PHP will not run, and noticing it in the log is worth
    // more than the process refusing to start. Only in the log, so a deployment
    // that creates the root during startup still works.
    for warning in volucer_core::preflight::inspect_with(&config, &document_root, Some(path)) {
        tracing::warn!("{}", warning.message);
        if let Some(hint) = &warning.hint {
            tracing::warn!("  {hint}");
        }
    }

    // A report on shutdown needs numbers that only exist at shutdown, so the
    // dispatcher stays reachable instead of being dropped with `Arc`'s last
    // strong count below.
    let reporting = Arc::clone(&dispatcher);

    // One handle, cloned into every listener, so a reload moves all of them or none. It must be
    // built here rather than per listener: a listener that built its own would keep serving the
    // old configuration after a reload, which takes effect on one protocol and reads as the
    // other front-end being right.
    //
    // The registry is what a *trigger* holds: it knows the one shared site table and every
    // front-end's snapshot-holder, so `apply` cannot half-succeed. Building it here rather than
    // inside a listener is what makes the process's own `SIGHUP` path able to reach a FastCGI
    // listener at all.
    let registry = Arc::new(volucer_http::Registry::new(volucer_http::SharedState::new(
        (*sites).clone(),
    )));

    // The FastCGI listener binds first, because binding is synchronous here while
    // the HTTP listener needs a runtime — and an occupied port should be reported
    // before the runtime is built.
    let fastcgi = match &fastcgi_address {
        Some(address) => {
            let listener = FastCgiListener::bind_registered(
                address,
                Arc::clone(&dispatcher),
                Arc::clone(&registry),
                Limits { max_connections },
            )?;
            println!("  fastcgi:          {}", listener.local_addr()?);
            Some(listener)
        }
        None => {
            println!("  fastcgi:          (disabled)");
            None
        }
    };

    // TLS applies to `http` unless a dedicated `https` listener was configured, in which case
    // `http` stays plaintext and `https` carries it. Validation has already refused every
    // combination outside that matrix, so this only picks between the two shapes.
    let config_dir = path.parent().unwrap_or_else(|| Path::new(""));
    let tls_active = config.server.tls.is_some();
    let https_address = config.server.https.clone();
    let tls_on_http = tls_active && https_address.is_none();
    let wants_redirect = config.server.redirect_to_https;

    // The redirect lives on whichever listener serves cleartext. With a split :80/:443 that is
    // `http` (which has no TLS of its own), and with TLS sharing the port it is the cleartext
    // half of that one listener. Validation has already ensured some plaintext listener exists.
    //
    // Only a TCP listener has a port. A unix socket cannot be reached over a redirect at all,
    // so it yields `None` and the redirect falls back to the listener's own port.
    let https_port = match &https_address {
        Some(address) => match volucer_core::config::parse_listener(address) {
            Ok(volucer_core::config::Listener::Tcp(addr)) => Some(addr.port()),
            _ => None,
        },
        None => None,
    };

    // `tls` and the redirect are separate settings on purpose: the plaintext listener in a
    // split deployment carries the redirect with *no* acceptor, and giving it one would turn it
    // into a TLS port that no cleartext client could reach.
    let http_settings = volucer_http::ListenerSettings {
        tls: if tls_on_http {
            Some(volucer_http::TlsSettings {
                config: &config,
                config_dir,
            })
        } else {
            None
        },
        redirect_to_https: wants_redirect && config.server.has_plaintext_listener(),
        https_port,
    };

    // The certificate table is built **once for the process**, before any port is bound, and
    // handed to every TLS-speaking listener.
    //
    // One resolver rather than one per listener, because a renewal has to reach all of them: an
    // `:80`/`:443` deployment has two TLS-speaking listeners, so two resolvers would mean a
    // renewed certificate appeared on one port and not the other — the same one-protocol-only
    // failure the shared site table exists to prevent, one layer down. A plaintext deployment
    // builds none at all, which is the whole of "TLS costs nothing when it is off".
    //
    // Loading it here rather than inside the listener is also the ordering hazard 56 was about:
    // a certificate that cannot be read must fail *before* a port is taken, not after `serve` has
    // printed that the port came up.
    let tls_state = match tls_active {
        true => Some(Arc::new(
            volucer_http::SniResolver::from_config(&config, config_dir).map_err(|err| {
                ServerError::Listener {
                    reason: err.to_string(),
                }
            })?,
        )),
        false => None,
    };

    // The ACME engine, when configured. Built *before* the listeners bind, because a challenge
    // handshake can arrive as soon as a port is open, so the challenge source has to be composed
    // into the resolver already.
    //
    // The resolver arrives through a cell: the resolver is composed with the engine's challenge
    // source, so the engine must exist first, and the engine needs somewhere to install what it
    // issues. See [`ResolverSink`].
    let (tls_state, acme) = match (&config.server.acme, tls_state) {
        (Some(acme), Some(resolver)) => {
            let cache = match acme.cache.is_absolute() {
                true => acme.cache.clone(),
                false => config_dir.join(&acme.cache),
            };
            let cell = Arc::new(std::sync::OnceLock::new());
            let engine = volucer_acme::Engine::new(
                acme.domains.clone(),
                match acme.authority {
                    volucer_core::config::Authority::Staging => volucer_acme::Authority::Staging,
                    volucer_core::config::Authority::Production => {
                        volucer_acme::Authority::Production
                    }
                },
                cache,
                Arc::new(ResolverSink {
                    resolver: Arc::clone(&cell),
                }),
            )
            .map_err(|err| ServerError::Listener {
                reason: err.to_string(),
            })?;

            // The challenge source is composed in **before** any port is bound, and only for a
            // challenge handshake — `SniResolver::resolve` consults it behind an exact `acme-tls/1`
            // ALPN match, so a normal handshake is answered by the site table exactly as it was.
            //
            // The resolver object is rebuilt rather than mutated, because the ACME field is set at
            // construction and the table behind it is what moves. The *table* is shared into the
            // new resolver, so an installation through the cell and a reload through the old handle
            // both reach the same place.
            let composed = Arc::new(
                volucer_http::SniResolver::from_shared(resolver.table(), resolver.config_dir())
                    .with_acme(Arc::new(engine.challenge_source())),
            );
            if cell.set(Arc::clone(&composed)).is_err() {
                return Err(ServerError::Listener {
                    reason: "the ACME sink could not be wired to the serving resolver".to_string(),
                });
            }

            println!("  acme:             {}", acme.domains.join(", "));
            println!("    authority:      {}", acme.authority.as_str());
            if acme.contact.is_empty() {
                tracing::warn!(
                    "no server.acme.contact is set, so the CA has no way to warn this deployment \
                     that renewals are failing; `volucer check` reports the days left instead"
                );
            }
            (Some(composed), Some(engine))
        }
        (Some(_), None) => {
            // Validation refuses this, so reaching it means the two rules have come apart.
            return Err(ServerError::Listener {
                reason: "[server.acme] needs [server.tls]: ACME replaces the certificate being \
                         served, so name a fallback pair"
                    .to_string(),
            });
        }
        (None, other) => (other, None),
    };

    // The cached certificate is installed before anything is bound, so a restart serves it from the
    // first handshake rather than from whenever the state's own first poll completes. Blocking here
    // is deliberate: it is one file read, and a listener that accepted a handshake before it
    // finished would be presenting the fallback certificate to a client that should have had the
    // issued one.
    // The cached certificate is installed before anything is bound, so a restart serves it from the
    // first handshake rather than from whenever the state's own first poll completes. Blocking here
    // is deliberate: it is one file read, and a listener that accepted a handshake before it
    // finished would be presenting the fallback certificate to a client that should have had the
    // issued one.
    //
    // `run` installs it too, and the two are not redundant: a renewal has to reach the table months
    // after startup, and that is the event path. This one is what makes the *window before the first
    // poll* serve the issued certificate — the engine's own `load_cert` step is asynchronous, and
    // `serve` does not wait for it.
    if let Some(engine) = &acme {
        let installed = futures::executor::block_on(engine.install_cached());
        if installed {
            tracing::info!("the cached certificate is serving before any port was bound");
        }
    }

    // The handles one listener needs: the shared table, the one registry, and TLS when it speaks
    // it. Built per listener because the resolver is `Some` for a TLS listener and `None` for a
    // plaintext one, which is what keeps a plaintext listener from carrying TLS state at all.
    let handles_for = |speaks_tls: bool| {
        let handles = volucer_http::ReloadHandles::new(Arc::clone(&registry));
        match (speaks_tls, &tls_state) {
            (true, Some(resolver)) => handles.with_tls(Arc::clone(resolver)),
            _ => handles,
        }
    };

    let http = match &http_address {
        Some(address) => {
            let running = RunningHttp::start_with_tls(
                address,
                Arc::clone(&dispatcher),
                handles_for(tls_on_http),
                HttpLimits {
                    max_connections,
                    ..HttpLimits::default()
                },
                http_settings,
            )
            .map_err(|err| ServerError::Listener {
                reason: err.to_string(),
            })?;
            if tls_on_http {
                println!("  http:             {} (https)", running.local_addr());
            } else {
                println!("  http:             {}", running.local_addr());
            }
            Some(running)
        }
        None => {
            println!("  http:             (disabled)");
            None
        }
    };

    let https = match &https_address {
        Some(address) => {
            let running = RunningHttp::start_with_tls(
                address,
                Arc::clone(&dispatcher),
                handles_for(true),
                HttpLimits {
                    max_connections,
                    ..HttpLimits::default()
                },
                // TLS only: this listener is already the https endpoint, so a redirect here
                // would send a client to where it came from.
                volucer_http::ListenerSettings {
                    tls: Some(volucer_http::TlsSettings {
                        config: &config,
                        config_dir,
                    }),
                    redirect_to_https: false,
                    https_port,
                },
            )
            .map_err(|err| ServerError::Listener {
                reason: err.to_string(),
            })?;
            println!("  https:            {} (https)", running.local_addr());
            Some(running)
        }
        None => {
            if tls_active {
                println!("  https:            (disabled)");
            }
            None
        }
    };

    println!("press Ctrl-C to stop");

    // The engine is moved **into** the thread, which is what keeps `Arc<Engine>` — and therefore
    // `AcmeState`'s lack of `Sync` — out of the signal handler entirely. The reloader needs no
    // handle to it: what it asks is the *resolver*, which is `Sync` because it has to be.
    if let Some(engine) = acme {
        std::thread::Builder::new()
            .name("acme".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(err) => {
                        tracing::error!(
                            %err,
                            "no runtime for ACME issuance; certificates will not be renewed"
                        );
                        return;
                    }
                };
                runtime.block_on(engine.run());
            })
            .map_err(|err| ServerError::Listener {
                reason: format!("the ACME task could not be started: {err}"),
            })?;
    }

    install_shutdown_handler(
        fastcgi.as_ref().map(|l| l.shutdown_handle()),
        Arc::clone(&registry),
        tls_state.clone(),
        path.to_path_buf(),
        document_root_flag.map(Path::to_path_buf),
        front_controller_flag.map(str::to_string),
    );

    // The FastCGI listener owns this thread while the HTTP listener runs on its
    // own, so both are served concurrently. With no FastCGI listener there is
    // nothing to drive, so the process waits on the shared shutdown flag instead.
    match fastcgi {
        Some(listener) => listener.run()?,
        None => wait_for_shutdown(),
    }

    // Stopping the HTTP listener joins its thread, which drops the dispatcher on
    // the other side when both shared handles are gone — and with it the worker
    // pool, which reaps every PHP process.
    if let Some(http) = http {
        let stats = Arc::clone(&http.stats());
        http.stop();
        let (_, requests, _, failed) = stats.snapshot();
        println!("http: {requests} request(s), {failed} failed");
        report_tls(&stats);
    }
    if let Some(https) = https {
        let stats = Arc::clone(&https.stats());
        https.stop();
        let (_, requests, _, failed) = stats.snapshot();
        println!("https: {requests} request(s), {failed} failed");
        report_tls(&stats);
    }

    // Printed rather than traced: a load test wants this line in the same log as
    // the run it describes, and a filter that could silence it is the last thing a
    // diagnostic should depend on.
    for contention in reporting.contention() {
        println!("{}", contention_line(&contention));
    }

    println!("stopped");
    Ok(())
}

/// Print the TLS counters, when the listener had TLS at all.
///
/// Printed rather than traced for the same reason the request counts are: a failed handshake is
/// invisible from the client's side — the client sees a handshake error and never reaches this
/// log — so the number is the only signal that a port is refusing everyone.
fn report_tls(stats: &volucer_http::HttpStats) {
    let (handshakes, failures, plaintext) = stats.tls_snapshot();
    if handshakes == 0 && failures == 0 && plaintext == 0 {
        return;
    }
    println!("tls: {handshakes} handshake(s), {failures} failed");
    if plaintext > 0 {
        println!("tls: {plaintext} cleartext connection(s) accepted on the TLS port");
    }
}

/// The one line of slot-contention report, as a string.
///
/// Its own function because it is a format, not a side effect: the numbers are what a
/// measurement is read from, so the text they land in is worth a test that does not need
/// a server, a signal or a terminal.
fn contention_line(contention: &Contention) -> String {
    format!(
        "slots: {} worker(s), {} request(s), wait mean {} us, wait p50 {} us, wait p99 {} us, \
         busiest slot {} at {} us",
        contention.workers,
        contention.served,
        contention.mean_us,
        contention.p50_us,
        contention.p99_us,
        contention.busiest,
        contention.busiest_us,
    )
}

/// Wait until the shutdown flag is set.
///
/// Used when only the HTTP listener is configured: its thread owns the listener,
/// so this thread has nothing to drive and blocks on the same flag the Ctrl-C
/// handler sets.
fn wait_for_shutdown() {
    while !SHUTDOWN.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// Set once a Ctrl-C has been seen, so a listener that cannot be told directly
/// can still be waited on.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Everything a reload needs to re-read the configuration and apply it.
///
/// One value rather than a handful of arguments, because that is what the handler closure has to
/// capture and a caller that supplied half of them would be a reload that reported success and
/// changed nothing.
///
/// **Unused on Windows**, where there is no `SIGHUP` and therefore nothing that could reload. It is
/// built and passed there anyway rather than `#[cfg]`-ed away, so the two platforms' handler
/// signatures stay the same and the difference is stated in one place — the `signals` module —
/// instead of in every call site.
#[cfg_attr(windows, allow(dead_code))]
struct Reloader {
    /// The path named on the command line. Re-read as-is, which is open decision 9.1's answer:
    /// what an operator expects from `SIGHUP`, and no new concept such as a marker file.
    config_path: PathBuf,
    /// The document root and front controller **the command line resolved**.
    ///
    /// Replayed on a reload because they are part of what the configuration *means*: a deployment
    /// started with `--document-root` on a site-less config takes its root from the flag, and a
    /// reload that dropped it would move the root to `.` — a working server that starts serving
    /// 404s, with the change attributed to whatever the operator edited.
    document_root_flag: Option<PathBuf>,
    front_controller_flag: Option<String>,
    /// The one registry: the shared table and every front-end.
    registry: Arc<volucer_http::Registry>,
    /// The process's certificate table, when TLS is on, so a renewed PEM is picked up too.
    ///
    /// Also the reload's answer to "does ACME manage these certificates?": the composed resolver is
    /// the thing that actually answers handshakes, so asking *it* rather than a second flag is what
    /// keeps the reload's decision from drifting away from the serving path. The engine itself
    /// cannot be held here — `AcmeState` is not `Sync`, so it cannot move into the signal thread.
    tls: Option<Arc<volucer_http::SniResolver>>,
}

impl Reloader {
    /// Re-read the configuration file and apply it, whole or not at all.
    ///
    /// The two failure modes are reported and *nothing is changed*: a file that does not parse, and
    /// a file that parses and does not validate. That is the only safe direction — a reload that
    /// installed half of a file would leave the operator unable to tell which half took effect —
    /// and it is why this returns early rather than applying what it could.
    ///
    /// A reload that succeeds still may not have changed anything the implementation can honour;
    /// the settings that are **restart-only** are named in [`restart_only`] and printed, so an
    /// operator who edited one learns it here rather than from behaviour.
    #[cfg_attr(windows, allow(dead_code))]
    fn reload(&self) {
        let config = match Config::load(&self.config_path) {
            Ok(config) => config,
            Err(err) => {
                tracing::error!(%err, "reload refused: the configuration could not be read");
                return;
            }
        };
        if let Err(err) = config.validate() {
            tracing::error!(%err, "reload refused: the configuration is not valid");
            return;
        }

        // The site table, with the command line replayed, exactly as `serve` built the first one.
        // Built before anything is installed, so a table that cannot be built is a refusal rather
        // than a half-applied reload.
        let document_root = match &self.document_root_flag {
            Some(root) => absolute_document_root(root),
            None => absolute_document_root(&resolve_document_root(None, &config)),
        };
        let front_controller = config
            .server
            .resolve_front_controller(self.front_controller_flag.as_deref());
        let sites = Sites::from_config_with(&config, &document_root, front_controller.as_deref());

        let generation = self.registry.generation() + 1;
        let _previous = self.registry.apply(sites);
        tracing::info!(
            generation,
            file = %self.config_path.display(),
            document_root = %document_root.display(),
            sites = config.sites.len(),
            "configuration reloaded"
        );

        // Certificates are a second, independent table. Refreshed after the sites so that a
        // certificate that cannot be read leaves the *sites* reloaded and the certificates as they
        // were — one table refusing must not take the other with it, and both are whole-or-nothing
        // on their own.
        //
        // When ACME manages this deployment's certificates the rebuild is **skipped, not followed
        // by a repair**, because the issued certificate is not in the configuration at all: the
        // configured pair is only the fallback, so rebuilding from the file puts a self-signed
        // certificate back into service on a host that has a real one. Reading the cache back
        // afterwards would only fix that while the read succeeds, so the moment it does not is the
        // moment a deployment is downgraded with no error. The table is already correct, because
        // issuance installs into it, so leaving it alone makes the two halves unable to come apart
        // rather than repaired after they had.
        //
        // `has_acme` is asked of the resolver that answers handshakes rather than inferred from a
        // `[server.acme]` table existing: the question is whether *the serving path* consults an ACME
        // challenge source, which is a fact about this object and not about the file on disk.
        if self
            .tls
            .as_ref()
            .is_some_and(|resolver| resolver.has_acme())
        {
            tracing::info!(
                "ACME manages this deployment's certificates: the sites changed and the issued \
                 certificate stays in service"
            );
        } else if let Some(resolver) = &self.tls {
            match resolver.refresh(&volucer_http::CertificateSet::from_config(&config)) {
                Ok(_) => tracing::info!("certificate table refreshed"),
                Err(err) => {
                    tracing::error!(%err, "the certificate table was not replaced; the previous one is still serving")
                }
            }
        }

        for line in restart_only() {
            tracing::info!("{line}");
        }
    }
}

/// The settings a reload cannot honour, and the reason.
///
/// Named rather than discovered, which is the lesson of the TLS work: a setting the implementation
/// cannot apply has to be *stated*, or the operator finds out from behaviour instead — and a
/// reload that appears to have worked is worse than one that refused.
///
/// Returned as lines so the same text is what `serve` logs and what a test can assert on, rather
/// than a message assembled at the call site.
pub fn restart_only() -> Vec<&'static str> {
    vec![
        "a reload does not change: listener addresses (the sockets are already bound)",
        "a reload does not change: backends, their commands and worker counts (the processes are running)",
        "a reload does not change: [server.tls] min_version and alpn (the rustls ServerConfig is built once)",
    ]
}

/// Stop the listeners on the first Ctrl-C or SIGTERM, and **reload** on SIGHUP.
///
/// A second signal forces an exit, so a stuck shutdown cannot trap the operator.
///
/// **SIGTERM is what a supervisor actually sends**, and until this handled it the
/// process died on the spot with no chance to run a destructor — so `systemctl stop`,
/// `docker stop` and a plain `kill` all left the php-cgi children of a `spawn_cgi`
/// pool running. They accumulated: a benchmark host reached 68 orphans, and each one
/// held its port, its PHP state and its share of the CPU.
///
/// **SIGHUP reloads**, which is the conventional meaning on a Unix server and the reason the
/// handler has to distinguish the signals at all. That is what the platform split below is about,
/// and it is not merely a `#[cfg]` for tidiness: `ctrlc` registers `SIGHUP` under its `termination`
/// feature and runs the *shutdown* callback for it, so before this change `kill -HUP volucer`
/// stopped the server. Reproduced against the previous binary, not inferred: the process was gone
/// and the log read "shutting down".
///
/// Forcing is safe now, and was not before: the lifecycle guard reaps the PHP
/// processes when the process dies, *provided* the death is one it can observe.
///
/// The FastCGI listener takes a [`ShutdownHandle`], which is a shared flag. The
/// HTTP listener runs on another thread and is stopped by *dropping* its handle,
/// which is what `serve` does after this — so the only thing this handler has to
/// do for it is set the static flag that `wait_for_shutdown` polls.
fn install_shutdown_handler(
    shutdown: Option<volucer_server::ShutdownHandle>,
    registry: Arc<volucer_http::Registry>,
    tls: Option<Arc<volucer_http::SniResolver>>,
    config_path: PathBuf,
    document_root_flag: Option<PathBuf>,
    front_controller_flag: Option<String>,
) {
    let forced = Arc::new(AtomicBool::new(false));
    let stop = {
        let forced = Arc::clone(&forced);
        move || {
            if forced.swap(true, Ordering::SeqCst) {
                eprintln!("\nforced exit");
                std::process::exit(130);
            }
            eprintln!("\nshutting down; signal again to force");
            SHUTDOWN.store(true, Ordering::Relaxed);
            if let Some(shutdown) = &shutdown {
                shutdown.shutdown();
            }
        }
    };

    let reloader = Reloader {
        config_path,
        document_root_flag,
        front_controller_flag,
        registry,
        tls,
    };

    if let Err(err) = signals::install(stop, reloader) {
        // Not fatal, but it means a SIGTERM will leak the pools: say so at a level
        // an operator sees, because the symptom (surviving PHP processes) appears
        // long after the cause.
        tracing::warn!(%err, "no signal handler installed; stopping this process may leave its PHP children running, and SIGHUP cannot reload it");
    }
}

/// Signal handling, split by platform because the *mechanism* is — the behaviour is not.
///
/// The two implementations are deliberately one call apiece with the same meaning: stop on
/// SIGINT/SIGTERM, and reload on whatever this platform calls "reload". Windows has no `SIGHUP`
/// and no equivalent, so there the only signal is a stop, and reload is unavailable — which is
/// stated here rather than left for an operator on Windows to discover.
#[cfg(unix)]
mod signals {
    use super::Reloader;
    use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;
    use std::sync::Arc;

    /// Stop on SIGINT/SIGTERM, reload on SIGHUP.
    ///
    /// The signal number is what makes this possible, and it is the whole reason `signal-hook` is
    /// used here rather than the `ctrlc` this crate already depends on for Windows: `ctrlc` calls
    /// one handler for every signal it registered and cannot say which arrived, so a handler that
    /// had to tell a reload from a shutdown could not be written against it. Its `termination`
    /// feature also maps SIGHUP to the shutdown callback, which is what made `kill -HUP` stop the
    /// server.
    pub fn install(
        stop: impl Fn() + Send + 'static,
        reloader: Reloader,
    ) -> Result<(), std::io::Error> {
        let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP])?;
        let reloader = Arc::new(reloader);
        std::thread::Builder::new()
            .name("signals".into())
            .spawn(move || {
                for signal in signals.forever() {
                    match signal {
                        SIGHUP => {
                            tracing::info!("SIGHUP: reloading configuration");
                            reloader.reload();
                        }
                        SIGINT | SIGTERM => stop(),
                        // Registered signals only, so nothing else can arrive; named rather than
                        // silently ignored so that adding one to the set above without deciding
                        // what it means is visible here.
                        other => tracing::warn!(signal = other, "ignoring an unhandled signal"),
                    }
                }
            })
            .map(|_| ())
    }
}

#[cfg(windows)]
mod signals {
    use super::Reloader;
    use std::sync::Arc;

    /// Stop on Ctrl-C, Ctrl-Break and the console-close event.
    ///
    /// **There is no reload here, because Windows has no `SIGHUP`.** That is a property of the
    /// platform rather than a gap to work around, and it is why the reloader is dropped rather than
    /// stored: a handler that pretended to reload would be a signal nobody can send. A Windows
    /// deployment picks up a configuration change by restarting, exactly as it did before this
    /// phase.
    pub fn install(
        stop: impl Fn() + Send + 'static,
        _reloader: Reloader,
    ) -> Result<(), std::io::Error> {
        let _ = Arc::new(());
        ctrlc::set_handler(stop).map_err(std::io::Error::other)
    }
}

/// `check` validates configuration and reports the PHP binaries found here.
fn check(
    path: &Path,
    document_root_flag: Option<&Path>,
    extra_php_dirs: &[PathBuf],
) -> Result<(), volucer_core::Error> {
    let config = Config::load(path)?;
    config.validate()?;

    // The same resolution `serve` performs, so this reports the deployment that
    // would actually start rather than the one written in the file.
    let document_root = absolute_document_root(&resolve_document_root(document_root_flag, &config));
    let front_controller = config.server.resolve_front_controller(None);

    println!("configuration: {}", path.display());
    println!("  listeners:");
    match &config.server.http {
        Some(http) => println!("    http:    {http}"),
        None => println!("    http:    (disabled)"),
    }
    match &config.server.https {
        Some(https) => println!("    https:   {https}"),
        None => println!("    https:   (disabled)"),
    }
    match &config.server.fastcgi {
        Some(fcgi) => println!("    fastcgi: {fcgi}"),
        None => println!("    fastcgi: (disabled)"),
    }

    // TLS, and which paths were actually resolved. The resolution is printed rather than
    // assumed because a certificate path is the one setting whose *file* the reader cannot
    // see from the config: `certs/a.crt` is relative to this file, not to wherever the
    // process started.
    match resolved_server_tls(&config, path) {
        Some(server_tls) => {
            let tls = config
                .server
                .tls
                .as_ref()
                .expect("resolved implies present");
            println!("  tls:");
            println!("    certificate:      {}", server_tls.certificate.display());
            println!("    key:              {}", server_tls.key.display());
            println!(
                "    min version:      {}",
                match tls.min_version {
                    MinTlsVersion::V1_2 => "1.2",
                    MinTlsVersion::V1_3 => "1.3",
                }
            );
            println!("    alpn:             {}", tls.alpn.join(", "));
            if tls.plain {
                println!(
                    "    plain:            true (cleartext and TLS share the http port — \
                     accept it knowingly)"
                );
            } else {
                println!("    plain:            false");
            }
            if config.server.redirect_to_https {
                println!("    redirect_to_https: true");
            }
        }
        None => println!("  tls:              (disabled)"),
    }
    // A site's certificate is printed only when the site states one, for the same reason the
    // site table prints only stated keys: listing the inherited value per site would read as
    // though every site had been configured with it.
    for (name, site_tls) in resolved_site_tls(&config, path) {
        println!("    site {name}:        {}", site_tls.certificate.display());
    }

    // ACME, when configured. Printed even when the cache is empty, because that is a state an
    // operator has to be able to tell apart from a deployment whose issuance is working: silence
    // is the failure mode here, so what `check` can say is which authority, which names, and
    // whether a certificate for them is already cached.
    match &config.server.acme {
        Some(acme) => {
            println!("  acme:");
            println!("    authority:        {}", acme.authority.as_str());
            if acme.authority == volucer_core::config::Authority::Production {
                println!("      PRODUCTION:     real certificates, against per-week rate limits");
            }
            println!("    domains:          {}", acme.domains.join(", "));
            match acme.contact.is_empty() {
                true => println!(
                    "    contact:          (none: the CA has no way to warn you that renewals are \
                     failing)"
                ),
                false => println!("    contact:          {}", acme.contact.join(", ")),
            }
            let cache = resolve_tls_path(&acme.cache, path);
            println!("    cache:            {}", cache.display());
            // The cached certificate is *hashed* by the crate, so the file name is not derivable
            // here — what can be said is whether the directory has anything in it, which is the
            // question an operator actually has: has this deployment ever issued?
            let cached = std::fs::read_dir(&cache)
                .map(|entries| {
                    entries.filter_map(Result::ok).any(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with("cached_cert_")
                    })
                })
                .unwrap_or(false);
            println!(
                "      issued before:  {}",
                match cached {
                    true => "yes (a cached certificate exists, so a restart will not re-issue)",
                    false => "no (the first start will place an order)",
                }
            );

            // The days left on each certificate this deployment serves. Reported whenever ACME is
            // configured, because the *question* — is anything about to expire — is the same
            // whether or not issuance is working, and expiry is this feature's only symptom.
            if let Some(server_tls) = resolved_server_tls(&config, path) {
                if let Some(lifetime) = certificate_lifetime(&server_tls.certificate) {
                    println!("      {}", lifetime.report("serving certificate"));
                }
            }
            for (name, site_tls) in resolved_site_tls(&config, path) {
                if let Some(lifetime) = certificate_lifetime(&site_tls.certificate) {
                    println!(
                        "      {}",
                        lifetime.report(&format!("site {name} certificate"))
                    );
                }
            }
        }
        None => println!("  acme:             (disabled)"),
    }

    println!("  document root: {}", document_root.display());
    println!(
        "    exists:           {}",
        match (document_root.exists(), document_root.is_dir()) {
            (false, _) => "no",
            (true, true) => "yes",
            (true, false) => "no (it is a file, not a directory)",
        }
    );
    match &front_controller {
        Some(front) => {
            println!("    front controller: {front}");
            let script = document_root.join(front);
            println!(
                "      present:        {}",
                if script.is_file() { "yes" } else { "NO" }
            );
        }
        None => println!("    front controller: (none: only real .php files)"),
    }

    println!("  backends:");
    if config.backends.is_empty() {
        println!("    (none)");
    }
    for (name, backend) in &config.backends {
        let default = if config.default_backend.as_deref() == Some(name.as_str()) {
            " (default)"
        } else {
            ""
        };
        println!("    {name}: {}{default}", backend.kind().replace('_', "-"));
        println!(
            "      per-request reset: {}",
            if backend.resets_per_request() {
                "yes (PHP SAPI)"
            } else {
                "no (application-scoped worker)"
            }
        );
    }

    // A route is printed as the three matchers it can state, with `skip_stat` where it
    // applies — the saving is real and an operator should be able to see where the
    // router is still checking the filesystem and where it has stopped.
    let print_route = |route: &volucer_core::config::RouteConfig, worker: bool, indent: &str| {
        let host = route.host.as_deref().unwrap_or("*");
        let prefix = route.path_prefix.as_deref().unwrap_or("*");
        let suffix = route.path_suffix.as_deref().unwrap_or("*");
        println!("{indent}{host}{prefix}…{suffix} -> {}", route.backend);
        if volucer_core::config::skip_stat(route, worker) {
            println!("{indent}  stat: skipped (the pattern proves the extension)");
        }
    };
    let is_worker = |name: &str| {
        config.backends.get(name).is_some_and(|backend| {
            matches!(backend, volucer_core::config::BackendConfig::Worker(_))
        })
    };

    println!("  routes:");
    if config.routes.is_empty() {
        println!("    (none: every request goes to the default backend)");
    }
    for route in &config.routes {
        print_route(route, is_worker(&route.backend), "    ");
    }

    println!("  sites:");
    if config.sites.is_empty() {
        println!("    (none: [server] describes the only site)");
    }
    // Only what a site *states* is printed here. A key it does not state is the server's,
    // already printed above, and repeating it per site would make the table read as
    // though every site had been configured with the same values.
    for (name, site) in &config.sites {
        println!("    {name}: {}", site.hosts.join(", "));
        println!("      document root:    {}", site.document_root.display());
        if let Some(controller) = &site.front_controller {
            match controller.as_path() {
                Some(path) => println!("      front controller: {path}"),
                None => println!("      front controller: (none: only real .php files are served)"),
            }
        }
        if let Some(serve_only) = site.serve_only {
            println!("      serve only:       {serve_only}");
        }
        if let Some(routes) = &site.routes {
            println!("      routes:");
            for route in routes {
                print_route(route, is_worker(&route.backend), "        ");
            }
        }
        if let Some(backend) = &site.default_backend {
            println!("      default backend:  {backend}");
        }
    }

    println!("  static files:");
    println!("    {}", config.server.serve_only);
    if !config.sites.is_empty() {
        println!(
            "    this is the default site's setting; a site that states serve_only overrides it"
        );
    }
    if config.server.statics().enabled {
        println!("    enabled for the HTTP listener");
        println!(
            "      precompressed:   {}",
            if config.server.statics().precompressed {
                "yes (.br, then .gz)"
            } else {
                "no"
            }
        );
        println!(
            "      directory index: {}",
            if config.server.statics().directory_index {
                "index.html, with a redirect from the bare path"
            } else {
                "no"
            }
        );
        println!(
            "      dotfiles:        {:?}",
            config.server.statics().dotfiles
        );
        println!(
            "      cache-control:   {}",
            config.server.statics().cache_control
        );
    } else {
        println!("    (disabled: every request goes to a backend)");
    }

    let mut dirs = extra_php_dirs.to_vec();
    dirs.extend(php::default_search_dirs());
    let found = php::discover(&dirs)?;

    println!("  PHP binaries:");
    if found.is_empty() {
        println!("    (none found)");
    }
    for entry in &found {
        let binary = &entry.binary;
        let below_floor = binary.version.as_str() < volucer_server::MINIMUM_PHP_VERSION;
        println!(
            "    {:<4} PHP {} {:<8} {}{}",
            entry.role,
            binary.version,
            binary.flavour(),
            binary.path.display(),
            if below_floor {
                "  (below the worker floor; worker mode needs 8.2+)"
            } else {
                ""
            }
        );
    }

    // Warnings, not errors: every one of these is a normal intermediate state on
    // the way to a working deployment, and refusing to report anything else
    // because the root is not there yet would be the wrong trade.
    let warnings = volucer_core::preflight::inspect_with(&config, &document_root, Some(path));
    if !warnings.is_empty() {
        println!("  warnings:");
        for warning in &warnings {
            println!("    - {}", warning.message);
            if let Some(hint) = &warning.hint {
                println!("      {hint}");
            }
        }
    }

    println!("configuration OK");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_contention_line_carries_every_number_it_is_given() {
        let reported = Contention {
            backend: "app".to_string(),
            workers: 12,
            served: 40_000,
            mean_us: 812,
            p50_us: 1,
            p99_us: 16_384,
            busiest: 7,
            busiest_us: 41_233,
        };
        let line = contention_line(&reported);
        // Whitespace-collapsed comparison: the format is what matters, not the width of
        // the separators, and a test that pins the spacing fails on a harmless edit.
        let flat: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(
            flat,
            "slots: 12 worker(s), 40000 request(s), wait mean 812 us, wait p50 1 us, \
             wait p99 16384 us, busiest slot 7 at 41233 us"
        );
    }

    #[test]
    fn routing_uses_a_front_controller_by_default() {
        let routing = routing_for(Path::new("/srv/app"), Some("index.php"));
        assert_eq!(routing.front_controller.as_deref(), Some("index.php"));
        assert_eq!(routing.document_root, PathBuf::from("/srv/app"));
    }

    #[test]
    fn resolve_document_root_prefers_the_flags_that_are_set() {
        let text = r#"
[server]
http = "127.0.0.1:8080"
document_root = "/from/file"
"#;
        let config = Config::from_toml(text).expect("config parses");

        assert_eq!(
            resolve_document_root(Some(Path::new("/from/flag")), &config),
            PathBuf::from("/from/flag")
        );
        assert_eq!(
            resolve_document_root(None, &config),
            PathBuf::from("/from/file")
        );

        let bare = Config::from_toml("[server]\nhttp = \"127.0.0.1:8080\"\n").expect("parses");
        assert_eq!(resolve_document_root(None, &bare), PathBuf::from("."));
    }

    #[test]
    fn an_empty_front_controller_means_files_only() {
        let routing = routing_for(Path::new("."), None);
        assert_eq!(routing.front_controller, None);
    }

    /// Days left are reported in the units the decision is made in, and each band says something
    /// different.
    ///
    /// The thresholds are not round numbers chosen for looks: a renewal is attempted a third of the
    /// way through validity, so **under a third** means a renewal that should have happened has
    /// not, and **under a week** means several have failed in a row. Both are what an operator
    /// needs told, and the difference between them is the difference between "look at this" and
    /// "this has been broken for a while".
    ///
    /// Asserted per band rather than by containing a substring, because a single over-broad message
    /// would satisfy "contains days" for all of them.
    #[test]
    fn a_certificates_days_left_are_reported_with_the_warning_they_deserve() {
        let report = |days_left: i64| {
            CertificateLifetime {
                days_left,
                expired: days_left < 0,
            }
            .report("cert")
        };

        // Healthy: a bare count, no alarm.
        let healthy = report(75);
        assert_eq!(healthy, "cert: 75 day(s) left", "{healthy}");
        assert!(!healthy.contains("failed"), "{healthy}");

        // Under a third of 90 days: a renewal should have happened by now.
        let late = report(20);
        assert!(late.contains("20 day(s) left"), "{late}");
        assert!(late.contains("should have happened"), "{late}");

        // Under a week: several in a row have failed, and the CA is already warning.
        let urgent = report(3);
        assert!(urgent.contains("3 day(s) left"), "{urgent}");
        assert!(urgent.contains("several renewals"), "{urgent}");
        assert!(urgent.contains("contact"), "{urgent}");

        // Expired is its own message, because the consequence is different in kind: clients are
        // already being shown warnings.
        let expired = report(-1);
        assert!(expired.contains("EXPIRED"), "{expired}");
        assert!(expired.contains("client"), "{expired}");
    }

    /// A certificate file that is not a certificate is reported as *nothing*, not as a failure.
    ///
    /// `check` runs against a deployment that may not have its certificate yet — that is the state
    /// an ACME-managed first start is in — so a missing or unreadable file must not make `check`
    /// fail. The preflight warnings already report an unreadable certificate by name; this is the
    /// extra information when there *is* one.
    #[test]
    fn a_certificate_that_cannot_be_read_reports_no_lifetime_rather_than_failing() {
        let missing = std::env::temp_dir().join("volucer-no-such-cert-1.crt");
        assert!(
            certificate_lifetime(&missing).is_none(),
            "a missing certificate must report nothing"
        );

        let not_a_cert =
            std::env::temp_dir().join(format!("volucer-not-a-cert-{}.crt", std::process::id()));
        std::fs::write(&not_a_cert, b"this is not a certificate").expect("write the fixture");
        assert!(
            certificate_lifetime(&not_a_cert).is_none(),
            "a file that is not a certificate must report nothing"
        );
        let _ = std::fs::remove_file(&not_a_cert);
    }

    /// A real certificate's expiry is actually read, so the report is not always absent.
    ///
    /// The counterpart to the test above, and the one that would catch a parser wired up wrongly:
    /// "reports nothing for a bad file" passes just as well against code that reports nothing for
    /// every file. A certificate generated here has a real `not_after`, so a lifetime must come
    /// back — and its sign is checked rather than its value, because the fixture's validity window
    /// is not this test's business.
    #[test]
    fn a_real_certificates_expiry_is_read() {
        let generated = rcgen::generate_simple_self_signed(vec!["example.test".to_string()])
            .expect("generate a certificate");
        let path =
            std::env::temp_dir().join(format!("volucer-real-cert-{}.crt", std::process::id()));
        std::fs::write(&path, generated.cert.pem()).expect("write the fixture");

        let lifetime =
            certificate_lifetime(&path).expect("a real certificate must have a readable lifetime");
        assert!(
            !lifetime.expired,
            "a freshly generated certificate must not read as expired"
        );
        assert!(
            lifetime.days_left > 0,
            "a freshly generated certificate must have days left: {}",
            lifetime.days_left
        );

        let _ = std::fs::remove_file(&path);
    }

    /// The restart-only list names the three classes that genuinely cannot be replaced, and
    /// **not** the two that an earlier draft wrongly listed.
    ///
    /// This is the text an operator reads on every reload, so it is the thing that tells them a
    /// setting they edited is not in effect. It is asserted rather than trusted for two reasons.
    /// First, the list is a claim about the implementation, and the implementation moved: a site's
    /// `routes` and `[server.statics]` on/off were both listed as restart-only here and are both
    /// read per request out of the swapped table, so both *are* reloadable — an operator told
    /// otherwise would restart a server for nothing.
    /// Second, this list is what the README's reloadable/restart-only split is copied from, so a
    /// silent drift between the two is a documentation bug in a file no test reads.
    ///
    /// The assertion is deliberately about class rather than wording: what matters is that a bound
    /// socket, a running process and a built `rustls::ServerConfig` are named, and that nothing
    /// the reload *does* apply appears.
    #[test]
    fn the_restart_only_list_names_what_cannot_be_replaced_and_nothing_else() {
        let lines = restart_only();
        let joined = lines.join("\n");

        for required in ["listener addresses", "backends", "min_version"] {
            assert!(
                joined.contains(required),
                "the restart-only list must name {required:?}, or an operator who edited it gets \
                 silence: {joined}"
            );
        }

        // The two an earlier draft got wrong, in the code's favour. Both are read per request out
        // of the swapped table, so telling an operator they are restart-only is telling them to
        // restart for nothing.
        for wrong in ["routes", "statics", "static_files"] {
            assert!(
                !joined.contains(wrong),
                "{wrong:?} is read per request out of the swapped table and is therefore \
                 reloadable; listing it as restart-only is the half-reload claim in reverse: \
                 {joined}"
            );
        }
    }

    /// A PEM blob of a chain plus a key splits at the key, not at the second block.
    ///
    /// `rustls-acme` hands the issued certificate over as one blob, and the shape is a *chain*
    /// followed by a key. The tempting implementation — "the first block is the certificate, the
    /// second is the key" — is wrong the moment a chain has an intermediate, and it fails in a way
    /// that reads as a key problem rather than a parsing one. So the split is by PEM label, and this
    /// pins that: a three-block blob must give a two-certificate chain.
    #[test]
    fn a_pem_blob_splits_at_the_key_not_at_the_second_block() {
        let chain_then_key = "-----BEGIN CERTIFICATE-----\nleaf\n-----END CERTIFICATE-----\n\
                              -----BEGIN CERTIFICATE-----\nintermediate\n-----END CERTIFICATE-----\n\
                              -----BEGIN PRIVATE KEY-----\nkey\n-----END PRIVATE KEY-----\n";
        let (cert, key) = split_pem_pair(chain_then_key.as_bytes());

        let cert = String::from_utf8_lossy(cert);
        let key = String::from_utf8_lossy(key);
        assert!(
            cert.contains("leaf") && cert.contains("intermediate"),
            "the certificate half must be the whole chain, intermediates included: {cert}"
        );
        assert!(
            !cert.contains("BEGIN PRIVATE KEY"),
            "the chain must not carry the key: {cert}"
        );
        assert!(
            key.starts_with("-----BEGIN PRIVATE KEY-----"),
            "the key half must begin at the key's own marker: {key}"
        );
        assert!(
            !key.contains("intermediate"),
            "the key half must not carry a certificate: {key}"
        );

        // The single-certificate shape, which is the common one, and the shapes of key marker an
        // `openssl` invocation actually produces.
        for marker in [
            "-----BEGIN PRIVATE KEY-----",
            "-----BEGIN RSA PRIVATE KEY-----",
            "-----BEGIN EC PRIVATE KEY-----",
        ] {
            let blob = format!(
                "-----BEGIN CERTIFICATE-----\nonly\n-----END CERTIFICATE-----\n{marker}\nk\n"
            );
            let (cert, key) = split_pem_pair(blob.as_bytes());
            assert_eq!(
                String::from_utf8_lossy(cert).trim_end(),
                "-----BEGIN CERTIFICATE-----\nonly\n-----END CERTIFICATE-----",
                "one certificate must be the whole chain half ({marker})"
            );
            assert!(
                String::from_utf8_lossy(key).starts_with(marker),
                "the key half must begin at {marker}"
            );
        }

        // No key at all: the whole blob is the chain and the key half is empty, so the loader
        // reports a missing key rather than the chain being silently truncated.
        let (cert, key) = split_pem_pair(b"-----BEGIN CERTIFICATE-----\nonly\n");
        assert_eq!(cert, b"-----BEGIN CERTIFICATE-----\nonly\n");
        assert!(
            key.is_empty(),
            "a blob with no key must give an empty key half, not a truncated chain"
        );
    }
}
