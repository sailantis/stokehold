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
> client, CGI environment, `php-cgi` pool, backend dispatch and the FastCGI front-end are all
> implemented, with integration tests driving real `php-cgi` processes end to end. The worker
> protocol is specified in [docs/worker-protocol.md](docs/worker-protocol.md), implemented in
> `crates/stokehold-worker`, and the PHP worker SDK in [sdk/php](sdk/php) implements the other
> end — proven by tests that drive a live `php worker.php` over pipes. The HTTP listener and the
> worker pool that supervises those workers are next — see [docs/plan.md](docs/plan.md) for the
> phase order.

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
by a browser and spoken to by nginx at the same time.

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

`stokehold` prints the address it bound, then serves FastCGI until Ctrl-C. Point nginx at it:

```nginx
location ~ \.php$ {
    fastcgi_pass 127.0.0.1:9080;
    include fastcgi_params;
    fastcgi_param SCRIPT_FILENAME $document_root$fastcgi_script_name;
}
```

Pass an empty `--front-controller` to serve only real `.php` files, which matches nginx's
`location ~ \.php$` behaviour; the default resolves everything else through `index.php` with
`PATH_INFO`, which is what most frameworks want.

`stokehold` answers nginx's `GET_VALUES` health probe, so it stays in rotation rather than being
marked down.

## Layout

| Path | Contents |
| --- | --- |
| `crates/stokehold-core` | Configuration model, backend types, PHP discovery |
| `crates/stokehold-fcgi` | FastCGI record codec, client, transport and CGI environment |
| `crates/stokehold-worker` | The worker protocol: framed codec and message set over a worker's pipes |
| `crates/stokehold-server` | Backend abstraction, `php-cgi` pool, process lifecycle, dispatch |
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

The PHP SDK's own suite needs no Rust and no PHPUnit:

```bash
php sdk/php/tests/run.php
```

## License

MIT — see [LICENSE](LICENSE).
