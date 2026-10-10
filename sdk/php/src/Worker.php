<?php

declare(strict_types=1);

namespace Stokehold;

/**
 * A persistent worker: the loop that serves requests over the protocol.
 *
 * This is the PHP end of docs/worker-protocol.md. The shape an application sees
 * is deliberately three lines:
 *
 * ```php
 * $worker = Worker::create();
 * $worker->run(function (Request $request, Worker $worker): Response {
 *     return Response::text('hello ' . $request->uri);
 * });
 * ```
 *
 * ## The two rules
 *
 * The protocol document names two rules that prevent the classic bugs, and both
 * are enforced here rather than merely documented:
 *
 * ### 1. stdout is the protocol channel
 *
 * Every frame this class writes goes to a handle on `php://stdout`. If the
 * application wrote there too — via `echo`, a warning, a notice — its output
 * would land *inside* a frame and desynchronise the stream for good.
 *
 * So the application's own output is not banned, it is **captured**:
 * `ob_start()` around each request collects `echo` and template output, and
 * [`ini_set`]`('display_errors', 'stderr')` in [`Worker::create`] sends
 * warnings and notices to stderr, which is not part of the protocol. Captured
 * output becomes the response body when the handler does not supply one, which
 * makes this a feature as much as a safeguard: an application that just echoes
 * works unchanged.
 *
 * The residual risk is a write made directly to the `STDOUT` handle
 * (`fwrite(STDOUT, …)`, `file_put_contents('php://stdout', …)`), which no
 * output buffer can intercept. That is not how PHP or any framework produces
 * output, and the consequence is a checksum mismatch that the server reports
 * clearly rather than a silent corruption — see [`Header`].
 *
 * ### 2. Graceful shutdown by construction
 *
 * When the server stops the worker it closes the child's stdin. This loop reads
 * that as EOF and returns from `run()`, so the worker exits on its own. There is
 * no signal to catch and no flag to poll, which is why the server only needs to
 * kill the process after a grace period rather than to ask nicely twice.
 */
final class Worker
{
    /** The largest body slice sent in one frame, matching MAX_BODY_CHUNK in Rust. */
    public const MAX_BODY_CHUNK = 65536;

    /** The request id reserved for unsolicited frames. */
    public const UNSOLICITED = 0;

    /** @var resource */
    private $in;

    /** @var resource */
    private $out;

    /** @var resource */
    private $err;

    private int $requestId = self::UNSOLICITED;

    /** The output-buffer nesting depth we found when the capture started. */
    private int $captureLevel = 0;

    private bool $keepAlive = true;

    private bool $stopping = false;

    /**
     * @param resource $in  the server's end: frames to read
     * @param resource $out stdout: frames to write
     * @param resource $err stderr: diagnostics, never part of the protocol
     */
    public function __construct($in, $out, $err)
    {
        $this->in = $in;
        $this->out = $out;
        $this->err = $err;
    }

    /**
     * Create a worker bound to the real stdio.
     *
     * `php://stdin` and `php://stdout` are opened as byte handles in binary
     * mode. Diagnostics go to the `STDERR` constant, which the protocol leaves
     * entirely free.
     *
     * `display_errors` is forced to `stderr` here so that a warning or notice
     * raised at any point during the worker's life cannot be printed into the
     * frame stream. That setting is rule 1's second half.
     */
    public static function create($in = null, $out = null): self
    {
        ini_set('display_errors', 'stderr');

        if ($in === null) {
            $in = fopen('php://stdin', 'rb');
        }
        if ($out === null) {
            $out = fopen('php://stdout', 'wb');
        }

        if ($in === false || $out === false) {
            throw new ProtocolException('could not open the stdio pipe');
        }

        stream_set_blocking($in, true);
        stream_set_blocking($out, true);

        /** @var resource $err */
        $err = defined('STDERR') ? STDERR : fopen('php://stderr', 'wb');

        return new self($in, $out, $err);
    }

