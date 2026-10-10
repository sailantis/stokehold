//! FastCGI protocol support: record codec and client.
//!
//! Nothing is implemented yet beyond the crate's public surface; the codec
//! lands next. See `docs/plan.md` for the phase order.

use stokehold_core::error::{Error, Result};

/// Placeholder until the record codec lands.
pub fn not_implemented_yet(what: &str) -> Result<()> {
    Err(Error::Config(format!("{what} is not implemented yet")))
}
