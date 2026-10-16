//! What can go wrong on the way to an origin.
//!
//! The distinction the front-end needs is the same one [`volucer_fcgi::FcgiError`] makes, and it
//! exists for the same reason: a gateway has to choose between a 502 and a 504, and only the layer
//! that talked to the backend knows which failure a deadline produces. [`UpstreamError::is_timeout`]
//! is that knowledge, kept here rather than repeated in a front-end.
//!
//! [`volucer_fcgi::FcgiError`]: https://docs.rs/volucer-fcgi

/// A failure on the request's way to or from an origin.
#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    /// The configured origin is not usable. Reported at startup, not per request.
    #[error("{origin:?} is not a usable origin: {reason}")]
    InvalidOrigin { origin: String, reason: String },

    /// A CA certificate named for an `https:` origin could not be read.
    #[error("{path}: cannot read the CA certificate: {reason}")]
    Ca { path: String, reason: String },

    /// Nothing answered, or the connection failed. This is the 502.
    #[error("the origin is unreachable: {0}")]
    Unreachable(String),

    #[error("the request to the origin could not be built: {0}")]
    BadRequest(String),

    #[error("the origin's response is unusable: {0}")]
    BadResponse(String),

    /// The origin was reached but did not answer in time. This is the 504.
    #[error("the origin did not answer in time")]
    Timeout,

    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl UpstreamError {
    /// Whether the origin was reached but did not answer in time.
    ///
    /// The distinction between a 502 and a 504, decided where it is known. A TLS handshake that
    /// times out is a `TimedOut` i/o error and is deliberately *not* a timeout here: a handshake
    /// that never completed means the origin is not serving, which is what a 502 says.
    pub fn is_timeout(&self) -> bool {
        matches!(self, UpstreamError::Timeout)
    }
}

impl From<hyper_util::client::legacy::Error> for UpstreamError {
    fn from(source: hyper_util::client::legacy::Error) -> Self {
        // The client's own error type carries the cause as its source; `to_string` renders both,
        // which is what belongs in a log line and in the 502 the operator reads.
        UpstreamError::Unreachable(source.to_string())
    }
}

impl From<hyper::Error> for UpstreamError {
    fn from(source: hyper::Error) -> Self {
        UpstreamError::Unreachable(source.to_string())
    }
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, UpstreamError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_deadline_is_a_timeout() {
        // A 504 is a specific claim about the origin: it was reached and stayed silent. A
        // handshake that timed out means the origin is not serving at all, which is a 502.
        assert!(UpstreamError::Timeout.is_timeout());
        assert!(!UpstreamError::Unreachable("connection refused".into()).is_timeout());
        assert!(!UpstreamError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "handshake"
        ))
        .is_timeout());
    }

    #[test]
    fn messages_name_what_to_look_at() {
        let err = UpstreamError::InvalidOrigin {
            origin: "https://1.2.3.4".into(),
            reason: "the CA could not be read".into(),
        };
        assert!(err.to_string().contains("https://1.2.3.4"), "{err}");
        assert!(err.to_string().contains("CA"), "{err}");
    }
}
