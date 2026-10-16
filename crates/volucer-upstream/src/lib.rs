//! The HTTP backend: volucer as a reverse proxy in front of an origin that speaks HTTP itself.
//!
//! # Why this is a crate of its own
//!
//! It is the only part of volucer that needs `hyper`'s **client** stack. The listener needs the
//! server half, and the two halves share nothing — `hyper-util`'s `client-legacy` is the sole user
//! of `socket2`, `httparse` and `libc`, and its `Client` is a pooled, upgradeable connection
//! manager that would be a strange thing to link into the FastCGI-only binary by accident.
//!
//! It is named by *role*, not by protocol: if HTTP/2 origins never happen, "talks to an HTTP
//! origin" is still what this is.
//!
//! # Why there is a second request type
//!
//! `volucer_fcgi::Request` is CGI-shaped on purpose — `params`, a `Vec<u8>` body, a keep-alive flag
//! — and that shape is what loses header case, duplicate headers and the request line. It is the
//! right loss for a CGI process and the wrong one for an origin, which is an HTTP peer and expects
//! HTTP. So an upstream is reached through [`UpstreamRequest`] instead, and the *front-end* decides
//! which of the two a backend needs. Nothing about the CGI path changes.
//!
//! # Why the connector is written here
//!
//! [`hyper_util::client::legacy::connect::http::HttpConnector`] is hardcoded to dial TCP and to
//! refuse any scheme that is not `http` (`enforce_http` defaults on), so it cannot reach a `unix:`
//! origin and cannot complete a TLS handshake. The pool, the HTTP/1.1 and HTTP/2 state machines and
//! the upgrade machinery are all still `hyper-util`'s — a connector is only asked "give me a
//! connection for this URI", which makes it the one seam where two extra transports fit without a
//! second copy of any of the rest.

pub mod client;
pub mod error;
pub mod origin;
pub mod translate;

pub use client::{UpstreamBody, UpstreamClient, UpstreamRequest, UpstreamResponse};
pub use error::{Result, UpstreamError};
pub use origin::{Origin, UpstreamTls};
pub use translate::{prepare_request, translate_response};

/// The configuration model's checked origin, re-exported.
///
/// So a caller that has an `Origin` can name the type it was built from without depending on
/// `volucer-core` as well, and so the two halves of "what an origin may be" travel together.
pub use volucer_core::config::OriginSpec;
