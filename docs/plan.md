# volucer — cross-platform PHP application server (Rust)

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
volucer **never** parses multipart itself: it passes `CONTENT_TYPE`, `CONTENT_LENGTH` and the
raw body as `STDIN`, and PHP does the rest.

## 2. Recovered context

The previous planning session's `plan.md` was lost (its session folder was deleted), and the
session-store database was empty for this machine. The full prior discussion was recovered from
[conversation.txt](../conversation.txt) (400 lines), which preserved the architecture, the
verified `php-cgi.exe -b` probe, the naming evaluation and the open questions. This plan
restores and extends it.

Naming is **settled**: `volucer` (formerly `stokehold`) — Latin for *winged, swift*, from *volāre*
("to fly"). The name is the performance claim rather than a metaphor for it, and it replaced
`stokehold`, whose `hold` reads as *withhold* to anyone who does not know the nautical term — arguing
against the `stoke` half of the name. Free on crates.io, npm, PyPI and RubyGems; `.dev`, `.io`,
`.org`, `.co` and `.app` unclaimed; no trademark found in a screen of USPTO and the major databases
(**screened, not legally cleared**). See hazard 55 for the rename itself and the one name that had to
be withdrawn on trademark grounds. (Rejected along the way: `velifer` — the Latin is exactly right
("sail-bearing"), but *Velifer®* is a registered BASF bioinsecticide, EPA Reg. No. 71840-22;
`firestarter` — taken on crates.io and collides with the Linux firewall GUI; `tinderbox` — collides
with a GPU forest-fire simulator; `php-wpm` — narrows the scope to worker pooling.)

## 3. Architecture

```
                        ┌──────────────────────────────────────────┐
 browser ──HTTP───────► │ HTTP listener (hyper)                    │
                        │   └► Host ► site (root, controller)      │──► worker pool    (php worker.php, persistent)
 nginx  ──FastCGI─────► │ FastCGI listener                         │──► php-fpm         (external unix socket / TCP)
                        │  └► CGI env builder ► dispatcher         │──► php-cgi.exe     (N processes, N ports, Windows)
                        └──────────────────────────────────────────┘
```

- Both front-ends are just **listeners** that produce the same CGI environment and hand off to
  one dispatcher — so `volucer serve` can be hit directly by a browser **and** spoken to by
  nginx. One binary, no second application.
- The HTTP listener resolves the `Host` header **before** anything else, because that is the one
  decision the FastCGI path cannot make for itself: a `host` matcher in the route table is
  consulted after `SCRIPT_FILENAME` is resolved, so it can choose a backend but never a document
  root. `[sites.<name>]` gives each host its own root, controller, routes and static layer, and
  the routing for each is built once at startup rather than per request.
- volucer is a **protocol gateway, not a forward proxy**: it terminates the client protocol
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

[backends.cgi]                       # portable fallback; the only option on Windows
kind      = "spawn_cgi"
binary    = 'C:\php-8.3.31\php-cgi.exe'
workers   = 8

[sites.app]                          # one host, one root, in the same process
hosts            = ["app.example.com", "*.app.example.com"]
document_root    = "/srv/app/public"
front_controller = "index.php"
default_backend  = "app"
```

Route → backend selection is per-site or per-route. Worker-safe hot paths go to the worker
pool; anything needing real isolation or SAPI-specific behaviour goes to a FastCGI backend.

There was a fourth backend kind, `spawn_fpm`, which volucer would have started itself. It
parsed and validated while `Backend::from_config` refused it, so `version` advertised a feature
that could not run — see the decisions table for why it was removed rather than implemented.

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
| TLS | **rustls**, not OpenSSL — no DLL/dylib matrix across three platforms. **Not** build-dependency-free though: the default `aws-lc-rs` provider needs a C toolchain, NASM and CMake (hazard 54) |
| Worker transport | child **stdin/stdout pipes** — portable, and needs no port per worker |
| Config format | **TOML** (`serde`) — no YAML dependency, better tooling |
| Front-end scope | HTTP listener **in v1**; TLS/ACME/HTTP-2 as a later milestone |
| Prefork | **deferred**, Unix-only, forked from a PHP boot parent — never from the Tokio runtime |
| HTTP front-end runtime | **`hyper` on `tokio`, confined to `crates/volucer-http`** — the target is the public-facing case (static files, HTTP/2 now, HTTP/3 later), and HTTP/3 needs `quinn`, which is itself a `tokio` program. So the runtime was arriving anyway; this is that bet made earlier rather than a second one. Every backend stays blocking and is reached only through `spawn_blocking` |
| PHP floor | **8.2** — the floor is `memory_reset_peak_usage()`, which the SDK calls to make each `END` report that request's peak rather than a high-water mark that never falls. 8.1 was never verified, and azera and clarity already require 8.2+, so one floor across the stack beats three |
| Static files | **In the HTTP front-end, above the backends, off by default** — a path naming a file is answered from disk, a path naming nothing is dispatched. Streaming through `tokio::fs` rather than zero-copy: `sendfile` is Unix-only, so it would be a second code path rather than a faster one |
| Where the document root lives | **`server.document_root`, with the `--document-root` flag overriding it** — a deployment fact belongs in the file a systemd unit already reads, and the more specific statement (the flag) is the one that takes effect. Same rule for `front_controller`. An empty file value is **rejected** rather than given a meaning, because TOML cannot distinguish it from an absent key |
| Several hosts in one process | **`[sites.<name>]`, resolved from `Host` before anything else** — because a route's `host` matcher runs after `SCRIPT_FILENAME` is resolved, so it can choose a backend but never a document root. Name-keyed like `[backends.<name>]` so every diagnostic can name the site at fault. Exact name, then the longest `*.` wildcard, then `[server]`; a key a site omits is the server's, and `static_files` falls back per key |
| A route's `path_suffix` and `host` wildcards | **Added**, because `"all `.php` files go to this backend" is the common answer for a project with several entry points and was not expressible with `path_prefix` alone. `path_suffix` is compared **case-insensitively**, the way `has_php_extension` is, so `/upload.PHP` cannot be a script to one layer and miss its route in the other. Ordering stays three readable sentences: more matchers wins, an exact host beats a wildcard, declaration order settles the rest |
| Whether the router must `stat` a path | **Inferred per route, never configured.** The stat does two jobs: existence, which `php-cgi` checks itself and answers `No input file specified` for, and the extension guard, which it does *not* check — handed `config.json` it emits it verbatim with a 200 (hazard 19). So it is skipped only where the pattern already proves the extension (`path_suffix = ".php"`) or where the backend never interprets the path (a **worker**, which routes on `REQUEST_URI`). A `path_prefix` route to `php-cgi` keeps it. `check` prints which routes skip it, and it is not a key because the decision belongs with the code that knows why it is safe |
| `serve_only` | **One key replacing `static_files.enabled`**: unset serves everything, `"backend"` skips the static layer, `"static"` never consults a backend. `static_files = false` is exactly `"backend"`, and a contradictory pair is rejected. The three states are what a reader needs: "static files first" and "no static layer" differ by a stat per request, and "files only" is a site the previous shape could not express at all |
| `spawn_fpm` | **Removed**, not implemented. It parsed and validated while `Backend::from_config` refused it with `Misconfigured`, so `version` advertised a feature that could not run and `check` called such a file OK. §9 already says not to reimplement what php-fpm does — pool management, `pm.max_requests`, multi-pool tenancy, graceful reload — and `external` attaches to the php-fpm systemd already supervises. The key is now rejected by name |
| Path traversal in the router | **Refused**, sharing one predicate with the static layer. `cgi::route` only did `trim_start_matches('/')`, so `..` was carried into `PATH_INFO`, `REQUEST_URI` and the `SCRIPT_FILENAME` built from them; nginx normalises the URI, so the two front-ends disagreed about the same request. Checked **after** percent-decoding, because `%2e%2e` is `..` by the time a path is compared, and per segment, because a URL path always begins with `/` and a backslash separates on Windows but is a filename byte on Unix |
| Per-route static and header policy | **Designed, not built** — `policy = { cache_control, precompressed, headers }` with `backend` becoming optional, so a route is a location rather than only a backend selector. Kept out of the vhosts change because it shares no code with host resolution, and a site's own `static_files` already covers the multi-site case. There is **no** on-the-fly compression today: `precompressed` serves a file that was already built |
| Configuration reload | **Not built** — see open decision 5. Restarting the process is the honest answer until the dispatcher can be reloaded whole rather than in part |
| Where the PHP SDK lives | **Its own repository, `sailantis/volucer-php-sdk`** — Packagist requires `composer.json` at the root of a repository, so a tagged `sailantis/volucer-worker` cannot exist while the SDK sits in `sdk/php/`. The spec (`docs/worker-protocol.md`) stays here: it is the shared document both ends are tested against, and two copies of a specification are two specifications. A release of the package links back to it at a **pinned commit**, because a branch link would silently start describing a later protocol than the release implements |
| Skipped end-to-end tests | **A skip is visible, and a failure when `VOLUCER_E2E=1`** — `libtest` hides a passing test's stderr, so "skipped with a printed reason" was in practice identical to "passed". The suites that need PHP or the SDK are the ones that actually prove the wire format, so CI sets `VOLUCER_E2E=1` and a skip fails there |

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

Shape of the SDK (`sailantis/volucer-worker`, MIT, its own repository —
[`sailantis/volucer-php-sdk`](https://github.com/sailantis/volucer-php-sdk) — because a
Composer package needs `composer.json` at the root of a repository):

```php
$worker = Volucer\Worker::create();          // HELLO handshake
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
`crates/volucer-fcgi`.

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
   `crates/volucer-fcgi/tests/php_cgi.rs`, which is why that test exists.
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
   `crates/volucer-server/src/worker_pool/` (`exchange.rs`).
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
    through `spawn_blocking` — `crates/volucer-http/src/listener.rs`, and the rule is stated in
    that crate's module docs so it is not rediscovered.
11. **Static serving must not decode the URL path.** Percent-decoding a path before resolving it
    to a file is how a file server is walked out of its document root: `%2e%2e%2f` resolves to
    `../` in a layer that decodes and to a literal file name in a layer that does not. The static
    layer therefore refuses `..` segments outright *and* leaves the path encoded, so the two rules
    cannot disagree. PHP's router, which does decode, is reached through the front controller and
    is a separate trust boundary — see `crates/volucer-http/src/static_files.rs`.
12. **A ranged or streamed response cannot carry a `Content-Length` hyper did not choose.** A body
    whose `size_hint` is not exact is framed as chunked, so a stream over a file must be a `Body`
    with `SizeHint::with_exact` rather than more bytes than were promised — a mismatch desynchronises
    the connection rather than merely being wrong. The streaming body in `static_files.rs` keeps its
    own remaining-byte count for exactly this reason.
13. **TOML cannot express "set, but empty".** `front_controller = ""` and an absent
    `front_controller` deserialize to the same thing, so an empty value cannot be given a meaning
    distinct from the default. Rather than guessing which the operator meant, the value is
    *rejected* with a message pointing at the command-line flag that does mean "no front
    controller" — and `volucer check` reports the resolved controller so the effective value is
    always visible. The general rule: when a config format cannot distinguish two states, refuse
    the ambiguous input instead of picking a winner.
14. **A preflight hint is a claim about the rest of the system, so test it against the rest of the
    system.** The first version of the missing-front-controller hint told the operator to write
    `server.front_controller = ""`, which `Config::validate` rejects — a warning whose advice fails
    when followed. `crates/volucer-core/src/preflight.rs` has a test asserting no hint
    recommends a value validation refuses, because the two were written separately and nothing else
    connects them.
15. **A static file server must refuse to serve a script, or it discloses source code.** The static
    layer answers a path that names any existing file, and `.php` source is a perfectly valid
    response body — so `GET /index.php` returned the program rather than running it, with a 200 and
    a plausible `Content-Type`, and nothing downstream could tell. Worse, this was **missed by the
    test suite for a structural reason**: the shared fixture never wrote an `index.php`, so the
    assertion `"/index.php" → not a file` passed because the file was absent, not because the rule
    existed. A test that asserts a refusal must create the thing being refused. The rule now shares
    `volucer_fcgi::cgi::has_php_extension` with the routing layer, because two lists of "what is a
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
    `PHP_FCGI_CHILDREN` for php-fpm silently hands it to volucer too. Measured on this machine:
    `workers = 2` produced **21 processes** (one parent plus 20 children per slot, 20 being the
    inherited value). The pool now sets `PHP_FCGI_CHILDREN=0` (`0`, not `1`: php-cgi spawns
    CHILDREN *children in addition to itself*) and `PHP_FCGI_MAX_REQUESTS=0`, because PHP's own
    check is `if (max_requests && requests == max_requests)` and its default is 500 — so without
    the second variable a child exits and is silently replaced while volucer's `max_requests`
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
    being *found*: a wrong `VOLUCER_SDK_DIR` would have been indistinguishable from a pass.
    `VOLUCER_E2E=1` makes a skip a failure, which is what CI should set; it is deliberately off
    by default so a laptop with no PHP stays green.

21. **A lock held for the length of a request makes `workers` a lie.** The dispatcher kept its
    `Dispatcher` behind one `Arc<Mutex<_>>` and the pools took `&mut self`, so *every* request in
    the whole server queued on one lock. Measured on the homepage, throughput was flat at ~65 rps
    from concurrency 1 to 16 while RoadRunner on the same site scaled linearly to ~765. The
    per-request cost was never the difference — both did ~66 rps at concurrency 1, so volucer was
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
    `crates/volucer-server/tests/external.rs::concurrent_requests_reach_the_backend_at_the_same_time`
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

25. **The worker arm spent a thread per request, and removing that turned the only losing arm into
    a winning one.** On the trivial handler the piped worker used to reach 20,422 rps at concurrency
    32 while `spawn_cgi` with the *same* 12 slots reached 29,950 — and RoadRunner's piped worker,
    speaking the same protocol shape, reached **31,525**. So volucer *lost* the direct
    worker-versus-worker comparison by about 54% at concurrency 32, having been level up to
    concurrency 8 (17,471 vs 16,610). The cause was `request_with_deadline`
    (`crates/volucer-server/src/worker_pool/exchange.rs`): a pipe read has no portable timeout, so each
    request spawned a `std::thread` to run the exchange and took the result through an
    `mpsc::recv_timeout`, while `CgiPool::request` calls straight into the slot. Every request paid
    a thread create and a channel handoff, so the pool reached its ceiling *below* the point where
    PHP was saturated — the signature was the piped arm's `p99` climbing to 5.48 ms while its rps
    stayed flat.

    **Fixed.** `ChannelExchange` owns one thread per worker for the worker's whole life and owns
    the `WorkerChannel`; a request is posted to it on an `mpsc` channel, so the deadline is now a
    `recv_timeout` on a channel that already exists and no thread is created per request. Two
    things fell out of doing it that were not obvious from the outside:

    - **The retry that a recycle needs became possible, and had to be made safe.** A request that
      lands on a worker being recycled now fails as `Closed`/`BrokenPipe` and used to become a 502.
      The pool retries it, but only when replaying cannot execute an application twice, which needs
      *two* facts: `WorkerChannel::request_staged` reports the `RequestStage` it failed in (a
      failure while sending proves the `REQUEST` frame never left, so nothing can have run), and
      `is_idempotent` reads the verb from the CGI params (a `POST` replayed is a second write).
      Stage alone would have made every `POST` retryable — the exact bug. Measured on the VM, a
      `GET` that lands in the kill window is now answered by the replacement, and a `POST` in the
      same window is reported instead of replayed.
    - **A dead worker is detected on the read, not the write.** Windows reports a write to a dead
      pipe as `BrokenPipe` only when the write fails; `is_available`'s `try_wait` catches the
      already-reaped case at `pick_slot`, so the read path is what a mid-request death actually
      hits.

    **Re-measured on the 12-core VM, 12 workers, 5 s, trivial handler (`WORKERS=12 bash bench.sh`):**

    | arm | 1 | 2 | 4 | 8 | 16 | 32 |
    |---|---|---|---|---|---|---|
    | B piped worker, before | 2,875 | 5,653 | 10,664 | 17,471 | 19,737 | 20,422 |
    | **B piped worker, after** | **3,307** | **7,219** | **13,751** | **24,383** | **32,386** | **36,876** |
    | E RoadRunner piped | 3,105 | 6,131 | 11,858 | 17,126 | 22,808 | 32,733 |
    | A static (front-end ceiling) | 5,680 | 11,683 | 24,079 | 35,698 | 37,011 | 36,691 |

    **Volucer now leads RoadRunner at every concurrency** — +10% at 32, +42% at 16, +42% at 8 —
    where before it trailed by 54% at 32. Three repetitions gave `B` 23.0/23.5/24.5k at concurrency 8
    against `E` 17.4/17.0/17.1k and `B` 38.7/38.5/38.3k at 32 against `E` 32.0/32.1/32.0k, so the
    margin is stable to about ±3%. Two things are worth noting alongside it: the piped worker now
    *matches the static-file arm* at concurrency 32 (36,876 vs 36,691), which means PHP is no longer
    what limits it; and `spawn_cgi` did **not** move (29,950 → 30,429), which is the control that
    says the change altered the worker path and nothing else.

    The `external` backend, once hazard 22 was fixed, beats RoadRunner 36,341 to 31,525 at
    concurrency 32 — and with a real database query in the request the piped worker is ahead of
    every FastCGI arm at low concurrency (hazard 27).

26. **`php-cgi -b` accepts a unix socket where the platform has one.** `-b <address:port>|<port>`
    is not TCP-only: `fcgi_listen` (`main/fastcgi.c`) treats a bind path that contains no `:` as
    an `AF_UNIX` path on anything that is not Windows — `unlink`s it and binds it — and only
    falls back to TCP when `:` is present or the argument is all digits. So a `spawn_cgi` pool can
    be given unix sockets on Linux. On Windows the same code path is a named pipe, which is a
    different feature and not what `-b 127.0.0.1:port` does there.

    This was originally justified by the TCP arm's cost, which hazard 30 later showed was a bug
    on volucer's side rather than a property of loopback TCP. With that fixed the two transports
    measure the same (1.238 ms against 1.219 ms), so the option is kept on its own merits —
    convention, and a filesystem path being easier to police with permissions than a port — and
    **not** as a performance recommendation.

    **Implemented** as `socket = "unix:/run/volucer/cgi"` in a `spawn_cgi` backend
    (`SpawnCgiConfig::socket`). Three things about it are deliberate rather than incidental. The
    configured value is a **path prefix**, not a finished path: slot `i` binds `<prefix>.<i>.sock`,
    because one socket per process is what lets a slot be recycled, counted and reasoned about on
    its own — the same reason each TCP slot needs its own port. A `Slot` therefore stores an
    `Endpoint` rather than a port, and recovers the bind address from it for recycling and restart,
    so there is no second field that could drift out of agreement with where the slot actually
    listens. And the `unix:` form is **rejected at validation time** on Windows rather than accepted
    and silently ignored, because the same `-b` argument there means a named pipe. The path-length
    check accounts for the per-slot suffix, so a prefix that fits for slot 0 but not slot 11 is a
    startup error rather than a late surprise. The startup probe connects through the slot's
    endpoint, which is the only probe that proves the *client* can reach the address rather than
    that something is listening somewhere.

    When this was written the sentence ended "…means a named pipe **that this backend does not
    drive**". It now does — see hazard 45.

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

28. **With a real framework in the request, the warm model's advantage is a multiple, not a rounding
    error — and the trivial handler hid it completely.** This was the open question from the whole
    benchmarking effort: on a handler that returns 34 bytes, warm and cold differ by about 10%
    (hazard 27's arms move ~10% between the piped and `php-cgi` transports), which made the worker
    backend look like a marginal optimisation. It is not marginal. **The trustworthy evidence is the
    harness's own published dataset** (`azera-competition: results/real-deployments.json`, real
    servers, real HTTP, same applications as the in-process harness), comparing RoadRunner's resident
    worker against nginx + php-fpm, which re-runs the entry script per request:

    | app, `GET /` | RoadRunner (warm) | nginx + php-fpm (cold) | ratio |
    |---|---|---|---|
    | azera (boot 0.47 ms) | 0.255 ms | 0.785 ms | **3.1×** |
    | laravel (boot 3.0 ms) | 0.591 ms | 4.42 ms | **7.5×** |

    **The ratio scales with what the application has to boot.** A 0.47 ms container costs 3× per
    request to redo; a 3 ms Laravel bootstrap costs 7.5×, and Laravel's `GET /` is otherwise almost
    free, so the boot is most of the request. Against ~1.1× on the trivial handler (hazard 25), this
    is the answer to "does the worker model pay off": on an empty handler hardly at all, on a real
    framework by a multiple. `deploy/volucer/` and `scripts/fw-bench.sh` in `azera-competition` add
    volucer as a third server so the comparison has two independent server pairs rather than one.

    **What is deliberately not claimed here: my own four-arm run's cold numbers.** In that run
    volucer's `spawn_cgi` beat php-fpm on azera (1,115 vs 402 rps) and lost to it badly on Laravel
    (21.8 vs 49.5), and neither figure reconciles with the published FPM rows above (azera should be
    ~1,274 rps, not 402). A cold-versus-cold disagreement in *both* directions means the cold arms
    are measuring something other than the deployment model, so they are not evidence for anything
    and the published dataset is used instead. The warm-over-cold ratio measured *within* volucer
    (resident worker vs `spawn_cgi`, same server, same app: 2.3× azera, 30× Laravel) brackets the
    published ratios and agrees on the magnitude, which is the most that can be said for it.

    Three things were learned while finding that out, all worth keeping:

    - **A first `spawn_cgi` measurement was 3× low because of ini, not architecture.** php-cgi as
      installed has `opcache.enable_cli = Off` and `xdebug.mode = develop`, and it *reads* that flag
      while php-fpm does not. The harness now runs php-cgi through a wrapper with the same settings
      the FPM pools use (`scripts/fw-bench.sh`). Before the wrapper, Laravel's cold arm measured
      16.6 rps against php-fpm's 49 — a gap with nothing to do with the server.
    - **A diagnostic probe was itself confounded, and it is worth remembering why.**
      `opcache.file_update_protection` defaults to 2 seconds, and opcache refuses to cache a file
      whose mtime is newer than that. Writing a probe script and calling it immediately makes it
      miss on every request *by design*, which looks exactly like an opcache that does not survive
      between requests. Any future opcache experiment must `touch` the script and wait before
      measuring.
    - **A php-cgi pool leaked on SIGTERM, and it leaked into the numbers** — see hazard 29.
      `ctrlc::set_handler` covered SIGINT only, so `systemctl stop`, `docker stop` and a plain `kill`
      ended the process before any destructor ran and left every php-cgi child alive. One benchmark
      host accumulated **68 orphans**, each holding a port and a PHP state and competing for the 12
      cores under test. Fixed by enabling `ctrlc`'s `termination` feature.

29. **`max_requests = 0` meant "recycle after every request" — the opposite of php-fpm.**
    `pm.max_requests = 0` in a php-fpm pool and `max_jobs: 0` in a RoadRunner config both mean *never
    recycle*, and a php-fpm pool config is exactly what an operator copies when configuring a Worker
    backend. Both pools compared `served >= max` unguarded, so copying that line made volucer boot
    the application once per request — the deployment model the backend exists to avoid. It was found
    while writing the framework benchmark, where it produced 11 rps where 2,569 was expected, and it
    is the most consequential configuration footgun in the project precisely because it degrades
    silently instead of failing. Fixed in `worker_pool/` and `pool.rs` (`max > 0`), with a test named
    for the value that goes wrong, and both config doc comments now state the zero case.

    That SIGTERM leak was the *other* bug found the same way, and it is the same shape: a correct-
    looking configuration (`Ctrl-C works`) with a silently wrong failure mode (`kill` does not).

30. **A Linux `php-cgi` never sets `TCP_NODELAY`, and that cost `spawn_cgi` over loopback TCP
    34x — a flat 42 ms per request. FIXED.** Getting here took three wrong answers, so the
    wrong ones are kept below. The correct one first.

    **The mechanism.** `main/fastcgi.c:1088` guards its `TCP_NODELAY` call with
    `req->keep && req->tcp`, and `req->tcp` is assigned in exactly one place — line 897,
    under `#ifdef _WIN32`. On Linux it keeps the zero `calloc` gave it, so `req->tcp` is
    permanently 0 and **the call is never reached**. A loopback php-cgi therefore runs with
    Nagle enabled. That is harmless on its own. It becomes a 40 ms stall because php-cgi
    flushes its response in 8 KiB chunks (`out_buf[1024*8]`, line 222): any response larger
    than 8 KiB leaves a short final segment, Nagle withholds it until the previous data is
    ACKed, and on an idle keep-alive connection Linux's ACK is a *delayed* ACK — 40 ms.

    The reason it was invisible for so long: the withheld tail is still unacknowledged when
    the **next** request arrives on that connection, and keep-alive means the connection is
    always reused. The stall is therefore not a first-request cost that a warm-up hides —
    it recurs on every single request. That is why `/`, a 5 KB page, was slow too: the
    previous response's tail is permanently outstanding.

    **The measurement.** Same host, same harness, same requests, one slot, `/items`,
    5 x 2000 sequential:

    | | before | after |
    |---|---|---|
    | T over loopback TCP | **42.005 ms** | **1.238 ms** |
    | U over unix socket | 1.222 ms | 1.219 ms |
    | `TcpExtDelayedACKs` | 10,054 | **0** |
    | T, 12 slots, p95 | 42.09 ms | 1.77 ms |

    42.005 with a spread of 0.05 ms is the tell: that is not a throughput limit, it is a
    timer. `TcpExtDelayedACKs` was ~1 per request on T and **exactly 0** on U, and the wire
    trace showed php-cgi's trailing 1728-byte segment leaving **10 us after** volucer's ACK
    arrived 40.7 ms after the data — sender-side Nagle, receiver-side delayed ACK.

    **The fix** is receiver-side and belongs in `volucer-fcgi`: `TCP_QUICKACK` immediately
    before each `read` on a TCP backend (`transport.rs`). The kernel clears the flag after
    using it and after every `recv`, so it is re-armed per read rather than set once at
    connect time. It takes effect from the next read, which is exactly the read whose ACKs
    would otherwise be delayed. A `setsockopt` failure is ignored: a non-TCP socket or a
    kernel without the option reports `EINVAL`, and correct delivery never depended on this,
    only latency. This is also why **nginx was never affected** — it sets `TCP_NODELAY` on
    its side and drains promptly, and its php-cgi over TCP measured a healthy 848 rps in the
    isolation test all along. The bug was ours, not the transport's.

    Corrected consequence: **loopback TCP and a unix socket are now equivalent** (1.238 vs
    1.219 ms, ~1.5%), so `socket = "unix:..."` (hazard 26) is worth keeping for tidiness and
    for Windows-absent deployments rather than for speed. Any FastCGI measurement taken
    over TCP before this fix understates its arm, so the piped worker's standing against the
    FastCGI arms (hazards 22, 23, 25, 27) is at least as good as those figures say.

    ---

    The three wrong answers, kept because each was plausible and each was refuted by a
    measurement rather than by argument:

    **Wrong answer 1 — "loopback TCP costs 29x".** A single wrk connection per arm showed T
    collapsing to 198–281 rps against U's 8,107. Reported as a real transport cost. It was an
    artifact: **one wrk connection lands on one pool slot**, so that slot serialises while the
    other eleven — and the aggregate rate — are unaffected. The column measured one slot. The
    same artifact is what made the sweep show T at 281 rps on `/items` while sequential latency
    on a *single* connection showed 42 ms: both were one slot, one of them while 12 were
    saturated around it.

    **Wrong answer 2 — "it is delayed ACK, from php-cgi setting `TCP_NODELAY` late".** Half
    right, for entirely the wrong reason. The claim was that php-cgi sets the flag only *after*
    `fcgi_accept_request` reads the request, so volucer's writes fall in a Nagle-eligible
    window. Reading the guard at line 1088 shows the flag is never set on Linux at all, so the
    window never closes rather than closing late. The conclusion happened to be delayed ACK;
    the reasoning was refuted by source.

    **Wrong answer 3 — "the 8-segment write".** `tcpdump` showed volucer sending each request
    as 8 TCP segments, one per 8-byte record header, against nginx's 1–2. That was real and was
    fixed (`Client::send` assembles in a reused buffer and writes once), and it measured
    **1.572 -> 1.509 ms, about 4%** — genuine, and not the cause. It also produced the second
    measurement above (13% on `/items`, 1% on `/`, *identical* with fresh connections), all of
    which was consistent with a 4% fix and none of which would have found a 40 ms timer,
    because a reused connection with one outstanding response is precisely the shape that the
    stall distorts least obviously.

    The lesson worth keeping from all three: **a number that is suspiciously round in wall
    time is a timer, not a rate.** 42 ms is 40 ms of kernel timer plus ~2 ms of work. Looking
    for the timer first would have skipped all of the above.

31. **The warm model's advantage is real at concurrency 1-2 and gone by 16, and the crossover
    is the shape of the answer — not a single ratio.** Hazard 28 established that a resident
    worker beats a per-request boot by a multiple per request; hazard 30's bug made the cold
    arms look worse still. Neither answered the question a deployment faces: *at what load*.

    The two existing measurements were at the two extremes. `latency.php` holds concurrency at
    1, where a cold arm pays its whole framework boot with nothing to overlap it. A wrk sweep
    holds it at the slot count, where that boot is overlapped by eleven other requests and
    vanishes from the aggregate rate. So the same pair of servers reads as **2.07x** and
    **1.00x** depending on which one is asked, and both are correct.

    `scripts/crossover.php` measures the middle: N closed-loop clients, one process each, each
    running the same blocking request loop `latency.php` uses. `/items`, median of 3 runs x 500
    iterations per client, ms:

    | conc | P volucer | R RoadRunner | F php-fpm | U volucer unix | T volucer TCP | R/F |
    |---|---|---|---|---|---|---|
    | 1 | 0.645 | 0.595 | 1.240 | 1.464 | 1.490 | **2.08x** |
    | 2 | 0.646 | 0.580 | 1.177 | 1.348 | 1.381 | 2.03x |
    | 4 | 0.672 | 0.616 | 1.231 | 1.358 | 1.370 | 2.00x |
    | 8 | 0.786 | 0.720 | 1.377 | 1.595 | 1.556 | 1.91x |
    | 12 | 0.931 | 0.802 | 1.528 | 1.895 | 1.910 | 1.91x |
    | 16 | 1.019 | 0.937 | 1.864 | 2.111 | 2.086 | **1.99x** |

    Three things worth keeping:

    - **The advantage does not cross over.** It is ~2.0x at concurrency 1 and still ~2.0x at
      concurrency 16, dipping to ~1.9x in between and returning. So on this application and
      this host there is no concurrency at which the cold model catches up, and the "crossover"
      is better described as flat. That is the opposite of the intuition that a per-request boot
      must eventually be hidden by concurrency, and the reason is in hazard 32: the cold arm
      saturates its CPU budget at ~8,500 rps while the warm arm reaches ~16,000, so the cold arm
      is *already* the one queuing and the gap has no reason to close.
    - **The two transports are the same**, now that hazard 30 is fixed: T and U agree to 3% at
      every concurrency, and this is the clean answer to hazard 23's original question.
    - **P and R agree to 10%**, two independent servers on the same model, which is what makes
      the warm half of the table credible. *(Superseded by hazard 33: the 10% was volucer's
      own front-end waste, it is now fixed, and P leads R — see that hazard for the CPU
      attribution that found it.)*

    The same pass on `/` gives a flat ~1.5x (P/F) and ~1.64x (R/F) at every concurrency. Note
    that the *absolute* cold-arm penalty is much larger on `/items` (0.58 ms) than on `/`
    (0.18 ms), which is more than a constant boot would explain: the cold arm appears to pay
    in proportion to what the request touches, plausibly because autoloading and lazy class
    resolution follow the code path. That is an inference from the numbers, not a measurement,
    and it is recorded as such.

    **Caveat, stated rather than hidden:** past concurrency 12 the *client* is the limit. All N
    client processes share the same 12 cores as the servers, and every measured latency rises
    roughly linearly with N for every arm — including two warm arms that have no boot to pay.
    So the absolute values above 12 are not trustworthy; only the ratio between arms at the same
    concurrency is, and even that is muddied once clients steal scheduled time from server slots.
    A second machine to generate load is the fix, and it is not available here.

    Also worth recording: the first version of this measurement was wrong in exactly the way
    this project keeps being wrong. Driving N handles from one process with `curl_multi`, it
    reported concurrency 2 as *exactly* twice the latency of concurrency 1, on three unrelated
    servers, with throughput flat between them — which no real server does. Two client bugs:
    `curl_getinfo` read after `curl_multi_remove_handle` (every status 0), and
    `curl_multi_select($multi, 0.05)` whose timeout is 50 **milliseconds**, a retry floor that
    cost 1,516 rps where `wrk` got 3,211 at concurrency 2. A hand-tuned timeout would have
    fixed it; running N processes and using the already-validated blocking loop removes the
    class of bug instead. Validated against `latency.php` at concurrency 1: 0.616 ms against
    0.617 ms.

32. **The cold model wastes about a third of the machine, and that — not the transport and not
    the SAPI — is what the warm model's advantage actually is.** A saturated run was observed
    using only ~84% of the 12 cores, with twelve php-cgi children at ~72% each. The obvious
    explanation is wrong: it is not the pool size. Raising `workers` from 12 to 24 to 36 at
    concurrency 32 left throughput flat and slightly lower (7,634 / 7,738 / 7,250 rps), so the
    children were never the limit.

    Measuring CPU per request explains it exactly:

    | arm | rps @ conc 32 | cores used | % of 12 | **CPU per request** |
    |---|---|---|---|---|
    | P worker (warm) | 15,563 | 10.37 | 86% | 666 us |
    | T spawn_cgi (cold) | 7,900 | 8.40 | 70% | **1,063 us** |

    Both numbers close arithmetically: `15,563 x 666 us = 10.37 cores` and
    `7,900 x 1,063 us = 8.40 cores`. So the per-request model costs ~1.06 ms of CPU per request
    against ~0.67 ms, a ratio of 1.60 — and since the cold arm's own 1.06 ms is its *entire*
    budget, it cannot be run faster by adding workers. It is not idle capacity waiting to be
    used; it is the cost of booting PHP and the framework per request.

    The idle 16% is the other half of the same story. `php-cgi` is single-threaded and its
    request cycle is not all CPU: some of it is waiting on its own socket for the next request.
    Twelve such processes leave a core or two unscheduled, and no amount of concurrency fixes
    that, because concurrency is bounded by the 12 children — a php-cgi child serves one request
    at a time. This is a structural property of the cold model, and it is a clean, measurable one:
    **the cold model has a duty cycle, and its ceiling is the pool size times that duty cycle.**

    Which makes the deployment-model result a CPU-efficiency result. The warm model wins because
    it does less work per request, not because it waits less. That also explains why the advantage
    survives to concurrency 16 in hazard 31: the cold arm saturates its cores at ~7,500 rps while
    the warm arm reaches 15,500, so the warm arm is further from its own ceiling at every
    concurrency.

33. **volucer's HTTP front-end used to cost 1.36x RoadRunner's CPU per request, and the whole
    of it was three avoidable scans. Fixed — volucer is now the cheaper of the two.**

    The starting observation was that the warm arm sat ~10% behind RoadRunner on every endpoint
    (16,329 against 18,258 rps on `/items` at concurrency 32) while implementing the same model:
    one resident PHP process per slot, one request at a time down a pipe. A 10% gap between two
    implementations of the same design is small enough to be worth naming, and there are only
    three places it can live — the front-end, the worker protocol, or the client. Wall clock
    cannot separate them, because both servers report the same latency and the same rps.

    **CPU can.** `scripts/diag-split.sh` snapshots `utime + stime` for every process in the arm's
    process tree, walked up the PPID chain from the server's own pid (not matched on `comm`,
    which truncates at 15 characters and cannot tell `php worker.php` from `php-cgi`), brackets
    exactly the measured wrk window, and attributes the delta per `comm`:

    | arm, conc 32 | rps | server CPU/req | PHP CPU/req | total |
    |---|---|---|---|---|
    | P volucer (before) | 16,017 | **413 us** | 608 us | 1,021 us |
    | R RoadRunner | 17,860 | **309 us** | 704 us | 1,013 us |

    The totals agree to 1% while the server halves differ by 1.34x, so the gap is entirely
    volucer's front-end and nothing else — not the protocol, not the client, not PHP.

    **`scripts/diag-profile.sh` then named it.** A flat `perf record` on the server process during
    a saturated run put 15-19% of *everything* in one function, and two more above 5%:

    | symbol | self time, before |
    |---|---|
    | `volucer-worker::codec::Channel::read_frame` (inlined `crc32c`) | **14.81-19%** |
    | `volucer-fcgi::client::parse_response_headers` | 5.20% |
    | `volucer-fcgi::client::split_response_body` | 5.17% |

    Each was a full pass over the 12 KB response payload, per request, in userspace:

    - **`crc32c` was a bit-by-bit implementation** — eight shift-and-mask rounds per byte,
      written by hand so the PHP SDK could mirror it line for line. The SDK does not need a
      transcription; only the *value* has to agree, and the standard check value is already
      pinned by test. A 12 KB page meant ~96,000 inner-loop iterations per request. Replaced
      with the `crc32c` crate (SSE4.2/ARM CRC32 instructions, slicing fallback otherwise).
      *The lesson: "keep it dependency-free so another language can mirror it" is a good
      argument about the encode path and a bad one about the verify path.*
    - **`parse_response_headers` began with `String::from_utf8_lossy(stdout)`** — validating
      UTF-8 across the entire payload, body included, and copying it when invalid, all to find a
      `\r\n\r\n` that a byte search finds for free. Rewritten to work on bytes.
    - **`split_response_body` searched the payload a second time** for the same separator that
      the header parser had just been through, and `find_subslice` used `windows(4).position`,
      comparing four bytes at every offset of 12 KB, twice.

    The three fixes together, measured on the same host and methodology:

    | | before | after |
    |---|---|---|
    | server CPU per request | 413 us | **137 us** |
    | volucer rps @ conc 32 | 16,017 | **18,946** |
    | RoadRunner server CPU per request | 309 us | 153 us |
    | RoadRunner rps @ conc 32 (re-measured) | 17,860 | 18,019 |

    Two of the three were correctness fixes as much as speed ones: the lossy conversion could
    let a non-UTF-8 byte *in the body* move the located header terminator, because the offset was
    computed on a `String` whose length differs from the slice's. `parse_response` now scans once
    and returns status, headers and body together, with the two older entry points delegating to
    it and a test pinning all three to each other.

    **The general lesson, which is hazard 30's lesson again:** "volucer is N% slower than
    RoadRunner" is not a fact about a design, it is a fact about a measurement that has not been
    decomposed. Attribute CPU before optimising anything, or the optimisation is a guess — three
    of these four items are invisible in wall clock, and the first attempt at this gap (hazard 23)
    hypothesised a transport cost and was wrong.

