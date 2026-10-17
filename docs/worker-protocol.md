# The volucer worker protocol

**Version 1.** This document is the specification; `crates/volucer-worker` is its reference
implementation, and the tests in that crate (plus `tests/wire_format.rs`) pin it byte for byte.
The PHP SDK implements the other end; it lives in
[`sailantis/volucer-php-sdk`](https://github.com/sailantis/volucer-php-sdk), because a
Composer package needs `composer.json` at the root of a repository. A release of that package
links back to this document at a pinned commit, so the two versions can be compared.

## Why this exists at all

Worker mode boots a PHP application once and serves many requests from it. That needs a channel
the child can talk back on, and neither of the obvious candidates works:

- **FastCGI cannot carry it.** FastCGI is server-driven and half-duplex: the worker can only
  answer the request it was handed. It has no way to push a log line, a metric, or a "recycle me"
  notice, and worker mode is exactly the case where the worker needs to talk first.
- **goridge is a framing codec, not a protocol.** Adopting its 12-byte header saves the easy
  part while importing a versioning commitment to its wire format; adopting its semantics means
  becoming a RoadRunner clone.

So volucer models the same proven header shape and owns a message set.

## Transport

The channel is the child process's **stdin/stdout pipe**. That is portable, needs no port per
worker, and cannot exhaust sockets. `stderr` is *not* part of the protocol: it is free for the
worker to write to, and the supervisor drains it so the pipe buffer cannot fill and block the
child.

```
  volucer ──HELLO/REQUEST/CONTROL/PING/CALL──►  php worker  (stdin)
            ◄─HELLO_ACK/RESPONSE/BODY/RETURN ──  … ──LOG/METRIC/EVENT─  (stdout)
```

The protocol is **symmetric**: the same codec reads what it wrote, so the PHP SDK mirrors this
document rather than inverting it.

## Frame format

Every frame is a 16-byte header followed by exactly `payload_len` payload bytes.

```
offset  size  field
0       4     payload_len        payload bytes, excluding this header
4       1     protocol_version   must be 1
5       1     message_type       1..=16, see below
6       2     flags              see below
8       4     request_id         0 = unsolicited / control
12      4     crc32c(payload)    CRC-32C, see below
```

All multi-byte integers are **little-endian**. Both ends are little-endian in practice
(x86-64, arm64) and the fields are read with fixed-size loads rather than byte-by-byte
accumulators; a big-endian host would lose a memcpy, never correctness.

The reference frame, in bytes, for `REQUEST` with `request_id = 1`, `payload_len = 2`:

```
02 00 00 00   payload_len = 2
01            protocol_version = 1
03            message_type = REQUEST
00 00         flags = 0
01 00 00 00   request_id = 1
aa d0 7b 29   crc32c("{}") = 0x297BD0AA
7b 7d         payload = "{}"
```

`HELLO` is the one frame whose payload is not the caller's: see [Handshake](#handshake-once-at-boot)
for the exact bytes it carries.

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
| 2 | `HELLO_ACK` | worker → server | no | `protocol_version`, `php_version`, `sapi`, `pid`, `runtime` (see below) |
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
| 15 | `CALL` | server → worker | yes | `method`, JSON params. A call to the *application*, asked while a request is in flight |
| 16 | `RETURN` | worker → server | yes | `ok`, and a JSON result or an error |

An unknown type number is rejected rather than skipped: skipping it would silently misalign the
reader from the sender's expectations.

### Which types a worker may receive

A worker handles `HELLO`, `REQUEST`, `PING`, `CONTROL` and `CALL`, and answers each of them. It
does not handle `RESPONSE`, `BODY`, `END`, `HELLO_ACK`, `CONTROL_ACK`, `PONG`, `RETURN` or `ERROR` —
a server sending any of those is a protocol error, and both the Rust codec and the PHP SDK refuse
them rather than guessing. `LOG`, `METRIC` and `EVENT` are absorbed and ignored wherever they
appear, because a peer echoing telemetry back must not derail the loop.

### Calls, and why they share this channel

A `CALL` is the server asking the *application* something — `kv.get`, `lock.acquire`, a declared
metric — and it travels on this channel rather than a socket of its own, for two reasons: the
channel is already open, and a second one would make the worker's liveness ambiguous (closing this
channel is the whole shutdown mechanism, see *Shutdown*).

The rule a worker follows is one sentence: **while a request is in flight, a frame that is not part
of the response is a call.** `CONTROL`, `PING`, `LOG`, `METRIC` and `EVENT` already behaved that
way; `CALL` is another arm of the same dispatch. Calls must be answered even when they cannot be
served — a `RETURN` carrying an error, never a dropped frame — because dropping one leaves the
server waiting until its deadline.

Calls are **server → worker only**. A worker has no way to originate one: the response is how it
answers. In the other direction, work arriving for a worker (a job, a gRPC message) is a second
*exchange*, not a call, and is deliberately not part of this mechanism.

