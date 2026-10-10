<?php

declare(strict_types=1);

/**
 * Worker behaviour, driven without a subprocess.
 *
 * The [`Worker`] is constructed over in-memory streams, so a test can write the
 * exact frames a server would send and then inspect the exact frames the worker
 * wrote. These are the fast, precise tests; the process-level ones live in the
 * Rust integration test, where a real pipe and a real server are involved.
 *
 * The tests that matter most assert on the *raw bytes* rather than on parsed
 * frames — `echo inside a handler never reaches the frame stream` and the warning
 * test — because the failure they guard against is application output landing
 * inside a frame, which by definition would make parsing the frames the wrong
 * thing to do.
 */

require __DIR__ . '/../autoload.php';
require __DIR__ . '/Assert.php';

use Stokehold\Frame;
use Stokehold\Header;
use Stokehold\MessageType;
use Stokehold\Request;
use Stokehold\Response;
use Stokehold\Worker;

/**
 * Encode a sequence of `[type, request_id, payload_or_array, flags]` frames into
 * a stream positioned at the start.
 *
 * @param list<array{0: int, 1: int, 2?: mixed, 3?: int}> $frames
 * @return resource
 */
function server_stream(array $frames)
{
    $bytes = '';
    foreach ($frames as $frame) {
        $payload = $frame[2] ?? '';
        if (is_array($payload)) {
            $payload = json_encode($payload, JSON_UNESCAPED_SLASHES | JSON_THROW_ON_ERROR);
        }
        $bytes .= (new Frame($frame[0], $frame[1], (string) $payload, $frame[3] ?? 0))->encode();
    }

    $stream = fopen('php://memory', 'r+b');
    fwrite($stream, $bytes);
    rewind($stream);

    return $stream;
}

/** @return resource */
function sink()
{
    return fopen('php://memory', 'r+b');
}

/** Read everything written to a memory stream. */
function drained($stream): string
{
    rewind($stream);
    $bytes = stream_get_contents($stream);

    return $bytes === false ? '' : $bytes;
}

/**
 * Decode every frame in a byte string, verifying each checksum.
 *
 * @return list<array{header: Header, payload: string}>
 */
function parse_frames(string $bytes): array
{
    $frames = [];
    $offset = 0;
    $length = strlen($bytes);

    while ($offset < $length) {
        $header = Header::decode(substr($bytes, $offset, Header::HEADER_LEN));
        $offset += Header::HEADER_LEN;
        $payload = substr($bytes, $offset, $header->payloadLen);
        $offset += $header->payloadLen;

        if (Frame::crc32c($payload) !== $header->checksum) {
            throw new RuntimeException('the worker wrote a frame with a bad checksum');
        }

        $frames[] = ['header' => $header, 'payload' => $payload];
    }

    return $frames;
}

/**
 * The **raw** payloads of the frames with a given type.
 *
 * Raw, not decoded: a BODY payload is application bytes and need not be JSON, so
 * decoding here would destroy exactly what the interesting assertions look at.
 *
 * @param list<array{header: Header, payload: string}> $frames
 * @return list<string>
 */
function payloads_of(array $frames, int $type): array
{
    $out = [];
    foreach ($frames as $frame) {
        if ($frame['header']->messageType === $type) {
            $out[] = $frame['payload'];
        }
    }

    return $out;
}

/**
 * The decoded JSON payloads of the frames with a given type.
 *
 * @param list<array{header: Header, payload: string}> $frames
 * @return list<array<string, mixed>>
 */
function json_of(array $frames, int $type): array
{
    return array_map(
        static fn (string $payload): array => json_decode($payload, true, 512, JSON_THROW_ON_ERROR),
        payloads_of($frames, $type),
    );
}

/**
 * Run a worker to completion over in-memory streams.
 *
 * @param list<array{0: int, 1: int, 2?: mixed, 3?: int}> $frames the server's frames
 * @param callable(Request, Worker): mixed $handler
 * @return array{frames: list<array{header: Header, payload: string}>, raw: string, stderr: string}
 */
