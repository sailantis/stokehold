//! What a worker process is started from: the resolved program, its arguments, its
//! environment, and the checks that make a typo a startup error by name rather than a
//! confusing `CreateProcess` / `execvp` failure later. **No process is created here** —
//! this module only decides *what* to run.


use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use stokehold_core::config::{ChecksumMode, WorkerConfig, WorkerTransport};
use stokehold_worker::{Checksum};
use crate::error::{Result, ServerError};


/// What a worker process is started from.
#[derive(Debug, Clone)]
pub(super) struct Spec {
    /// The resolved program, so a missing interpreter is a startup error rather
    /// than a confusing `CreateProcess` / `execvp` failure later.
    pub(super) binary: PathBuf,
    pub(super) args: Vec<String>,
    pub(super) working_dir: Option<PathBuf>,
    pub(super) env: BTreeMap<String, String>,
    /// The checksum policy to offer at handshake.
    ///
    /// On the spec rather than passed to `boot` because a *replaced* worker must be
    /// handshaken on the same terms as the one it replaces: a pool that negotiated
    /// `optional` at startup and then respawned a worker under `required` would have one
    /// worker verifying and the rest not, which is a difference nothing would report.
    pub(super) checksum: Checksum,
    /// How to connect to the worker. On the spec for the same reason as `checksum`: a
    /// replaced worker must be reached the same way as the one it replaces.
    pub(super) transport: WorkerTransport,
}

impl Spec {
    /// Split, resolve and check a configured worker command.
    ///
    /// Checked here rather than at spawn time so that a typo in `command` or
    /// `working_dir` is reported by name, before any port is bound and before a
    /// pool half-exists.
    pub(super) fn from_config(name: &str, config: &WorkerConfig) -> Result<Self> {
        let (program, args) =
            split_command(&config.command).map_err(|reason| ServerError::Misconfigured {
                backend: name.to_string(),
                reason,
            })?;
        let binary = resolve_program(&program);

        if !binary.is_file() {
            return Err(ServerError::Misconfigured {
                backend: name.to_string(),
                reason: format!(
                    "the worker program {} does not exist (from command {:?})",
                    binary.display(),
                    config.command
                ),
            });
        }

        if let Some(dir) = &config.working_dir {
            if !dir.is_dir() {
                return Err(ServerError::Misconfigured {
                    backend: name.to_string(),
                    reason: format!("working_dir {} is not a directory", dir.display()),
                });
            }
        }

        Ok(Spec {
            binary,
            args,
            working_dir: config.working_dir.clone(),
            env: config.env.clone(),
            checksum: match config.checksum {
                ChecksumMode::Required => Checksum::Required,
                ChecksumMode::Optional => Checksum::Optional,
            },
            transport: config.transport,
        })
    }
}

