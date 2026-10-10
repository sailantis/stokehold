//! Front-ends: listeners that accept a client-facing protocol and hand the
//! resulting request to the dispatcher.
//!
//! Both front-ends produce the same thing — a `stokehold_fcgi::Request` — so
//! there is one dispatch path and one place where backends are chosen.

pub mod fastcgi;

pub use fastcgi::{
    read_management_request, read_request, split_response, write_error, write_get_values,
    write_response, ConnectionOutcome, IncomingRequest, ServerErrorKind, Timing,
};
