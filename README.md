# stokehold

A cross-platform PHP application server.

`stokehold` runs PHP applications in two modes from a single Rust binary:

- **Worker mode** — a persistent PHP process that boots your application once and serves many
  requests.
- **FastCGI mode** — full per-request state reset, the way `php-fpm` behaves.

It works on Linux, macOS and Windows, and it never embeds PHP: `php`, `php-cgi` and `php-fpm`
are spawned as ordinary external processes. That matters most on **Windows**, which has no
`php-fpm` at all — `php-cgi` is the only FastCGI-capable PHP binary there, and nothing
supervises it for you.

> **Status: early but working.** The configuration model, PHP discovery, FastCGI codec and
> client, CGI environment, `php-cgi` pool, backend dispatch and both front-ends are all
> implemented, with integration tests driving real `php-cgi` processes end to end. The worker
> protocol is specified in [docs/worker-protocol.md](docs/worker-protocol.md), implemented in
> `crates/stokehold-worker`, and the PHP worker SDK in [sdk/php](sdk/php) implements the other
> end — proven by tests that drive a live `php worker.php` over pipes. The worker pool that
> supervises those workers is implemented in `crates/stokehold-server`, so `kind = "worker"` now
> serves for real: `stokehold serve` boots the pool, handshakes each worker, and serves through it.
> The HTTP front-end is implemented in `crates/stokehold-http` — a `hyper` listener speaking
> HTTP/1.1 and HTTP/2 over the same dispatcher as the FastCGI one — and it serves static files
> from the document root, so a single `stokehold` process can front a site the way nginx plus
> `php-fpm` would. TLS and HTTP/3 are not there yet; see [docs/plan.md](docs/plan.md) for the
> phase order.
>
> **The async boundary.** `stokehold-http` is the only place with a `tokio` runtime, because
> `hyper` needs one. Every backend stays blocking, and the dispatcher is reached only through
> `spawn_blocking` — see [the crate docs](crates/stokehold-http/src/lib.rs) for why.

## Why

Two problems that `php-fpm` alone does not solve:

1. **Windows has no `php-fpm`.** `php-cgi -b` provides real per-request SAPI reset —
   superglobals, POST bodies, multipart and `$_FILES`, `is_uploaded_file()` — but nothing
   spawns, supervises, health-checks or recycles it. That is the gap this project fills.
2. **Per-request reset is expensive.** Booting a framework on every request costs real time.
   Worker mode boots once and stays resident.

Both are served by one dispatcher, so a route can pick whichever semantics it needs.

## Design in one picture

```
                        ┌──────────────────────────────────────────┐
 browser ──HTTP───────► │ HTTP listener (hyper)                    │
                        │                                          │──► worker pool   (persistent PHP, pipes)
 nginx  ──FastCGI─────► │ FastCGI listener                         │──► php-fpm        (external socket / TCP)
                        │  └► CGI env builder ► dispatcher         │──► php-cgi        (N processes, N ports)
                        └──────────────────────────────────────────┘
```

One binary. No second application is needed to try it — `stokehold serve` can be hit directly
by a browser and spoken to by nginx at the same time, and it serves the site's static files
itself.

## Backends

| `kind` | What it is | Per-request reset | Platforms |
| --- | --- | --- | --- |
| `worker` | Persistent PHP process speaking stokehold's framed protocol over pipes | No — the application clears its own request scope | all |
| `external` | An already-running `php-fpm` (systemd-managed) | Yes, by php-fpm's SAPI | Linux, macOS |
| `spawn_fpm` | `stokehold` starts `php-fpm`, which still manages its own children | Yes | Linux, macOS |
| `spawn_cgi` | N `php-cgi` processes on N ports | Yes, by php-cgi's SAPI | all, **required on Windows** |

`spawn_cgi` is implemented: the pool starts the processes, assigns ports, health-checks each
slot, restarts one that died, and recycles on `max_requests` — the equivalent of php-fpm's
`pm.max_requests`, which does not exist on Windows. On Unix the processes are placed in their
own process group; on Windows they are adopted into a job object with
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so nothing is left running if `stokehold` dies.