    /**
     * Handshake, then serve requests until the server closes stdin.
     *
     * `$handler` receives the [`Request`] and this worker, and may return:
     *
     * - a [`Response`] — sent as given;
     * - a `string` — sent as a 200 `text/html` body;
     * - `null` — the captured output becomes the body (the `echo` case).
     *
     * Returning null is what makes a plain PHP script work as a worker: whatever
     * it echoed is the response.
     *
     * @param callable(Request, self): (Response|string|null) $handler
     */
    public function run(callable $handler): void
    {
        $this->handshake();

        while (!$this->stopping) {
            $header = $this->readHeader();

            if ($header === null) {
                // Clean EOF: the server closed our stdin, which is how a worker
                // is asked to stop. Not an error, and not worth a log line.
                break;
            }

            $payload = $this->readPayload($header);

            switch ($header->messageType) {
                case MessageType::REQUEST:
                    $this->serve($header, $payload, $handler);
                    break;

                case MessageType::PING:
                    $this->send(new Frame(MessageType::PONG, self::UNSOLICITED));
                    break;

                case MessageType::CONTROL:
                    $this->handleControl($payload);
                    break;

                case MessageType::LOG:
                case MessageType::METRIC:
                case MessageType::EVENT:
                    // The server should not send these, but a peer that echoes
                    // telemetry back must not derail the loop.
                    $this->diagnose('ignoring an unexpected ' . $header->typeName() . ' from the server');
                    break;

                default:
                    throw new ProtocolException(
                        'the server sent a ' . $header->typeName() . ', which a worker cannot serve'
                    );
            }
        }
    }

    /**
     * Serve one request: read its body, call the handler, write the response.
     *
     * @param callable(Request, self): (Response|string|null) $handler
     */
    private function serve(Header $header, string $payload, callable $handler): void
    {
        $this->requestId = $header->requestId;
        $this->keepAlive = true;

        $metadata = $this->decodeJson($payload, 'a REQUEST header');
        $body = $this->readRequestBody($header->requestId, $this->intField($metadata, 'body_len'));

        $started = hrtime(true);
        $this->beginCapture();

        try {
            $request = $this->buildRequest($metadata, $body);
            $result = $handler($request, $this);
            $captured = $this->endCapture();
            $response = $this->normalise($result, $captured);
            $appStatus = 0;
        } catch (\Throwable $exception) {
            // The handler left its buffer open or unwound ours, so recover
            // whatever is there before reporting the failure.
            $this->endCapture();

            $this->diagnose(sprintf(
                'uncaught %s: %s in %s:%d',
                $exception::class,
                $exception->getMessage(),
                $exception->getFile(),
                $exception->getLine(),
            ));

            $response = Response::text("Internal Server Error\n", 500);
            $appStatus = 1;
        }

        $durationUs = (int) ((hrtime(true) - $started) / 1000);

        $this->respond($response, $appStatus, $durationUs);
    }

    /**
     * Write a response: head, body chunks, then the terminal status.
     *
     * Private on purpose. The protocol puts `body_len` in the RESPONSE head, so
     * a response has to be complete before it starts — there is no way to stream
     * one. That makes "return a response" the only sane shape for a handler, and
     * a public `respond()` would only offer a way to send two of them. A second
     * head is a protocol error the server reports, so the footgun is removed
     * rather than documented.
     *
     * `body_len` is declared here and every byte is then sent, because the
     * server treats it as authoritative *and* requires the final body frame to
     * carry LAST. Declaring a length the body does not match is exactly the bug
     * those two checks exist to catch.
     */
    private function respond(Response $response, int $appStatus, int $durationUs): void
    {
        $body = $response->body;

        $this->sendJson(MessageType::RESPONSE, $this->requestId, [
            'status' => $response->status,
            'headers' => $response->headers,
            'body_len' => strlen($body),
        ]);

        if ($body !== '') {
            $chunks = str_split($body, self::MAX_BODY_CHUNK);
            $last = count($chunks) - 1;
            foreach ($chunks as $index => $chunk) {
                $this->send(new Frame(
                    MessageType::BODY,
                    $this->requestId,
                    $chunk,
                    $index === $last ? Header::FLAG_LAST : 0,
                ));
            }
        }

        $this->sendJson(MessageType::END, $this->requestId, [
            'app_status' => $appStatus,
            'peak_memory_bytes' => memory_get_peak_usage(true),
            'duration_us' => $durationUs ?? 0,
            'keep_alive' => $this->keepAlive,
        ]);

        if (!$this->keepAlive) {
            $this->stopping = true;
        }
    }

