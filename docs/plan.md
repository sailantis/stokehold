# stokehold — cross-platform PHP application server (Rust)

## 1. Problem

We want a single Rust binary that runs PHP applications in two modes on Linux, macOS and
Windows:

- **Worker mode** — a persistent PHP process that boots the application once and serves many
  requests (the RoadRunner/Swoole/FrankenPHP value proposition).
- **FastCGI / FPM-like mode** — full per-request state reset, the way php-fpm behaves.

Hard constraints established in discussion:

- **No PHP embedding, no FFI.** `php`/`php-cgi`/`php-fpm` are always spawned as external
  processes. The embedding path is rejected: it needs a C-level SAPI, loses isolation, and
  forces a rebuild per PHP version.
- **All three platforms**, including Windows — which has **no `php-fpm` at all**. That gap is
  the actual justification for this project: on Windows the only FastCGI-capable PHP binary is
  `php-cgi.exe`, and nothing supervises it for you.
- **Linux is the primary target**; Windows needs to work, not to match Linux throughput.
- **HTTP from the start.** Otherwise you need a second application (nginx) just to see the
  server work. The HTTP listener is in the first milestone; TLS/ACME/HTTP-2 stays a later
  milestone.

`php-cgi -b` already provides full per-request SAPI reset in C — superglobals, POST body and
multipart/`$_FILES` parsing, upload temp files, `is_uploaded_file()`, request startup and
shutdown. That is precisely the work a forked plain PHP process cannot do (it has no SAPI to
populate `$_SERVER`/`$_GET`/`$_POST`/`$_FILES`, and FFI cannot install SAPI callbacks). So
stokehold **never** parses multipart itself: it passes `CONTENT_TYPE`, `CONTENT_LENGTH` and the
raw body as `STDIN`, and PHP does the rest.

## 2. Recovered context

The previous planning session's `plan.md` was lost (its session folder was deleted), and the
session-store database was empty for this machine. The full prior discussion was recovered from
[conversation.txt](../conversation.txt) (400 lines), which preserved the architecture, the
verified `php-cgi.exe -b` probe, the naming evaluation and the open questions. This plan
restores and extends it.

Naming is **settled**: `stokehold`. crates.io-free, low search collision. (Rejected along the
way: `firestarter` — taken on crates.io and collides with the Linux firewall GUI; `tinderbox` —
collides with a GPU forest-fire simulator; `php-wpm` — narrows the scope to worker pooling.)

## 3. Architecture

```
                        ┌──────────────────────────────────────────┐
 browser ──HTTP───────► │ HTTP listener (hyper)                    │
                        │                                          │──► worker pool    (php worker.php, persistent)
 nginx  ──FastCGI─────► │ FastCGI listener                         │──► php-fpm         (external unix socket / TCP)
                        │  └► CGI env builder ► dispatcher         │──► php-cgi.exe     (N processes, N ports, Windows)
                        └──────────────────────────────────────────┘
```

- Both front-ends are just **listeners** that produce the same CGI environment and hand off to
  one dispatcher — so `stokehold serve` can be hit directly by a browser **and** spoken to by
  nginx. One binary, no second application.
- stokehold is a **protocol gateway, not a forward proxy**: it terminates the client protocol
  (HTTP or FastCGI) and originates FastCGI to the backend. It owns both ends, so it translates
  rather than relays — with one exception (pass-through below).
- **Backend is an enum** so one dispatcher serves everything:

```toml
[backends.app]                       # persistent PHP, our protocol, pipes
kind    = "worker"
command = "php worker.php"

[backends.fpm]                       # system php-fpm: Linux/macOS default
kind   = "external"
socket = "unix:/run/php/php8.3-fpm.sock"

[backends.own_fpm]                   # Unix only: stokehold starts php-fpm itself
kind   = "spawn_fpm"
binary = "/usr/sbin/php-fpm"
config = "./fpm.conf"

[backends.cgi]                       # portable fallback; the only option on Windows
kind      = "spawn_cgi"
binary    = 'C:\php-8.3.31\php-cgi.exe'
workers   = 8
```

