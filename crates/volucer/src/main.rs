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
    let shared = volucer_http::SharedState::new((*sites).clone());

    // The FastCGI listener binds first, because binding is synchronous here while
    // the HTTP listener needs a runtime — and an occupied port should be reported
    // before the runtime is built.
    let fastcgi = match &fastcgi_address {
        Some(address) => {
            let listener = FastCgiListener::bind_shared(
                address,
                Arc::clone(&dispatcher),
                shared.clone(),
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

    let http = match &http_address {
        Some(address) => {
            let running = RunningHttp::start_with_tls(
                address,
                Arc::clone(&dispatcher),
                shared.clone(),
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
                shared.clone(),
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

    install_shutdown_handler(fastcgi.as_ref().map(|l| l.shutdown_handle()));

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

/// Stop the listeners on the first Ctrl-C or SIGTERM.
///
/// A second signal forces an exit, so a stuck shutdown cannot trap the operator.
///
/// **SIGTERM is what a supervisor actually sends**, and until this handled it the
/// process died on the spot with no chance to run a destructor — so `systemctl stop`,
/// `docker stop` and a plain `kill` all left the php-cgi children of a `spawn_cgi`
/// pool running. They accumulated: a benchmark host reached 68 orphans, and each one
/// held its port, its PHP state and its share of the CPU. The `termination` feature of
/// `ctrlc` is what makes the handler cover SIGTERM as well as SIGINT; without it this
/// function only ever saw a Ctrl-C.
///
/// Forcing is safe now, and was not before: the lifecycle guard reaps the PHP
/// processes when the process dies, *provided* the death is one it can observe.
///
/// The FastCGI listener takes a [`ShutdownHandle`], which is a shared flag. The
/// HTTP listener runs on another thread and is stopped by *dropping* its handle,
/// which is what `serve` does after this — so the only thing this handler has to
/// do for it is set the static flag that `wait_for_shutdown` polls.
fn install_shutdown_handler(shutdown: Option<volucer_server::ShutdownHandle>) {
    let forced = Arc::new(AtomicBool::new(false));
    let result = ctrlc::set_handler(move || {
        if forced.swap(true, Ordering::SeqCst) {
            eprintln!("\nforced exit");
            std::process::exit(130);
        }
        eprintln!("\nshutting down; signal again to force");
        SHUTDOWN.store(true, Ordering::Relaxed);
        if let Some(shutdown) = &shutdown {
            shutdown.shutdown();
        }
    });
    if let Err(err) = result {
        // Not fatal, but it means a SIGTERM will leak the pools: say so at a level
        // an operator sees, because the symptom (surviving PHP processes) appears
        // long after the cause.
        tracing::warn!(%err, "no signal handler installed; stopping this process may leave its PHP children running");
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
}
