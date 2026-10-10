//! The `stokehold` command line interface.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::{Parser, Subcommand};

use stokehold_core::config::Config;
use stokehold_core::php;
use stokehold_fcgi::cgi::Routing;
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

/// Run the listener until interrupted.
fn serve(
    path: &Path,
    document_root: &Path,
    front_controller: &str,
    max_connections: usize,
) -> Result<(), ServerError> {
    let config = load_and_validate(path)?;
    let routing = routing_for(document_root, front_controller);

    // Backends start before the listener binds: a misconfigured backend should
    // be reported before a port is occupied.
    let dispatcher = Dispatcher::from_config(&config, routing)?;
    for name in dispatcher.backend_names() {
        tracing::info!(backend = %name, "backend ready");
    }

    let address = config
        .server
        .fastcgi
        .clone()
        .ok_or(ServerError::NoListener)?;

    let listener = FastCgiListener::bind(&address, dispatcher, Limits { max_connections })?;
    let bound = listener.local_addr()?;
    let stats = Arc::clone(&listener.stats());

    println!("stokehold listening for FastCGI on {bound}");
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
    if config.server.http.is_some() {
        println!("  note: the HTTP listener is not implemented yet; only FastCGI is served");
    }
    println!("press Ctrl-C to stop");

    install_shutdown_handler(listener.shutdown_handle());

    listener.run()?;
    println!(
        "stopped after {} request(s)",
        stats.requests.load(Ordering::Relaxed)
    );
    Ok(())
}

/// Stop the listener on the first Ctrl-C.
///
/// A second Ctrl-C forces an exit, so a stuck shutdown cannot trap the operator.
/// Forcing is safe because the lifecycle guard reaps the PHP processes when the
/// process dies.
fn install_shutdown_handler(shutdown: stokehold_server::ShutdownHandle) {
    let forced = Arc::new(AtomicBool::new(false));
    let result = ctrlc::set_handler(move || {
        if forced.swap(true, Ordering::SeqCst) {
            eprintln!("\nforced exit");
            std::process::exit(130);
        }
        eprintln!("\nshutting down; press Ctrl-C again to force");
        shutdown.shutdown();
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