function run_worker(array $frames, callable $handler): array
{
    $in = server_stream($frames);
    $out = sink();
    $err = sink();

    (new Worker($in, $out, $err))->run($handler);

    $raw = drained($out);

    return [
        'frames' => parse_frames($raw),
        'raw' => $raw,
        'stderr' => drained($err),
    ];
}

/** A HELLO, as the server sends first. */
const A_HELLO = [MessageType::HELLO, 0];

/** A GET request, with no body. */
function a_get(string $uri = '/', int $id = 1): array
{
    return [[MessageType::REQUEST, $id, [
        'method' => 'GET',
        'uri' => $uri,
        'protocol' => 'HTTP/1.1',
        'headers' => [['Accept', 'text/html'], ['Host', 'example.test']],
        'remote_addr' => '127.0.0.1',
        'script_filename' => 'index.php',
        'body_len' => 0,
        'https' => false,
    ]]];
}

// ---------------------------------------------------------------- handshake

$handshake = run_worker([A_HELLO], static fn () => Response::text('unused'));
$acks = json_of($handshake['frames'], MessageType::HELLO_ACK);
Assert::same(1, count($acks), 'the worker answers a HELLO with exactly one HELLO_ACK');
Assert::same(1, $acks[0]['protocol_version'], 'the HELLO_ACK declares protocol version 1');
Assert::same(PHP_VERSION, $acks[0]['php_version'], 'the HELLO_ACK reports PHP_VERSION');
Assert::same(PHP_SAPI, $acks[0]['sapi'], 'the HELLO_ACK reports PHP_SAPI');
Assert::true($acks[0]['pid'] > 0, 'the HELLO_ACK reports a real pid');
Assert::same(0, $handshake['frames'][0]['header']->requestId, 'the HELLO_ACK is unsolicited (request id 0)');

// A mismatched server version is refused at boot, not on the first request.
Assert::throws(
    static function (): void {
        $in = server_stream([[MessageType::HELLO, 0, ['protocol_version' => 9]]]);
        (new Worker($in, sink(), sink()))->run(static fn () => Response::text('x'));
    },
    'a server speaking an unknown protocol version is refused at the handshake',
    'protocol version 9',
);

// --------------------------------------------------------- a simple request

$result = run_worker(
    array_merge([A_HELLO], a_get('/hello')),
    static fn (Request $request) => Response::text('hi ' . $request->uri),
);

$responseHeads = json_of($result['frames'], MessageType::RESPONSE);
$ends = json_of($result['frames'], MessageType::END);

Assert::same(1, count($responseHeads), 'one RESPONSE is written');
Assert::same(200, $responseHeads[0]['status'], 'the status is 200');
Assert::same(9, $responseHeads[0]['body_len'], 'the RESPONSE declares the real body length');
Assert::same(1, count($ends), 'one END terminates the request');
Assert::true($ends[0]['keep_alive'], 'the worker stays alive by default');
Assert::same(0, $ends[0]['app_status'], 'a successful request reports app_status 0');
Assert::same(['hi /hello'], payloads_of($result['frames'], MessageType::BODY), 'the body arrives intact');

foreach ($result['frames'] as $frame) {
    if (in_array($frame['header']->messageType, [MessageType::RESPONSE, MessageType::BODY, MessageType::END], true)) {
        Assert::same(1, $frame['header']->requestId, 'every correlated frame carries the request id');
    }
}
$bodyFrames = array_values(array_filter(
    $result['frames'],
    static fn ($f) => $f['header']->messageType === MessageType::BODY,
));
Assert::true($bodyFrames[0]['header']->isLast(), 'the final body frame carries LAST');

// ---------------------------------------------------------- an empty body

$empty = run_worker(
    array_merge([A_HELLO], a_get('/empty')),
    static fn () => new Response(204, [], ''),
);
Assert::same(
    [MessageType::HELLO_ACK, MessageType::RESPONSE, MessageType::END],
    array_map(static fn ($f) => $f['header']->messageType, $empty['frames']),
    'an empty body sends no BODY frame at all',
);

