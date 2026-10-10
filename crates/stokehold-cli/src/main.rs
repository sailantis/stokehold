//! The `stokehold` command line interface.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use stokehold_core::config::Config;
use stokehold_core::php;

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
        Command::Serve { config } => serve(&config),
        Command::Check { config, php_dirs } => check(&config, &php_dirs),
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

fn load_and_validate(path: &Path) -> Result<Config, stokehold_core::Error> {
    let config = Config::load(path)?;
    config.validate()?;
    tracing::info!(path = %path.display(), "configuration loaded");
    Ok(config)
}

/// `serve` is wired up once the supervisor and listeners exist.
fn serve(path: &Path) -> Result<(), stokehold_core::Error> {
    let config = load_and_validate(path)?;
    let backends: Vec<&str> = config.backends.keys().map(String::as_str).collect();

    Err(stokehold_core::Error::Config(format!(
        "serve is not implemented yet; parsed OK with {} backend(s) [{}] and listeners http={:?} fastcgi={:?}",
        backends.len(),
        backends.join(", "),
        config.server.http,
        config.server.fastcgi
    )))
}

/// `check` validates configuration and reports the PHP binaries found here.
fn check(path: &Path, extra_php_dirs: &[PathBuf]) -> Result<(), stokehold_core::Error> {
    let config = load_and_validate(path)?;

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
