//! Shared building blocks for stokehold: configuration, backend model and PHP discovery.

pub mod config;
pub mod error;
pub mod php;

pub use config::{BackendConfig, Config, Listener};
pub use error::{Error, Result};
pub use php::{PhpBinary, Sapi};