### Correlation

`request_id` is meaningful for `REQUEST`, `RESPONSE`, `BODY`, `END`, `CALL` and `RETURN`. Every
other type is unsolicited and **must** carry `request_id = 0`.

Request ids are allocated by the server, start at **1**, and increment per request. **`0` is
reserved** and never used as a request id, so a correlation bug is visible instead of silently
matching. The counter wraps at `u32::MAX` back to 1, skipping 0. Requests are never multiplexed: a
worker serves one request at a time, so a demux table would buy nothing.

**Call ids are allocated downward from `u32::MAX`**, so the two ranges meet only after four billion
requests and a mix-up between a call and a request is a visible correlation failure rather than a
silent match — the same reasoning that reserved 0.

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
| `0x0010` | `NO_CHECKSUM` | The `crc32c` field is **not** a checksum and must not be verified. Only legal once the handshake has agreed to it |

`NO_CHECKSUM` is a **flag rather than a zero checksum**, and the reason is that zero is already
a real checksum: `crc32c("") == 0`, which every empty-payload frame carries. Using the value
itself as the sentinel would make a correctly-checksummed `HELLO` or `END` indistinguishable from
an unchecked frame, and would let corruption of the checksum field — the one field whose value
must never decide whether it is verified — silently disable the check. The field is still
written, as zero, so the header stays 16 bytes.

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

### Turning it off

The checksum walks every payload byte, so on a 12 KB page it costs about as much as 12 KB of
`memcpy` — which is to say, on a request that spends most of its time inside the application, not
much. It is nevertheless the largest single item on the *framing* path, and it is the one part of
the protocol that is pure overhead when the transport is a pipe between two processes on one
machine. So it is optional, and negotiated:

- The server may set `no_checksum: true` in its `HELLO`, which is an **offer**. If it does not,
  nothing changes: the worker keeps checksumming and the server keeps verifying.
- The worker **answers** with `no_checksum` in its `HELLO_ACK`, `true` only if it understood the
  field and agrees to the offer. An SDK that predates the field omits it, which decodes as
  `false`, and the pair stays checked.
- Only when both said `true` does either end stop hashing — the server after reading the ack, the
  worker after sending it. Every frame from then on carries `NO_CHECKSUM` and a zero checksum
  field.

Three properties are deliberate:

1. **The handshake is always checksummed.** The agreement is carried *in* frames, so the frames
   that carry it must themselves be verified; otherwise a corrupt `HELLO_ACK` could claim
   agreement that was never given. The `HELLO` and `HELLO_ACK` are therefore written with real
   checksums even when both ends are about to stop computing them.
2. **A sender cannot turn the check off by itself.** A frame claiming `NO_CHECKSUM` on an end
   that never agreed is a protocol violation, not a quiet acceptance, and is refused as such. The
   flag is the writer's claim; believing it is the reader's decision.
3. **A frame that says it is checksummed still is.** The policy relaxes the check only for frames
   that opt out, so a mixed stream is verified wherever it can be.

What is lost is exactly what the checksum detects on unchecked frames: a bit-flip in the payload
of a frame that declared no checksum is not detected, and the corrupt bytes are passed to the
application or the client. On a pipe between two processes on one machine that is a remote
possibility — and a deliberate trade, not an oversight.

## Exchange sequences

### Handshake, once at boot

```
server  ──HELLO (version)──────────────────►  worker
        ◄─HELLO_ACK (version, php, sapi, pid)─
```

The worker's `protocol_version` is checked **here**, at boot, not on the first request: a
mismatched SDK is a configuration problem and should look like one.

`runtime` identifies the implementation and the **build** it is running, as
`volucer-worker-php/7c6945f`. The build is not the protocol version: the protocol is `1` and moves only
when the wire format does, while the build names one particular checkout or release of the SDK. A server
that does not care about the field ignores it, since it is a free-form string; a server that does can
report which SDK build a worker is running without shelling into the host.

A worker reports `unknown` when nothing can tell it what it is — a bare `php worker.php` with no Composer
around it. That is a diagnostic gap, not an error, and a server must not treat it as one.

The field is deliberately reused rather than added. A new field would be a change to the wire
format and would require a protocol version bump — a breaking change to discover a string that
the message already had room for.

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

`peak_memory_bytes` is **per request, not per process**, and the protocol says so because the two
behave differently. PHP's `memory_get_peak_usage(true)` is a high-water mark that begins at
process start and never falls, so reporting it raw would mean a worker that peaked once — during
bootstrap, or during one large request — reports a figure above the configured limit on *every*
request thereafter, and is recycled after each one. The SDK reads the figure and then calls
`memory_reset_peak_usage()` (PHP 8.2+, the supported floor; below it the function is absent and the
figure stays monotonic, which is the old behaviour rather than a failure), so the number describes
the request that just finished. A worker implementation that does not reset is free to report a cumulative
figure — the server acts on whatever it is told — but it will be recycled more eagerly.

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

