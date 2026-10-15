# volucer

A cross-platform PHP application server.

`volucer` runs PHP applications in two modes from a single Rust binary:

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
> `crates/volucer-worker`, and the PHP worker SDK implements the other end — it lives in its own
> repository, [`sailantis/volucer-php-sdk`](https://github.com/sailantis/volucer-php-sdk),
> because a Composer package needs `composer.json` at the root of a repository. It is proven by
> tests that drive a live `php worker.php` over pipes. The worker pool that
> supervises those workers is implemented in `crates/volucer-server`, so `kind = "worker"` now
> serves for real: `volucer serve` boots the pool, handshakes each worker, and serves through it.
> The HTTP front-end is implemented in `crates/volucer-http` — a `hyper` listener speaking
> HTTP/1.1 and HTTP/2 over the same dispatcher as the FastCGI one — and it serves static files
> from the document root, so a single `volucer` process can front a site the way nginx plus
> `php-fpm` would. The CLI is complete: `serve`, `check` and `version`, with the document root and
> front controller configurable in the file rather than only on a command line. TLS and HTTP/3 are
> not there yet; see [docs/plan.md](docs/plan.md) for the phase order.
>
> **The async boundary.** `volucer-http` is the only place with a `tokio` runtime, because
> `hyper` needs one. Every backend stays blocking, and the dispatcher is reached only through
> `spawn_blocking` — see [the crate docs](crates/volucer-http/src/lib.rs) for why.

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
                        │   └► Host ► site (root, controller)      │──► worker pool   (persistent PHP, pipes)
 nginx  ──FastCGI─────► │ FastCGI listener                         │──► php-fpm        (external socket / TCP)
                        │  └► CGI env builder ► dispatcher         │──► php-cgi        (N processes, N ports)
                        └──────────────────────────────────────────┘
```

One binary. No second application is needed to try it — `volucer serve` can be hit directly
by a browser and spoken to by nginx at the same time, and it serves the site's static files
itself. Several `Host` names can be served from one process, each with its own document root; see
[Sites](#sites).

## Backends

| `kind` | What it is | Per-request reset | Platforms |
| --- | --- | --- | --- |
| `worker` | Persistent PHP process speaking volucer's framed protocol over pipes | No — the application clears its own request scope | all |
| `external` | An already-running `php-fpm` (systemd-managed) | Yes, by php-fpm's SAPI | Linux, macOS |
| `spawn_cgi` | N `php-cgi` processes on N ports | Yes, by php-cgi's SAPI | all, **required on Windows** |

A fourth kind, `spawn_fpm`, was accepted by the parser and refused at startup — `version`
advertised it while `Backend::from_config` rejected it. It is **gone** rather than implemented:
php-fpm already does pool management, `pm.max_requests` recycling, multi-pool tenancy and graceful
reload better than a supervisor we would write, and `external` attaches to the php-fpm that
systemd already runs. A file still carrying `kind = "spawn_fpm"` is now rejected by name.

`spawn_cgi` is implemented: the pool starts the processes, assigns ports, health-checks each
slot, restarts one that died, and recycles on `max_requests` — the equivalent of php-fpm's
`pm.max_requests`, which does not exist on Windows. On Unix the processes are placed in their
own process group; on Windows they are adopted into a job object with
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so nothing is left running if `volucer` dies.

One `spawn_cgi` slot is **one process on one port**, and volucer enforces that by setting
`PHP_FCGI_CHILDREN=0` for each child. `php-cgi -b` is otherwise not one process: it spawns
`PHP_FCGI_CHILDREN` children that all share the single bound socket, and the count is read from its
inherited environment — so a host that exports it for `php-fpm` would silently give every slot 20
hidden workers, making `workers` a lie and `max_requests` unaccountable. `PHP_FCGI_MAX_REQUESTS=0`
goes with it, because php-cgi's own default is to retire a child after 500 requests; recycling here
is the pool's decision, so `max_requests` means the same thing for every backend.

`worker` is implemented too, and it is the interesting case. Its `command` is a command line
(`php worker.php`), split without a shell — double quotes group, and a backslash is kept unless it
escapes a quote or another backslash, so a Windows path needs no doubling. Each worker is started,
handshaken, and kept resident; `stderr` is drained on its own thread so a chatty application
cannot fill a pipe and block; and a worker is replaced when it asks to be (`keep_alive = false`),
when it reaches `max_requests`, or when the peak RSS it reports exceeds `memory_limit_mb`. Every
wait has a deadline, enforced by killing the worker — the process *is* the cancellation token,
because a pipe read has no portable timeout. A failed request is deliberately **not** retried,
unlike the FastCGI backends: a worker failure can happen after the application ran.

`checksum = "optional"` turns off the protocol's per-frame CRC-32C. The default is `"required"`,
which is what every earlier release did, so omitting the line changes nothing. When it is
`"optional"` the server *offers* the option in its `HELLO`, and only stops verifying if the worker
answers in its `HELLO_ACK` that it agrees — an SDK that predates the field omits it, which decodes
as "no", and the pair stays checked. The handshake itself is always checksummed, in both
directions, because it is the exchange that carries the agreement. Setting it is worth about what
one `memcpy` of the page costs per frame, which on a request that spends its time inside the
application is close to nothing; it is a knob to measure with, not a promised speed-up. See
[`docs/worker-protocol.md`](docs/worker-protocol.md) for the wire details and the flag.

`transport = "socket"` moves the worker protocol off the child's stdio and onto a loopback socket.
It exists for one measured reason, and it is a large one: **on Windows, PHP takes ~8 ms median
(15 ms at p95) to return from a blocking read on a pipe, where the same read on a socket returns in
~0.09 ms.** A Rust process reading the identical pipe returns in 0.010 ms, and RoadRunner measured
the same 15 ms on the same host, so this is PHP's Windows stream layer rather than anything
volucer does — but it is per request, not per connection, so it decides which deployment model
wins: on the whole `/items` route it was **21×** (0.72 ms against 15.24 ms median).

**The default is therefore a socket on Windows and a pipe everywhere else.** A configuration that
never mentions the setting gets the one its platform is fast on, and the choice is logged at
startup (`worker pool started … transport=socket`) because the transport is invisible on the wire
— a pool on the wrong one answers every request with a `200`, only slower. Set `transport = "pipe"`
explicitly to opt out.

The worker opts in with `Volucer\Worker::connect()` (an SDK at or after the release that added
it) and needs no other change, because the protocol does not know which transport it is on. A
worker that still calls `Worker::create()` cannot read the socket, so it is refused at startup
with a message naming the fix — the failure is a startup error rather than a mystery on the first
request. See [`docs/plan.md`](docs/plan.md) hazard 46 for the measurements and the alternatives
that were ruled out.

```toml
default_backend = "app"

[server]
http = "127.0.0.1:8080"
fastcgi = "unix:/run/volucer.sock"

[backends.app]
kind = "worker"
command = "php worker.php"
memory_limit_mb = 256
checksum = "optional"
# Defaults to "socket" on Windows, "pipe" elsewhere. Set "pipe" to opt out.
# transport = "pipe"

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

A `spawn_cgi` pool talks to its `php-cgi` processes over loopback TCP by default. `socket` moves
it off the loopback stack, and what that setting means depends on the platform: a unix socket
path on Linux and macOS, and a **named pipe** on Windows, because that is what `php-cgi -b` binds
there. Both are a *prefix* — each slot appends its index, so `\\.\pipe\volucer-cgi` gives slot 0
the pipe `\\.\pipe\volucer-cgi.0`. The pipe is the faster of the two transports on Windows: a
4 KiB request measures ~168 µs over a pipe against ~211 µs over loopback TCP.

```toml
# Linux and macOS
socket = "unix:/run/volucer/cgi"

# Windows
socket = '\\.\pipe\volucer-cgi'
```

See [volucer.example.toml](volucer.example.toml) for a commented version of the whole
configuration surface.

## Getting started

Requires Rust 1.85+.

```bash
cargo build --release
```

Validate a configuration and see what `volucer` found on this host:

```bash
cargo run -- check --config volucer.example.toml
```

That prints the configured listeners, the document root and front controller it resolved (and
whether they are actually there), each backend and whether it resets per request, the static-file
settings, and every PHP binary discovered (role, version, ZTS/NTS flavour and path) — flagging any
that are below the worker floor. Extra search directories can be added with `--php-dir <dir>`.

It also checks that each configured program can actually be started: a `spawn_cgi` `binary` or the
first token of a worker's `command` that names nothing on disk is reported, which is the mistake a
configuration file is most likely to make and the one `Config::validate` cannot see, because it is
about the shape of the file rather than the contents of the disk.

Problems it can see are reported as **warnings**, and the exit status stays 0: a document root that
does not exist yet is a normal thing to check before a deploy creates it. What the warnings buy is
that a mistake is reported once, here, instead of as a puzzling 404 at the first request.

`volucer version` prints what the build is, including the worker protocol version and the PHP
floor it supports.

Then serve it:

```bash
cargo run -- serve --config volucer.example.toml
```

`volucer` prints both addresses it bound, then serves HTTP and FastCGI side by side until
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

## Where the application lives

Which directory PHP scripts resolve against is a deployment fact, so it belongs in the file rather
than on a command line that a systemd unit or a container image has to carry:

```toml
[server]
document_root = "/srv/app/public"
front_controller = "index.php"
```

Both keys are also `serve` flags, and **a flag wins over the file** — the more specific statement
should be the one that takes effect.

`front_controller` is what keeps a framework working: a path that names no file under the document
root is given to this script with `PATH_INFO` set, instead of answering 404. Pass an empty
`--front-controller ''` to serve only real `.php` files, which matches nginx's `location ~ \.php$`
behaviour. That form is a *flag* and not a config value, deliberately: TOML cannot tell an empty
string from an absent key, so writing `front_controller = ""` in the file is rejected with a
message saying so rather than being given a meaning of its own.

`volucer check` prints the document root and front controller it resolved and whether each is
actually present, which is the fastest way to see what a running deployment uses.

`volucer` answers nginx's `GET_VALUES` health probe, so it stays in rotation rather than being
marked down.

Failures map to the status the client needs: **404** for a path that resolves to no script,
**403** for a forbidden extension, **413** for an oversized request body, **502** for a backend
that cannot be reached, **504** for one that does not answer in time, and **503** when no backend
slot is free.

## Routing

A route selects a backend, so one process can serve a worker-backed API and a php-cgi-backed
legacy area at once:

```toml
default_backend = "app"

[[routes]]
path_prefix = "/api"        # a textual prefix, like nginx's `location`
backend = "app"

[[routes]]
path_suffix = ".php"        # every PHP entry point, wherever it lives
backend = "cgi"

[[routes]]
host = "admin.example.com"  # a bare name, or `*.example.com`; no port, case-insensitive
path_prefix = "/"
backend = "fpm"
```

The **most specific** matching route wins, and ties go to the earlier entry. "Most specific" is
three sentences rather than a weight, because a table whose winner could not be predicted by
reading the file would be worse than a simple one:

1. a route stating **more** constraints beats one stating fewer;
2. at the same count, an **exact** `host` beats a `*.` wildcard;
3. anything still tied goes to the route declared **first**.

`path_suffix` is compared **case-insensitively**, deliberately: the router decides what a script
is the same way, so `/upload.PHP` cannot be a script to one layer and miss its route in the other.
It must start with `.` or `/` — `"php"` would match `/notphp`, which is not what it looks like it
does, so it is refused.

### Several PHP entry points

`.php` files are resolved to real scripts, so `path_suffix = ".php"` sends *every* PHP entry point
to one backend — the usual answer for a project with more than one front controller:

```toml
[[routes]]
path_prefix = "/api"                # a framework API on the persistent worker pool
backend = "app"

[[routes]]
path_suffix = ".php"                # each real script on the CGI pool
backend = "cgi"
```

A **worker** backend is told each request's `script_filename` but does not switch file on it: it
routes on `REQUEST_URI`. So an entry script reached through a worker gets its real path in
`$_SERVER`, and the worker's own bootstrap decides what to run — correct for one application per
site, and wrong for several independent scripts that each need their own bootstrap. Use
`spawn_cgi` for those.

Because a route's `path_suffix` can prove that a path names a script, such a route **skips the
router's own `stat`**: `php-cgi` checks existence itself and answers `No input file specified`. A
route that proves nothing about the extension — a bare `path_prefix`, or a path to `php-cgi` whose
pattern could name `config.json` — keeps the check, because `php-cgi` does not verify that a file
is PHP: given a non-PHP file it emits it verbatim with a 200. `volucer check` prints which routes
skip it, so the saving and the guard are both visible.

## Sites

One process can serve several hosts, each with its own document root:

```toml
[server]
http             = "127.0.0.1:8080"
document_root    = "/srv/default/public"   # the fallback site
front_controller = "index.php"

[backends.app]
kind    = "worker"
command = "php worker.php"

[backends.cgi]
kind    = "spawn_cgi"
binary  = 'C:\php-8.3.31\php-cgi.exe'

[sites.example_com]
hosts            = ["example.com", "www.example.com", "*.shop.example.com"]
document_root    = "/srv/example/public"
default_backend  = "app"

[sites.corp]
hosts            = ["corp.internal", "*.corp.internal"]
document_root    = "/srv/corp"
front_controller = false                   # files-only: no front controller
serve_only       = "backend"               # no static layer
static_files     = { precompressed = true }

  routes = [
      { path_suffix = ".php", backend = "cgi" },
  ]
```

This exists because a `host` matcher in the route table is consulted **after** the script is
resolved, so it can choose a backend but never a document root. Sites are resolved first, which is
what makes two roots possible in one process.

- `hosts` takes exact names, case-insensitively and ignoring any `:port`, or a leading `*.`
  wildcard. `*.example.com` matches `a.example.com` and `a.b.example.com`, but **not**
  `example.com` itself — a wildcard names a subdomain.
- The rule is: an exact name, then the **longest** matching wildcard, then `[server]`. A host
  nobody claims is the default site's.
- Every key except `hosts` and `document_root` falls back to the `[server]` value, so a site that
  states only a root and a host list inherits the rest. `static_files` falls back **per key**: a
  site naming `precompressed` keeps the server's `cache_control`.
- `front_controller = false` means "serve only real `.php` files". The global key cannot say this —
  TOML cannot tell `front_controller = ""` from an absent key, so that spelling stays reserved for
  the `--front-controller ''` flag — but a site can, because there is no flag per site.
- `serve_only` says what the site serves: unset serves everything (static first, then a backend),
  `"backend"` skips the static layer, `"static"` answers a path naming no file with a 404 and never
  consults a backend. `static_files = false` is exactly `serve_only = "backend"`.
- `--document-root` and `--front-controller` describe the **default** site, so combining either
  with `[sites]` is refused rather than silently applying to one site.

`volucer check` prints the site table, what each site states, and which routes skip the stat.

## Static files

Static serving is a routing layer of its own, and it is **on by default**: a site serves its own
assets unless it says otherwise.

```toml
[server.static_files]
precompressed = true
```

Turning it on only changes the answer for a path that names a real, non-script file. A path that
names nothing, or names a `.php` script, reaches the front controller exactly as before — so a
front-controller application behaves the same either way. The reason it is on rather than off is
simply that every site has assets, and making each one ask for its own was a line that said
nothing.

`static_files = false` — or `serve_only = "backend"`, which is the same state — turns it off for a
site. `static_files = true` is the short form of `enabled = true` when the rest is at its
defaults. Because static content is served over **HTTP** only, asking for it in a config with no
`server.http` listener is refused; leaving the key out is not asking, so a FastCGI-only deployment
is unaffected by the default.

**No backend is needed at all for a files-only site.** A configuration with a listener, a document
root and nothing else is valid, and `volucer serve` starts and serves it:

```toml
[server]
http = "127.0.0.1:8080"
document_root = "/srv/site"
serve_only = "static"
```

Serve it with `--front-controller ''` so a path that names no file is a 404 rather than something
handed to a script. Without that flag the default front controller (`index.php`) is still in effect,
so a path naming no file has nowhere to go and answers 500 — `volucer check` warns about exactly
this case, once, at startup.

The document root is the same `server.document_root` that routing already resolves scripts against,
so one setting describes the whole site.

A request is then answered from disk when the path names a real file under the document root, and
goes to PHP when it does not. `/assets/app.css` is a file; `/about` is not, so the front controller
still sees it. That one sentence is the whole rule.

A file PHP is meant to execute is never served from disk — `GET /index.php` runs the script, it does
not return its source. That also gives `/index.php/more/path` the `PATH_INFO` behaviour a deployment
expects.

Two rules hold whether or not static serving is enabled, because both are about what PHP is asked to
run rather than about this layer:

- **A file that is not a script is never run as one.** Only `.php`/`.php3`/`.phtml` can be the script;
  anything else falls through to the front controller as application input. Without this, `php-cgi`
  would emit a file it cannot parse verbatim — `GET /config.json` and, worse, `GET /.env`.
- **A dotfile is never given to the application**, not even as `PATH_INFO`, for the same reason. It
  answers 404 on every method.

Note that the static layer handles only `GET`, `HEAD` and `OPTIONS`: a `POST` to a path that names a
file goes to the front controller, so a framework can route it. Those two rules are what keep that
from becoming a way to read a file.

With it enabled, `volucer` serves:

| Feature | Detail |
| --- | --- |
| Content types | A table of what a web build contains; anything unknown is `application/octet-stream` |
| Validators | `ETag` (length + mtime) and `Last-Modified`, with 304s for `If-None-Match` / `If-Modified-Since` |
| Ranges | `bytes=0-99`, `bytes=100-`, `bytes=-500`, several at once, 416 for a range past the end, `If-Range` |
| Precompressed | `file.br` then `file.gz` when the client asked — opt-in, and only when the file exists |
| Directory indexes | `index.html`, with a 301 from `/docs` to `/docs/` |
| Caching | One `Cache-Control` for everything, because a per-extension guess goes stale silently |
| Refusals | Dotfiles answer 404, and a `..` segment answers 403 rather than being clamped |
| Scripts | A `.php`/`.php3`/`.phtml` path is never served from disk, so its source cannot be disclosed |

Serving a file costs no PHP worker and no `spawn_blocking`: path resolution is one blocking
`stat`, and the bytes stream through `tokio::fs` in 64 KiB blocks with a *content-length* frame, so
hyper frames the response without chunked encoding. Zero-copy (`sendfile`) is *not* used — that
would be a Unix-only second code path — so a large file is copied per block. See
[the module docs](crates/volucer-http/src/static_files.rs) for what is deliberately absent.

## TLS

`[server.tls]` is what turns TLS on — there is no `enabled` key, because a flag and a certificate
are two statements about one thing and two statements can disagree. A table without both paths is
an error rather than a defaulted half.

```toml
[server]
http = "0.0.0.0:443"

[server.tls]
certificate = "certs/default.crt"   # PEM chain, leaf first
key = "certs/default.key"           # PEM private key
min_version = "1.2"                 # or "1.3"; a floor, not a cap
alpn = ["h2", "http/1.1"]           # h2 preferred; do NOT drop http/1.1
plain = false                       # also answer cleartext on this same port
```

A relative `certificate`/`key` resolves against **the directory holding the config file**. That is
a deliberate exception to `document_root`'s rule, which is relative to the process's working
directory: a certificate beside `volucer.toml` is the usual case, and resolving it against
wherever the process started would be a footgun with a security consequence. `volucer check`
prints the path it resolved.

### Plain HTTP and TLS together

All three shapes work. `http` always keeps its name and meaning, so no existing configuration
changes behaviour.

| Shape | Configuration |
| --- | --- |
| Cleartext only | `http = "...:8080"`, no `[server.tls]` — **unchanged from before** |
| TLS only | `http = "...:443"` + `[server.tls]` |
| **Both on one port** | `[server.tls].plain = true` |
| Both, conventional ports | `http = "...:80"` and `https = "...:443"` |

**One port carrying both** works by looking at the first byte: a TLS record always begins with
`0x16`, and a plaintext request begins an ASCII method letter. This is what nginx's `ssl_preread`
does, and it costs one `peek` per *connection* — never per request, because the protocol is decided
once and then the connection is one path or the other.

It is a **detection, not a downgrade**: a client that offers TLS and fails the handshake still
fails. There is no fallback to cleartext on a handshake error. And because accepting cleartext on a
port that clients believe is encrypted is a security decision, `plain` is off by default and is
logged loudly at startup when on.

Two mechanisms rather than one, because SMTP's analogy does not carry over: SMTP negotiates in-band
(`STARTTLS` inside the protocol, so plaintext always comes first), while HTTP has no `STARTTLS` —
RFC 2817 defined `Upgrade: TLS/1.0` and nothing adopted it. So HTTP only ever has what SMTP calls
"direct SSL", where TLS begins at byte one and the server must decide the protocol before either
side has spoken.

No port is privileged either: `https://host:8443` sends the identical ClientHello. Only two things
genuinely need a fixed port, and both are `https`'s job — **ACME HTTP-01** (the challenge server
always contacts `:80`) and an **`:80 → :443` redirect**.

