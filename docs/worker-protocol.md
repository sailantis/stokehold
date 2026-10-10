# The stokehold worker protocol

**Version 1.** This document is the specification; `crates/stokehold-worker` is its reference
implementation, and the tests in that crate (plus `tests/wire_format.rs`) pin it byte for byte.
The PHP SDK in `sdk/php/` implements the other end.

## Why this exists at all

Worker mode boots a PHP application once and serves many requests from it. That needs a channel
the child can talk back on, and neither of the obvious candidates works:

- **FastCGI cannot carry it.** FastCGI is server-driven and half-duplex: the worker can only
  answer the request it was handed. It has no way to push a log line, a metric, or a "recycle me"
  notice, and worker mode is exactly the case where the worker needs to talk first.
- **goridge is a framing codec, not a protocol.** Adopting its 12-byte header saves the easy
  part while importing a versioning commitment to its wire format; adopting its semantics means
  becoming a RoadRunner clone.

So stokehold models the same proven header shape and owns a message set.

## Transport

The channel is the child process's **stdin/stdout pipe**. That is portable, needs no port per
worker, and cannot exhaust sockets. `stderr` is *not* part of the protocol: it is free for the
worker to write to, and the supervisor drains it so the pipe buffer cannot fill and block the
child.

```
  stokehold ──HELLO/REQUEST/CONTROL/PING──►  php worker  (stdin)
            ◄─HELLO_ACK/RESPONSE/BODY ────  … ──LOG/METRIC/EVENT─  (stdout)
```

The protocol is **symmetric**: the same codec reads what it wrote, so the PHP SDK mirrors this
document rather than inverting it.

## Frame format

Every frame is a 16-byte header followed by exactly `payload_len` payload bytes.

```
offset  size  field
0       4     payload_len        payload bytes, excluding this header
4       1     protocol_version   must be 1
5       1     message_type       1..=14, see below
6       2     flags              see below
8       4     request_id         0 = unsolicited / control
12      4     crc32c(payload)    CRC-32C, see below
```

All multi-byte integers are **little-endian**. Both ends are little-endian in practice
(x86-64, arm64) and the fields are read with fixed-size loads rather than byte-by-byte
accumulators; a big-endian host would lose a memcpy, never correctness.

The reference layout, in bytes, for `Hello`, `request_id = 0`, empty payload:

```
00 00 00 00   payload_len = 0
01            protocol_version = 1
01            message_type = HELLO
00 00         flags = 0
00 00 00 00   request_id = 0
00 00 00 00   crc32c("") = 0
```

### Payload limits

| Limit | Value | Why |
| --- | --- | --- |
| `MAX_PAYLOAD_LEN` | 64 MiB | A corrupt `payload_len` must not make the reader allocate 4 GiB. Far above any legitimate frame, because bodies are chunked. |
| `MAX_BODY_CHUNK` | 64 KiB | Bounds the memory a single frame can demand and keeps a chunk far from the sanity limit. |

A frame declaring more than `MAX_PAYLOAD_LEN` is rejected **before** allocating.

## Message types

The numbering is part of the protocol. These values must never be reordered or reused.

| # | Name | Direction | Correlated | Carries |
| --- | --- | --- | --- | --- |
| 1 | `HELLO` | server → worker | no | Protocol version, for the handshake |
| 2 | `HELLO_ACK` | worker → server | no | `protocol_version`, `php_version`, `sapi`, `pid`, `runtime` |
| 3 | `REQUEST` | server → worker | yes | Request metadata (JSON). The body follows in `BODY` frames |
| 4 | `RESPONSE` | worker → server | yes | Response head: `status`, `headers`, `body_len` |
| 5 | `BODY` | either direction | yes | Raw bytes of the request or response body |
| 6 | `EVENT` | worker → server | no | Application-level telemetry. Currently traced and dropped |
| 7 | `LOG` | worker → server | no | One log record: `level`, `message`, `request_id` |
| 8 | `METRIC` | worker → server | no | One sample: `name`, `value`, `unit` |
| 9 | `CONTROL` | server → worker | no | `drain`, `shutdown`, `reload` or `report` |
| 10 | `CONTROL_ACK` | worker → server | no | Acknowledgement of a `CONTROL` |
| 11 | `END` | worker → server | yes | Terminal status: `app_status`, `peak_memory_bytes`, `duration_us`, `keep_alive` |
| 12 | `PING` | either direction | no | Keep-alive probe |
| 13 | `PONG` | either direction | no | Answer to `PING` |
| 14 | `ERROR` | either direction | no | `message`, `request_id`, `recoverable` |

An unknown type number is rejected rather than skipped: skipping it would silently misalign the
reader from the sender's expectations.

### Which types a worker may receive