Route → backend selection is per-vhost or per-route. Worker-safe hot paths go to the worker
pool; anything needing real isolation or SAPI-specific behaviour goes to a FastCGI backend.

### Pass-through on the hot path

For the FastCGI front-end, the backend's `STDOUT` is already `Status:`-header + body, which is
exactly what the client expects. Relaying those bytes verbatim (after validating the status
line) avoids header/body parsing per request. Header rewriting (e.g. `Location` handling) is
opt-in and only when configured.

### Decisions taken

| Topic | Decision |
| --- | --- |
| Protocol for worker pool | **Own framed protocol** (see §4) — FastCGI cannot express worker→server messages |
| Protocol for CGI backends | **FastCGI** — mandatory for `php-cgi`/`php-fpm`; also nginx-testable |
| TLS | **rustls**, not OpenSSL — no DLL/dylib matrix across three platforms |
| Worker transport | child **stdin/stdout pipes** — portable, and needs no port per worker |
| Config format | **TOML** (`serde`) — no YAML dependency, better tooling |
| Front-end scope | HTTP listener **in v1**; TLS/ACME/HTTP-2 as a later milestone |
| Prefork | **deferred**, Unix-only, forked from a PHP boot parent — never from the Tokio runtime |

## 4. Worker protocol (own protocol)

Why own it rather than goridge: goridge is only a **framing codec** (12-byte length + CRC32
header, pipes/TCP/unix transport) and carries no message semantics. Porting just its framing
saves the easy part while importing a versioning commitment to its wire format; being fully
goridge-compatible would mean also adopting `roadrunner/worker`'s semantics, i.e. becoming a
RoadRunner clone. We model the same proven header shape and keep the payload behind a codec
boundary so goridge or msgpack interop can be added later without touching framing.

**Frame header, 16 bytes, little-endian:**

```
offset size field
0      4    payload_len
4      1    protocol_version
5      1    message_type
6      2    flags            (COMPRESSED, CHUNKED, LAST)
8      4    request_id       (0 = unsolicited / control)
12     4    crc32c(payload)  (detects truncated or garbled frames)
```

**Message types:** `HELLO` / `HELLO_ACK` (version, PHP/SAPI/pid handshake), `REQUEST` /
`RESPONSE` (correlated by `request_id`; response carries rss + duration), `BODY` (raw body
bytes as a dedicated frame type — avoids base64 inflation on uploads), `EVENT` / `LOG` /
`METRIC` (unsolicited, `request_id = 0`), `CONTROL` / `CONTROL_ACK` (drain, shutdown, reload),
`PING` / `PONG`, `ERROR`.

**Payload:** JSON in v1 (via serde / `json_encode`), behind the codec boundary.

**Two rules that prevent the classic bugs:**

1. **Worker stdout is the protocol channel.** The SDK must rebind PHP's own output — `echo`,
   warnings, notices — to the `EVENT` stream or `STDERR`, or application output corrupts the
   framing.
2. **Graceful shutdown by construction.** Closing the child's stdin makes `waitRequest()`
   return false; the worker exits; only after a grace period do we kill it.

### Fastest PHP-side implementation

Deliberately stdlib-only — no extensions, no FFI, no build step:

- **Exact reads:** `stream_get_contents($h, $n)` — it blocks until `$n` bytes or EOF and handles
  the partial-read loop internally. Do **not** hand-roll `fread` loops; that is the bug-prone
  part of every pipe protocol.
- **Blocking mode**, no `stream_select` busy loops on the request path.
- **Writes:** `fwrite($h, $frame)` followed by `fflush($h)`.
- **Diagnostics:** the `STDERR` constant — never stdout.
- **Output capture:** `ob_start()` / `ob_get_clean()` per request, plus
  `ini_set('display_errors', 'stderr')`.
- **Payload:** `json_encode` / `json_decode` (C extension, fast). Binary bodies travel in a
  `BODY` frame type rather than base64 in JSON, which is the runtime-speed answer.
- **RSS reporting:** the SDK reports its own peak RSS (`memory_get_peak_usage(true)` plus the OS
  figure where available) so the supervisor can recycle on a threshold — the same idea as
  RoadRunner's `max_worker_memory`.

