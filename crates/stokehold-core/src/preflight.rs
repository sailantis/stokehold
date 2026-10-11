//! Checks that need the filesystem, not just the configuration file.
//!
//! Everything here is a *warning*, never an error. `stokehold check` is a
//! diagnostic: a document root that does not exist yet is a normal thing to
//! check before a deploy creates it, and a front controller below the root is a
//! normal thing to serve from a directory whose contents are built later. What
//! these checks buy is that a mistake is reported once, here, instead of as a
//! puzzling 404 at the first request.

use std::path::Path;

use crate::config::Config;

/// One thing that is probably not what the operator meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// What is wrong, in a sentence an operator can act on.
    pub message: String,

    /// How to fix it, when there is a specific answer.
    pub hint: Option<String>,
}

impl Warning {
    fn new(message: impl Into<String>) -> Self {
        Warning {
            message: message.into(),
            hint: None,
        }
    }

    fn with_hint(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Warning {
            message: message.into(),
            hint: Some(hint.into()),
        }
    }
}

/// Inspect the host and report anything that will not work as written.
///
/// `document_root` is the document root that will actually be used, after the
/// command line has had its say — so `check` can report the same decision `serve`
/// would make rather than the one the file happens to contain.
pub fn inspect(config: &Config, document_root: &Path) -> Vec<Warning> {
    let mut warnings = Vec::new();

    if !document_root.exists() {
        warnings.push(Warning::with_hint(
            format!("document root {} does not exist", document_root.display()),
            "create it, or point server.document_root somewhere else",
        ));
    } else if !document_root.is_dir() {
        warnings.push(Warning::with_hint(
            format!(
                "document root {} is not a directory",
                document_root.display()
            ),
            "point server.document_root at the directory that holds the application",
        ));
    }

    // A front controller that is not there is the classic silent failure: every
    // unmatched path resolves to a script PHP will refuse to execute, and the
    // client sees a 404 that says nothing about the cause.
    if document_root.is_dir() {
        if let Some(front) = config.server.resolve_front_controller(None) {
            let script = document_root.join(&front);
            if !script.is_file() {
                warnings.push(Warning::with_hint(
                    format!(
                        "front controller {front} was not found under {}",
                        document_root.display()
                    ),
                    // `front_controller = \"\"` cannot be written in the file —
                    // TOML cannot tell an empty string from an absent key, so the
                    // meaning is reserved for the flag. The hint has to say that,
                    // or it recommends something the validator rejects.
                    "check the path, or serve with `--front-controller ''` to mean \"only real \
                     .php files\" — every unmatched request will 404 meanwhile"
                        .to_string(),
                ));
            }
        }
    }

    if config.server.static_files.enabled && !document_root.is_dir() {
        warnings.push(Warning::new(
            "server.static_files is enabled, but nothing can be served while the document root \
             is unusable",
        ));
    }

    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        let text = r#"
default_backend = "app"
[server]
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "php worker.php"
"#;
        Config::from_toml(text).expect("config parses")
    }

    /// A directory that is removed when the test ends.
    struct Fixture(std::path::PathBuf);

    impl Fixture {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("stokehold-preflight-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create the fixture");
            Fixture(path)
        }

        fn write(&self, relative: &str) {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create the parent");
            }
            std::fs::write(path, "<?php\n").expect("write the fixture file");
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_directory_with_its_front_controller_is_clean() {
        let root = Fixture::new("clean");
        root.write("index.php");
        assert_eq!(inspect(&config(), root.path()), Vec::new());
    }

    #[test]
    fn a_missing_document_root_is_reported() {
        let root = Fixture::new("missing");
        let gone = root.path().join("not-created-yet");
        let warnings = inspect(&config(), &gone);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].message.contains("does not exist"),
            "{warnings:?}"
        );
        assert!(warnings[0].hint.is_some());
    }

    #[test]
    fn a_missing_front_controller_is_reported_with_the_escape_hatch() {
        let root = Fixture::new("no-front");
        let warnings = inspect(&config(), root.path());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].message.contains("front controller"),
            "{warnings:?}"
        );
        let hint = warnings[0].hint.as_deref().unwrap_or_default();
        assert!(hint.contains("--front-controller"), "{hint}");
    }

    /// A hint that recommends something the validator rejects is worse than no
    /// hint, so the two are checked against each other rather than by eye. This
    /// is not hypothetical: the first version of this hint said to write
    /// `server.front_controller = ""`, which `Config::validate` refuses.
    #[test]
    fn no_hint_tells_the_operator_to_write_something_validation_refuses() {
        let root = Fixture::new("hints");
        for path in [root.path().to_path_buf(), root.path().join("gone")] {
            for warning in inspect(&config(), &path) {
                let Some(hint) = &warning.hint else { continue };
                assert!(
                    !hint.contains("front_controller = \"\""),
                    "the hint recommends a value the validator rejects: {hint}"
                );
            }
        }
    }

    #[test]
    fn an_empty_front_controller_is_not_checked_for() {
        // "Only real .php files" has nothing to look for, so a directory holding
        // no index.php is perfectly correct.
        let root = Fixture::new("files-only");
        let mut config = config();
        config.server.front_controller = Some(String::new());
        assert_eq!(inspect(&config, root.path()), Vec::new());
    }

    #[test]
    fn a_document_root_that_is_a_file_is_reported() {
        let root = Fixture::new("is-a-file");
        root.write("index.php");
        let warnings = inspect(&config(), &root.path().join("index.php"));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].message.contains("not a directory"),
            "{warnings:?}"
        );
    }
}