A worker only ever handles `HELLO`, `REQUEST`, `PING` and `CONTROL`, and answers each of them. It
does not handle `RESPONSE`, `BODY`, `END`, `HELLO_ACK`, `CONTROL_ACK`, `PONG` or `ERROR` — a
server sending any of those is a protocol error, and both the Rust codec and the PHP SDK refuse
them rather than guessing. `LOG`, `METRIC` and `EVENT` are absorbed and ignored wherever they
appear, because a peer echoing telemetry back must not derail the loop.

### Correlation

`request_id` is meaningful only for `REQUEST`, `RESPONSE`, `BODY` and `END`. Every other type is
unsolicited and **must** carry `request_id = 0`.

Ids are allocated by the server, start at **1**, and increment per request. **`0` is reserved**
and never used as a request id, so a correlation bug is visible instead of silently matching.
The counter wraps at `u32::MAX` back to 1, skipping 0. Requests are never multiplexed: a worker
serves one request at a time, so a demux table would buy nothing.

## Body framing

A body is zero or more `BODY` frames in the direction of travel, all sharing the request's id.
Two invariants apply, and a reader must enforce both:

1. **`body_len` is authoritative.** `REQUEST` declares the request body length in `body_len`;
   `RESPONSE` declares the response body length the same way. The receiver knows how much to
   expect before the first byte arrives, and a short body is *detected* rather than hanging.
2. **The last `BODY` frame carries `LAST`.** A cheap second check that catches a stream that has
   desynchronised in a way the length alone would not.

A body of zero length sends **no** `BODY` frames at all.

Bodies are never JSON. Base64 inside JSON inflates an upload by a third and makes the runtime do
work per byte on the hot path, so bytes travel as bytes.

## Flags

| Bit | Name | Meaning |
| --- | --- | --- |
| `0x0001` | `COMPRESSED` | Reserved. No codec is negotiated in v1, so a frame carrying this bit is **rejected** rather than misread |
| `0x0002` | `CHUNKED` | The payload is one chunk of a larger logical message |
| `0x0004` | `LAST` | This is the last chunk of a logical message |
| `0x0008` | `ERROR` | The sender is reporting a failure |

## Checksum

The header carries **CRC-32C (Castagnoli)**: polynomial `0x82F6_3B78`, reflected, initial and
final value `!0`. This is the "crc32c" algorithm, *not* the "crc32b" one that zlib and PHP's
plain `crc32()` use.

The check value is the standard one:

```
crc32c("123456789") == 0xE3069283
crc32c("")          == 0x00000000
```

The same value is what `hash('crc32c', '123456789')` produces in PHP, which is why the PHP SDK can
use the built-in hash rather than reimplementing the polynomial.

The checksum detects truncated and garbled frames. A mismatch means framing has already
desynchronised, so the stream is unrecoverable and the worker must be discarded.

## Exchange sequences

### Handshake, once at boot

```
server  ──HELLO (version)──────────────────►  worker
        ◄─HELLO_ACK (version, php, sapi, pid)─
```

The worker's `protocol_version` is checked **here**, at boot, not on the first request: a
mismatched SDK is a configuration problem and should look like one.

### Serving a request

```
server  ──REQUEST (id, metadata, body_len)─►  worker
        ──BODY (id, chunk)*────────────────►
        ◄─RESPONSE (id, status, headers, body_len)─
        ◄─BODY (id, chunk)*, last flagged LAST─────
        ◄─END (id, app_status, rss, duration)──────
```

`app_status` in `END` is the **application's** exit status and is **not** the HTTP status. A
worker that throws can report `app_status = 1` while the response head still carries
`status = 500`. Conflating the two is how a protocol ends up unable to express a failed request
that produced a valid error page.

`END` also carries `peak_memory_bytes`, so the supervisor can recycle a worker on an RSS
threshold — the same idea as RoadRunner's `max_worker_memory` — and `keep_alive`. A worker that
handled a fatal error reports `keep_alive = false` and is replaced instead of reused.

### Control

```
server  ──CONTROL (drain|shutdown|reload|report)─►  worker
        ◄─CONTROL_ACK──────────────────────────────
```

### Health

```
either  ──PING──►  other
        ◄─PONG───
```

### Telemetry, at any time

`LOG`, `METRIC` and `EVENT` may arrive **during** any of the above, always with `request_id = 0`.
A reader must absorb them rather than fail the exchange: a log line arriving in the middle of a
response is normal, not a protocol error. Records seen while a request is in flight are also
attached to that request's response.

An unreadable `LOG` or `METRIC` payload is logged and dropped; it is never worth failing a
request over. An unreadable payload on a correlated frame *is* an error.

`ERROR` carries `recoverable`. A recoverable error is a warning and the exchange continues; a
non-recoverable one fails the request but leaves the stream aligned and usable.