    /**
     * Emit a log record without touching stdout.
     *
     * The protocol exists so a worker can say something *outside* a request;
     * writing the line to stdout instead would corrupt the stream, so this is
     * the supported way.
     */
    public function emitLog(string $message, string $level = 'info'): void
    {
        $this->sendJson(MessageType::LOG, self::UNSOLICITED, [
            'level' => $level,
            'message' => $message,
            'request_id' => $this->requestId,
        ]);
    }

    /**
     * Emit a metric sample without touching stdout.
     */
    public function emitMetric(string $name, float $value, string $unit = ''): void
    {
        $this->sendJson(MessageType::METRIC, self::UNSOLICITED, [
            'name' => $name,
            'value' => $value,
            'unit' => $unit,
        ]);
    }

    /**
     * Ask the server to replace this worker after the current request.
     *
     * For an application that has put itself into a state it cannot clean up.
     * The server sees `keep_alive = false` in END and recycles instead of
     * reusing.
     */
    public function recycle(): void
    {
        $this->keepAlive = false;
    }

    /**
     * The request currently being served, for telemetry correlation.
     */
    public function currentRequestId(): int
    {
        return $this->requestId;
    }

    // ---------------------------------------------------------------- handshake

    /**
     * Exchange HELLO for HELLO_ACK.
     *
     * Tolerant about what arrives before and after the HELLO, but strict about
     * the version: a mismatched SDK is a configuration problem, and it should be
     * reported at boot rather than on the first request.
     */
    private function handshake(): void
    {
        while (true) {
            $header = $this->readHeader();
            if ($header === null) {
                throw new ProtocolException('the server closed the pipe before the handshake');
            }

            $payload = $this->readPayload($header);

            if ($header->messageType === MessageType::HELLO) {
                if ($header->payloadLen > 0) {
                    $hello = $this->decodeJson($payload, 'a HELLO');
                    $version = $this->intField($hello, 'protocol_version', Header::PROTOCOL_VERSION);
                    if ($version !== Header::PROTOCOL_VERSION) {
                        throw new ProtocolException(sprintf(
                            'the server speaks protocol version %d, this SDK speaks %d',
                            $version,
                            Header::PROTOCOL_VERSION,
                        ));
                    }
                }
                break;
            }

            if ($header->messageType === MessageType::LOG
                || $header->messageType === MessageType::METRIC
                || $header->messageType === MessageType::EVENT
            ) {
                continue;
            }

            throw new ProtocolException(
                'expected a HELLO to begin the handshake, got a ' . $header->typeName()
            );
        }

        $this->sendJson(MessageType::HELLO_ACK, self::UNSOLICITED, [
            'protocol_version' => Header::PROTOCOL_VERSION,
            'php_version' => PHP_VERSION,
            'sapi' => PHP_SAPI,
            'pid' => (int) getmypid(),
            'runtime' => 'stokehold-worker-php',
        ]);
    }

    /**
     * Act on a CONTROL frame, acknowledging it either way.
     */
    private function handleControl(string $payload): void
    {
        $control = $this->decodeJson($payload, 'a CONTROL');
        $kind = is_string($control['kind'] ?? null) ? $control['kind'] : '';

        $this->send(new Frame(MessageType::CONTROL_ACK, self::UNSOLICITED));

        switch ($kind) {
            case 'drain':
                // Stop accepting work and exit once idle. We are idle between
                // requests, so this is immediate — and the server has already
                // been told, so it will not send another request.
                $this->stopping = true;
                break;

            case 'shutdown':
                // No END is written: END is request-scoped, and a control frame
                // only ever arrives between requests. The CONTROL_ACK is the
                // whole answer — the server already knows it asked us to stop.
                $this->stopping = true;
                break;

            case 'report':
                $this->emitMetric('worker.peak_memory_bytes', (float) memory_get_peak_usage(true), 'bytes');
                break;

            case 'reload':
                // A worker has no separate bootstrap to re-run: the handler is
                // the application, and it is already loaded. Acknowledged so the
                // server's state machine completes; the server recycles the
                // worker if it needs the code reloaded.
                $this->diagnose('reload acknowledged: this worker has no separate bootstrap');
                break;

            default:
                $this->diagnose("unknown control kind '{$kind}' acknowledged and ignored");
        }
    }

    // ------------------------------------------------------------ request side

