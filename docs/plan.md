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
| HTTP front-end runtime | **`hyper` on `tokio`, confined to `crates/stokehold-http`** — the target is the public-facing case (static files, HTTP/2 now, HTTP/3 later), and HTTP/3 needs `quinn`, which is itself a `tokio` program. So the runtime was arriving anyway; this is that bet made earlier rather than a second one. Every backend stays blocking and is reached only through `spawn_blocking` |
| PHP floor | **8.2** — the floor is `memory_reset_peak_usage()`, which the SDK calls to make each `END` report that request's peak rather than a high-water mark that never falls. 8.1 was never verified, and azera and clarity already require 8.2+, so one floor across the stack beats three |
| Static files | **In the HTTP front-end, above the backends, off by default** — a path naming a file is answered from disk, a path naming nothing is dispatched. Streaming through `tokio::fs` rather than zero-copy: `sendfile` is Unix-only, so it would be a second code path rather than a faster one |
| Where the document root lives | **`server.document_root`, with the `--document-root` flag overriding it** — a deployment fact belongs in the file a systemd unit already reads, and the more specific statement (the flag) is the one that takes effect. Same rule for `front_controller`. An empty file value is **rejected** rather than given a meaning, because TOML cannot distinguish it from an absent key |
| Configuration reload | **Not built** — see open decision 5. Restarting the process is the honest answer until the dispatcher can be reloaded whole rather than in part |
| Where the PHP SDK lives | **Its own repository, `sailantis/stokehold-php-sdk`** — Packagist requires `composer.json` at the root of a repository, so a tagged `sailantis/stokehold-worker` cannot exist while the SDK sits in `sdk/php/`. The spec (`docs/worker-protocol.md`) stays here: it is the shared document both ends are tested against, and two copies of a specification are two specifications. A release of the package links back to it at a **pinned commit**, because a branch link would silently start describing a later protocol than the release implements |
| Skipped end-to-end tests | **A skip is visible, and a failure when `STOKEHOLD_E2E=1`** — `libtest` hides a passing test's stderr, so "skipped with a printed reason" was in practice identical to "passed". The suites that need PHP or the SDK are the ones that actually prove the wire format, so CI sets `STOKEHOLD_E2E=1` and a skip fails there |

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