Shape of the SDK (`sailantis/stokehold-worker`, MIT, shipped in this repo under `sdk/php/`):

```php
$worker = Stokehold\Worker::create();          // HELLO handshake
while ($req = $worker->waitRequest()) {        // one frame per request
    $worker->respond(handle($req));            // writes RESPONSE (+ BODY)
}
```

plus PSR-7 request/response and PSR-15 middleware adapters so a real application is a handful
of lines. The SDK is a **transport, not a framework**.

## 5. FastCGI facts the codec must implement

Source: the FastCGI specification
(<https://fastcgi-archives.github.io/FastCGI_Specification.html>). Values below were verified
against the specification and secondary implementations, and are confirmed by fixture tests in
`crates/stokehold-fcgi`.

- **Record header (8 bytes):** `version`, `type`, `requestIdB1`, `requestIdB0`,
  `contentLengthB1`, `contentLengthB0`, `paddingLength`, `reserved`. Content length is
  big-endian and capped at **65535** per record; padding must be zero bytes.
- **Types:** `BEGIN_REQUEST`=1, `ABORT_REQUEST`=2, `END_REQUEST`=3, `PARAMS`=4, `STDIN`=5,
  `STDOUT`=6, `STDERR`=7, `DATA`=8, `GET_VALUES`=9, `GET_VALUES_RESULT`=10, `UNKNOWN_TYPE`=11.
- **`BEGIN_REQUEST` body (8 bytes):** `roleB1`, `roleB0`, `flags`, `reserved[5]`. Roles:
  `RESPONDER`=1, `AUTHORIZER`=2, `FILTER`=3. `FCGI_KEEP_CONN`=1.
- **`END_REQUEST` body:** `appStatusB3..B0` (4 bytes), `protocolStatus` (1 byte),
  `reserved[3]`. Protocol status: `REQUEST_COMPLETE`=0, `CANT_MPX_CONN`=1, `OVERLOADED`=2,
  `UNKNOWN_ROLE`=3.
- **Name-value pairs:** length `< 128` is one byte with the high bit clear; `>= 128` is four
  bytes big-endian with the high bit of the first byte set (the remaining 31 bits are the
  length). This applies to both name and value lengths.
- **Responses carry a `Status:` header** (e.g. `Status: 404 Not Found`); absent it, the status
  is `200`. php-fpm reports errors as `STDOUT` content and/or an `END_REQUEST` with a non-zero
  app/protocol status.
- Request IDs are ours to allocate; we assign one per in-flight request and **do not multiplex
  concurrently** (php-cgi processes sequentially, so multiplexing buys nothing and adds a demux
  table).

### Operational hazards to build in from the start

1. **Drain `STDERR` for every backend, always.** One reader task per backend, non-negotiable —
   if nobody reads it, the pipe buffer fills and the backend blocks forever. This is the
   classic "why did every worker hang" bug.
2. **Never assume connection reuse.** `FCGI_KEEP_CONN` is a request *and* a server may refuse.
   **Verified against php-cgi 8.3.31: it closes the connection after every response even when
   keep-alive was requested** — the second request on a reused connection fails with a
   connection reset. `php-fpm` does honour it. So the only correct design is: attempt reuse,
   and on a transport-level failure re-establish the connection once and replay the request.
   The replay is safe because a connection that dies mid-frame never reached the application as
   a complete request. Both `external` and `spawn_cgi` share one `is_transport_error` predicate
   so they behave identically; the `external` backend integration test asserts recovery after
   the server behind the socket is replaced.
3. **Windows process hygiene.** Loopback TCP only (no unix sockets); create a **Job Object**
   with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` so `php-cgi` children are reaped when the master
   dies. Unix equivalent: process group plus `PR_SET_PDEATHSIG`, or kill the group. Do this
   early — orphaned `php-cgi` processes holding ports make the dev loop miserable.
4. **`security.limit_extensions`** (php-fpm) rejects `.php`-less script paths; the CGI
   environment builder must produce a `SCRIPT_FILENAME` the backend accepts, or surface the
   backend's error instead of a generic 502.
5. **`CONTENT_LENGTH` is mandatory, not decoration.** Verified against php-cgi 8.3.31: without
   it PHP leaves `php://input` empty and never runs the request-body parser, so the raw bytes we
   send as `STDIN` are silently dropped while the response still looks successful. The
   environment builder must always send it, derived from the body it is about to frame. This was
   found by the `php-cgi` integration test in
   `crates/stokehold-fcgi/tests/php_cgi.rs`, which is why that test exists.
6. **On Windows an accepted socket inherits the listener's non-blocking mode.** A listener put
   into non-blocking mode so `accept` can be polled hands every accepted connection the same
   flag, and Rust does not reset it. Every read then returns `WSAEWOULDBLOCK` immediately and the
   connection is dropped after the first record. The listener must call
   `set_nonblocking(false)` on each accepted stream. This hid for a while because a
   single-request test passes either way — all the bytes are already buffered, so the
   non-blocking read succeeds. It only surfaced on an *interactive* exchange (a `GET_VALUES`
   probe followed by a request), which is why the probe test exists.
7. **A pipe read has no portable timeout, so the worker is the cancellation token.** To give a
   request or a handshake a deadline the exchange runs on its own thread and the pool stops
   waiting on it; the worker is then *killed*, which is what makes the pending read return so the
   thread can exit and drop the pipes. The consequence to remember is that a timed-out worker's
   pipes must not be reused — the reader may still be inside a read.
   `crates/stokehold-server/src/worker_pool.rs`.
8. **Draining `STDERR` on the caller's thread is the same hang as not draining it.** A worker
   whose stderr pipe fills blocks for ever on its next write, which is why one reader thread per
   backend is mandatory. But reading that pipe to end-of-file *blocks until the worker exits*, so
   doing it inline deadlocks the boot: the worker waits for `HELLO` while the server waits for a
   line that will never come. It must be `std::thread::spawn`ed, and the failure looks like a
   startup hang with no error at all — found here by tracing the pool's boot path, not by reading
   the code.

## 6. Phases

Phase 0 — skeleton and baseline
1. `skeleton-workspace` — Cargo workspace, crate layout, CLI stub, error types, `tracing`.
2. `verify-php-cgi` — verify on this machine that `php-cgi.exe -b 127.0.0.1:<port>` binds and
   listens; record `php-cgi -v` SAPI/ZTS details; probe php-fpm availability.
3. `docs-in-repo` — commit this plan and the design notes to `docs/` so they are openable and
   reviewable in VS Code (the previous plan was unreadable because it only lived in a session
   folder).
4. `settings-model-fix` — pin the `research` subagent model in user `settings.json` (it has no
   entry today, so those subagents fall back to an expensive default).

Phase 1 — FastCGI codec
5. `fastcgi-codec` — records, header codec, name-value encoding, `PARAMS` builder, `END`
   parsing, padding handling; unit tests against spec fixtures and captured php-cgi traffic.

Phase 2 — FastCGI client
6. `fastcgi-client` — connection state machine, request framing, response header/status
   parsing, `GET_VALUES` health probe, keep-conn handling.
7. `child-drain` — per-backend `STDERR` drain task, wired into the client from the first
   integration test.

Phase 3 — CGI environment
8. `cgi-env` — full CGI/1.1 variable set, `SCRIPT_FILENAME` / `PATH_INFO` / `DOCUMENT_ROOT` /
   `REQUEST_URI` computation for front-controller routing, `$_SERVER`/`$_GET` correctness,
   `security.limit_extensions` awareness.

Phase 4 — backends and supervision
9. `backend-enum` — backend enum, routing rules, TOML config model.
10. `supervisor` — spawn, health check, recycle, restart-on-crash, graceful drain, backoff on
    crash loops.
11. `unix-lifecycle` — process groups, signals, `SIGUSR2`-style reload semantics for
    `spawn_fpm`.
12. `win-lifecycle` — Job Object, `CREATE_NEW_PROCESS_GROUP`, console-ctrl event, TCP-only
    transport.
13. `external-fpm` — attach to an existing php-fpm socket (unix and TCP), no spawning.
14. `spawn-cgi-pool` — N `php-cgi` processes on N ports (the Windows answer), portable to all
    platforms so it is one code path.

Phase 5 — worker pool
15. `worker-protocol` — write the protocol spec doc, implement the Rust codec, freeze the
    message set, add fuzz/round-trip tests.
16. `worker-sdk-php` — PHP SDK: loop, framing, output rebinding, RSS reporting, PSR-7/PSR-15
    adapters, example worker.
17. `worker-pool-rust` — spawn and supervise workers over pipes, handshake, request
    correlation, drain/shutdown, recycle on RSS threshold.

Phase 6 — front-ends
18. `fastcgi-frontend` — accept FastCGI from nginx, translate to the selected backend, re-frame
    the response, pass-through fast path.
19. `http-frontend` — `hyper` listener, request parsing, route → backend selection, error pages
    (502/504 mapping), body streaming.
20. `config-cli` — `stokehold serve|check|version`, TOML loading, validation and diagnostics.

Phase 7 — packaging
21. `packaging-ci` — release artifacts per target, systemd unit, Windows service notes, CI
    matrix, README and docs.
22. `bench-harness` — load generator plus comparison against nginx+php-fpm, RoadRunner and
    php-cgi, to state realistic expectations rather than claims.

Later milestone
23. `tls-acme` — `rustls` + `rustls-acme`, HTTP/2, optional HTTP/3; this is the nginx
    replacement, deliberately decoupled from the PHP work.
24. `prefork-optional` — Unix-only prefork to cut worker bootstrap cost, forked from a PHP boot
    parent (never the Tokio runtime).
25. `rpc-sidechannel` — only if a v2 need appears (background jobs, websockets) that the
    request/response protocol genuinely cannot express.

## 7. Testing strategy

- **Codec unit tests** first, against the specification's own examples, plus byte-exact
  fixtures captured from a real php-cgi session.
- **Integration tests** against real binaries on this machine: `php-cgi.exe -b`, and php-fpm if
  present. Tests skip with an explicit message when a binary is missing rather than failing.
- **SDK tests** in PHP, including the output-rebinding case (an `echo` inside application code
  must not corrupt framing) and a large binary upload to exercise `BODY` frames.
- **Supervisor tests** for crash restart, drain, and — on Windows — master death leaving no
  orphaned `php-cgi`.
- **Cross-platform CI** for Linux/macOS/Windows; Windows exercises the `spawn_cgi` path and the
  Job Object.
- **Benchmarks** report latency percentiles and throughput, never raw averages, and always with
  the PHP version and binary recorded.

## 8. Open decisions

1. **Default backend out of the box.** Recommendation: `external` when a php-fpm socket is
   detected, else `spawn_cgi`; worker mode opt-in per route because it depends on an
   application that is worker-safe.
2. **Payload encoding freeze.** v1 JSON is proposed; msgpack can be added behind the codec
   boundary if profiling shows JSON is material.
3. **`spawn_fpm` on Linux.** Do we ship it in v1 for self-contained dev, or stay `external`-only
   and let systemd own php-fpm (the recommendation, since php-fpm does pool management,
   `pm.max_requests`, per-pool users and slow logs better than we would)?
4. **Static file serving** in the HTTP front-end: minimal `static` middleware, or out of scope
   for v1?

## 9. Notes

- **Do not reimplement what php-fpm already does on Linux**: `pm.max_children`,
  `pm.max_requests` recycling, multi-pool multi-tenancy, slow log, status page, graceful reload.
  On Windows there is no php-fpm, which is the whole reason the custom spawner exists.
- **Windows needs N processes on N ports.** `PHP_FCGI_CHILDREN` relies on `fork()` and cannot
  work there; `SO_REUSEADDR` on one port does not load-balance on Windows. One slot per
  process, one port per slot. Worker mode avoids this entirely by using pipes.
- **The plan lives in `docs/` in the repo** so it can be opened and commented on in VS Code,
  which was the friction that blocked review of the previous plan.
