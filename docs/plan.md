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
                        │   └► Host ► site (root, controller)      │──► worker pool    (php worker.php, persistent)
 nginx  ──FastCGI─────► │ FastCGI listener                         │──► php-fpm         (external unix socket / TCP)
                        │  └► CGI env builder ► dispatcher         │──► php-cgi.exe     (N processes, N ports, Windows)
                        └──────────────────────────────────────────┘
```

- Both front-ends are just **listeners** that produce the same CGI environment and hand off to
  one dispatcher — so `stokehold serve` can be hit directly by a browser **and** spoken to by
  nginx. One binary, no second application.
- The HTTP listener resolves the `Host` header **before** anything else, because that is the one
  decision the FastCGI path cannot make for itself: a `host` matcher in the route table is
  consulted after `SCRIPT_FILENAME` is resolved, so it can choose a backend but never a document
  root. `[sites.<name>]` gives each host its own root, controller, routes and static layer, and
  the routing for each is built once at startup rather than per request.
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

There was a fourth backend kind, `spawn_fpm`, which stokehold would have started itself. It
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
| TLS | **rustls**, not OpenSSL — no DLL/dylib matrix across three platforms |
| Worker transport | child **stdin/stdout pipes** — portable, and needs no port per worker |
| Config format | **TOML** (`serde`) — no YAML dependency, better tooling |
| Front-end scope | HTTP listener **in v1**; TLS/ACME/HTTP-2 as a later milestone |
| Prefork | **deferred**, Unix-only, forked from a PHP boot parent — never from the Tokio runtime |
| HTTP front-end runtime | **`hyper` on `tokio`, confined to `crates/stokehold-http`** — the target is the public-facing case (static files, HTTP/2 now, HTTP/3 later), and HTTP/3 needs `quinn`, which is itself a `tokio` program. So the runtime was arriving anyway; this is that bet made earlier rather than a second one. Every backend stays blocking and is reached only through `spawn_blocking` |
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
   `crates/stokehold-server/src/worker_pool/` (`exchange.rs`).
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

25. **The worker arm spent a thread per request, and removing that turned the only losing arm into
    a winning one.** On the trivial handler the piped worker used to reach 20,422 rps at concurrency
    32 while `spawn_cgi` with the *same* 12 slots reached 29,950 — and RoadRunner's piped worker,
    speaking the same protocol shape, reached **31,525**. So stokehold *lost* the direct
    worker-versus-worker comparison by about 54% at concurrency 32, having been level up to
    concurrency 8 (17,471 vs 16,610). The cause was `request_with_deadline`
    (`crates/stokehold-server/src/worker_pool/exchange.rs`): a pipe read has no portable timeout, so each
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

    **Stokehold now leads RoadRunner at every concurrency** — +10% at 32, +42% at 16, +42% at 8 —
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
    on stokehold's side rather than a property of loopback TCP. With that fixed the two transports
    measure the same (1.238 ms against 1.219 ms), so the option is kept on its own merits —
    convention, and a filesystem path being easier to police with permissions than a port — and
    **not** as a performance recommendation.

    **Implemented** as `socket = "unix:/run/stokehold/cgi"` in a `spawn_cgi` backend
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
    framework by a multiple. `deploy/stoke/` and `scripts/fw-bench.sh` in `azera-competition` add
    stokehold as a third server so the comparison has two independent server pairs rather than one.

    **What is deliberately not claimed here: my own four-arm run's cold numbers.** In that run
    stokehold's `spawn_cgi` beat php-fpm on azera (1,115 vs 402 rps) and lost to it badly on Laravel
    (21.8 vs 49.5), and neither figure reconciles with the published FPM rows above (azera should be
    ~1,274 rps, not 402). A cold-versus-cold disagreement in *both* directions means the cold arms
    are measuring something other than the deployment model, so they are not evidence for anything
    and the published dataset is used instead. The warm-over-cold ratio measured *within* stokehold
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
    backend. Both pools compared `served >= max` unguarded, so copying that line made stokehold boot
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
    trace showed php-cgi's trailing 1728-byte segment leaving **10 us after** stokehold's ACK
    arrived 40.7 ms after the data — sender-side Nagle, receiver-side delayed ACK.

    **The fix** is receiver-side and belongs in `stokehold-fcgi`: `TCP_QUICKACK` immediately
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
    `fcgi_accept_request` reads the request, so stokehold's writes fall in a Nagle-eligible
    window. Reading the guard at line 1088 shows the flag is never set on Linux at all, so the
    window never closes rather than closing late. The conclusion happened to be delayed ACK;
    the reasoning was refuted by source.

    **Wrong answer 3 — "the 8-segment write".** `tcpdump` showed stokehold sending each request
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

    | conc | P stokehold | R RoadRunner | F php-fpm | U stokehold unix | T stokehold TCP | R/F |
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
      the warm half of the table credible. *(Superseded by hazard 33: the 10% was stokehold's
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

33. **stokehold's HTTP front-end used to cost 1.36x RoadRunner's CPU per request, and the whole
    of it was three avoidable scans. Fixed — stokehold is now the cheaper of the two.**

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
    | P stokehold (before) | 16,017 | **413 us** | 608 us | 1,021 us |
    | R RoadRunner | 17,860 | **309 us** | 704 us | 1,013 us |

    The totals agree to 1% while the server halves differ by 1.34x, so the gap is entirely
    stokehold's front-end and nothing else — not the protocol, not the client, not PHP.

    **`scripts/diag-profile.sh` then named it.** A flat `perf record` on the server process during
    a saturated run put 15-19% of *everything* in one function, and two more above 5%:

    | symbol | self time, before |
    |---|---|
    | `stokehold-worker::codec::Channel::read_frame` (inlined `crc32c`) | **14.81-19%** |
    | `stokehold-fcgi::client::parse_response_headers` | 5.20% |
    | `stokehold-fcgi::client::split_response_body` | 5.17% |

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
    | stokehold rps @ conc 32 | 16,017 | **18,946** |
    | RoadRunner server CPU per request | 309 us | 153 us |
    | RoadRunner rps @ conc 32 (re-measured) | 17,860 | 18,019 |

    Two of the three were correctness fixes as much as speed ones: the lossy conversion could
    let a non-UTF-8 byte *in the body* move the located header terminator, because the offset was
    computed on a `String` whose length differs from the slice's. `parse_response` now scans once
    and returns status, headers and body together, with the two older entry points delegating to
    it and a test pinning all three to each other.

    **The general lesson, which is hazard 30's lesson again:** "stokehold is N% slower than
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
    `stokehold-fcgi/tests/allocations.rs` and `stokehold-worker/tests/allocations.rs`. A new
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
    - **`read_record` in `stokehold-fcgi` is the allocating reader** and is still what every call
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

    Both halves are pinned, deliberately split by crate. `stokehold-worker`'s
    `tests/request_path.rs` drives a whole round trip over the real codec, so a reintroduced
    clone anywhere on that path fails there; `stokehold-fcgi`'s `tests/allocations.rs` holds the
    router. A slope test in the crate whose source changed is worth more than an exact count in
    a crate that only consumes it.

    The one thing to know when writing these: **compare bytes, not decoded structs.** Every field
    of `RequestHeader` except `method` and `uri` carries a serde `default`, so a field missing
    from the borrowed serialization view would not fail a `Json::decode::<RequestHeader>`
    round-trip at all — it would arrive empty. The byte-equality test exists for exactly that.

    **`stokehold-core` is not a target.** It was the question this pass started from, and the
    answer is that it is cold path: configuration, PHP discovery and preflight all run once at
    startup. Its only per-request functions are `RouteConfig::matches` and `Config::route_backend`,
    and `route_backend` is **dead code** — `Dispatcher::backend_for` duplicates the filter-and-max
    rule inline, so a route change has two implementations to keep in step.

**38. Both transports entered the kernel once per frame field.** Hazard 37 asked where the
    request path rewrote a request it already had. This pass asked the neighbouring question —
    where it *entered the kernel* more than it had to — and the answer was every transport in
    the project.

    The rule the codebase already knew, in `stokehold-fcgi`'s client: *"assemble the whole request
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

    - **`stokehold-worker` gained `BoundedChannel`**, a `WorkerChannel` over
      `BufWriter<Box<dyn Write + Send>>` and `BufReader<Box<dyn Read + Send>>`. It is a new type
      rather than a change to `WorkerChannel` because that type is generic over its streams on
      purpose — the tests drive it over a `Cursor` — and because the buffering is a property of
      talking to a *pipe*, not of the protocol.
    - **`stokehold-fcgi`'s `Client` now holds `BufReader<T>`** rather than `T`. Writes go through
      `get_mut()`, so the one-write-per-request guarantee is untouched and `client.stream`
      became `client.stream.get_ref()` at the two test sites that inspect what was written.
    - **`stokehold-server`'s listener wraps its accepted socket in a `BufReader`** and writes
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
    (`stokehold-http`) uses hyper's own buffered IO and is not affected; the **static file**
    path is dominated by the file read itself; and `stokehold-server`'s listener still flushes
    the response once per record, which the tests pin at five writes for a response with no
    `STDERR` — assembling it first would make it two, but the response is written once per
    request and the reads were the expensive half.

    **The wall-clock answer, measured afterwards.** Re-running the three stokehold arms on the
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
    socket was the alternative and is what `stokehold-fcgi`'s client does; here the response has
    a known end, so assembling it whole is simpler than reasoning about where a flush must land.

    Pinned at **five writes → one**, and the bytes asserted equal to the record-by-record
    framing via the existing `append_records` twin. The old test that counted writes to prove a
    large body was still split was rewritten to walk the framing **by bytes**, because a write
    count can no longer distinguish "one write, five records" from "one write, one record" — and
    the new count asserts a 300 KB body leaves in one write rather than one per record.

    Verified end to end against **real nginx** (this front-end's actual peer, which the unit
    tests cannot stand in for): `GET /` and `GET /items` both 200 through nginx →
    stokehold FastCGI → php-cgi, 30/30 requests over keep-alive, correct HTML, and an empty
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
    request frame* from `stokehold-server/tests/request_header_allocations.rs`, a different layer
    that the FastCGI path never pays. So the first thing was to measure `to_cgi` itself, and the
    measurement says stop.

    `crates/stokehold-http/tests/to_cgi_allocations.rs`, release build, 200,000 iterations, across
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
18. `fastcgi-frontend` — **done**, in `crates/stokehold-server/src/frontend/`. Accepts FastCGI
    from nginx, translates it into the same `Dispatcher` the HTTP front-end calls, and re-frames
    the response, with a pass-through fast path for a record it can forward unchanged. It is
    wired in `stokehold-cli`'s `serve` alongside the HTTP listener: the FastCGI listener owns the
    calling thread while HTTP runs on its own, so both are served concurrently, and `serve` binds
    FastCGI first because binding is synchronous while the HTTP listener is not. `server.fastcgi`
    unset means disabled. `tests/frontend.rs` drives a real request through the whole chain and
    asserts the bytes come back intact, alongside the several-in-a-row, 502-on-backend-failure,
    real-socket codec and build-from-parser cases. **This step was implemented but the plan never
    said so** — it read as unbuilt until this line was corrected.
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
    already has. The `Sites` table is the map SNI certificate selection needs, so this step no
    longer has to grow one.
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
28. `sites-resolver` — **done** (`b609cf6`). `stokehold_core::sites::Sites`: exact name, then the
    longest `*.` wildcard, then `[server]`; ports and case are the client's spelling. It returns
    configuration rather than a `Routing`, because `Routing` lives in a crate that depends on this
    one. Two things the tests forced: the default front controller is applied here rather than
    left to the CLI, and `static_files` in a site table is a table of *optional* keys — a
    `StaticConfig` has defaults, so `{ precompressed = true }` would also have asserted
    `enabled = false` over the server's setting for the whole document root.
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
34. `sites-fastcgi` — **outstanding.** The FastCGI front-end still calls `dispatch_to_default`, so
    the route table is bypassed for a request arriving from nginx. Behind nginx the document root
    cannot be re-decided anyway — nginx resolved it — but the *backend* could still be chosen from
    the route table, and that is a real difference from the HTTP side.
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
    the recycling contract is otherwise silently unenforced.
38. `script-extensions` — make "what counts as a script" configurable per site instead of the
    hardcoded `[".php", ".php3", ".phtml"]`. Required before a non-PHP worker can serve `.py` or
    `.rb`: `path_suffix` can *route* such a path, but `has_php_extension` decides what resolves as a
    script, so today `/foo.rb` reaches the front controller as `PATH_INFO` instead.
39. `plugin-abis` — only if a real need appears. RoadRunner's "plugins" are statically linked Go
    packages resolved by a compile-time DI container (`container/plugins.go`; there is no
    `plugin.Open` anywhere), so the enum backend kind stays the extension point.
40. `route-policy` — `RouteConfig` gains `policy` and `backend` becomes optional, so a route is a
    location rather than only a backend selector. §4.8 of the design notes.
41. `compression` — on-the-fly gzip/brotli, and the ETag and negotiation-cache questions it raises.
    Belongs with packaging (step 22), not with sites.

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
2. **The step list above** (Phase 0–7) — which steps are done; steps 18, 19, 20 and 21 carry a
   **done** marker and the rest say what is still open. Step 18 was implemented long before the
   plan said so; that marker is now corrected.
3. **"What actually works today"** below — the same question asked of a running binary, per
   capability, with the probe that established it. This is the section that answers "does it do
   HTTP/2 / SSL yet" without reading code.
4. **The hazards** — numbered findings under "Operational hazards to build in from the start"
   and after it, each one a measured result with its controls. Hazards 45–49 are the Windows
   transport and FastCGI work.

The raw evidence for the benchmark claims is **not** here: it is committed to
`stokehold-competition` as `results/raw-*.tsv`, alongside that repo's README, which carries the
conclusions. Anything measured on this host that is not in one of those two files did not
happen.

### The published benchmark and the current build

`stokehold-competition/results/report.md` now names its build as **`91cc7b6`**, and every
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
namespace, npm, PyPI, Docker Hub and DNS. `stokehold` itself is **still free on crates.io**.

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
  engine metaphor `stokehold` came from, and clean on crates.io, npm and both domains.
  `bowshock` (the shock wave ahead of a supersonic vehicle) is the better pure-speed metaphor and
  equally clean; `zephyrix`/`plasmax` are the invented-fantasy option.

### Open decisions, restated

Open decisions 1, 2 and 5 are unresolved; 3 and 4 are answered in place. Of the hazards, 23 and
26 are closed (23 by hazard 33's CPU attribution, 26 by the shipped `spawn_cgi` socket support),
and 27 and 28 are recorded findings rather than open questions. What is left is a short list:

1. **Default backend out of the box** — open decision 1.
2. **msgpack behind the codec boundary** — open decision 2, deferred pending a profile.
3. **Configuration reload on `SIGHUP`** — open decision 5, *deliberately not built*, with the
   three options and the reason a half-reload is worse than none.
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
   arm in `stokehold-competition` is FrankenPHP, and it is indistinguishable from stokehold at
   concurrency 1 (1.018x, spread spanning 1.00) and 32% behind at 32 (0.677x). CPU accounting rules
   out the easy answer: at concurrency 16 it consumes 7.17 of 12 cores while serving *fewer*
   requests per second than stokehold, which uses 8.10 — so it is not CPU-bound, and its ceiling is
   in dispatch
   or queueing rather than in what its interpreter costs per request. Worth knowing because it is
   the shape a "PHP embedded in a Rust server" design would land in if the embedding were done
   naively; it is not a defect this project has to fix, and the arm exists to price a decision
   stokehold already made on other grounds (isolation, a rebuild per PHP version) rather than to be
   beaten. Carries two stated confounds: the bundled PHP is 8.5.11 against this host's 8.3.35.

Hazard 46's open product decision — whether the worker transport should default to `socket` on
Windows — is **answered** in hazard 47: yes, by platform, with a startup hint for the one worker
shape it breaks. What remains genuinely open from that work is only the
`stokehold-competition` lock question below.

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
| TLS / HTTPS | **does not work** | a TLS handshake to the HTTP port fails; there is **no** `rustls`, `openssl` or `native-tls` dependency in any `Cargo.toml`, and no `tls`/`cert` key in the config model |
| HTTP/3 | **does not exist** | needs `quinn` + `h3`; step 24 |
| FastCGI front-end | **works** | `crates/stokehold-server/src/frontend/`, wired into `serve`, 5 tests in `tests/frontend.rs` |
| `/index.php` is executed, not disclosed | **works** | the body is the script's output, not its source |
| static: `ETag` → 304 | **works** | the `etag` header echoed back as `If-None-Match` answers 304 |
| static: `If-Modified-Since` → 304 | **works** | 304 with the file's own `last-modified` |
| static: `Range` → 206, unsatifiable → 416 | **works** | `bytes=0-4` gives `206` and `stati`; `bytes=99-` gives 416 |
| static: `If-Range` | **works** | a matching validator gives 206, a stale one gives the full 200 — which is what RFC 9110 asks for |
| static: multi-range | **works** | `bytes=0-2,4-6` answers 206 `multipart/byteranges; boundary=stokehold-boundary` |
| static: dotfile denial, directory redirect | **works** | `/.env` → 404; `/docs` → 301 to `/docs/`, then the index |
| static: no traversal escape | **works** | `/../secret.txt`, `/..%2fsecret.txt`, `/%2e%2e/secret.txt` and `/....//secret.txt` all fail to reach a file outside the document root — the encoded ones are treated as *names that do not exist*, not decoded into segments |

