//! Discovery and identification of PHP binaries.
//!
//! stokehold never embeds PHP, so the only thing it needs to know about a PHP
//! installation is which binary to spawn and what it is: the SAPI decides which
//! backend mode the binary can serve, and the ZTS/NTS flavour matters when
//! diagnosing threading-related extension problems.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};

/// Candidate file names, in preference order, for the FastCGI-capable binary.
#[cfg(windows)]
pub const CGI_NAMES: &[&str] = &["php-cgi.exe"];
#[cfg(not(windows))]
pub const CGI_NAMES: &[&str] = &["php-cgi"];

/// Candidate file names, in preference order, for the CLI binary.
#[cfg(windows)]
pub const CLI_NAMES: &[&str] = &["php.exe"];
#[cfg(not(windows))]
pub const CLI_NAMES: &[&str] = &["php"];

/// Candidate file names for the FPM binary. Absent on Windows by definition.
pub const FPM_NAMES: &[&str] = &["php-fpm"];

/// The SAPI a PHP binary runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sapi {
    Cli,
    Cgi,
    Fpm,
    /// Something we recognise as PHP but not as one of the three we use.
    Other(String),
}

impl Sapi {
    fn from_name(name: &str) -> Self {
        match name {
            "cli" => Sapi::Cli,
            "cgi" | "cgi-fcgi" => Sapi::Cgi,
            "fpm-fcgi" => Sapi::Fpm,
            other => Sapi::Other(other.to_string()),
        }
    }

    /// True when this SAPI can serve FastCGI requests for a CGI-mode backend.
    pub fn serves_fastcgi(&self) -> bool {
        matches!(self, Sapi::Cgi | Sapi::Fpm)
    }
}

/// A PHP binary plus what `-v` told us about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhpBinary {
    pub path: PathBuf,
    pub version: String,
    pub sapi: Sapi,
    /// `Some(true)` for ZTS, `Some(false)` for NTS, `None` when unreported.
    pub zts: Option<bool>,
    /// The raw first line, kept for diagnostics.
    pub banner: String,
}

impl PhpBinary {
    /// Run `<path> -v` and parse the banner.
    pub fn probe(path: &Path) -> Result<Self> {
        let output = Command::new(path).arg("-v").output()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let banner = stdout
            .lines()
            .chain(stderr.lines())
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();

        Self::parse_banner(path, &banner)
    }

    /// Parse the first line of `php -v` output.
    ///
    /// Observed shapes:
    /// - `PHP 8.3.31 (cli) (built: May  5 2026 17:23:34) (ZTS Visual C++ 2019 x64)`
    /// - `PHP 8.3.31 (fpm-fcgi) (built: ...) (NTS)`
    pub fn parse_banner(path: &Path, banner: &str) -> Result<Self> {
        let rest = banner
            .strip_prefix("PHP ")
            .ok_or_else(|| Error::PhpUnrecognizedVersion {
                path: path.to_path_buf(),
            })?;

        let mut parts = rest.split_whitespace();
        let version = parts.next().unwrap_or_default().to_string();
        if version.is_empty() {
            return Err(Error::PhpUnrecognizedVersion {
                path: path.to_path_buf(),
            });
        }

        // The SAPI is the first parenthesised token, e.g. `(cli)`.
        let sapi = rest
            .find('(')
            .and_then(|open| {
                rest[open + 1..]
                    .find(')')
                    .map(|close| &rest[open + 1..open + 1 + close])
            })
            .map(Sapi::from_name)
            .unwrap_or(Sapi::Other("unknown".to_string()));

        let zts = if rest.contains("ZTS") {
            Some(true)
        } else if rest.contains("NTS") {
            Some(false)
        } else {
            None
        };

        Ok(PhpBinary {
            path: path.to_path_buf(),
            version,
            sapi,
            zts,
            banner: banner.to_string(),
        })
    }

    /// Human readable flavour, for logs and `stokehold check`.
    pub fn flavour(&self) -> &'static str {
        match self.zts {
            Some(true) => "ZTS",
            Some(false) => "NTS",
            None => "unknown",
        }
    }
}

/// A PHP binary found on disk for a specific purpose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    /// `cgi`, `cli` or `fpm`.
    pub role: &'static str,
    pub binary: PhpBinary,
}

/// Look for PHP binaries in the given directories.
///
/// Directories are searched in order and the first hit per role wins, so pass
/// the most specific directory first. Unreadable or non-PHP files are skipped
/// rather than failing the whole scan.
pub fn discover(dirs: &[PathBuf]) -> Result<Vec<Discovered>> {
    let mut found = Vec::new();
    for (role, names) in [("cgi", CGI_NAMES), ("cli", CLI_NAMES), ("fpm", FPM_NAMES)] {
        'outer: for dir in dirs {
            for name in names {
                let candidate = dir.join(name);
                if !candidate.is_file() {
                    continue;
                }
                if let Ok(binary) = PhpBinary::probe(&candidate) {
                    found.push(Discovered { role, binary });
                    break 'outer;
                }
            }
        }
    }
    Ok(found)
}

