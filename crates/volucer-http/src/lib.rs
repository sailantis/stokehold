//! The HTTP front-end: a `hyper` listener in front of the same dispatcher the
//! FastCGI front-end uses.
//!
//! # Why this is a separate crate
//!
//! This is the **only** place in volucer where async code runs, and it is
//! separated on purpose. `hyper` needs a `tokio` runtime, while every backend is
//! blocking by construction: a worker slot is one process on a pipe and serves
//! one request at a time, and `php-cgi`/`php-fpm` process requests sequentially.
//! So the runtime is confined here, and the boundary between the two worlds is a
//! single call — `tokio::task::spawn_blocking` — rather than a property of the
//! whole program.
//!
//! The rule to keep is one line long: **never call a blocking backend from an
//! async context except through `spawn_blocking`**. Reaching the dispatcher
//! directly would stall the runtime's worker thread, and enough of those stalls
//! would starve every other connection.
//!
//! The static layer is the one part of this crate that does its filesystem work
//! on the runtime, through `tokio::fs`, which is why it is not behind
//! `spawn_blocking` — and why a `std::fs::File` is opened off the runtime and
//! handed to `tokio::fs::File`. See [`listener::open_file`].
//!
//! # What is deliberately not here
//!
//! TLS, HTTP/2 over TLS (ALPN) and HTTP/3. `hyper` handles HTTP/1.1 and,
//! over plaintext, HTTP/2 negotiated by the **prior-knowledge** path — a client
//! that opens with the connection preface. The **`Upgrade: h2c`** path does
//! *not* work here, and that is a property of this crate rather than of
//! `hyper`: upgrade-capable serving needs
//! `hyper_util::server::conn::auto::Builder::serve_connection_with_upgrades`,
//! and [`listener`] calls plain `serve_connection`, so a client that offers
//! `Upgrade: h2c` is answered in HTTP/1.1 with no `101`. Verified by hand;
//! the crate doc used to claim both paths. Both halves are now pinned by tests rather than by
//! this paragraph — `alpn_h2_negotiates_a_real_http_2_connection`, which asserts a SETTINGS
//! frame actually arrives, and `an_upgrade_h2c_offer_is_answered_in_http_1_1`, which asserts
//! the offer is *not* taken — so the gap is a decision rather than an accident. HTTP/3 is
//! `quinn` plus `h3`, not `hyper`.

pub mod error;
pub mod listener;
pub mod static_files;
pub mod tls;

pub use error::HttpError;
pub use listener::{
    to_cgi, to_cgi_routed, HttpLimits, HttpListener, HttpShutdown, HttpStats, ListenerSettings,
    ReloadHandle, ReloadHandles, Reloaded, Resolved, RunningHttp, TlsSettings,
};
pub use tls::{
    CertificateSet, Certs, ServerCertificate, SiteCertificate, SiteCertificatePair, SniResolver,
    TlsError,
};
pub use volucer_watch::{Registry, SharedState, Swappable, DEFAULT_SITE_KEY};
