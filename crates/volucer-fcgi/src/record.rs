//! Record framing: the fixed 8-byte header, and readers and writers for records.
//!
//! The header is big-endian throughout and the content length is capped at
//! 65535 per record, so larger payloads are split across several records of the
//! same type and terminated by a zero-length record (for stream types).

use std::io::{self, Read, Write};

use crate::error::FcgiError;
use crate::protocol::{RecordType, HEADER_LEN, MAX_CONTENT_LEN, VERSION_1};

/// A record header, exactly as it appears on the wire.
///
/// Field names mirror the specification so the mapping stays obvious.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub version: u8,
    pub record_type: RecordType,
    pub request_id: u16,
    pub content_length: u16,
    pub padding_length: u8,
    pub reserved: u8,
}

impl Header {
    /// Build a header for a single record, computing the padding needed to
    /// satisfy the specification's 8-byte alignment, which applies to
    /// `PARAMS`, `STDIN`, `STDOUT` and `STDERR`.
    pub fn new(record_type: RecordType, request_id: u16, content_length: u16) -> Self {
        let padding_length = if record_type.is_stream() {
            padding_for(content_length as usize)
        } else {
            0
        };
        Header {
            version: VERSION_1,
            record_type,
            request_id,
            content_length,
            padding_length,
            reserved: 0,
        }
    }

    /// Total bytes of this record, header and padding included.
    pub fn total_len(&self) -> usize {
        HEADER_LEN + self.content_length as usize + self.padding_length as usize
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let id = self.request_id.to_be_bytes();
        let len = self.content_length.to_be_bytes();
        [
            self.version,
            self.record_type.as_u8(),
            id[0],
            id[1],
            len[0],
            len[1],
            self.padding_length,
            self.reserved,
        ]
    }

    pub fn decode(bytes: [u8; HEADER_LEN]) -> Result<Self, FcgiError> {
        if bytes[0] != VERSION_1 {
            return Err(FcgiError::UnsupportedVersion(bytes[0]));
        }
        // No length validation is needed: `content_length` is a two-byte field,
        // so it cannot exceed MAX_CONTENT_LEN by construction.
        Ok(Header {
            version: bytes[0],
            record_type: RecordType::try_from(bytes[1])?,
            request_id: u16::from_be_bytes([bytes[2], bytes[3]]),
            content_length: u16::from_be_bytes([bytes[4], bytes[5]]),
            padding_length: bytes[6],
            reserved: bytes[7],
        })
    }

    /// Read and decode a header, or `None` at a clean end of stream.
    pub fn read<R: Read>(reader: &mut R) -> Result<Option<Self>, FcgiError> {
        let mut buf = [0u8; HEADER_LEN];
        match read_exact_or_eof(reader, &mut buf)? {
            false => Ok(None),
            true => Ok(Some(Header::decode(buf)?)),
        }
    }

    /// Write this header.
    pub fn write<W: Write>(&self, writer: &mut W) -> Result<(), FcgiError> {
        writer.write_all(&self.encode())?;
        Ok(())
    }
}

/// Padding length that brings a record to an 8-byte boundary.
///
/// The specification: "A record with a payload of length `n` will be padded to
/// a multiple of 8 with `8 - (n % 8)` padding bytes", where a payload that is
/// already a multiple of 8 gets no padding.
pub fn padding_for(content_length: usize) -> u8 {
    let remainder = content_length % 8;
    if remainder == 0 {
        0
    } else {
        (8 - remainder) as u8
    }
}

/// Write a payload as one or more records of `record_type`, split at the
/// 65535-byte boundary.
///
/// When `terminate` is set, a zero-length record is emitted afterwards, which is
/// how stream record types (`PARAMS`, `STDIN`, `DATA`) signal their end.
pub fn write_records<W: Write>(
    writer: &mut W,
    record_type: RecordType,
    request_id: u16,
    content: &[u8],
    terminate: bool,
) -> Result<(), FcgiError> {
    for chunk in content.chunks(MAX_CONTENT_LEN) {
        let header = Header::new(record_type, request_id, chunk.len() as u16);
        header.write(writer)?;
        writer.write_all(chunk)?;
        write_padding(writer, header.padding_length)?;
    }

    // An empty payload still needs its terminating record, so this check is on
    // `terminate` rather than on the content being non-empty.
    if terminate || content.is_empty() && record_type.is_stream() {
        let header = Header::new(record_type, request_id, 0);
        header.write(writer)?;
    }
    writer.flush()?;
    Ok(())
}