**The one surprising row is the `Upgrade` failure, and the mechanism is ours, not `hyper`'s.**
`hyper-util`'s `auto::Builder` handles the upgrade case only through
`serve_connection_with_upgrades`; [`listener`](../crates/stokehold-http/src/listener.rs) calls
plain `serve_connection`, so the offer is never honoured. The crate doc at
`crates/stokehold-http/src/lib.rs` used to claim `auto` negotiates "the prior-knowledge and
`Upgrade` cases" — the prior-knowledge half is true and measured, the `Upgrade` half is not, and
the doc now says so. Prior knowledge is what a real reverse proxy uses for h2c, so the practical
loss is small; a browser-facing listener would carry HTTP/2 over ALPN instead, which arrives with
step 24.

**There is no HTTP/2 test anywhere in the repository.** `stokehold version` has printed
"HTTP/2 (cleartext)" since step 19, and the evidence for it is the probe above rather than a
test — so a `hyper` upgrade that changed the negotiation would be caught by nothing. A test that
sends the preface and asserts a SETTINGS frame comes back (and one asserting the upgrade offer
*stays* HTTP/1.1, so the gap is a decision rather than an accident) is the obvious small
addition; it is not in this session's scope.

### The published Windows latency claim **is** now committed

Hazard 46's 21× figure (pipe 15.24 ms against socket 0.72 ms median on `/items`, 9 runs of 1000
requests each, ranges disjoint) survives in this plan and in the competition README, and as of
hazard 48 the raw rows behind it **are** committed — as
`stokehold-competition/results/raw-windows-279dc35.tsv`, 70 rows (`P` = socket, `Pp` = pipe), plus
`results/raw-windows-fcgi-279dc35.tsv` for hazard 49's 27 interleaved rows. Both match the
`!results/raw-*.tsv` whitelist in that repo's `.gitignore`. Earlier text here predicted the
filename would be `results/raw-windows.tsv` and called the claim uncommitted; this paragraph
replaces it. The 21× and the 11× can be re-derived from the repository rather than taken on trust.

