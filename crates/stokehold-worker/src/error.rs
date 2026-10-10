//! Errors from the worker protocol.
//!
//! The distinction that matters is between a *framing* failure (the stream is
//! unusable, close it) and a *message* failure (one request was bad, the worker
//! is fine). [`WorkerError::is_fatal`] draws that line so the pool can decide
//! whether to recycle a worker or keep using it.

use crate::header::MessageType;

/// A worker protocol failure.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("unsupported protocol version {found} (this build speaks {expected})")]
    UnsupportedVersion { found: u8, expected: u8 },

    #[error("unknown message type {0}")]
    UnknownMessageType(u8),

    #[error("message type {actual:?} is not a valid {expected}")]
    NotA {
        expected: &'static str,
        actual: MessageType,
    },

    #[error("frame at offset {offset} is truncated: header claims {needed} bytes")]
    TruncatedFrame { offset: usize, needed: usize },

    #[error("the stream ended in the middle of a frame header")]
    TruncatedHeader,

    #[error("payload checksum mismatch on request {request_id}: expected {expected:#010x}, computed {computed:#010x}")]
    ChecksumMismatch {
        request_id: u32,
        expected: u32,
        computed: u32,
    },

    #[error("payload is not valid UTF-8: {0}")]
    NotUtf8(#[from] std::str::Utf8Error),

    #[error("payload is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("payload is {size} bytes, more than the {max} byte limit")]
    PayloadTooLarge { size: usize, max: usize },

    #[error("the worker answered request {expected} but we asked for {actual}")]
    WrongRequestId { expected: u32, actual: u32 },

    #[error("the worker reported an error: {message}")]
    Remote { message: String },

    #[error("the worker did not answer within {0:?}")]
    Timeout(std::time::Duration),

    #[error("the worker closed its output: {0}")]
    Closed(String),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

impl WorkerError {
    /// Whether the failure makes the stream unusable.
    ///
    /// Two different things justify this, and it is worth being precise about
    /// which, because a reader will otherwise assume the wrong one:
    ///
    /// - **Framing has desynchronised.** A checksum mismatch means we cannot know
    ///   where the next header begins, and every subsequent read is guesswork.
    ///   This is the strong case.
    /// - **The peer contradicted itself, or is gone.** A payload that is not
    ///   valid UTF-8 or not valid JSON was *fully consumed* — our position in the
    ///   stream is fine — but the worker's own frame is unusable, and an SDK
    ///   whose payloads do not decode is not one to keep serving from. An I/O
    ///   error, a closed pipe or a timeout means there is no peer left to serve
    ///   from at all.
    ///
    /// What is deliberately *not* fatal: a wrong request id, and a remote error
    /// the worker labelled recoverable. Both leave the stream aligned and the
    /// worker otherwise healthy, so discarding it would trade a good worker for a
    /// bad request.
    ///
    /// The two cases above are collapsed into one predicate because they have one
    /// consequence — the pool must replace the worker — and because a caller that
    /// needed to tell them apart would have to ask why, which the error already
    /// says.
    pub fn is_fatal(&self) -> bool {
        match self {
            WorkerError::UnsupportedVersion { .. }
            | WorkerError::UnknownMessageType(_)
            | WorkerError::NotA { .. }
            | WorkerError::TruncatedFrame { .. }
            | WorkerError::TruncatedHeader
            | WorkerError::ChecksumMismatch { .. }
            | WorkerError::NotUtf8(_)
            | WorkerError::Json(_)
            | WorkerError::PayloadTooLarge { .. }
            | WorkerError::Io(_)
            | WorkerError::Closed(_)
            | WorkerError::Timeout(_) => true,
            WorkerError::WrongRequestId { .. } | WorkerError::Remote { .. } => false,
        }
    }

    /// Whether the failure means "this worker is gone" rather than "this request
    /// was bad", which is what the pool retries or replaces on.
    pub fn is_transport(&self) -> bool {
        match self {
            WorkerError::Io(io) => matches!(
                io.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::WouldBlock
            ),
            WorkerError::Closed(_) | WorkerError::TruncatedHeader => true,
            _ => false,
        }
    }
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, WorkerError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_checksum_mismatch_desynchronises_the_stream() {
        let err = WorkerError::ChecksumMismatch {
            request_id: 3,
            expected: 0xdead_beef,
            computed: 0x0000_0001,
        };
        assert!(err.is_fatal());
        assert!(!err.is_transport());
        assert!(err.to_string().contains("request 3"), "{err}");
    }

    #[test]
    fn a_remote_error_keeps_the_stream_usable() {
        let err = WorkerError::Remote {
            message: "boom".to_string(),
        };
        assert!(!err.is_fatal());
        assert!(err.to_string().contains("boom"), "{err}");
    }

    #[test]
    fn a_wrong_request_id_is_not_fatal() {
        // The one case where the worker is healthy and only the request was
        // wrong, so replacing the worker would trade a good one for a bad id.
        let err = WorkerError::WrongRequestId {
            expected: 7,
            actual: 8,
        };
        assert!(!err.is_fatal());
        assert!(!err.is_transport());
    }

    #[test]
    fn an_unreadable_payload_is_fatal_for_the_worker_not_the_framing() {
        // The frame was consumed whole, so our position is intact — but an SDK
        // whose payloads do not decode is not one to keep serving from.
        let from_json = WorkerError::Json(serde_json::from_str::<u32>("nope").unwrap_err());
        assert!(from_json.is_fatal());
        assert!(!from_json.is_transport());

        // Reached through `String::from_utf8` rather than `str::from_utf8` with a
        // literal, so this is a genuine runtime decode failure: the `str` form
        // with an invalid literal is rejected at compile time, which would make
        // the test assert nothing.
        let from_utf8 = WorkerError::NotUtf8(
            String::from_utf8(vec![0xffu8, 0xfe, 0xfd])
                .expect_err("invalid UTF-8")
                .utf8_error(),
        );
        assert!(from_utf8.is_fatal());
        assert!(!from_utf8.is_transport());
    }

    #[test]
    fn a_broken_pipe_is_a_transport_failure() {
        let err = WorkerError::Io(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone"));
        assert!(err.is_transport());
        assert!(err.is_fatal());
    }
}