    /**
     * Read the request body from the BODY frames that follow a REQUEST.
     *
     * Stops at `body_len` bytes, because the declared length is authoritative.
     * A frame flagged LAST that arrives early means the sender and this reader
     * disagree about the length, which is worth failing on rather than papering
     * over: the rest of the stream cannot be trusted after that.
     */
    private function readRequestBody(int $requestId, int $bodyLen): string
    {
        if ($bodyLen === 0) {
            return '';
        }

        $body = '';
        while (strlen($body) < $bodyLen) {
            $header = $this->readHeader();
            if ($header === null) {
                throw new ProtocolException(sprintf(
                    'the server closed the pipe after %d of %d body bytes',
                    strlen($body),
                    $bodyLen,
                ));
            }

            $payload = $this->readPayload($header);

            if ($header->messageType !== MessageType::BODY) {
                throw new ProtocolException(
                    'expected a BODY while reading the request body, got a ' . $header->typeName()
                );
            }

            if ($header->requestId !== $requestId) {
                throw new ProtocolException(sprintf(
                    'a BODY for request %d arrived while reading request %d',
                    $header->requestId,
                    $requestId,
                ));
            }

            $body .= $payload;

            if ($header->isLast() && strlen($body) !== $bodyLen) {
                throw new ProtocolException(sprintf(
                    'the last BODY frame arrived after %d of %d declared bytes',
                    strlen($body),
                    $bodyLen,
                ));
            }
        }

        return $body;
    }

    /**
     * Build the [`Request`] from the REQUEST metadata.
     *
     * @param array<string, mixed> $metadata
     */
    private function buildRequest(array $metadata, string $body): Request
    {
        /** @var list<array{0: string, 1: string}> $rawHeaders */
        $rawHeaders = [];
        if (isset($metadata['headers']) && is_array($metadata['headers'])) {
            foreach ($metadata['headers'] as $pair) {
                if (is_array($pair) && count($pair) === 2) {
                    $rawHeaders[] = [(string) $pair[0], (string) $pair[1]];
                }
            }
        }

        return new Request(
            method: $this->stringField($metadata, 'method', 'GET'),
            uri: $this->stringField($metadata, 'uri', '/'),
            protocol: $this->stringField($metadata, 'protocol', 'HTTP/1.1'),
            headers: $this->headersToMap($rawHeaders),
            remoteAddr: $this->stringField($metadata, 'remote_addr'),
            scriptFilename: $this->stringField($metadata, 'script_filename'),
            https: (bool) ($metadata['https'] ?? false),
            body: $body,
        );
    }

    /**
     * Collapse `[['Accept', 'text/html'], …]` into a name → value map.
     *
     * Repeated names are joined with a comma, which is what HTTP says a repeated
     * field means. (`Set-Cookie` is the famous exception, and it only matters in
     * the response direction, where [`Response`] keeps the list form.)
     *
     * @param list<array{0: string, 1: string}> $pairs
     * @return array<string, string>
     */
    private function headersToMap(array $pairs): array
    {
        $map = [];
        foreach ($pairs as [$name, $value]) {
            if (isset($map[$name])) {
                $map[$name] .= ', ' . $value;
            } else {
                $map[$name] = $value;
            }
        }

        return $map;
    }

    /**
     * Turn whatever the handler returned into a [`Response`].
     */
    private function normalise(mixed $result, string $captured): Response
    {
        if ($result instanceof Response) {
            return $result;
        }

        if (is_string($result)) {
            return Response::html($result);
        }

        if ($result === null) {
            // The `echo` case: the application's own output is the body.
            return Response::html($captured);
        }

        throw new ProtocolException(
            'a worker handler must return a Response, a string or null, got ' . get_debug_type($result)
        );
    }

    // ----------------------------------------------------------- output capture

    /**
     * Start capturing the application's output.
     *
     * The current buffer depth is recorded so [`Worker::endCapture`] can tell
     * an inner buffer the handler forgot to close from our own, and can still
     * recover if the handler unwound ours.
     */
    private function beginCapture(): void
    {
        $this->captureLevel = ob_get_level();
        ob_start();
    }

    /**
     * Stop capturing and return what the application printed.
     *
     * Written defensively because a handler can leave buffers unbalanced. The
     * goal is that one badly-behaved request cannot corrupt the frame stream or
     * leak its output into the next request's response — not to punish the
     * handler, which will see its own exception if it does.
     */
    private function endCapture(): string
    {
        // Close any buffers the handler opened and did not close.
        while (ob_get_level() > $this->captureLevel + 1) {
            ob_end_clean();
        }

        if (ob_get_level() > $this->captureLevel) {
            $captured = ob_get_clean();

            return is_string($captured) ? $captured : '';
        }

        // Our buffer is already gone: the handler ended it, so the output is
        // lost and cannot be recovered. Not worth failing the request over.
        return '';
    }