// --------------------------------------------------- rule 1: output capture

// The critical one. A handler that echoes *and* returns a Response must not leak
// its echo into the frame stream.
$leaky = run_worker(
    array_merge([A_HELLO], a_get('/leak')),
    static function (): Response {
        echo 'LEAKED-MARKER';

        return Response::text('the real body');
    },
);
Assert::true(
    !str_contains($leaky['raw'], 'LEAKED-MARKER'),
    'echo inside a handler never reaches the frame stream',
);
Assert::same(
    ['the real body'],
    payloads_of($leaky['frames'], MessageType::BODY),
    'the returned Response body is what is sent',
);

// The echo case: returning null makes the captured output the body, which is what
// lets a plain PHP script run unchanged as a worker.
$echoed = run_worker(
    array_merge([A_HELLO], a_get('/echo')),
    static function () {
        echo 'printed by the application';

        return null;
    },
);
Assert::same(
    ['printed by the application'],
    payloads_of($echoed['frames'], MessageType::BODY),
    'a null return uses the captured output as the body',
);
Assert::same(
    strlen('printed by the application'),
    json_of($echoed['frames'], MessageType::RESPONSE)[0]['body_len'],
    'the declared body_len matches the captured output',
);

// The second half of rule 1: a warning must not be captured into the body either.
// `display_errors=stderr` is what makes that hold, and `Worker::create` sets it,
// so mirror it here rather than trusting the test process's own setting.
//
// Running this test prints `WARNING-MARKER` on stderr. That is the mechanism
// working, not a failure: the warning went to stderr instead of into a frame.
$previousDisplayErrors = ini_get('display_errors');
ini_set('display_errors', 'stderr');

$warned = run_worker(
    array_merge([A_HELLO], a_get('/warn')),
    static function () {
        trigger_error('WARNING-MARKER', E_USER_WARNING);
        echo 'body after the warning';

        return null;
    },
);

ini_set('display_errors', is_string($previousDisplayErrors) ? $previousDisplayErrors : '1');

Assert::true(
    !str_contains($warned['raw'], 'WARNING-MARKER'),
    'a warning never reaches the frame stream',
);
Assert::same(
    ['body after the warning'],
    payloads_of($warned['frames'], MessageType::BODY),
    'a warning is not captured into the response body',
);

// ------------------------------------------------------- a failing handler

// A handler exception becomes a 500 and the worker *keeps serving*: one bad
// request must not take the worker down.
$recovered = run_worker(
    array_merge([A_HELLO], a_get('/boom', 1), a_get('/fine', 2)),
    static function (Request $request): Response {
        if ($request->uri === '/boom') {
            throw new RuntimeException('handler exploded');
        }

        return Response::text('still here');
    },
);

$recoveredHeads = json_of($recovered['frames'], MessageType::RESPONSE);
$recoveredEnds = json_of($recovered['frames'], MessageType::END);
Assert::same(2, count($recoveredHeads), 'both requests are answered');
Assert::same(500, $recoveredHeads[0]['status'], 'the failing request becomes a 500');
Assert::same(1, $recoveredEnds[0]['app_status'], 'the failing request reports app_status 1');
Assert::same(200, $recoveredHeads[1]['status'], 'the next request is served normally');
Assert::same(0, $recoveredEnds[1]['app_status'], 'the second request reports app_status 0');
Assert::true($recoveredEnds[1]['keep_alive'], 'the worker does not ask to be replaced after a handler error');

// ------------------------------------------- a handler that leaks a buffer

// A handler can leave an output buffer open. The stream must still be aligned,
// and this request's leftovers must not become the next request's body.
$unbalanced = run_worker(
    array_merge([A_HELLO], a_get('/one', 1), a_get('/two', 2)),
    static function (Request $request) {
        if ($request->uri === '/one') {
            ob_start();
            echo 'leftover from request one';

            return Response::text('one done');
        }

        echo 'two';

        return null;
    },
);
$unbalancedBodies = payloads_of($unbalanced['frames'], MessageType::BODY);
Assert::true(
    !in_array('leftover from request one', $unbalancedBodies, true),
    'a leaked buffer never becomes a response body',
);
Assert::same(['one done', 'two'], $unbalancedBodies, 'both requests produce exactly their own bodies');

