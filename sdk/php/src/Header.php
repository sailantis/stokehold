<?php

declare(strict_types=1);

namespace Stokehold;

/**
 * A decoded frame header: the 16 bytes that precede a payload.
 *
 * This is a direct transliteration of `Header::decode` in
 * crates/stokehold-worker/src/header.rs. The two ends are held together by the
 * golden fixtures in tests/WireFormatTest.php, so a change to one that is not
 * mirrored in the other fails there rather than in production.
 *
 * Layout, little-endian:
 *
 *   offset size field
 *   0      4    payload_len
 *   4      1    protocol_version
 *   5      1    message_type
 *   6      2    flags
 *   8      4    request_id
 *   12     4    crc32c(payload)
 */
final class Header
{
    public const PROTOCOL_VERSION = 1;
    public const HEADER_LEN = 16;

    /** Reserved. No codec is negotiated in v1, so a frame carrying it is refused. */
    public const FLAG_COMPRESSED = 0x0001;
    /** The payload is one chunk of a larger logical message. */
    public const FLAG_CHUNKED = 0x0002;
    /** This is the last chunk of a logical message. */
    public const FLAG_LAST = 0x0004;
    /** The sender is reporting a failure. */
    public const FLAG_ERROR = 0x0008;

    public function __construct(
        public readonly int $payloadLen,
        public readonly int $messageType,
        public readonly int $flags,
        public readonly int $requestId,
        public readonly int $checksum,
    ) {
    }

    /**
     * Parse a header, which must be exactly 16 bytes.
     *
     * Exactly — not "at least". The read loop satisfies this by asking the
     * stream for 16 bytes and never holding more, which is the whole reason the
     * SDK does not hand-roll a partial-read loop: `stream_get_contents($h, 16)`
     * blocks until it has 16 bytes or reaches EOF, handling short reads
     * internally and leaving nothing behind to steal from the payload.
     *
     * Anything shorter is an error, never a clean close: silently treating a
     * stream that died mid-header as ended would hide a truncated frame.
     */
    public static function decode(string $bytes): self
    {
        $length = strlen($bytes);
        if ($length !== self::HEADER_LEN) {
            throw new ProtocolException(sprintf(
                'a frame header is exactly %d bytes, got %d',
                self::HEADER_LEN,
                $length,
            ));
        }

        /** @var array{payload_len: int, version: int, type: int, flags: int, request_id: int, checksum: int} $fields */
        $fields = unpack('Vpayload_len/Cversion/Ctype/vflags/Vrequest_id/Vchecksum', $bytes);

        if ($fields['version'] !== self::PROTOCOL_VERSION) {
            throw new ProtocolException(sprintf(
                'unsupported protocol version %d (this SDK speaks %d)',
                $fields['version'],
                self::PROTOCOL_VERSION,
            ));
        }

        if ($fields['type'] < MessageType::HELLO || $fields['type'] > MessageType::ERROR) {
            throw new ProtocolException(
                'unknown message type ' . $fields['type'] . ' (v1 defines 1..14)'
            );
        }

        // Refused rather than misread: with no codec negotiated, a "compressed"
        // payload would be handed to json_decode as if it were plain text.
        if (($fields['flags'] & self::FLAG_COMPRESSED) !== 0) {
            throw new ProtocolException('refusing a compressed frame: no codec is negotiated');
        }

        return new self(
            $fields['payload_len'],
            $fields['type'],
            $fields['flags'],
            $fields['request_id'],
            $fields['checksum'],
        );
    }

    public function isLast(): bool
    {
        return ($this->flags & self::FLAG_LAST) !== 0;
    }

    /**
     * The wire name of this frame's type, for diagnostics.
     */
    public function typeName(): string
    {
        return MessageType::name($this->messageType);
    }
}