<br>

---

<br>

**34. The response parser's own allocations, and the front-end routing twice.** A second
    pass over the same code came out of a colleague's review of `parse_response` and
    `find_header_end`, and then out of *measuring* instead of reading: an allocation-counting
    `#[global_allocator]` in a dedicated test binary, which turns "this looks allocation-heavy"
    into a number. Four findings, none of which wall clock or `perf` had shown.

    | finding | before | after |
    |---|---|---|
    | `parse_response` headers | `(String, String)` — 2 allocs + 2 `memcpy` per header, ~21 total | `Vec<(&str, &str)>` borrowing `stdout` — **1** |
    | `split_response_body` | delegated to `parse_response`, building a list it discarded — 1 alloc | its own search — **0** |
    | `build_params` header keys | `format!("HTTP_{}", name.to_ascii_uppercase().replace('-', "_"))` — 6 allocs per request header (3 intermediates + the pair's 2) | built into the pair's buffer — **2** |
    | `has_php_extension` | `to_ascii_lowercase()` — 1 alloc **per candidate prefix**, so `n` allocs for an `n`-segment path, each proportional to its prefix | suffix compare on bytes — **0** |
    | `to_cgi` → `build_params` | routed **twice** per request, `is_file` syscalls included | `build_params_for` takes the route the front-end already resolved — **once** |

    The allocation counts are now *asserted exactly*, not bounded, in
    `volucer-fcgi/tests/allocations.rs` and `volucer-worker/tests/allocations.rs`. A new
    allocation in this code is a decision, and a test that names it is the cheapest way to notice.

    **Two correctness bugs fell out of the same review**, which is the part worth remembering:

    - **`parse_response` counted the status line with `enumerate()` over the raw split**, so a
      header block that opened with a blank line — legal — made the real first line `index == 1`
      and a `Status:` there was *silently ignored*, leaving the status at `200`. The loop now
      skips leading empty lines before it looks for the status.
    - **`has_php_extension` was rewritten as `path[path.len() - 5..]`**, which panics when the
      offset lands inside a multi-byte character. Caught by a test with `/café` in the path
      before it could ship, and fixed by comparing bytes rather than a `&str` suffix.

    A third, latent: the status line compared its name *untrimmed* (`Status :` was not `Status:`)
    while every other header was pushed *trimmed*, so one line could be read as `200` and as a
    `Status` header at once. Now the rule is one sentence: a header name is trimmed, for the status
    line as for any other.

    **`to_cgi` calling `cgi::route` and then `build_params` calling it again** was found by
    reading, not measuring — and it is the kind of thing `perf` will not show, because `is_file`
    is a syscall and the routing itself is cheap. It is exactly the "two full passes over the same
    input" shape as hazard 33's two payload scans, one layer up.

<br>

---

<br>

**35. The declared MSRV was already false.** `Cargo.toml` said `rust-version = "1.75"` and the
    README said "Requires Rust 1.75+", but the *locked* dependency tree already required **1.85**:
    `clap`/`clap_builder`/`clap_derive` 4.6.7, `hyper-util` 0.1.21, `indexmap` 2.14.2,
    `hashbrown` 0.17.1 and `tokio-util` 0.7.20 all declare it, so a real 1.75 would be rejected at
    resolve time. The declaration was aspirational, not true, and nothing verified it — there is
    no CI config in this repo at all, which is the reason it could drift silently.

    Raised to **1.85**, the true floor. `[u8]::trim_ascii` (1.80) replaced the hand-rolled
    `position`/`rposition` pair in `client.rs`, verified equivalent first (`"  hi \r\n"` → `"hi"`,
    `"   "` → `""`, `""` → `""`). The same raise also enabled `clippy::manual_repeat_n` (stable in
    1.82), which flagged four `repeat(0u8).take(n)` padding writes across two crates — so the
    dependency floor was the thing gating a lint as well as an API.

    *Worth adding: an MSRV that is not checked by CI is a comment, not a guarantee. A
    `cargo build` under the declared toolchain in CI is what would make it true.*

<br>

---

<br>

**36. Left deliberately on the table.** Recorded so the next pass does not rediscover them as
    findings, and so a later decision has the reasoning rather than just the code.

    - **`request_header` in `worker_pool/request.rs` allocates an owned `String` per CGI parameter** (about
      seven) and two per header, via `text_param` and `headers_from_params`. The worker protocol
      carries a JSON `RequestHeader`, which owns its strings, so this is inherent to the protocol
      rather than to the translation — removing it means a borrowed or `Cow` header threaded through
      the worker crate. Not done: it is an invasive change across crates, no measurement says it
      matters, and the front-end already costs less than RoadRunner's. If the worker arm is ever
      behind again, this is the first place to look after `perf`.
    - **`text_param` scans the whole parameter list for each of the seven names it looks up** —
      about 210 `eq_ignore_ascii_case` comparisons per request over a typical environment. Each is
      a length check first, so this is genuinely trivial; a name-to-index map would cost more to
      build than it saves.
    - **`Header::read_frame` still `memset`s the whole payload** before reading over it
      (`vec![0u8; len]`), which the comment there already explains: the alternative on stable Rust
      is `MaybeUninit`, and profiling does not show it next to anything else on the path. The
      allocation tests now state its size — one allocation, exactly the payload — so if it is ever
      worth removing, the number that has to improve is on record.
    - **`read_record` in `volucer-fcgi` is the allocating reader** and is still what every call
      site uses, including the client's per-request `receive`. A buffer-reusing `read_record_into`
      was written and then removed rather than added speculatively, since nothing measured it and
      it would have been public API with no caller. The allocation counts above are what would
      justify revisiting it.

---

<br>

**37. The request path rewrote the whole request three times.** Hazard 34 fixed the *response*
    side and asked whether the request side had the same shape. It did, and worse: three separate
    stages each held an owned copy of every header, and all three ran on every request.

    The router first, which both listeners call before anything else and which had been left
    alone:

    | | before | after |
    |---|---|---|
    | `percent_decode` | copied the whole path, though nearly every request has no `%` | `Cow` — borrows when there is nothing to decode |
    | forbidden-extension guard | `decoded.to_ascii_lowercase().ends_with(&ext.to_ascii_lowercase())` **inside `any`** — one copy of the path plus one per configured extension | byte compare — constant |
    | `/items` routed to the front controller | **13** allocations | **6** |

    The forbidden guard is the same defect that `has_php_extension` was fixed for in hazard 34 —
    a `to_ascii_lowercase()` to answer a question about a few trailing bytes — and that fix walked
    past this call site without noticing the sibling next to it.

    One syscall was also being spent per request for nothing. `route` tested `candidate.is_file()`
    and `has_php_extension(relative)` together, so a path that could not be a PHP script was
    still resolved and `stat`ed before the extension check rejected it. The two are pure
    predicates, so the answer is identical either way, but with a front controller configured a
    path naming no script is the common case and now never reaches the filesystem.

    Then the worker path, which copied the same headers three times:

    - **`request_staged` deep-cloned the header to stamp in `body_len`** —
      `RequestHeader { body_len, ..request.clone() }`: five `String`s plus two per header, per
      request, to change one integer. It serializes a borrowed view now.
    - **The pool cloned the header into its work item and the codec copied it again.** The
      exchange carries an `Arc<RequestHeader>`, so the listener thread and the exchange thread
      share one allocation.
    - **`Dispatcher::dispatch` allocated a `String` for a backend name it only reads.** The
      borrow is still live at the `dispatch_to` call, so the clone bought nothing.

    For a 16-header request: **90** allocations across the parts this touched before (13 router,
    38 codec clone, 38 exchange clone, 1 backend name), **6** after. Neither remaining term scales
    with the header count.

    The method that found all of it, for the third pass running: **write the counter before
    believing the reading.** A slope test — the same work with 2 headers and with 40 — localises
    a per-header cost immediately, and it caught two of these that reading had not. The clone in
    `request_staged` is four lines and looks harmless; it is 38 allocations.

    Both halves are pinned, deliberately split by crate. `volucer-worker`'s
    `tests/request_path.rs` drives a whole round trip over the real codec, so a reintroduced
    clone anywhere on that path fails there; `volucer-fcgi`'s `tests/allocations.rs` holds the
    router. A slope test in the crate whose source changed is worth more than an exact count in
    a crate that only consumes it.

    The one thing to know when writing these: **compare bytes, not decoded structs.** Every field
    of `RequestHeader` except `method` and `uri` carries a serde `default`, so a field missing
    from the borrowed serialization view would not fail a `Json::decode::<RequestHeader>`
    round-trip at all — it would arrive empty. The byte-equality test exists for exactly that.

    **`volucer-core` is not a target.** It was the question this pass started from, and the
    answer is that it is cold path: configuration, PHP discovery and preflight all run once at
    startup. Its only per-request functions are `RouteConfig::matches` and `Config::route_backend`,
    and `route_backend` is **dead code** — `Dispatcher::backend_for` duplicates the filter-and-max
    rule inline, so a route change has two implementations to keep in step.

**38. Both transports entered the kernel once per frame field.** Hazard 37 asked where the
    request path rewrote a request it already had. This pass asked the neighbouring question —
    where it *entered the kernel* more than it had to — and the answer was every transport in
    the project.

    The rule the codebase already knew, in `volucer-fcgi`'s client: *"assemble the whole request
    in `self.out` and write it once… the number of `write(2)` calls changes, from eight to one."*
    It had been applied to the client's **write** side in an earlier pass and to nothing else.
    Every **read** side was still unbuffered, and every write side of the two servers was still
    field-by-field.

    | | transport | before | after |
    |---|---|---|---|
    | worker `REQUEST` + body | `ChildStdin` | 2 writes | **1** |
    | worker response, 3 frames | `ChildStdout` | 6 reads | **1** |
    | worker response, 100 KB body | `ChildStdout` | 6 reads | ≤ 14, tracks size not framing |
    | FastCGI client response, 2 records | `Stream` | 4 reads | **1** |
    | FastCGI front-end request, 1 record | `TcpStream` | 7 reads | **1** |
    | FastCGI front-end request, 12 records | `TcpStream` | **40 reads** | **1** |

    The counts are exact and measured, not estimated — `pipe_calls.rs`, `allocations.rs` and
    `frontend_syscalls.rs` wrap the real codecs with a counting `Read`/`Write` and drive the
    production call pattern. A `Cursor` makes every one of these free, which is precisely why
    no existing test noticed: the whole suite runs over in-memory streams.

    Three fixes, one per transport:

    - **`volucer-worker` gained `BoundedChannel`**, a `WorkerChannel` over
      `BufWriter<Box<dyn Write + Send>>` and `BufReader<Box<dyn Read + Send>>`. It is a new type
      rather than a change to `WorkerChannel` because that type is generic over its streams on
      purpose — the tests drive it over a `Cursor` — and because the buffering is a property of
      talking to a *pipe*, not of the protocol.
    - **`volucer-fcgi`'s `Client` now holds `BufReader<T>`** rather than `T`. Writes go through
      `get_mut()`, so the one-write-per-request guarantee is untouched and `client.stream`
      became `client.stream.get_ref()` at the two test sites that inspect what was written.
    - **`volucer-server`'s listener wraps its accepted socket in a `BufReader`** and writes
      through `get_mut()`.

    Two details that matter more than they look:

    - **`write_frame` already flushed after every frame**, which is what makes a `BufWriter` safe
      here. The channel is half-duplex: the worker answers only once the request is complete, so
      a frame left in the buffer is a deadlock, not a latency win. Nothing had to be added; the
      flush was already there for its own reasons.
    - **A buffer smaller than a body chunk neither deadlocks nor loses bytes.** `MAX_BODY_CHUNK`
      is 64 KiB and the buffer is 8 KiB, which looks alarming. It is fine for two reasons, and
      neither is that the reader drains concurrently — the exchange thread sends and then
      receives, sequentially: `BufWriter` flushes when the buffer fills, so every byte of a
      64 KiB chunk is written; and the worker reads a whole request before it answers anything,
      which the protocol and the PHP SDK both require, so its stdin is being consumed while the
      client writes and the client never blocks.

    **The `BoundedChannel` name is a misnomer** and should probably be `PipedChannel` or
    `BufferedChannel`; it reads like it is about a bounded queue, which is a different thing this
    project does not have.

    **Settled afterwards, from nginx's own source.** `read_next` decodes each `PARAMS` record
    independently, so an environment split **mid-pair** fails with a truncated pair. That is safe
    with nginx, and now as a fact rather than a belief: `ngx_http_fastcgi_create_request`
    (`ngx_http_fastcgi_module.c`, nginx 1.30.5) sums `params_len` over *every* pair, allocates one
    buffer for `NGX_HTTP_FASTCGI_PARAMS + len + padding`, writes the header once with that total,
    and then appends a zero-length `PARAMS` record and an empty `STDIN`. The whole CGI environment
    therefore travels in a single record, and a pair can never straddle two of them — which is
    what `frontend_syscalls.rs` models.

    The limit is still there, and worth stating plainly rather than closing: the mid-pair case is
    *unhandled*, so a FastCGI client that split at the 65535-byte record boundary would get a 502
    for a request nginx would have served. Nothing in the protocol requires nginx's behaviour, and
    reassembling the `PARAMS` stream before decoding would make the question moot. Latent
    interoperability limit, not a bug with a known trigger.

    What is still unbuffered, deliberately recorded rather than fixed: the **HTTP front end**
    (`volucer-http`) uses hyper's own buffered IO and is not affected; the **static file**
    path is dominated by the file read itself; and `volucer-server`'s listener still flushes
    the response once per record, which the tests pin at five writes for a response with no
    `STDERR` — assembling it first would make it two, but the response is written once per
    request and the reads were the expensive half.

    **The wall-clock answer, measured afterwards.** Re-running the three volucer arms on the
    competition VM against a binary built from the commit *before* these three fixes and one
    built from them, same harness, same load, three repeats, medians:

    | arm | endpoint | conc | before | after | |
    |---|---|---|---|---|---|
    | P worker (pipe) | `/` | 32 | 36,986 | **38,141** | +3.1% |
    | P worker (pipe) | `/` | 8 | 23,800 | 24,320 | +2.2% |
    | T spawn_cgi (TCP) | `/` | 32 | 15,901 | **16,868** | **+6.1%** |
    | U spawn_cgi (unix) | `/` | 32 | 16,506 | 16,900 | +2.4% |
    | T spawn_cgi (TCP) | `/items` | 32 | 8,006 | 8,009 | 0% |
    | U spawn_cgi (unix) | `/items` | 32 | 8,353 | 8,254 | −1.2% |
    | P worker (pipe) | `/items` | 32 | 18,739 | 19,438 | +3.7% |
    | P worker (pipe) | `/` | 1 | 3,289 | 3,393 | +3.2% |

    So the answer is **yes, but modestly** — about 2-6% on the arms where the request and
    response are small and the syscalls are a real share of the work, and **nothing where the
    request already spends its time elsewhere**: `/items` runs an ORM query and renders a
    template, so ~6 saved syscalls per request on each side disappear into the noise, and the
    per-repeat spread within one binary is larger than the difference between them (T's
    `/items` conc-32 repeats were 8,006 / 8,275 / 8,448).

    This is worth stating as the lesson rather than the ratio: **a syscall cut is a fixed cost
    removed, not a proportional one.** It pays most where the request is otherwise cheap — which
    is exactly the `/` arm a deployment comparison leads with, and exactly why the earlier
    passes' allocation and parse work showed up as CPU-percent and this shows up as a few
    percent of throughput. The 40-read front-end request in the table above was the largest
    number in the pass and buys the least throughput, because nginx's request arrives as *one*
    record in practice (see above), not twelve.

    One thing the A/B was *not*: it is not a clean isolation of the buffering commit. The
    pre-buffering binary is `54b8a91`, which also predates hazard 37's request-path fix and the
    MSRV commit, so part of the gain is those. The direction and the magnitude are trustworthy;
    the attribution to one commit is not.

    **The write side of the same hazard, finished.** Hazard 38 fixed every read side and every
    write side *except* one, and left that one by argument rather than measurement — "the
    response is written once per request and the reads were the expensive half". The argument
    was wrong, and wrong for a reason created by the fix in the same pass: the listener wraps
    its socket in a `BufReader` and writes through `get_mut()`, so its **write half is
    unbuffered**. `write_response` was therefore writing STDOUT's header, its payload,
    END_REQUEST's header, its payload and a flush as five separate `write(2)`s on a socket with
    `TCP_NODELAY` set — five syscalls and five segments per response. `write_error` and
    `write_get_values` had the same shape.

    All three now build the framed response in a reused per-thread buffer and write once. The
    per-thread buffer is the point rather than an implementation detail: a response is
    body-sized, so a `Vec::with_capacity` per request would be a 12 KB allocation per request on
    a 12 KB page, which is the cost being removed. It never outlives the closure, so no
    concurrent writer can observe another's bytes and nothing is locked. A `BufWriter` around the
    socket was the alternative and is what `volucer-fcgi`'s client does; here the response has
    a known end, so assembling it whole is simpler than reasoning about where a flush must land.

    Pinned at **five writes → one**, and the bytes asserted equal to the record-by-record
    framing via the existing `append_records` twin. The old test that counted writes to prove a
    large body was still split was rewritten to walk the framing **by bytes**, because a write
    count can no longer distinguish "one write, five records" from "one write, one record" — and
    the new count asserts a 300 KB body leaves in one write rather than one per record.

    Verified end to end against **real nginx** (this front-end's actual peer, which the unit
    tests cannot stand in for): `GET /` and `GET /items` both 200 through nginx →
    volucer FastCGI → php-cgi, 30/30 requests over keep-alive, correct HTML, and an empty
    nginx error log.

**39. The worker path rebuilt the whole request, for the third time.** Hazard 37 removed the
    *copies* the worker path made of a request it already had. This pass asked what it still
    *built*: reading `to_cgi` showed `RequestInfo` had to own its strings because it flows into
    the worker's `RequestHeader`, so a `Cow` refactor there would only move the allocations —
    but the flow itself was the finding. A request on the piped worker was translated twice and
    rebuilt once:

    ```
    to_cgi        → RequestInfo { 9 owned Strings }        (measured: 30 fixed + 2/header)
    build_params  → NameValuePairs { 2 heap Vecs per pair }
    request_header→ RequestHeader { 9 owned Strings }      ← the third rebuild
    → dropped after the worker answered; nothing reads it
    ```

    Every number below was measured by counting, not read: a counter around each translation,
    then both translations driven through the real channel over a `Cursor` with a canned reply
    so the comparison is one host, one binary, one request. `request_header` was previously
    unpinned; `tests/request_header_allocations.rs` is new and now holds all of it.

    | request headers | owned round trip | borrowed round trip | owned translation | borrowed translation |
    |---|---|---|---|---|
    | 0 | 28 | **12** | 12 | **0** |
    | 10 | 49 | **13** | 32 | **0** |
    | 20 | 70 | **14** | 52 | **0** |

    So the translation cost **30 fixed plus two per request header** and now costs **zero at any
    header count**, and a whole request on the worker path dropped from 70 to 14 allocations at
    twenty headers.

    The fix is `borrowed_request`, the counterpart of `request_header`: every field is a slice of
    the CGI environment it came from — a CGI variable's value *is* the client's header value, so
    the value needs no copy at all — and the one value that cannot be a slice, the
    `HTTP_ACCEPT_ENCODING` → `Accept-Encoding` name, is rendered into a **stack buffer** while
    the JSON is being written rather than into a `String` first. That keeps the name off the heap
    in the common case, with `header_name` as the fallback for a name that is too long or not
    ASCII, where the character-wise case fold differs from the byte-wise one.

    **Two implementations of one protocol rule is how they drift**, so the guarantee is pinned
    rather than argued: `the_borrowed_translation_encodes_identically_to_the_owned_one` compares
    the encodings of the whole frame head **byte for byte** across environments chosen for their
    corners — no request headers, the ordinary set, a `HTTP_` prefix that names nothing (which
    both must drop), and a value that is not UTF-8 (which both must render lossily the same way).
    Byte-equality rather than a round trip, for the reason hazard 37 recorded: every field but
    `method` and `uri` carries a serde default, so a field the borrowed translation forgot would
    not fail decoding at all — it would arrive empty.

    The interesting part is the boundary, and it is worth writing down because the obvious
    designs do not work:

    - The head **cannot** be borrowed across the thread boundary. The pool hands the exchange to a
      per-worker thread, and on a timeout that thread outlives the call that made it, so a
      reference into the CGI environment would dangle. (A `transmute` to `'static` "works" and is
      exactly the pattern Miri rejects for that reason.)
    - The **channel's** buffer cannot move to the caller either, or it is an allocation per
      request — the thing being removed.
    - So the boundary is crossed in the one direction that is sound: the head is **serialized on
      the pool's side** and the bytes travel. That costs one `Vec` per request, so the head's
      allocation moves rather than disappears — and it is still worth doing, because the exchange
      **already** copied the body into a `Vec` to move it across. The request now pays one buffer
      where the owned translation paid eleven.
    - `ChannelExchange::request` therefore takes `Vec<u8>` rather than `Arc<RequestHeader>`, and
      `body_len` is stamped in by the pool, at the one place that holds both the head and the
      body. **Getting that wrong is what a test caught**: the first version let the channel stamp
      it in as before, but the channel now receives bytes that are already framed, so
      `body_len` stayed 0 and PHP silently received an empty body — `serve_reaches_a_persistent_php_worker`
      failed with `body: 0 bytes` for a request that carried a marker. The owned path stamps it
      in `send_request` as it always did; the encoded path cannot, so the pool does.

    Three things this leaves, deliberately:

    - **`to_cgi` still owns nine `String`s** to build `RequestInfo`. The figure once attributed to
      it here — "30 fixed + 2 per header" — was the *worker request frame*, a different layer, and
      was wrong for this path. Measured now (hazard 42): 64 fixed + 4.07 per request header, of
      which `RequestInfo` is a third and the whole translation under 1% of a request. **Not worth
      doing**, and the measurement is what says so rather than the shape of the code.
    - **`BoundedChannel` is still a misnomer** and should be `PipedChannel` or `BufferedChannel`
      (hazard 38).
    - The **`render_logs` per-record `String`** and the **`static_files.rs` stats** stand as
      recorded in hazard 38.

**40. The reply-read path allocated a buffer per frame, and zeroed it.** Hazard 39 asked whether
    the worker *write* path could stop rebuilding the request, and answered it. The obvious
    follow-up — asked as "would a pool of buffers for reading request headers help?" — turned out
    to be the wrong shape for the right problem, and the measurement is the finding.

    First, a scoping correction: on the Rust side there is **no request-header read path**. Rust
    writes requests and reads responses; the PHP SDK does the reverse. The read side here is the
    **response** path: a `ResponseHead` JSON frame, then the body in `Body` frames.

    What it cost, measured by driving the real `WorkerChannel::request_encoded` over a `Cursor`
    with a canned reply:

    | reply body | frames | before | after |
    |---|---|---|---|
    | 11 B | 1 | 8 allocs / 392 B | 7 / 293 B |
    | 2 KB | 1 | 8 / 4,468 | 7 / 2,332 |
    | 50 KB | 1 | 8 / 102,773 | 7 / 51,485 |
    | 200 KB | 4 | 13 / 663,926 | 7 / 205,086 |
    | 1 MB | 16 | 27 / 3,080,567 | 7 / 1,048,863 |

    Two costs, both per frame:

    - **The allocation.** `Header::read_frame` did `vec![0u8; len]` once per frame, so a body in
      `MAX_BODY_CHUNK` (64 KiB) frames cost one allocation per frame — 27 for a 1 MB response.
    - **The zero-fill.** That `vec![0u8; len]` is a `memset` of the whole payload that the
      following `read_exact` immediately overwrites. The comment in `read_frame` had already
      flagged this as a known cost deliberately left, because the alternative on stable Rust is
      `MaybeUninit`.

    **A pool is the wrong shape.** `collect_response` runs on the exchange thread and serves one
    request at a time, so there is no concurrency to pool for: what is wanted is a single buffer
    the channel keeps, exactly like `self.payload` on the send side. That asymmetry was the real
    finding — the *write* side already did `std::mem::take(&mut self.payload)` and reused it; the
    read side was never given the same treatment.

    So: `Header::read_into(reader, &mut Vec<u8>) -> Result<Option<(Header, usize)>>`, the
    counterpart of `read_frame` for a caller that expects more frames. It returns the payload's
    **length**, not the payload, because the bytes stay in the caller's buffer — returning a
    slice of a field would borrow-conflict with the `&mut self` the loop needs. It resizes only
    when the buffer is too short (`resize` zero-fills just the new tail) and truncates otherwise,
    so a steady stream of 64 KiB frames stops writing 64 KiB of zeros each time.

    The buffer lives on `WorkerChannel` as `scratch` and is taken out for the duration of
    `collect_response`, which now delegates to `read_response(request_id, &mut frame_buffer)` for
    the same borrow reason `send_request` takes the payload buffer. It goes home on every path,
    including a failure.

    Three more details worth keeping:

    - **The body is reserved from its declaration.** The head arrives before the body and declares
      `body_len`, so the body is sized once instead of regrowing frame by frame — most of the
      3.08 MB → 1.05 MB drop on a 1 MB response, and why the count is now flat in the body size.
      The reservation is **capped** (`BODY_RESERVE_LIMIT`, 1 MiB) because the declaration is the
      worker's to make: uncapped, a worker claiming a gigabyte would make the server commit it
      before sending a byte. Past the cap the body still grows to whatever arrives, so the cap
      decides whether growth is prepaid, not whether it happens. The declaration was already
      validated against the bytes that actually arrive, so a false one is still an error.
    - **A checksum failure clears the buffer** before returning, so a caller that ignores the
      error cannot read the failed bytes as if they were good.
    - **The steady state is 5 allocations.** A *warm* channel costs 5 for any body size — the
      `ResponseHead`, its two `String`s for a header pair, the body buffer, and the `End`. The
      body is the one that cannot go: it is handed to the caller inside `WorkerResponse`, so its
      allocation moves rather than disappears. Only the first request on a channel pays the frame
      buffer (7), and it grows in two steps because the head frame and the body frame are
      different sizes.

    Pinned `read_into` against `read_frame` with a **differential test over the same stream** —
    same headers, same payloads, same stopping point, same `PayloadTooLarge` before any growth —
    because it is a second implementation of one reading rule, which is the shape that silently
    diverges.

    Not claimed: throughput. The worker arm is CPU-bound on PHP, and hazard 38's write-path fix
    measured **no** throughput gain through nginx. Seven allocations and a third of the bytes
    allocated is solid evidence about allocations; a throughput claim needs a VM run of the P arm.

    Left, unchanged: the `ResponseHead` decode's own four allocations (one struct plus two
    `String`s per header pair), which a caller receives and keeps — a pool cannot recycle what
    the caller takes ownership of, which is why the header-buffer idea does not work.

**41. The frame checksum is now optional, negotiated, and worth about 0.26%.** Hazard 40's last
    paragraph was "the checksum is the one cost left on the framing path". It was, and it is now a
    setting — but the measurement that justifies the work is not the one that was expected.

    > **Superseded in scope by hazard 51.** The 0.26% below times only the *Rust-side frame
    > encoding*, and the checksum is hashed **twice** per response — once by the PHP worker that
    > writes the frame and once by the server that reads it. Measured end to end the cost is
    > ~6 µs of CPU per request (~1.5%), and the throughput saving is +2.0% to +3.0%, with a
    > spread that crosses 1.00. The knob is worth more than this hazard claims, and hazard 51 has
    > the per-process attribution that shows where it goes. The decisions below are unaffected.

    Measured, by encoding a whole response (head + 12 KB body + `END`) with the real SDK in a
    loop, 50,000 iterations, four runs:

    | | checked | unchecked | saved |
    |---|---|---|---|
    | 12 KB `BODY` frame alone | 1.63–1.77 µs | 0.73–1.01 µs | ~0.9–1.0 µs (55–57%) |
    | whole response | 3.54–3.81 µs | 1.96–2.12 µs | ~1.5–1.7 µs |

    Against the 617 µs `GET /items` on the worker arm that is **0.26%**. The PHP side is the one
    that mattered, because the worker writes the response and the server merely reads it; the
    Rust side hashes the same bytes once.

    So this is a knob, not a win, and the honest framing is the one hazard 40 used for the read
    path: the evidence is about the framing path, and no throughput number is claimed. It is
    worth having because it is the *last* per-byte cost in the protocol and because turning it off
    is now a one-line configuration change on either side of a measurement, not because 0.26%
    needed a feature.

    Three decisions that are protocol, not implementation:

    - **A flag, not a zero sentinel.** `crc32c("") == 0` is a *valid* checksum, carried by every
      empty-payload frame — the protocol document's own `HELLO` fixture shows it. A value-based
      sentinel would make a correctly-checksummed `HELLO` or `END` indistinguishable from an
      unchecked one, and would let corruption of the checksum field — the one field whose value
      must never decide whether it is verified — turn the check off. Bit `0x0010` instead.
    - **Zero means the status quo.** `NO_CHECKSUM` is opt-in, so an old peer ignores the bit and
      keeps verifying, and a mismatch fails loudly. The reverse polarity — a `CHECKSUM` bit whose
      absence means "unchecked" — would silently downgrade every frame a current release writes,
      because they all carry `flags = 0`. The existing flags already follow this convention
      (`COMPRESSED`, `ERROR`).
    - **The offer is checksummed even when the agreement is to stop.** The handshake carries the
      agreement, so a corrupt `HELLO_ACK` must not be able to claim agreement that was never
      given. Both ends hash `HELLO` and `HELLO_ACK` and switch only afterwards.

    Two corrections found while doing it, both worth recording because they were latent:

    - **`Header::read_frame` and `read_into` had a second copy of the payload check.** Adding the
      policy meant the strict reader refused a frame the lenient one accepted *from the same
      bytes* — a differential test caught it immediately. `read_frame` now delegates to
      `read_frame_with(.., Checksum::Required)`, so the rule lives in one place again.
    - **The `HELLO` payload byte fixtures were unpinned.** `wire_format.rs` pinned 16-byte headers
      and the reference `HELLO` was the empty-payload zero fixture, so the day `HELLO` stopped
      being an empty frame — which it did when it moved from a reused `RequestHeader` to a real
      two-field payload — nothing failed. There are now golden bytes for both the offered and
      the plain `HELLO`, computed independently, and for the specification's `REQUEST` layout.

    Not claimed: any throughput change. The VM has not been synced and no benchmark has been run
    for this or for hazard 40.

**42. Borrowing `RequestInfo` is worth about 0.1%, so it is not being done.** Hazard 39's closing
    note named `to_cgi`'s nine owned `String`s as "the natural next target". It was never
    measured — the figure quoted for it, "30 fixed + 2 per header", was actually the *worker
    request frame* from `volucer-server/tests/request_header_allocations.rs`, a different layer
    that the FastCGI path never pays. So the first thing was to measure `to_cgi` itself, and the
    measurement says stop.

    `crates/volucer-http/tests/to_cgi_allocations.rs`, release build, 200,000 iterations, across
    runs:

    | request headers | `RequestInfo` | environment build | `to_cgi` total |
    |---|---|---|---|
    | 5 | 647–714 ns | 2,126–2,342 ns | 2,900–3,900 ns |
    | 15 | 1,297–1,561 ns | 2,331–2,977 ns | 4,200–5,400 ns |

    `RequestInfo` is **19–34%** of the translation and grows by **~63–91 ns per request header**
    (four allocations at roughly 16 ns). The environment build is flat and three to four times
    larger — and *it* is where the other two allocations per header go, so a `RequestInfo` that
    borrowed would not even leave the header cost behind.

    Against `GET /items` at 617 µs on the worker arm and ~362 µs on the FastCGI arms, the whole
    translation is about **1%** of a request and the borrow work would recover about **0.1%**. That
    is for a change that makes `RequestInfo` generic over a lifetime, rewrites `build_params` and
    `build_params_for`, and threads that lifetime through every caller of both.

    Recorded rather than done, in hazard 41's shape: the evidence is about the translation path and
    no throughput number is claimed. The allocation counts are real and honestly four per header;
    the *time* is not there, and four allocations at 16 ns is not where a 617 µs request goes.

    What the file does instead is pin the **ratio** — `RequestInfo` under half the translation —
    so if the environment build ever gets cheaper, or `to_cgi` ever gets more expensive, a test
    says that the reasoning has changed and the borrow work has become the obvious next step.

    One correction carried forward: the earlier "30 fixed + 2 per header" was wrong for this path
    and should not be quoted. `to_cgi` is **64 allocations fixed, 4.07 per request header**, and
    its wall-clock translation is a few microseconds, not a large part of anything.



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
18. `fastcgi-frontend` — **done**, in `crates/volucer-server/src/frontend/`. Accepts FastCGI
    from nginx, translates it into the same `Dispatcher` the HTTP front-end calls, and re-frames
    the response, with a pass-through fast path for a record it can forward unchanged. It is
    wired in `volucer`'s `serve` alongside the HTTP listener: the FastCGI listener owns the
    calling thread while HTTP runs on its own, so both are served concurrently, and `serve` binds
    FastCGI first because binding is synchronous while the HTTP listener is not. `server.fastcgi`
    unset means disabled. `tests/frontend.rs` drives a real request through the whole chain and
    asserts the bytes come back intact, alongside the several-in-a-row, 502-on-backend-failure,
    real-socket codec and build-from-parser cases. **This step was implemented but the plan never
    said so** — it read as unbuilt until this line was corrected.
