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

> **Status: early.** The workspace, configuration model and PHP discovery are implemented. The
> FastCGI codec, the supervisor and the listeners are next — see [docs/plan.md](docs/plan.md)
> for the phase order.

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

## Layout

| Path | Contents |
| --- | --- |
| `crates/stokehold-core` | Configuration model, backend types, PHP discovery |
| `crates/stokehold-fcgi` | FastCGI record codec and client |
| `crates/stokehold-cli` | The `stokehold` binary |
| `docs/plan.md` | The design and the phase plan |
| `sdk/php` | The PHP worker SDK (later phase) |

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

## License

MIT — see [LICENSE](LICENSE).
