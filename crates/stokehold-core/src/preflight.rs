//! Checks that need the filesystem, not just the configuration file.
//!
//! Everything here is a *warning*, never an error. `stokehold check` is a
//! diagnostic: a document root that does not exist yet is a normal thing to
//! check before a deploy creates it, and a front controller below the root is a
//! normal thing to serve from a directory whose contents are built later. What
//! these checks buy is that a mistake is reported once, here, instead of as a
//! puzzling 404 at the first request.

use std::path::Path;

use crate::config::{BackendConfig, Config};
use crate::program::{resolve_program, split_command};

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

    if config.server.statics().enabled && !document_root.is_dir() {
        warnings.push(Warning::new(
            "server.static_files is enabled, but nothing can be served while the document root \
             is unusable",
        ));
    }

    // Each site's own root and front controller, because those are what a request to that
    // site actually uses. A site whose root is missing is not visible in the default site's
    // checks at all, and it answers 404 or 500 for every request it receives.
    for (name, site) in &config.sites {
        let root = &site.document_root;
        if !root.is_dir() {
            warnings.push(Warning::with_hint(
                format!(
                    "sites.{name}: document root {} does not exist",
                    root.display()
                ),
                "create it, or point this site's document_root somewhere else — every request \
                 to this site's hosts will fail meanwhile",
            ));
            continue;
        }
        // The site's own controller, or the one it inherits. A site that says
        // `front_controller = false` has none on purpose and is not warned about.
        if let Some(controller) = &site.front_controller {
            if let Some(front) = controller.as_path() {
                if !root.join(front).is_file() {
                    warnings.push(Warning::with_hint(
                        format!(
                            "sites.{name}: front controller {front} was not found under {}",
                            root.display()
                        ),
                        "check the path, or write `front_controller = false` to serve only real \
                         files",
                    ));
                }
            }
        }
    }

    // A listener with no backends is a static-only deployment, which works — but
    // only for paths that name a file. A path that names nothing has nowhere to go
    // and answers 500, so it is worth saying once here rather than leaving it to be
    // discovered as an "internal error" at the first request.
    if config.backends.is_empty() {
        warnings.push(Warning::with_hint(
            "no backends are configured: only paths naming a real file can be answered",
            "with a front controller set, every other path is a 500 — either add a backend, or \
             serve with `--front-controller ''` if this really is files only",
        ));
    }

    inspect_programs(config, &mut warnings);

    warnings
}

