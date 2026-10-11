//! The `stokehold` command line interface.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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
        #[arg(long, default_value = ".")]
        document_root: PathBuf,

        /// Front controller, relative to the document root. Set it empty to
        /// serve only actual files, matching nginx's `location ~ \.php$`.
        #[arg(long, default_value = "index.php")]
        front_controller: String,

        /// Maximum concurrent client connections.
        #[arg(long, default_value_t = 512)]
        max_connections: usize,
    },

    /// Validate the configuration and report what was found on this host.
    Check {
        /// Path to `stokehold.toml`.
        #[arg(short, long, default_value = "stokehold.toml")]
        config: PathBuf,

        /// Extra directories to search for PHP binaries.
        #[arg(long = "php-dir")]
        php_dirs: Vec<PathBuf>,
    },
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
        } => serve(&config, &document_root, &front_controller, max_connections),
        Command::Check { config, php_dirs } => check(&config, &php_dirs).map_err(ServerError::from),
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

fn routing_for(document_root: &Path, front_controller: &str) -> Routing {
    let front_controller = front_controller.trim();
    Routing {
        document_root: document_root.to_path_buf(),
        front_controller: if front_controller.is_empty() {
            None
        } else {
            Some(front_controller.to_string())
        },
        ..Routing::default()
    }
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
    document_root: &Path,
    front_controller: &str,
    max_connections: usize,
) -> Result<(), ServerError> {
    let config = load_and_validate(path)?;
    let routing = routing_for(document_root, front_controller);
    let static_state = config.server.static_files.clone();
    let http_address = config.server.http.clone();
    let fastcgi_address = config.server.fastcgi.clone();
    if http_address.is_none() && fastcgi_address.is_none() {
        return Err(ServerError::NoListener);
    }

    // Backends start before a listener binds: a misconfigured backend should be
    // reported before a port is occupied. One dispatcher serves both front-ends,
    // behind a mutex because a backend slot serves one request at a time whichever
    // protocol arrives.
    let dispatcher = Dispatcher::from_config(&config, routing.clone())?;
    for name in dispatcher.backend_names() {
        tracing::info!(backend = %name, "backend ready");
    }
    let dispatcher = Arc::new(Mutex::new(dispatcher));

    println!("stokehold");
    println!("  document root:    {}", document_root.display());
    println!(
        "  front controller: {}",
        if front_controller.trim().is_empty() {
            "(none: only real .php files are served)"
        } else {
            front_controller
        }
    );
    println!("  backends:         {}", config.backends.len());
    if !config.routes.is_empty() {
        println!("  routes:           {}", config.routes.len());
    }
    if static_state.enabled {
        println!("  static files:     serving from the document root");
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
fn check(path: &Path, extra_php_dirs: &[PathBuf]) -> Result<(), stokehold_core::Error> {
    let config = Config::load(path)?;
    config.validate()?;

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
        println!(
            "    {:<4} PHP {} {:<8} {}",
            entry.role,
            binary.version,
            binary.flavour(),
            binary.path.display()
        );
    }

    println!("configuration OK");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_uses_a_front_controller_by_default() {
        let routing = routing_for(Path::new("/srv/app"), "index.php");
        assert_eq!(routing.front_controller.as_deref(), Some("index.php"));
        assert_eq!(routing.document_root, PathBuf::from("/srv/app"));
    }

    #[test]
    fn an_empty_front_controller_means_files_only() {
        let routing = routing_for(Path::new("."), "   ");
        assert_eq!(routing.front_controller, None);
    }
}
