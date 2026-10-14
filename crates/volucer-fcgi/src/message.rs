//! Typed bodies for the `BEGIN_REQUEST`, `END_REQUEST` and value records.

use crate::error::FcgiError;
use crate::protocol::{flags, ProtocolStatus, Role};

/// `BEGIN_REQUEST` body: role, flags, five reserved bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BeginRequest {
    pub role: Role,
    pub keep_conn: bool,
}

impl BeginRequest {
    /// A `RESPONDER` request that asks the application to keep the connection.
    pub fn responder(keep_conn: bool) -> Self {
        BeginRequest {
            role: Role::Responder,
            keep_conn,
        }
    }

    pub fn encode(&self) -> [u8; 8] {
        let role = self.role.as_u16().to_be_bytes();
        let mut out = [0u8; 8];
        out[0] = role[0];
        out[1] = role[1];
        out[2] = if self.keep_conn { flags::KEEP_CONN } else { 0 };
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, FcgiError> {
        let role_bytes = bytes
            .get(0..2)
            .ok_or(FcgiError::TruncatedNameValue { offset: 0 })?;
        let role = Role::try_from(u16::from_be_bytes([role_bytes[0], role_bytes[1]]))?;
        let keep_conn = bytes
            .get(2)
            .is_some_and(|byte| byte & flags::KEEP_CONN != 0);
        Ok(BeginRequest { role, keep_conn })
    }
}

/// `END_REQUEST` body: a four-byte application status, a protocol status, and
/// three reserved bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndRequest {
    pub app_status: u32,
    pub protocol_status: ProtocolStatus,
}

impl EndRequest {
    pub fn complete() -> Self {
        EndRequest {
            app_status: 0,
            protocol_status: ProtocolStatus::RequestComplete,
        }
    }

    pub fn encode(&self) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[..4].copy_from_slice(&self.app_status.to_be_bytes());
        out[4] = self.protocol_status.as_u8();
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, FcgiError> {
        let status_bytes = bytes
            .get(0..4)
            .ok_or(FcgiError::TruncatedNameValue { offset: 0 })?;
        let app_status = u32::from_be_bytes(
            status_bytes
                .try_into()
                .expect("slice of length 4 checked above"),
        );
        let protocol_status = ProtocolStatus::try_from(
            *bytes
                .get(4)
                .ok_or(FcgiError::TruncatedNameValue { offset: 4 })?,
        )?;
        Ok(EndRequest {
            app_status,
            protocol_status,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_request_encodes_role_big_endian_then_flags() {
        let begin = BeginRequest::responder(true);
        assert_eq!(begin.encode(), [0, 1, 1, 0, 0, 0, 0, 0]);
        assert_eq!(BeginRequest::decode(&begin.encode()).unwrap(), begin);

        let no_keep = BeginRequest::responder(false);
        assert_eq!(no_keep.encode(), [0, 1, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn begin_request_roles_round_trip() {
        for role in [Role::Responder, Role::Authorizer, Role::Filter] {
            let begin = BeginRequest {
                role,
                keep_conn: false,
            };
            assert_eq!(BeginRequest::decode(&begin.encode()).unwrap().role, role);
        }
    }

    #[test]
    fn begin_request_rejects_an_unknown_role() {
        assert!(BeginRequest::decode(&[0, 9, 0, 0, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn begin_request_rejects_a_short_body() {
        assert!(BeginRequest::decode(&[0]).is_err());
        assert!(BeginRequest::decode(&[]).is_err());
    }

    #[test]
    fn end_request_encodes_app_status_then_protocol_status() {
        let end = EndRequest {
            app_status: 0x0102_0304,
            protocol_status: ProtocolStatus::Overloaded,
        };
        assert_eq!(end.encode(), [1, 2, 3, 4, 2, 0, 0, 0]);
        assert_eq!(EndRequest::decode(&end.encode()).unwrap(), end);

        assert_eq!(EndRequest::complete().encode(), [0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn end_request_rejects_a_short_body() {
        assert!(EndRequest::decode(&[0, 0, 0]).is_err());
    }
}
