//! The `stokehold` command line interface.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::{Parser, Subcommand};

use stokehold_core::config::Config;
use stokehold_core::php;
use stokehold_fcgi::cgi::Routing;
use stokehold_http::{HttpLimits, RunningHttp, StaticState};
use stokehold_server::{Dispatcher, FastCgiListener, Limits, ServerError};

#[derive(Parser, Debug)]
#[command(
    name = "stokehold",
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
        /// Path to `stokehold.toml`.
        #[arg(short, long, default_value = "stokehold.toml")]
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
        /// Path to `stokehold.toml`.
        #[arg(short, long, default_value = "stokehold.toml")]
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
            eprintln!("stokehold: {err}");
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
/// a per-request peak. See the README in `sailantis/stokehold-php-sdk`.
fn version() {
    println!("stokehold {}", env!("CARGO_PKG_VERSION"));
    println!("  front-ends:       HTTP/1.1, HTTP/2 (cleartext), FastCGI");
    println!(
        "  worker protocol:  {}",
        stokehold_server::WORKER_PROTOCOL_VERSION
    );
    println!(
        "  php floor:        {}",
        stokehold_server::MINIMUM_PHP_VERSION
    );
    println!("  backends:         worker, external fpm, spawn_fpm, spawn_cgi");
}

fn init_tracing(verbosity: u8) {
    use tracing_subscriber::EnvFilter;

    let fallback = match verbosity {
        0 => "stokehold=info",
        1 => "stokehold=debug",
        _ => "stokehold=trace",
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
    let routing = routing_for(&document_root, front_controller.as_deref());
    let static_state = config.server.static_files.clone();
    let http_address = config.server.http.clone();
    let fastcgi_address = config.server.fastcgi.clone();
    if http_address.is_none() && fastcgi_address.is_none() {
        return Err(ServerError::NoListener);
    }

    // Backends start before a listener binds: a misconfigured backend should be
    // reported before a port is occupied. One dispatcher serves both front-ends,
    // shared without a lock because each backend slot synchronises internally —
    // so two requests to two workers run at the same time.
    let dispatcher = Dispatcher::from_config(&config, routing.clone())?;
    for name in dispatcher.backend_names() {
        tracing::info!(backend = %name, "backend ready");
    }
    let dispatcher = Arc::new(dispatcher);

    println!("stokehold");
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
    if static_state.enabled {
        println!("  static files:     serving from the document root");
    }

    // Reported before the listeners start, not after: a document root that is
    // not there is the mistake that produces a 404 for every static file and a
    // front controller PHP will not run, and noticing it in the log is worth
    // more than the process refusing to start. Only in the log, so a deployment
    // that creates the root during startup still works.
    for warning in stokehold_core::preflight::inspect(&config, &document_root) {
        tracing::warn!("{}", warning.message);
        if let Some(hint) = &warning.hint {
            tracing::warn!("  {hint}");
        }
    }

    // The FastCGI listener binds first, because binding is synchronous here while
    // the HTTP listener needs a runtime — and an occupied port should be reported
    // before the runtime is built.
    let fastcgi = match &fastcgi_address {
        Some(address) => {
            let listener = FastCgiListener::bind(
                address,
                Arc::clone(&dispatcher),
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

    let http = match &http_address {
        Some(address) => {
            let running = RunningHttp::start(
                address,
                Arc::clone(&dispatcher),
                routing,
                HttpLimits {
                    max_connections,
                    ..HttpLimits::default()
                },
                StaticState {
                    config: static_state.clone(),
                    document_root: document_root.to_path_buf(),
                },
            )
            .map_err(|err| ServerError::Listener {
                reason: err.to_string(),
            })?;
            println!("  http:             {}", running.local_addr());
            Some(running)
        }
        None => {
            println!("  http:             (disabled)");
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
    }

    println!("stopped");
    Ok(())
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

/// Stop the listeners on the first Ctrl-C.
///
/// A second Ctrl-C forces an exit, so a stuck shutdown cannot trap the operator.
/// Forcing is safe because the lifecycle guard reaps the PHP processes when the
/// process dies.
///
/// The FastCGI listener takes a [`ShutdownHandle`], which is a shared flag. The
/// HTTP listener runs on another thread and is stopped by *dropping* its handle,
/// which is what `serve` does after this — so the only thing this handler has to
/// do for it is set the static flag that `wait_for_shutdown` polls.
fn install_shutdown_handler(shutdown: Option<stokehold_server::ShutdownHandle>) {
    let forced = Arc::new(AtomicBool::new(false));
    let result = ctrlc::set_handler(move || {
        if forced.swap(true, Ordering::SeqCst) {
            eprintln!("\nforced exit");
            std::process::exit(130);
        }
        eprintln!("\nshutting down; press Ctrl-C again to force");
        SHUTDOWN.store(true, Ordering::Relaxed);
        if let Some(shutdown) = &shutdown {
            shutdown.shutdown();
        }
    });
    if let Err(err) = result {
        // Not fatal: without a handler the listener runs until the process is
        // signalled, and the lifecycle guard still reaps the children.
        tracing::debug!(%err, "no Ctrl-C handler; stop the process with a signal");
    }
}

/// `check` validates configuration and reports the PHP binaries found here.
fn check(
    path: &Path,
    document_root_flag: Option<&Path>,
    extra_php_dirs: &[PathBuf],
) -> Result<(), stokehold_core::Error> {
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
    match &config.server.fastcgi {
        Some(fcgi) => println!("    fastcgi: {fcgi}"),
        None => println!("    fastcgi: (disabled)"),
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

    println!("  routes:");
    if config.routes.is_empty() {
        println!("    (none: every request goes to the default backend)");
    }
    for route in &config.routes {
        let host = route.host.as_deref().unwrap_or("*");
        let path = route.path_prefix.as_deref().unwrap_or("*");
        println!("    {host}{path} -> {}", route.backend);
    }

    println!("  static files:");
    if config.server.static_files.enabled {
        println!("    enabled for the HTTP listener");
        println!(
            "      precompressed:   {}",
            if config.server.static_files.precompressed {
                "yes (.br, then .gz)"
            } else {
                "no"
            }
        );
        println!(
            "      directory index: {}",
            if config.server.static_files.directory_index {
                "index.html, with a redirect from the bare path"
            } else {
                "no"
            }
        );
        println!(
            "      dotfiles:        {:?}",
            config.server.static_files.dotfiles
        );
        println!(
            "      cache-control:   {}",
            config.server.static_files.cache_control
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
        let below_floor = binary.version.as_str() < stokehold_server::MINIMUM_PHP_VERSION;
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
    let warnings = stokehold_core::preflight::inspect(&config, &document_root);
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