`worker` is implemented too, and it is the interesting case. Its `command` is a command line
(`php worker.php`), split without a shell — double quotes group, and a backslash is kept unless it
escapes a quote or another backslash, so a Windows path needs no doubling. Each worker is started,
handshaken, and kept resident; `stderr` is drained on its own thread so a chatty application
cannot fill a pipe and block; and a worker is replaced when it asks to be (`keep_alive = false`),
when it reaches `max_requests`, or when the peak RSS it reports exceeds `memory_limit_mb`. Every
wait has a deadline, enforced by killing the worker — the process *is* the cancellation token,
because a pipe read has no portable timeout. A failed request is deliberately **not** retried,
unlike the FastCGI backends: a worker failure can happen after the application ran.

```toml
default_backend = "app"

[server]
http = "127.0.0.1:8080"
fastcgi = "unix:/run/stokehold.sock"

[backends.app]
kind = "worker"
command = "php worker.php"
memory_limit_mb = 256

[backends.fpm]
kind = "external"
socket = "unix:/run/php/php8.3-fpm.sock"

[backends.cgi]
kind = "spawn_cgi"
binary = "C:/php-8.3.31/php-cgi.exe"
workers = 8
```

Paths with backslashes are easiest as TOML **literal strings** (`'C:\php-8.3.31\php-cgi.exe'`):
in a `"…"` basic string a backslash starts an escape sequence, so `C:\Users` is a parse error
rather than a path.

See [stokehold.example.toml](stokehold.example.toml) for a commented version of the whole
configuration surface.

## Getting started

Requires Rust 1.75+.

```bash
cargo build --release
```

Validate a configuration and see what `stokehold` found on this host:

```bash
cargo run -- check --config stokehold.example.toml
```

That prints the configured listeners, each backend and whether it resets per request, and every
PHP binary discovered (role, version, ZTS/NTS flavour and path). Extra search directories can be
added with `--php-dir <dir>`.

Then serve it:

```bash
cargo run -- serve --config stokehold.example.toml \
  --document-root /srv/app --front-controller index.php
```

`stokehold` prints both addresses it bound, then serves HTTP and FastCGI side by side until
Ctrl-C. Open `http://127.0.0.1:8080/` in a browser, or point nginx at the FastCGI listener:

```nginx
location ~ \.php$ {
    fastcgi_pass 127.0.0.1:9080;
    include fastcgi_params;
    fastcgi_param SCRIPT_FILENAME $document_root$fastcgi_script_name;
}
```

Set `server.http` or `server.fastcgi` to run only one of them. Both feed the same dispatcher, so
a request gets the same backend whichever protocol carried it.

Pass an empty `--front-controller` to serve only real `.php` files, which matches nginx's
`location ~ \.php$` behaviour; the default resolves everything else through `index.php` with
`PATH_INFO`, which is what most frameworks want.

`stokehold` answers nginx's `GET_VALUES` health probe, so it stays in rotation rather than being
marked down.

Failures map to the status the client needs: **404** for a path that resolves to no script,
**403** for a forbidden extension, **413** for an oversized request body, **502** for a backend
that cannot be reached, **504** for one that does not answer in time, and **503** when no backend
slot is free.

## Routing

A route selects a backend, so one process can serve a worker-backed API and a php-fpm-backed
admin area at once:

```toml
default_backend = "app"

[[routes]]
path_prefix = "/api"        # a textual prefix, like nginx's `location`
backend = "app"

[[routes]]
host = "admin.example.com"  # a bare host name: no port, case-insensitive
path_prefix = "/"
backend = "fpm"
```

The **most specific** matching route wins — a route stating both a host and a path beats one
stating only a path — and ties go to the earlier entry. That is deliberately the whole rule: a
table whose winner depended on something cleverer could not be predicted by reading the file.

## Static files

Static serving is a routing layer of its own, and it is **off by default**: a request that used to
reach the front controller still does, so upgrading does not change an existing deployment.

```toml
[server.static_files]
enabled = true
```

A request is then answered from disk when the path names a real file under the document root, and
goes to PHP when it does not. `/assets/app.css` is a file; `/about` is not, so the front controller
still sees it. That one sentence is the whole rule.

With it enabled, `stokehold` serves:

| Feature | Detail |
| --- | --- |
| Content types | A table of what a web build contains; anything unknown is `application/octet-stream` |
| Validators | `ETag` (length + mtime) and `Last-Modified`, with 304s for `If-None-Match` / `If-Modified-Since` |
| Ranges | `bytes=0-99`, `bytes=100-`, `bytes=-500`, several at once, 416 for a range past the end, `If-Range` |
| Precompressed | `file.br` then `file.gz` when the client asked — opt-in, and only when the file exists |
| Directory indexes | `index.html`, with a 301 from `/docs` to `/docs/` |
| Caching | One `Cache-Control` for everything, because a per-extension guess goes stale silently |
| Refusals | Dotfiles answer 404, and a `..` segment answers 403 rather than being clamped |

Serving a file costs no PHP worker and no `spawn_blocking`: path resolution is one blocking
`stat`, and the bytes stream through `tokio::fs` in 64 KiB blocks with a *content-length* frame, so
hyper frames the response without chunked encoding. Zero-copy (`sendfile`) is *not* used — that
would be a Unix-only second code path — so a large file is copied per block. See
[the module docs](crates/stokehold-http/src/static_files.rs) for what is deliberately absent.

## Layout

| Path | Contents |
| --- | --- |
| `crates/stokehold-core` | Configuration model, backend types, PHP discovery |
| `crates/stokehold-fcgi` | FastCGI record codec, client, transport and CGI environment |
| `crates/stokehold-worker` | The worker protocol: framed codec and message set over a worker's pipes |
| `crates/stokehold-server` | Backend abstraction, `php-cgi` and worker pools, process lifecycle, dispatch |
| `crates/stokehold-http` | The HTTP front-end: a `hyper` listener and the static file layer — the only async code in the project |
| `crates/stokehold-cli` | The `stokehold` binary |
| `sdk/php` | The PHP worker SDK — the other end of the worker protocol |
| `docs/plan.md` | The design and the phase plan |
| `docs/worker-protocol.md` | The worker wire protocol — the specification the PHP SDK implements |

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

The FastCGI integration tests in `crates/stokehold-fcgi/tests/php_cgi.rs` start a real
`php-cgi -b` process and speak the protocol to it. They skip themselves, with a printed reason,
when no `php-cgi` binary is on `PATH`.

The worker protocol tests — `crates/stokehold-worker/src/codec.rs` and
`crates/stokehold-worker/tests/wire_format.rs` — need no PHP. They drive the codec over an
in-memory stream and assert the exact wire bytes, so the protocol stays frozen without depending
on a PHP install.

`crates/stokehold-worker/tests/php_worker.rs` does the opposite: it starts a real
`sdk/php/example/worker.php` process and speaks the protocol to it over pipes, so the Rust codec
and the PHP SDK are checked against each other rather than only against their own fixtures.

`crates/stokehold-server/tests/worker_pool.rs` tests the pool around that protocol against real
worker processes: it boots a pool, serves through every worker in rotation, replaces a worker that
was killed or asked to be recycled, recycles on `max_requests` and on `memory_limit_mb`, and
checks that dropping the pool leaves no processes behind. It skips itself when no `php` is on
`PATH`.

`crates/stokehold-http/tests/frontend.rs` drives the HTTP listener over a real socket against a
real worker pool: a GET, a POST body verified byte for byte, request headers landing in the CGI
environment, an application error becoming a 500 while the worker survives, `Content-Length`
agreeing with the body that was sent, keep-alive serving two requests on one connection, four
concurrent requests, and the error mappings that do not need a backend (404, 413, 502).

The static tests in the same file need no PHP at all, which is most of the point: they start a
listener over a backend nothing can reach, so a request answered correctly can only have been
answered by the file server. Serving a file, a 304 for an unchanged one, a 206 range with exactly
the requested bytes, a 416 past the end, the `/assets` → `/assets/` redirect, a dotfile, a
traversal, a precompressed variant, a 256 KiB file arriving whole — and, crucially, **502 for
`/about`**, which is how the tests prove a path that names no file still reaches the application.

`crates/stokehold-cli/tests/serve.rs` is the end-to-end one: it runs the shipped `stokehold`
binary with both listeners configured on port 0, then drives it over HTTP *and* over FastCGI in
the same process, which is what proves the two front-ends really do share one dispatcher. A second
test in that file serves a static file and reaches PHP for a route through the same config and the
same binary.

The PHP SDK's own suite needs no Rust and no PHPUnit:

```bash
php sdk/php/tests/run.php
```

## License

MIT — see [LICENSE](LICENSE).
