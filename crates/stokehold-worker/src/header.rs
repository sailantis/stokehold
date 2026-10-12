//! The 16-byte frame header and the message type table.
//!
//! Every frame on the worker channel is a header followed by `payload_len`
//! bytes of payload. The header is **little-endian** because both ends are
//! little-endian in practice (x86-64, arm64) and the fields are read with a
//! fixed-size load rather than a byte-by-byte accumulator; a big-endian host
//! would only lose a memcpy, never correctness.
//!
//! ```text
//! offset size field
//! 0      4    payload_len      (payload bytes, excluding this header)
//! 4      1    protocol_version (must be PROTOCOL_VERSION)
//! 5      1    message_type     (see MessageType)
//! 6      2    flags            (see flags::*)
//! 8      4    request_id       (0 for unsolicited / control frames)
//! 12     4    crc32c(payload)  (detects truncation and corruption)
//! ```

use std::io::{Read, Write};

use crate::error::{Result, WorkerError};

/// The only protocol version that exists so far.
pub const PROTOCOL_VERSION: u8 = 1;

/// The size of a frame header on the wire.
pub const HEADER_LEN: usize = 16;

/// Refuse absurd payloads before allocating.
///
/// The limit exists so a corrupt `payload_len` cannot make the reader allocate
/// 4 GiB. It is far above the largest legitimate body (the PHP SDK streams
/// bodies in `BODY` chunks instead of one huge `REQUEST`), so it never bites a
/// correct peer.
pub const MAX_PAYLOAD_LEN: usize = 64 * 1024 * 1024;

/// Bits in the `flags` field.
pub mod flags {
    /// The payload is compressed. Reserved: no codec is negotiated yet, so a
    /// frame with this bit is rejected rather than misread.
    pub const COMPRESSED: u16 = 0b0000_0001;
    /// The payload is one chunk of a larger logical message.
    pub const CHUNKED: u16 = 0b0000_0010;
    /// This is the last chunk of a logical message.
    pub const LAST: u16 = 0b0000_0100;
    /// The sender is reporting a failure; the payload is an `ERROR` description.
    pub const ERROR: u16 = 0b0000_1000;
}

/// What a frame carries.
///
/// The numbering is part of the protocol: the PHP SDK has the same table, so
/// these values must never be reordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum MessageType {
    /// Server → worker: start a worker, with its version information.
    Hello = 1,
    /// Worker → server: I am ready, here is my pid/SAPI/version.
    HelloAck = 2,
    /// Server → worker: serve this request (JSON metadata, no body).
    Request = 3,
    /// Worker → server: the response head for a request (status + headers).
    Response = 4,
    /// Either direction: raw bytes belonging to the request/response in
    /// `request_id`. Bodies travel here, not inside JSON, so uploads are not
    /// base64-inflated by a third.
    Body = 5,
    /// Worker → server: application-level telemetry (log line, timing, memory).
    Event = 6,
    /// Worker → server: one log record, forwarded rather than written to
    /// stdout (which is the protocol channel).
    Log = 7,
    /// Worker → server: one metric sample.
    Metric = 8,
    /// Server → worker: drain, shutdown, reload or ping-a-worker.
    Control = 9,
    /// Worker → server: acknowledgement of a control message.
    ControlAck = 10,
    /// Worker → server: the request is finished; carries the terminal status.
    End = 11,
    /// Either direction: keep the stream alive on an idle connection.
    Ping = 12,
    /// Either direction: answer to `Ping`.
    Pong = 13,
    /// Either direction: something went wrong; the payload is a description.
    Error = 14,
}

impl MessageType {
    /// Decode the wire value, rejecting anything this build does not know.
    pub fn from_u8(value: u8) -> Result<Self> {
        Ok(match value {
            1 => MessageType::Hello,
            2 => MessageType::HelloAck,
            3 => MessageType::Request,
            4 => MessageType::Response,
            5 => MessageType::Body,
            6 => MessageType::Event,
            7 => MessageType::Log,
            8 => MessageType::Metric,
            9 => MessageType::Control,
            10 => MessageType::ControlAck,
            11 => MessageType::End,
            12 => MessageType::Ping,
            13 => MessageType::Pong,
            14 => MessageType::Error,
            other => return Err(WorkerError::UnknownMessageType(other)),
        })
    }