    // ------------------------------------------------------------------- codec

    /**
     * Read one frame header, or null at a clean end of stream.
     *
     * `stream_get_contents($h, 16)` is doing the work here, and that is
     * deliberate: it blocks until it has 16 bytes or the stream ends, handling
     * partial reads itself. Hand-rolling an `fread` loop is the bug-prone part
     * of every pipe protocol, and this call is why the SDK does not have one.
     */
    private function readHeader(): ?Header
    {
        $bytes = stream_get_contents($this->in, Header::HEADER_LEN);

        if ($bytes === false || $bytes === '') {
            return null;
        }

        return Header::decode($bytes);
    }

    /**
     * Read a frame's payload and verify its checksum.
     *
     * A mismatch means framing has already desynchronised, so this is fatal by
     * construction: the exception propagates out of `run()` and the process
     * exits, rather than trying to resynchronise a stream that cannot be
     * resynchronised.
     */
    private function readPayload(Header $header): string
    {
        if ($header->payloadLen === 0) {
            return '';
        }

        $payload = stream_get_contents($this->in, $header->payloadLen);

        if ($payload === false || strlen($payload) !== $header->payloadLen) {
            throw new ProtocolException(sprintf(
                'a frame declared %d payload bytes but the stream ended after %d',
                $header->payloadLen,
                $payload === false ? 0 : strlen($payload),
            ));
        }

        $computed = Frame::crc32c($payload);
        if ($computed !== $header->checksum) {
            throw new ProtocolException(sprintf(
                'payload checksum mismatch on request %d: expected 0x%08x, computed 0x%08x',
                $header->requestId,
                $header->checksum,
                $computed,
            ));
        }

        return $payload;
    }

    /**
     * Write one frame, in full.
     *
     * `fwrite` may write fewer bytes than it was given, so the loop is not
     * paranoia: a short write on a pipe would leave half a frame on the wire and
     * desynchronise the server permanently.
     */
    private function send(Frame $frame): void
    {
        $bytes = $frame->encode();
        $length = strlen($bytes);
        $offset = 0;

        while ($offset < $length) {
            $written = fwrite($this->out, substr($bytes, $offset));
            if ($written === false || $written === 0) {
                throw new ProtocolException('the server closed its input while a frame was being written');
            }
            $offset += $written;
        }

        fflush($this->out);
    }

    /**
     * Serialize a payload as JSON and send it.
     */
    private function sendJson(int $messageType, int $requestId, mixed $value): void
    {
        $encoded = json_encode($value, JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES);

        $this->send(new Frame($messageType, $requestId, $encoded));
    }

    /**
     * Decode a JSON payload, turning a bad payload into a protocol error.
     *
     * @return array<string, mixed>
     */
    private function decodeJson(string $payload, string $what): array
    {
        if ($payload === '') {
            return [];
        }

        try {
            /** @var mixed $decoded */
            $decoded = json_decode($payload, true, 512, JSON_THROW_ON_ERROR);
        } catch (\JsonException $exception) {
            throw new ProtocolException(
                "{$what} was not valid JSON: {$exception->getMessage()}"
            );
        }

        if (!is_array($decoded)) {
            throw new ProtocolException("{$what} must be a JSON object");
        }

        /** @var array<string, mixed> $decoded */
        return $decoded;
    }

    /**
     * @param array<string, mixed> $data
     */
    private function intField(array $data, string $key, int $default = 0): int
    {
        $value = $data[$key] ?? $default;

        return is_numeric($value) ? (int) $value : $default;
    }

    /**
     * @param array<string, mixed> $data
     */
    private function stringField(array $data, string $key, string $default = ''): string
    {
        $value = $data[$key] ?? $default;

        return is_string($value) ? $value : $default;
    }

    /**
     * Write a diagnostic line to stderr.
     *
     * Never stdout: a line there would land inside a frame.
     */
    private function diagnose(string $message): void
    {
        fwrite($this->err, '[stokehold] ' . $message . "\n");
        fflush($this->err);
    }
}
