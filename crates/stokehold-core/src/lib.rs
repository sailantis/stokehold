//! Shared building blocks for stokehold: configuration, backend model and PHP discovery.

pub mod config;
pub mod error;
pub mod php;
pub mod preflight;

pub use config::{BackendConfig, Config, Listener, ServerConfig};
pub use error::{Error, Result};
pub use php::{PhpBinary, Sapi};
pub use preflight::{inspect, Warning};