// ------------------------------------------------------------- body chunking

$big = str_repeat('abcdefgh', 70000); // 560000 bytes: nine chunks of 65536
$chunked = run_worker(array_merge([A_HELLO], a_get('/big')), static fn () => Response::text($big));

$chunkFrames = array_values(array_filter(
    $chunked['frames'],
    static fn ($f) => $f['header']->messageType === MessageType::BODY,
));
Assert::same(
    (int) ceil(strlen($big) / Worker::MAX_BODY_CHUNK),
    count($chunkFrames),
    'a large body is split into MAX_BODY_CHUNK chunks',
);
$reassembled = '';
foreach ($chunkFrames as $index => $frame) {
    $reassembled .= $frame['payload'];
    if ($index < count($chunkFrames) - 1) {
        Assert::true(!$frame['header']->isLast(), 'only the final chunk carries LAST');
    }
}
Assert::true($chunkFrames[count($chunkFrames) - 1]['header']->isLast(), 'the final chunk carries LAST');
Assert::same($big, $reassembled, 'the chunks reassemble to the original body');
Assert::same(
    strlen($big),
    json_of($chunked['frames'], MessageType::RESPONSE)[0]['body_len'],
    'body_len matches the chunked body',
);

// ----------------------------------------------------------- request bodies

$withBody = run_worker(
    array_merge(
        [A_HELLO],
        [[MessageType::REQUEST, 1, [
            'method' => 'POST',
            'uri' => '/submit?x=1',
            'protocol' => 'HTTP/1.1',
            'headers' => [['Content-Type', 'application/json'], ['Host', 'example.test']],
            'remote_addr' => '127.0.0.1',
            'body_len' => 7, // '{"a":1}'
            'script_filename' => 'index.php',
            'https' => false,
        ]]],
        [[MessageType::BODY, 1, '{"a":', 0]],
        [[MessageType::BODY, 1, '1}', Header::FLAG_LAST]],
    ),
    static fn (Request $request) => Response::json([
        'body' => $request->body,
        'content_type' => $request->contentType(),
        'query' => $request->query(),
        'method' => $request->method,
        'host' => $request->serverVars()['HTTP_HOST'] ?? 'missing',
    ]),
);
$withBodyPayload = json_decode(
    payloads_of($withBody['frames'], MessageType::BODY)[0],
    true,
    512,
    JSON_THROW_ON_ERROR,
);
Assert::same('{"a":1}', $withBodyPayload['body'], 'a body split across frames is reassembled');
Assert::same('application/json', $withBodyPayload['content_type'], 'headers are readable case-insensitively');
Assert::same(['x' => '1'], $withBodyPayload['query'], 'the query string is parsed');
Assert::same('POST', $withBodyPayload['method'], 'the method is carried through');
Assert::same('example.test', $withBodyPayload['host'], 'request headers become HTTP_* server variables');

// A body that overshoots its declared length is refused: the declared length is
// authoritative, and exceeding it means the two ends disagree about the framing.
Assert::throws(
    static function (): void {
        run_worker(
            array_merge(
                [A_HELLO],
                [[MessageType::REQUEST, 1, [
                    'method' => 'POST', 'uri' => '/', 'protocol' => 'HTTP/1.1',
                    'headers' => [], 'remote_addr' => '', 'body_len' => 5,
                    'script_filename' => '', 'https' => false,
                ]]],
                [[MessageType::BODY, 1, 'toolong', Header::FLAG_LAST]],
            ),
            static fn () => Response::text('x'),
        );
    },
    'a LAST body frame that overshoots the declared length is refused',
    'declared bytes',
);

// -------------------------------------------------------------- ping/control

$pinged = run_worker([A_HELLO, [MessageType::PING, 0]], static fn () => Response::text('x'));
Assert::same(1, count(payloads_of($pinged['frames'], MessageType::PONG)), 'a PING is answered with a PONG');
Assert::same(0, count(payloads_of($pinged['frames'], MessageType::RESPONSE)), 'a PING is not a request');