```toml
# Plain on :80, TLS on :443, and redirect the former to the latter.
[server]
http = "0.0.0.0:80"
https = "0.0.0.0:443"
redirect_to_https = true

[server.tls]
certificate = "certs/default.crt"
key = "certs/default.key"
```

`redirect_to_https` is off by default because a redirect is a deployment's decision. It needs
something actually serving cleartext to fire on — a separate `https`, or `plain = true` — so
setting it with TLS on `http` alone is an error rather than a silently inert key. The redirect
answers `301`, keeps the path and query, and names the `https` port rather than the port the
request arrived on; `:443` is left out of the authority because it is the default.

**It applies to cleartext connections only.** With `plain = true` one listener carries both
protocols, and a redirect on the TLS half would send the client back to where it just came from
— an infinite loop. The decision is per connection for exactly that reason.

### Per-site certificates (SNI)
```toml
[sites.shop.tls]
certificate = "certs/shop.crt"
key = "certs/shop.key"
```

Only `certificate` and `key` can be per site. The name is claimed in the TLS handshake, before any
request exists, so a per-site table can only be about *which certificate is presented* —
`min_version`, `alpn` and `plain` describe the socket, and setting one per site is an error rather
than a key that does nothing.

Certificates are selected by **SNI**, not by the `Host` header: exact name, then the longest `*.`
wildcard, then the server's certificate, using the same matching rule as [Sites](#sites). The two
*should* agree but need not — a client may claim one name in the handshake and another in the
request. That is not an error: the certificate for the claimed name is presented, and the request
is then served by whatever site its `Host` selects.