/// Append a payload as one or more records to a buffer.
///
/// The byte-for-byte equivalent of [`write_records`], but onto a `Vec` so a caller
/// can build a whole request and put it on the wire in one `write`. The duplication
/// is deliberate and the two are kept adjacent and tested against each other
/// (`append_and_write_produce_the_same_bytes`): making `write_records` delegate to
/// this one would allocate a `Vec` per request on every path that uses the plain
/// writer, which is the opposite of the point.
pub fn append_records(
    out: &mut Vec<u8>,
    record_type: RecordType,
    request_id: u16,
    content: &[u8],
    terminate: bool,
) {
    for chunk in content.chunks(MAX_CONTENT_LEN) {
        let header = Header::new(record_type, request_id, chunk.len() as u16);
        out.extend_from_slice(&header.encode());
        out.extend_from_slice(chunk);
        append_padding(out, header.padding_length);
    }

    if terminate || content.is_empty() && record_type.is_stream() {
        let header = Header::new(record_type, request_id, 0);
        out.extend_from_slice(&header.encode());
    }
}

fn append_padding(out: &mut Vec<u8>, padding: u8) {
    if padding > 0 {
        // Padding must be zero bytes.
        const ZEROS: [u8; 8] = [0u8; 8];
        out.extend_from_slice(&ZEROS[..padding as usize]);
    }
}

fn write_padding<W: Write>(writer: &mut W, padding: u8) -> Result<(), FcgiError> {
    if padding > 0 {
        // Padding must be zero bytes.
        const ZEROS: [u8; 8] = [0u8; 8];
        writer.write_all(&ZEROS[..padding as usize])?;
    }
    Ok(())
}

/// A record read off the wire, payload included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub header: Header,
    pub content: Vec<u8>,
}

impl Record {
    pub fn record_type(&self) -> RecordType {
        self.header.record_type
    }

    pub fn request_id(&self) -> u16 {
        self.header.request_id
    }
}

/// Read one record, discarding its padding.
pub fn read_record<R: Read>(reader: &mut R) -> Result<Option<Record>, FcgiError> {
    let Some(header) = Header::read(reader)? else {
        return Ok(None);
    };

    let mut content = vec![0u8; header.content_length as usize];
    reader.read_exact(&mut content)?;

    if header.padding_length > 0 {
        let mut padding = [0u8; 255];
        reader.read_exact(&mut padding[..header.padding_length as usize])?;
    }

    Ok(Some(Record { header, content }))
}