    /// The wire name of this type, spelled exactly as the protocol document and
    /// the PHP SDK spell it.
    ///
    /// An associated function rather than a `Display` impl so the error paths
    /// can name an expectation as a `&'static str` without allocating.
    pub fn name(self) -> &'static str {
        match self {
            MessageType::Hello => "HELLO",
            MessageType::HelloAck => "HELLO_ACK",
            MessageType::Request => "REQUEST",
            MessageType::Response => "RESPONSE",
            MessageType::Body => "BODY",
            MessageType::Event => "EVENT",
            MessageType::Log => "LOG",
            MessageType::Metric => "METRIC",
            MessageType::Control => "CONTROL",
            MessageType::ControlAck => "CONTROL_ACK",
            MessageType::End => "END",
            MessageType::Ping => "PING",
            MessageType::Pong => "PONG",
            MessageType::Error => "ERROR",
        }
    }

    /// Whether a `request_id` is meaningful for this type.
    ///
    /// Unsolicited frames use `request_id = 0`; a reader that demanded a
    /// correlation id for a log line would reject perfectly good telemetry.
    pub fn is_correlated(self) -> bool {
        !matches!(
            self,
            MessageType::Hello
                | MessageType::HelloAck
                | MessageType::Event
                | MessageType::Log
                | MessageType::Metric
                | MessageType::Control
                | MessageType::ControlAck
                | MessageType::Ping
                | MessageType::Pong
        )
    }
}

/// A decoded frame header. The payload is read separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub payload_len: u32,
    pub protocol_version: u8,
    pub message_type: MessageType,
    pub flags: u16,
    pub request_id: u32,
    pub checksum: u32,
}

impl Header {
    /// Build a header, defaulting the checksum to "computed by the writer".
    pub fn new(message_type: MessageType, request_id: u32, payload: &[u8]) -> Result<Self> {
        if payload.len() > MAX_PAYLOAD_LEN {
            return Err(WorkerError::PayloadTooLarge {
                size: payload.len(),
                max: MAX_PAYLOAD_LEN,
            });
        }

        Ok(Header {
            payload_len: payload.len() as u32,
            protocol_version: PROTOCOL_VERSION,
            message_type,
            flags: 0,
            request_id,
            checksum: crc32c(payload),
        })
    }