19. `http-frontend` — **done**, in `crates/volucer-http`: a `hyper` listener speaking HTTP/1.1
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
    "serves PHP" into "can front a site". See `crates/volucer-http/src/static_files.rs`, and
    hazards 11 and 12.
21. `config-cli` — **done**. `volucer serve|check|version`. The gap this closed was that the two
    settings that describe a deployment most concretely — the document root and the front
    controller — existed only as `serve` flags, so a systemd unit or a container image had to carry
    a command line to say where the application lived. Both are now `server.document_root` and
    `server.front_controller`, with **a flag winning over the file**. `version` prints the worker
    protocol version and the PHP floor from constants next to the code they describe, so a build
    cannot claim a floor it does not have. `check` gained preflight warnings that need the
    filesystem rather than the file — is the document root there, is the front controller actually
    present, is a discovered PHP below the worker floor — reported as warnings rather than errors,
    because a document root that does not exist yet is a normal thing to check. Reload was
    deliberately **not** built at the time: see open decision 5 — **built later as steps 53–59.**

Phase 7 — packaging
22. `packaging-ci` — release artifacts per target, systemd unit, Windows service notes, CI
    matrix, README and docs. **Deferred to Phase 10 on a stated reason rather than an oversight:**
    the prerequisites had already changed once mid-flight (Rust → Rust + C toolchain + NASM + CMake,
    hazard 54) and packaging written before that settled would have been written twice. They have
    now settled — the last TLS consumer is `rustls-acme` — so this is step 69 plus the Composer
    channel in step 70. **Step 69 is done**; the deferral was the right call, and it is the reason
    the prerequisites were known rather than discovered on the release machine.
23. `bench-harness` — load generator plus comparison against nginx+php-fpm, RoadRunner and
    php-cgi, to state realistic expectations rather than claims.

Later milestone
24. `tls-acme` — **split into 24a and 24b** (hazard 54). 24a is TLS termination, which is testable
    offline with a self-signed certificate; 24b is ACME issuance, which is a network protocol with
    its own failure modes, and bundling them means a review cannot tell which half a bug is in.
    24a: `rustls` + `tokio-rustls`, `aws-lc-rs` (measured to build here, hazard 54), ALPN for HTTP/2
    over TLS, and plain and TLS on one port or on two. 24b — the ACME half. HTTP/3 is `quinn` +
    `h3` + `rustls`, not `hyper`; `quinn` runs on `tokio`, which the HTTP front-end already has, and
    it stays out of both.
    **The `Sites` table is *not* the map SNI needs**, contrary to what this step used to claim:
    `Sites`' host maps are private and `ResolvedSite` carries no `hosts`. SNI needs its own
    host→certificate map; what is reusable is the *rule*, so it is extracted into `HostMap<V>` and
    shared by sites, SNI and route matching (hazard 54).
25. `prefork-optional` — Unix-only prefork to cut worker bootstrap cost, forked from a PHP boot
    parent (never the Tokio runtime).
26. `rpc-sidechannel` — only if a v2 need appears (background jobs, websockets) that the
    request/response protocol genuinely cannot express.

Phase 8 — sites (vhosts)

A `host` matcher in the route table is consulted *after* `SCRIPT_FILENAME` is resolved, so it can
choose a backend but never a document root. Serving two applications from one process therefore
needs the `Host` resolved first, which is what this phase adds. Steps 27–31 are the model, 32–36
the wiring.

27. `sites-model` — **done** (`b9d34c4`). `SiteConfig` and `Config::sites`, with `path_suffix` and
    wildcard `host` matchers on `RouteConfig`, `serve_only`, and a `static_files` key that takes a
    bare boolean. `deny_unknown_fields` was added to the backend structs, which were missing it:
    a top-level `routes` written after a `[backends.x]` header was being absorbed into the backend
    table rather than rejected, so the configuration silently had no routes at all.
28. `sites-resolver` — **done** (`b609cf6`). `volucer_core::sites::Sites`: exact name, then the
    longest `*.` wildcard, then `[server]`; ports and case are the client's spelling. It returns
    configuration rather than a `Routing`, because `Routing` lives in a crate that depends on this
    one. Two things the tests forced: the default front controller is applied here rather than
    left to the CLI, and `static_files` in a site table is a table of *optional* keys — a
    `StaticConfig` has defaults, so `{ precompressed = true }` would also have asserted
    `enabled = false` over the server's setting for the whole document root. The wildcards are
    keyed by suffix rather than held in a sorted list: since a wildcard matches a host exactly when
    the host ends with it, the host's *own* suffixes are probed in place of every pattern, which
    enumerates them longest-first for free and removes a `format!` per candidate. Two distinct
    suffixes cannot match one host at the same length, so the ordering tie the old sorted list
    documented is unreachable.
29. `route-matchers` — **done** (with 27). `path_suffix`, wildcard `host`, one `host_matches` shared
    by sites and routes, and the extended ordering.
30. `serve-only` — **done** (model in `b9d34c4`, behaviour in `3ae93f2`). The three states, the
    static-on-by-default rule with the HTTP-listener validation narrowed to an explicit enable,
    and the ordering change so `"backend"` does not 404 before the dispatcher runs.
31. `path-normalisation` — **done** (`950d59f`). A pre-existing hazard, not a vhost feature:
    `cgi::route` never refused `..`, so it reached `PATH_INFO`, `REQUEST_URI` and
    `SCRIPT_FILENAME` while the static layer refused the same shape. One predicate, both layers.
32. `spawn-fpm-decision` — **done** (`fec492c`). The unimplemented kind is removed and rejected by
    name, rather than advertised by `version` and refused at startup.
33. `sites-http` — **done** (`3ae93f2`). The host is resolved before the static decision and each
    site serves from its own prebuilt `Routing` and `StaticFiles`; `serve_only` and `skip_stat` are
    honoured; `to_cgi` and `to_cgi_routed` are separate entry points so the unsafe direction is not
    the easy one. `two_sites_in_one_process_serve_different_roots` is the test that proves it.
34. `sites-fastcgi` — **done.** The FastCGI front-end resolves the site from `HTTP_HOST` (falling
    back to `SERVER_NAME`) and dispatches against that site's own route table via
    `Dispatcher::dispatch_with_routes`, rather than always `dispatch_to_default`. Behind nginx the
    document root still cannot be re-decided — nginx resolved it, and the params are relayed
    verbatim — but the *backend* is now chosen the same way the HTTP front-end chooses it, so a
    route naming a host or path picks the same pool on either protocol.
    `a_route_selects_the_backend_for_a_fastcgi_request_too` is the test that proves it.
35. `sites-cli` — **done** (`10e7e48`). `check` prints the site table, each site's own settings,
    all three route matchers, and which routes skip the stat; preflight checks each site's root and
    controller. `--document-root` / `--front-controller` are refused when `[sites]` is present.
36. `sites-docs` — **done.** README "Sites", "Routing" and "Backends" sections; this file's
    architecture, decisions and step list.

Later, and deliberately not part of Phase 8:

37. `worker-sdks` — Python / Ruby / Node worker **clients**, mirroring `docs/worker-protocol.md`,
    one repository each. Nothing blocks them: `HelloAck` carries a free-form `runtime` and the
    handshake checks the *protocol* version, not the language, so a non-PHP worker handshakes
    today. What each needs is its own answer to "how does this process report its RSS", because
    the recycling contract is otherwise silently unenforced. **Order changed after step 68 was
    written:** `script-extensions` (38) comes *first*, and the first client should be **Go**, which
    is the one language that is also a measurement — RoadRunner is a Go worker server, so a Go
    client compares the two on the same runtime with no language confound.
38. `script-extensions` — make "what counts as a script" configurable per site instead of the
    hardcoded `[".php", ".php3", ".phtml"]`. Required before a non-PHP worker can serve `.py` or
    `.rb`: `path_suffix` can *route* such a path, but `has_php_extension` decides what resolves as a
    script, so today `/foo.rb` reaches the front controller as `PATH_INFO` instead. **It is step
    37's prerequisite and must land first.** There are **three** hardcoded lists, and the third is a
    security rule rather than a routing one: `cgi::has_php_extension`
    (`crates/volucer-fcgi/src/cgi.rs:208`) and its `PHP_EXTENSIONS` table (`:305`), the mirror in
    `is_script_extension` (`crates/volucer-core/src/config.rs:518`) — which exists because two lists
    of "what is a script" must not be able to disagree — and the static layer's refusal to serve a
    script from disk (`crates/volucer-http/src/static_files.rs:235`), whose comment says why:
    serving `index.php` from disk *"sends its source to the client"*. Widening the first two without
    the third turns every newly-routable extension into a source disclosure, so the three move
    together or the step is not done.
39. `plugin-abis` — only if a real need appears. RoadRunner's "plugins" are statically linked Go
    packages resolved by a compile-time DI container (`container/plugins.go`; there is no
    `plugin.Open` anywhere), so the enum backend kind stays the extension point.
40. `route-policy` — `RouteConfig` gains `policy` and `backend` becomes optional, so a route is a
    location rather than only a backend selector. §4.8 of the design notes.
41. `compression` — on-the-fly gzip/brotli, and the ETag and negotiation-cache questions it raises.

Phase 9 — TLS termination (step 24a, split from `tls-acme` by hazard 54)

The `Sites` table is a host→site map and SNI needs a host→certificate one, so the *rule* is shared
rather than the table: exact name, then longest `*.` wildcard, then the server's certificate. Steps
42–44 exist because that rule is currently written once and must be written once more.

42. `tls-provider-setup` — **done.** NASM 3.02 and CMake 4.4.4 installed and verified; `aws-lc-rs`
    confirmed to build and construct a real `ServerConfig` here, and the `ring` fallback confirmed
    too (hazard 54). Rustls 0.24 is pre-release on every crate involved, so 0.23 stable is the
    target.
43. `tls-config` — **done.** `TlsConfig` on `ServerConfig` and `SiteTlsConfig` on `SiteConfig`;
    `server.https` and `redirect_to_https`; validation for every combination the listener matrix
    allows and every one it does not; `check` reports the resolved certificate paths and says per
    site whether a certificate is its own or inherited; preflight reports a certificate that is
    missing, unreadable, or not PEM.
44. `host-map` — **done.** `volucer_core::hostmap::HostMap<V>` holds the rule (exact name, then
    the longest `*.` wildcard, then the caller's fallback — no default is stored, because what an
    unmatched host means differs between callers and inventing one is how SNI and site selection
    would come to disagree). `Sites` is re-homed on `HostMap<String>` and the eight vhost tests pass
    **unchanged**, which is the control for the refactor. Extracting it found a real divergence
    between the two implementations, recorded as hazard 54b and now pinned by a test that runs
    every pattern against every host and requires them to agree.
45. `tls-termination` — **done.** `crates/volucer-http/src/tls.rs` loads the chain and key, pairs
    them, and resolves SNI over `HostMap<Arc<CertifiedKey>>`; `serve_connection` is generic over the
    stream so one code path serves both protocols; `bind` builds the acceptor and **fails before
    binding** on a certificate it cannot load. A real bug was found by running it rather than
    reading it: rustls 0.23 installs its crypto provider lazily and `CertifiedKey::from_der` runs
    before any call that would install it, so the first version failed with "no usable crypto
    provider" on a machine where the provider was compiled in — `install_crypto_provider` makes the
    order irrelevant.
46. `dual-protocol` — **done.** `peek` on the first byte (`0x16` = TLS record) picks the path when
    `[server.tls].plain` asks for both on one port, which is what nginx's `ssl_preread` does; the
    second `https` listener is a second `RunningHttp`, and `redirect_to_https` answers cleartext
    with a `301`. Verified by hand and by test: cleartext is refused on a TLS-only port and served
    on a `plain = true` one, with TLS still working on the same socket. **The first version of the
    redirect was wrong and a test caught it** — with `plain` on it redirected TLS requests too, so
    a client would have been sent to the endpoint it had already reached, for ever. The redirect is
    now gated on the connection being cleartext, which is only knowable per connection because one
    listener carries both protocols.
47. `alpn-h2` — **done** (with 45). ALPN advertisement, and `alpn_negotiates_http2_then_http11`,
    the repository's first HTTP/2 test. The `Upgrade: h2c` gap remains pre-existing and unpinned.
48. `tls-cli` — **done, and the ordering it claimed was not the ordering it had** (hazard 56). `serve` parses the certificates for real and fails before binding; `version`, whose
    front-end line currently describes a build with no TLS.
49. `tls-docs` — **done.** README and this file.
50. `tls-verify` — **done.** End to end, self-signed: SNI selection, ALPN negotiation, the same-port
    sniff in both directions, the redirect, a certificate that fails before bind, and the TLS-off
    benchmark. Nine tests in `crates/volucer-http/tests/tls.rs` plus the `h2c` one in
    `tests/frontend.rs`. Two of the six rows did not exist when this step was first written and the
    "fails before bind" row was **not true** — hazard 56. The TLS-off benchmark is the only number
    in that work measured on a host rather than reasoned about: **-0.08%, 95% CI [-0.66%,
    +0.51%]**, so the honest form of the claim is "less than about 0.7%" rather than "nothing".
    Harness and rows in `volucer-competition`: `scripts/tls-off-ab.js`,
    `results/tls-off-ab.tsv`, `results/tls-off-ab-noise.tsv`.
51. `ci-build-matrix` — a build and test workflow. TLS is the workspace's first native dependency
    (C toolchain + NASM + CMake), and at the time there was no `.github`, no CI, no release artifact
    and no systemd unit, so "it builds everywhere" was verified only by hand on one host. The
    prerequisite list just grew from "Rust" to "Rust + C toolchain + NASM + CMake".

    **Expanded into Phase 10, and step 69 there is the reason — and step 69 is now done.** The
    matrix was written as a verification chore, which is why it kept being deferred; once packaging
    is a *deliverable*, the same workflow has an output to produce and a reason to exist. It is not
    superseded — it is step 69, and it is no longer last. It found the MSRV claim was false on its
    first run (hazard 74), which is the chore paying for itself.
52. `tls-acme` — **24b.** `rustls-acme`, whose default and recommended mechanism is TLS-ALPN-01 and
    needs no second port. The plaintext listener this phase adds is what HTTP-01 would need, so
    that option is open rather than blocked.
53. `reload-handle` — **done** (`a937f43`, moved into `volucer-watch` by `931daaa`).
    `volucer_watch::Swappable<T>` (an `ArcSwap` behind an `Arc`), and `listener::Reloaded` — the
    *one* value this listener's request path reads, holding a snapshot of the site table plus the
    per-site routings and static layers derived from it. One pointer rather than three, and that is
    the correctness: three separate swaps are not one snapshot, so a request could take a new site
    table with an old routing map and resolve a site the map has no entry for. The failure would not
    look like a race — it would look like a site serving the wrong document root, once, under load.
    `ReloadHandle::apply` is whole-or-nothing by construction (there is no setter for a single
    field) and returns the state it replaced, so a caller can report *what* changed.
54. `reload-sites` — **done** (`a937f43`). The request path takes one snapshot per request
    (`FrontEnd::state`) and moves it into the blocking dispatch task, so an in-flight request keeps
    the configuration it started under and a reload affects the **next** request. That ordering is
    the only one explainable to an operator — and it is now enforced *inside* the snapshot rather
    than by convention: `Reloaded::resolve` returns the site *and* its maps from one call, so a
    request cannot resolve against one generation and route against another.
55. `reload-tls` — **done** (`8c63807`). The certificate table is replaceable too, because a
    certificate that appears while the process is running has to be presented on the next handshake
    — which is the half ACME needs and cannot be worked around. `SniResolver` gained a `config_dir`,
    a `Swappable<Certs>` and two ways in: `refresh` (re-read the pairs a `CertificateSet` names) and
    `replace` (install a table already in memory, for a certificate whose bytes never came from a
    file this process wrote). Whole-or-nothing: the whole set loads before anything is installed, so
    one unreadable pair leaves the previous table serving untouched. `HttpListener` gained
    `resolver()` and `bind_with_resolver`, the latter reading **no certificate file at all** — the
    only way a deployment whose certificate does not exist yet can bind its port.
56. `reload-shared` — **done** (`931daaa`), and it was a live defect rather than tidying.
    `FastCgiListener` held its **own** `Arc<Sites>` while a reload replaced only the HTTP
    front-end's, so a reloaded site took effect over `:80` and kept reaching the old pool behind
    nginx — a difference that reads as *the other front-end being right* and that no single-protocol
    test can see. A new crate, `volucer-watch`, owns `SharedState` (the live table both listeners
    hold) and absorbs `volucer-http`'s `reload.rs`, because the mechanism was never HTTP-specific.
    Both front-ends now take a `SharedState` at bind time, `serve` builds exactly one, and the
    FastCGI listener resolves *per request* rather than per connection so nginx's keep-alive does
    not pin a connection to the configuration it was accepted under.
57. `reload-sighup` — **done** (`reload: SIGHUP reloads, and it used to stop the server`), and it
    was a **live defect rather than a gap**, which reading `ctrlc`'s source is what exposed. The
    plan said "there is no `SIGHUP` on Windows, so the watcher needs a `#[cfg]` split, and `main.rs`
    already owns a `ctrlc` handler with the `termination` feature that covers `SIGTERM`" — true as
    far as it goes, and it omits the part that mattered: **that feature registers `SIGHUP` too, and
    runs the same shutdown callback for it.** So `kill -HUP volucer` — the conventional reload on
    every Unix server — stopped the process. Reproduced against the previous binary before writing
    any fix: the process was gone and the log read "shutting down". `ctrlc`'s public API cannot say
    *which* signal arrived, so no handler written against it could distinguish a reload from a
    shutdown; signals are therefore handled per signal by `signal-hook` on Unix (the iterator yields
    the signal number), and `ctrlc` stays for Windows, where it is now a dependency without the
    `termination` feature. Hazard 60.

    **Open decision 9.1 is answered: (a), re-read the file as-is** — what an operator expects from
    SIGHUP, and no new concept. The reload replays the *command line* too (`--document-root`,
    `--front-controller`): a deployment started with `--document-root` on a site-less config takes
    its root from the flag, and a reload that dropped it would move the root to `.` — a working
    server that starts answering 404s, with the change attributed to whatever the operator edited.

    The trigger also forced the reload path to be one place. `Reloaded` and the site table's derived
    maps moved from `volucer-http` into `volucer-watch`, and a new `Registry` owns the shared table
    *and* every front-end's snapshot-holder, so `serve` and a signal handler apply the same call.
    Before this, only a listener could apply a reload — which is why the trigger could not be
    written at all without either duplicating the rebuild or reaching the table through one
    listener.

    Verified on Linux (WSL), because the mechanism is a signal and no Windows test can exercise it:
    `sighup_reloads_the_running_configuration_and_a_bad_file_changes_nothing` drives a **real
    process**. Falsified twice — stub `reload` to return immediately and the reload assertion fails;
    ignore the validation `Result` and the *refusal* assertion fails, which is the half that had to
    be reworked (hazard 61).
58. `reload-cli` — **done**, folded into step 57 rather than left separate, because the list is what
    the signal handler prints and splitting them would have meant writing the handler twice. The
    **restart-only list** is named on every reload rather than discovered — the lesson of the TLS
    story, that a setting the implementation cannot honour has to be *stated* or the operator finds
    out from behaviour. `restart_only()` returns the lines, so the text is also what a test can
    assert on.

    **Two entries in the plan's list were wrong, in the code's favour.** `static_files` on/off and
    `routes` were both listed as restart-only, and neither is: `[server.statics]` is read per request
    out of the swapped table, and a site's `routes` travel *inside* `Sites` and are read per request
    (`dispatch_with_routes(&site.routes, …)`). The published list is now listener addresses,
    backends and their commands/worker counts, and `[server.tls]` `min_version`/`alpn` — which is
    what a reader of the code would find, and what the README says.
59. `reload-tests` — **done.** The **measurement** is done: an interleaved A/B of two
    release binaries, `68bfdcc` against the reload work, both with no `[server.tls]` and both
    serving a static file — so the site resolution and routing lookup the snapshot feeds are on the
    measured path — reads **−0.09%, 95% CI [−1.43%, +1.27%], n = 20**, with an A-vs-A control at
    **+0.34%, 95% CI [−0.91%, +1.61%]**. The interval contains zero, so the indirection is not
    measurable here, and the honest bound is the interval rather than the point estimate.

    **The first ten-round run of the same pair read +1.75%, 95% CI [+0.56%, +2.95%] — excluding
    zero — and its control read −1.44%**, a point estimate larger than the effect it was supposed
    to give scale to and in the opposite direction. Twenty rounds changed the verdict without
    changing the binaries, because the interval on log-ratios shrinks as √n while the per-round
    spread does not. That pair of runs is kept in `volucer-competition` as
    `results/reload-cost-ab.tsv` and `-noise.tsv`, beside the 20-round ones that supersede them,
    with the harness at `scripts/reload-cost-ab.js`. A run that stopped at the first result would
    have claimed the reload made the request path **faster**.

    Unlike the TLS-off comparison this window **is** an isolation: `git merge-base --is-ancestor
    68bfdcc HEAD` succeeds and the range holds only the two reload commits, so no refactor rides
    along — which is hazard 56's lesson applied *before* the measurement rather than after it.

    **Still open:** none. The two that were open are now done, and both turned out to have a lesson
    worth keeping (hazards 62 and 63):

    - **Swap under *concurrent* requests** — `a_reload_under_concurrent_requests_is_one_configuration_per_request`
      in `volucer-http/tests/frontend.rs`. Two samplers and four requesters race one reload; the
      assertion is the one-pointer invariant (site, routing and static layer from one generation),
      with the three parts made individually identifiable so a mixture has something to be wrong
      about. Sampling is what makes it observable — **no request-level assertion can see this**, and
      that was established by falsifying it, not argued.
    - **A certificate *and* a site in one signal** — `one_sighup_reloads_a_certificate_and_a_site_together`
      in `volucer/tests/serve.rs`, Unix-only, against a real process: one `SIGHUP` moves both, read
      from a handshake *and* a request on the same listener. Falsified by deleting the certificate
      refresh from `Reloader::reload`, which fails the test and **passes every other test in the
      tree** — the reloader's certificate half was previously unpinned.
    - **The restart-only report** — now asserted rather than observed:
      `the_restart_only_list_names_what_cannot_be_replaced_and_nothing_else` pins the three classes
      that cannot be replaced *and* the two that an earlier draft wrongly listed, because the README's
      split is copied from this text and a drift between them is a documentation bug no test read.
      `volucer/tests/serve.rs` also now parses the listener banner's address by first token, so the
      `(https)` suffix `serve` prints cannot leak into an address.

    **A fourth gap, found later and not by this step's own reading** (hazard 71). The middle bullet
    below claims the reloader's certificate half was unpinned and is now pinned. The second test does
    pin it — for a deployment whose certificates are *named in the file*. For a deployment with
    `[server.acme]` it was the opposite of pinned: the reload rebuilt the table from the
    configuration, and an issued certificate is not in the configuration, so an ordinary `SIGHUP`
    silently returned the deployment to its self-signed fallback. It took the ACME phase to create
    the case, and the test that closes it (`a_reload_keeps_the_certificate_that_was_issued`) is
    committed with the ACME work. The lesson is about the shape of the claim: "the certificate half
    is reloaded" was tested, "the certificate half is reloaded *correctly for every way a
    certificate can get there*" was not, and only the second is what an operator depends on.

### Phase — ACME issuance (steps 60–64)

This phase has its own working plan (revision 2 of the reload/ACME plan), which is where the
reasoning and the open decisions live. What is recorded here is the outcome and the corrections,
because the tracked plan's step 52 was written from a reading of `rustls-acme` that two of its
claims turned out to understate.

60. `acme-deps` — **done.** `rustls-acme` 0.15.4 with `default-features = false, features =
    ["aws-lc-rs", "webpki-roots"]`. The second feature is the correction: `webpki-roots` is one of
    the crate's three defaults *and* the gate on `AcmeConfig::new` itself — the constructor that
    builds the `RootCertStore` verifying **Let's Encrypt's own** HTTPS endpoint. Dropping it, as the
    draft prescribed, removes the function, so the client cannot reach the directory. The error names
    no feature and suggests no cause. `tokio` is **off deliberately**, because it enables
    `AcmeConfig::tokio_incoming` — a ready-made accept loop that would mean adopting the crate's
    resolver rather than composing ours (step 61). One `rustls 0.23.46` in the graph, asserted by
    `volucer-http/tests/one_rustls.rs` rather than by a one-off `cargo tree`.
61. `acme-branch` — **done, and it is not an accept-loop branch.** `ResolvesServerCert::resolve` is
    handed the `ClientHello` *including its ALPN* (`rustls/server_conn.rs:129`, `ClientHello::alpn`),
    so the dispatch belongs inside our resolver — which is where `rustls-acme` puts it too
    (`acceptor.rs`). `LazyConfigAcceptor` was not needed, and adding it would have created a second
    accept path answering the same question. New crate `volucer-acme` holds the composition;
    `ChallengeResolver` deliberately does **not** implement `ResolvesServerCert`, so installing the
    ACME resolver where it could answer every SNI name does not type-check. Pinned by
    `the_acme_source_answers_only_an_exact_challenge_handshake` (three real handshakes: normal,
    `h2`+`http/1.1`, `h2`+`acme-tls/1`), falsified against a substring ALPN check — which first
    passed, because the stub owned the decision; the stub now delegates to the real resolver. Hazard
    62's shape.
62. `acme-engine` — **done, and it found the defect that would have made the phase decoration.**
    `AcmeState` installs an issued certificate into *its own* resolver and the bytes are not
    reachable from there, so reacting to `DeployedNewCert` with "install the certificate" would have
    installed nothing a handshake can see — and the symptom would not appear for **weeks**. What is
    reachable is the cache: `rustls-acme` writes the PEM there and reports `EventOk::CertCacheStore`
    when that has returned `Ok`. So `Engine::install_cached` reads it back, called before the first
    poll (so a restart serves its cached certificate and spends no rate limit) **and** on
    `CertCacheStore` (which is the renewal reaching the serving table without a restart — the
    deliverable of the whole phase). Falsified by disabling the second call.
63. `acme-config` — **done.** `[server.acme]` as a table rather than a flag, with `Authority` an
    enum so production must be named. **Open decision §9.5 was answered *no* here and is *reversed* by
    step 67** — see hazard 72. The reasoning that was wrong: "*a listener with nothing to serve fails
    every handshake that is not a challenge, so `[server.tls]` must name a fallback pair*". The first
    half is true and the conclusion does not follow — the invariant is that the listener has
    *something* to present, and **generating** one satisfies it. Requiring the operator to hand-make a
    certificate that is replaced moments later was a proxy for the invariant, and the proxy cost work
    for no safety. `domains` is stated rather than derived from `[sites.*].hosts` **in this table**,
    because a wildcard host pattern cannot go on a certificate and TLS-ALPN-01 cannot validate one —
    a rule step 67 keeps for the deployment-wide table and relaxes per site, where derivation is both
    safe and what an operator expects. Nine tests, each failing on the thing its own message names.
64. `acme-cli` — **done.** `check` reports the authority, the names, the contact, the resolved
    cache path, whether a certificate has ever been cached, and **the days left on each certificate
    being served** — which is the direct mitigation for this feature's failure mode, silence. A
    production authority is called out, and so is a missing `contact`, since that is the CA's only
    channel for warning that renewals are failing. `version` advertises `tls-alpn-01` as the one
    challenge this build performs. The README and `volucer.example.toml` both gained the block, and
    the example's now-wrong claim that `:80`/`:443` is the only shape that can host an ACME
    challenge is corrected.
65. `acme-verify` — **step 65's own verification, partly done, and the reason is stated rather than
    papered over.** The 12 `tests/tls.rs` and 27 `tests/frontend.rs` suites are **unchanged** — the
    control the phase was planned around, for "ACME is additive". Two new tests cover the seams that
    had no coverage:

    - `an_issued_certificate_is_served_without_dropping_the_others` (`volucer-http/tests/tls.rs`) —
      an in-memory PEM pair reaches the next handshake **and** installing it leaves a site's own
      certificate alone. Two claims in one test because they are one behaviour from either side, and
      falsified separately: dropping the *other* entries on install fails the second assertion, which
      is the half a `refresh`-style whole-or-nothing install would silently break.
    - `a_pem_blob_splits_at_the_key_not_at_the_second_block` (`volucer`) — a three-block blob (leaf,
      intermediate, key) gives a two-certificate chain, because `rustls-acme` hands over a chain plus
      a key as **one** blob and "the second block is the key" breaks the moment there is an
      intermediate.

    **The live run is where this step is honest about a limit.** Pebble was built and run here and
    volucer pointed at it; the account was created and cached and `newOrder` completed against a real
    ACME server — the rejections that followed were Pebble policy (`example.com` is on its blocklist;
    `pebble.test` has no public suffix), not protocol errors. The `tls-alpn-01` challenge was then
    attempted and the **validation step failed because the stub resolver Pebble was configured with
    never received the queries** — a harness gap on this host, verified by querying the stub directly
    (A and AAAA both answered) and seeing zero packets arrive. So: protocol up to `newOrder` verified
    against a live ACME server; validation and finalize not completed on this host; the full issuance
    has **not** been run end to end. Recorded as such, because hazard 68 is the lesson about claiming
    a feature works when only its parts do.
66. `acme-wire` — **done, and it was not on the plan's step list at all.** The five steps above were
    each committed and green before anyone noticed that `serve` never built an `Engine`: a complete
    `[server.acme]` table validated, printed nothing about ACME, bound its ports, served the fallback
    certification, and never attempted an order. **Hazard 68.** The plan named *parts* rather than the
    path through them, and no ACME test starts the binary, so nothing could see it.

    Fixed and pinned by `an_acme_configured_server_serves_its_cached_certificate` in
    `volucer/tests/serve.rs` — the real binary, a real `[server.acme]`, a certificate written through
    the crate's own `CertCache` trait, and an assertion that the handshake gets the *cached*
    certificate rather than the fallback. Falsified by making `serve` build no engine: it fails, and
    every other ACME test stays green.

    The wiring also needed one table behind two names (`SniResolver::from_shared`) and the engine on a
    thread of its own with a current-thread runtime, because `AcmeState` is not `Sync`. Both are
    hazards 69 and 70.
67. `acme-optional-and-per-site` — **done, and it reverses step 63's answer to §9.5 and adds a
    certificate per vhost.** Three changes with one theme: *the configuration should say what the
    deployment means, not what the implementation needs.*

    - **`certificate`/`key` are optional** when an ACME table will replace them. The placeholder is
      generated **in memory** (`rcgen`, promoted from a dev-dependency) for the names of the scope that
      needs it, and never written down — it is replaced, so a private key on disk for it would be a
      liability with no benefit. Validation refuses an absent pair with no ACME, which is what keeps
      the rule from being "generate and forget": the failure it prevents is a listener that looks
      healthy and is trusted by nobody, with the error moved from startup to the client's browser.
      Hazard 72.
    - **`[sites.<name>.acme]`**: a site's own order, account and certificate, inheriting `authority`,
      `contact` and `cache` from the deployment. One cache directory is safe and one account is
      *shared* — `DirCache` names a certificate `cached_cert_{hash(domains, directory)}` and an account
      `cached_account_{hash(contact, directory)}`, so distinct domain sets never collide and matching
      contacts need no work to share. Two sites with *different* contacts silently create two accounts,
      which is why the derivation is documented rather than left to be discovered.
    - **`domains` may be omitted on a site**, and is then derived from its `hosts` with every wildcard
      dropped and reported. A derivation that leaves nothing is an error naming the wildcards, not an
      empty order. The deployment-wide table keeps `domains` required: deriving across a whole
      deployment is exactly where a wildcard host bites.

    **Two real defects came out of falsifying this, both in the plumbing rather than the config:**

    - `install_issued` always installed as the **default** entry, and the default answers every name
      with no entry of its own. So one site's certificate answered for the whole deployment — a wrong
      certificate on a name nobody asked about, whose only symptom would be *another* site's browser
      warning. Fixed with an explicit `takes_default`, true only for the deployment's own table.
    - `default_names` preferred `[server.acme].domains` only after listing every site's hosts, so the
      deployment's placeholder was named for a site's hosts whenever ACME was on. Reordered to prefer
      the ACME names outright and to skip sites that have their own table.

    The test that found them, `a_site_with_its_own_acme_serves_a_placeholder_for_its_own_names`, is the
    one the earlier draft of it could not have been: an unpatched cache leaves **both** certificates
    as placeholders, so a test that only compares certificates to each other cannot tell which of the
    two scopes is wrong. It now writes a certificate into the site's own cache and asserts the site
    serves it **and** that the deployment's name does not. Falsified by forcing `takes_default` true,
    which fails on the second assertion alone.

    `check` gained a per-site ACME block with everything inherited made explicit, because printing only
    what the file states would leave an operator unable to tell inherited staging from defaulted
    staging. `volucer check` against the documented example was run rather than assumed, and its
    wildcard warning fires on the config the README recommends.

Phase 10 — being deployable, and being reachable from an application

Two questions arrived together and they are not the same question: *can a PHP developer install
this*, and *can a runtime that already owns its HTTP loop use it*. The first is packaging and
frameworks; the second is a new backend kind. They are ordered here so each one stands on
something that exists.

**The ordering is forced, and it is the phase's one real constraint.** `framework-bridges` (step
72) is not *technically* blocked by packaging — a bridge is a Composer package that talks to the
worker protocol, and the protocol is done. It is blocked *practically*: a bridge whose README says
"point your configuration at the `volucer` binary" is unusable when there is no released binary to
point at. So 68 → 69 → 70 → 71 → 72.

**Phase 7's deferral is honoured, not reversed.** Packaging was pushed last on the reasoning that
the prerequisites were still moving and would make the work be done twice. That reasoning was
right, and it is why `ci-build-matrix` is now step 69 of this phase rather than a chore before it:
the workflow is the same one, but it finally has an output, so it stops being deferrable.