### What TLS costs when it is off

Nothing. With no `[server.tls]` there is no peek, no branch, and no TLS value is constructed — the
cleartext path is the same code it was before TLS existed.

### Build prerequisites

`rustls` is used rather than OpenSSL, so there is no DLL/dylib to match against a system OpenSSL.
It is **not** dependency-free at build time, though: its default `aws-lc-rs` provider is C and
assembly, so it needs a **C toolchain** (MSVC on Windows), **NASM**, and **CMake**. The `ring`
provider needs only the C compiler, and is one feature line away if that ever matters:

```toml
rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12"] }
```

Rustls 0.24 moves providers into separate crates (`rustls-aws-lc-rs`, `rustls-ring`) selected at
runtime; those are still pre-release, so this targets 0.23 stable.

## Layout

| Path | Contents |
| --- | --- |
| `crates/volucer-core` | Configuration model, backend types, PHP discovery, deployment preflight checks |
| `crates/volucer-fcgi` | FastCGI record codec, client, transport and CGI environment |
| `crates/volucer-worker` | The worker protocol: framed codec and message set over a worker's pipes |
| `crates/volucer-server` | Backend abstraction, `php-cgi` and worker pools, process lifecycle, dispatch |
| `crates/volucer-http` | The HTTP front-end: a `hyper` listener and the static file layer — the only async code in the project |
| `crates/volucer` | The `volucer` binary — the crate is named `volucer` so `cargo install volucer` works |
| `docs/plan.md` | The design and the phase plan |
| `docs/worker-protocol.md` | The worker wire protocol — the specification the PHP SDK implements |