/// Split a command line into a program and its arguments.
///
/// Configuration carries one string (`command = "php worker.php"`), so it has to
/// be split into an argv. This is a deliberately small tokenizer, not a shell:
/// double quotes group, a backslash escapes the next character inside and outside
/// quotes, and there are no variables, globs, subshells or operators. That covers
/// every documented example — including a Windows path, where a backslash before
/// an ordinary character stays a backslash — while keeping a configuration file
/// from being able to run arbitrary shell syntax.
fn split_command(command: &str) -> std::result::Result<(String, Vec<String>), String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quoted = false;
    let mut chars = command.chars();

    while let Some(character) = chars.next() {
        match character {
            '"' => {
                // A quote starts a token even when it is empty, so `""` is an
                // argument rather than nothing.
                quoted = !quoted;
                started = true;
            }
            '\\' => {
                started = true;
                match chars.next() {
                    // `\\` and `\"` are the escapes that mean something; a
                    // backslash before anything else is kept, because that is
                    // what a Windows path needs.
                    Some(next @ ('\\' | '"')) => current.push(next),
                    Some(next) => {
                        current.push('\\');
                        current.push(next);
                    }
                    None => current.push('\\'),
                }
            }
            character if character.is_whitespace() && !quoted => {
                if started {
                    tokens.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            character => {
                started = true;
                current.push(character);
            }
        }
    }

    if quoted {
        return Err(format!("unbalanced quote in command {command:?}"));
    }
    if started {
        tokens.push(current);
    }

    let mut tokens = tokens.into_iter();
    let Some(program) = tokens.next() else {
        return Err("the worker command is empty".to_string());
    };
    Ok((program, tokens.collect()))
}

/// The program to execute for a command's first token.
///
/// `CreateProcess` on Windows and `execvp` on Unix both search `PATH` for a bare
/// name, but only the search is platform-specific and neither reports *which*
/// file it picked. Resolving here means one code path and one error message on
/// both, and it means the preflight check in [`Spec::from_config`] is checking
/// the same file that will actually be run.
fn resolve_program(program: &str) -> PathBuf {
    let path = Path::new(program);
    // A name with a separator is a path, not a `PATH` lookup.
    if path.components().count() > 1 {
        return path.to_path_buf();
    }
    find_on_path(program).unwrap_or_else(|| path.to_path_buf())
}

/// Look a bare program name up on `PATH`.
fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;

    #[cfg(windows)]
    let extensions = path_extensions();

    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }

        #[cfg(windows)]
        for extension in &extensions {
            let candidate = dir.join(format!("{program}.{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// The extensions a `PATH` search should try on Windows.
///
/// Read from `PATHEXT` rather than hard-coded, because `php.exe` is not the only
/// shape a PHP install can take — a shim manager may well provide `php.bat`.
#[cfg(windows)]
fn path_extensions() -> Vec<String> {
    let raw = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    raw.split(';')
        .map(|extension| {
            extension
                .trim()
                .trim_start_matches('.')
                .to_ascii_lowercase()
        })
        .filter(|extension| !extension.is_empty())
        .collect()
}


#[cfg(test)]
mod tests {
    use super::*;

#[test]
fn a_command_is_split_on_whitespace() {
    let (program, args) = split_command("php worker.php").unwrap();
    assert_eq!(program, "php");
    assert_eq!(args, vec!["worker.php"]);
}

#[test]
fn quotes_group_an_argument_that_contains_spaces() {
    let (program, args) =
        split_command(r#""C:\Program Files\php\php.exe" -d x=1 worker.php"#).unwrap();
    assert_eq!(program, r"C:\Program Files\php\php.exe");
    assert_eq!(args, vec!["-d", "x=1", "worker.php"]);
}

#[test]
fn a_windows_path_keeps_its_backslashes() {
    // The escapes that mean something are `\\` and `\"`; anything else keeps
    // the backslash, because otherwise every Windows path would need
    // doubling — which is exactly the papercut that makes a config file
    // unreadable.
    let (program, args) = split_command(r"C:\php\php.exe C:\app\worker.php").unwrap();
    assert_eq!(program, r"C:\php\php.exe");
    assert_eq!(args, vec![r"C:\app\worker.php"]);
}

#[test]
fn an_empty_quoted_argument_survives() {
    let (_, args) = split_command(r#"php "" worker.php"#).unwrap();
    assert_eq!(args, vec!["", "worker.php"]);
}

#[test]
fn an_unbalanced_quote_is_refused() {
    let err = split_command(r#"php "worker.php"#).unwrap_err();
    assert!(err.contains("unbalanced quote"), "{err}");
}

#[test]
fn an_empty_command_is_refused() {
    let err = split_command("   ").unwrap_err();
    assert!(err.contains("empty"), "{err}");
}

#[test]
fn a_missing_program_is_a_misconfiguration_not_a_panic() {
    let config = WorkerConfig {
        command: "definitely-not-a-php-binary worker.php".to_string(),
        workers: 1,
        memory_limit_mb: None,
        max_requests: None,
        working_dir: None,
        env: Default::default(),
        checksum: Default::default(),
        transport: Default::default(),
    };
    let err = Spec::from_config("test", &config).unwrap_err();
    assert!(matches!(err, ServerError::Misconfigured { .. }), "{err}");
    assert!(err.to_string().contains("does not exist"), "{err}");
}

#[test]
fn a_working_directory_that_is_not_a_directory_is_refused() {
    // A path that certainly exists but is not a directory: the test binary
    // itself.
    let binary = std::env::current_exe().expect("the test binary path");
    let config = WorkerConfig {
        command: binary.display().to_string(),
        workers: 1,
        memory_limit_mb: None,
        max_requests: None,
        working_dir: Some(binary),
        env: Default::default(),
        checksum: Default::default(),
        transport: Default::default(),
    };
    let err = Spec::from_config("test", &config).unwrap_err();
    assert!(err.to_string().contains("is not a directory"), "{err}");
}

#[test]
fn an_empty_command_is_a_misconfiguration() {
    let config = WorkerConfig {
        command: String::new(),
        workers: 1,
        memory_limit_mb: None,
        max_requests: None,
        working_dir: None,
        env: Default::default(),
        checksum: Default::default(),
        transport: Default::default(),
    };
    let err = Spec::from_config("test", &config).unwrap_err();
    assert!(err.to_string().contains("empty"), "{err}");
}

#[test]
fn a_program_with_a_separator_is_not_looked_up_on_path() {
    let resolved = resolve_program("definitely/not/a/real/program");
    assert_eq!(resolved, PathBuf::from("definitely/not/a/real/program"));
}
}