$drainedWorker = run_worker(
    [A_HELLO, [MessageType::CONTROL, 0, ['kind' => 'drain']]],
    static fn () => Response::text('x'),
);
Assert::same(
    [MessageType::HELLO_ACK, MessageType::CONTROL_ACK],
    array_map(static fn ($f) => $f['header']->messageType, $drainedWorker['frames']),
    'a drain is acknowledged and then the worker stops',
);

$shut = run_worker([A_HELLO, [MessageType::CONTROL, 0, ['kind' => 'shutdown']]], static fn () => Response::text('x'));
Assert::same(
    [MessageType::HELLO_ACK, MessageType::CONTROL_ACK],
    array_map(static fn ($f) => $f['header']->messageType, $shut['frames']),
    'a shutdown is acknowledged and then the worker stops',
);
Assert::same(
    0,
    count(payloads_of($shut['frames'], MessageType::END)),
    'a shutdown writes no END: END is request-scoped and no request was in flight',
);

// A clean EOF — the server closing stdin — ends the loop without an error. This
// is rule 2: shutdown by construction, with no signal to catch.
$eof = run_worker([A_HELLO], static fn () => Response::text('x'));
Assert::same(1, count(payloads_of($eof['frames'], MessageType::HELLO_ACK)), 'a clean EOF is not an error');

// A report is asked for as a control and answered with a metric, not a request.
$reported = run_worker(
    [A_HELLO, [MessageType::CONTROL, 0, ['kind' => 'report']]],
    static fn () => Response::text('x'),
);
$reportedMetrics = json_of($reported['frames'], MessageType::METRIC);
Assert::same(1, count($reportedMetrics), 'a report answers with one metric');
Assert::same('worker.peak_memory_bytes', $reportedMetrics[0]['name'], 'the report names its metric');

// ------------------------------------------------------------- stream errors

// A truncated payload is a framing failure, not a clean close.
Assert::throws(
    static function (): void {
        $in = server_stream([A_HELLO, [MessageType::REQUEST, 1, ['method' => 'GET', 'uri' => '/', 'body_len' => 0]]]);
        $bytes = drained($in);
        $cut = fopen('php://memory', 'r+b');
        fwrite($cut, substr($bytes, 0, strlen($bytes) - 5));
        rewind($cut);
        (new Worker($cut, sink(), sink()))->run(static fn () => Response::text('x'));
    },
    'a stream that ends mid-payload is a protocol error',
    'payload bytes',
);

// A tampered payload fails the checksum. This is the check that turns "output
// leaked into the stream" into a clear error rather than a silent corruption.
Assert::throws(
    static function (): void {
        $bytes = (new Frame(MessageType::HELLO, 0))->encode()
            . (new Frame(MessageType::REQUEST, 1, '{"method":"GET","uri":"/","body_len":0}'))->encode();
        $bytes[strlen($bytes) - 3] = "\x00";
        $stream = fopen('php://memory', 'r+b');
        fwrite($stream, $bytes);
        rewind($stream);
        (new Worker($stream, sink(), sink()))->run(static fn () => Response::text('x'));
    },
    'a corrupted payload fails the checksum',
    'checksum mismatch',
);

// The server sending something only a worker may send is a protocol error.
Assert::throws(
    static function (): void {
        run_worker([A_HELLO, [MessageType::RESPONSE, 1, ['status' => 200]]], static fn () => Response::text('x'));
    },
    'a RESPONSE from the server is refused',
    'cannot serve',
);

// ------------------------------------------------------- the create() factory

$savedDisplayErrors = ini_get('display_errors');
Worker::create(server_stream([A_HELLO]), sink());
Assert::same(
    'stderr',
    ini_get('display_errors'),
    'Worker::create sends diagnostics to stderr, never into the frame stream',
);
ini_set('display_errors', is_string($savedDisplayErrors) ? $savedDisplayErrors : '1');

