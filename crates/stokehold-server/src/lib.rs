//! Backend supervision and dispatch.
//!
//! One [`Backend`] per configured backend, one [`Dispatcher`] in front of them,
//! so both front-ends have a single call to make. See `docs/plan.md` §3.

pub mod backend;
pub mod dispatch;
pub mod error;
pub mod lifecycle;
pub mod pool;

pub use backend::Backend;
pub use dispatch::Dispatcher;
pub use error::{Result, ServerError};
pub use lifecycle::LifecycleGuard;
pub use pool::CgiPool;