/// Standard installation directories to search, most specific first.
///
/// `PATH` comes first because that is how PHP is actually reachable in
/// practice — php.net's Windows installer and every Unix package manager put the
/// binaries there — followed by the conventional manual-install locations.
pub fn default_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Some(path) = std::env::var_os("PATH") {
        for entry in std::env::split_paths(&path) {
            if !entry.as_os_str().is_empty() {
                dirs.push(entry);
            }
        }
    }

    #[cfg(windows)]
    {
        for pattern in [
            "C:/php",
            "C:/php-*",
            "C:/tools/php*",
            "C:/Program Files/php*",
        ] {
            dirs.extend(glob_dirs(pattern));
        }
    }

    #[cfg(not(windows))]
    {
        for dir in [
            "/usr/bin",
            "/usr/local/bin",
            "/opt/homebrew/bin",
            "/usr/sbin",
        ] {
            dirs.push(PathBuf::from(dir));
        }
    }

    dedupe(dirs)
}

/// Remove duplicates while preserving order, so the first hit still wins.
fn dedupe(dirs: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    dirs.into_iter()
        .filter(|dir| seen.insert(dir.clone()))
        .collect()
}

#[cfg(windows)]
fn glob_dirs(pattern: &str) -> Vec<PathBuf> {
    // Minimal wildcard expansion for one trailing `*`: avoids a glob dependency
    // for what amounts to enumerating `C:\php-8.3.31` style directories.
    let Some((prefix, _)) = pattern.split_once('*') else {
        let path = PathBuf::from(pattern);
        return if path.is_dir() {
            vec![path]
        } else {
            Vec::new()
        };
    };

    let (parent, stem) = match prefix.rfind('/') {
        Some(index) => (&prefix[..index], &prefix[index + 1..]),
        None => ("", prefix),
    };

    let Ok(entries) = std::fs::read_dir(rooted(parent).as_ref()) else {
        return Vec::new();
    };
    let stem = stem.to_ascii_lowercase();
    let mut matches: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.to_ascii_lowercase().starts_with(&stem))
        })
        .collect();
    matches.sort();
    matches
}

/// Make a directory specification absolute.
///
/// `C:` is *drive-relative* on Windows: `read_dir("C:")` reads the current
/// working directory, not the drive root. Appending the separator is what turns
/// it into the drive root, which is why this cannot be skipped.
#[cfg(windows)]
fn rooted(parent: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;

    let trimmed = parent.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return Cow::Borrowed(".");
    }
    if trimmed.len() == 2 && trimmed.ends_with(':') {
        let mut owned = trimmed.to_string();
        owned.push('\\');
        return Cow::Owned(owned);
    }
    Cow::Borrowed(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_windows_zts_banner() {
        let banner = "PHP 8.3.31 (cli) (built: May  5 2026 17:23:34) (ZTS Visual C++ 2019 x64)";
        let binary = PhpBinary::parse_banner(Path::new("php.exe"), banner).unwrap();
        assert_eq!(binary.version, "8.3.31");
        assert_eq!(binary.sapi, Sapi::Cli);
        assert_eq!(binary.zts, Some(true));
        assert_eq!(binary.flavour(), "ZTS");
    }

    #[test]
    fn parses_an_fpm_nts_banner() {
        let banner = "PHP 8.3.31 (fpm-fcgi) (built: Jan  1 2026) (NTS)";
        let binary = PhpBinary::parse_banner(Path::new("php-fpm"), banner).unwrap();
        assert_eq!(binary.sapi, Sapi::Fpm);
        assert_eq!(binary.zts, Some(false));
        assert!(binary.sapi.serves_fastcgi());
    }

    #[test]
    fn parses_a_banner_without_a_flavour_marker() {
        let binary =
            PhpBinary::parse_banner(Path::new("php"), "PHP 8.1.2 (cli) (built: Jan 1 2026)")
                .unwrap();
        assert_eq!(binary.zts, None);
        assert_eq!(binary.flavour(), "unknown");
    }

    #[test]
    fn recognises_php_cgi_as_a_fastcgi_sapi() {
        let binary = PhpBinary::parse_banner(
            Path::new("php-cgi"),
            "PHP 8.3.31 (cgi-fcgi) (built: Jan 1 2026) (NTS)",
        )
        .unwrap();
        assert_eq!(binary.sapi, Sapi::Cgi);
        assert!(binary.sapi.serves_fastcgi());
    }

    #[test]
    fn rejects_output_that_is_not_php() {
        let err = PhpBinary::parse_banner(Path::new("notphp"), "not a php banner").unwrap_err();
        assert!(matches!(err, Error::PhpUnrecognizedVersion { .. }));
    }

    #[test]
    fn cli_is_not_a_fastcgi_sapi() {
        assert!(!Sapi::Cli.serves_fastcgi());
        assert!(!Sapi::Other("embed".to_string()).serves_fastcgi());
    }

    #[test]
    fn dedupe_preserves_first_occurrence_order() {
        let dirs = vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/a"),
        ];
        assert_eq!(dedupe(dirs), vec![PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    #[cfg(windows)]
    #[test]
    fn rooted_makes_a_bare_drive_absolute() {
        // `C:` is drive-relative; `C:\` is the drive root.
        assert_eq!(rooted("C:"), "C:\\");
        assert_eq!(rooted("C:/"), "C:\\");
        assert_eq!(rooted("C:\\"), "C:\\");
        assert_eq!(rooted("C:/php"), "C:/php");
        assert_eq!(rooted(""), ".");
    }
}