    /// Serialize to its 16-byte form.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(&self.payload_len.to_le_bytes());
        out[4] = self.protocol_version;
        out[5] = self.message_type as u8;
        out[6..8].copy_from_slice(&self.flags.to_le_bytes());
        out[8..12].copy_from_slice(&self.request_id.to_le_bytes());
        out[12..16].copy_from_slice(&self.checksum.to_le_bytes());
        out
    }

    /// Parse the 16-byte form.
    pub fn decode(bytes: &[u8; HEADER_LEN]) -> Result<Self> {
        let payload_len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let protocol_version = bytes[4];
        if protocol_version != PROTOCOL_VERSION {
            return Err(WorkerError::UnsupportedVersion {
                found: protocol_version,
                expected: PROTOCOL_VERSION,
            });
        }
        let message_type = MessageType::from_u8(bytes[5])?;
        let flags = u16::from_le_bytes([bytes[6], bytes[7]]);
        if flags & flags::COMPRESSED != 0 {
            return Err(WorkerError::NotA {
                expected: "an uncompressed frame (no codec is negotiated)",
                actual: message_type,
            });
        }
        let request_id = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let checksum = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);

        Ok(Header {
            payload_len,
            protocol_version,
            message_type,
            flags,
            request_id,
            checksum,
        })
    }

    /// Read exactly one header.
    ///
    /// `Ok(None)` means a clean end of stream *before* the first byte. A stream
    /// that ends part-way through a header is an error: silently treating it as
    /// a clean close would hide a truncated frame.
    pub fn read<R: Read>(reader: &mut R) -> Result<Option<Self>> {
        let mut bytes = [0u8; HEADER_LEN];
        match read_exact_or_eof(reader, &mut bytes)? {
            0 => return Ok(None),
            HEADER_LEN => {}
            _ => return Err(WorkerError::TruncatedHeader),
        }
        Ok(Some(Header::decode(&bytes)?))
    }

    /// Write this header.
    pub fn write<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_all(&self.encode())?;
        Ok(())
    }

    /// Read a whole frame: header plus payload, with the checksum verified.
    pub fn read_frame<R: Read>(reader: &mut R) -> Result<Option<(Self, Vec<u8>)>> {
        let Some(header) = Header::read(reader)? else {
            return Ok(None);
        };

        let len = header.payload_len as usize;
        if len > MAX_PAYLOAD_LEN {
            return Err(WorkerError::PayloadTooLarge {
                size: len,
                max: MAX_PAYLOAD_LEN,
            });
        }

        // `vec![0u8; len]` zeroes the buffer before the read overwrites it, which
        // is a `memset` of the whole payload. It is left in place deliberately:
        // the alternative on stable Rust is reading into uninitialised storage
        // through `MaybeUninit`, and the profile shows this cost is not visible
        // next to the checksum. The checksum is the one that was worth removing.
        let mut payload = vec![0u8; len];
        if let Err(source) = reader.read_exact(&mut payload) {
            if source.kind() == std::io::ErrorKind::UnexpectedEof {
                return Err(WorkerError::TruncatedFrame {
                    offset: 0,
                    needed: len,
                });
            }
            return Err(WorkerError::Io(source));
        }

        let computed = crc32c(&payload);
        if computed != header.checksum {
            return Err(WorkerError::ChecksumMismatch {
                request_id: header.request_id,
                expected: header.checksum,
                computed,
            });
        }

        Ok(Some((header, payload)))
    }

    /// Read a whole frame into a caller-owned buffer, reusing its capacity.
    ///
    /// The counterpart of [`Header::read_frame`] for a reader that expects more frames, which
    /// is every loop in the protocol. It returns the payload's *length* rather than the payload
    /// because the bytes stay in `buffer`, where the caller can read them without the borrow
    /// conflict that returning a slice of a field would cause.
    ///
    /// Two costs disappear that `read_frame` has to pay on every frame:
    ///
    /// - the allocation, because the buffer outlives the frame; and
    /// - the zero-fill, because only the bytes past what is already initialised need zeroing —
    ///   a buffer already long enough is truncated instead of re-zeroed, so a steady stream of
    ///   64 KiB body frames stops writing 64 KiB of zeros per frame.
    ///
    /// The buffer is left holding the payload afterwards, so a caller that keeps the bytes must
    /// copy them before the next read.
    pub fn read_into<R: Read>(
        reader: &mut R,
        buffer: &mut Vec<u8>,
    ) -> Result<Option<(Self, usize)>> {
        let Some(header) = Header::read(reader)? else {
            return Ok(None);
        };

        let len = header.payload_len as usize;
        if len > MAX_PAYLOAD_LEN {
            return Err(WorkerError::PayloadTooLarge {
                size: len,
                max: MAX_PAYLOAD_LEN,
            });
        }

        // `resize` zero-fills only the new tail; `truncate` grows nothing back. Either way the
        // bytes that `read_exact` is about to overwrite are the only ones touched.
        if buffer.len() < len {
            buffer.resize(len, 0);
        } else {
            buffer.truncate(len);
        }

        if let Err(source) = reader.read_exact(&mut buffer[..len]) {
            if source.kind() == std::io::ErrorKind::UnexpectedEof {
                return Err(WorkerError::TruncatedFrame {
                    offset: 0,
                    needed: len,
                });
            }
            return Err(WorkerError::Io(source));
        }

        let computed = crc32c(&buffer[..len]);
        if computed != header.checksum {
            // The buffer holds a frame that failed its checksum; clearing it keeps a caller
            // that ignores the error from reading the bytes as if they were good.
            buffer.clear();
            return Err(WorkerError::ChecksumMismatch {
                request_id: header.request_id,
                expected: header.checksum,
                computed,
            });
        }

        Ok(Some((header, len)))
    }
}

