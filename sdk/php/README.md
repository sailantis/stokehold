# stokehold-worker — the PHP SDK

The PHP end of the [stokehold worker protocol](../../docs/worker-protocol.md): a persistent PHP
process that boots your application once and serves many requests over stdin/stdout.

It is **stdlib-only**. No extensions, no FFI, no build step, and no Composer requirement just to
*run* a worker. The whole SDK is four small classes and a codec.

## What it is not

It is a **transport, not a framework**. There is no router, no container, no middleware pipeline,
and no PSR-7 dependency — because forcing one would mean this package could not be used from the
many applications that use a different one. `Request` and `Response` are plain value objects;
adapting them to whatever your application already speaks is a few lines.

## Quick start

```php
<?php
require __DIR__ . '/vendor/autoload.php';   // or: require 'path/to/sdk/php/autoload.php';

use Stokehold\Request;
use Stokehold\Response;
use Stokehold\Worker;

Worker::create()->run(function (Request $request): Response {
    return Response::text('hello ' . $request->uri);
});
```

Start it with `php worker.php`. It will sit waiting, because a worker speaks the protocol over
stdin/stdout rather than HTTP — point stokehold at it, or see
[Example](#example) for how to poke it by hand.

## The two rules

Both are enforced by the SDK, not merely documented. They are the two things that go wrong in
every persistent-PHP protocol, so it is worth knowing they are handled for you.

### 1. stdout is the protocol channel

Every frame goes to `php://stdout`. Anything the *application* writes there would land inside a
frame and desynchronise the stream permanently.

So application output is not banned — it is **captured**:

- `ob_start()` runs around each request, so `echo` and template output are collected rather than
  written.
- `ini_set('display_errors', 'stderr')` sends warnings and notices to stderr, which is not part
  of the protocol.

The captured output becomes the response body when your handler returns `null`. That means **a
plain PHP script works as a worker without being changed** — if it echoes, that echo is the
response. If your handler returns a `Response`, the captured output is discarded.

To talk to the server *outside* a request, use `emitLog()` / `emitMetric()`. Those are the reason
this protocol exists instead of using FastCGI: a worker can speak first.

The residual risk is a write made directly to the `STDOUT` constant
(`fwrite(STDOUT, …)`, `file_put_contents('php://stdout', …)`), which no output buffer can
intercept. Nothing in PHP or any framework produces output that way, and if it happens the
consequence is a checksum mismatch the server reports clearly rather than silent corruption.

### 2. Shutdown by construction

When the server wants the worker to stop it closes the child's stdin. The loop reads that as a
clean EOF and returns from `run()`, so the worker exits on its own. There is **no signal to
catch** and no flag to poll — which is why the server never has to ask twice before killing.

## Writing a handler

The handler receives the `Request` and returns one of:

| Return | Result |
| --- | --- |
| `Response` | sent exactly as given |
| `string` | sent as a `200` `text/html` body |
| `null` | the captured output becomes the body |

If the handler throws, the SDK logs it to stderr, answers `500`, and **keeps serving**. One bad
request does not take the worker down.

```php
Worker::create()->run(function (Request $request, Worker $worker): ?Response {
    // Telemetry never touches the response.
    $worker->emitMetric('request.count', 1.0, 'count');
    $worker->emitLog("serving {$request->uri}");

    if ($request->uri === '/health') {
        return Response::json(['status' => 'ok']);
    }

    // Echo case: the captured output is the body, so a plain script works.
    echo 'you asked for ', htmlspecialchars($request->uri, ENT_QUOTES);

    return null;
});
```

### `Request`

| Member | Notes |
| --- | --- |
| `method`, `uri`, `protocol` | as sent |
| `headers` | `name => value`; `header($name)` looks up case-insensitively |
| `body` | the raw request body |
| `contentType()` | shorthand for the `Content-Type` header |
| `query()` | the query string parsed the way PHP populates `$_GET` |
| `serverVars()` | CGI-style variables derived from the request (for legacy code) |
| `remoteAddr`, `scriptFilename`, `https` | as sent |

### `Response`

```php
Response::text('plain');                       // 200, text/plain
Response::html('<h1>hi</h1>');                 // 200, text/html
Response::json(['ok' => true]);                // 200, application/json
new Response(404, [['Content-Type', 'text/plain']], 'not found');

$response->withStatus(201)->withHeader('X-Trace', 'abc')->withBody('…');
```

`withHeader()` **appends** rather than replaces, so `Set-Cookie` can be sent more than once.

### `Worker`

| Method | Purpose |
| --- | --- |
| `Worker::create()` | bind to stdio, force `display_errors=stderr` |
| `run($handler)` | handshake, then serve until EOF |
| `emitLog($message, $level)` | one log record, unsolicited |
| `emitMetric($name, $value, $unit)` | one metric sample, unsolicited |
| `recycle()` | ask the server to replace this worker after the request |
| `currentRequestId()` | the in-flight request id, for telemetry correlation |

`recycle()` is what an application calls when it has put itself into a state it cannot clean up —
a leaked global, a stuck connection, a fat static cache. The server sees `keep_alive = false` in
`END` and boots a replacement; the request that asked is still answered normally, so a handler
never has to choose between responding and being cleaned up.

## Example

[`example/worker.php`](example/worker.php) is a runnable worker with `/`, `/health`, `/echo`,
`/big`, `/boom` and `/recycle` routes — including a route that throws (to show that a failing
request does not kill the process) and one that asks to be recycled. `/echo` also echoes the raw
request body back, which is how the pool tests confirm a body arrived as bytes rather than merely
arrived.

To drive it by hand you need something that speaks the protocol; the test below does exactly
that, over a real pipe:

```bash
cargo test -p stokehold-worker --test php_worker
```

And the server side of the contract — spawning, handshaking, recycling, shutdown — is covered
against real worker processes by:

```bash
cargo test -p stokehold-server --test worker_pool
```

Those tests are also the honest answer to "how do you know it works": a Rust client writes
REQUEST frames into a live `php worker.php` and reads the RESPONSE back, so both implementations
are held to the same wire format with a real process in between. They skip themselves, with a
printed reason, when no `php` is on `PATH`.

## Tests

```bash
php tests/run.php          # 124 assertions, no PHPUnit needed
```

- `tests/WireFormatTest.php` pins the header layout, CRC-32C and the **exact golden bytes** —
  the same literals that `crates/stokehold-worker/tests/wire_format.rs` asserts, so if either
  implementation drifts, one of the two suites fails.
- `tests/WorkerTest.php` drives a `Worker` over in-memory streams: output capture, chunking,
  request bodies, control frames, error recovery, and the framing failures.

Running the suite prints a `WARNING-MARKER` line on stderr. That is the mechanism working, not a
failure: the warning went to stderr instead of into a frame.

## Requirements and versions

- PHP **8.1+** — `readonly` constructor promotion is the floor, and PHP 8.0 fails to parse the
  SDK (verified: 8.0 errors, 8.1 not tested, **8.2.23, 8.3.31, 8.4.24 and 8.5.9 all pass the
  full suite**).
- `ext-hash` for `hash('crc32c', …)` — that is how the checksum is computed without
  reimplementing the polynomial, and it matches `crc32c` in Rust exactly.
- `ext-json`.

The protocol version is `1`. A worker and a server that disagree are caught at the handshake
rather than on the first request.

The SDK relies on `stream_get_contents($stream, $n)` for exact reads, which blocks until it has
`$n` bytes or reaches EOF and handles partial reads internally. That is deliberate: hand-rolling
an `fread` loop is the bug-prone part of every pipe protocol, and this call is why the SDK does
not contain one.

## License

MIT — see [LICENSE](../../LICENSE).
