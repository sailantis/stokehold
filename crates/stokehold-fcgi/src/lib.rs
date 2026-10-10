//! FastCGI protocol support: the record codec.
//!
//! stokehold speaks FastCGI in both directions — as a client to `php-cgi` and
//! `php-fpm`, and as a server to nginx — so the codec is deliberately symmetric:
//! [`record::read_record`] reads what [`record::write_records`] wrote.
//!
//! Nothing here allocates per record beyond the payload itself, and no record is
//! ever buffered whole: see `docs/plan.md` for why the client relays `STDOUT`
//! verbatim instead of parsing it.

pub mod client;
pub mod error;
pub mod message;
pub mod nvpair;
pub mod protocol;
pub mod record;
pub mod transport;

pub use client::{
    parse_response_headers, Client, Request, Response, MANAGEMENT_REQUEST_ID, REQUEST_ID,
};
pub use error::{FcgiError, Result};
pub use message::{BeginRequest, EndRequest};
pub use nvpair::{decode_pairs, encode_pair, encode_pairs, NameValuePairs};
pub use protocol::{
    flags, ProtocolStatus, RecordType, Role, HEADER_LEN, MAX_CONTENT_LEN, VERSION_1,
};
pub use record::{padding_for, read_record, write_records, Header, Record};
pub use transport::{Endpoint, Stream};
