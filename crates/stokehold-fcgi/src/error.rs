//! Errors produced by the FastCGI codec and client.

/// A protocol-level failure. Every variant is recoverable from the caller's
/// point of view: a malformed record means the connection is unusable, not that
/// the process should abort.
#[derive(Debug, thiserror::Error)]
pub enum FcgiError {
    #[error("unsupported FastCGI version {0} (only version 1 exists)")]
    UnsupportedVersion(u8),

    #[error("unknown FastCGI record type {0}")]
    UnknownRecordType(u8),

    #[error("unknown FastCGI role {0}")]
    UnknownRole(u16),

    #[error("unknown FastCGI protocol status {0}")]
    UnknownProtocolStatus(u8),

    #[error("name-value pair at offset {offset} is truncated")]
    TruncatedNameValue { offset: usize },

    #[error("expected a {expected:?} record but received {actual:?}")]
    UnexpectedRecordType {
        expected: crate::protocol::RecordType,
        actual: crate::protocol::RecordType,
    },

    #[error("response for request {request_id} is missing the {} record", if *.complete { "END_REQUEST" } else { "STDOUT" })]
    IncompleteResponse { request_id: u16, complete: bool },

    #[error("backend ended the request with protocol status {status:?}")]
    BackendProtocolStatus {
        status: crate::protocol::ProtocolStatus,
    },

    #[error("backend reported application status {status}")]
    BackendAppStatus { status: u32 },

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience alias for codec and client results.
pub type Result<T> = std::result::Result<T, FcgiError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_describe_the_failure() {
        let err = FcgiError::UnsupportedVersion(2);
        assert!(err.to_string().contains("version 2"), "{err}");

        let err = FcgiError::TruncatedNameValue { offset: 12 };
        assert!(err.to_string().contains("12"), "{err}");

        let err = FcgiError::BackendProtocolStatus {
            status: crate::protocol::ProtocolStatus::Overloaded,
        };
        assert!(err.to_string().contains("Overloaded"), "{err}");
    }
}