## Stream ends

- A stream that ends **before the first byte of a header** is a clean close.
- A stream that ends **part-way through a header or payload** is an error. Treating it as a clean
  close would hide a truncated frame.

## Errors and their consequences

The distinction that matters is between a *framing* failure and a *message* failure.

| | Fatal (stream is unusable) | Not fatal (stream stays usable) |
| --- | --- | --- |
| | unsupported version, unknown message type, unexpected message type, truncated frame or header, checksum mismatch, invalid UTF-8, invalid JSON, payload too large, I/O error, closed, timeout | wrong request id, remote (application) error |

A wrong request id or a remote error leaves the stream aligned, so the worker is worth keeping.
Everything else means the reader can no longer trust its position in the stream.

## Two rules that prevent the classic bugs

1. **Worker stdout *is* the protocol channel.** The SDK must rebind PHP's own output — `echo`,
   warnings, notices — away from stdout, or application output corrupts the framing. The SDK
   **captures** rather than bans it: `ob_start()` per request collects `echo` and template
   output (and turns it into the response body when the handler returns null), `ini_set('display_errors', 'stderr')`
   sends warnings to stderr, and `emitLog`/`emitMetric` are the supported way to speak outside a
   request. See `sdk/php/src/Worker.php`.
2. **Graceful shutdown by construction.** Closing the child's stdin makes the worker's read
   return EOF and `waitRequest()` return `false`; the worker exits on its own. Only after a grace
   period does the supervisor kill it. There is no signal to catch and no flag to poll, which is
   why the server never has to ask twice.

Both rules are implemented in `sdk/php/` and tested against a real PHP process by
`crates/stokehold-worker/tests/php_worker.rs`, which drives a live `php worker.php` over pipes.

## The server's side of the contract

Everything above is the wire format. This section is the policy the server applies on top of it,
because a worker cannot enforce any of it and an application author needs to know what to expect —
`crates/stokehold-server/src/worker_pool.rs` is the implementation.

**When a worker is replaced.** A worker is not replaced because it failed a request; it is
replaced on exactly three signals, all of which come from the worker itself or from
configuration:

| Signal | Meaning |
| --- | --- |
| `keep_alive = false` in `END` | The application asked to be recycled (`Worker::recycle()`) |
| `max_requests` reached | The configured request count for one worker |
| `peak_memory_bytes` above `memory_limit_mb` | The worker's own report of its peak RSS |

The replacement is invisible to the request that triggered it: the response is returned, and the
next request lands on a freshly booted worker. The pool's size does not change — `pick_slot`
replaces a worker that died in place rather than shrinking.

**When a worker is discarded.** A fatal protocol failure (see the table above) means the stream
can no longer be trusted, so the worker is killed and replaced rather than reused. A wrong
request id or a remote error does not: those leave the stream aligned.

**A failed request is not retried.** This is the one place the worker backend deliberately differs
from the FastCGI backends, and it is a correctness point rather than a simplification. There, a
transport error proves the request never reached the application, so replaying it is safe. Here a
failure can happen *after* the application ran and produced side effects, and a frame the server
did not finish reading is not evidence that nothing happened. Replaying a `POST` because its
response was malformed would make the application run twice, so the error is reported instead.

**Every wait has a deadline, and the deadline is a kill.** A pipe read has no portable timeout, so
each exchange runs on a thread the pool is free to stop waiting on; the worker is killed to make
the pending read return. That is also why a recycled worker's pipes are not reused after a
timeout — the reader may still be inside a read.

**Recycling and telemetry.** `LOG` and `METRIC` records that arrive while a request is in flight
are attached to that request's response *and* kept on the channel, so a caller can see either the
per-request view or everything the worker has ever said. The server renders a request's records
into the response's `stderr`, which a front-end already forwards to its own error log.

**Shutdown.** The server sends `CONTROL drain`, then closes the worker's stdin. The `CONTROL_ACK`
is not awaited: the frame is already in the pipe buffer when the pipe closes, so the worker sees
both signals in order. If the worker has not exited after a grace period it is killed, and a
lifecycle guard (a job object on Windows, a process group on Unix) reaps anything left if the
server itself dies.

## Versioning

`protocol_version` is `1`. The reader rejects any other value rather than guessing, so a future
version is an explicit negotiation rather than a silent misinterpretation.

Payloads are **JSON** in version 1, behind a codec boundary in both implementations. The header,
checksum and framing are payload-agnostic by construction, so a future msgpack codec (if
profiling ever justifies one) changes no framing. Bodies are the exception and are always raw
bytes.

Field names in the JSON payloads are part of the protocol: the PHP SDK decodes the same objects,
so renaming a field is a protocol change.