### What the first real Linux run found

`STOKEHOLD_E2E=1 cargo test --workspace` had never been run on Linux with a real PHP
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
`STOKEHOLD_E2E=1` run on the *same* tree was still passing when the harness was written,
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

  **Implemented** as `socket = '\\.\pipe\stokehold-cgi'` on a `spawn_cgi` backend, which
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
  loopback listener before spawning, passes the address as `STOKEHOLD_WORKER_SOCKET`, and the
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
    a plain checkout without the change, and again in `stokehold-competition`, whose `vendor/`
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
by the *server* and announced to the worker in `STOKEHOLD_WORKER_SOCKET`. A worker written
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

**A named pipe does not escape it either — measured.** Since stokehold already speaks FastCGI
over a named pipe on Windows (`stokehold-fcgi/src/pipe.rs`), the obvious follow-up question was
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
`GET /items`, 12 workers, both arms behind the same `stokehold` binary at `279dc35`,
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
  `stokehold-competition` (`P` = socket, `Pp` = pipe), which closes hazard 46's
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
  at `stokehold=debug` (a fresh socket pool, 12 workers, **~94,700 requests** across three
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
at `91cc7b6`, both about evidence rather than about stokehold's code. Neither is a defect in the
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
| P | stokehold worker — **the arm the commits changed** | **+0.35%** | 2/10 |
| T | stokehold `spawn_cgi` | −1.54% | 8/10 |
| U | stokehold `spawn_cgi` unix | −1.77% | 8/10 |
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
explicit: `pkill` the launcher, then `pkill -f 'stokehold serve'` and let each server's `Drop`
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

`rr` is the control and the reason the result is legible: a round in which every stokehold variant
moved but `rr` did not is a round in which stokehold moved. Seven rounds × 2 endpoints × 3
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
stdout, stokehold reads the first byte `a` (97) as a protocol version, and the pool dies with
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
the sequence is not monotonic. Per-request CPU agrees: stokehold's server cost is **flat at
121–126 µs across c=8..32** and RoadRunner's moves 9 µs, so if 16 were a threshold stokehold's
column would bend there and nothing bends. Attributing that cell to slot exhaustion would have
been a story the data does not support, and the lesson generalises — at ~1% a single concurrency
level is not a meaningful unit of evidence.

*What the denser sweep did find is a tail divergence with no counterpart in throughput.* Median
p99 on `/items`, RoadRunner against stokehold:

    c=12  1,760 µs  vs  1,640 µs   (0.93×)
    c=14  1,930 µs  vs  2,195 µs   (1.14×)
    c=16  2,105 µs  vs  3,470 µs   (1.65×)
    c=18  2,275 µs  vs  5,255 µs   (2.31×)
    c=20  2,415 µs  vs  6,770 µs   (2.80×)
    c=24  2,690 µs  vs 10,425 µs   (3.88×)

RoadRunner's tail is **flat** above its slot count, which is what a pool that holds a queue looks
like. Stokehold's grows without bound, and its p50 is *better* than RoadRunner's at every one of
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
`crc32c` fix's +10–16%, which was called "the whole lead" over RoadRunner (the fix put stokehold
4–7% ahead; this puts it 27–30% ahead). The one effect measured here that is larger is not
stokehold's: removing xdebug from the host's CLI PHP doubled the warm arms, which was a
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