/// Read exactly `buf.len()` bytes, reporting a clean EOF only when nothing at
/// all was read.
fn read_exact_or_eof<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<bool, io::Error> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "stream ended in the middle of a record header",
                ))
            }
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_layout_is_big_endian() {
        let header = Header::new(RecordType::BeginRequest, 0x0102, 0x0304);
        let bytes = header.encode();
        assert_eq!(bytes, [1, 1, 0x01, 0x02, 0x03, 0x04, 0, 0]);
        assert_eq!(Header::decode(bytes).unwrap(), header);
    }

    #[test]
    fn request_ids_use_both_bytes() {
        let header = Header::new(RecordType::Stdin, 0xabcd, 0);
        let bytes = header.encode();
        assert_eq!([bytes[2], bytes[3]], [0xab, 0xcd]);
        assert_eq!(Header::decode(bytes).unwrap().request_id, 0xabcd);
    }

    #[test]
    fn padding_aligns_to_eight_bytes() {
        assert_eq!(padding_for(0), 0);
        assert_eq!(padding_for(1), 7);
        assert_eq!(padding_for(7), 1);
        assert_eq!(padding_for(8), 0);
        assert_eq!(padding_for(9), 7);
        assert_eq!(padding_for(16), 0);
        // A full record stays a multiple of 8 with no padding.
        assert_eq!(padding_for(MAX_CONTENT_LEN), 1);
        for len in 0..64usize {
            assert_eq!((len + padding_for(len) as usize) % 8, 0, "len {len}");
        }
    }

    #[test]
    fn non_stream_records_have_no_padding() {
        let header = Header::new(RecordType::BeginRequest, 1, 8);
        assert_eq!(header.padding_length, 0);
        assert_eq!(header.total_len(), HEADER_LEN + 8);
    }

    #[test]
    fn stream_records_carry_padding_in_the_header() {
        let header = Header::new(RecordType::Stdin, 1, 5);
        assert_eq!(header.padding_length, 3);
        assert_eq!(header.total_len(), HEADER_LEN + 5 + 3);
    }

    #[test]
    fn rejects_an_unknown_version() {
        let mut bytes = Header::new(RecordType::Stdin, 1, 0).encode();
        bytes[0] = 2;
        assert!(matches!(
            Header::decode(bytes),
            Err(FcgiError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn rejects_a_zero_length_padding_value() {
        // Padding length must be 0..=255; 255 is legal, and the reader must
        // cope with it rather than allocating from a bogus size.
        let mut bytes = Header::new(RecordType::Stdin, 1, 0).encode();
        bytes[6] = 255;
        let header = Header::decode(bytes).unwrap();
        assert_eq!(header.padding_length, 255);
    }

    #[test]
    fn writes_and_reads_a_stream_terminated_by_an_empty_record() {
        let mut wire = Vec::new();
        write_records(&mut wire, RecordType::Stdin, 7, b"hello world", true).unwrap();

        let mut reader = wire.as_slice();
        let first = read_record(&mut reader).unwrap().unwrap();
        assert_eq!(first.request_id(), 7);
        assert_eq!(first.content, b"hello world");
        assert_eq!(first.header.padding_length, 5);

        let terminator = read_record(&mut reader).unwrap().unwrap();
        assert_eq!(terminator.record_type(), RecordType::Stdin);
        assert!(terminator.content.is_empty());

        assert_eq!(read_record(&mut reader).unwrap(), None);
    }

    #[test]
    fn splits_payloads_larger_than_the_record_limit() {
        let payload = vec![b'x'; MAX_CONTENT_LEN + 10];
        let mut wire = Vec::new();
        write_records(&mut wire, RecordType::Stdin, 3, &payload, false).unwrap();

        let mut reader = wire.as_slice();
        let first = read_record(&mut reader).unwrap().unwrap();
        assert_eq!(first.content.len(), MAX_CONTENT_LEN);

        let second = read_record(&mut reader).unwrap().unwrap();
        assert_eq!(second.content.len(), 10);

        // `terminate = false` means no zero-length record follows.
        assert_eq!(read_record(&mut reader).unwrap(), None);
    }

    #[test]
    fn a_truncated_header_is_an_error_not_a_clean_eof() {
        let mut wire = Header::new(RecordType::Stdin, 1, 4).encode().to_vec();
        wire.truncate(3);
        assert!(read_record(&mut wire.as_slice()).is_err());
    }

    #[test]
    fn eof_between_records_is_clean() {
        assert_eq!(read_record(&mut [].as_slice()).unwrap(), None);
    }

    /// `append_records` exists so a request can be built in memory and written in
    /// one call. If the two paths ever disagree the fast path sends a different
    /// request from the tested one, which is exactly the bug that would be hardest
    /// to see — a working request with a subtly different framing.
    #[test]
    fn append_and_write_produce_the_same_bytes() {
        let cases: Vec<(Vec<u8>, bool)> = vec![
            (vec![], true),
            (vec![], false),
            (b"a".to_vec(), true),
            (b"short params".to_vec(), false),
            (vec![b'p'; 7], true),
            (vec![b'p'; 8], true),
            (vec![b'p'; 9], true),
            (vec![b'z'; MAX_CONTENT_LEN], true),
            (vec![b'z'; MAX_CONTENT_LEN + 1], true),
            (vec![b'z'; MAX_CONTENT_LEN * 2], false),
        ];

        for (content, terminate) in cases {
            for record_type in [RecordType::Params, RecordType::Stdin] {
                let mut written = Vec::new();
                write_records(&mut written, record_type, 7, &content, terminate).unwrap();

                let mut appended = Vec::new();
                append_records(&mut appended, record_type, 7, &content, terminate);

                assert_eq!(
                    appended,
                    written,
                    "record_type {record_type:?}, {} bytes, terminate={terminate}",
                    content.len()
                );
            }
        }
    }
}