// -------------------------------------------------------------- recycle()

$recycled = run_worker(
    array_merge([A_HELLO], a_get('/recycle')),
    static function (Request $request, Worker $worker): Response {
        $worker->recycle();

        return Response::text('done');
    },
);
Assert::true(
    !json_of($recycled['frames'], MessageType::END)[0]['keep_alive'],
    'recycle() reports keep_alive=false so the server replaces the worker',
);

// Telemetry goes out unsolicited, at request id 0, never inside a response.
$telemetry = run_worker(
    array_merge([A_HELLO], a_get('/log')),
    static function (Request $request, Worker $worker): Response {
        $worker->emitLog('hello from the handler');
        $worker->emitMetric('handler.count', 1.0, 'count');

        return Response::text('ok');
    },
);
$logFrames = array_values(array_filter(
    $telemetry['frames'],
    static fn ($f) => $f['header']->messageType === MessageType::LOG,
));
Assert::same(1, count($logFrames), 'a log record is emitted');
Assert::same(0, $logFrames[0]['header']->requestId, 'a log record is unsolicited (request id 0)');
$decodedLog = json_decode($logFrames[0]['payload'], true, 512, JSON_THROW_ON_ERROR);
Assert::same('hello from the handler', $decodedLog['message'], 'the log message survives');
Assert::same(1, $decodedLog['request_id'], 'the log record records which request it belongs to');

// ------------------------------------------------------- peak memory reporting

// `memory_get_peak_usage(true)` is a high-water mark that begins at process
// start, so a worker that peaked once would report a figure above a configured
// `memory_limit_mb` on every later request, and the server would recycle it every
// time. The END figure is therefore read and then re-baselined, which makes it a
// per-request measurement.
$twoRequests = run_worker(
    array_merge([A_HELLO], a_get('/first', 1), a_get('/second', 2)),
    static function (Request $request): Response {
        if ($request->uri === '/first') {
            // Big enough to move `memory_get_peak_usage(true)` on any version,
            // and freed before the request ends, so "usage now" is unchanged.
            $spike = str_repeat('x', 16 * 1024 * 1024);
            unset($spike);
        }

        return Response::text('ok');
    },
);

$peakEnds = json_of($twoRequests['frames'], MessageType::END);
Assert::same(2, count($peakEnds), 'two requests produce two END frames');
Assert::true(
    $peakEnds[0]['peak_memory_bytes'] > 0,
    'a worker reports a positive peak resident set',
);

if (\function_exists('memory_reset_peak_usage')) {
    Assert::true(
        $peakEnds[1]['peak_memory_bytes'] < $peakEnds[0]['peak_memory_bytes'],
        'the peak is re-baselined after each request, so one spike does not follow the worker for ever',
    );
} else {
    Assert::same(
        $peakEnds[0]['peak_memory_bytes'],
        $peakEnds[1]['peak_memory_bytes'],
        'without memory_reset_peak_usage (PHP < 8.2) the reported figure stays monotonic',
    );
}

// The `report` control reads the same figure and resets it too, so an operator
// sampling between requests cannot resurrect a spike the last request cleared.
$afterReport = run_worker(
    array_merge([A_HELLO], a_get('/spike', 3), [[MessageType::CONTROL, 0, ['kind' => 'report']]]),
    static function (Request $request): Response {
        $spike = str_repeat('y', 16 * 1024 * 1024);
        unset($spike);

        return Response::text('ok');
    },
);
$spikeEnd = json_of($afterReport['frames'], MessageType::END)[0]['peak_memory_bytes'];
$reported = json_of($afterReport['frames'], MessageType::METRIC);
Assert::same(1, count($reported), 'a report answers with one metric');
if (\function_exists('memory_reset_peak_usage')) {
    Assert::true(
        $reported[0]['value'] < $spikeEnd,
        'a report after a large request measures the baseline, not the cleared spike',
    );
} else {
    Assert::same((float) $spikeEnd, $reported[0]['value'], 'without a reset the report repeats the peak');
}

exit(Assert::report('worker behaviour'));