`stokehold-competition` gained a sixth arm, and it is the only one that can test the project's
**first** constraint rather than its tuning. That constraint is *no PHP embedding, no FFI —
`php`/`php-cgi`/`php-fpm` are always spawned as external processes*. FrankenPHP is the
counter-proposal: a Caddy server with a ZTS PHP built into the binary, twelve worker threads in
one process, framework booted once. Same `BenchApp`, same page, same worker count,
`max_requests`/`max_jobs`/`pm.max_requests` all 0, and the same ini reached through `php_ini`.

Interleaved, 7 rounds, order rotated, `/items`, 12 workers (`scripts/ab-perf.sh`):

    conc   FrankenPHP   stokehold   RoadRunner     franken/rr    franken/stokehold
      1      2,441        2,388        1,773         1.381x        1.018x  [0.969..1.054]
      8     14,337       15,538       11,131         1.288x        0.921x
     16     15,140       19,984       17,554         0.876x        0.759x
     32     15,823       23,250       18,164         0.867x        0.677x

Every ratio held the same way in all 7 rounds except the one at concurrency 1, whose spread
spans 1.00 — so **FrankenPHP and stokehold are indistinguishable when one request is in
flight, and FrankenPHP is the slower of the three from concurrency 8 upward.** What survives
is the *shape*: the arm with the interpreter inside its process does not convert concurrency
into throughput, gaining 10.4% from 8 to 32 where stokehold gains 49.6%. On this page, in the
range a server is actually for, the no-embedding rule was a cost rather than a saving — which
is worth stating plainly, because the constraint was argued on other grounds (isolation, a
rebuild per PHP version) and this is not evidence against those.

It is **not** that the embedded interpreter costs more per request. Twelve seconds at
concurrency 16, `utime + stime` summed over each whole process tree out of `/proc/<pid>/stat`
— the integral across the window, not a `top` snapshot:

    server      pids   server CPU-s   cores used   rps
    FrankenPHP     2       85.99         7.17       15,262
    RoadRunner    13      105.90         8.83       17,794
    stokehold     13       97.18         8.10       20,204

FrankenPHP serves the fewest requests per second **and** leaves the most cores idle: 7.17 of
12, against stokehold's 8.10 at 33% higher throughput. Capacity is sitting unused while its
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