68. `upstream-backend` — **done.** `kind = "upstream"`: a reverse proxy to an HTTP origin, for a
    runtime that already owns its HTTP loop. `http://`, `https://` and `unix:` origins, a pooled
    client, a streaming request and response body, and one front-end branch. Five commits:
    `56c8bd7` (the crate) `051e232` (the config) `bada54b` (the backend) `0b0334f` (the front-end)
    `f8a39ab` (streaming).

    - **A second request path is what it needed to be, and the reason is the CGI representation.**
      Every backend was reached through one shape — `Request { params, body }` built by `to_cgi` —
      which *is* CGI and loses exactly what an HTTP origin must preserve: header case, duplicate
      headers and the request line. So the front-end takes a branch instead of the dispatcher
      growing an arm, and `dispatch` **stays blocking**: every other backend is a process on a pipe,
      so making its type async to accommodate the one that is not would drag `spawn_blocking` into
      the type system of all four.
    - **The origin's body forced `ResponseBody` from `BoxBody` to `UnsyncBoxBody`**, because
      `hyper::body::Incoming` wraps `h2::RecvStream` and is `Send` but not `Sync`. The plan called
      this the step's main risk — "a whole-file change to the hottest code in the server, and if it
      moves any `static_files` test it comes back out". **It moved none**: two constructions
      (`body_of`, one `FileBody`), `FileBody` already `Send`, and `hyper`'s server accepts any
      `B: Body + 'static`. Hazard 78.
    - **`https:` is supported and always verified.** The plan's earlier draft refused it, on the
      reasoning that a second trust story was not what "use this for my Node app" needed. That was
      wrong: nginx does not refuse it, the components were already in the build (`tokio-rustls`'s
      connector, `rustls-pemfile`, `webpki-roots`), and since the `unix:` arm already forced a
      hand-written `Service<Uri>` connector, `https:` is that connector with one more step. There
      is **no `verify = false`** — the same rule that keeps a self-signed certificate off the
      serving port — and `[backends.<n>.tls]` beside a plaintext origin is refused rather than
      ignored, because verification an operator believes in and does not have is worse than none.
    - **A `unix:` origin was in the first commit, not deferred.** `Endpoint::parse` and
      `SlotAddress::Socket` already used that spelling, so an upstream that could only speak TCP
      would be the one backend that cannot reach what every other backend reaches. A Windows named
      pipe is **not** supported — `hyper` has no pipe transport — and the error says so by name.
    - **Three silent readers were fixed, and one of them had a real consequence.** `version`'s
      backend list, `check`'s per-request-reset line (which claimed "PHP SAPI" or "application-scoped
      worker", neither true of somebody else's runtime — `RequestScope` has three answers now), and
      the `stat`-skip rule. The last one mattered: `skip_stat`'s parameter was `backend_is_worker`,
      and `!is_worker()` is the *wrong* answer for an upstream because a CGI backend is also not a
      worker — so both `BackendConfig` and `Backend` gained `interprets_path()`. Falsified by making
      `Upstream` interpret the path: three unit tests and a new CLI test fail while the *pre-existing*
      CLI test still passes, so the new test is load-bearing rather than a restatement.
    - **The config's parser could not live where the plan put it.** The plan had `validate_backend`
      calling `volucer_upstream::Origin::parse`; `volucer-core` is the leaf crate and `Origin` is
      hyper-typed, so that inverts the graph. The parser moved into core as `OriginSpec` — hyper-free,
      the same reasoning that keeps `ChecksumMode` there as a mirror — and `Origin::from_spec` became
      the only constructor of the client-side type, so "the origin was validated" is structural rather
      than a habit. It also lets `preflight` describe a *structured* origin rather than only asking
      "does it parse", which is what its two new warnings need.
    - **Streaming and concurrency are asserted by timing, and both tests needed falsifying twice.**
      The response test reads the first of two chunks and checks the clock before the origin's second
      chunk exists — the one thing a buffering proxy cannot do. The concurrency test's first version
      was measuring *its own fixture*: a single-threaded accept loop, so five 400 ms requests took
      2000 ms and looked like a stalled front-end. Its second version *passed* when the upstream path
      was wrapped in `spawn_blocking`, because that pool is not a single slot — the real failure mode
      is a runtime with no thread left, so the test now over-subscribes the harness's two-thread
      runtime five times, and fails correctly when a blocking call is introduced. Hazard 77.
    - **`max_body_bytes` does not apply to an upstream**, and both halves are asserted in one test:
      the same oversized body reaches the origin whole *and* is still a 413 on the CGI path, through
      the same lowered limit. The CGI half is the contrast that makes the bypass a decision — the
      environment must carry `CONTENT_LENGTH` before PHP reads a byte, so it cannot stream there.
      This is a real change in exposure and the README says so beside the limit.
    - **One request is now routed once, not twice.** `prepare()` chose the route from the *site's*
      table to decide `skip_stat`, and `dispatch()` then re-routed through the *server's* table to
      pick the backend — so a site with its own routes could skip the stat for one route and dispatch
      to another. The FastCGI front-end already routed through the site; the HTTP one now does too.
    - **What it does not do: WebSockets.** The pool keeps the capability — `hyper-util`'s client hands
      a connection back after an upgrade — but the hop-by-hop filter drops `Connection` and `Upgrade`,
      which is what a handshake needs to survive. So the *only* change WebSockets need is a
      conditional skip of those two names, and SSE works today. Recorded rather than implied.
    - **Verification:** 553 pre-existing tests plus 62 new ones across five commits, green on Windows
      and on Linux under WSL (Linux is the only place the `unix:` arm compiles). Four falsifications:
      the path-skip rule, the buffering path, a blocking call in the async context, and the front-end
      branch itself — the last producing the 502 that is exactly hazard 68's signature.