The PHP worker SDK is not in this repository. It is
[`sailantis/volucer-php-sdk`](https://github.com/sailantis/volucer-php-sdk), a Composer
package that implements the other end of the worker protocol. The tests here drive its example
worker, so a working checkout is expected beside this one — or at `VOLUCER_SDK_DIR`.

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

The FastCGI integration tests in `crates/volucer-fcgi/tests/php_cgi.rs` start a real
`php-cgi -b` process and speak the protocol to it. They skip themselves, with a printed reason,
when no `php-cgi` binary is on `PATH`.

## Releasing

The crates are not published yet. When they are, the order is **forced by the dependency graph**:
a published crate cannot depend on a version that is not on crates.io yet, so the leaves go first.

```bash
cargo publish -p volucer-core
cargo publish -p volucer-worker
cargo publish -p volucer-fcgi
cargo publish -p volucer-server
cargo publish -p volucer-http
cargo publish -p volucer        # the CLI, last: it depends on four of the above
```

Crate names are **flat** on crates.io even though the `sailantis` organization owns them — there
is no `sailantis::volucer-core`. The `volucer-` prefix is what keeps the family together, and the
CLI is the exception: its crate is named `volucer` so `cargo install volucer` works.

The internal dependencies in the workspace root carry both a `version` and a `path`, which is
required: `cargo publish` strips the `path` and resolves from crates.io, so a `path`-only
dependency is rejected with *"all dependencies must have a version requirement specified"*. All
six crates share `[workspace.package] version`, and they must be released together — a version
bump that moves one crate but not its dependents breaks the chain. When publishing a new version,
publish the leaves first and wait for the index to update before publishing the crates that depend
on them.

The worker protocol tests — `crates/volucer-worker/src/codec.rs` and
`crates/volucer-worker/tests/wire_format.rs` — need no PHP. They drive the codec over an
in-memory stream and assert the exact wire bytes, so the protocol stays frozen without depending
on a PHP install.

`crates/volucer-worker/tests/php_worker.rs` does the opposite: it starts a real
`example/worker.php` from the SDK checkout and speaks the protocol to it over pipes, so the Rust
codec and the PHP SDK are checked against each other rather than only against their own fixtures.

`crates/volucer-server/tests/worker_pool.rs` tests the pool around that protocol against real
worker processes: it boots a pool, serves through every worker in rotation, replaces a worker that
was killed or asked to be recycled, recycles on `max_requests` and on `memory_limit_mb`, and
checks that dropping the pool leaves no processes behind. It skips itself when no `php` is on
`PATH` or the SDK checkout is missing.

The two suites above are the only tests that need the SDK. libtest hides the stderr of a passing
test, so their skip message is invisible in an ordinary run — the suite reports the same result
whether the SDK was exercised or not. Where it must have been (CI), set **`VOLUCER_E2E=1`**:
a skip then fails instead of passing. `VOLUCER_SDK_DIR` points them at a checkout that is not
beside this repository.

`crates/volucer-http/tests/frontend.rs` drives the HTTP listener over a real socket against a
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

`crates/volucer/tests/serve.rs` is the end-to-end one: it runs the shipped `volucer`
binary with both listeners configured on port 0, then drives it over HTTP *and* over FastCGI in
the same process, which is what proves the two front-ends really do share one dispatcher. A second
test in that file serves a static file and reaches PHP for a route through the same config and the
same binary.

The PHP SDK's own suite needs no Rust and no PHPUnit. It runs in the SDK repository:

```bash
php tests/run.php
```

## License

MIT — see [LICENSE](LICENSE).
