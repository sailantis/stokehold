//! Resolving a configured command into a program and its arguments.
//!
//! This lives here rather than beside the pool that spawns a worker because it
//! has two callers with different jobs: the pool resolves a program in order to
//! run it, and `stokehold check` resolves one in order to *report* it. A check
//! that used its own copy of this logic could pass on a programme the pool then
//! refuses to run, which is the failure this module exists to prevent.

use std::path::PathBuf;

/// Split a command line into a program and its arguments.
///
/// Configuration carries one string (`command = "php worker.php"`), so it has to
/// be split into an argv. This is a deliberately small tokenizer, not a shell:
/// double quotes group, a backslash escapes the next character inside and outside
/// quotes, and there are no variables, globs, subshells or operators. That covers
/// every documented example — including a Windows path, where a backslash before
/// an ordinary character stays a backslash — while keeping a configuration file
/// from being able to run arbitrary shell syntax.
pub fn split_command(command: &str) -> Result<(String, Vec<String>), String> {
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
/// both, and it means a preflight check is checking the same file that will
/// actually be run.
///
/// A name that cannot be found is returned unchanged, so the caller's
/// "does not exist" message quotes what the operator wrote rather than a path
/// this function invented.
pub fn resolve_program(program: &str) -> PathBuf {
    let path = std::path::Path::new(program);
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
    fn a_program_with_a_separator_is_not_looked_up_on_path() {
        let resolved = resolve_program("definitely/not/a/real/program");
        assert_eq!(resolved, PathBuf::from("definitely/not/a/real/program"));
    }

    /// A bare name that is nowhere on `PATH` is reported as written, so an
    /// operator reads back their own typo instead of a path nobody asked for.
    #[test]
    fn an_unknown_bare_name_is_returned_unchanged() {
        let resolved = resolve_program("definitely-not-a-php-binary");
        assert_eq!(resolved, PathBuf::from("definitely-not-a-php-binary"));
    }

    /// A directory that exists is not a program, so it must not resolve.
    ///
    /// `is_file` rather than `exists` is the whole difference: a relative name
    /// that happens to name a directory would otherwise be "resolved" and fail
    /// only when something tried to execute it.
    #[test]
    fn a_directory_is_not_a_resolved_program() {
        let resolved = resolve_program("./");
        assert!(!resolved.is_file(), "{resolved:?}");
    }
}
