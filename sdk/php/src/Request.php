<?php

declare(strict_types=1);

namespace Stokehold;

/**
 * A request as the server sent it.
 *
 * Deliberately a plain value object rather than PSR-7: the SDK is a transport,
 * not a framework, so it must not force a dependency on anyone's interfaces. An
 * application that wants PSR-7 builds one from this in a few lines (see
 * docs/worker-protocol.md and example/worker.php).
 */
final class Request
{
    /**
     * @param array<string, string> $headers header names as received, deduplicated by exact name
     * @param array<string, string> $server CGI-style server variables
     */
    public function __construct(
        public readonly string $method,
        public readonly string $uri,
        public readonly string $protocol,
        public readonly array $headers,
        public readonly string $remoteAddr,
        public readonly string $scriptFilename,
        public readonly bool $https,
        public readonly string $body,
        public readonly array $server = [],
    ) {
    }

    /**
     * Look up a header case-insensitively, the way HTTP requires.
     */
    public function header(string $name): ?string
    {
        foreach ($this->headers as $key => $value) {
            if (strcasecmp($key, $name) === 0) {
                return $value;
            }
        }

        return null;
    }

    public function contentType(): ?string
    {
        return $this->header('content-type');
    }

    /**
     * The value for $_SERVER, built the way the CGI environment would.
     *
     * Not a full CGI environment: the server passes only what the protocol
     * carries and the SDK derives the conventional names from it.
     *
     * @return array<string, string>
     */
    public function serverVars(): array
    {
        $server = [
            'REQUEST_METHOD' => $this->method,
            'REQUEST_URI' => $this->uri,
            'SERVER_PROTOCOL' => $this->protocol,
            'REMOTE_ADDR' => $this->remoteAddr,
            'SCRIPT_FILENAME' => $this->scriptFilename,
            'HTTPS' => $this->https ? 'on' : '',
            'CONTENT_LENGTH' => (string) strlen($this->body),
        ];

        $path = parse_url($this->uri, PHP_URL_PATH);
        $query = parse_url($this->uri, PHP_URL_QUERY);
        if (is_string($path)) {
            $server['PATH_INFO'] = $path;
        }
        if (is_string($query)) {
            $server['QUERY_STRING'] = $query;
        }

        foreach ($this->headers as $name => $value) {
            $key = 'HTTP_' . strtoupper(str_replace('-', '_', $name));
            $server[$key] = $value;
        }

        foreach ($this->server as $key => $value) {
            $server[$key] = $value;
        }

        return $server;
    }

    /**
     * The query string parsed the way PHP populates $_GET.
     *
     * @return array<string, mixed>
     */
    public function query(): array
    {
        $query = parse_url($this->uri, PHP_URL_QUERY);
        if (!is_string($query) || $query === '') {
            return [];
        }

        $params = [];
        parse_str($query, $params);

        return $params;
    }
}
