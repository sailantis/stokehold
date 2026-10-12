//! The protocol freeze.
//!
//! `docs/worker-protocol.md` is the specification and the PHP SDK will be written
//! against it, so the wire format is a contract rather than an implementation
//! detail. These tests assert the **exact bytes** and the **exact message
//! numbers**, which means a refactor that changes framing fails here loudly
//! instead of quietly breaking the SDK.
//!
//! The golden fixtures below were computed independently (a throwaway Python
//! model of the header and CRC) and cross-checked against the Rust
//! implementation, so they are not merely "whatever the code currently does".

use stokehold_worker::codec::write_hello_ack;
use stokehold_worker::header::{crc32c, flags, write_frame, Checksum, HEADER_LEN, PROTOCOL_VERSION};
use stokehold_worker::{
    Codec, Header, Hello, HelloAck, Json, MessageType, RequestEnd, ResponseHead, MAX_BODY_CHUNK,
    MAX_PAYLOAD_LEN, UNSOLICITED,
};

fn hex(data: &[u8]) -> String {
    data.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A frame as the writer would emit it: header, then payload.
fn wire(message_type: MessageType, request_id: u32, payload: &[u8], extra_flags: u16) -> Vec<u8> {
    wire_with(
        message_type,
        request_id,
        payload,
        extra_flags,
        Checksum::Required,
    )
}

/// The same, written under a checksum policy.
fn wire_with(
    message_type: MessageType,
    request_id: u32,
    payload: &[u8],
    extra_flags: u16,
    policy: Checksum,
) -> Vec<u8> {
    let mut header =
        Header::with_checksum(message_type, request_id, payload, policy).expect("a small payload");
    header.flags |= extra_flags;
    let mut out = Vec::new();
    write_frame(&mut out, &header, payload).expect("writing to a Vec cannot fail");
    out
}

#[test]
fn the_request_fixture_in_the_specification_matches_the_writer() {
    // `docs/worker-protocol.md` shows this frame byte for byte as the reference layout, so it
    // is a contract with a reader, not an illustration. The payload is deliberately the
    // smallest one with a non-zero checksum, because a zero checksum here could not tell a
    // working hash from a missing one.
    let expected = concat!(
        "02000000", "01", "03", "0000", "01000000", "aad07b29", "7b7d",
    );
    assert_eq!(hex(&wire(MessageType::Request, 1, b"{}", 0)), expected);
    assert_eq!(crc32c(b"{}"), 0x297B_D0AA);
}

#[test]
fn the_header_is_sixteen_bytes_of_little_endian_fields() {
    assert_eq!(HEADER_LEN, 16);

    // A hand-built header with a distinct value in every field, so the test
    // pins field order and endianness rather than just round-tripping.
    let header = Header {
        payload_len: 0x1122_3344,
        protocol_version: PROTOCOL_VERSION,
        message_type: MessageType::Hello,
        flags: 0x5566,
        request_id: 0x7788_99AA,
        checksum: 0xBBCC_DDEE,
    };

    //      payload_len      ver  type  flags    request_id       crc32c
    let expected = concat!("44332211", "01", "01", "6655", "aa998877", "eeddccbb");
    assert_eq!(hex(&header.encode()), expected);
}

#[test]
fn a_hello_with_an_empty_payload_is_the_canonical_zero_fixture() {
    // Every field but the version and type is zero, which makes this the easiest
    // fixture for the PHP SDK to match first.
    let expected = concat!("00000000", "01", "01", "0000", "00000000", "00000000");
    assert_eq!(
        hex(&wire(MessageType::Hello, UNSOLICITED, b"", 0)),
        expected
    );
}

#[test]
fn the_crc32c_check_values_are_the_standard_ones() {
    assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    assert_eq!(crc32c(b""), 0x0000_0000);
    // Not crc32b, which is the polynomial zlib and PHP's `crc32()` use.
    assert_ne!(crc32c(b"123456789"), 0xCBF4_3926);
}

#[test]
fn every_message_type_encodes_to_its_frozen_wire_number() {
    // Frozen: the PHP SDK has the same table, so these numbers are not ours to
    // renumber. Adding a type appends; it never reorders.
    let frozen = [
        (MessageType::Hello, 1u8),
        (MessageType::HelloAck, 2),
        (MessageType::Request, 3),
        (MessageType::Response, 4),
        (MessageType::Body, 5),
        (MessageType::Event, 6),
        (MessageType::Log, 7),
        (MessageType::Metric, 8),
        (MessageType::Control, 9),
        (MessageType::ControlAck, 10),
        (MessageType::End, 11),
        (MessageType::Ping, 12),
        (MessageType::Pong, 13),
        (MessageType::Error, 14),
    ];

    for (message_type, number) in frozen {
        let frame = wire(message_type, UNSOLICITED, b"", 0);
        assert_eq!(
            frame[5], number,
            "{message_type:?} must be wire type {number}"
        );
        assert_eq!(
            MessageType::from_u8(number).expect("a frozen type parses"),
            message_type
        );
    }

    // The table is 1..=14 with no gaps, so "type + 1" is the next type.
    assert_eq!(frozen.len(), 14);
    assert!(MessageType::from_u8(0).is_err(), "0 is not a message type");
    assert!(MessageType::from_u8(15).is_err(), "15 is not defined in v1");
}

#[test]
fn the_protocol_version_is_one_and_anything_else_is_refused() {
    assert_eq!(PROTOCOL_VERSION, 1);

    let mut frame = wire(MessageType::Hello, UNSOLICITED, b"", 0);
    frame[4] = 2;
    let header: [u8; HEADER_LEN] = frame[..HEADER_LEN].try_into().unwrap();
    let err = Header::decode(&header).expect_err("version 2 must not be guessed at");
    assert!(err.to_string().contains("version 2"), "{err}");
}

#[test]
fn the_reserved_compressed_flag_is_rejected_rather_than_misread() {
    let frame = wire(MessageType::Request, 1, b"xxxx", flags::COMPRESSED);
    let header: [u8; HEADER_LEN] = frame[..HEADER_LEN].try_into().unwrap();
    let err = Header::decode(&header).expect_err("no codec is negotiated in v1");
    assert!(err.to_string().contains("uncompressed"), "{err}");
}

#[test]
fn the_payload_limits_are_the_documented_values() {
    // Pinned as literals because both are part of the contract: the chunk size
    // bounds what one frame carries, and the frame limit bounds what a corrupt
    // length field can make a reader allocate.
    assert_eq!(MAX_BODY_CHUNK, 64 * 1024);
    assert_eq!(MAX_PAYLOAD_LEN, 64 * 1024 * 1024);
}

#[test]
fn a_minimal_response_exchange_matches_its_golden_bytes() {
    // RESPONSE(status 200, no headers, empty body) then END(keep_alive).
    let head = Json::encode(&ResponseHead::default()).unwrap();
    let end = Json::encode(&RequestEnd::default()).unwrap();

    // Guard the fixture itself: if the JSON shape ever changes, say so here
    // rather than producing a confusing byte diff below.
    assert_eq!(head, br#"{"status":200,"headers":[],"body_len":0}"#);
    assert_eq!(
        end,
        br#"{"app_status":0,"peak_memory_bytes":0,"duration_us":0,"keep_alive":true}"#
    );

    let mut exchange = wire(MessageType::Response, 1, &head, 0);
    exchange.extend_from_slice(&wire(MessageType::End, 1, &end, 0));

    let expected = concat!(
        // payload_len 40, version 1, RESPONSE (4), flags 0, request_id 1, crc32c
        "28000000",
        "01",
        "04",
        "0000",
        "01000000",
        "83129f7b",
        "7b22737461747573223a3230302c2268656164657273223a5b5d2c22626f64795f6c656e223a307d",
        // payload_len 72, version 1, END (11), flags 0, request_id 1, crc32c
        "48000000",
        "01",
        "0b",
        "0000",
        "01000000",
        "770fe5b3",
        "7b226170705f737461747573223a302c227065616b5f6d656d6f72795f6279746573223a302c",
        "226475726174696f6e5f7573223a302c226b6565705f616c697665223a747275657d",
    );

    assert_eq!(hex(&exchange), expected);
    assert_eq!(exchange.len(), 144, "two frames of 16 + payload each");

    // And the reader recovers exactly what was written.
    let mut cursor = std::io::Cursor::new(exchange);
    let (first, first_payload) = Header::read_frame(&mut cursor).unwrap().unwrap();
    assert_eq!(first.message_type, MessageType::Response);
    assert_eq!(first.payload_len, 40);
    assert_eq!(first_payload, head);
    let (second, second_payload) = Header::read_frame(&mut cursor).unwrap().unwrap();
    assert_eq!(second.message_type, MessageType::End);
    assert_eq!(second_payload, end);
    assert!(Header::read_frame(&mut cursor).unwrap().is_none());
}

#[test]
fn a_last_flagged_body_frame_sets_only_the_last_bit() {
    // The SDK checks this bit, so its value is part of the contract.
    let frame = wire(MessageType::Body, 7, b"ok", flags::LAST);
    assert_eq!(&frame[6..8], &[0x04, 0x00]);
    assert_eq!(frame[8..12], 7u32.to_le_bytes());

    let header: [u8; HEADER_LEN] = frame[..HEADER_LEN].try_into().unwrap();
    let decoded = Header::decode(&header).unwrap();
    assert_eq!(decoded.flags & flags::LAST, flags::LAST);
    assert_eq!(decoded.flags & flags::CHUNKED, 0);
    assert_eq!(decoded.flags & flags::COMPRESSED, 0);
}

#[test]
fn the_request_id_is_the_field_at_offset_eight() {
    // Cheap to get wrong (it sits between flags and the checksum), so pin the
    // offset rather than only round-tripping.
    let frame = wire(MessageType::Request, 0x0102_0304, b"", 0);
    assert_eq!(&frame[8..12], &[0x04, 0x03, 0x02, 0x01]);
    assert_eq!(
        crc32c(b""),
        u32::from_le_bytes(frame[12..16].try_into().unwrap())
    );
}

#[test]
fn the_handshake_helper_writes_what_the_channel_expects() {
    // `write_hello_ack` exists for tests and for a fixture producer; make sure
    // it emits a real, readable HELLO_ACK rather than something hand-rolled.
    let ack = HelloAck {
        protocol_version: PROTOCOL_VERSION,
        php_version: "8.3.31".to_string(),
        sapi: "cli".to_string(),
        pid: 4242,
        runtime: String::new(),
        no_checksum: false,
    };

    let mut bytes = Vec::new();
    write_hello_ack(&mut bytes, &ack).unwrap();

    let mut cursor = std::io::Cursor::new(bytes);
    let (header, payload) = Header::read_frame(&mut cursor).unwrap().unwrap();
    assert_eq!(header.message_type, MessageType::HelloAck);
    assert_eq!(header.request_id, UNSOLICITED);
    let decoded: HelloAck = Json::decode(&payload).unwrap();
    assert_eq!(decoded, ack);
}

#[test]
fn the_hello_payload_is_the_documented_fields_in_the_documented_order() {
    // Pinned because the PHP SDK decodes this JSON and because the payload is *not* fixed
    // length: it is the one handshake frame whose bytes changed when the checksum option
    // arrived. The `HELLO` header fixture above cannot catch a payload change.
    let required = Json::encode(&Hello {
        protocol_version: PROTOCOL_VERSION,
        no_checksum: false,
    })
    .unwrap();
    assert_eq!(
        required,
        br#"{"protocol_version":1,"no_checksum":false}"#,
        "the field names and order are the protocol"
    );
    assert_eq!(required.len(), 42);

    let optional = Json::encode(&Hello {
        protocol_version: PROTOCOL_VERSION,
        no_checksum: true,
    })
    .unwrap();
    assert_eq!(optional, br#"{"protocol_version":1,"no_checksum":true}"#);
}

#[test]
fn a_hello_frame_matches_its_golden_bytes() {
    // Computed independently (a throwaway Python model of the header and CRC-32C), like the
    // other fixtures here, so a change to either side of the framing is caught.
    let required = Json::encode(&Hello {
        protocol_version: PROTOCOL_VERSION,
        no_checksum: false,
    })
    .unwrap();
    let expected_required = concat!(
        // payload_len 42, version 1, HELLO (1), flags 0, request_id 0, crc32c
        "2a000000", "01", "01", "0000", "00000000", "96d7c823",
        "7b2270726f746f636f6c5f76657273696f6e223a312c226e6f5f636865636b73756d223a66616c73657d",
    );
    assert_eq!(hex(&wire(MessageType::Hello, UNSOLICITED, &required, 0)), expected_required);

    // The optional form is the same frame with the `NO_CHECKSUM` flag raised and a zero
    // checksum field: the header keeps its 16 bytes and the payload only loses the `l` of
    // `false`.
    let optional = Json::encode(&Hello {
        protocol_version: PROTOCOL_VERSION,
        no_checksum: true,
    })
    .unwrap();
    let frame = wire_with(
        MessageType::Hello,
        UNSOLICITED,
        &optional,
        flags::NO_CHECKSUM,
        Checksum::Optional,
    );
    assert_eq!(
        hex(&frame),
        concat!(
            "29000000", "01", "01", "1000", "00000000", "00000000",
            "7b2270726f746f636f6c5f76657273696f6e223a312c226e6f5f636865636b73756d223a747275657d",
        )
    );
}

#[test]
fn the_no_checksum_flag_is_bit_four_of_the_flags_field() {
    // The bit's value is part of the protocol for the same reason the message numbers are:
    // the PHP SDK tests it.
    assert_eq!(flags::NO_CHECKSUM, 0b0001_0000);
    let frame = wire(MessageType::Request, 1, b"", flags::NO_CHECKSUM);
    assert_eq!(&frame[6..8], &[0x10, 0x00]);
    // And it is disjoint from every other defined bit, so raising it cannot be confused
    // with `LAST` or `ERROR`.
    for other in [flags::COMPRESSED, flags::CHUNKED, flags::LAST, flags::ERROR] {
        assert_eq!(flags::NO_CHECKSUM & other, 0, "{other:#06x} overlaps");
    }
}