Shape of the SDK (`sailantis/stokehold-worker`, MIT, its own repository —
[`sailantis/stokehold-php-sdk`](https://github.com/sailantis/stokehold-php-sdk) — because a
Composer package needs `composer.json` at the root of a repository):

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
9. **`hyper` panics rather than silently skipping a deadline when no timer is set.** Configuring
   any timeout — `header_read_timeout`, for instance — requires a `Timer` to be supplied first, and
   the panic ("no timer") happens per connection, inside the runtime, so it looks like every
   request failing rather than a missing line of setup. `TokioTimer::new()` on the HTTP/1 builder
   is what makes the timeout real.
10. **An async front-end must never call a blocking backend directly.** A worker slot serves one
    request at a time and `php-cgi` processes sequentially, so every backend blocks. Calling one
    from an async context stalls a runtime worker thread; enough stalls starve every other
    connection, and the symptom is latency, not an error. The dispatcher is therefore reached only
    through `spawn_blocking` — `crates/stokehold-http/src/listener.rs`, and the rule is stated in
    that crate's module docs so it is not rediscovered.
11. **Static serving must not decode the URL path.** Percent-decoding a path before resolving it
    to a file is how a file server is walked out of its document root: `%2e%2e%2f` resolves to
    `../` in a layer that decodes and to a literal file name in a layer that does not. The static
    layer therefore refuses `..` segments outright *and* leaves the path encoded, so the two rules
    cannot disagree. PHP's router, which does decode, is reached through the front controller and
    is a separate trust boundary — see `crates/stokehold-http/src/static_files.rs`.
12. **A ranged or streamed response cannot carry a `Content-Length` hyper did not choose.** A body
    whose `size_hint` is not exact is framed as chunked, so a stream over a file must be a `Body`
    with `SizeHint::with_exact` rather than more bytes than were promised — a mismatch desynchronises
    the connection rather than merely being wrong. The streaming body in `static_files.rs` keeps its
    own remaining-byte count for exactly this reason.
13. **TOML cannot express "set, but empty".** `front_controller = ""` and an absent
    `front_controller` deserialize to the same thing, so an empty value cannot be given a meaning
    distinct from the default. Rather than guessing which the operator meant, the value is
    *rejected* with a message pointing at the command-line flag that does mean "no front
    controller" — and `stokehold check` reports the resolved controller so the effective value is
    always visible. The general rule: when a config format cannot distinguish two states, refuse
    the ambiguous input instead of picking a winner.
14. **A preflight hint is a claim about the rest of the system, so test it against the rest of the
    system.** The first version of the missing-front-controller hint told the operator to write
    `server.front_controller = ""`, which `Config::validate` rejects — a warning whose advice fails
    when followed. `crates/stokehold-core/src/preflight.rs` has a test asserting no hint
    recommends a value validation refuses, because the two were written separately and nothing else
    connects them.
15. **A static file server must refuse to serve a script, or it discloses source code.** The static
    layer answers a path that names any existing file, and `.php` source is a perfectly valid
    response body — so `GET /index.php` returned the program rather than running it, with a 200 and
    a plausible `Content-Type`, and nothing downstream could tell. Worse, this was **missed by the
    test suite for a structural reason**: the shared fixture never wrote an `index.php`, so the
    assertion `"/index.php" → not a file` passed because the file was absent, not because the rule
    existed. A test that asserts a refusal must create the thing being refused. The rule now shares
    `stokehold_fcgi::cgi::has_php_extension` with the routing layer, because two lists of "what is a
    script" would drift and the one that drifted would be the one serving source. It also makes
    `/index.php/foo` work, which is the `PATH_INFO` case a deployment needs.
16. **A relative document root silently breaks a *persistent* PHP pool after the first request.**
    `php-cgi` `chdir()`s into the directory of the script it executed, and the pool keeps the
    process, so from the second request on a relative `SCRIPT_FILENAME` resolves against the
    previous request's directory and PHP answers `No input file specified`. Verified against
    php-cgi 8.3.31: the identical path succeeds once and fails every time afterwards — which also
    means a single-request test cannot see it. The CLI therefore absolutises the document root
    before it builds the CGI environment. This is *not* something nginx has to do, because nginx
    does not cache a `chdir` across requests.
17. **`php-cgi -b` is not one process, and `PHP_FCGI_CHILDREN` is inherited from the environment.**
    The plan's Windows note ("`PHP_FCGI_CHILDREN` relies on `fork()` and cannot work there") is
    **wrong about the mechanism**: on Windows php-cgi re-spawns itself with `CreateProcess`, up to
    `WIN32_MAX_SPAWN_CHILDREN` (64) of them, and all of them share the single bound socket. So
    `php-cgi -b host:port` gives *one slot's worth of concurrency* rather than one process, and the
    count is read from php-cgi's own environment (`sapi/cgi/cgi_main.c`) — a host that exports
    `PHP_FCGI_CHILDREN` for php-fpm silently hands it to stokehold too. Measured on this machine:
    `workers = 2` produced **21 processes** (one parent plus 20 children per slot, 20 being the
    inherited value). The pool now sets `PHP_FCGI_CHILDREN=0` (`0`, not `1`: php-cgi spawns
    CHILDREN *children in addition to itself*) and `PHP_FCGI_MAX_REQUESTS=0`, because PHP's own
    check is `if (max_requests && requests == max_requests)` and its default is 500 — so without
    the second variable a child exits and is silently replaced while stokehold's `max_requests`
    accounting keeps counting. That made every pool test start 21 processes instead of 1, which is
    also an excellent way to make an integration suite flaky.
18. **A test helper that waits for a specific banner field hangs on a config that does not set it.**
    The CLI tests parsed the startup banner and returned only once the *FastCGI* address had been
    seen. A test configuring only `http` therefore never satisfied the condition, and because it
    used `read_line` — which has no timeout — on a pipe the server never closes, the loop's own
    deadline was never re-checked. The symptom was a test that hung for ever instead of failing,
    which is strictly worse than a failure: it hides the assertion it was about to make. The
    requirement is now "any listener was reported".
19. **Routing must only treat a script as a script, and must not hand PHP a dotfile.** `php-cgi`
    does not check that the file it is given is PHP: handed a file it cannot parse it emits it
    **verbatim with a 200**. Two rules keep that from turning the document root into a file server:
    the script-boundary split requires a PHP extension (so `/config.json` is application input
    rather than a script), and a path with a dotfile component is 404 before the front-controller
    fallback (so `/.env` is not answered with its own contents as `PATH_INFO`). Found by hand,
    not by a test: `GET /.env` returned the secrets in it *with static serving switched off*, so
    the static layer's dotfile rule never applied, and `POST /assets/app.css` returned the file
    because the static layer deliberately only handles `GET`/`HEAD`/`OPTIONS` and routing then
    dispatched the stylesheet as a script. The tests that existed could not catch it: their fixture
    never wrote a non-PHP file, and the FastCGI tests drive routing through a client-built CGI
    environment.

20. **A skipped test and a passing test are the same green.** `libtest` hides the captured stderr
    of a *passing* test, so `println!`/`eprintln!` reason strings never reach an ordinary
    `cargo test` run. The worker suites ("skipped, with a printed reason, when no `php` is on
    `PATH`") therefore report the same result whether the protocol was exercised against PHP or
    not — and a green suite is exactly the shape a silently broken end-to-end test takes. Worse,
    the SDK moved to its own repository, so the two suites that drive it now depend on a checkout
    being *found*: a wrong `STOKEHOLD_SDK_DIR` would have been indistinguishable from a pass.
    `STOKEHOLD_E2E=1` makes a skip a failure, which is what CI should set; it is deliberately off
    by default so a laptop with no PHP stays green.

21. **A lock held for the length of a request makes `workers` a lie.** The dispatcher kept its
    `Dispatcher` behind one `Arc<Mutex<_>>` and the pools took `&mut self`, so *every* request in
    the whole server queued on one lock. Measured on the homepage, throughput was flat at ~65 rps
    from concurrency 1 to 16 while RoadRunner on the same site scaled linearly to ~765. The
    per-request cost was never the difference — both did ~66 rps at concurrency 1, so stokehold was
    simply never using more than one core. The protocol's one-request-at-a-time rule is *per
    worker*, not per pool, so nothing about the design wanted that serialisation: the pools now
    lock per slot and the request path takes `&self`. Two lessons worth keeping. First, a
    concurrency bug looks like a *constant*, not an error — no failure, no latency regression at
    low load, just a ceiling nobody set. Second, the obvious regression test is a trap: asserting
    that concurrent requests come back with distinct worker pids passes even when the pool is
    fully serialised, because round-robin hands consecutive requests to different workers whether
    or not they queue. The test that actually bites measures **overlap** — four 500 ms requests
    against four workers must finish in about one sleep, not four — and it was confirmed to fail
    (2.08 s) with a pool-wide lock deliberately reintroduced.

22. **An external backend is one connection, so it is one request at a time — and nobody told the
    front-end.** `Backend::External` kept a single `Client` behind a `Mutex`, and
    `request_external` held that lock for the *whole* request. That was correct as written: one
    FastCGI connection is a conversation, and multiplexing requests onto it is not in the codec.
    The defect was that nothing bounded how much work is offered: a pool of 12 php-fpm workers
    behind one socket served exactly one request at a time, for ever. Measured on a 12-core Linux
    box against a 12-worker php-fpm pool with `pm = static`, throughput was **3,916 rps at
    concurrency 1 and 3,907 / 5,709 / 5,460 / 5,472 / 5,481 at 2 / 4 / 8 / 16 / 32** — it peaked at
    *concurrency 1* and was then flat, while nginx over the same socket and the same pool did
    3,908 → 43,740. This was hazard 21's shape at a smaller scale: the ceiling is real, invisible
    in a single-request test, and looks like the *pool* being slow rather than the client being
    serial.

    **Fixed.** `Backend::External` now owns a `ClientPool`: one connection per in-flight request,
    taken for the duration of a request and put back when it comes back, which took the arm from
    flat-at-5,500 to **3,614 / 7,075 / 12,993 / 21,719 / 27,504 / 27,657** at concurrency 1→32.
    Reuse is unconditional for any connection the backend agreed to keep, because a pooled
    connection cannot be proven alive without spending the round trip it is trying to save — and
    `request_external` already recovered from a stale connection by reconnecting once, so handing
    one out costs nothing when it is dead and saves a connect when it is not.

    Only *retention* is capped, and the cap is **one**, which is not arbitrary. `php-fpm` accepts a
    connection *into a child* and then waits for the request on it, and a child blocked in that
    read counts as **active** in php-fpm's own scoreboard, so every parked connection permanently
    removes a child from the set the master will dispatch to. Retaining 4 cost 7% of throughput at
    concurrency 32 (34,250 vs 36,906 rps) and retaining 12 cost 26% (27,407 vs 36,906) — measured
    by making the cap an environment variable and sweeping it. One is worth a single child,
    because the reconnect it saves is on the path of *every* request: it raised concurrency-1
    throughput from 2,986 to 3,598 rps and cost nothing at concurrency 32 (36,497 vs 36,906,
    inside the noise). Two bought nothing further (3,578).

    The comparison that makes the cap legible: **nginx reuses nothing.** Read from php-fpm's own
    `accepted conn` counter (`pm.status_path`, polled over the unix socket with a hand-rolled
    FastCGI client because nginx's default `fastcgi_keep_conn` is off and a direct unix-socket
    `curl` resets), an nginx run served ~135,370 requests with **138,076 accepted connections** —
    one connect per request — while reaching 43,265 rps. So the residual gap to nginx is not
    connection handling at all: with retention at one, arm D now matches arm A, static file
    serving by the same Rust front-end with no PHP in the path, so the backend is no longer what
    D is limited by. Regression test:
    `crates/stokehold-server/tests/external.rs::concurrent_requests_reach_the_backend_at_the_same_time`
    — four requests sleeping 300 ms each must finish in under a second, and it fails at 1.26 s
    against the single-connection version (verified by reverting).

23. **The pipe is not the problem on Linux; the *SAPI* is.** Measured on 12 cores, 12 workers, same
    trivial JSON handler in every arm: piped worker **2,936 / 5,789 / 11,101 / 18,050 / 20,984 /
    20,864** (concurrency 1→32) versus php-cgi over TCP **2,937 / 5,969 / 11,105 / 19,230 / 25,351 /
    30,278**. Identical to within noise up to concurrency 8, then the TCP path pulls away. So the
    ~15 ms floor measured on Windows was indeed a Windows pipe/timer artifact and does *not*
    generalise: on Linux a pipe round trip is a direct wake-up, and the two transports start out
    equal. What separates them is later: the CGI arm keeps scaling to 32 while the piped arm
    plateaus at ~21k. The practical consequence is that the choice between a piped worker and
    `php-cgi` on Linux is not about speed at all — it is about superglobals, POST and `$_FILES`,
    all of which the real CGI SAPI supplies for free and a piped worker must reimplement in PHP.
    Choose the SAPI you want, then measure.

24. **`workers` and the machine's cores are unrelated numbers.** On Windows the homepage ran 12
    workers on 8 logical CPUs and beat a 12-worker RoadRunner; here, 12 workers on 12 cores put
    static-file serving at 36–37k rps, which is *above* the 12-worker application ceiling by
    design — a static request never touches PHP. Sweeping to concurrency 32 on 12 cores still
    improved the CGI arm (30,278 rps), so the useful sweep is `cores × 2–3`, not "as many as
    possible". Recording it because the per-arm `p99` is the number that moves first: the piped
    arm's p99 goes 458 µs → 5.27 ms between concurrency 16 and 32 while its rps is flat, which is
    the signature of a queue forming behind a saturated pool rather than a throughput problem.

25. **The worker arm still spends a thread per request, and that is what caps it.** On the trivial
    handler the piped worker reaches 20,422 rps at concurrency 32 while `spawn_cgi` with the *same*
    12 slots reaches 29,950 — and RoadRunner's piped worker, speaking the same protocol shape,
    reaches **31,525**. So stokehold currently *loses* the direct worker-versus-worker comparison
    by about 54% at concurrency 32, having been level up to concurrency 8 (17,471 vs 16,610). The
    difference on the worker path is `request_with_deadline`
    (`crates/stokehold-server/src/worker_pool.rs`): a pipe read has no portable timeout, so each
    request spawns a `std::thread` to run the exchange and takes the result through an
    `mpsc::recv_timeout`, while `CgiPool::request` calls straight into the slot. So every request
    costs a thread create and a channel handoff, and the pool reaches its ceiling *below* the
    point where PHP is saturated — the signature is the piped arm's `p99` climbing to 5.48 ms
    while its rps stays flat. The fix has to keep the deadline: a persistent per-slot reader
    thread with a channel handoff preserves it, and the timeout then becomes a `recv_timeout` on
    a channel that already exists rather than one created per request. Not yet done.

    The consolation is that this is the *only* arm where stokehold is behind: the `external`
    backend, once hazard 22 was fixed, beats RoadRunner 36,341 to 31,525 at concurrency 32 — and
    with a real database query in the request the piped worker is ahead of every FastCGI arm at
    low concurrency (hazard 27), which suggests the deficit is amortised away by any request that
    does actual work. It is still a real cost on the path of every request and worth removing.

26. **`php-cgi -b` accepts a unix socket where the platform has one.** `-b <address:port>|<port>`
    is not TCP-only: `fcgi_listen` (`main/fastcgi.c`) treats a bind path that contains no `:` as
    an `AF_UNIX` path on anything that is not Windows — `unlink`s it and binds it — and only
    falls back to TCP when `:` is present or the argument is all digits. So a `spawn_cgi` pool can
    be given unix sockets on Linux, which removes the loopback-TCP hop. On Windows the same code
    path is a named pipe, which is a different feature and not what `-b 127.0.0.1:port` does
    there. Relevant because the TCP arm's advantage over the piped arm at high concurrency is a
    per-request cost, and a unix socket is the cheaper version of the same shape.

27. **With a real database in the request, the four arms converge — and the piped worker stops
    being behind.** One `SELECT` of one row from a local MariaDB, `p:`-prefixed persistent
    connections, 12 workers, concurrency 1→32: piped worker **2,073 / 4,085 / 7,844 / 12,987 /
    15,415 / 16,264**, `external` php-fpm **1,989 / 3,505 / 6,084 / 10,561 / 14,768 / 17,439**,
    nginx over the *same pool and socket* **1,799 / 3,683 / 6,700 / 12,714 / 17,517 / 19,424**, and
    `php-cgi` **1,551 / 3,280 / 6,361 / 10,225 / 13,312 / 14,858**. Every arm loses ~55% of its
    trivial-handler throughput, so all the transport differences that dominate an empty request
    are noise next to one round trip to the database. Two things follow. First, **the question
    "does php-fpm fall back" has no single answer**: at concurrency 1 the piped worker wins
    (2,073), and above it `external`/nginx win (17,439 vs 16,264 at 32) — so neither backend is
    uniformly better and the choice should follow the SAPI you want, not the graph. Second, **the
    piped worker is not paying a pipe tax here**: it is ahead of both FastCGI arms at concurrency
    1 and within 7% at 32, where a per-request transport cost would show up most. The earlier
    "piped worker looks slow" reading was the thread-per-request cost of hazard 25 amortised over
    a very short request; a real request hides it.

    Worth recording alongside it: the **naive pattern is indistinguishable from the persistent
    one.** A fresh `mysqli` connect per request measured 1,382 / 2,734 / 5,026 / 8,337 / 10,389 /
    11,173 for the piped worker, against 2,073 / 4,085 / 7,844 / 12,987 / 15,415 / 16,264 with a
    persistent connection — only ~20% apart, not the order of magnitude a connect cost suggests.
    A loopback connect plus authentication to MariaDB is that cheap, which is why benchmarking an
    application that reconnects per request still produces believable numbers.

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
19. `http-frontend` — **done**, in `crates/stokehold-http`: a `hyper` listener speaking HTTP/1.1
    and HTTP/2 over the same dispatcher as the FastCGI front-end, with route → backend selection
    and the 404/403/413/502/503/504 mappings. This is the project's only async code, and the
    boundary is `spawn_blocking`; see the crate docs for why.
20. `static-files` — **done**. Files under the document root are served without a backend: a MIME
    type table, `ETag`/`Last-Modified` and 304s, single and multi-range requests with `If-Range`,
    precompressed `.br`/`.gz` variants, directory indexes with a redirect, dotfile denial, and a
    configurable `Cache-Control`. It is deliberately **off by default**, so upgrading changes
    nothing, and it sits *above* the backends rather than beside them: a path that names no file
    goes to PHP exactly as before, which is what keeps a front controller working. Split out of
    step 19 because it is a feature rather than a translation layer, and because it is what turns
    "serves PHP" into "can front a site". See `crates/stokehold-http/src/static_files.rs`, and
    hazards 11 and 12.
21. `config-cli` — **done**. `stokehold serve|check|version`. The gap this closed was that the two
    settings that describe a deployment most concretely — the document root and the front
    controller — existed only as `serve` flags, so a systemd unit or a container image had to carry
    a command line to say where the application lived. Both are now `server.document_root` and
    `server.front_controller`, with **a flag winning over the file**. `version` prints the worker
    protocol version and the PHP floor from constants next to the code they describe, so a build
    cannot claim a floor it does not have. `check` gained preflight warnings that need the
    filesystem rather than the file — is the document root there, is the front controller actually
    present, is a discovered PHP below the worker floor — reported as warnings rather than errors,
    because a document root that does not exist yet is a normal thing to check. Reload was
    deliberately **not** built: see open decision 5.

Phase 7 — packaging
22. `packaging-ci` — release artifacts per target, systemd unit, Windows service notes, CI
    matrix, README and docs.
23. `bench-harness` — load generator plus comparison against nginx+php-fpm, RoadRunner and
    php-cgi, to state realistic expectations rather than claims.

Later milestone
24. `tls-acme` — `rustls` + `rustls-acme`, ALPN for HTTP/2 over TLS, optional HTTP/3. HTTP/3 is
    `quinn` + `h3` + `rustls`, not `hyper`; `quinn` runs on `tokio`, which the HTTP front-end
    already has.
25. `prefork-optional` — Unix-only prefork to cut worker bootstrap cost, forked from a PHP boot
    parent (never the Tokio runtime).
26. `rpc-sidechannel` — only if a v2 need appears (background jobs, websockets) that the
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
   for v1? — **Answered: in scope, as its own routing layer above the backends, off by default.**
   See step 20 and the Static files section of the README.
5. **Configuration reload.** Re-reading `stokehold.toml` on `SIGHUP` without dropping a connection
   is the obvious next thing an operator asks for, and it is **not** built. The reason is that
   only part of the file can be reloaded in place: listeners are bound, backends are running
   processes with live pipes, and both front-ends hold the dispatcher behind a handle. Reloading
   routes or static settings into a *running* dispatcher needs the dispatcher to be re-readable
   rather than owned, which is a real change to how `serve` holds it — so the honest options are
   (a) leave it out and restart the process, (b) reload only what is cheap, or (c) make the
   dispatcher a reloadable handle. Pick deliberately rather than growing a half-reload that
   changes some settings and silently ignores others.

## 9. Notes

- **Do not reimplement what php-fpm already does on Linux**: `pm.max_children`,
  `pm.max_requests` recycling, multi-pool multi-tenancy, slow log, status page, graceful reload.
  On Windows there is no php-fpm, which is the whole reason the custom spawner exists.
- **Windows needs N processes on N ports.** `PHP_FCGI_CHILDREN` relies on `fork()` and cannot
  work there; `SO_REUSEADDR` on one port does not load-balance on Windows. One slot per
  process, one port per slot. Worker mode avoids this entirely by using pipes.
  — **Corrected in hazard 17:** `fork()` is not the mechanism (Windows php-cgi re-spawns itself
  with `CreateProcess`, and does honour `PHP_FCGI_CHILDREN`), but the *conclusion* holds and one
  process per port is still right: the children share one socket, so the slot cannot be recycled,
  counted or killed as a unit. The pool now forces `PHP_FCGI_CHILDREN=0` to say so explicitly
  rather than relying on the host not having exported it.
- **The plan lives in `docs/` in the repo** so it can be opened and commented on in VS Code,
  which was the friction that blocked review of the previous plan.
