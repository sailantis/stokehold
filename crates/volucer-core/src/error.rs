//! Error and result types shared across volucer crates.

/// Errors produced while loading configuration or inspecting the host.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("failed to read {path}: {source}")]
    Read {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse configuration: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("PHP binary {path} produced no version output")]
    PhpUnrecognizedVersion { path: std::path::PathBuf },

    #[error("no usable PHP binary found (looked for: {0})")]
    PhpNotFound(String),
}

pub type Result<T> = std::result::Result<T, Error>;