69. `release-artifacts` — **done.** A build and test workflow for Linux/macOS/Windows, tagged
    releases with per-target archives and checksums, two Docker images, and the systemd unit whose
    `ExecReload=` is what makes step 53–59's `SIGHUP` reachable in production.
    - **What the workflow pins is three prerequisites, and only one of them was the expected
      one.** Rust, a C toolchain + NASM + CMake (`aws-lc-rs`, hazard 54) — and NASM turns out to be
      the one that bites: CMake and MSVC are on every runner image, NASM is on **none** of the
      three, verified from the image manifests rather than assumed. It is installed explicitly.
      `aws-lc-rs` carries a `prebuilt-nasm` feature that would avoid the assembler; it is not
      enabled, because a silently different crypto build is not a change to make for convenience
      and it has not been verified on all three targets.
    - **The declared MSRV was false, and CI is what found it** — which is the step earning its
      keep on its first run. `[workspace.package] rust-version = "1.85"` refused the workspace on
      1.85.1: `time` 0.3.55 and `time-core` 0.1.9 declare `rust-version = 1.88.0`, and they reach us
      through `x509-parser` → `asn1-rs` → `time` (and `rcgen`). Measured: 1.85.1 fails, 1.88.0
      passes. The number is now **1.88**, and the `msrv` job checks it against a real 1.88
      toolchain on every run rather than trusting the manifest. Hazard 74.
    - **A fourth prerequisite nobody had written down: the tree was not `rustfmt`-clean.** 29 files
      disagreed with rustfmt 1.10.0 under rustc 1.99, none of it recent — the committed code was
      formatted by an older rustfmt whose defaults have since moved, and nothing had ever run the
      check. Formatted in its own commit (`094c4f4`) so the gate in the next commit has a green
      base rather than a 29-file exception list. **The same formatting is stable across rustfmt
      1.88 and 1.99**, which was checked rather than hoped: a version-pinned formatter would make
      an MSRV job fail on whitespace.
    - **`VOLUCER_E2E=1` is what makes the test job mean anything**, and it is the reason the job is
      worth having. The suites that drive a real `php-cgi` and a real `example/worker.php` skip
      themselves when PHP or the SDK checkout is missing, and libtest hides the stderr of a passing
      test — so a machine with no interpreter reports the same green as one that ran everything.
      The workflow installs PHP, checks out `sailantis/volucer-php-sdk`, sets `VOLUCER_SDK_DIR` and
      turns a skip into a failure.
    - **Tagged releases produce what RoadRunner produces**, which was the shape to copy:
      `volucer-<version>-<target>.tar.gz` / `.zip`, a `.sha256` beside each, and a **draft** release
      — a tag is a decision and a broken release is public the moment it is not one. Targets:
      `x86_64`/`aarch64` Linux (`gnu` and `musl`), both macOS architectures, and
      `x86_64-pc-windows-msvc`. Each archive holds the binary, `README.md`, `LICENSE` and the
      systemd unit.
    - **The packaging is testable without a tag, and that is what makes it verified rather than
      written.** The staging and smoke-test halves live in `packaging/stage-archive.sh` and
      `packaging/smoke-test-archive.sh` rather than as inline `run:` blocks, because the only way
      to test an inline block is to push a tag. `packaging/release-selftest.sh` runs the whole path
      against a real binary on the host it is invoked on: stage, pack, **checksum, extract to a
      temporary directory, run `volucer version` from there**, and then corrupt the archive and
      assert the smoke test *refuses* it — a smoke test that passes on anything is a smoke test
      that proves nothing (hazard 62's shape). Run here under Git Bash: passes.
    - **Docker: two images from one crate, and neither is published.** A `glibc` image and a
      `scratch`-based `musl` one. The musl image carries its own `ca-certificates.crt` and names it
      in `SSL_CERT_FILE`, which is the one thing a static binary cannot work out for itself and
      without which ACME cannot verify the CA's own endpoint. Both are built and smoke-tested in
      the workflow with `push: false`; this repository has never pushed to a registry, and adding
      `push: true` plus a login is the only change needed.
    - **`cargo package --workspace` is a CI job now**, and it is the publish order's guard: a
      published crate cannot depend on a version that is not on crates.io yet, and `cargo package`
      verifies each crate in dependency order. It passes today for all eight crates.
    - **`.github/` did not exist before this step** — 0 workflows, 0 tags, no release. **The
      workflow is green**, and its first run found two test failures the single-platform
      development loop could not have (one a genuine macOS bug, one a port race). Hazard 75.
      The release path is written but has never run, because it needs a tag.

70. `volucer-cli-package` — **done**, and the built thing differs from this step in four verified
    details. `sailantis/volucer-cli` (31 files, 223 assertions) resolves the archive for this
    platform, fetches it to a temporary file, verifies the release's own SHA-256 **before
    extracting**, and moves the binary into place last.
    - **What `composer require spiral/roadrunner` actually does, verified rather than assumed:**
      nothing. The `composer.json` at that repository's root is `"type": "metapackage"` with no
      `require`, no `bin` and no `autoload` — `composer require` downloads metadata and not one byte
      of the Go binary. So the downloader CLI *is* the mechanism, and a metapackage alone would give
      a PHP developer nothing runnable. **Re-verified on Packagist; the claim holds exactly.**
    - **Four details this step had wrong, each found by reading the packages rather than the docs.**
      The downloader is **`roadrunner/cli`** (this step said `spiral/roadrunner-cli`, which exists
      only in that package's `replace` block); it lives at **`roadrunner-php/cli`**, not
      `spiral/roadrunner-cli`, which 404s; the download is **not in the CLI's code at all** but in a
      separate package **`internal/dload`** (`php-internal/dload`) reached through DLoad's `get`
      command, with the CLI's own `resources/dload.xml` deliberately empty; and DLoad selects assets
      by **regex** — `<repository … asset-pattern="/^roadrunner-.*/"/>` — so the interface between a
      release workflow and a downloader is an asset-name *pattern*, not a constructed URL.
    - **Decided against this step's naming.** It proposed `volucer/volucer-cli`; the package and
      repository are both `sailantis/volucer-cli`, matching the existing Composer vendor
      `sailantis/volucer-worker`. This org already separates repository name from package name — the
      SDK is repository `volucer-php-sdk` and package `sailantis/volucer-worker` — so it was a free
      choice, and one vendor for the project is the better one.
    - **Also decided against this step's binary name.** It proposed `"bin": ["bin/volucer"]`. Composer
      writes a PHP proxy into `vendor/bin` under the tool's name, and the tool's whole job is to
      install a binary *also* meant to be called `volucer` — so a shared name would have the download
      overwrite its own proxy, and the next `vendor/bin/volucer` would run the *server* with
      `get-binary` as an argument. The tool is `bin/volucer-cli` and installs `volucer`, and CI
      asserts that `vendor/bin/volucer` does not exist. **This one was only findable by reading how
      Composer installs a `bin` and then running a real `composer install`**, which is what the
      `install` job now does.
    - **The metapackage is deferred, not rejected.** Its value is a version-compatibility table, and
      step 72 is where a real compatibility matrix first exists. A second package versioned in
      lockstep with nothing to say yet is a cost with no reader.
    - **Stdlib-only, deliberately, against RoadRunner's shape.** `ext-hash` and `ext-json`; no
      `symfony/console`, no HTTP client, no lockfile. RR's CLI pulls four packages, which its
      `.rr.yaml` generator justifies; downloading one file does not. Requiring this adds one
      directory to `vendor/`.
    - **Verified against a local server, because no release exists yet.** The API base URL is
      injectable, so version selection, asset selection, the real checksum, real extraction and every
      refusal run against `php -S` serving a fixture release built from real tar.gz and zip bytes.
      A fixture server is not a stub: every class under test is the shipping one, and only the origin
      of the bytes is local. `VOLUCER_CLI_TRANSPORT` forces either transport, because a fallback that
      has never run is a fallback that does not work.
    - **Four defences, each falsified by disabling it and confirming the suite fails**: traversal,
      links, nested archives, and the checksum. Disabling the checksum installed a corrupt archive and
      the end-to-end test caught it. Hazards 82–84.
    - **Not chosen: a PHP extension.** FrankenPHP's model embeds PHP in the server, which is the
      design hazard 53 priced and this project rejected on grounds of isolation.

71. `sdk-session-helper` — small, and a prerequisite for the next step rather than tidying.
    - `docs/worker-protocol.md` currently tells an adapter author where the session handoff lives:
      *"Both adapters share one implementation of this, `Azera\Http\WorkerSession` in
      `sailantis/azera-framework`, which is where a third one should look first."* For a **public**
      SDK that is backwards — it points a third-party author at one vendor's framework to find a
      helper for a protocol that has nothing to do with that framework.
    - Move the two halves — read the session id from the request and write `Set-Cookie` on the
      response, with the between-requests reset — into `sailantis/volucer-worker`, where the rest of
      the worker contract already lives. The framework adapters then depend on the SDK instead of
      each other.
    - **Why it is a step and not a note:** the failure it prevents is silent. `session_id()`
      outlives the request in a long-lived process, so a client arriving with no cookie inherits the
      *previous* client's session — which passes every test that sends no cookie.

72. `framework-bridges` — a package per framework, and the work is per-framework state reset rather
    than the protocol.
    - **The protocol half is already written.** `sailantis/volucer-worker` is the SDK and
      `example/worker.php` is a working worker; `azera-volucer`'s `VolucerHttpWorker` is the
      reference shape — translate the request, run the framework's router and dispatcher, read the
      response through its getters, then reset per-request state. A bridge is that class with a
      different kernel underneath.
    - **The acceptance criterion is the reset, not the routing.** PHP assumes a fresh process per
      request; a worker breaks that assumption, and each framework repairs it differently. Symfony
      reboots the kernel between requests; Laravel has Octane's application factory and its flush
      listeners; Spiral was written for RoadRunner and fits closest. "It serves a request" is not
      the bar — *two consecutive requests that must not see each other's state* is.
    - **Laravel has a wall, and it should be recorded before anyone promises it.** Octane's
      engine list is hardcoded in `StartCommand` (`match ($server) { 'swoole' => …, 'roadrunner' =>
      …, 'frankenphp' => …, default => invalidServer(…) }`), so `--server=volucer` is an upstream
      pull request, not something a package can add. The standalone-bridge path is what
      `baldinof/roadrunner-bundle` did for Symfony — a third-party bundle needing no upstream
      change — and the same is available here.
    - **Candidate order by cost, not by popularity:** Spiral (closest fit), Symfony (a bundle, no
      upstream change), Laravel (bridge now, upstream PR as a separate decision), then
      Slim/Mezzio/custom PSR-15 (thin, and validates the SDK against a framework with no
      boot-cost story to protect).
    - **Verification is per bridge and behavioural:** the framework's own test that two requests do
      not see each other's state, run *through the worker*; and the SDK's own tests unchanged, since
      the SDK is what a bridge must not need to patch.

**What this phase does not do.** No PHP extension (FrankenPHP's model, priced by hazard 53 and
rejected for isolation). No multiplexed FastCGI — the specification has it (§1: *"the protocol
multiplexes a single transport connection between several independent FastCGI requests"*, and the
`request_id` is a `u16`) but no provider implements it, so a consumer would have to be written as
well as a producer. No HTTP/3, no `prefork-optional`, no `rpc-sidechannel`.

**Two things are recorded here that this plan previously did not mention at all**, because the user
asked about both and the honest answer was "neither is anywhere in this document":

### gRPC — a named non-goal, in the shape RoadRunner has, with the trigger that would unblock it

The user's recollection was that RoadRunner advertises gRPC "as a worker". That is **correct**, and
reading RR's own source settled what it actually is:

```yaml
grpc:
  listen: "tcp://127.0.0.1:9001"     # a real HTTP/2 server of its own, not a proxy
  proto: ["*.proto"]                  # descriptors for routing and reflection
  pool:
    command: "php my-super-app.php"   # the pool still speaks RR's own framed protocol
```

and on the PHP side (`tests/php_test_files/grpc/worker-grpc.php`) a Spiral `Server` registers
services and calls `serve(Worker::create())`. So **gRPC terminates in the server and the worker
protocol carries the message** — in, out, and the worker re-serializes. It is not `grpc_pass`.

**That shape fits volucer, and the earlier dismissal of this was wrong.** The objection was that
worker-style backends serve one request at a time, so concurrent HTTP/2 streams would serialize —
but that limit is a property of the worker protocol's *framing* (one request in flight per
connection), not of gRPC or of PHP, and RoadRunner does not multiplex either. So it is viable.

**Why it is nonetheless its own step and not this phase.** Two hard reasons, and neither is "it is
big":

1. **Trailers are entirely absent, and gRPC's status *is* a trailer.** `grep -i trailer` across every
   crate finds three hits, all of them in a *strip* list (`translate.rs:34`, `listener.rs:1502`,
   `client.rs:279`) and none of them a construction. `translate_response` copies
   `{status, headers, body, version}` (`translate.rs:152`), so `grpc-status` would be dropped and
   **every call would look failed**. This is shared work: it unlocks gRPC-to-an-origin too. It is the
   first deliverable, not a detail.
   — **Sharply qualified by [plugins.md](./plugins.md) §6.6.** Read that before acting on this item.
   The finding is correct about *this code* — nothing here constructs a trailer — but wrong about
   *the effort*, because the ability to carry one is already present and unused: hyper 1.12 surfaces
   trailers on ingress for both protocols (`hyper-1.12.0/src/body/incoming.rs:176`, `:209`),
   `volucer_upstream::incoming_body` preserves frames (`client.rs:248`), and the front-end hands an
   origin's body to the client without conversion (`listener.rs:1323`), since `UpstreamBody` and
   `ResponseBody` are one type. What is missing is a small `Body` impl that *yields*
   `Frame::trailers` — the same shape as the hand-written `FileBody` at `static_files.rs:806` — plus
   the status mapping. The two claims that survive unqualified are the shared-work point and the
   need for its own tests.
2. **nginx's record in exactly this code path.** nginx only gained HTTP/2 to an origin in **1.29.4
   (09 Dec 2025)** — `CHANGES:233` — and has since shipped **two memory-safety CVEs in it**:
   CVE-2026-42055 (heap buffer overflow proxying a crafted request to an HTTP/2 **or gRPC** backend,
   1.30.3) and CVE-2026-42926 (`proxy_set_body` data injection into an HTTP/2 backend, 1.30.1).
   Two in eighteen months, in a codebase with more hardening than anything else on the internet.
   Framing code gets its own step with its own test story.
   — *Mitigated, not dismissed:* volucer does not construct h2 frames at all, hyper does, so this
   exposure is the trailer-construction and status-mapping code rather than a hand-written state
   machine.

**What it needs, concretely** — restated from §6.6, which supersedes the list that was here:
the listener already speaks h2 with ALPN and prior knowledge both proven (the row at *What actually
works today*), and a gRPC route is `/pkg.Service/Method`, which `path_prefix` already expresses. The
remaining work is **three separable tiers**: (A) construct trailers and pass a call through to a
Unix-socket origin — no schema, no worker, no new listener; (B) carry the call to an application over
`CALL`/`RETURN`, still with the server opaque to the message; (C) reflection, the only consumer of
`.proto` descriptors, and optional. **No `.proto` parsing is on the request path in any tier**,
because gRPC's inner framing is length-delimited and therefore schema-independent. That replaces
this section's earlier assumption that `.proto` descriptors, `prost-build`/`protox` and a new config
surface were prerequisites — they are prerequisites of tier C alone.

**The trigger that should make someone start it:** a real application that wants to *serve* gRPC, as
opposed to a runtime that wants to *call* gRPC services. Note that the *calling* case is §6.6 tier A
and is now the cheapest gRPC work available, and that a runtime which already speaks gRPC — Node,
Go, Python — is reached through the same `upstream` backend that §7.1 adds for Django and Rails.
Recorded so the answer is "here is the design", not silence.

**Extended by [plugins.md](./plugins.md) §6.6, which corrects two things above.** The trailers
finding is right about the *code* and too pessimistic about the *effort*: hyper 1.12 surfaces
trailers on ingress for h1 and h2, `volucer_upstream::incoming_body` preserves frames, and the
front-end hands an origin's body to the client without conversion — so what is actually missing is
only the code that **constructs** a trailer, not the ability to carry one. And the `.proto`
question turned out to be a false choice: the server can route, frame and answer a gRPC call
without parsing any schema, because gRPC's inner framing is length-delimited and therefore
schema-independent; descriptors are needed only by an optional reflection service. That re-cuts
the work into three S–M steps rather than one XL one, the first of which is a pass-through to a
Unix-socket origin and involves no worker at all. The section also covers the *rest* of the
RoadRunner surface — metrics, a control-plane `CALL`/`RETURN` pair on the worker channel, KV, locks,
jobs, OTLP export, the Python/Ruby question — and §9 of it lists what is deliberately **not**
copied from RoadRunner.

### Observability — **done**, in six commits

`init_tracing` (`main.rs`) was `EnvFilter` + `fmt` and nothing else; there were **no spans anywhere**
(zero `#[instrument]`, `span!`, `Span` across every crate) and **no access log** — the only per-request
data was atomic counters read once for the shutdown summary. The signal path additionally used
`eprintln!`, which bypasses `tracing` and is exactly the line that breaks once logging is
provider-based.

**nginx's way is the target, and it is cheaper than it sounds.** nginx has no tracing framework:
`$request_id` is 16 random bytes hex-dumped to 32 chars (`ngx_http_variables.c:2298`), and
`$request_time`/`$msec`/`$bytes_sent`/`$status`/`$upstream_response_time` are variables
(`ngx_http_variables.c:288–373`) that `ngx_http_log_module.c` interpolates into an operator-chosen
format. One id, two timings, both on one line, correlated afterwards. Only OTLP export needs a
framework; a structured request line and W3C `traceparent` propagation do not.

**The user chose the core first and OTel as a layer on top of it**, and the reason is not caution:
OpenTelemetry exports *spans*, this server had none and no request identity, and adopting it first
would have meant wiring an exporter to nothing.

Six commits: `8fef784` (the signal path off `eprintln!`, and diagnostics to stderr), `6be089e` (the
new `volucer-observe` crate: `RequestId` and `TraceContext`), `9cabe8c` (the access log), `68b6bc3`
(the `[server.access_log]` config), `468ed73` (`Observability`, the one place identity is decided),
`f9090de` (both front-ends wired, plus `check`/`version`).

**A new crate rather than modules in `volucer-core`**, for the reason `volucer-watch` exists: there
are **two front-ends**, so a request id only one assigned, or an access log only one wrote, would make
the same application's requests appear differently depending on which protocol received them.

**Findings worth recording, because they shrank the work or changed the design:**

- **`traceparent` already reached PHP and origins unchanged**, before any of this: `to_cgi` maps every
  remaining header to `HTTP_<NAME>`, and `traceparent` is in neither strip list. So the work was
  generating and adopting the id, never forwarding it.
- **`getrandom` 0.4 is already in the lockfile twice**, so naming it added exactly one package —
  `volucer-observe` — and no external crate. Verified by diffing `Cargo.lock`, since the whole claim
  was that it was already there.
- **`fmt`'s default writer is `io::stdout`** (`tracing-subscriber-0.3.23/src/fmt/fmt_layer.rs:749`),
  which is the stream the test harness parses field by field for its startup banner. Moving the signal
  path into `tracing` without moving diagnostics to stderr would have emitted a log record *into that
  banner*. The two are one change, and stderr is also what lets the access log be a third, separately
  silenceable sink.
- **`RequestId` is an enum, not bytes.** The first version folded an inbound id into 16 bytes and
  printed that, which defeats adoption entirely — the client is looking for *its* string. Hazard 80 is
  what happened when the test for it was falsified.
- **OTLP/HTTP (port 4318) avoids `tonic`/`prost` entirely**, in a lockfile containing neither, which is
  also why RoadRunner defaults to `client: http`. That is the transport for the export layer.
- **Three falsifications, and two of them found defects in the tests rather than the code**: the ASCII
  guard on `TraceContext::parse` was *redundant* (removing it still passed, so it was deleted rather
  than kept as decoration), and the end-to-end verbatim test used an all-lowercase UUID and so could
  not detect a normalisation (hazard 80). The third — un-escaping the space in the access log —
  correctly failed on the field count (hazard 79).

713 tests, clippy clean, green on Windows and Linux (WSL). Hazards 79, 80 and 81 are this phase's own
record.

**Why the multiplexing refusal is written down rather than left implicit:** it is the question an
async-runtime user asks first, and the answer that satisfies them is not multiplexing. An async
runtime does not lack the *ability* to handle concurrent requests — `Node`'s event loop is the whole
point of choosing it — and one request per connection, of which `volucer` opens as many as there are
requests in flight (hazard 22: 3,916 → 27,657 rps at concurrency 32), already gives it that. What
worker mode actually withholds from such a runtime is a streaming response, and `upstream` is where
that is fixed. The `node-fastcgi` package is the proof that this is not a gap in the design: it
describes itself as a "near drop-in replacement for `node`'s `http` module", i.e. it too serves one
request per connection.

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
5. **Configuration reload.** Re-reading `volucer.toml` on `SIGHUP` without dropping a connection
   is the obvious next thing an operator asks for, and it is **built** — steps 53–59 above, after
   TLS. The original reason it was deferred still holds and is why it is tiered
   rather than wholesale: only part of the file can be reloaded in place. Listeners are bound,
   backends are running processes with live pipes, and both front-ends hold the dispatcher behind
   a handle. So the line is drawn deliberately, and the rule is that **everything behind a
   `Swappable` is replaceable *whole* and everything outside one is restart-only** — so the
   boundary is a property of the type rather than a comment a later change can contradict.
   The tiers:
   - **Tier 1 — reloadable.** `[server.tls]` and `[sites.*.tls]` certificate/key, `[sites.*]`
     `hosts`, `document_root`/`front_controller`/`serve_only`/`default_backend`/`routes`, and
     `[server.statics]` (including on/off), `redirect_to_https`, `https_port`. Steps 53–55 and 57.
   - **Tier 2 — restart-only, and therefore *reported* by every reload:** listener addresses,
     backends and their commands/worker counts, and TLS `min_version`/`alpn`.
   - **Tier 3 — was named as out, and turned out not to be: `routes`.** The original entry read
     *"`sites` and `statics` are reloadable; the route table is not"*, and **that was wrong about
     the code**: a site's routes travel inside `Sites`, and dispatch reads them per request out of
     the swapped table (`dispatch_with_routes(&site.routes, …)`), so they were already reloadable
     in `a937f43`. `static_files` on/off was wrong for the same reason — `[server.statics]` is
     resolved into each `ResolvedSite` and read per request. Both entries are corrected in the
     published list, which is drawn from the code rather than from this plan; there is no remaining
     third tier.
   **Settled: what `SIGHUP` reads.** (a) the file as-is, or (b) only a config the operator has
   already validated and marked. (a) is what an operator expects from a Unix server and needs no
   new concept, but it means a *staged* edit is applied by a signal sent for something else, and a
   mistyped file mid-incident turns a reload into a refusal — safe, since nothing is applied, but
   noisy. (b) is more machinery and no staging surprise. **Answered: (a).** Step 57 also replays
   the command line's `--document-root`/`--front-controller`, because they are part of what the
   configuration means rather than an override of it.

   **And the tiers are only as good as their reach** — hazard 59. The table used to be held
   per listener, so `[sites.*]` reloaded over HTTP and not behind nginx. "Does this reload
   everything it claims to" is a question about reach, and reach is not visible from inside one
   front-end, which is why the state is now a type both listeners must be handed.

   **A trigger is not a reload.** `SIGHUP` arrives as a signal number or it does not arrive at all,
   and a dependency that "handles signals" has handled them with *some* behaviour — hazard 60,
   where `ctrlc`'s `termination` feature had registered `SIGHUP` and wired it to shutdown, so the
   conventional reload signal stopped the server. Worth recording beside the tiers for the same
   reason the tiers exist: what a setting does is a fact about the implementation, and it is not
   in the plan.
6. **Swap mechanism — answered: `arc-swap`.** It is a new dependency and the only crate this phase
   adds. The read is the request path and `RwLock` still has every reader touch an atomic counter
   and block a writer; `ArcSwap` is a pointer load that no writer ever blocks. The alternative was
   a hand-rolled pointer swap, which is the kind of code that is easier to get subtly wrong than
   to review.

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

## 10. Where the work stands

Written to be the briefing a fresh checkout can read on its own, because the benchmark plan
that drove the last stretch of this work lived only in a chat session and is **not** in any of
these repositories. Nothing in it was lost — the two things it knew that no repository does are
the name research and the open steps below — but it cannot be re-read, so this section replaces
it.

**Everything in this section is committed and pushed, so this file is the whole answer to "where
do I read the state of the work from the other machine".** Nothing important lives in a session
folder, and nothing here needs the chat to interpret. The four places to look, in order:

1. **This section** — the current build, the open decisions, and the list of what is built.
2. **The step list above** (Phase 0–10) — which steps are done; steps 18, 19, 20, 21 and 69 carry a
   **done** marker and the rest say what is still open. Step 18 was implemented long before the
   plan said so; that marker is now corrected. **Phase 10 is half done as of step 69**: the server
   now builds and is packaged for six targets behind a CI gate, and what remains (68, 70, 71, 72)
   is making it *reachable* — an HTTP `upstream` backend, a Composer channel, the SDK session
   helper, and one bridge per framework.
3. **"What actually works today"** below — the same question asked of a running binary, per
   capability, with the probe that established it. This is the section that answers "does it do
   HTTP/2 / SSL yet" without reading code.
4. **The hazards** — numbered findings under "Operational hazards to build in from the start"
   and after it, each one a measured result with its controls. Hazards 45–49 are the Windows
   transport and FastCGI work.

The raw evidence for the benchmark claims is **not** here: it is committed to
`volucer-competition` as `results/raw-*.tsv`, alongside that repo's README, which carries the
conclusions. Anything measured on this host that is not in one of those two files did not
happen.

### The published benchmark and the current build

`volucer-competition/results/report.md` now names its build as **`91cc7b6`**, and every
throughput, sequential-latency and crossover table in it comes from that sweep. The four commits
that used to be the gap are measured rather than pending:

| commit | what it changes | throughput |
| --- | --- | --- |
| `9308ac4` | assemble a FastCGI response into a single write | 2–6% on `/`, nothing on `/items` (A/B, two binaries) |
| `270d20d` | stop rebuilding the request the pool already had | not claimed (hazard 39) |
| `26e4f11` | reuse the reply frame buffer, size the body once | not claimed (hazard 40) |
| `2549eb1` | the per-frame checksum becomes optional | ~0.26% of one request (hazard 41) |

Two things were established about them, both in hazard 50. The sweep at `841c963` that
followed them read 1–3% slower in *every* cell, which looked like a regression and was not:
every arm fell except the one the commits touched, and against a control the lead *grew* (P/R
+5.3% on `/`). And the `worker_pool.rs` split at `91cc7b6` was A/B'd against `ddaada7` on the
same idle host — median **−0.00%**, every cell inside the repeat spread — so the current sweep
is comparable to the one before it.

What is still not measured on this build: the two CPU-attribution tables ("Where the warm arm's
10% went", "The cold model's real cost is CPU"), which are carried over from the previous build.
Their conclusions — a duty cycle, not headroom — do not depend on the few percent that moved,
but the report says so rather than implying they are current.

### The rename, if it is ever made

Researched against the registries that actually bind a project — crates.io, the GitHub
namespace, npm, PyPI, Docker Hub and DNS. `volucer` itself is **still free on crates.io**.

| name | crates.io | GitHub | domain |
| --- | --- | --- | --- |
| `jetfuel` | **TAKEN** — placeholder crate, 2022, ~1.5k downloads | user taken | `.dev`/`.io`/`.app` registered |
| `firebox` | free | org taken (dormant since 2015) | `.dev`/`.sh` free, `.io` registered |
| `propellant` | free | org taken (dormant since 2017) | `.dev`/`.io` registered, `.sh` free |
| `scramjet` | free | user taken | `.dev`/`.sh` registered |
| `bowshock` | free | user taken | **`.dev` free, `.sh` free** |
| `combustor` | free | user taken | **`.dev` free, `.sh` free** |
| `zephyrix` / `plasmax` | free | user taken | both free |

- `jetfuel` is gone on crates.io and its npm, PyPI, Docker Hub and GitHub-user entries are all
  occupied. Not a clean rename target.
- **Every** candidate is taken as a GitHub *user* — that is what happens to short dictionary
  words. It does not block a crate, a Packagist package, a Docker image or a domain, and the
  project org stays `sailantis`.
- **Recommendation: `combustor`** — the chamber that contains and directs the burn, fitting the
  engine metaphor `volucer` came from, and clean on crates.io, npm and both domains.
  `bowshock` (the shock wave ahead of a supersonic vehicle) is the better pure-speed metaphor and
  equally clean; `zephyrix`/`plasmax` are the invented-fantasy option.

### Open decisions, restated

Open decisions 1, 2 and 5 are unresolved; 3 and 4 are answered in place. Of the hazards, 23 and
26 are closed (23 by hazard 33's CPU attribution, 26 by the shipped `spawn_cgi` socket support),
and 27 and 28 are recorded findings rather than open questions. What is left is a short list:

1. **Default backend out of the box** — open decision 1.
2. **msgpack behind the codec boundary** — open decision 2, deferred pending a profile.
3. **Configuration reload on `SIGHUP`** — open decision 5, *deliberately not built*, with the
   three options and the reason a half-reload is worse than none. **Answered and built: steps
   53–59.** It is kept in this list because the list is a record of what was open, and struck
   through rather than deleted for the same reason the hazards are.
4. **The rename** — a product decision, above.
5. **The re-run** — the five arms against current `main`, above.
6. **The worker pool's tail** — hazard 52, **fixed and measured** (`e2b031e`). The cause was
   measured rather than inferred: slot waits accumulated from concurrency 13, with a p99 of
   1–16 ms against a p50 of 2–19 µs. A fair queue replaced the scan, and on the idle host p99 fell
   60–81% (18.98 → 3.68 ms at c=32) with throughput *up* 13–21% and no refusals — checked against
   `wrk`'s non-2xx line rather than assumed. The gain was then re-measured **interleaved** (two
   builds in rotating order inside one round, RoadRunner as a control, 42 cells, 0 discarded)
   because a blocked layout cannot settle a number of that size on a host that drifts: it reads
   **+24.5% at c=24 and +22.9% at c=32**, spread clear of 1.00 in all 14 paired comparisons, with
   the pre-fix binary still landing exactly where hazard 51 left it (+3.0%/+5.7% over RoadRunner).
   The median at c=32 is 16% worse, which is the deliberate cost: the request that would have been
   served on contact now waits its turn (p50 8 → 256 µs at c=24) instead of one request waiting
   10 ms. Open now only as a separate question — whether the remaining ~587 µs of *mean* queueing
   at c=32 is worth attacking, which is not a fairness problem.
7. **Why the embedded runtime stops scaling** — hazard 53, **measured, not explained**. The sixth
   arm in `volucer-competition` is FrankenPHP, and it is indistinguishable from volucer at
   concurrency 1 (1.018x, spread spanning 1.00) and 32% behind at 32 (0.677x). CPU accounting rules
   out the easy answer: at concurrency 16 it consumes 7.17 of 12 cores while serving *fewer*
   requests per second than volucer, which uses 8.10 — so it is not CPU-bound, and its ceiling is
   in dispatch
   or queueing rather than in what its interpreter costs per request. Worth knowing because it is
   the shape a "PHP embedded in a Rust server" design would land in if the embedding were done
   naively; it is not a defect this project has to fix, and the arm exists to price a decision
   volucer already made on other grounds (isolation, a rebuild per PHP version) rather than to be
   beaten. Carries two stated confounds: the bundled PHP is 8.5.11 against this host's 8.3.35.

Hazard 46's open product decision — whether the worker transport should default to `socket` on
Windows — is **answered** in hazard 47: yes, by platform, with a startup hint for the one worker
shape it breaks. What remains genuinely open from that work is only the
`volucer-competition` lock question below.

Two items were raised in that same stretch and are recorded here rather than left in a chat:
**the crate doc's `Upgrade` claim is now corrected** (see "What actually works today"), and
**an NTS Windows build is the outstanding experiment** (same section) — it is the only way to
separate the ZTS confound from the platform effect in hazard 49, and it needs a host that is
genuinely idle, so it waits for one.

### What actually works today

The step list above is the *design* order and it says which steps are built; this is the same
question asked of a running binary. Probed by hand against `0.1.0` with a `spawn_cgi` backend and
static files on, because "the front-end speaks HTTP/2" is a claim worth confirming rather than
reading out of a crate description.

| capability | state | how it was checked |
| --- | --- | --- |
| HTTP/1.1 | **works** | `GET`/`HEAD`/`OPTIONS` on a static file, 200 with the right `content-type` |
| HTTP/2 cleartext, **prior knowledge** | **works** | `curl --http2-prior-knowledge` reports HTTP/2 and answers `HTTP/2 200`; a hand-built preface + empty SETTINGS gets a real SETTINGS frame back |
| HTTP/2 cleartext, **`Upgrade: h2c`** | **does not work** | a client offering `Upgrade: h2c` is answered **HTTP/1.1 200** with no `101` |
| TLS / HTTPS | **works** | `[server.tls]` with `certificate`/`key`/`min_version`/`alpn`/`plain`; `server.https` for a second listener; per-site `[sites.x.tls]` certificates selected by SNI; `redirect_to_https`. Verified by hand against a running server (a real `curl`/schannel handshake, `200` over HTTPS, cleartext refused on a TLS-only port, cleartext and TLS both answering on one port) and then by six integration tests that use `rustls` as a client, including `sni_selects_a_per_site_certificate`, which compares the presented certificate byte for byte |
| TLS / HTTP-2 over ALPN | **works** | `alpn_negotiates_http2_then_http11` — h2 wins when both are offered, and `http/1.1` when h2 is not. This is the **first HTTP/2 test** in the repository |
| HTTP/3 | **does not exist** | needs `quinn` + `h3`; step 24 |
| FastCGI front-end | **works** | `crates/volucer-server/src/frontend/`, wired into `serve`, 5 tests in `tests/frontend.rs` |
| `/index.php` is executed, not disclosed | **works** | the body is the script's output, not its source |
| static: `ETag` → 304 | **works** | the `etag` header echoed back as `If-None-Match` answers 304 |
| static: `If-Modified-Since` → 304 | **works** | 304 with the file's own `last-modified` |
| static: `Range` → 206, unsatifiable → 416 | **works** | `bytes=0-4` gives `206` and `stati`; `bytes=99-` gives 416 |
| static: `If-Range` | **works** | a matching validator gives 206, a stale one gives the full 200 — which is what RFC 9110 asks for |
| static: multi-range | **works** | `bytes=0-2,4-6` answers 206 `multipart/byteranges; boundary=volucer-boundary` |
| static: dotfile denial, directory redirect | **works** | `/.env` → 404; `/docs` → 301 to `/docs/`, then the index |
| static: no traversal escape | **works** | `/../secret.txt`, `/..%2fsecret.txt`, `/%2e%2e/secret.txt` and `/....//secret.txt` all fail to reach a file outside the document root — the encoded ones are treated as *names that do not exist*, not decoded into segments |
| reload: sites, routings and statics | **works** | `a_reload_changes_what_the_next_request_is_served` — the *same* address and the *same* listener answer from a different `document_root` after `ReloadHandle::apply`, asserted as a client sees it. In-flight requests keep their snapshot (`a_snapshot_survives_a_reload_landing_mid_request`) |
| reload: certificates | **works** | `a_certificate_reload_is_presented_on_the_next_handshake` — a renewed pair on disk is the one presented by the next handshake, on the same listener, compared byte for byte. `an_in_memory_certificate_needs_no_file_and_no_config` covers the ACME-shaped path, where the bytes never came from a file this process wrote |
| reload: whole-or-nothing | **works** | `a_certificate_reload_that_cannot_load_applies_neither_half` — a set with one unreadable pair installs **neither** half, proven by naming a *valid* pair alongside the bad one so a partial install would be visible |
| reload: a plaintext listener carries no TLS state | **works** | `a_plaintext_listener_has_no_resolver_and_still_reloads_sites` — `resolver()` is `None`, so there is nothing to load and no branch to take, and site reloading still works there |
| reload: **both** front-ends, not just HTTP | **works** | `a_reload_moves_the_fastcgi_front_ends_site_table_too` — the real socket, and **both requests on one connection** because nginx keeps its upstream alive. Falsified by moving the snapshot from per request to per connection, which fails with the old pool named in the 502. Hazard 59: the table used to be per listener, so a reload took effect on `:80` and not behind the proxy |
| reload: **one** apply path for the process | **works** | `volucer_watch::Registry` owns the shared table *and* every front-end's snapshot-holder, so `serve`'s own signal handler and `ReloadHandle::apply` are the same call. Before it, only a listener could apply a reload — which is why the trigger could not be written without duplicating the rebuild or reaching the table through one listener |
| reload: what it costs when nothing reloads | **not measurable here** | interleaved A/B of two release binaries, `68bfdcc` against the reload work, no `[server.tls]`, static files on: **−0.09%, 95% CI [−1.43%, +1.27%], n = 20**, against an A-vs-A control at +0.34%, 95% CI [−0.91%, +1.61%]. The *first* ten-round run read +1.75% **excluding zero** while its control read −1.44%, which is why the verdict is drawn from the interval and the control rather than the point estimate. Rows in `volucer-competition/results/reload-cost-ab-20.tsv` |
| reload: the **trigger** (`SIGHUP`) | **works, Unix only** | `sighup_reloads_the_running_configuration_and_a_bad_file_changes_nothing` — a **real process**, signalled. Reloads a changed document root on the same listener, and leaves the previous configuration serving for a file that does not parse *and* for one that does not validate. Windows has no `SIGHUP`, so it is skipped there with a reason. Hazard 60: `ctrlc`'s `termination` feature had registered SIGHUP and run the **shutdown** callback for it, so `kill -HUP` stopped the server |
| reload: a certificate **and** a site from one signal | **works, Unix only** | `one_sighup_reloads_a_certificate_and_a_site_together` — one `SIGHUP`, read from a handshake *and* a request on the same listener, against a real process. Falsified by deleting the certificate refresh from `Reloader::reload`: the test fails and **every other test in the tree still passes**, so this was the only thing pinning that half. Hazard 63: the two tables are applied in sequence on purpose, so the test waits for both rather than reading the window between them |
| reload: no request sees a **mixture** of generations | **works** | `a_reload_under_concurrent_requests_is_one_configuration_per_request` — two samplers and four requesters race one reload on a real listener; the assertion is that the site, its routing and its static layer always come from one generation, with the three made individually identifiable so a mixture has something to be wrong about. Sampling is what makes it observable: falsifying it by pairing the new table with the previous maps is caught **only** by the sampler, and no request-level assertion sees it at all (hazard 62) |
| reload: the **restart-only report** | **works** | `the_restart_only_list_names_what_cannot_be_replaced_and_nothing_else` — pins the three classes that genuinely cannot be replaced *and* the two (`routes`, `static_files`) an earlier draft wrongly listed, since the README's split is copied from this text |
| ACME: the composition, ours not the crate's | **works** | `the_acme_source_answers_only_an_exact_challenge_handshake` — three real handshakes through a real listener (normal; `h2`+`http/1.1`; `h2`+`acme-tls/1`) assert which resolver answered each, and the combined offer gets the **site** certificate because an exact ALPN match is what makes it not a validation. `rustls-acme`'s resolver returns one certificate for *every* non-challenge SNI name, so installing it directly would let a site present another site's. `ChallengeResolver` does not implement `ResolvesServerCert`, so that mistake does not type-check. Hazard 67: the first version's stub owned the decision, and the falsification passed |
| ACME: a renewal reaches the serving table | **works** | `a_published_certificate_is_read_back_into_the_serving_table` — driven on the real `EventOk::CertCacheStore` with a real `DirCache`, so only the network is stubbed. `AcmeState` installs an issued certificate into *its own* resolver, whose bytes are unreachable, so the certificate is read back from the cache. Falsified by disabling that call. Hazard 66: without it a renewal would work, be logged, and never be served — for weeks |
| ACME: only one `rustls`, and the features that build it | **works** | `exactly_one_rustls_is_in_the_graph` reads the lockfile; `the_acme_client_is_built_with_a_root_store` names `AcmeConfig::new`, so removing the `webpki-roots` feature fails the build with the reason. Hazard 64: the plan's own feature line removed that constructor, because `webpki-roots` is the `#[cfg]` gate on it *and* one of the crate's three defaults |
| ACME: staging by default, production named | **works** | `acme_defaults_to_staging_and_production_is_named` in `volucer-core`. A named enum rather than a `bool`, because the cost of getting it wrong is a real certificate for a name somebody owns, against per-week limits |
| ACME: config validation | **works** | `an_acme_domain_that_cannot_be_validated_is_refused_with_its_reason` (five cases, each asserting its own reason), `an_acme_contact_must_be_a_mailto_uri`, `a_duplicate_acme_domain_is_refused_with_both_positions`, `acme_without_a_fallback_certificate_is_refused`, `an_unknown_key_under_server_acme_is_refused`. Every one is something a CA would reject *after* an order had started |
| ACME: additive — off unless configured | **works** | `acme_is_absent_until_the_table_is_written` in `volucer-core`, and the 12 `tests/tls.rs` + 27 `tests/frontend.rs` suites unchanged, which is the control the phase was planned around |
| ACME: days-to-expiry visibility | **works** | `a_certificates_days_left_are_reported_with_the_warning_they_deserve` and `a_real_certificates_expiry_is_read`, both in `volucer` (the second falsified by making the parser report nothing). `volucer check` prints the authority, names, contact, cache state and days left, because silence is this feature's failure mode |
| ACME: the engine is actually **wired into `serve`** | **works** | `an_acme_configured_server_serves_its_cached_certificate` — the real binary, a real `[server.acme]`, a certificate written through the crate's own `CertCache` trait, and an assertion that the handshake gets the **cached** certificate rather than the fallback. Falsified by making `serve` build no engine: it fails, and **every other ACME test stays green**, so this is the only coverage of the wiring. Hazard 68: the five steps before it were each committed and green while `serve` never constructed an `Engine` — a deployment with a complete ACME config issued nothing, silently |
| observability: the id a client sends is the id it gets back | **works** | `the_access_log_records_the_id_the_client_sees` — the real binary, a real `[server.access_log]`, and a request carrying a **mixed-case** UUID. The id must come back verbatim (hyphens, case and all) and appear in the log file, which is read back rather than asserted on a struct. Hazard 80: the first version used an all-lowercase UUID, so lowercasing the echo **passed** — a value that already matches its normalised form cannot test a normalisation |
| observability: a hostile header cannot write this log line | **works** | `a_header_cannot_forge_a_log_record_or_shift_a_field` — a `User-Agent` containing a newline, quotes, a backslash and spaces. The count of fields is the assertion that catches it; every positional assertion passed while the line had 31 fields. Hazard 79 |
| observability: off by default changes nothing | **works** | `no_access_log_config_means_no_log_file_and_no_id_header` — no file created, and no `x-request-id` on the response, because a deployment that did not ask must not have an id appear on its answers. The unit control is `a_plaintext_deployment_is_unchanged_by_any_of_this`, which pins `access_log` absent alongside `tls` and `https` |
| observability: the trace is continued, not replaced | **works** | `an_inbound_trace_is_continued_and_a_missing_one_starts_unsampled` — the client's trace id is kept, this hop's span id is new, its sampling flag is respected, and a request with no `traceparent` starts **unsampled** rather than claiming to be recorded |
| Composer: the package **installs** and its proxy runs | **works** | `sailantis/volucer-cli`'s `Install (as a user would)` job: `composer require` into a fresh project against a path repository, then run `vendor/bin/volucer-cli`. This is the half a passing unit suite cannot see — the local suite calls the classes directly and requires `autoload.php` by hand, so it would pass with a wrong `bin` entry or no PSR-4 mapping at all |
| Composer: the archive the downloader asks for is the one the release builds | **works** | six asset names asserted against `release.yml`'s matrix, so a downloader that stops matching the releases it installs fails the suite instead of failing in the field |
| Composer: the tool and the binary cannot overwrite each other | **works** | the `vendor/bin/volucer` assertion. Composer proxies a `bin` entry under its own name, so a shared name would have the download replace its own proxy and the next run execute the *server* with `get-binary` as an argument |
| Composer: a corrupt download installs nothing | **works** | `EndToEndTest` flips one byte of a real archive and leaves the stored checksum alone: exit 1, "checksum mismatch", no binary, and **no temporary file left behind**. Falsified by removing the verification, which installs the corrupt archive — so this is the only thing between a hostile download and an operator's filesystem |
| Composer: the fallback transport is not just written | **works** | `TransportTest` runs curl **and** the stream wrapper against one fixture server, asserting the status for a 200, a 404, a 500 and a redirect on both. `StreamTransport` reported every status as `0` — `$http_response_header` is a local of the scope that made the request — which would have broken the tool on exactly the hosts that have no curl. Hazard 83 |

**The one surprising row is the `Upgrade` failure, and the mechanism is ours, not `hyper`'s.**
`hyper-util`'s `auto::Builder` handles the upgrade case only through
`serve_connection_with_upgrades`; [`listener`](../crates/volucer-http/src/listener.rs) calls
plain `serve_connection`, so the offer is never honoured. The crate doc at
`crates/volucer-http/src/lib.rs` used to claim `auto` negotiates "the prior-knowledge and
`Upgrade` cases" — the prior-knowledge half is true and measured, the `Upgrade` half is not, and
the doc now says so. Prior knowledge is what a real reverse proxy uses for h2c, so the practical
loss is small; a browser-facing listener would carry HTTP/2 over ALPN instead, which arrives with
step 24.

**Both HTTP/2 tests now exist, and one of them corrected this paragraph.** `volucer version` has
printed "HTTP/2 (cleartext)" since step 19, and for a long time the evidence for it was a hand
probe rather than a test — so a `hyper` upgrade that changed the negotiation would have been
caught by nothing. They were added with `tls-verify` (step 50):

- `alpn_h2_negotiates_a_real_http_2_connection` sends the connection preface over TLS with `h2`
  as the only ALPN offer and **asserts a SETTINGS frame (type `0x04`) comes back**, so the claim
  is that HTTP/2 works rather than that it was negotiated. `alpn_negotiates_http2_then_http11`
  covers the other half — which protocol ALPN selected.
- `an_upgrade_h2c_offer_is_answered_in_http_1_1` sends a real `Upgrade: h2c` offer and asserts no
  `101` and no `upgrade` header, which is what makes the gap a **decision** rather than an
  accident.

The sentence that used to open this paragraph was *"There is no HTTP/2 test anywhere in the
repository"*, which had already stopped being true when the ALPN test landed in step 47 — the
same stale-marker problem as step 18. Corrected here rather than deleted, because the way it went
stale is the point: a plan claim is only as good as the last time someone re-read it.

### The published Windows latency claim **is** now committed

Hazard 46's 21× figure (pipe 15.24 ms against socket 0.72 ms median on `/items`, 9 runs of 1000
requests each, ranges disjoint) survives in this plan and in the competition README, and as of
hazard 48 the raw rows behind it **are** committed — as
`volucer-competition/results/raw-windows-279dc35.tsv`, 70 rows (`P` = socket, `Pp` = pipe), plus
`results/raw-windows-fcgi-279dc35.tsv` for hazard 49's 27 interleaved rows. Both match the
`!results/raw-*.tsv` whitelist in that repo's `.gitignore`. Earlier text here predicted the
filename would be `results/raw-windows.tsv` and called the claim uncommitted; this paragraph
replaces it. The 21× and the 11× can be re-derived from the repository rather than taken on trust.

### What the first real Linux run found

`VOLUCER_E2E=1 cargo test --workspace` had never been run on Linux with a real PHP
present. It found three defects that cannot exist on Windows, which is where the suite
had been exercised:

- **43. The concurrency test did not fail, it hung.** `concurrent_requests_reach_the_backend_at_the_same_time`
  pointed an `external` backend at a lone `php-cgi -b` and expected four concurrent
  responses. A single Linux `php-cgi` serves its connections **one at a time**: it takes
  the next request on the connection it is already holding, because the client asked for
  keep-alive, and `PHP_FCGI_CHILDREN` is read only in the Windows branch of
  `sapi/cgi/cgi_main.c` (`cgi_main.c:1972`; the Linux path calls
  `fcgi_init_request(fcgi_fd, NULL, NULL, NULL)` at line 1969 with no accept hooks), so
  there is no child pool to distribute to. The four request threads sat in `recv` inside
  `Stream::read` for **17 minutes** with no timeout set — three in `wait_woken`, one on
  the scope's join futex. Measured directly: one request passes in 0.3 s, **two hang past
  60 s**. A real `php-fpm` is a master over a child pool and answers as many at once as
  `pm.max_children` allows, which is the thing `ClientPool` exists for — so the *fixture*
  could not exhibit the property under test. The test now uses a FastCGI server that
  answers each connection in its own thread (a stricter backend than php-fpm: one request
  per connection, then close), and a bounded receive turns a regression into a failure in
  five seconds. Negative control: serialising that server behind a mutex makes it fail in
  1.20 s with "close to the 1.2s they would take if they were serialised".

- **44. The socket-bound test never tested the bound.** `"a".repeat(..)` is a *relative*
  path, so `validate_cgi_socket` rejected it as relative before reaching the length check,
  and the `expect` passed for the wrong reason. On Windows the whole body was inside
  `if cfg!(unix)` and asserted nothing at all. The fixtures are now absolute and sit
  exactly on the boundary, with the boundary arithmetic itself asserted.

- **The checksum negotiation had never met a real PHP.** Both the worker-level and the
  pool-level test failed against the SDK at `5ede306`, which predates the flag — the ack
  came back `false` because the offer was ignored, which is precisely the documented
  backward-compatible behaviour and not a defect in either side. With the SDK at
  `a028ce3` both pass, so `NO_CHECKSUM` is now verified end to end with real PHP on both
  ends for the first time. **The polarity choice is what made this benign:** with
  `NO_CHECKSUM` (rather than `CHECKSUM`), zero means status quo, so an old peer keeps
  verifying and simply never agrees.

**And the claim that mattered:** the five-arm benchmark needed **no PHP-side change at
all**, because the application pins a SDK that predates the flag. An old SDK still serves
correctly against a new server — the compatibility direction that a `CHECKSUM` flag would
have broken silently. That is now a measured fact rather than a design argument.

Also worth recording because it is about the benchmark rather than the code: the first
`VOLUCER_E2E=1` run on the *same* tree was still passing when the harness was written,
so "the tests pass" was never evidence that the Linux path had been exercised.

- **45. The Windows named pipe is not just reachable, it is the faster transport — and the one
  that needed a deadline built into it.** `-b` on Windows goes down the `CreateNamedPipe` branch of
  `fcgi_listen`, and the SAPI detects it with `!GetNamedPipeInfo(..)`. That branch had been
  dismissed as a "different feature" for as long as the TCP arm looked adequate; with the pool's
  other options priced, it was worth measuring rather than assuming.

  **The measurement.** A 4 KiB FastCGI exchange over a *persistent* connection — which is
  `spawn_cgi`'s steady state, since `ClientPool::acquire` keeps one client alive per slot — 2000
  requests per sample, 9 samples:

  | transport | median | best | worst |
  | --- | --- | --- | --- |
  | loopback TCP | 210.9 µs | 207.9 µs | 222.5 µs |
  | named pipe | **168.3 µs** | 164.1 µs | 169.4 µs |

  The pipe is **20% faster and the spreads do not overlap** — the pipe's worst sample sits below
  TCP's best — so this is a signal rather than the noise-floor drift the five-arm re-run was
  careful not to claim. It is the mechanism `php-fpm` already uses on unix: a kernel object
  instead of a trip through the loopback stack. On the cold `/items` arm, where the backend leg is
  roughly 40% of a request, that is an end-to-end effect of about **8%** — larger than the last
  four optimisations put together.

  **Implemented** as `socket = '\\.\pipe\volucer-cgi'` on a `spawn_cgi` backend, which
  `validate_cgi_socket` now accepts and `cgi_socket_path` turns into one pipe per slot (the index
  is appended to the *name*, as it is to a path). The protocol layer needed no change at all:
  `Client::from_stream` already accepts any `Read + Write`, so this is purely a transport addition,
  plus an `Endpoint::Pipe` variant and a `SlotAddress::Pipe`. A pool-level test proves a two-slot
  pipe pool serves the same bytes the TCP pool does.

  **Three things the transport has to get right**, none of which a happy path would catch:

  - **Every operation is overlapped, because a blocking pipe read has no deadline.** This is
    hazard 43's missing timeout in new clothes: the handle is opened `FILE_FLAG_OVERLAPPED` and
    each read and write is bounded with `WaitForSingleObject`, so a backend that accepts and then
    goes quiet becomes `TimedOut` rather than a 17-minute hang. A test pins it — a stream that
    sends nothing must fail inside its deadline — and a *second* read on the same stream repeats
    the check, which is what exercises the drain path below.
  - **The `OVERLAPPED` must outlive the call that started it.** `CancelIoEx` only *asks* the kernel
    to cancel; the operation may still complete, and until it does the kernel owns the
    `OVERLAPPED` and writes its result into it. A stack-local structure would therefore be a
    use-after-free that no passing test reveals, so each direction owns a persistent `OVERLAPPED`
    in the stream, and one abandoned operation is drained before the next is started.
  - **`CreateFileW` fails with `ERROR_PIPE_BUSY` whenever the server is between clients** — the
    normal state for a per-request client, since `php-cgi -b` accepts one client at a time.
    `WaitNamedPipeW` in a loop bounded by the caller's deadline is what makes a reconnect work;
    measured before the transport existed, that path needed **0.88 busy-retries per open**.
    `FlushFileBuffers` is deliberately *not* used for `flush`: on a pipe it waits until the peer has
    read everything already written, and fails outright while the client is still connecting.

  **What it does not change.** Linux is unaffected: `php-cgi` binds a unix socket there and `unix:`
  already shipped, so the pipe is a Windows-only mechanism. It also does not re-open the
  `PHP_FCGI_CHILDREN` question — each pipe carries its own slot's instance, so one process per slot
  is still what the pool wants.

- **46. On Windows, PHP reads a pipe 90–150× slower than a socket — so the warm arm loses to
  the cold one, and the published conclusion is Linux-specific.** This was found by running the
  five-arm benchmark on Windows and getting a result that contradicted the published one: the
  *warm* worker measured 15.24 ms median on `/items` against the *cold* `spawn_cgi` arm's
  4.86 ms, where on Linux the same arms are 0.556 ms and 1.523 ms. A cold start twice as fast as
  a resident process is not a plausible result, so it was treated as a defect until it was
  separated from one.

  **What it is not.** Every alternative was eliminated by measurement rather than argument:

  | hypothesis | experiment | result |
  | --- | --- | --- |
  | Windows timer granularity | `timeBeginPeriod(1)` around the run | no change, still 15.2 ms |
  | Rust channel or timed wait | 4 shapes of `recv_timeout`, incl. the pool's own | 0.00–0.06 ms |
  | the pool, threads, HTTP | direct codec, no pool and no threads | still 15.085 ms |
  | which direction | client and worker stamps on one clock | **request 14.47 ms, response 0.71 ms** |
  | the pipe itself | Rust child echoing 8 bytes on the same pipe | **0.010 ms** |
  | the read strategy | `stream_select` vs `stream_get_contents` vs `fread`/`feof` loop | all identical |
  | a PHP version regression | 8.3.31 and 8.5.9 | identical |
  | this server, or this SDK | RoadRunner 3.0.0, `relay: pipes`, same app, 12 workers | **15.15 ms vs 15.24 ms** |

  Two independent servers and two independent SDKs agreeing to 0.6% is what makes it a property
  of the platform rather than of either implementation.

  **What it is.** PHP's own read, timed inside PHP, with the wake measured as `after − write_time`
  so that a `usleep` interval cannot be mistaken for latency, and with the write verified to have
  been issued after the stamp:

  | what PHP is reading | median | p95 |
  | --- | --- | --- |
  | a **pipe** | ~8 ms | ~15.4 ms |
  | a **socket** | **0.09–0.12 ms** (0.098 / 0.090 / 0.093 across 3 repeats) | — |

  So the transport is not merely slower, it is slower by ~100×, and the cost is *per request*
  rather than per connection.

  **The ratio was then checked the other way round**, on the warm arm itself rather than on a
  synthetic read probe, and at the full arm scale — which is what the caveat above asked for. It
  holds, and end to end it is *larger* than the probe suggests: see the table below.

  **Consequence, implemented.** `transport = "socket"` on a `worker` backend: the server binds a
  loopback listener before spawning, passes the address as `VOLUCER_WORKER_SOCKET`, and the
  worker dials back instead of using stdio. The PHP side is one call — `Worker::connect()` — which
  is `Worker::create()` plus the decision, so an application does not change and the protocol does
  not know which transport it is on.

  **The end-to-end result**, `GET /items` on the same application, same 12 workers, same host, the
  repository's own `latency.php`, 1000 requests × 9 runs per arm, marker verified on every run:

  | arm | median | worst run median | best run median |
  | --- | --- | --- | --- |
  | warm worker over a **pipe** | 15.24 ms | 15.274 ms | 15.203 ms |
  | warm worker over a **socket** | **0.72 ms** | 0.785 ms | 0.671 ms |

  **21× faster, a 0.06 ms spread across runs, and the two ranges do not overlap by two orders of
  magnitude** — the socket's *worst* run is 19× faster than the pipe's *best*. It puts Windows back
  in line with the Linux figure for the same arm (0.556 ms), which is what makes it a transport
  result rather than a Windows result.

  **The two defects the first version of this had**, both of which are the same class as hazard 43
  — a failure that hangs instead of failing:

  - **`accept()` had no deadline.** The comment said the timeout mattered; no timeout was
    implemented. A worker that connects to the wrong address produced no error anywhere, because
    nothing in the sequence reports a *missing* connection, so startup held for ever. It now polls
    against the handshake budget, and checks `try_wait` first: a worker that has already exited
    cannot connect, so its exit status is the useful part of the story and is reported
    immediately rather than after the full budget.
  - **`Worker::connect()` did not exist when this was first written**, which is a real
    compatibility trap rather than a test artefact: the worker file the *integration tests* drive
    is the SDK's example, and a checkout of the SDK that predates the method dies with a fatal
    error that the server correctly reports as "unsupported protocol version 97" — 97 being `a`,
    the first byte of PHP's error text. The same thing happened on the VM, whose SDK checkout was
    a plain checkout without the change, and again in `volucer-competition`, whose `vendor/`
    pins the SDK at `5ede306`. **A worker file that calls a method the pinned SDK does not have is
    a fatal error on startup, not a degraded path**, so the pin has to move with the server.

  **What it does not decide.** `Pipe` remains the default, because it is faster on Linux and Linux
  is where the unix-socket equivalent also lives; `Socket` is the Windows recommendation. Making
  it the Windows *default* would be the right call on the evidence, but it is a behaviour change
  for existing deployments and belongs with the other open decisions rather than in a transport
  patch.

**47. The worker transport now defaults by platform, and the reason it needed a hint.** Hazard
46 measured the Windows pipe read at ~8 ms median against ~0.09 ms for a socket — 21× on the
whole `/items` route, with disjoint ranges over 9 runs of 1000 requests — and left "should
`socket` be the Windows default" as a product decision. It is now **yes**: `WorkerTransport`
has a hand-written `Default` returning `Socket` under `cfg!(windows)` and `Pipe` elsewhere, so
a configuration that never mentions `transport` gets the transport its platform is fast on. The
setting is overridable both ways, and the choice is now logged (`worker pool started …
transport=socket`) because the transport is invisible on the wire: a pool on the wrong one
answers every request with a `200`, only slower.

**The trap, and why it is not a reason to keep `Pipe` as the default.** The transport is chosen
by the *server* and announced to the worker in `VOLUCER_WORKER_SOCKET`. A worker written
before the option exists — including anything following the SDK README as it stood, which
taught `Worker::create()` — reads `php://stdin`, which the socket transport has set to
end-of-file, and therefore exits during its handshake. The first full-suite run after the
default changed failed on exactly that: an *integration fixture* of ours, three files away from
the config, still calling `create()`. So the blast radius is real and it is not only third-party
code.

It is still the right default, because the failure is a **startup** failure, not a wrong-answer
one, and it is prompt: the worker exits in milliseconds rather than blocking, and `boot` already
reports a worker that died before connecting as soon as `try_wait` sees it. Measured: with the
default on, a `create()` worker is refused in under a second with exit status 1, not after the
30 s handshake budget. What that failure *was* missing is an explanation. "exited during startup
with exit code 1" sends a reader into PHP, where nothing is wrong, so `ServerError::StartupExited`
and `StartupTimeout` gained a `hint: Hint` field carrying `SOCKET_WORKER_HINT`: call
`connect()`, or set `transport = "pipe"`. The hint is attached only on the socket transport, so a
pipe worker's ordinary application failure is not given advice about a socket.

The hint is worded as a **condition** ("if the worker calls `create()`…"), not a diagnosis, and
that matters: the server's `accept` loop knows only that no connection arrived and the child has
exited, which is what a missing file, a syntax error and a `create()` worker all look like from
there. The first draft asserted the cause outright and was immediately wrong — an acceptance run
against a fixture with a *relative* path produced a file-not-found (PHP exits 1 for that) and the
message explained it as a `create()` worker. The wording now names the check and both ways out,
and points at the worker's own stderr, which `drain_stderr` logs above it.

**The alternative that would have avoided the trap entirely, and why it is impossible.** A worker
needs no change at all if the *server* hands it a socket as its stdin and stdout, because then
`php://stdin` *is* the socket. That works in Rust — `Command::stdin(unsafe {
Stdio::from_raw_handle(sock.into_raw_socket() as _) })` — and `into_raw_socket` means the handle
has exactly one owner, so there is no double free. It fails on PHP's side, and the reason is a
single fact worth recording: **Windows `GetFileType` reports a socket as `FILE_TYPE_PIPE` (3),
the same as a pipe.** Verified directly (`GetFileType` on a socket handle → 3, on `NUL` → 2, on a
file → 1), and it is documented behaviour: `GetFileType` is only a coarse classification, and a
socket handle is *also* a file handle that `GetFileType` calls `FILE_TYPE_PIPE`. PHP's stream
layer branches on exactly that, so a socket on fd 0 takes PHP's slow pipe read path anyway — the
8 ms stays. PHP's own `proc_open` cannot do it either: passing a stream resource as a descriptor
makes the child read a **bad file descriptor** (`fread(): Read of 8192 bytes failed with errno=9`),
so the PHP end cannot even set it up. The option is therefore closed by the platform, not by
preference, and `connect()` is the only route.

**A named pipe does not escape it either — measured.** Since volucer already speaks FastCGI
over a named pipe on Windows (`volucer-fcgi/src/pipe.rs`), the obvious follow-up question was
whether the *worker* protocol could use one: a named pipe is opened by **name**, not as the
child's stdio, so unlike fd 0 it would not be caught by `GetFileType`'s socket-as-pipe
coarseness, and it would still beat stdio because the server could create the pipe and hand the
name to the worker. So it was measured directly, with all three arms driven by the same writer
loop (so any probe overhead cancels) and PHP timing its own `fread` of one byte on a parked read:

| arm | channel | median | p95 |
| --- | --- | --- | --- |
| `pipe` | anonymous pipe (stdio) | **15.00 ms** | 15.56 ms |
| `named` | `\\.\pipe\name`, `fopen`'d `r+b` | **14.97 ms** | 15.56 ms |
| `tcp` | loopback socket | **0.012 ms** | 0.028 ms |

**A named pipe is the slow path, to within 0.2% of the anonymous pipe.** The control that rules
out the probe's own writer: the same `WriteFile` on the same named-pipe handle measures
**0.0031 ms median** — 5000× faster than the read it was paired with, and the write and read are
one round trip on the same handle, so the 15 ms is PHP's side of the exchange. PHP also reports
the named pipe as `stream_type=STDIO`, i.e. it places it on the same path as stdio.

Three details fall out of that and are worth keeping:

- **The cost is *per read*, not per setup.** A named pipe avoids the connection dial and the
  `connect()` API entirely, and still pays the full 15 ms per frame — so a worker protocol over
  one would be no faster than the pipe it already uses, while adding a name convention. The
  socket is not fast because it is *not stdio*; it is fast because it is a socket.
- **It refines the cause in the same direction as `GetFileType`.** PHP classifies the named pipe
  as `STDIO`/`plainfile` and the TCP stream as `tcp_socket/ssl`, and the two behave exactly as
  those classifications predict, so the ~5 Hz polling theory now has a concrete face: PHP takes
  its **stdio/plain-file read path** on anything that is not a socket, not a path specific to fd
  0.
- **It confirms the `spawn_cgi` pipe finding was a different measurement.** Hazard 45 found the
  named pipe *faster* than loopback TCP for a **request-sized** FastCGI request (~168 µs against
  ~211 µs). That is the same conclusion as this table: TCP's advantage is per *small park-and-wake
  round trip*, not per byte, and at ~4 KiB the per-syscall and polling costs are amortised while
  the socket's own stack overhead is not. Both numbers are real, and they are not in conflict —
  they are different regimes.

**Tests.** `a_worker_uses_the_better_transport_for_the_platform` and
`the_transport_can_always_be_stated_explicitly` pin the default and both overrides;
`a_worker_still_on_create_is_told_to_call_connect` runs a real `create()` worker on the socket
transport and asserts the message names `connect()` *and* `transport = "pipe"`;
`a_pipe_worker_that_dies_is_not_told_about_the_socket` pins the other side, that the hint is
transport-specific. That last one asserts only "a startup failure and no socket advice", not
which of the two startup messages it is: whether the exit status or the closed pipe is seen first
is a race, and it is not what the test is about.

The `accept_with_deadline` doc comment was also wrong — it claimed the listener was polled
*rather than* put in non-blocking mode, when the code does set it non-blocking and only restores
blocking on the *accepted* stream. Corrected.

**A third instance of the same class, in the tests.** The integration suite's `config()` helper
returns `transport: Default::default()`, deliberately — it is what a deployment gets. Once that
default became platform-dependent, every integration pool on Windows moved to the socket, and
`a_socket_transport_pool_serves_the_same_bytes_as_a_pipe_one` — whose whole purpose is to compare
the two transports — began comparing the socket with itself, silently, because its "pipe" arm was
`config(2)`. Nothing failed; the test simply stopped testing the pipe. That is the same trap as
the `create()` fixture, one level up: **a default that depends on the platform is invisible at
every call site that relies on it**, so each place that means a *specific* transport now says so
(`pipe_config()`, `socket_config()`) and only the sites that genuinely mean "whatever the
deployment gets" are left on `Default`.

**48. The loopback socket's real Windows advantage under load is 11×, not 21× — and the
missing 10× is PHP's CPU, not the transport.** Hazard 46 measured the socket at 0.72 ms
median against the pipe's 15.24 ms, sequentially, which is the like-for-like per-request
number. It is not, however, the number a saturated pool sees, and the difference is
large enough to be worth separating: an operator sizing a Windows deployment needs the
*ceiling*, and the two transports do not reach it the same way.

Measured on the Windows host (Ryzen 5 7500F, 6 cores / 12 threads, PHP 8.3.31 ZTS,
`GET /items`, 12 workers, both arms behind the same `volucer` binary at `279dc35`,
home-grown keep-alive load generator, 5 runs of 5 s per cell, marker verified and **0
errors in all 70 runs**):

| conc | socket rps | pipe rps | ratio | socket p50 | pipe p50 |
| --- | --- | --- | --- | --- | --- |
| 1 | 1,418 | 66 | **21.5×** | 0.658 ms | 15.11 ms |
| 4 | 5,297 | 265 | **20.0×** | 0.683 ms | 15.10 ms |
| 8 | 8,651 | 528 | **16.4×** | 0.903 ms | 15.09 ms |
| 12 | 8,528 | 775 | **11.0×** | 1.215 ms | 15.53 ms |
| 16 | 8,796 | 775 | **11.3×** | 1.547 ms | 16.04 ms |
| 24 | 8,655 | 774 | **11.2×** | 2.082 ms | 31.03 ms |
| 32 | 8,599 | 773 | **11.1×** | 2.504 ms | 45.34 ms |

Sequential per-request latency was checked with the repository's own `latency.php` in the
same session and agrees with hazard 46: socket **0.82 ms** median against pipe **15.26 ms**
over 9 runs of 1000 — 18.6×, and on the same route and host. Both arms served
byte-identical 9839-byte bodies (equal SHA-256), so the comparison is the transport and
not the page.

**Why the ratio *shrinks* as load rises — two different ceilings.**

- **The pipe arm cannot use more than one worker.** It scales linearly at **exactly 66
  rps per connection** to concurrency 8 (66 / 132 / 264 / 528), then stops dead and holds
  **772–775 rps whether 12 or 32 connections are offered** — 12 workers × 66 rps, within
  0.4% across every cell. The ceiling *is* the 15 ms read of hazard 46/47; nothing the
  client does moves it. Past 12 connections the extras only queue, which is why p50 climbs
  to 31 ms and then 45 ms with throughput flat. **No amount of pool tuning recovers this**:
  the process that would do the work is the one that is parked.
- **The socket arm is capped by CPU, not by the pool.** At c=12 it uses **61–63% of the
  whole 12-thread host** while the pipe arm uses **2.9–6.6%** — and at c=12 the measured
  per-request CPU is *lower* for the socket (0.80 ms against the pipe's 1.02 ms), because
  the pipe's workers spend their time blocked rather than working. So the socket's ~8.6k
  is a CPU ceiling that a second client cannot lift, and the pipe's 2.9% is the signature
  of a pool waiting.

**The controls, because the load generator is ours.**

- **The client is not the bottleneck.** One client at c=12 produced 9,306 rps and two
  clients at c=6 each produced 9,209 — the same number from twice the processes at the
  same offered concurrency, so the limit is server-side. The client's own cost is **0.06 ms
  of CPU per request** (½ core at 8,200 rps).
- **CPU attribution is not contaminated by respawns.** A replacement worker starts after
  the measurement begins, so `Get-CimInstance` would charge a whole PHP *startup* to that
  arm — 16.6 ms each, twelve times, is 199 ms, which is 10–13% of the ~1.7 s measured and
  exactly the kind of number that gets mistaken for a result. The PID set is compared
  across the snapshots so a contaminated row is flagged rather than believed, and the
  numbers above come from a run in which **neither arm respawned a single worker**.
- **The raw rows are committed**, as `results/raw-windows-279dc35.tsv` in
  `volucer-competition` (`P` = socket, `Pp` = pipe), which closes hazard 46's
  "the published Windows latency claim is not committed" — the 21× and the 11× can now be
  re-derived from the repository rather than taken on trust.

**Two things to read carefully, and one that is still open.**

- The socket's absolute 8.6k is **not** comparable to Linux's `wrk`-derived ~19.4k for the
  same arm: different host, different core count, different load generator. The
  like-for-like comparison is the sequential one, where socket 0.82 ms against Linux
  0.556 ms is the same order and pipe 15.26 ms against Linux 0.556 ms is not. This is why
  the ceiling ratio is the claim here and the absolute throughput is not.
- The pipe's 15 ms is not a *contention* effect — 66 rps at concurrency 1 is the same
  per-request cost as 775 rps at 32 — so the "one worker at a time" description above is
  deliberately narrow. What the numbers show is that the pipe arm's throughput equals
  66 × *min*(connections, 12) and nothing more; whether the pool could in principle have
  kept more workers busy is not what was measured, and a 15 ms stall on a read is what
  would make it not.
- **The one silent event, and a failed attempt to reproduce it.** The first socket sweep
  logged `a worker died; replacing it` for **all 12 slots at once** during a 32-connection
  cell, and because the socket transport sets the worker's `stdout` to `null` and worker
  `stderr` is logged at **`debug`** rather than `warn`, nothing the dead workers said was
  recorded. No request failed and no throughput moved, and it did not recur in any later
  run. An all-at-once death is the signature of an external kill rather than twelve
  independent crashes, so the ordinary candidates were tested against a reproduction build
  at `volucer=debug` (a fresh socket pool, 12 workers, **~94,700 requests** across three
  load shapes):

  | hypothesis | how it was driven | result |
  | --- | --- | --- |
  | connection teardown | 32 and 48 concurrent connections against 12 workers, headers read and the body *not* read before closing | 0 deaths |
  | mid-response abort | peer RST (`SO_LINGER` 0) after the header block, 1,280 aborts | 0 deaths |
  | a resource ceiling | 72,000 requests on one 12-connection pool | **working set flat at 35.3 MB** (min 35.3, max 35.6), private bytes constant at 14.1 MB, against a 128 MB `memory_limit` |
  | the pool's own recycling | `max_requests = 0`, so it is disabled by construction | 0 `recycling a worker` lines |

  The whole session produced **12 `worker ready` lines — the initial spawns only — and no
  `worker died`, no `worker stderr`, no error of any kind.** So it is not reproduced here
  and no cause is claimed. Two things are worth keeping anyway: the socket transport did
  not leak per request over 72k requests, and, more usefully, the event is a **silent**
  one by construction — at `info` verbosity there is nowhere a worker's dying words can
  appear, because `drain_stderr` logs at `debug` and a socket worker's `stdout` is null.
  The one thing it was not is a consequence of load: the deaths at 22:07 and 22:10
  happened while the *pipe* arm was the one under test and the socket arm was idle.

**49. Windows FastCGI is healthy at ~1,700 rps, and the transport question does not apply to
it.** Hazard 46 left Windows looking like a platform where FastCGI beats the resident worker,
because the 15 ms pipe read made the *warm* arm the slow one. That reading was about the pipe
and not about FastCGI, and it is worth settling with `spawn_cgi` measured properly on the same
host — because on Windows `spawn_cgi` is the only FCGI path (`php-fpm` does not exist there, so
the `external` and `spawn_fpm` backends cannot run, and the five-arm matrix's arm F has no
Windows counterpart).

Measured on the Windows host (Ryzen 5 7500F, 6 cores / 12 threads, **not idle** — the desktop
and editor hold 20–30% of it throughout, PHP 8.3.31 ZTS, `GET /items`, 12 slots per arm,
three arms **interleaved within each concurrency level** so background drift cannot be charged
to whichever arm ran during a busy moment, 3 rounds, 5 s per cell, **0 errors and 0 worker
respawns in all 27 runs**):

| conc | `spawn_cgi` + **named pipe** | `spawn_cgi` + TCP | `worker` + socket | worker/pipe | worker/tcp | pipe/tcp |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 274 rps | 267 rps | 1,201 rps | 4.38× | 4.50× | 1.028 |
| 12 | 1,641 rps | 1,631 rps | 9,717 rps | 5.92× | 5.96× | **1.006** |
| 32 | 1,728 rps | 1,693 rps | 7,716 rps | 4.46× | 4.56× | 1.021 |

**FastCGI's ceiling is ~1,730 rps against the resident worker's ~9,700 — about 5.6×, where the
Linux arms on the same route give 8,179 against 18,655, or 2.3×.** That is a *bigger* warm-over-
cold margin on Windows, not a smaller one, and it is the opposite of the direction hazard 46
suggested. The like-for-like cross-platform number is the sequential one, because absolute
throughput here is not comparable (half the cores, SMT, ZTS rather than NTS, a home-grown load
generator, and 20–30% of the host in use throughout):

| metric (`GET /items`) | Linux, 12 cores, idle | Windows, 6c/12t, 20–30% busy | Windows/Linux |
| --- | --- | --- | --- |
| resident worker, sequential p50 | 0.585 ms | 0.82 ms | 1.4× |
| FastCGI, sequential p50 | 1.52 ms | 4.6 ms | **3.0×** |
| peak throughput, resident worker | 18,655 rps (c=32) | 9,717 rps (c=12) | 0.52× |
| peak throughput, FastCGI | 8,179 rps (c=32) | 1,728 rps (c=32) | **0.21×** |
| warm-over-cold, sequential | 2.7× | **5.6×** | — |

**Windows is slower for both models, but the *cold* one pays much more of the penalty** — ~1.5×
for the resident worker against ~3× for FastCGI. So the resident worker is a *stronger* choice on
Windows than on Linux, which is the conclusion hazard 46 reached by a different route. One
confound is stated rather than neutralised: the Windows PHP here is **ZTS** (8.3.31,
`Visual C++ 2019 x64`) while the Linux run used **NTS** (8.3.35), and a thread-safe build is
measurable work for no benefit in a one-request-at-a-time SAPI. That is hazard 28's lesson —
a difference of build mistaken for a difference of server — and it is named here because this
measurement cannot separate it from the platform.

**The transport is a non-question here, and that is the finding.** Pipe and TCP are within
**0.6–2.8%** at every concurrency level — 0.6% at c=12, which is inside the run-to-run spread.
The mechanism is visible in the CPU column:

| conc | worker CPU/req | fcgi-pipe CPU/req | fcgi-tcp CPU/req | fcgi/worker |
| --- | --- | --- | --- | --- |
| 1 | 0.93 ms | 5.27 ms | 4.25 ms | 5.7× |
| 12 | 1.16 ms | 4.58 ms | 4.80 ms | 4.0× |
| 32 | 1.06 ms | 4.73 ms | 4.86 ms | 4.5× |

A `spawn_cgi` request costs **5×** the CPU of a worker request, because the difference is the
framework boot the CGI SAPI pays every time. The transport difference is **42 µs on a ~4.6 ms
request** — hazard 45 measured 168.3 µs for the named pipe against 210.9 µs for TCP, over the
FastCGI exchange alone — which is ~0.9%, and it matches the ~1.5% latency difference above.
**Hazard 45 extrapolated that 42 µs to "an end-to-end effect of about 8%" on the `/items` arm**,
by assuming the backend leg was roughly 40% of a request. It is not: the exchange is ~4% of a
~4.6 ms request, and the prediction is not borne out at any concurrency level measured here. The
number hazard 45 *measured* is sound and is not in dispute; the end-to-end inference from it is
what this replaces.

Where the same shape of reasoning does hold is the worker, and that is the whole of hazards
46/47: there the pipe read was **15 ms against a ~1 ms request** (0.82 ms sequential), because a
worker's read *is* the request, whereas a FastCGI exchange is a rounding error on one. That is
the difference between a 21× transport effect and a 1% one, and it is why `transport = "socket"`
is load-bearing for a worker and irrelevant for `spawn_cgi`.

**And a correction the interleaving forced.** A block-ordered run of the same two arms
reported the pipe **22% faster** at c=1 (5.29 ms against 4.34 ms) and 22% *slower* at c=32
(15.98 against 18.18). Alternating them round by round on the busy host gives the opposite of
the first and no support for the second:

| sequential, alternated 3× | p50 range across rounds |
| --- | --- |
| `spawn_cgi` + named pipe | **4.50–4.76 ms** |
| `spawn_cgi` + TCP | **4.59–4.68 ms** |

They are the same to 1.5%, and the pipe's worst round beats the TCP's best. **A 22% effect that
reverses sign between two runs of the same arms is drift, and on a host with 20–30% background
load a block layout manufactures exactly that.** This is the reason the three arms above are
interleaved rather than measured one after another, and it is worth stating as the method note
it is: on a shared host, compare by cycling, not by blocks.

**Two things checked rather than assumed.** `php-cgi` and the `php` CLI read the *same*
`php.ini` on this host with identical values (`opcache.enable=1`, `opcache.enable_cli=1`,
`memory_limit=128M`, xdebug off), so unlike the Linux harness — which needs its
`php-cgi-bench` wrapper because `php-cgi` reads `opcache.enable_cli` and `php-fpm` does not —
there is no ini confound here and no wrapper was used. (`opcache.jit=tracing` is set but
`opcache.jit_buffer_size=0`, so no JIT is running on any arm.) And the 24 parked `php-cgi`
processes were confirmed to consume **exactly 0 CPU** over a 5 s idle sample, so leaving the
FCGI pools running does not pollute another arm's measurement — they are blocked, like hazard
48's pipe workers.

**50. The leak guard could not see the arms it guarded, and "not a regression" needed a
measurement rather than an argument.** Two findings from re-running the competition's five arms
at `91cc7b6`, both about evidence rather than about volucer's code. Neither is a defect in the
server; both are defects in the *instrument*, which is why they are recorded here.

*The guard.* `bench.sh` reported "8 stray php-cgi pools after teardown" and the report explained
them away. The pattern was `pgrep -f 'php-cgi -b'`, but arms T and U spawn PHP through the
`php-cgi-bench` wrapper, whose argv inserts its `-d` flags **before** `-b`:

```
/usr/bin/php-cgi -d opcache.enable_cli=1 -d opcache.validate_timestamps=0 \
    -d xdebug.mode=off -d memory_limit=512M -b 127.0.0.1:19300
```

So the literal string never appears. Measured on the bench host: `pgrep -f 'php-cgi -b'`
returned **0 while 12 of arm T's own children were alive**, and still **0 after a `kill -9` of
the server left all twelve orphaned**. The guard was therefore not merely imprecise — it was
blind to the arms under measurement, and would have reported clean through any leak the wrapper
produced. The "8 strays" it did catch were leftovers from older configs that invoked
`/usr/bin/php-cgi` directly, which is exactly why the number looked plausible enough to explain
away. The fix matches `-b` as its own word via `ps -eww` (the `-ww` is load-bearing: without it
a long wrapper command line is truncated and loses the flag) and runs after **every** arm,
refusing to continue if a leak cannot be cleared. `pgrep -x php-cgi` would also have worked and
would have been the wrong fix — it matches php-fpm's children on a host that serves its own
sites, and the first version of the reap that used it killed 21 of the host's `www`-pool
children. The lesson generalises past this harness: **a process-name pattern is a guess about
argv, and a guard that cannot observe the thing it guards reports success, not failure.**

*The reading.* The competition's first re-run of all five arms read 1–3% slower than the sweep
before it in every cell, and the note in the report dismissed it as drift, citing a
"clean re-run of R and F" that was **never committed** — a sentence, not data. Re-deriving it
from the two committed TSVs shows the pattern that note missed: every arm fell **except the two
the four intervening commits touched**.

| arm | what it is | median change | cells that fell |
| --- | --- | --- | --- |
| P | volucer worker — **the arm the commits changed** | **+0.35%** | 2/10 |
| T | volucer `spawn_cgi` | −1.54% | 8/10 |
| U | volucer `spawn_cgi` unix | −1.77% | 8/10 |
| R | RoadRunner — untouched | −2.79% | **10/10** |
| F | nginx + php-fpm — untouched | −1.63% | 7/10 |

Cancelling the common mode against a control makes it unambiguous: **P/R moved +5.3% on `/`
(lead grew in 5/5 cells) and +1.35% on `/items` (5/5)**. So the four commits did not cost
anything, and the absolute rps column is not comparable between sweeps — which the report now
says explicitly. This is hazard 28's shape one level up: there, a difference of *ini* was
mistaken for a difference of server; here a difference of *host condition* was mistaken for a
difference of server, and the control arms are what separate them.

*And the refactor was measured, not argued.* P read ~3.3% below the previous sweep at `91cc7b6`,
above every control arm, and the only code change since `ddaada7` is the `worker_pool.rs` split —
a pure move, which by construction cannot cost throughput. That reasoning is exactly what this
project has been wrong about before, so two binaries were built and alternated `A,B,B,A` on the
idle host, 5 s × 3 repeats per cell: median **−0.00%**, 5/10 cells each way, every delta inside
the 3.3% within-cell repeat spread. The refactor is free, and the ~3.3% was drift. The `A,B,B,A`
layout is deliberate — a block layout charges linear host drift to whichever binary ran second,
which is how a 22% "effect" appeared and reversed sign in hazard 49's own measurements.

*One process hazard, for the next run.* **Stopping the SSH client does not stop the remote
harness.** `stop_powershell` kills the `ssh.exe` locally; the remote `bash` survives, and because
its `EXIT` trap never fires, `bench.sh` keeps serving and the next run contends with it. That
happened here — two sweeps ran concurrently against the same 12 cores (load 4.33, two `wrk`
clients, two server pools) and produced a P column ~25% low before it was caught. Cleanup is
explicit: `pkill` the launcher, then `pkill -f 'volucer serve'` and let each server's `Drop`
take its own children, then reap any `php-cgi -b` that ignored SIGTERM.

**51. The crc32c fix is worth 10–16%, and hazard 41 measured the checksum against half its own
    cost.** The question that started this was the right one to ask and was asked for the right
    reason: the lead over RoadRunner *looked* lost again in the latest sweep. An interleaved A/B
    settles it, and the answer is that the effect is not merely still there — it accounts for the
    whole lead. Two corrections fall out of it, one about a number this document already
    published and one about the host the benchmark was measured on.

*Method, because the previous attempt at this question was a trend and a trend cannot answer it.*
Five committed sweeps were compared cell by cell and could not separate a real movement from the
host's own drift: in the one sweep that looked like a regression, every arm fell — including
`RoadRunner` and `nginx + php-fpm`, which none of the intervening commits touch. So this is a
different instrument. Four variants, every one measured **inside the same round**, and the
starting order rotated by round so that a drift of any shape charges every variant equally:

| variant | what it is |
| --- | --- |
| `new-req` | current `main`, `checksum = "required"` — what every published number used |
| `new-opt` | current `main`, `checksum = "optional"` — the knob hazard 41 added |
| `old-req` | current `main` with **only** `crc32c` reverted to the hand-written loop |
| `rr` | RoadRunner, resident worker — shares no code with any of the others |

`rr` is the control and the reason the result is legible: a round in which every volucer variant
moved but `rr` did not is a round in which volucer moved. Seven rounds × 2 endpoints × 3
concurrencies = **168 measured cells, 0 failed**, and the guard from hazard 50 reports clean after
every variant. The estimator is a ratio computed *within* a round, so its spread across the seven
rounds is the error bar — which is what a blocked layout throws away.

*The revert is targeted, and that matters.* Building `07fea44^` would have been simpler and would
have isolated nothing: it is twenty commits back and bundles the FastCGI parser de-allocation
(5.2%), the single-pass `parse_response` (5.2%) and the request-path copies along with the
checksum. So only `header::crc32c` was restored to the bit-by-bit loop it replaced, in a second
checkout, and the two binaries were confirmed to differ in behaviour not at all: both return
**byte-identical** bodies for both endpoints, and the pinned `crc32c_matches_the_standard_check_value`
test still passes on the reverted tree. The value is what has to match, not the implementation —
which is the point the original commit made and this confirms from the other side.

| ratio, median over 7 rounds | `/` c=8 | `/` c=16 | `/` c=32 | `/items` c=8 | `/items` c=16 | `/items` c=32 |
| --- | --- | --- | --- | --- | --- | --- |
| `crc32c` fix (`new-req`/`old-req`) | **+11.8%** | **+11.4%** | **+16.3%** | **+10.2%** | **+12.1%** | **+13.7%** |
| checksum option (`new-opt`/`new-req`) | +2.0% | +2.9% | +2.5% | +3.0% | +2.7% | +2.8% |
| lead over `rr` (`new-req`/`rr`) | +18.0% | +4.6% | +4.2% | +3.3% | **−1.7%** | +7.0% |
| **pre-fix vs `rr` (`old-req`/`rr`)** | **+5.3%** | **−6.5%** | **−9.1%** | **−5.8%** | **−12.9%** | **−6.2%** |

The crc32c row is positive in **all 42 paired cells** and the pre-fix row is negative in all six
cells of every round. That is the answer: **the pre-fix build is behind RoadRunner everywhere**,
by 5.8–12.9%, so the crc32c fix is not one optimization among several — it is the one that turned
a consistent deficit into a consistent lead.

*The mechanism agrees, and in the column it predicted.* A ratio says the cost exists; CPU
attribution says whose it is. Per request at c=32 on `/items`, median of three rounds:

| | server µs | PHP µs | total µs | rps |
| --- | --- | --- | --- | --- |
| `old-req` (bit-by-bit loop) | **184** | 287 | 471 | 16,957 |
| `new-req` (crate, hardware CRC32) | **125** | 283 | 408 | 19,428 |
| `new-opt` (crate + checksum off) | **121** | **281** | 402 | 19,414 |

The fix removes **59 µs of server CPU per request (184 → 125, −32%)** and leaves the PHP column
where it was (287 → 283), which is exactly what it should do: the server hashes every response
once, and the PHP worker was already using `hash('crc32c', …)`, which is hardware-accelerated.
The same figure at c=16 is 181 → 122. So the 10–16% in throughput is a ~13% cut in total CPU per
request, and the two independent instruments land on the same number. The warm arm at 19,400 rps
is using 7.8 of the 12 cores, so it is not CPU-saturated at this load — which is why the
throughput gain is a little smaller than the CPU gain, and why the gain is not uniform across
concurrency.

*Correction to hazard 41, which is the reason this pass was worth running.* Hazard 41 concluded
that the optional checksum was "worth about 0.26%", from a microbenchmark that encoded a whole
response with the SDK 50,000 times. That measurement is not wrong, it is **incomplete**, and the
omission is systematic: it timed the *Rust-side frame encoding*, but the checksum is hashed
**twice** per response — once by the PHP worker that writes the `BODY` frame and once by the server
that reads it. The PHP hash is the one that dominates, precisely because the worker's hash is the
SDK's and the benchmark had never isolated it. Measured end to end the cost is **~6 µs of CPU per
request (1.5%)**, not 1.7 µs, and the throughput medians are **+2.0% to +3.0%**.

Even so, the throughput evidence for this one is weaker than the table makes it look, and the
difference between the two rows above is the lesson: **the crc32c row's seven-round spread never
crosses 1.00, while the checksum row's does, in four of the six cells** (`/` c=8 `[0.987..1.034]`,
`/items` c=16 `[0.997..1.045]`). A median that is positive while its own spread straddles 1.00 is
a cost that exists reported through a noise floor that is larger than it. The CPU columns settle
it — the option removes a real 3–4 µs on each side — and it should be read as a small, real,
now-measured saving rather than as the 0.26% hazard 41 implied.

*And the benchmark host was not measuring what the repository pins.* Setting this experiment up
found that the VM's application tree is **not a git checkout** and had drifted: all 31 apparently
differing files were CRLF in a tree that is LF in the repository, but two real differences were
underneath — `worker.php` still called `Worker::create()` where `main` calls `Worker::connect()`,
and the vendored SDK was `5ede306` where `composer.json` pins `475bfd2b`. That is the exact failure
commit `134a7bc` documents: `connect()` on an SDK that predates it makes PHP write its error to
stdout, volucer reads the first byte `a` (97) as a protocol version, and the pool dies with
"unsupported protocol version 97". The published sweep's *application* source does match the repo;
only the SDK pin and that one call differ, and on Linux both resolve to the pipe — so the published
numbers are not invalidated, but they are not reproducible byte-for-byte from the repository
either, and the fix is the tree refresh plus `composer install` rather than the tree refresh alone.

**52. The one cell RoadRunner wins is noise, but the tail is not — and it points at `pick_slot`.**
Hazard 51's interleaved A/B has a single cell where RoadRunner leads: `/items` at c=16, −1.7%.
The obvious reading is a crossover, because the pool has 12 slots and 16 is the first concurrency
above it, so the cell was chased with a denser sweep — the same two variants at c=12, 14, 16, 18,
20 and 24, eight rounds, a balanced two-variant rotation, 96 cells and none failed.

    c=12  +1.3%    c=14  +1.1%    c=16  −1.1%
    c=18  −1.3%    c=20  −0.4%    c=24  +3.0%

**There is no crossover.** The deficit is ~1% (about 200 rps in 18,000), it sits in a band from 16
to 20 rather than at a threshold, the per-round spread crosses 1.00 at four of the six levels, and
the sequence is not monotonic. Per-request CPU agrees: volucer's server cost is **flat at
121–126 µs across c=8..32** and RoadRunner's moves 9 µs, so if 16 were a threshold volucer's
column would bend there and nothing bends. Attributing that cell to slot exhaustion would have
been a story the data does not support, and the lesson generalises — at ~1% a single concurrency
level is not a meaningful unit of evidence.

*What the denser sweep did find is a tail divergence with no counterpart in throughput.* Median
p99 on `/items`, RoadRunner against volucer:

    c=12  1,760 µs  vs  1,640 µs   (0.93×)
    c=14  1,930 µs  vs  2,195 µs   (1.14×)
    c=16  2,105 µs  vs  3,470 µs   (1.65×)
    c=18  2,275 µs  vs  5,255 µs   (2.31×)
    c=20  2,415 µs  vs  6,770 µs   (2.80×)
    c=24  2,690 µs  vs 10,425 µs   (3.88×)

RoadRunner's tail is **flat** above its slot count, which is what a pool that holds a queue looks
like. Volucer's grows without bound, and its p50 is *better* than RoadRunner's at every one of
those levels. A p99 that grows with offered load while the median improves is the signature of
queueing with no bound on the queue: the requests are not served slower, they wait longer before
they are served. That is also why the throughput number never showed it — a bounded queue costs
latency, not throughput, and every pass in this project until now reported a median.

*The mechanism was a hypothesis. It has now been measured, and it is confirmed — with a
correction, and with a number attached.* The pool reports, at shutdown, the mean and the p50/p99
of the time each request spent between asking for a slot and holding one: per-slot totals plus a
powers-of-two histogram, all accumulated inside the server by `WorkerPool::attempt`, surfaced
through `Contention`, and printed by the CLI (`4dd402a`, `a6df392`). Because it is
measured in-process it cannot include anything a load generator did, which is the property that
makes it evidence rather than another correlation.

Two experiments on the benchmark host, `/items`, 8 s per run, three repeats per level,
`scripts/diag-slots.sh`.

**The client is exonerated.** The harness drives `wrk` as `-t$c -c$c`, so at c=32 the load
generator owns 32 threads on a 12-core box, and I suspected *that* was the tail. Holding
concurrency at 32 and varying only the thread count changes nothing server-side:

    threads   2: client p99 17.80 ms   pool mean 895 µs   pool p99 16.4 ms
    threads  32: client p99 18.33 ms   pool mean 898 µs   pool p99 16.4 ms

3% apart, and the pool's own columns do not move at all. This was my hypothesis, it was
plausible, and the measurement refutes it.

**The queue, at concurrency equal to threads:**

    conc  excess   client p50   client p99   pool mean   pool p50   pool p99   pool share of time in system
      12       0      0.71 ms      1.68 ms       34 µs       2 µs     1.02 ms      4%
      16       4      0.79 ms      3.94 ms      157 µs       3 µs     2.05 ms     17%
      20       8      0.88 ms      7.68 ms      325 µs       4 µs     8.19 ms     30%
      24      12      0.97 ms     10.74 ms      509 µs       4 µs     8.19 ms     40%
      32      20      1.16 ms     18.98 ms      906 µs      19 µs    16.4  ms     54%

Three things follow, and the first is a correction to my own reasoning earlier in this item.

- **It grows from concurrency 13, it is not a step.** I had reasoned that because the winner is
  chosen by cursor and then locked again, a request is served the moment the cursor lands on a
  free slot, so the wait would be a step at `workers + 1` and flat after. That is wrong. A step
  shape would mean waiting for the one request already in front; 34 → 157 → 325 → 509 → 906 µs
  across 0 → 4 → 8 → 12 → 20 excess clients is a queue *accumulating*, each new arrival waiting
  for everything ahead of it. The old docstring's "the search never blocks on a busy worker" did
  not describe this code.
- **It is unfairness, and the arithmetic ties it to the client's tail.** The pool's median wait
  is 2–19 µs while its p99 is 1–16 ms: most requests take an uncontended mutex and a few wait for
  whole requests. The pool's own `p99 − p50` tracks the client's `p99 − p50` at every level —
  1.02 vs 0.97 ms, 2.05 vs 3.15, 8.19 vs 6.80, 8.19 vs 9.77, 16.37 vs 17.82 — so the tail *is* the
  pool's, and it accounts for 84–105% of the client's excess over its own median.
- **The mean is real too, and it is a cost, not only a tail.** The pool's mean wait is a growing
  share of a request's mean time in system: 4% at c=12, **54% at c=32**, where 0.91 ms of a
  1.69 ms request is spent waiting for a busy worker. That is worth separating from the tail. A
  fair queue would not delete that time — a slot is still held for a whole request — it would
  spread it evenly instead of concentrating it, so it is a latency-*variance* fix. Removing the
  time itself is a different design question, and the throughput plateau at ~19k is that
  question seen from the other side.

**What is still open is which fix.** The measurement identifies the defect, its mechanism and
its magnitude: ~0.9 ms of mean latency and a 16 ms p99 at c=32, in `pick_slot`. It does not
compare candidates.

*Both were weighed, one was built, and it holds.* The fair queue was chosen over `try_lock` in the
scan for a reason worth recording, because it is the part that generalises: **the mean wait is
structural.** 32 clients against 12 workers at ~600 µs each is ~20 requests in flight beyond the
workers, and no scheduling change removes that time — it is Little's law. What a fix can change is
*who* is charged it. `try_lock` scanning for a free slot is a race rather than a fairness
mechanism, and at c=32, where the damage is, all twelve slots are usually busy, so there is little
free to skip to; it would have helped the case that was not broken.

`pick_slot`, the cursor and the healthy-not-free test are gone, replaced by a lease taken from a
free list (`e2b031e`). Same sweeps, same host, three repeats per level:

    conc   rps before -> after    client p99        pool mean wait    pool p99 wait
      12   15,686 -> 16,342  +4.2%   1.68 -> 1.41 ms    34 ->   0 us     1.02 -> 0.008 ms
      16   17,548 -> 19,884 +13.3%   3.94 -> 1.57 ms   157 ->  30 us     2.05 -> 0.51  ms
      20   18,581 -> 22,114 +19.0%   7.68 -> 1.78 ms   325 -> 123 us     8.19 -> 1.02  ms
      24   18,799 -> 22,787 +21.2%  10.74 -> 2.24 ms   509 -> 271 us     8.19 -> 2.05  ms
      32   18,982 -> 22,360 +17.8%  18.98 -> 3.68 ms   906 -> 625 us    16.4  -> 4.10  ms

Three of those were not predicted, and one of them is the one to distrust first:

- **Nothing was shed to achieve it.** A bounded queue that refuses under load produces exactly
  this shape while converting throughput into `503`s, so it was checked directly: `wrk` prints
  non-2xx responses as an explicit line and printed **none** at c=16, 24 or 32, and the server
  logged no refusal. Every response counted was a success.
- **Throughput rose 13–21%, which was not the goal.** The mechanism explains it — the old scan
  locked each candidate and called `try_wait` on each, and at c=32 walked the whole array before
  blocking on one slot, all of it per request on 12 cores at 19,000 rps — but it was not predicted,
  and an unpredicted win is the kind that deserves re-measuring rather than celebrating.
- **The median cost much less than expected.** The estimate was p50 rising to ~1.5–1.7 ms at c=32
  as the mean got shared out; it went to 1.35 ms. Direction right, size wrong.

The trade that *was* predicted did happen: at c=32 the **median is 16% worse** while the p99 is 81%
better. That is the fairness working as designed — the request that would have been served on
contact now shares the queue — and it is confined to the range where there is something to share,
because at c=12 the median is slightly better. Against RoadRunner's c=24 dense-sweep figures
(2.70 ms p99, 1.23 ms p50) the fair queue is now better on both, which this project could not say
before.

What remains genuinely open is whether the *mean* queueing is worth attacking — a different design
question from fairness, and the ~625 µs at c=32 is where it would show.

*And the one number in that table that deserved distrust has now been re-measured the way this
project measures things, and it is larger than the blocked layout said.* The +13–21% was
before-vs-after, and this repository has already published a 22% "effect" that reversed sign the
moment two binaries were interleaved instead of blocked (hazard 49). So the same two binaries were
measured again inside a single round, in rotating order, with RoadRunner as a control that shares no
code with either — `diag-slots.sh` gains an A/B mode for it, and `scripts/slots-report.py` reads it.
7 rounds x 3 variants x 2 concurrency levels, a discarded warm-up pass per round so the first
variant of round 1 pays for the cold binary and cold fs cache only in a row that is thrown away.
42 measured cells, 0 discarded.

    conc   new / pre-fair        spread over 7 rounds   new / rr      pre-fair / rr
      24   1.245x  (+24.5%)      1.223 .. 1.271          1.272x        1.030x
      32   1.229x  (+22.9%)      1.197 .. 1.244          1.302x        1.057x

The spread never crosses 1.00 at either level and all 14 paired comparisons point the same way. The
control is what makes it legible: **the pre-fair binary measured +3.0% and +5.7% over RoadRunner,
the same band hazard 51's interleaved A/B established for it on its `/` and `/items` c=32 cells
(+4.2%, +7.0%; its one `/items` c=16 cell was −1.7%)** — it still lands exactly where it always did
— while the current build sits at +27% and +30%. So the blocked layout *understated* the gain, and
this is the largest throughput effect this project's own code has produced — larger than the
`crc32c` fix's +10–16%, which was called "the whole lead" over RoadRunner (the fix put volucer
4–7% ahead; this puts it 27–30% ahead). The one effect measured here that is larger is not
volucer's: removing xdebug from the host's CLI PHP doubled the warm arms, which was a
configuration bug that made one arm measure the host rather than the server.

