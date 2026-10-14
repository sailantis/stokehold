//! What a worker process is started from: the resolved program, its arguments, its
//! environment, and the checks that make a typo a startup error by name rather than a
//! confusing `CreateProcess` / `execvp` failure later. **No process is created here** â€”
//! this module only decides *what* to run.

use crate::error::{Result, ServerError};
use std::collections::BTreeMap;
use std::path::PathBuf;
use stokehold_core::config::{ChecksumMode, WorkerConfig, WorkerTransport};
use stokehold_core::program::{resolve_program, split_command};
use stokehold_worker::Checksum;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
