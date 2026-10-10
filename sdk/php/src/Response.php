<?php

declare(strict_types=1);

namespace Stokehold;

/**
 * A response the application produced.
 *
 * Header order is preserved as given, because the server relays it verbatim.
 */
final class Response
{
    /**
     * @param list<array{0: string, 1: string}> $headers
     */
    public function __construct(
        public int $status = 200,
        public array $headers = [],
        public string $body = '',
    ) {
    }

    public static function json(mixed $value, int $status = 200): self
    {
        $encoded = json_encode($value, JSON_THROW_ON_ERROR);

        return new self($status, [['Content-Type', 'application/json']], $encoded);
    }

    public static function text(string $body, int $status = 200): self
    {
        return new self($status, [['Content-Type', 'text/plain; charset=utf-8']], $body);
    }

    public static function html(string $body, int $status = 200): self
    {
        return new self($status, [['Content-Type', 'text/html; charset=utf-8']], $body);
    }

    public function withHeader(string $name, string $value): self
    {
        $this->headers[] = [$name, $value];
        return $this;
    }

    public function withStatus(int $status): self
    {
        $this->status = $status;
        return $this;
    }

    public function withBody(string $body): self
    {
        $this->body = $body;
        return $this;
    }
}