The pool's own columns move with it rather than against it, median over the seven rounds:

    conc   build      mean wait   wait p50   wait p99   client p50   client p99
      24   pre-fair     522 us       8 us     8,192 us     1.00 ms      10.92 ms
      24   fair         263 us     256 us     2,048 us     0.98 ms       2.19 ms
      32   pre-fair     887 us      64 us    16,384 us     1.21 ms      17.44 ms
      32   fair         587 us   1,024 us     4,096 us     1.28 ms       3.47 ms

The p99 halves twice over *while* the mean wait falls (522 -> 263 us); the p50 rises, which is the
fairness made visible (the request served on contact now shares the queue, so the unlucky one no
longer waits 10 ms); and the cost at c=32 is 0.07 ms of median against 14 ms of tail. Nothing was
shed: three 8-second runs at c=24 printed `wrk`'s full summary and showed **no `Non-2xx` line and no
`Socket errors` line** in any of them, 232,751 / 233,591 / 235,206 successful requests, with the
pool's 500 ms slot-wait ceiling never reached. `run_measurement` now checks for those lines on every
cell and prints them if they appear, so the claim is enforced by the harness rather than by memory.
Raw rows are committed beside the earlier ones (`slot-waits-fair-ab-c24.tsv`, `-c32.tsv`).

