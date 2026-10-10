<?php

declare(strict_types=1);

namespace Stokehold;

/**
 * The message type table from docs/worker-protocol.md.
 *
 * The numbering is the protocol. These constants must match
 * `MessageType` in crates/stokehold-worker/src/header.rs exactly and must never
 * be reordered; a new type is appended, never inserted.
 */
final class MessageType
{
    public const HELLO = 1;
    public const HELLO_ACK = 2;
    public const REQUEST = 3;
    public const RESPONSE = 4;
    public const BODY = 5;
    public const EVENT = 6;
    public const LOG = 7;
    public const METRIC = 8;
    public const CONTROL = 9;
    public const CONTROL_ACK = 10;
    public const END = 11;
    public const PING = 12;
    public const PONG = 13;
    public const ERROR = 14;

    /**
     * The wire name of a type, for diagnostics.
     *
     * Mirrors `MessageType::name()` on the Rust side so both ends describe a
     * frame the same way in a log.
     */
    public static function name(int $type): string
    {
        return match ($type) {
            self::HELLO => 'HELLO',
            self::HELLO_ACK => 'HELLO_ACK',
            self::REQUEST => 'REQUEST',
            self::RESPONSE => 'RESPONSE',
            self::BODY => 'BODY',
            self::EVENT => 'EVENT',
            self::LOG => 'LOG',
            self::METRIC => 'METRIC',
            self::CONTROL => 'CONTROL',
            self::CONTROL_ACK => 'CONTROL_ACK',
            self::END => 'END',
            self::PING => 'PING',
            self::PONG => 'PONG',
            self::ERROR => 'ERROR',
            default => "unknown ({$type})",
        };
    }
}
