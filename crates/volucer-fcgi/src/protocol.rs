//! FastCGI protocol constants and wire types.
//!
//! Every value here comes from the FastCGI specification:
//! <https://fastcgi-archives.github.io/FastCGI_Specification.html>

use crate::error::FcgiError;

/// The only protocol version FastCGI has ever had.
pub const VERSION_1: u8 = 1;

/// A record's content may not exceed this many bytes.
pub const MAX_CONTENT_LEN: usize = 65_535;

/// The fixed record header size.
pub const HEADER_LEN: usize = 8;

/// Record type. Not `#[non_exhaustive]`: the specification fixes the set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RecordType {
    BeginRequest = 1,
    AbortRequest = 2,
    EndRequest = 3,
    Params = 4,
    Stdin = 5,
    Stdout = 6,
    Stderr = 7,
    Data = 8,
    GetValues = 9,
    GetValuesResult = 10,
    UnknownType = 11,
}

impl RecordType {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Whether this record type carries a payload whose end is signalled by a
    /// zero-length record of the same type.
    ///
    /// `PARAMS`, `STDIN` and `DATA` are streams; `BEGIN_REQUEST` and the value
    /// records are single records.
    pub fn is_stream(self) -> bool {
        matches!(
            self,
            RecordType::Params | RecordType::Stdin | RecordType::Data
        )
    }
}

impl TryFrom<u8> for RecordType {
    type Error = FcgiError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            1 => RecordType::BeginRequest,
            2 => RecordType::AbortRequest,
            3 => RecordType::EndRequest,
            4 => RecordType::Params,
            5 => RecordType::Stdin,
            6 => RecordType::Stdout,
            7 => RecordType::Stderr,
            8 => RecordType::Data,
            9 => RecordType::GetValues,
            10 => RecordType::GetValuesResult,
            11 => RecordType::UnknownType,
            other => return Err(FcgiError::UnknownRecordType(other)),
        })
    }
}

/// The role a request asks the application to play.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Role {
    Responder = 1,
    Authorizer = 2,
    Filter = 3,
}

impl Role {
    pub fn as_u16(self) -> u16 {
        self as u16
    }
}

impl TryFrom<u16> for Role {
    type Error = FcgiError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Ok(match value {
            1 => Role::Responder,
            2 => Role::Authorizer,
            3 => Role::Filter,
            other => return Err(FcgiError::UnknownRole(other)),
        })
    }
}

/// `BEGIN_REQUEST` flags. Only `FCGI_KEEP_CONN` is defined.
pub mod flags {
    /// Ask the application to keep the connection open after the response.
    ///
    /// This is a *request*, not a guarantee: the application may close anyway,
    /// so the client must treat it as a slot-level state rather than assume
    /// persistence.
    pub const KEEP_CONN: u8 = 1;
}

/// `END_REQUEST` protocol status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ProtocolStatus {
    RequestComplete = 0,
    CantMpxConn = 1,
    Overloaded = 2,
    UnknownRole = 3,
}

impl ProtocolStatus {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// True when the application served the request to completion.
    pub fn is_complete(self) -> bool {
        matches!(self, ProtocolStatus::RequestComplete)
    }
}

impl TryFrom<u8> for ProtocolStatus {
    type Error = FcgiError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => ProtocolStatus::RequestComplete,
            1 => ProtocolStatus::CantMpxConn,
            2 => ProtocolStatus::Overloaded,
            3 => ProtocolStatus::UnknownRole,
            other => return Err(FcgiError::UnknownProtocolStatus(other)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_type_values_match_the_specification() {
        for (value, expected) in [
            (1u8, RecordType::BeginRequest),
            (2, RecordType::AbortRequest),
            (3, RecordType::EndRequest),
            (4, RecordType::Params),
            (5, RecordType::Stdin),
            (6, RecordType::Stdout),
            (7, RecordType::Stderr),
            (8, RecordType::Data),
            (9, RecordType::GetValues),
            (10, RecordType::GetValuesResult),
            (11, RecordType::UnknownType),
        ] {
            assert_eq!(RecordType::try_from(value).unwrap(), expected);
            assert_eq!(expected.as_u8(), value);
        }
        assert!(RecordType::try_from(0).is_err());
        assert!(RecordType::try_from(12).is_err());
    }

    #[test]
    fn only_stream_records_are_streams() {
        assert!(RecordType::Params.is_stream());
        assert!(RecordType::Stdin.is_stream());
        assert!(RecordType::Data.is_stream());
        assert!(!RecordType::Stdout.is_stream());
        assert!(!RecordType::BeginRequest.is_stream());
    }

    #[test]
    fn role_values_match_the_specification() {
        assert_eq!(Role::try_from(1).unwrap(), Role::Responder);
        assert_eq!(Role::Authorizer.as_u16(), 2);
        assert_eq!(Role::Filter.as_u16(), 3);
        assert!(Role::try_from(4).is_err());
    }

    #[test]
    fn protocol_status_values_match_the_specification() {
        assert_eq!(
            ProtocolStatus::try_from(0).unwrap(),
            ProtocolStatus::RequestComplete
        );
        assert_eq!(ProtocolStatus::CantMpxConn.as_u8(), 1);
        assert_eq!(ProtocolStatus::Overloaded.as_u8(), 2);
        assert_eq!(ProtocolStatus::UnknownRole.as_u8(), 3);
        assert!(ProtocolStatus::RequestComplete.is_complete());
        assert!(!ProtocolStatus::Overloaded.is_complete());
        assert!(ProtocolStatus::try_from(4).is_err());
    }

    #[test]
    fn keep_conn_flag_is_one() {
        assert_eq!(flags::KEEP_CONN, 1);
    }
}