Two things still worth keeping separate. This is **not** a throughput defect and fixing it would
move no number in the competition report — measured throughput at c=24 is already 4% *ahead*.
And it is a user-visible property in a way a 1% throughput deficit is not: a service whose p99 is
19 ms under 32 concurrent connections is a different proposition from one that is merely faster
on average. A resident-worker server that wins the mean and loses the tail is a case this project
had not previously been able to see, because every pass it owns reported a median.

### Hazard 53 — the embedded runtime, measured against the two that spawn

`volucer-competition` gained a sixth arm, and it is the only one that can test the project's
**first** constraint rather than its tuning. That constraint is *no PHP embedding, no FFI —
`php`/`php-cgi`/`php-fpm` are always spawned as external processes*. FrankenPHP is the
counter-proposal: a Caddy server with a ZTS PHP built into the binary, twelve worker threads in
one process, framework booted once. Same `BenchApp`, same page, same worker count,
`max_requests`/`max_jobs`/`pm.max_requests` all 0, and the same ini reached through `php_ini`.

Interleaved, 7 rounds, order rotated, `/items`, 12 workers (`scripts/ab-perf.sh`):

    conc   FrankenPHP   volucer   RoadRunner     franken/rr    franken/volucer
      1      2,441        2,388        1,773         1.381x        1.018x  [0.969..1.054]
      8     14,337       15,538       11,131         1.288x        0.921x
     16     15,140       19,984       17,554         0.876x        0.759x
     32     15,823       23,250       18,164         0.867x        0.677x

Every ratio held the same way in all 7 rounds except the one at concurrency 1, whose spread
spans 1.00 — so **FrankenPHP and volucer are indistinguishable when one request is in
flight, and FrankenPHP is the slower of the three from concurrency 8 upward.** What survives
is the *shape*: the arm with the interpreter inside its process does not convert concurrency
into throughput, gaining 10.4% from 8 to 32 where volucer gains 49.6%. On this page, in the
range a server is actually for, the no-embedding rule was a cost rather than a saving — which
is worth stating plainly, because the constraint was argued on other grounds (isolation, a
rebuild per PHP version) and this is not evidence against those.

It is **not** that the embedded interpreter costs more per request. Twelve seconds at
concurrency 16, `utime + stime` summed over each whole process tree out of `/proc/<pid>/stat`
— the integral across the window, not a `top` snapshot:

    server      pids   server CPU-s   cores used   rps
    FrankenPHP     2       85.99         7.17       15,262
    RoadRunner    13      105.90         8.83       17,794
    volucer     13       97.18         8.10       20,204

FrankenPHP serves the fewest requests per second **and** leaves the most cores idle: 7.17 of
12, against volucer's 8.10 at 33% higher throughput. Capacity is sitting unused while its
throughput is capped, which is the signature of dispatch or queueing rather than of compute.
That is as far as the evidence goes; the reason is open decision 7 below.

Two confounds, stated rather than corrected, and both belong with any number quoted from this
arm: the release binary bundles **PHP 8.5.11** against this host's **8.3.35** and there is no 8.3
build (so some of the c=1 lead may be 8.5 rather than embedding, and some of the c=32 deficit may
be a runtime this new), and it reports **2 pids** where the other two report 13, because its
workers are threads and not children — which is the architecture under measurement rather than an
artefact of it.

