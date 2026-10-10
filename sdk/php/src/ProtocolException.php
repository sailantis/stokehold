<?php

declare(strict_types=1);

namespace Stokehold;

/**
 * A protocol failure.
 *
 * The distinction the Rust side calls `is_fatal` matters here too: a *framing*
 * failure means the stream can no longer be trusted and the worker should exit,
 * while a *message* failure means one request was bad and the worker is fine.
 */
class ProtocolException extends \RuntimeException
{
}
