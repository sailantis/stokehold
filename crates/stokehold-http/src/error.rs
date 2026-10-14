//! Errors from the HTTP front-end.

/// A failure that happens before a response can be produced.
///
/// The variants exist so the listener can choose a status code without inspecting
/// strings. Everything that reaches the client as an error page is decided by
/// [`HttpError::status`].
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// The request could not be read or understood.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// The request body was larger than the configured limit.
    ///
    /// Separate from [`HttpError::BadRequest`] because the status differs: this is
    /// a 413, not a 400, and a client that sent a large upload should be told
    /// exactly that.
    #[error("the request body exceeds the {limit} byte limit")]
    BodyTooLarge { limit: usize },

    /// No script matched the request, and no front controller is configured.
    ///
    /// The CGI layer produces this as a "bad request", but for HTTP a request for
    /// a path that names no script is a 404 — the client asked for something that
    /// does not exist, not for something malformed.
    #[error("not found: {0}")]
    NotFound(String),

    /// The path names something the configuration forbids.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// The backend could not be reached.
    #[error("bad gateway: {0}")]
    BadGateway(String),

    /// The backend was reached but did not answer in time.
    #[error("gateway timeout: {0}")]
    GatewayTimeout(String),

    /// The server is at its connection limit.
    #[error("too many connections")]
    Overloaded,

    /// Binding the listener failed.
    #[error("cannot bind {address}: {source}")]
    Bind {
        address: String,
        #[source]
        source: std::io::Error,
    },

    /// The `tokio` runtime could not be created.
    #[error("cannot start the async runtime: {0}")]
    Runtime(#[source] std::io::Error),

    /// A certificate or key could not be loaded, or they do not pair.
    ///
    /// Reported before the port is bound, because a listener that starts and then fails every
    /// handshake is worse than one that refuses to start: the failure belongs to the operator
    /// reading the log, not to every client that connects.
    #[error("TLS is configured but unusable: {reason}")]
    Tls { reason: String },
}

impl HttpError {
    /// The status code that should be sent for this failure.
    pub fn status(&self) -> u16 {
        match self {
            HttpError::BadRequest(_) => 400,
            HttpError::Forbidden(_) => 403,
            HttpError::NotFound(_) => 404,
            HttpError::BodyTooLarge { .. } => 413,
            HttpError::BadGateway(_) => 502,
            HttpError::Overloaded => 503,
            HttpError::GatewayTimeout(_) => 504,
            // Neither of these reaches a client: they stop the listener before it
            // accepts anything. Reported as a 500 rather than panicking.
            HttpError::Bind { .. } | HttpError::Runtime(_) | HttpError::Tls { .. } => 500,
        }
    }

    /// The reason phrase for this failure's status.
    pub fn reason(&self) -> &'static str {
        match self.status() {
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            413 => "Content Too Large",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            504 => "Gateway Timeout",
            _ => "Internal Server Error",
        }
    }
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, HttpError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_failure_has_the_status_a_client_needs() {
        assert_eq!(HttpError::BadRequest("x".into()).status(), 400);
        assert_eq!(HttpError::Forbidden("x".into()).status(), 403);
        assert_eq!(HttpError::NotFound("x".into()).status(), 404);
        assert_eq!(HttpError::BodyTooLarge { limit: 1 }.status(), 413);
        assert_eq!(HttpError::BadGateway("x".into()).status(), 502);
        assert_eq!(HttpError::Overloaded.status(), 503);
        assert_eq!(HttpError::GatewayTimeout("x".into()).status(), 504);
    }

    #[test]
    fn a_missing_script_is_a_404_not_a_500() {
        // The distinction the front-end exists to make: the CGI layer calls this
        // a bad request, because there it is a protocol-level refusal. To an HTTP
        // client it is simply a path that does not exist.
        let missing = HttpError::NotFound("/nope".to_string());
        assert_eq!(missing.status(), 404);
        assert_eq!(missing.reason(), "Not Found");
        assert!(missing.to_string().contains("/nope"), "{missing}");
    }
}