**One harness lesson, and it generalises well past this arm.** `php_server` runs a `worker` only
where `match` says so, and its default is "requests for a `.php` file". Everything else goes to the
ordinary per-request PHP handler. Same binary, same worker script, same thread count, two
Caddyfiles differing by one directive, `GET /items` at concurrency 32:

    worker { match /* }       15,856 rps
    no `match` (the default)     662 rps

A 24x error that no HTTP response can reveal — both return 200, both contain the page marker, both
serve byte-identical bodies. A 662 rps arm would have been read as "embedding is much slower",
not as "this arm is cold". So the arm proves its own model: the worker script logs the first time
its loop is actually asked to serve something, and the harness refuses to measure the arm if that
line is missing. Two weaker checks were built first and **discarded with their numbers** — a
timing floor (1.26 ms warm against 2.61 ms cold medians, with the best-case cold at 1.96 ms
overlapping the worst-case warm at 1.35 ms, so any threshold between them is a coin flip) and a
worker-thread count read out of the log (both configurations start all twelve threads and both
log all twelve reaching their loop; the worker is real in both cases and is simply never asked to
serve anything). A check that cannot fail is not a check — hazard 28's lesson with a new
instrument.

Two more traps specific to this arm, each of which produced a broken arm before it produced a
number. The release binary loads **no php.ini whatsoever** (`php_ini_loaded_file()` comes back
empty), so absent the Caddyfile's `php_ini` the arm would have run `memory_limit` 128M and
`opcache.validate_timestamps` **on** while every other arm ran 512M and off — the values are
therefore read back from inside a worker thread into the arm's own log, twelve lines, one per
thread. And **`STDERR` does not exist in this SAPI**: a worker's output *is* the HTTP response, so
the first version of the worker died on `Undefined constant "STDERR"` before reaching its request
loop, which the server reports only as `worker ... has not reached
frankenphp_handle_request()`.

### Hazard 54 — "rustls needs no system dependency" is true at runtime and false at build time

The case for rustls over OpenSSL has always been stated here as *no DLL/dylib matrix* (§3, decisions
table). That is correct and it still holds: a rustls build links no OpenSSL, so there is nothing to
match to a system version on any of the three platforms. What the sentence does **not** cover is the
build, and the two were being read as one thing.

**Measured.** rustls 0.23's default crypto provider is `aws-lc-rs`, which is C and assembly, and
`aws-lc-sys` invokes CMake and needs an assembler. On this host before the work started:

    nasm    absent
    cmake   absent
    cl      installed (14.44.35207) but not on PATH

so the workspace's first TLS dependency would not have compiled, and the reason would have looked
like "rustls is hard to build" rather than "this machine is missing two tools". After
`winget install NASM.NASM` and `winget install Kitware.CMake`:

    nasm    3.02   C:\Users\christian\AppData\Local\bin\NASM\nasm.exe   ** not on PATH **
    cmake   4.4.4  winget shim on PATH

Two things were established by building rather than by reading. **First, aws-lc-rs does compile
here** — `rustls` 0.23.46 + `tokio-rustls` 0.26.6 + `aws-lc-sys` 0.46.0 built in 39 s, and a real
`ServerConfig` was constructed from an rcgen self-signed certificate, which is the part that would
panic if the provider were unusable rather than merely present. The installed provider reports **9
cipher suites**. **Second, the `ring` fallback is not a fallback any more** — with
`default-features = false, features = ["ring", "std", "tls12"]` the same probe also built and ran,
so if aws-lc-rs ever breaks on a target, one feature line reverts it.

**The trap worth recording** is that winget installed NASM correctly and then did not put it on
PATH, while CMake *did* get a shim. So `nasm -v` fails in a fresh shell even though the tool is
installed, and the failure surfaces much later as a CMake error from inside `aws-lc-sys`. Nothing on
the machine or user PATH matched `NASM`; the directory had to be added by hand. The build
prerequisites for this project are therefore now **Rust + a C toolchain + NASM + CMake**, not
"Rust", and that is a real change to what a fresh checkout needs. It is why `ci-build-matrix`
(step 51, now step 69 in Phase 10) should exist before the cross-platform claim is made — and it
now does. **NASM was the one that mattered**, and the reason is worth carrying forward: CMake and
MSVC are on every GitHub runner image and NASM is on none of them, so a workflow that installs the
toolchain and not the assembler fails later, from inside `aws-lc-sys`, in a way that reads as a
dependency problem rather than a missing tool. The same wrong shape as the winget trap above, on a
machine nobody can log into.

**Version decision, also measured rather than assumed.** The Rustls 0.24 line moves providers out of
the main crate into `rustls-aws-lc-rs` / `rustls-ring`, selected by an explicit runtime install
rather than a feature. Checked on crates.io: `rustls` is at `0.24.0-dev.2`,
`rustls-aws-lc-rs` and `rustls-ring` are both at `0.1.0-dev.1`, while `tokio-rustls` is at
`0.26.6` and `rustls-acme` at `0.15.4`. So **0.24 is pre-release on every crate involved** and
targeting it would mean a dev dependency in a server's TLS path. **0.23 stable is the target**;
the provider move is a mechanical migration when 0.24 and its siblings release.

### Hazard 54a — the redirect turned the plaintext listener into a TLS port

Step 46 was verified by hand against a running server, which is the only reason this was found:
the tests that existed all passed, and so did `check`.

`HttpListener::bind_with_tls` took `Option<TlsSettings>`, and *both* the TLS acceptor and the
redirect were derived from that one `Some`. So a deployment that wanted a redirect had to pass
settings, and passing settings built an acceptor — meaning the listener the redirect lived on
was a TLS port. In the split `:80`/`:443` shape that is the **plaintext** listener:

    [server]
    http  = "0.0.0.0:80"
    https = "0.0.0.0:443"
    redirect_to_https = true

`http` on :80 then built an acceptor and stopped answering cleartext, while `run` logged a
successful `TLS enabled` for it and `check` reported the deployment as valid. A client got a
connection that was accepted and then went nowhere. Nothing in the log said which listener was
wrong, because from the code's point of view nothing was: `Some(settings)` genuinely meant "this
listener speaks TLS", and the *caller* was wrong.

**Two bugs in this one step, and they were found differently, which is the point.** The first
(the redirect looping TLS clients when `plain = true`) was caught by a test as soon as the test
existed. This one was caught by running the deployment and watching a plaintext port not answer —
no test covered the shape, because every test constructed its listener the way the buggy code
expected. The fix separates the two decisions:

    pub struct ListenerSettings<'a> {
        pub tls: Option<TlsSettings<'a>>,   // does *this* listener speak TLS
        pub redirect_to_https: bool,        // does it redirect cleartext
        pub https_port: Option<u16>,
    }

so a listener carries a redirect with no certificate, which is what the split shape needs. Pinned
by `a_redirect_does_not_turn_a_plaintext_listener_into_a_tls_port`, a test that constructs a
plaintext listener with a redirect and asserts it still answers cleartext — the test that would
have caught it, written after the fact because the shape was never tried.

**The lesson is about which shapes the tests reproduce.** Six TLS tests passed while the shipped
configuration shape was broken, because they were written against the API rather than against the
configurations an operator writes. The end-to-end run is what closed the gap; the test was written
from it rather than instead of it.

**One correction to the step-24 text, which claimed the `Sites` table is "the map SNI certificate
selection needs":** it is not. `Sites`' host maps are private, `named()` returns `ResolvedSite`, and
`ResolvedSite` carries no `hosts` — so SNI needs its own host→certificate map. The saving is real but
smaller than stated: the *rule* is reusable (exact name, then longest `*.` wildcard, then the server's
certificate), and it is extracted into `HostMap<V>` so sites, SNI and route matching share one
implementation of it rather than growing a third.

### Hazard 54b — the wildcard walk and `host_matches` had already disagreed

`HostMap<V>` (step 44) is the second implementation of "does this host match this pattern", because
the site table was rewritten to walk the host's own labels while the route table still compares the
pattern. Two implementations of one rule is exactly the drift this codebase refuses elsewhere, so
they are **pinned against each other by a test** rather than trusted:
`this_table_and_host_matches_agree_on_every_shape` runs four patterns against seventeen hosts and
asserts the two agree on every pair.

**It failed on the first run, on a real divergence, and the divergence was introduced by the
earlier optimisation.** Host `a..example.com`, pattern `*.example.com`:

    HostMap::get          -> no match
    host_matches          -> match

The label walk refuses an empty label; `host_matches` did not, because its check was `host.len() >
suffix.len() + 1` plus "the byte before the suffix is a dot". `a..example.com` is longer than the
suffix and the byte before the suffix *is* a dot — the empty label is the one two bytes earlier, and
one length comparison cannot see it. So a route with `host = "*.example.com"` matched a host that the
site table sent to the default site, which is precisely the unreproducible-from-either-table bug the
shared rule was meant to prevent.

Fixed by refusing a zero-length label in `host_matches` as well, and the fix is pinned by the
agreement test rather than by either side's own tests — neither implementation could have caught
this alone, which is the argument for the test existing at all.

Recorded rather than quietly fixed because the lesson generalises: **the optimisation in `206b41f`
was verified against the tests that existed, and they did not cover the empty label.** The
`HostMap` extraction is what surfaced it, which is a reason to do this kind of consolidation before
building on top of the old behaviour rather than after.

### Hazard 55 — the rename, and the one name that had to be withdrawn

`stokehold` was renamed to `volucer`. The reason is in §2; this entry records the mechanics and the
two traps, because both cost real time and neither is obvious from the code.

**The name that looked perfect and was not.** `velifer` was the strongest candidate on the merits:
Latin *vēlum* ("sail") + *-fer* ("bearing"), so "sail-bearing" — a portmanteau of the Sailantris
maritime theme and "carries requests", with the productive English `-fer` that also gives conifer and
aquifer. It was free on crates.io, npm, PyPI and RubyGems with `.dev`, `.io` and `.org` unclaimed,
and no trademark turned up in the usual search. It was still unusable: **`Velifer®` is a registered
BASF bioinsecticide**, EPA Reg. No. 71840-22, active ingredient *Beauveria bassiana*. The lesson is
the order of checks: registry availability and etymology screening are cheap and were done first,
while the one check that decides the outcome — a trademark search — was done last. For a name that
will be a crate, a package and a domain, the trademark check belongs first, not as a confirmation
pass. Note also that a web search for the bare word ranked etymological dictionaries above the
commercial product, so "no trademark found" from a general search is weak evidence on its own.

**A name rename can break a length-dependent test, and here it did.**
`reading_one_request_does_not_enter_the_kernel_once_per_field` asserted **40** unbuffered reads for a
twelve-record environment. That total is not a property of the codec — it depends on how the
environment's byte length falls across record boundaries, and `params_pairs` uses **the project's own
name** as `SERVER_SOFTWARE`. Shortening that value by two bytes (`stokehold` → `volucer`) moved a
record boundary and the count became **39**. A controlled experiment settled it: restoring the
9-byte value made the test pass again, twice, and setting the 7-byte value made it fail, twice.

The test was wrong, not the rename — but only in the sense that it was over-specified. The assertion
worth keeping is the *ratio* between the buffered and unbuffered readings, which is what the test
exists to defend; the exact integer is a fixture artefact. It is now pinned at 39 with a comment
saying it must be re-measured if that fixture's length changes again. **The general hazard: a test
that hard-codes a value derived from the project's own name will fail on a rename for a reason that
has nothing to do with the rename**, and the failure looks like a real regression.

**Renaming a directory tree is not a substitute for verifying content.** Every renamed file was
checked mechanically rather than by eye: for each file in `HEAD`, apply the name substitutions to the
`HEAD` bytes and compare with the working-tree file. 78 of 78 in this crate tree matched with two
reviewed exceptions (the regenerated `Cargo.lock`, and the test above). Two things that check caught
or was needed for: the substitutions must be applied **most-specific-first** (`STOKEHOLD_SDK_DIR`
before `STOKEHOLD`; `stokehold-php-sdk` before `stokehold`), or the general rule eats the specific
one; and a bare `stoke` rule must come **after** every `stokehold` rule, or it rewrites `stokehold`
into `volucerhold`. Both are silent corruption rather than an error.

**Renaming a package can silently disable a hook.** `azera-worker.php` looks for `volucer-worker.php`
in the application root. The Sailantis homepage's file was named `stokehold-worker.php`, so the
rename would have made the site's custom not-found handler **quietly stop loading** — no error, just
a different 404. This is the hazard worth carrying forward: when a name appears in a *convention*
(a filename a framework looks up, an environment variable, a default config filename) rather than
only in code, renaming the project changes that convention, and every consumer has to be found by
searching for the convention rather than for the name.

### Hazard 56 — the certificate was checked *after* the port was taken, and the plan said the opposite

Step 48 recorded, as a **done** item, that *"`serve` parses the certificates for real and fails
before binding"*. It did not. In `bind_with_tls` the port was bound first and the certificate was
loaded afterwards, so a deployment with a bad certificate took its port and only then discovered it
could not use it.

**The comment above the load was right and the doc comment on the function was wrong**, which is
why this survived review: the inline comment said *"the acceptor is built before anything is
served"*, which was true, while the doc comment said *"before the port is bound"*, which was not.
Two statements about one thing disagreed, the weaker one was true, and the stronger one was what
the plan recorded.

**Why it matters, measured by running it rather than by reading it.** Against the built binary with
a missing certificate, the process printed the `http:` and `https:` listener lines *first* and only
then failed:

    http:             127.0.0.1:45871 (https)
    https:            (disabled)
      ...
    volucer: a listener could not start: TLS is configured but unusable: server.tls: cannot read
    the certificate ... (os error 3)

So the operator reads that both ports came up, and then reads an error. Under a service manager
that restarts on failure, a port is taken and released in a loop and every client arriving in the
window gets nothing — **which is the failure mode the sentence in this step claimed was
prevented.** Exit code is 1 and the port is released immediately, so it is not a hang; it is a
window, and windows are what a restart loop turns into a permanent state.

**The fix is three lines of reordering** — resolve and build the acceptor before
`TcpListener::bind` — and it makes the code match the comment that was already there.

**The test took two attempts, and the first one is the lesson.** The obvious test — assert that
`bind_with_tls` returns a TLS error and that the port is free afterwards — **passes against the
buggy code**, because the failed call binds and then drops the listener, so the port *is* free by
the time anything is asserted. A second version raced two concurrent calls at the same address and
also passed, because the first call failed and released the port before the second reached its own
bind. Both were describing a post-condition of a sequence rather than the ordering they meant to
pin.

What works is forcing the choice: **hold the port, then call `bind_with_tls` with a certificate
that does not exist.** Both faults are now present at once and the order decides which is reported:

    certificate checked first -> the certificate error (the port was never touched)
    port bound first          -> a bind error ("address already in use")

Against the old code the test fails with `cannot bind 127.0.0.1:54912: ... (os error 10048)`,
which is the port being taken before the certificate was ever opened. That is the failure message
this hazard exists to explain, and it is committed as
`a_certificate_that_cannot_load_fails_before_the_port_is_bound`.

**A third finding, in the measurement itself, and it is the more embarrassing of the three.**
The TLS-off A/B compares `206b41f` (the last commit before any TLS code) against `HEAD`, and the
first version of its write-up named the fair queue `e2b031e` as a live-but-inert confound, on the
reasoning that it is the largest effect this project has produced and that "a static file never
leases a slot".

**Both halves of that were wrong.** `git merge-base --is-ancestor e2b031e 206b41f` succeeds: the
fair queue is *in* the baseline, so it is not a confound at all — both arms are after it, along
with the static layer (`6d7228d`). And the "never leases a slot" claim was a guess about which path
a static request takes and which lock protects a slot; the code shows the static layer runs *before*
any dispatch, so the guess about the slot was harmless, but it was still reasoning from a memory of
the code rather than from the code.

**The confound that is actually live is `ba09e03`, the `HostMap` extraction**, which replaced a
linear scan over wildcard patterns with a walk of the host's own labels on every request. It is one
of exactly two commits in the window that touch `volucer-http/src`:

    git log --oneline 206b41f..HEAD -- crates/volucer-http/src
    e1af51b  tls: check the certificate before the port   # load-time, before any accept
    dae2983  refactor: rename stokehold to volucer        # names only

So the window is TLS (a branch not taken with no `[server.tls]`), `HostMap` (per request, and it
is meant to be faster), a rename, and a load-time reorder — which makes the null interpretable
rather than unexplained, but means it is **not** a pure TLS isolation.

**The lesson is about how a baseline is chosen and described, and it generalises past this
measurement:** a commit chosen by *"it is the one before the TLS work"* is a claim about history,
and it has to be checked with `git merge-base --is-ancestor` rather than inferred from a commit
list read top-down. Both named confounds were checked afterwards and neither was live; the
difference that was live had not been considered, because it is a *refactor* and refactors are
easy to assume are free — which this repository has already contradicted once, at hazard 54b,
where the same optimisation introduced a real divergence.

**And a second finding from the same step, which is about the plan rather than the code.**
`alpn_h2_negotiates_a_real_http_2_connection` asserts that HTTP/2 **works** — the preface is
answered with a SETTINGS frame — where the step-47 test only asserted that ALPN *selected* h2.
`an_upgrade_h2c_offer_is_answered_in_http_1_1` pins the upgrade gap as a decision. Both were
promised by step 47 and by the TLS plan's §4.5 and neither existed, which is the same stale-claim
pattern as step 18: **a plan records an intention and then reads as a fact, and only a re-read
catches it.**

### Hazard 57 — a test that asserts *nothing changed* passes against a test that does nothing

Found while falsifying `reload-tls` (step 55), and it is a distinct failure from the usual "the
test passed because the bug was never reachable".

The claim is whole-or-nothing: a certificate set with one unreadable pair must install **nothing**.
So the natural test loads a set, expects `Err`, and asserts that the certificate being served is
still the one from before. Written that way, **it passed against a deliberately broken `refresh`
that installed nothing at all** — the stub I used to falsify it:

```rust
pub fn refresh(&self, certificates: &CertificateSet) -> Result<Arc<Certs>, TlsError> {
    let _ = load(certificates, &self.config_dir)?;
    Ok(self.state.load())          // FALSIFY: loads, reports, installs nothing
}
```

That stub refuses the bad set *and* refuses the good set, so "the old certificate is still being
served" is true for the wrong reason. The assertion was about a *post-condition of a sequence*
rather than about the ordering — the same shape as hazard 56's first two attempts, where a failed
`bind_with_tls` releases the port and so "the port is free" proved nothing about the order.

**What makes it an observation: name something in the refused set that *could* have been
installed.** The reworked test passes a perfectly good *server* pair alongside the unreadable
*site* pair, so a partial install is visible as a **changed** answer for the server's host:

```
refresh({  server: renewed (valid),  sites: [ shop -> absent (invalid) ] })
  whole-or-nothing -> Err, and default.test still serves the OLD server certificate
  partial install  -> Err, and default.test serves the NEW one   <- the test now fails
```

Against the partial-install stub it fails with *"a refused reload must not install the half that
could be read"*, and against the real implementation it passes. A separate case keeps the original
shape (an invalid server pair, an empty site table) because its **message** is what an operator
acts on and it is the failure a first issuance produces — that one is not falsifiable by
construction, and is marked as such by testing the layer name rather than the absence of change.

**The generalisation, and it is the one worth keeping:** an assertion that a state is *unchanged*
is only a test if the same code path could have changed it. Where the assertion is "nothing was
applied", the *input must contain something that would have been applied*, or the test is a
restatement of `Ok(())`.

### Hazard 58 — ten round pairs said the reload was *faster*, and the control said −1.44%

Step 58's measurement is the one number in the reload work, and it produced a result that excludes
zero and a result that contains it from **the same two binaries**, so the interesting part is why.

The harness is the TLS-off one with the fixture changed, and the arms are `68bfdcc` (three `Arc`s
on `FrontEnd`) against the reload work (one `Arc<Swappable<Reloaded>>`). Both release builds, both
with no `[server.tls]`, `static_files` **on** — deliberately the opposite of the TLS harness's
choice, because the site resolution and routing lookup the snapshot feeds are on the static path.

    first run, 10 rounds    effect +1.75%   95% CI [+0.56%, +2.95%]   # excludes 0
    its control, 10 rounds  effect -1.44%   95% CI [-2.91%, +0.05%]   # a point estimate larger
                                                                       # than the effect it scales,
                                                                       # and the wrong sign

    second run, 20 rounds   effect -0.09%   95% CI [-1.43%, +1.27%]   # contains 0
    its control, 20 rounds  effect +0.34%   95% CI [-0.91%, +1.61%]

**Twenty rounds changed the verdict and nothing else did.** The servers were identical between the
two pairs; the only difference is n. That is the mechanism: the interval is on the mean of the
per-round *log* ratios, so it narrows as √n while the per-round spread — which is what the host
actually contributes — does not move. The 10-round intervals were therefore not evidence of a
1.75% effect; they were evidence that ten round pairs cannot resolve one on this host.

**Both pairs are committed, and that is the point rather than completeness.** A record holding only
the 20-round result would read as a tidy null. The 10-round pair is the *scale* of the mistake —
without it, "we ran twenty rounds" looks like thoroughness rather than a correction, and the
control's −1.44% is the single most useful number in the set: an A-vs-A run, which by construction
has no effect to find, produced a point estimate larger than the one it was supposed to give scale
to.

**What this confirms rather than discovers.** The project already draws verdicts from intervals and
already keeps an A-vs-A control beside every A/B — the TLS-off work established both, for the same
reason, and its own write-up says so. What is new is that a *plausible* result arrived first and the
control refused it. Hazard 56 lost three attempts to assertions about post-conditions and to a
baseline described from memory; this one lost only the point estimate.

### Hazard 59 — a reload would have reached one of the two front-ends, and no test could see it

Found while starting step 57 (SIGHUP), by asking *what does a reload actually reach* rather than by
running anything — and it was live in `a937f43`, the commit that introduced reload.

`FastCgiListener` held its **own** `Arc<Sites>`, resolved per request, while `serve` handed the HTTP
listener and the FastCGI listener *the same* `Arc`. So the table a reload replaced was the HTTP
front-end's, and only its. A site whose routes or backend a reload changed took effect over `:80`
and kept reaching the old pool behind nginx — for the life of the process, with no error anywhere.

**Why no test could have caught it.** Every reload test in the repository drives the HTTP listener.
`a_reload_changes_what_the_next_request_is_served` is a good test and it passed, because on the
protocol it exercises the reload worked. The failure is *relational* — one front-end moved and the
other did not — so it needs two front-ends in one assertion, which nothing had.

**Why it would not have looked like a bug.** A deployment behind nginx has both listeners, and nginx
talks to the FastCGI one. The operator reloads, sees the new site answer on `:80`, and sees the old
behaviour through the proxy. The most natural reading is that nginx needs a reload too; the next is
that the proxy is caching. Neither is checkable in a way that points at this code.

**The fix names the shared thing.** A new crate, `volucer-watch`, owns `SharedState` — the live
`Arc<Swappable<Sites>>` both listeners hold — and absorbs `volucer-http`'s `reload.rs`, because the
mechanism was never HTTP-specific: `volucer-server` needed the same one, and `volucer-http` owning it
is *why* the FastCGI listener built its own copy. Both front-ends now take a `SharedState` at bind
time, `serve` constructs exactly one, and a per-listener table is not expressible.

**Three further bugs surfaced, and the ways they were caught are the useful part.**

*The snapshot was per connection, not per request.* The first FastCGI version took `sites.sites()`
once at connection start — *nearly* right, which is what makes it dangerous: a fresh connection per
request still sees the new table, but nginx keeps its upstream alive, so the request after a reload
usually arrives on a connection accepted before it. **The first version of the test dialled a fresh
connection each time and passed against the defect.** It now sends both requests on **one
connection**, and moving the snapshot from per request to per connection makes it fail with the old
pool named in the 502. This is hazard 57's shape exactly: a test that cannot fail.

*`Reloaded::resolve` had to exist.* The original `Reloaded` stored a `Sites` that `sites.resolve()`
read back off the value, which let a request resolve a site against one generation and route against
another — the failure `Reloaded` was introduced to prevent, reintroduced through the accessor.
`resolve` now returns the site *and* its maps from one call.
`a_reload_changes_what_the_next_request_is_served` caught it, so the test from `a937f43` was still
doing its job.

*`SharedState::replace` returns the table it replaced*, so rebuilding from its return value rebuilt
the maps from the **previous** generation: a reload that reported a change and served the old
document root. One line, and it is the same off-by-one-generation `resolve` was about.

**The generalisation:** "does this reload everything it claims to" is a question about *reach*, and
reach is not visible from inside one front-end. The place to ask it is where the state is
constructed — which is why the answer here is a type both listeners must be handed rather than a
convention both are expected to follow.

### Hazard 60 — `SIGHUP` was a *stop* signal, and the plan said it was a `#[cfg]`

Step 57's plan text read: *"There is no `SIGHUP` on Windows, so the watcher needs a `#[cfg]` split,
and `main.rs` already owns a `ctrlc` handler with the `termination` feature that covers `SIGTERM`."*
Every clause is true. The sentence is still wrong, because of what it leaves out: **that feature
registers `SIGHUP` as well**, and routes it to the same shutdown callback.

So `kill -HUP volucer` — *the* conventional reload on a Unix server — did not fail to reload, and did
not do nothing. It stopped the server. Reproduced against the previous binary before any code was
written:

    --- before ---   http=200
    RESULT: process is DEAD after SIGHUP
    shutting down; signal again to force

**It was found by reading the dependency, not by testing.** The plan's own note pointed at `ctrlc`;
following it into `ctrlc-3.5.2/src/platform/unix/mod.rs` showed `sigaction(SIGHUP, …)` inside
`#[cfg(feature = "termination")]`, using the same handler as `SIGINT` and `SIGTERM`. Nothing in the
repository would have failed: no test sends a signal, and `serve`'s own doc comment about `termination`
explains the `SIGTERM` half correctly. A plan that names a crate as a *convenience* has to be read as
a claim about what that crate does.

**Why no handler could be written against it.** `ctrlc`'s public API is `set_handler` and
`try_set_handler`, both taking a `FnMut()` — no signal number. One handler runs for every registered
signal, so "reload on SIGHUP, stop on SIGTERM" was not expressible at all. The fix is per-signal
handling on Unix (`signal-hook`'s iterator yields the number), with `ctrlc` kept for Windows and its
`termination` feature dropped — which also removes the *only* thing that had made SIGHUP a stop.

**The generalisation:** a dependency that "covers" a signal has covered it *with some behaviour*, and
which behaviour is the whole question. The cheap check is to read the platform module, and the cheap
verification is to send the signal to the real binary — `kill -HUP` took one line and would have
settled it before the plan was written.

### Hazard 61 — again, a refusal test that passed against a refusal that did not happen

Hazard 57, one layer over, in the same phase. The new test asserts that a configuration the server
*refuses* leaves the previous one serving. Written the obvious way, both bad files named the document
root that was **already serving**:

```rust
std::fs::write(&config_path, "this is not toml [[[\n")            // no root at all
std::fs::write(&config_path, format!("default_backend = \"nope\"\n\n{}", config_for(&first)))
                                                                  // the root already in effect
```

An assertion that the answer is still `"first"` passes against code that refused the file *and*
against code that applied it, because applying it produces the same answer. So the test could not
have failed — and stubbing the validation `Result` to be ignored did not make it fail, which is how
this was caught rather than reasoned about.

**The fix is the same as hazard 57's, and so is the rule:** where the claim is *nothing was applied*,
the input must contain something that **would have been** applied. Both bad files now name the
*other* document root, so a reader that took the table despite the error is visible in the answer. The
falsification then fails as it should — `left: "second", right: "first"` — which is the evidence that
the assertion is now load-bearing.

The malformed file is a **duplicate `document_root` key** rather than a syntactic mess, and that is
deliberate: it fails at parse, and it still *contains* a root a partial reader would have taken. A
file with no root in it at all would leave the assertion vacuous for a second reason.

### Hazard 62 — an invariant between three values that were always equal

Writing the concurrency test for step 59, the first version asserted that a request answered under a
reload still named one of the two document roots — the site table and the static layer agreeing about
which root the request was for. The fixture gave both roots a file of the same name, so the body said
which generation answered.

That assertion **cannot fail**, and the reason is the one this project keeps meeting in a new
costume: `Reloaded` holds the site table, the routing map and the static layer behind a *single*
`Swappable`, built by one constructor, so the three are equal to each other by construction. A test
that checks they agree is checking that a thing is itself. The request-level version was worse than
useless — it was reassuring.

The replacement asserts the invariant but makes the three parts **individually identifiable**: the
default site's root, the named site's root and the static layer's root are three different directories
(`first`, `first/app`, and a file under one of them), so a mixture has something to be wrong about.
And the sampling is validated separately, because "no sample ever disagreed" is exactly hazard 57's
shape: `three_separate_swaps_be_observed_mid_generation` in `volucer-watch` runs the same loop against
the three parts deliberately separated — three `Swappable`s with a `yield_now` between the writes —
and *asserts that it observes the disagreement*. Without that control a clean run of the real test
would mean nothing at all.

**Falsified properly afterwards**, and the result is the part worth keeping: making `Registry::apply`
pair the new site table with the previous generation's maps **is** caught, by the sampler rather than
by any request. The request assertion passed throughout, which is the evidence for why the invariant
had to be sampled rather than observed through the socket. A test written only against HTTP would
have been green against a mixed generation.

### Hazard 63 — the reload's two tables are applied in sequence, and that is visible

`one_sighup_reloads_a_certificate_and_a_site_together` polls until both halves have moved, and the
first version polled only on the site. It failed on the certificate, intermittently — and the reason
is a property of the design rather than a flake, which a manual reproduction in WSL settled in one
run: the log shows

    configuration reloaded generation=1 document_root=/tmp/vcert/second sites=1
    certificate table refreshed

in that order, and for good reason. `Reloader::reload` applies the site table first and refreshes the
certificates after, **specifically** so that a certificate that cannot be read leaves the *sites*
reloaded and the certificates as they were — one table refusing must not take the other with it. The
consequence is a genuine window in which the new document root is serving under the previous
certificate, and a test that closes its poll loop on the first half reads that window and calls it a
failure.

The test now waits for both. The window itself is left alone — it is the whole-or-nothing rule applied
per table, and the alternative (all-or-nothing across *both* tables) would make an unreadable
certificate refuse a valid site change, which is the worse failure. What was fixed is the test's
description of the claim: *one signal applied both by the time it finished*, not *both are visible at
the same instant*.

Found by running the real binary and reading its log, not by reasoning about the code — the same
technique hazard 60 needed.

### Hazard 64 — the plan's own dependency features removed the function it needed

The ACME plan (§3.3) said to take `rustls-acme` with `--no-default-features, features =
["aws-lc-rs"]`, reasoning — correctly — that the crate is a compile-time fork of our crypto provider,
so the provider must be named explicitly. It then drew the wrong conclusion from a right premise:
dropping the defaults also drops `webpki-roots`, which is the gate on `AcmeConfig::new` **itself**.

The constructor it removes is the one that builds the `RootCertStore` verifying Let's Encrypt's own
HTTPS endpoint (`config.rs:65`), so the client cannot reach the directory at all. The error is

```text
error[E0599]: no associated function or constant named `new` found for struct `AcmeConfig`
   --> rustls-acme-0.15.4/src/config.rs:135:5
```

which names no feature and suggests no cause. A `#[cfg]` attribute on a function is invisible from
the call site, so the missing thing looks like an API change rather than a build configuration.

**The generalisation is narrower than "read the features", and worth stating precisely:** the crate's
*docs* are not where this lives. The feature table and the `#[cfg]` on the constructor are, and the
README of a crate like this describes the recommended usage — which is with the defaults on, so it
never mentions that one default gates a function. Two of three defaults were right to reconsider
(`tls12`, and the provider name); the third was load-bearing in a way nothing at the call site could
show.

**The fix is permanent rather than a comment:** `volucer-http/tests/one_rustls.rs` names
`AcmeConfig::new`, so removing `webpki-roots` fails the *build* on a line that says why. Verified by
removing it and reading the error back.

### Hazard 65 — the plan specified a second accept path for a question already answered in one place

The ACME plan (§6.1) specified `LazyConfigAcceptor` in the accept loop, peeking the `ClientHello` and
choosing between two `ServerConfig`s — and §6.3, two pages later, specified that a *normal* handshake
must be answered by the site table. Those two are in tension, and the plan did not notice: the first
puts the decision in the accept loop, the second requires it wherever the sites are consulted.

The resolution is that `ResolvesServerCert::resolve` is already handed the `ClientHello` *including
its ALPN*:

```rust
// rustls-0.23.46/src/server/server_conn.rs:129
fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<sign::CertifiedKey>>;
```

so the sites' resolver — installed in the one `ServerConfig` built at startup, replaceable by design
since the reload phase — is the only place the question needs answering. `rustls-acme` itself does it
this way (`acceptor.rs`: `if is_tls_alpn_challenge(&handshake.client_hello())`), which the plan had
read and did not draw the conclusion from.

**What shipped is simpler *and* safer than the plan**: one `ServerConfig`, one accept path, and the
composition as a single `if` in one function. An accept-loop branch would have been a second place
that could disagree with the first — the same failure the reload work spent a hazard on, at one layer
down.

The lesson is about plan length rather than about rustls: a design written across two sections that
each specify *where* a decision is made, without naming the function that already receives its input,
can specify the same decision twice.

### Hazard 66 — a certificate that is issued, cached, and never served

The plan's §6.1 said an issued certificate "must reach the resolver *in memory*", and the engine step
initially reacted to `DeployedNewCert` by installing the certificate. There was nothing to install
from: `AcmeState` installs it into `ResolvesServerCertAcme` — **its own** resolver, the one with Gap 3
— and the certificate bytes are not reachable from that resolver or from the event.

The failure this would have produced is the worst shape in the project: **nothing visible for weeks.**
The port is up, the previous certificate serves, the renewal appears to succeed in the log, and the
first symptom is an expired certificate. Every test in the tree would have passed, because every
one of them configures a certificate by file.

What is reachable is the cache. `rustls-acme` writes the PEM there (its `store_cert` early action)
and reports `EventOk::CertCacheStore` once that has returned `Ok`, so reading the cache back on that
event gets exactly the certificate just issued — through the public `CertCache` trait, with no fork.
`Engine::install_cached` is therefore called on that event, which is the renewal path, **and before
the first poll**, which is what makes a restart serve its cached certificate immediately instead of
ordering a new one.

**Falsified by disabling the call on `CertCacheStore`:** the test fails with "a published renewal
must reach the serving table exactly once: []". The generalisation: *for a value that arrives
asynchronously, ask where it can be read from, not where it was written.* The event reports that a
write happened; it does not, and cannot, hand over the bytes.

### Hazard 67 — the stub owned the decision, so the falsification passed

The first version of `the_acme_source_answers_only_an_exact_challenge_handshake` put the
"is this a challenge?" decision in the test's own stub. Replacing `rustls-acme`'s exact-match
`is_tls_alpn_challenge` with a substring ALPN check — the falsification — **passed**, and the test
was therefore asserting its own stub rather than the composition.

This is hazard 62's shape in a smaller place, and it has a sharper rule: **a stub may record, but
what it asserts has to come from the code under test.** The stub now delegates both the gate and the
resolution to a real `ChallengeResolver` built from a real (network-free) `AcmeState`, so the exact
ALPN match being asserted is the dependency's own. The same falsification then fails on the combined
`h2`+`acme-tls/1` offer, which is the case that matters: a client appending the challenge protocol to
a normal offer is not validating, and a substring check would hand it a validation-only certificate.

The near-miss is instructive rather than embarrassing: the falsification was *run*, which is the only
reason it was caught. A plan that had trusted the first green run would have shipped a test that could
not fail.

### Hazard 68 — the parts were built and the server never used them

`acme-deps`, `acme-branch`, `acme-engine`, `acme-config` and `acme-cli` were all done, committed and
green — 589 tests on Linux — and **`serve` never constructed an `Engine`**. A deployment with a
complete `[server.acme]` table validated, printed nothing about ACME, bound its ports, served its
fallback certificate, and never attempted an order. No test could see it, because every ACME test
drives a unit or a stub and none of them starts the binary.

This is the phase's own subject matter turned on the phase: the failure mode of ACME is silence, and
the plan had five steps each of which was individually verifiable without ever asking whether the
composition existed. A step list that names *parts* rather than *the path through them* will do that.

**The countermeasure is the shape of the test, not more care.** `an_acme_configured_server_serves_its_cached_certificate`
starts the real binary with a real `[server.acme]`, writes a certificate through the crate's own
`CertCache` trait (so the hashed file name is the one the engine looks for), and asserts the handshake
gets the **cached** certificate rather than the fallback the configuration names. Falsified by making
`serve` build no engine: the test fails, and every other ACME test in the tree stays green — which is
the evidence that this is the only thing covering the wiring.

There is a second, smaller lesson inside it. The first falsification attempt probed
`Engine::install_cached` in `serve`, and **passed**, because `Engine::run` installs the cached
certificate too — on its own thread, a moment later. The probe was too narrow as well as unreachable
in an earlier form: what needed removing was the *engine*, not one of its two calls. A falsification
narrows to the claim, and "the wiring exists" is not the same claim as "this one call happens".

### Hazard 69 — an `ArcSwap` behind two names, and why that had to be the design

Wiring the engine meant breaking a genuine cycle: the resolver is composed with the engine's challenge
source, so the engine must exist first, and the engine needs a resolver to install issued certificates
into. Three shapes were available, and two of them are wrong in ways worth recording.

**Rebuilding the resolver around a *copy* of the table** was the first attempt, and it does not
compile — but the reason matters more than the error. A copy would mean the composed resolver and the
reload handle's resolver each hold their own `Swappable`, so a certificate installed through one would
not be visible to the other, and an ACME renewal would be *silently* undone by the next site reload.
That is hazard 66's failure with an extra step.

**Spawning the engine on the HTTP listener's runtime** was the second, and it fails on a type
requirement that is easy to read past: `AcmeState` holds `Pin<Box<dyn Future + Send>>`, so it is not
`Sync`, and `Handle::spawn` requires `Send + Sync`. A current-thread runtime of the engine's own has no
such requirement — one background thread for a task that does a network round trip every few weeks.

What shipped is one `Arc<Swappable<Certs>>` shared by both resolvers through
`SniResolver::from_shared`, with the sink reaching it through a `OnceLock` set before any port is
bound. The invariant is stated as a property rather than a convention: **there is one certificate
table, and two ways of naming it.**

### Hazard 70 — a second `rustls` would compile, and this is what it would look like

Recorded because it is the failure hazard 64's sibling would produce and it is genuinely confusing:
`rustls-acme` reaches the ACME directory through `async-web-client`, which is built on
`futures-rustls` — a *different* TLS wrapper from the `tokio-rustls` this server uses. Both re-export
their own `rustls`, and `is_tls_alpn_challenge` takes the `futures_rustls::rustls::ClientHello` while
our resolver is handed the `tokio_rustls::rustls::ClientHello`.

Those two are the **same type** only because the lockfile resolves one `rustls 0.23.46` for both. Had
it resolved two, the composition would not have compiled — with an error naming a trait "not
implemented for" a type that appears identical to the reader, at a line where both spellings say
`rustls::server::ClientHello`. `crates/volucer-http/tests/one_rustls.rs` exists to turn that into one
sentence, and this note is why the test is worth its lines: the confusing error is the *likely*
outcome of a routine `cargo update`.

### Hazard 71 — `SIGHUP` un-issues a certificate, and the tests all agreed it did not

Found by running a question through the code rather than answering it from the summary, and it is
the second time in this phase that a *missing* path was the defect (hazard 68 was the first).

`Reloader::reload` rebuilt the certificate table with `SniResolver::refresh`, which loads exactly the
pairs the **configuration** names. That is the right shape for a named deployment and the wrong one
for an ACME deployment, because an issued certificate is not in the file: the configured pair is only
the fallback. So the ordinary operator action — edit a site, `kill -HUP` — put the **self-signed
fallback** back into service on a host that had a real certificate, with nothing in the log, and the
only symptom a browser warning that appears afterwards and gets attributed to something else.

Every existing test agreed it did not. `one_sighup_reloads_a_certificate_and_a_site_together` passes
because it *is* a named deployment and the refresh is exactly what it should do.
`an_acme_configured_server_serves_its_cached_certificate` passes because it never sends a signal. The
gap is the intersection, and the phase's own "ACME is additive" control — the 39
`tls.rs`/`frontend.rs` tests staying unchanged — could not see it either.

The fix is to **skip** the rebuild when the serving resolver has an ACME challenge source, rather
than to rebuild and then re-install from the cache. Both work while the cache read succeeds, but the
second is a repair *after* the table has been wrong, and the moment the cache read fails is the
moment a deployment is downgraded silently. Skipping makes the two halves unable to come apart: the
table is already correct because issuance installs into it, so the reload only moves the sites.

Two design details, both forced rather than chosen:

- **The reloader asks the resolver, not the engine.** `Arc<Engine>` is not `Send`, because `AcmeState`
  holds `Pin<Box<dyn Future + Send>>` (hazard 69's fact, met again from a new direction) — so the
  engine cannot be held beside the reloader at all. `SniResolver::has_acme` is the question that
  actually matters: not "is there an `[server.acme]` table" but "does the serving path consult an
  ACME challenge source", which is a property of the object answering handshakes.
- **The engine is moved into its thread rather than shared**, which is what keeps that `!Sync`
  requirement from reaching the signal handler. The compiler refused the shared version, and the
  refusal was the design correction.

`a_reload_keeps_the_certificate_that_was_issued` (`volucer/tests/serve.rs`) pins it, with a **control**
that matters as much as the assertion: it asserts the issued certificate is serving *before* the
signal, so the test cannot pass against a reloader that does nothing, nor against a build where the
engine was never wired up. Falsified by forcing the guard false: the fallback comes back and the
test fails with the certificate it restored. The plan records the consequence for the operator too —
migrating away from ACME needs a restart.

### Hazard 72 — requiring a certificate the operator did not need was a proxy for the invariant

Open decision §9.5 asked *"is a `[server.tls]`-less ACME-managed listener allowed?"* and `acme-config`
answered **no**, with this reasoning: a listener with nothing to serve fails every handshake that is
not a challenge, so name a fallback pair. The first half is true. The conclusion does not follow.

The invariant that matters is that the listener has **something to present**. Naming a self-signed
pair satisfies it, and so does generating one — and the second asks the operator for nothing. The
question the decision answered was about the *table*, and the question an operator has is about the
*work*: "why do I hand-make a certificate the point of which is to be replaced?" Requiring one was a
proxy for the invariant, and the proxy cost a step, a file and a private key on disk that nothing ever
trusted.

The reversal is not "no certificate is needed" — it is "**no certificate has to be named**". The
refusal that replaces the old one is narrower and better aimed: an absent pair with no ACME table is
still an error, because then nothing will ever replace it, and what the deployment would run is a
listener that looks healthy and is trusted by nobody. That is hazard 56's failure with the error moved
from startup to the client's browser, which is strictly worse.

Two design notes worth keeping:

- **In memory, not to a file.** A generated pair that is never written down means there is no cleanup,
  no path to get wrong, and no private key in a backup for a certificate nothing ever trusted. The one
  thing a file would add is the ability to inspect it, and `volucer check` prints that the placeholder
  is generated rather than showing a path — which is the honest form of the same information.
- **A generated certificate needs names.** A certificate with no SAN is one no client accepts, so the
  placeholder carries the scope's names: `[server.acme].domains` when there is one (literally "what
  this certificate must cover"), else the hosts of every site that has no ACME table of its own. The
  first version got this second case wrong and gave the deployment's placeholder a *site's* hosts —
  found by writing the test, not by reading the code.

The rule generalises past this decision: **when a requirement exists to protect an invariant, check
that the requirement is the invariant.** "Name a certificate" and "have a certificate" are the same
sentence only as long as nobody can generate one.

### Hazard 73 — the rule was enforced on the file and not on the account

Three independent reviews of the ACME configuration ended at the same complaint: *"presence of an
empty table as a boolean switch is an anti-pattern; make the mode explicit."* The diagnosis was right
and the proposed cure was wrong, and finding out why turned up a worse bug that none of them was
looking for.

**What the three proposals had in common, and why it does not hold here.** They wanted
`mode = "acme" | "manual" | "off"` — or `tls = "acme"` — as one setting per site, with the second going
further: *"a missing `tls` table = site over HTTP only."* That last one is not a preference this
server can honour. There is **one listener and one `rustls::ServerConfig`**; a site cannot be
plaintext while its neighbour on the same port is TLS. The types already say so — `SiteTlsConfig`
holds only `certificate` and `key`, while `min_version`, `alpn` and `plain` are `TlsConfig`-only
because they are a property of the socket. So `mode = "off"` reads as a per-vhost protocol choice and
can only mean something else: at server level it is self-contradictory (TLS-off is written by removing
the table, and "no certificate" is illegal since a listener must present something), and at site
level it can only mean "no certificate of my own".

Two smaller findings from testing the proposals rather than reasoning about them: a scalar cannot
coexist with the table of the same name (`duplicate key 'acme' in table 'sites.corp'` — TOML rejects it
before serde, so `acme = true` is not an alias you can add, it is one you must choose), and
`key_type` has nothing behind it (`rustls-acme` hardcodes ECDSA P-256 for both the account key and the
certificate key — grepped, not assumed).

**What was wrong was narrower than "the table is a bad switch".** One question the layout genuinely
left implicit: *how do I keep these settings and stop ordering?* Deleting the table is the only
answer today, and it throws away a contact and an authority. So `enabled: bool = true` was added to
both tables. It is not inherited from `[server.acme]` to a site, deliberately: a site that inherited
`false` would go quiet when the *deployment's* orders were turned off, and the deployments with
several unrelated hostnames — the ones with several orders — are exactly the ones that cannot afford
it. The switch also gave the layout its missing defaults-only table: `[server.acme] enabled = false`
stages a contact and an authority for every site that inherits them, without issuing for the
deployment's own names (whose list may then be left out, since it orders nothing).

**The bug underneath, which is the part worth remembering.** Checking whether "inheriting the
contact" actually shared an account, the account file was the hash of the *empty* list:
`cached_account_Z_J1BlIxQlHarXbwEhQdM8n3KQ9PjrsKAGp_AtOUqQQ`, where the configured
`mailto:ops@example.com` hashes to `cached_account_Bf6HANfw_u1_Q1RQbUQq0mtYHJb9KL6LfXJkTJuy2GA`.
`Engine::new` never called `.contact(...)`. Four consequences, all of them silent:

- Let's Encrypt never received a contact address, so **the CA had no channel to warn that renewals
  were failing** — the failure this feature is most prone to, and the reason `check` prints
  `(none: the CA has no way to warn you that renewals are failing)`. It printed that beside a
  `contact` key the operator had filled in.
- The documented account-sharing rule was wrong in both directions: `[]` is constant, so every scope
  on one authority hashed to *one* account and "two tables whose contacts differ create two accounts"
  described the impossible.
- Site inheritance of `contact` was inheritance-to-nothing, so the layout question above was partly
  being answered about a setting that did nothing.
- **No test noticed**, because `contact` appeared only in fixture strings. The key was validated,
  documented, printed, and inert.

Falsified the honest way rather than by assertion: `two_contacts_produce_two_accounts` starts one
server with a deployment table and a site table carrying *different* contacts into one cache, and
asserts two account files appear. The account file is written on the engine's first poll
(`state.rs:417`, `store_account` before any network call), so the test needs no CA — which is also
what explained an earlier probe where three scopes produced one file. Removing `.contact(...)` made
it fail in 45 s with one file; restoring it made it pass in 0.4 s.

**The general rule, and it is a sharper one than hazard 72's.** Hazard 72 said: when a requirement
protects an invariant, check the requirement *is* the invariant. This is its sibling: **a setting that
is read by the code that reports it is not thereby honoured.** `check` printed `contact` because the
*configuration* had it, and the account was registered without it — the two paths never met. A
validation that reads a field and a consumer that uses it are different things, and only a test that
observes the consumer can tell them apart.

### Hazard 74 — the MSRV was a promise nothing had ever checked

`[workspace.package] rust-version = "1.85"` sat in the manifest from the beginning, and CI is what
found out it was false. On a real 1.85.1 toolchain `cargo check --workspace` refuses the workspace
before compiling a line of it:

```text
error: rustc 1.85.1 is not supported by the following packages:
  time@0.3.55 requires rustc 1.88.0
  time-core@0.1.9 requires rustc 1.88.0
  time-macros@0.2.32 requires rustc 1.88.0
```

1.88.0 checks clean. `cargo tree -i time` says where it comes from, and the answer is **both** of
the crates that arrived with TLS: `x509-parser` → `der-parser` → `asn1-rs` → `time` (the ACME work)
and `rcgen` → `time` (the placeholder certificate). So the number was not stale — it was never
true for a tree that had TLS in it, and the TLS work landed several steps before anyone asked.

**The general rule.** A declared minimum is a claim about a machine that nobody in the development
loop is using: the only toolchain anyone had was 1.99, where the claim cannot be falsified. It is
the same shape as hazard 68's *"the parts were built and the server never used them"* — a statement
in the repository that nothing observes. It is also why the fix is a **job** and not a corrected
constant: `cargo +1.88.0 check` runs on every push now, so the number fails a build the day a
dependency raises its floor rather than the day a user finds out. The constant alone would have been
just as unverified at 1.88 as it was at 1.85.

**A near-miss worth recording beside it.** The MSRV job would have failed on whitespace if the
tree's formatting depended on the rustfmt version: `cargo fmt --all --check` is part of the same
workflow, and 1.88 and 1.99 ship different rustfmt builds. Measured rather than assumed — rustfmt
1.88.0 reports no diff on the tree formatted by 1.99.0. Had it differed, pinning the formatter to
the MSRV toolchain would have been the choice, and the check would have needed a comment saying why.

### Two rules that were one rule at one level and another at the other

Found while checking the same ground, and fixed in the same change.

**A named pair beside an ACME table.** `[server.tls]` with a certificate and key *plus*
`[server.acme]` was accepted; the same shape at site level was refused as *"two answers to one
question"*. It is not two answers — it is one now and another when the order completes, which is what
the machinery already does (`generated_sites` gives such a site no placeholder, and `install_issued`
overwrites the named entry for the names it covers). The server-level behaviour is also the migration
path from a certificate issued elsewhere, which a site-level deployment has just as much need of.
The site-level refusal was the wrong half of the pair, so it was removed and the levels now agree:
`a_site_may_name_a_certificate_and_ask_for_one_at_the_same_time` pins it, and
`a_site_that_names_a_certificate_without_acme_is_not_an_order` keeps the half that survives — with no
table there is no order, so the pair is the certificate and must be real.

**One name, two tables.** A name in both `[server.acme].domains` and a site's derived list was
accepted, and could never converge: a validation handshake carries only the SNI name, so the
deployment's challenge source answers first — with a challenge certificate committing to the
*deployment's* key authorization while the CA is asking this server for the site's. The site's order
failed validation on every retry, and the log said only that a step had failed. Now refused, naming
both tables, with the error message naming the edit that fixes it
(`a_name_issued_for_by_two_tables_is_refused_naming_both`, and
`a_site_that_states_its_own_domains_no_longer_overlaps_the_deployment` for the way out).

`enabled` reaching the four readers that had read "the table exists" as "something will issue" —
`any_acme`, the `generated_sites` filter, `default_names`, and the `serve` scope loop — is what these
two rules turned out to depend on, which is why they were worth doing in one change rather than three.

### Hazard 75 — the matrix found a bug only macOS could find, and a flake that looked like one

Step 69's whole argument was that a cross-platform claim nobody tests on three platforms is a
hypothesis. The first run of the workflow repaid it immediately, with **two failures that are not
the same kind of thing**:

**macOS, and a genuine platform bug.** `is_running` in `worker_pool.rs` was
`PathBuf::from(format!("/proc/{pid}")).exists()`. **`/proc` is Linux, not Unix** — macOS has no
procfs, so the predicate was unconditionally `false` there, and
`max_requests_zero_means_never_recycle` asserts the original worker is *still running*. It failed
deterministically, for a reason with nothing to do with recycling. The sibling helper in
`tests/pool.rs` already had the portable `kill -0` fallback; the second copy had been written
without it. **Only the macOS job could have found this**: every other platform either has `/proc`
or takes the `tasklist` arm, so a predicate that was always false was never exercised. This is the
sharpest argument for the matrix in the file.

**Linux, and a flake — which is a different finding, and worth naming as one.** `free_port` binds
`:0`, reads the port and closes it, and the six tests in `php_cgi.rs` run in parallel in one binary,
so two can be handed the same port. A test then connected to *another test's* `php-cgi`, which
answered the readiness probe and dropped the connection as `ConnectionReset` — the process had
already been killed by its owner's `Drop`, and `php-cgi` serves one connection at a time. It
**passed on rerun**, which is what a port race looks like and not what a bug looks like.
`volucer-server/tests/common/mod.rs` had already solved exactly this for the pool tests, with the
diagnosis written beside it; `volucer-fcgi` had no such lock.

**The general rule, and it is the third costume this project has seen it in: a test that cannot fail
on the machine it runs on is not evidence.** Hazard 57 was a test asserting *nothing changed* that
passed against a test that did nothing. Hazard 62 was a stub that reimplemented the thing under test,
so the falsification proved the stub. This is the same defect with the platform supplying the
blindness: the macOS predicate could not be true, and the Linux race could not be *reliably* false.
Neither is visible from one machine — and note that `fmt`, `clippy` and `cargo package` were all run
locally first and passed, so the local runs were not what caught this.

### What the live Pebble run established, and where it stopped

`acme-verify`'s plan asked for offline issuance against **Pebble**. Pebble was built and run here
(Go 1.22 in WSL, `pebble v2.10.1`), and volucer was pointed at it through a temporary trust seam —
the environment supplying both a directory URL and a CA root, because Pebble's `minica` root is in no
root store. That seam was probe code and has been reverted; nothing of it is committed.

**The issuance flow ran for real, and the account step completed.** The log shows an account created
and cached:

```text
INFO no cached certificate: an order will be started, which is expected on a first deployment
INFO ACME issuance started authority="staging" domains=["volucer-pebble-run.dev"]
... cached_account_Z_J1BlIxQlHarXbwEhQdM8n3KQ9PjrsKAGp_AtOUqQQ
```

and `rejectedIdentifier` responses **only after Pebble had parsed and policed our identifiers**:

```text
Invalid identifiers requested :: Cannot issue for "pebble.test": Domain name does not end with a
valid public suffix (TLD)
Error creating new order :: Cannot issue for "example.com": The ACME server refuses to issue a
certificate for this domain name, because it is forbidden by policy
```

That is a stronger statement than a unit test makes: **JWS signing, the `directory` fetch, the
nonce dance and `newOrder` all work against a real ACME server**, and both rejections are Pebble
policies rather than our protocol errors. (`example.com` is on Pebble's own blocklist; `pebble.test`
has no public suffix. A third run used `volucer-pebble-run.dev` and got past that check — the
`tls-alpn-01` **challenge was then attempted, and the DNS step failed** because the stub resolver this
host's Pebble build was given never received the queries. That is a harness gap, not a volucer one:
the resolver never saw a single packet, at `127.0.0.1` or `0.0.0.0`, with A *and* AAAA answers
verified present by a hand-written DNS query.)

**So the honest status of step 65 is: partly verified.** The protocol path up to and including
`newOrder` is verified against a live ACME server here (hazard 68's lesson — *run it* — is what
produced this). The validation and finalize steps are verified only through the cached-certificate
path, and **the full issuance has been verified outside CI by hand, but not completed on this host.**
What remains is a harness detail on WSL, not a question about the code, and the plan says so rather
than claiming a green run it did not get.

Two things the attempt bought anyway, both worth keeping:

- **Pebble's directory advertises `profiles`** (`"profiles": {"default": "The default profile"}`), and
  its config carries a `shortlived` profile with a 518400-second validity. So Gap 2 — rustls-acme
  sends no `profile` field — is checkable against Pebble rather than only arguable, and the profile
  we would need to request is one a real ACME server exposes.
- **A `challenge` rejection names the domain and the reason**, which is what the `check` days-left
  report is for. Reading the failures above is exactly the operator experience the CLI step is trying
  to replace.


### Hazard 76 — the parser the plan named could not live where the plan put it

Step 68's sub-plan said `validate_backend` would check an upstream origin by calling
`volucer_upstream::Origin::parse`. That is one line in a table and it cannot be written: `volucer-core`
is the leaf crate — `serde`, `thiserror`, `toml`, `tracing` — and `Origin` is hyper-typed
(`base: hyper::Uri`, `authority() -> &hyper::http::uri::Authority`), so the call inverts the graph.
The commit-1 work had also left `volucer-upstream` declaring a `volucer-core` dependency it never
referenced, which is the small version of the same confusion.

**The fix is the codebase's own existing pattern, and finding it was the useful part.** `ChecksumMode`
already lives in `volucer-core` as a *mirror* of the worker protocol's policy, with a comment saying
"so that the configuration model stays free of the protocol crate, which is what lets `volucer-core`
be the leaf that every other crate depends on". So the origin parser moved down as `OriginSpec`,
hyper-free, and `volucer-upstream::Origin::from_spec` became the **only** constructor of the
client-side type. That is better than a working call would have been: `OriginSpec` has no public
fields and one parser, so "the origin was validated" is a property of the program rather than of the
order two functions happen to be called in.

Two consequences worth having, both of which the plan could not have anticipated: `preflight` can now
describe a *structured* origin — scheme, host, resolved port, socket path — instead of only asking
"does it parse", which is what its "nothing is listening yet" and "this hop is in clear text"
warnings need. And the parser is now hyper-free, so `http://` and `https://` are refused by *name* in
one place rather than by whatever `hyper::Uri` happens to accept: `http:foo` used to reach a different
error from `http://`.

**The general rule: a plan that says "call X from Y" is making a claim about the dependency graph,
and that claim is as falsifiable as any other.** It read like a detail because it was one sentence in
a table of places that must learn a new enum arm — and every other row in that table *was* a detail.

### Hazard 77 — a test that measured its own fixture, twice

Step 68's sub-plan named the `spawn_blocking` boundary as risk 2 and prescribed the test: "a slow
origin *and* several concurrent requests — a serialised front-end would pass every single-request
test". The prescription was right and the implementation was wrong twice, in two different ways, and
both were only visible by falsifying.

**First: the fixture was the thing being measured.** The test origin was a stdlib accept loop that
served each connection *inline*. Five requests to an origin sleeping 400 ms each took 2000 ms — which
is exactly the signature the test was written to detect, produced entirely by a single-threaded
fixture. So the test failed against correct code, and the tempting read was "the front-end is
serialised". One thread per connection fixed the measurement and not the premise.

**Second: the premise was wrong even when the fixture was right.** With the origin concurrent, the
test *passed* when the upstream path was wrapped in `spawn_blocking`. That pool is not a single slot,
so "N concurrent requests take N times as long" is not the failure mode — the actual one is a runtime
with no thread left to drive the other connections. The test now sends **ten** requests against the
two-thread runtime the harness builds, five times over-subscribed, and fails correctly when a
`std::thread::sleep` is introduced into the async path. The doc comment says why the thread count is
the load-bearing part.

**The rule this is the third instance of, and the sharpest statement of it yet:** a test whose
falsification is never run is not evidence, *and* a falsification that passes is itself a result —
it means the test cannot see the bug it was written for. Both readings here would have been wrong
without one. Hazard 57 was a test asserting nothing-changed against a test that did nothing; hazard 62
was a stub that reimplemented the thing under test; this is a fixture that reimplemented the *failure
mode*. In each case the test was green and the green meant nothing.

### Hazard 78 — the change the plan called the risk was the change that cost nothing, and the plan had already worked out why

The sub-plan's risk 1 was `UnsyncBoxBody`: "a whole-file change to `volucer-http`'s public types and
touches `static_files.rs`, which is 66 KB and the hottest code in the server… if it moves any
`static_files` test it comes back out and streaming goes behind a second response type."

It moved none. Two constructions existed — `body_of`, and one `BoxBody::new(FileBody { .. })` — and
`FileBody` was already `Send`. The plan had *already verified* the reason in §3.7: `hyper`'s server
bounds the response body as `B: Body + 'static` with no `Send` or `Sync` (`http1.rs:536-541`), so the
looser type is not a compromise and nothing downstream narrows it again. The risk estimate was about
the *size of the diff* rather than the strictness of the bound, and those are not the same question.

**The general rule, which is the mirror image of hazard 77: a risk whose mitigation is "revert it if
it breaks" should be attempted early, because the measurement is the mitigation.** Two commits and a
full test run settled it, and the alternative — designing a second response type around a bound that
turns out not to bind, to protect against a failure that cannot occur — would have cost real design
work on behalf of a guess.
### Hazard 79 — the access log's own injection was a space, and every positional assertion missed it

The access log's text format is nginx's `combined` shape: space-separated, one record per line, with
client-controlled values in it. The obvious injections are a **quote** (which would end the quoted
field early) and a **newline** (which would forge a whole record), and the first version escaped both.
The security test asserted the field positions of a hostile line and passed.

It then asserted the field **count**, and failed with **31 fields where the format has 15**. A hostile
`User-Agent` of `evil 1.2.3.4 - - [now] "GET /admin HTTP/1.1" 200 1` split into seven tokens: the
client's own text was being read as fields of *this* log line, and its `200` landed where this
server's status belongs. Escaping quotes and newlines had hidden nothing worse than a mangled field;
the **space** is what made the other two matter, because without it a value cannot be told from the
field separator at all. Escapes are now nginx's `\x20`, `\x22` and `\x5C`, so one parser still reads
both logs.

**The general rule: in a delimited format, the delimiter is the injection, and positional assertions
cannot detect a shift in the fields they assert on.** Every `assert_eq!(fields[8], "200")` passed,
because `fields[8]` *was* `"200"` — the hostile value had supplied one. Only a count of the whole
record catches a field that should not exist, which is the same reason hazard 62 needed a sampler
rather than a request-level assertion: the failure was in a property no single value describes.

### Hazard 80 — a falsification passed because the test used a value that could not distinguish

The request id's central promise is that an **adopted** id comes back exactly as it arrived, because
the client is looking for *its* string in *this* server's log. The unit test asserted that over
several shapes and would have caught a re-render. The end-to-end test asserted it too.

Falsifying it — lowercasing the echo — **passed**. The test's UUID was already all lowercase, so
lowercasing was a no-op. The assertion was proving nothing about verbatim-ness: a value that already
matches its normalised form cannot test a normalisation.

With the value changed to mixed case (`3F2504E0-4f89-11D3-9A0C-0305E82C3301` — upper- and lowercase
hex, which is a legal client id and is *not* its own normalisation) the same falsification fails on the
exact text, as it should.

**The general rule: a test input must be able to distinguish the bug from the fix, and "it looks like
a real example" is not that property.** A UUID is the realistic shape and exactly the wrong one here,
because real UUIDs are usually lowercase — so the realistic fixture was the one that could not fail.
The unit test had the same shape of hole and survived only because it enumerated five inputs.

### Hazard 81 — the same sequence of characters meant two different things to two programs

Writing a commit message through PowerShell, then `git commit -F`, failed with *"a NUL byte in commit
log message not allowed"* — and the byte really was there: the message contained a NUL at the offset
where the text read `version 00 only`.

PowerShell treats a backtick followed by a digit as an escape, so a literal backtick-`0`-`0` in prose
became `U+0000`. The same three characters were *text* to the editor that wrote the file and a
*control code* to the shell that stored it.

**The general rule: prose that quotes an escape sequence must not go through a tool that interprets
it.** The fix is a single-quoted here-string (which processes no escapes) or a doubled backtick, and
the check is one line — `ReadAllBytes` and count the zeros — which is far cheaper than the diagnosis.
The same class of mistake is `Add-Content` mojibaking non-ASCII (`—` becoming `â€”`), recorded in
hazard 77's neighbourhood: **every write of non-ASCII text on this host gets byte-verified afterwards,
and that is now a habit rather than a lesson.**

### Hazard 82 — the extraction check was dead code, and one format said so

The first `Archive` listed a tar through `PharData`, the obvious choice. **`PharData` normalises a
stored name as it reads it:** an entry whose raw header says `../evil.txt` is reported as a
*directory* named `..`, and its payload as no leaf at all. So the traversal check never saw a `..`
component, passed, and defended nothing — for tars.

It was noticed because **`ZipArchive` does not normalise.** `statIndex` reports `../evil.txt` and
`/abs.txt` verbatim, so the same hostile entry was refused in a zip and silently accepted in a tar.
The asymmetry between two libraries answering the same question was the whole signal, and it only
appeared because the hostile case was asserted **for both formats** rather than for the one that was
convenient.

`TarReader` now reads the 512-byte headers directly, so both formats hand `Archive` the same faithful
list and one check guards both. Two follow-ons are assertions rather than intentions: the reader's
fidelity is tested by name (`../evil.txt` must be *present* in its listing — if it normalised, `open`
would succeed and the name would be absent from its own output), and nothing from the `phar://`
wrapper is read at all.

**The general rule: a defence that a library normalises away is not a defence, and the way to find
out is to assert the hostile input through every path that can receive it.** A check tested against
one format tests one format.

### Hazard 83 — the fallback reported every status as zero, and only the other transport could say so

`StreamTransport` read `$http_response_header` in a **different method** from the one that made the
request. That variable is a *local of the calling scope*, not a global — the HTTP wrapper populates
it in the function that performed the request. Read anywhere else it is an empty array, which parses
to a status of `0`, which every caller reads as "not 200".

On a host without curl that makes a successful API response look like a server error and every
download look like a failed one. **The tool would be broken on exactly the machines that have no
curl, which are the machines least able to diagnose it** — and the development host has curl, so
every local run was green.

It was found by running the *other* transport rather than by reading it, and confirmed by reproducing
both arrangements side by side: the original returns `0` where the fixed shape returns `200`.

**The general rule: a fallback is a second implementation, and an untested implementation is not a
fallback — it is a second bug waiting for the environment that needs it.** The status is captured in
the scope that made the request now, and both transports run against the same fixture server with the
status asserted for a 200, a 404, a 500 and a redirect.

### Hazard 84 — the fixture did not carry the property the assertion depended on, and only two of three platforms said so

The test proving the executable bit survives extraction built its binary entry with the **default
mode of 0644** and then asserted `is_executable()`. That is false, and correctly false: the code
preserves each entry's mode, and the entry said 0644. **A fixture that does not carry the property
the code relies on cannot test the code** — hazard 77's shape, a test measuring its own fixture.

The assertion was guarded by `DIRECTORY_SEPARATOR === '/'`, because Windows has no such bit to read
back — so on the development host it never ran. Locally green; CI had Ubuntu and macOS red and
Windows green. **The guard was correct and the conclusion drawn from it was not**: "Unix-only
assertion" had quietly become "assertion that cannot fail where I am".

**The general rule: a platform-guarded assertion is invisible on the platforms that skip it, so a
local pass says nothing about it.** Same hazard as `/proc`-is-not-Unix (hazard 75) — a property
asserted on one platform and reported as a pass everywhere. The fix was reproduced in WSL before it
was pushed, so it was verified where it failed rather than inferred from where it passed.