/// Write a frame whose payload is already serialized.
pub fn write_frame<W: Write>(writer: &mut W, header: &Header, payload: &[u8]) -> Result<()> {
    header.write(writer)?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

/// Read into `buffer`, distinguishing "nothing at all" from "truncated".
fn read_exact_or_eof<R: Read>(reader: &mut R, buffer: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(source) if source.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(source) => return Err(WorkerError::Io(source)),
        }
    }
    Ok(filled)
}

/// CRC-32C (Castagnoli), the checksum the header carries.
///
/// The polynomial is the one PHP's `hash('crc32c', ...)` uses, and
/// `crc32c_matches_the_standard_check_value` pins it to the standard check value
/// so a swapped implementation cannot pass unnoticed.
///
/// This was written out by hand, eight shift-and-mask rounds per byte, because
/// the intent was that the PHP SDK could mirror it line for line. That was a
/// mistake about where the cost lands: the checksum is computed over every
/// response payload on every request, so a page of 12 KB meant ~96,000
/// iterations of the inner loop *per request*, and `perf` attributed 15-19% of
/// the server process's CPU to it. A table-driven or hardware implementation
/// is not an optimisation of a rare path; it is the removal of the largest
/// single item on the hot one.
///
/// The SDK does not need this to be a transcription. Only the *value* has to
/// agree, and it is pinned by test against a value PHP itself produces.
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c::crc32c(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_matches_the_standard_check_value() {
        // The canonical CRC-32C check value for "123456789", cross-checked
        // against `php -r "echo hash('crc32c', '123456789');"`.
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0);
    }

    #[test]
    fn crc32c_is_not_crc32b() {
        // Guards against reaching for the wrong polynomial by accident.
        assert_ne!(crc32c(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn a_header_round_trips() {
        let header = Header::new(MessageType::Request, 7, b"{}").unwrap();
        let bytes = header.encode();
        assert_eq!(bytes.len(), HEADER_LEN);
        assert_eq!(Header::decode(&bytes).unwrap(), header);
        assert_eq!(header.payload_len, 2);
        assert_eq!(header.protocol_version, PROTOCOL_VERSION);
    }

    #[test]
    fn the_header_layout_is_little_endian() {
        let header = Header {
            payload_len: 0x1122_3344,
            protocol_version: PROTOCOL_VERSION,
            message_type: MessageType::Hello,
            flags: 0x5566,
            request_id: 0x7788_99AA,
            checksum: 0xBBCC_DDEE,
        };
        let bytes = header.encode();
        assert_eq!(&bytes[0..4], &[0x44, 0x33, 0x22, 0x11]);
        assert_eq!(bytes[4], 1);
        assert_eq!(bytes[5], 1);
        assert_eq!(&bytes[6..8], &[0x66, 0x55]);
        assert_eq!(&bytes[8..12], &[0xAA, 0x99, 0x88, 0x77]);
        assert_eq!(&bytes[12..16], &[0xEE, 0xDD, 0xCC, 0xBB]);
    }

    #[test]
    fn every_message_type_survives_a_round_trip() {
        for raw in 1..=14u8 {
            let parsed = MessageType::from_u8(raw).unwrap();
            assert_eq!(parsed as u8, raw, "{parsed:?} did not round-trip");
        }
    }

    #[test]
    fn an_unknown_message_type_is_rejected() {
        let err = MessageType::from_u8(200).unwrap_err();
        assert!(matches!(err, WorkerError::UnknownMessageType(200)));
    }

    #[test]
    fn a_wrong_protocol_version_is_rejected() {
        let mut bytes = Header::new(MessageType::Hello, 0, b"").unwrap().encode();
        bytes[4] = 9;
        let err = Header::decode(&bytes).unwrap_err();
        assert!(
            matches!(
                err,
                WorkerError::UnsupportedVersion {
                    found: 9,
                    expected: 1
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn a_compressed_frame_is_refused_rather_than_misread() {
        let mut bytes = Header::new(MessageType::Request, 1, b"xxxx")
            .unwrap()
            .encode();
        bytes[6] = flags::COMPRESSED as u8;
        let err = Header::decode(&bytes).unwrap_err();
        assert!(err.to_string().contains("uncompressed"), "{err}");
    }

    #[test]
    fn only_correlated_types_have_request_ids() {
        assert!(MessageType::Request.is_correlated());
        assert!(MessageType::Body.is_correlated());
        assert!(MessageType::End.is_correlated());
        assert!(!MessageType::Log.is_correlated());
        assert!(!MessageType::Control.is_correlated());
        assert!(!MessageType::Ping.is_correlated());
    }

    #[test]
    fn a_frame_round_trips_through_a_stream() {
        let header = Header::new(MessageType::Body, 42, b"payload bytes").unwrap();
        let mut wire = Vec::new();
        write_frame(&mut wire, &header, b"payload bytes").unwrap();

        let mut cursor = std::io::Cursor::new(wire);
        let (read_header, payload) = Header::read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(read_header.request_id, 42);
        assert_eq!(read_header.message_type, MessageType::Body);
        assert_eq!(payload, b"payload bytes");
    }

    #[test]
    fn a_clean_end_of_stream_is_none_not_an_error() {
        let mut empty: &[u8] = &[];
        assert!(Header::read(&mut empty).unwrap().is_none());
        assert!(Header::read_frame(&mut empty).unwrap().is_none());
    }

    #[test]
    fn a_truncated_header_is_an_error_not_a_clean_close() {
        let header = Header::new(MessageType::Hello, 0, b"").unwrap();
        let bytes = header.encode();
        let mut partial = &bytes[..6];
        let err = Header::read(&mut partial).unwrap_err();
        assert!(matches!(err, WorkerError::TruncatedHeader), "{err}");
        assert!(err.is_fatal());
    }

    #[test]
    fn a_truncated_payload_is_reported_as_such() {
        let header = Header::new(MessageType::Body, 1, b"0123456789").unwrap();
        let mut wire = Vec::new();
        header.write(&mut wire).unwrap();
        wire.extend_from_slice(b"01234"); // only half the payload

        let mut cursor = std::io::Cursor::new(wire);
        let err = Header::read_frame(&mut cursor).unwrap_err();
        assert!(
            matches!(
                err,
                WorkerError::TruncatedFrame {
                    offset: 0,
                    needed: 10
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn a_corrupted_payload_fails_the_checksum() {
        let header = Header::new(MessageType::Body, 5, b"correct").unwrap();
        let mut wire = Vec::new();
        header.write(&mut wire).unwrap();
        wire.extend_from_slice(b"corrupt");

        let mut cursor = std::io::Cursor::new(wire);
        let err = Header::read_frame(&mut cursor).unwrap_err();
        match err {
            WorkerError::ChecksumMismatch { request_id, .. } => assert_eq!(request_id, 5),
            other => panic!("expected a checksum mismatch, got {other}"),
        }
    }

    #[test]
    fn an_absurd_payload_length_is_refused_before_allocating() {
        let mut bytes = Header::new(MessageType::Body, 1, b"").unwrap().encode();
        bytes[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut cursor = std::io::Cursor::new(bytes.to_vec());
        let err = Header::read_frame(&mut cursor).unwrap_err();
        assert!(matches!(err, WorkerError::PayloadTooLarge { .. }), "{err}");
    }

    #[test]
    fn writing_a_frame_computes_the_checksum_the_reader_verifies() {
        // The writer computes the checksum; a hand-built header with a stale
        // checksum must not be accepted.
        let mut header = Header::new(MessageType::Body, 1, b"aaaa").unwrap();
        header.checksum = crc32c(b"bbbb");
        let mut wire = Vec::new();
        write_frame(&mut wire, &header, b"aaaa").unwrap();
        let mut cursor = std::io::Cursor::new(wire);
        assert!(Header::read_frame(&mut cursor).is_err());
    }

    /// Frames of assorted shapes, for the two readers to be compared over.
    fn assorted_frames() -> Vec<(MessageType, u32, Vec<u8>)> {
        vec![
            (MessageType::End, 1, Vec::new()),
            (MessageType::Body, 2, b"x".to_vec()),
            (MessageType::Response, 3, vec![b'{'; 300]),
            (MessageType::Body, 4, vec![b'y'; crate::codec::MAX_BODY_CHUNK]),
            (MessageType::Log, 5, b"{\"level\":\"info\"}".to_vec()),
        ]
    }

    #[test]
    fn read_into_agrees_with_read_frame_over_a_whole_stream() {
        // `read_into` exists to remove the per-frame allocation and zero-fill that
        // `read_frame` pays. It is a second implementation of the same reading rule, so the
        // thing worth pinning is that it reads the same stream the same way — same headers,
        // same payloads, same stopping point, same errors. A difference in any of those would
        // be a protocol bug that only shows up on the pooled path.
        let mut wire = Vec::new();
        for (message_type, request_id, payload) in assorted_frames() {
            let header = Header::new(message_type, request_id, &payload).unwrap();
            write_frame(&mut wire, &header, &payload).unwrap();
        }

        let mut owned_cursor = std::io::Cursor::new(&wire);
        let mut reused_cursor = std::io::Cursor::new(&wire);
        let mut buffer: Vec<u8> = Vec::new();

        loop {
            let owned = Header::read_frame(&mut owned_cursor).unwrap();
            let reused = Header::read_into(&mut reused_cursor, &mut buffer).unwrap();

            match (owned, reused) {
                (None, None) => break,
                (Some((owned_header, owned_payload)), Some((reused_header, len))) => {
                    assert_eq!(owned_header, reused_header, "the headers disagree");
                    assert_eq!(
                        owned_payload,
                        buffer[..len],
                        "the payloads disagree for {reused_header:?}"
                    );
                }
                (owned, reused) => panic!(
                    "the two readers disagree on whether a frame was there: owned {:?}, \
                     reused {:?}",
                    owned.map(|(header, _)| header),
                    reused.map(|(header, _)| header)
                ),
            }
        }

        // Both must stop at the same place: a reader that consumed one frame too few would
        // break the *next* request on the channel rather than this one.
        assert_eq!(owned_cursor.position(), reused_cursor.position());
        assert_eq!(reused_cursor.position(), wire.len() as u64);
    }

    #[test]
    fn read_into_reuses_the_buffer_without_growing_it() {
        // The point of `read_into` is that the buffer's capacity is kept across frames: a
        // stream of body frames must not make the buffer grow each time, and a frame smaller
        // than the buffer must not shrink it either.
        let big = vec![b'z'; 4096];
        let small = b"tiny".to_vec();
        let mut wire = Vec::new();
        for (message_type, request_id, payload) in [
            (MessageType::Body, 1, &big),
            (MessageType::Body, 2, &small),
            (MessageType::Body, 3, &big),
        ] {
            let header = Header::new(message_type, request_id, payload).unwrap();
            write_frame(&mut wire, &header, payload).unwrap();
        }

        let mut cursor = std::io::Cursor::new(&wire);
        let mut buffer: Vec<u8> = Vec::new();
        let mut capacity = 0usize;

        while let Some((_, len)) = Header::read_into(&mut cursor, &mut buffer).unwrap() {
            assert!(len <= buffer.len());
            capacity = capacity.max(buffer.capacity());
        }

        assert!(
            buffer.capacity() == capacity,
            "the buffer's capacity changed across frames: {capacity} then {}",
            buffer.capacity()
        );
    }

    #[test]
    fn read_into_refuses_an_absurd_payload_length_before_growing() {
        // The guard must run before the buffer is resized, or a corrupt length would make the
        // reader allocate 4 GiB and only then report the error.
        let mut bytes = Header::new(MessageType::Body, 1, b"").unwrap().encode();
        bytes[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut cursor = std::io::Cursor::new(bytes.to_vec());
        let mut buffer: Vec<u8> = Vec::new();

        let err = Header::read_into(&mut cursor, &mut buffer).unwrap_err();
        assert!(matches!(err, WorkerError::PayloadTooLarge { .. }), "{err}");
        assert_eq!(buffer.capacity(), 0, "the buffer grew before the guard ran");
    }
}