### A call, while a request is in flight

```
server  ──CALL (method, params)────────────────────►  worker
        ◄─RETURN (ok, result | error)────────────────
```

A `CALL` may only arrive **during** a request, because that is when a worker is reading: between
requests the server's exchange thread is parked on its work channel and nothing drains the pipe.
A worker answers with `RETURN` in both cases, including when it cannot serve the method — a dropped
call leaves the server waiting until its deadline, and a deadline is a kill.

`CALL` and `RETURN` are correlated, and their ids come from the opposite end of the range to
request ids, so a call mistaken for a request is a visible correlation failure.

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
   request. See `src/Worker.php` in `sailantis/volucer-php-sdk`.
2. **Graceful shutdown by construction.** Closing the child's stdin makes the worker's read
   return EOF and `waitRequest()` return `false`; the worker exits on its own. Only after a grace
   period does the supervisor kill it. There is no signal to catch and no flag to poll, which is
   why the server never has to ask twice.

Both rules are implemented in the PHP SDK (`sailantis/volucer-php-sdk`) and tested against a
real PHP process by `crates/volucer-worker/tests/php_worker.rs`, which drives a live
`php worker.php` over pipes.

## The SAPI services a worker has to supply itself

A worker is not a SAPI, so the things PHP normally does before the script runs are missing. Two are
obvious and the SDK handles them — the request body must not be read from `php://input`, and
`$_POST`/`$_COOKIE` are not populated. The third is easy to miss and is recorded here because the
server side cannot detect it:

**Sessions are half a CGI service.** PHP starts a session from the request's `Cookie` header and
writes the cookie naming it into the response headers. Over a pipe neither happens, so a worker that
wants sessions must read the id in and write the `Set-Cookie` out itself.

The half that is easy to get wrong is the second one. A worker's SAPI does not deliver headers at
all — under `cli`, `header()`, `setcookie()` and `headers_list()` are all no-ops — so the cookie has
to be built by the adapter and carried to the client on the response. PHP's own attempt is not
merely redundant, it is invisible, which makes the failure silent: the session is created, the
worker logs nothing, and the id never reaches the client.

The session id also has to be reset between requests. `session_id()` outlives the request in a
long-lived process, so a client arriving with no cookie would otherwise inherit the previous
client's session from the process rather than from its own request.

Both adapters share one implementation of this, `Azera\Http\WorkerSession` in
`sailantis/azera-framework`, which is where a third one should look first.

## The server's side of the contract

Everything above is the wire format. This section is the policy the server applies on top of it,
because a worker cannot enforce any of it and an application author needs to know what to expect —
`crates/volucer-server/src/worker_pool/` is the implementation.

**When a worker is replaced.** A worker is not replaced because it failed a request; it is
replaced on exactly three signals, all of which come from the worker itself or from
configuration:

| Signal | Meaning |
| --- | --- |
| `keep_alive = false` in `END` | The application asked to be recycled (`Worker::recycle()`) |
| `max_requests` reached | The configured request count for one worker |
| `peak_memory_bytes` above `memory_limit_mb` | The worker's own report of its peak RSS for the request just finished |

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

**A call outlives a request, so a call's timeout is not a kill.** A `CALL` runs on the same
exchange thread as the request that is in flight, which means the request's deadline covers it —
and the pool's answer to a missed deadline is to kill the worker. For a request that is correct:
the response is lost and the application may have run it, so the worker is discarded rather than
reused (above). For a call it is not, because a call has no response to lose in that sense and does
have *state* to lose: a worker killed inside `lock.acquire` leaves the server unable to say whether
the lock was taken, and that is the ambiguity the lock API already refuses to paper over. So a call
carries its own deadline, enforced **inside** the handler rather than by the exchange thread, and a
call that runs out of time is reported as a failed call with unknown state and the worker is kept.
The request it was serving then fails on its own deadline if the delay was enough to matter, which
is the correct division: the request's deadline governs the *request*, not the calls made during
it.

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

Field names in the JSON payloads are part of the protocol: the SDK implements the same objects, so
renaming a field is a protocol change. **Adding an optional field is not** — a reader that does not
know it ignores it, which is why `HelloAck::runtime` and the call metadata can be added without a
version bump.

`CALL` and `RETURN` were added as types 15 and 16 in the same way: a new *number* in an existing
range, not a new framing. That was only safe because nothing was released — an SDK that does not
know a type number refuses it as fatal rather than skipping it (see *Errors and their
consequences*), so adding a type after a release would have required upgrading every worker before
the server that talks to it. The rule for the future is therefore: with deployed workers, a new
type number needs the handshake to advertise it and the server to send only what a worker has
agreed to.