/// Report every configured program that cannot be started.
///
/// This is the check `check` was missing: a mistyped `binary` is the mistake a
/// configuration file is *most* likely to make, and it is the one thing in the
/// file that is a filesystem path rather than a value with a default. Without
/// this, `stokehold check` answered "configuration OK" for a configuration that
/// could not start a single backend, because `Config::validate` is about the
/// shape of the file and the pool's own existence check only ran inside `serve`.
///
/// A warning rather than an error, for the same reason the document-root checks
/// are: an image that installs PHP in a later layer is a normal thing to check,
/// and refusing to report anything else because one binary is not there yet
/// would be the wrong trade. The value is that the mistake is reported once,
/// here, instead of as a startup failure the operator has to go looking for.
fn inspect_programs(config: &Config, warnings: &mut Vec<Warning>) {
    for (name, backend) in &config.backends {
        match backend {
            BackendConfig::Worker(worker) => {
                let (program, _) = match split_command(&worker.command) {
                    Ok(parts) => parts,
                    // An empty or unbalanced command is a hard error in
                    // `Config::validate`, so this is unreachable in practice;
                    // skipping keeps the two checks from disagreeing about
                    // who reports what.
                    Err(_) => continue,
                };
                let binary = resolve_program(&program);
                if !binary.is_file() {
                    warnings.push(Warning::with_hint(
                        format!(
                            "backends.{name}: the worker program {} does not exist (from command \
                             {:?})",
                            binary.display(),
                            worker.command
                        ),
                        "check the path, or the `command` of this backend — the worker is started \
                         with `command`, not with `binary`, so the two are independent settings",
                    ));
                }
            }
            BackendConfig::SpawnCgi(cgi) => {
                if !cgi.binary.is_file() {
                    warnings.push(Warning::with_hint(
                        format!("backends.{name}: {} does not exist", cgi.binary.display()),
                        "point `binary` at the php-cgi executable for this host — \
                         `stokehold check` lists every PHP binary it found above",
                    ));
                }
            }
            BackendConfig::External(external) => {
                // Nothing to look for on disk: the socket belongs to another
                // process, and whether it is there yet is not a configuration
                // mistake. A `unix:` path can be checked cheaply, though, and a
                // missing one is the common cause of a first-request 502.
                if let Some(path) = external.socket.strip_prefix("unix:") {
                    if !Path::new(path).exists() {
                        warnings.push(Warning::with_hint(
                            format!(
                                "backends.{name}: nothing is listening on {} yet",
                                external.socket
                            ),
                            "start php-fpm before serving, or ignore this if it starts first",
                        ));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        // The worker command names a program that certainly exists, so these
        // tests stay about the document root. `inspect` now also reports a
        // configured program that cannot be started, so a fixture naming `php`
        // would make every test in this module depend on whether PHP happens to
        // be installed on the host.
        let exe = std::env::current_exe().expect("the test binary path");
        let text = format!(
            r#"
default_backend = "app"
[server]
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "{}"
"#,
            // Backslashes are escaped because this is a TOML basic string, which
            // is the same papercut a hand-written Windows path hits.
            exe.display().to_string().replace('\\', "\\\\")
        );
        Config::from_toml(&text).expect("config parses")
    }

    /// A configuration with one backend whose program cannot be started.
    fn config_with_worker_command(command: &str) -> Config {
        let text = format!(
            r#"
default_backend = "app"
[server]
http = "127.0.0.1:8080"
[backends.app]
kind = "worker"
command = "{command}"
"#
        );
        Config::from_toml(&text).expect("config parses")
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

    /// The check `check` was missing: a `binary` that is not there.
    ///
    /// This is the mistake a configuration file is most likely to make, because
    /// `binary` is the one setting that is a filesystem path rather than a value
    /// with a default — and it is the one `Config::validate` cannot see.
    #[test]
    fn a_php_cgi_binary_that_does_not_exist_is_reported() {
        let root = Fixture::new("bad-binary");
        root.write("index.php");
        let text = r#"
default_backend = "cgi"
[server]
http = "127.0.0.1:8080"
[backends.cgi]
kind = "spawn_cgi"
binary = "C:/definitely/not/php-cgi.exe"
"#;
        let config = Config::from_toml(text).expect("config parses");
        // Validation is about the shape of the file, so the missing file is not
        // its business — which is exactly why `inspect` has to catch it.
        config.validate().expect("the shape of the file is valid");

        let warnings = inspect(&config, root.path());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].message.contains("does not exist"),
            "{warnings:?}"
        );
    }

    /// A worker's program comes from `command`, not from a `binary` key, so the
    /// check has to read the command rather than look for a field that is not
    /// there.
    #[test]
    fn a_worker_command_naming_a_missing_program_is_reported() {
        let root = Fixture::new("bad-command");
        root.write("index.php");
        let config = config_with_worker_command("definitely-not-php worker.php");
        let warnings = inspect(&config, root.path());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].message.contains("does not exist"),
            "{warnings:?}"
        );
        assert!(
            warnings[0].message.contains("command"),
            "the message must name `command`, since that is the setting at fault: {warnings:?}"
        );
    }

    /// The `spawn_fpm` key is gone, and a file that still carries it must be rejected
    /// by name rather than parsed into something else.
    ///
    /// It used to be accepted and then refused at startup, with `check` warning that it
    /// would not be started — a key that looked supported and could not work. `deny_unknown_fields`
    /// on the backend table is what turns that into an error at the point the file is
    /// read, which is where a removed setting should be noticed.
    #[test]
    fn the_removed_spawn_fpm_backend_is_rejected_by_name() {
        let text = r#"
default_backend = "fpm"
[server]
http = "127.0.0.1:8080"
[backends.fpm]
kind = "spawn_fpm"
binary = "/usr/sbin/php-fpm"
"#;
        let err = Config::from_toml(text).expect_err("the key must be rejected");
        assert!(
            err.to_string().contains("spawn_fpm"),
            "the message must name the key that was removed: {err}"
        );
    }

    /// No backends is a static-only deployment: valid, warned about, and the
    /// warning has to say what breaks (a path that names no file).
    #[test]
    fn a_static_only_configuration_is_valid_and_warned_about() {
        let root = Fixture::new("static-only");
        // A front controller that *is* present, so the warning under test is the
        // only one: this fixture deliberately has nothing to dispatch to while
        // still naming a script, which is the shape that answers 500.
        root.write("index.php");
        // No front controller key, so the default (`index.php`) is in effect and
        // a path naming no file would answer 500 — which is what the warning
        // exists to say before the first request does.
        let config = Config::from_toml(
            r#"
[server]
http = "127.0.0.1:8080"
"#,
        )
        .expect("config parses");
        config.validate().expect("no backends is valid");

        let warnings = inspect(&config, root.path());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].message.contains("no backends"), "{warnings:?}");
        assert!(
            warnings[0]
                .hint
                .as_deref()
                .unwrap_or_default()
                .contains("front-controller"),
            "the hint must offer the files-only escape: {warnings:?}"
        );
    }

    #[test]
    fn a_missing_document_root_is_reported() {
        let root = Fixture::new("missing");
        let gone = root.path().join("not-created-yet");
        let warnings = inspect(&config(), &gone);
        // Two: the root is gone, and the static layer — on by default — has nothing to
        // serve from. Both are true and both name the same cause, which is the point.
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings[0].message.contains("does not exist"),
            "{warnings:?}"
        );
        assert!(warnings[0].hint.is_some());
        assert!(
            warnings[1].message.contains("static_files is enabled"),
            "the static layer's own consequence must be stated: {warnings:?}"
        );
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
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings[0].message.contains("not a directory"),
            "{warnings:?}"
        );
        assert!(
            warnings[1].message.contains("static_files is enabled"),
            "{warnings:?}"
        );
    }
}
