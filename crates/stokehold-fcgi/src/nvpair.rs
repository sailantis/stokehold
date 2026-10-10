//! FastCGI name-value pair encoding, used by `PARAMS` and the value records.

use crate::error::FcgiError;

/// A name-value pair length field is one byte below this threshold and four
/// bytes at or above it.
pub const NAME_VALUE_LEN_1: usize = 128;
/// The high bit of the *first byte*, which flags the four-byte encoding.
const FIRST_BYTE_HIGH_BIT: u8 = 0x80;
/// The high bit of the four-byte encoding, i.e. bit 31 of the big-endian value.
const LEN_4_HIGH_BIT: u32 = 0x8000_0000;
/// The remaining 31 bits carry the length.
const LEN_4_MASK: u32 = 0x7fff_ffff;

/// A decoded list of name-value pairs, in the order they appeared.
pub type NameValuePairs = Vec<(Vec<u8>, Vec<u8>)>;

/// Encode a length per the specification.
///
/// `< 128` is a single byte; otherwise four bytes big-endian with the high bit
/// of the first byte set and the remaining 31 bits carrying the length.
pub fn encode_len(out: &mut Vec<u8>, len: usize) {
    if len < NAME_VALUE_LEN_1 {
        out.push(len as u8);
        return;
    }
    let len = (len as u32) & LEN_4_MASK;
    out.extend_from_slice(&(len | LEN_4_HIGH_BIT).to_be_bytes());
}

/// Decode a length field at `input[offset]`, returning the length and how many
/// bytes it consumed.
pub fn decode_len(input: &[u8], offset: usize) -> Result<(usize, usize), FcgiError> {
    let first = *input
        .get(offset)
        .ok_or(FcgiError::TruncatedNameValue { offset })?;

    if first & FIRST_BYTE_HIGH_BIT == 0 {
        return Ok((first as usize, 1));
    }

    let end = offset + 4;
    let bytes: [u8; 4] = input
        .get(offset..end)
        .ok_or(FcgiError::TruncatedNameValue { offset })?
        .try_into()
        .expect("slice of length 4");
    let value = u32::from_be_bytes(bytes) & LEN_4_MASK;
    Ok((value as usize, 4))
}

/// Encode one name-value pair, appending both lengths and then the bytes.
pub fn encode_pair(out: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    encode_len(out, name.len());
    encode_len(out, value.len());
    out.extend_from_slice(name);
    out.extend_from_slice(value);
}

/// Encode a whole `PARAMS` payload from name-value pairs.
///
/// Names are kept in the order given: FastCGI does not require any order, but
/// preserving the caller's order makes captured traffic comparable.
pub fn encode_pairs<'a, I>(pairs: I) -> Vec<u8>
where
    I: IntoIterator<Item = (&'a [u8], &'a [u8])>,
{
    let mut out = Vec::with_capacity(256);
    for (name, value) in pairs {
        encode_pair(&mut out, name, value);
    }
    out
}

/// Decode a complete name-value payload, as produced by `encode_pairs`.
pub fn decode_pairs(input: &[u8]) -> Result<NameValuePairs, FcgiError> {
    let mut pairs = Vec::new();
    let mut offset = 0;
    while offset < input.len() {
        let (name_len, name_len_bytes) = decode_len(input, offset)?;
        offset += name_len_bytes;

        let (value_len, value_len_bytes) = decode_len(input, offset)?;
        offset += value_len_bytes;

        let name_end = offset
            .checked_add(name_len)
            .ok_or(FcgiError::TruncatedNameValue { offset })?;
        let name = input
            .get(offset..name_end)
            .ok_or(FcgiError::TruncatedNameValue { offset })?
            .to_vec();
        offset = name_end;

        let value_end = offset
            .checked_add(value_len)
            .ok_or(FcgiError::TruncatedNameValue { offset })?;
        let value = input
            .get(offset..value_end)
            .ok_or(FcgiError::TruncatedNameValue { offset })?
            .to_vec();
        offset = value_end;

        pairs.push((name, value));
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_lengths_are_one_byte() {
        let mut out = Vec::new();
        encode_len(&mut out, 0);
        encode_len(&mut out, 4);
        encode_len(&mut out, 127);
        assert_eq!(out, vec![0x00, 0x04, 0x7f]);
    }

    #[test]
    fn long_lengths_are_four_bytes_with_the_high_bit_set() {
        let mut out = Vec::new();
        encode_len(&mut out, 128);
        assert_eq!(out, vec![0x80, 0x00, 0x00, 0x80]);

        out.clear();
        encode_len(&mut out, 0x0102_0304);
        assert_eq!(out, vec![0x81, 0x02, 0x03, 0x04]);
    }

    #[test]
    fn length_boundary_is_exactly_128() {
        assert_eq!(NAME_VALUE_LEN_1, 128);
        let mut short = Vec::new();
        encode_len(&mut short, 127);
        assert_eq!(short.len(), 1);

        let mut long = Vec::new();
        encode_len(&mut long, 128);
        assert_eq!(long.len(), 4);
    }

    #[test]
    fn length_round_trips_across_the_boundary() {
        for len in [0usize, 1, 127, 128, 129, 65_535, 0x00ff_ffff] {
            let mut out = Vec::new();
            encode_len(&mut out, len);
            let (decoded, consumed) = decode_len(&out, 0).unwrap();
            assert_eq!(decoded, len, "len {len}");
            assert_eq!(consumed, out.len(), "len {len}");
        }
    }

    #[test]
    fn pairs_round_trip() {
        let payload = encode_pairs([
            (&b"REQUEST_METHOD"[..], &b"GET"[..]),
            (&b"SCRIPT_FILENAME"[..], &b"/srv/app/public/index.php"[..]),
            // Forces the four-byte length encoding on both name and value.
            (&vec![b'N'; 200][..], &vec![b'v'; 300][..]),
        ]);
        let pairs = decode_pairs(&payload).unwrap();
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0].0, b"REQUEST_METHOD");
        assert_eq!(pairs[0].1, b"GET");
        assert_eq!(pairs[1].1, b"/srv/app/public/index.php");
        assert_eq!(pairs[2].0.len(), 200);
        assert_eq!(pairs[2].1.len(), 300);
    }

    #[test]
    fn empty_payload_decodes_to_no_pairs() {
        assert!(decode_pairs(&[]).unwrap().is_empty());
    }

    #[test]
    fn truncated_pair_is_an_error_not_a_panic() {
        // Claims a 10-byte name but supplies two bytes.
        let payload = vec![0x0a, 0x01, b'h', b'i'];
        assert!(matches!(
            decode_pairs(&payload),
            Err(FcgiError::TruncatedNameValue { .. })
        ));

        // A lone length byte with nothing after it.
        assert!(matches!(
            decode_pairs(&[0x04]),
            Err(FcgiError::TruncatedNameValue { .. })
        ));

        // High bit set but fewer than four bytes remain.
        assert!(matches!(
            decode_pairs(&[0x80, 0x00]),
            Err(FcgiError::TruncatedNameValue { .. })
        ));
    }

    #[test]
    fn zero_length_name_and_value_are_valid() {
        let payload = encode_pairs([(&b""[..], &b""[..])]);
        assert_eq!(payload, vec![0x00, 0x00]);
        let pairs = decode_pairs(&payload).unwrap();
        assert_eq!(pairs, vec![(Vec::new(), Vec::new())]);
    }
}
