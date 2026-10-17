# Plugins and additional runtimes

**What RoadRunner has, and how each piece would land in volucer.**

This document exists because the question "can volucer also do gRPC / KV / locks / jobs /
metrics, and can it run Django and Rails?" deserves an answer that is a design rather than a
shrug. It is a **plan, not a description of the code**: nothing in it is built unless a section
says so. `docs/plan.md` remains the record of what exists, and its §10 is the list to read for the
current state.

Read §1 if you want the survey, §2–§4 if you want the decisions that everything else depends on,
and §8 if you only want the order. §9 is the part that will be quoted back at us, because it says
which features are deliberately **not** the same as RoadRunner's.

---

## 1. What RoadRunner actually has

Vendored checkout: `../roadrunner` (engine v3, plugin line v6 — `roadrunner/go.mod` pins
`kv/v6`, `lock/v6`, `jobs/v6 v6.0.1`, `metrics/v6`, `grpc/v6`, `rpc/v6`, `otel/v6`).
`roadrunner/container/plugins.go` is the authoritative bundle list; each plugin is a **separate Go
module**, not vendored, so the config schema is the best local source for the real key surface:
`roadrunner/schemas/config/3.0.schema.json`, plus the annotated sample `roadrunner/.rr.yaml`.

RoadRunner is a single Go binary that loads ~40 plugins through the `endure` DI container. The
architecture that matters for us is this: **all control-plane traffic between PHP and the server
goes over one socket** — `rpc.listen` (default `tcp://127.0.0.1:6001`) — carrying **Goridge**
framing over Go's standard `net/rpc` (`roadrunner/internal/rpc/client.go:70` builds
`rpc.NewClientWithCodec(goridgeRpc.NewClientCodec(conn))`). KV, locks, jobs' producer side, the
informer (`rr workers`), the resetter (`rr reset`) and custom metrics are *all* just method calls
on that one socket. The CLI is itself a Goridge RPC client, which is why `rr workers` needs no
privileged access.

Two things are emphatically **not** on that socket: the gRPC plugin, which runs a real
gRPC/HTTP-2 server of its own (`grpc.listen`), and the metrics plugin, which exposes a real
Prometheus HTTP endpoint. Those are their own listeners.

### 1.1 Survey

| Plugin | What it is | Its transport | External dependency | Reimplementation |
| --- | --- | --- | --- | --- |
| `rpc` | The control-plane listener itself. Holds no domain logic; other plugins register method sets on it | Goridge over TCP/Unix | `goridge/v4` | **Low** — the real work is the framing codec |
| `metrics` | Embedded Prometheus registry: `/metrics` for scrapers, plus a `collect:` table PHP can push into over RPC | HTTP `/metrics` + Goridge RPC | `prometheus/client_golang` | **Low** — thin wrapper over a registry |
| `kv` | PSR-16 cache, proxied so PHP never talks to Redis itself | Goridge RPC | redis / memcached / bbolt clients | **Low** — 5 non-test Go files; the difficulty is the driver |
| `lock` | Exclusive and read (shared) distributed mutexes | Goridge RPC | go-redis v9 (Lua + Pub/Sub) | **Medium** — the Redis lock is the hard part |
| `jobs` | Priority queue with 7 broker drivers, consumer pools, ack/nack/requeue | Goridge RPC (producer + control) and the worker protocol (payload) | amqp091-go, aws-sdk-go-v2, franz-go, nats.go, … | **High** — 14 non-test files *plus* 7 driver modules |
| `grpc` | A real gRPC server that loads `.proto` at runtime and dispatches each call to a PHP worker | gRPC/HTTP-2 + the worker protocol | `google.golang.org/grpc`, `jhump/protoreflect/v2` | **Medium-high** — dynamic proto dispatch is the cost |
| `otel` | A shared tracer provider the other plugins pull spans from | none of its own | the OTel Go SDK | **Low-medium** plugin, pervasive wiring |

Also in the bundle and worth naming because they are adjacent rather than requested: `temporal`
(the largest thing in the tree), `centrifuge` (WebSockets/broadcast), `fileserver` + `static`,
`gzip` + `zstd`, `headers`, `send` (X-Sendfile), `status` (health/ready), `rate-limiter`,
`proxy_ip_parser`, `protoreg`, `informer`, `resetter`, `service` (a mini systemd for arbitrary
child processes), `app-logger`. `resetter` and `status` are effectively zero-config.

### 1.2 The config surface, verbatim

Worth having in one place, because "can volucer read my `.rr.yaml`?" is the first question anyone
asks. It cannot, and §3 says why — but these are the shapes that were studied.

```yaml
rpc:
  listen: tcp://127.0.0.1:6001        # or unix:///run/rr/rpc.sock + unix_socket.mode

kv:
  local-memory:  { driver: memory,  config: { } }
  fast-kv-fr:    { driver: redis,   config: { addrs: ["localhost:6379"], db: 0, … } }
  us-central-kv: { driver: memcached, config: { addr: ["localhost:11211"] } }
  boltdb-south:  { driver: boltdb,  config: { file: "rr.db", permissions: 0755, interval: 40 } }
  # driver ∈ {boltdb, memcached, memory, redis}; `memory` requires an empty config object

lock:                                  # zero-config by default (in-memory); ABSENT from the JSON schema
  driver: redis                        # ∈ {memory, redis}; redis needs Redis ≥ 7
  config: { addrs: ["localhost:6379"], read_timeout: 5s, tls: { … } }

metrics:
  address: "127.0.0.1:2112"            # /metrics is appended automatically
  prefix: "myapp_"                     # applied to rr_* plugin metrics only
  labels: { service: "my-microservice" }
  collect:                             # app metrics, pushed over RPC
    app_metric_histogram: { type: histogram, help: "…", labels: ["type"], buckets: [0.1, 0.2, 1.0] }

status:
  address: 127.0.0.1:2114              # GET /health?plugin=http, GET /ready
  unavailable_status_code: 503
  check_timeout: 60

jobs:
  num_pollers: 32                      # default: workers + 1
  timeout: 60
  pipeline_size: 100000                # the ONE internal priority queue, in memory only
  pool:
    command: "php my-super-app.php"    # consumers are a worker pool like any other
    num_workers: 0
    max_jobs: 0
    max_queue_size: 0
    dynamic_allocator: { max_workers: 25, spawn_rate: 10, idle_timeout: 10s }
  pipelines:
    test-local:
      driver: memory                   # ∈ {memory, amqp, sqs, kafka, beanstalk, nats, nsq, google_pub_sub, boltdb}
      config: { priority: 10, prefetch: 100000, … }

grpc:
  listen: "tcp://127.0.0.1:9001"       # a real HTTP/2 server, not a proxy
  proto: ["*.proto"]                   # globs allowed; used for routing AND reflection
  tls: { key: …, cert: …, root_ca: …, client_auth_type: no_client_certs }
  max_send_msg_size: 50  # MB
  max_recv_msg_size: 50
  max_concurrent_streams: 10
  ping_time: 1s
  timeout: 20s
  pool: { num_workers: 4 }             # the workers still speak RR's framed protocol

otel:
  resource: { service_name: "rr_test", service_version: "1.0.0" }
  exporter: otlp                       # v6 rejects zipkin; jaeger is gone
  client: grpc                         # ∈ {grpc, http} → :4317 or :4318
  endpoint: 127.0.0.1:4317
  insecure: true
  headers: { api-key: "secret" }
```

### 1.3 Semantics worth copying, and one worth refusing

Collected so that the design sections can refer to them rather than restating them.

**KV.** Per-item TTL; `getTtl` unsupported on memcached (a protocol limitation, not a bug);
pluggable serializers (PHP `serialize`, igbinary, sodium for E2E encryption). There is no
cross-key transaction beyond whatever the driver gives you, and no claim of one.

**Locks.** TTL per lock, `ttl = 0` meaning "held until released"; `UpdateTTL` **replaces** the
remaining TTL rather than extending it; a second read lock with the same owner id is refused
(dedup by id); `wait = 0` means exactly one attempt, bounded to 1 ms on memory and to
`read_timeout` on Redis; the Redis key prefix is fixed (`rr:lock:<resource>`); lock waiting
multiplexes over one shared Pub/Sub connection. **The important one:** a failed or timed-out Redis
command during acquisition returns an error whose lock state is *unknown*, so the caller must poll
`Exists`/`Release` to resolve it. And there are **no fencing tokens** — see §9.2.

**Jobs.** One in-memory priority queue shared by all pipelines (`pipeline_size`), which is *not*
persisted, so a crash loses everything reserved but not yet delivered regardless of how durable
the driver is. `autoAck` acks the instant the job leaves the broker, before the application sees
it. Named pools are selected by a reserved `pool` header, and an unknown pool name
**negatively-acks** even in single-pool mode. Requeue is a job-level call
(`fail($e, requeue: true)`); retry policy, backoff and dead-lettering are **application
convention, not server features** — RR has no built-in schedule. Trace context rides along as
job headers.

**Metrics.** Names are `rr_<plugin>_<metric>`, e.g. `rr_http_request_total`,
`rr_http_request_duration_seconds{status}`, `rr_grpc_request_total{grpc_method,status_code}`,
`rr_jobs_jobs_ok` / `_err` / `_requeue`, `rr_jobs_push_latency_bucket`, and a common pool set
(`{{plugin}}_total_workers`, `_worker_memory_bytes{pid}`, `_worker_state{pid,state}`,
`_workers_ready`, `_workers_working`, `_workers_invalid`) emitted by every plugin that owns a
pool. `prefix` and `labels` apply to plugin metrics only, never to `go_*`/`process_*` or `collect`
metrics, and a label name colliding with a reserved one (`pid`, `state`, `status`) **fails the
plugin at startup**. In v6 the jobs counters became counters rather than gauges, with the note that
failed and requeued outcomes no longer also increment `_ok`.

**gRPC.** Connection knobs are Go's `grpc.KeepaliveParams` / server parameters almost 1:1
(`max_connection_idle`, `max_connection_age`, `max_connection_age_grace`, `ping_time`, `timeout`,
`max_concurrent_streams`), which map cleanly onto the tuning hyper's h2 layer already accepts. The
service definitions are loaded **at runtime from `.proto`** and used for both routing and server
reflection — which is the single heaviest piece of the plugin, and §6.6 is about not having to copy
it: there, routing is a path string and the descriptors belong to an optional reflection service.

**RPC.** `rpc.Version` and `rpc.Config` exist; `rpc.Config` returns the server's configuration to
anyone who can reach the socket. §4.5 treats that as a hazard rather than a feature.

---

## 2. The question this document has to answer first

There are two separable requests in the original ask, and conflating them is how a plan becomes a
wish list:

1. **More RoadRunner-style services** — gRPC, KV, locks, jobs, metrics, a control plane.
2. **More runtimes** — Python/Django and Ruby/Rails in worker mode.

They share exactly one foundation, and it is not obvious which: **a way to call the server from a
worker**. KV, locks and the jobs producer are not really "plugins" in RR; they are *methods with a
different implementation behind each*. So volucer cannot have two of them without first having the
call path. Meanwhile the runtimes need no control plane at all — §7 shows the cheap path uses the
`upstream` backend that already exists.

That is the whole structure of the plan: **one new subsystem (§4), then services on top of it
(§6), and a separate, smaller track for runtimes (§7).** And §4 is smaller than it sounds, because
the channel is already open: what §4 designs is the *branch* on inbound frames, not a transport.

---

## 3. Compatibility stance: native, not drop-in

**Recommendation: volucer-native configuration and ergonomics; RoadRunner's *semantics* as the
spec, its *file format* not at all.** Application-facing APIs should be shape-compatible where a
PHP app would otherwise have to change code, which is a different thing from parsing `.rr.yaml`.

Reasons, none of which are about effort:

- **It is already the settled position.** `docs/plan.md` §3 and §4 chose an own protocol over
  goridge ("adopting its semantics means becoming a RoadRunner clone"), TOML over YAML, and a
  config model with `#[serde(deny_unknown_fields)]` on every table. Reading `.rr.yaml` would add a
  second configuration language, a YAML dependency, and a versioning commitment to somebody else's
  schema — and the schema in the checkout is already visibly out of step with the engine (it still
  enumerates `zipkin` and `jaeger` exporters that v6 rejects).
- **The formats disagree on purpose.** RR's plugin sections are a flat namespace with
  `driver` + `config` nesting and driver-specific keys interpreted by JSON-schema `if`/`then`
  chains. volucer's model is typed enums tagged by `kind` (`BackendConfig`,
  `crates/volucer-core/src/config.rs:1349`) with validation in Rust where the error message can be
  written for a human. Emulating the former in the latter loses the only advantage the latter has.
- **Where compatibility *does* pay is the PHP side**, and that is cheap to provide without
  touching the config: a KV client that implements `Psr\SimpleCache\CacheInterface`, and a jobs
  driver that implements the queue interface `sailantis/azera-queue` already defines
  (`azera-queue/src/Backend/Contract/ReservableQueueInterface.php`). An application swaps a class,
  not a YAML file.
- **The one thing to borrow literally is metric names.** `rr_http_request_total` and friends are
  what dashboards and alerts are already written against. Renaming them buys nothing and costs
  every operator a migration. See §6.1.

There is, however, one deliberate *structural* lesson to take from RR, in §5.

---

## 4. The control plane

### 4.1 What is missing today

volucer has no external control surface at all. `Command` in `crates/volucer/src/main.rs:34` is
`Serve`, `Check`, `Version` and nothing else; a running server exposes no way to ask it anything or
tell it anything. Signals are the entire interface (shutdown, and `SIGHUP` for reload — plan.md
hazard 60). Internally the pieces exist and are unused by anything outside the process: worker
`CONTROL` messages (`Drain`, `Shutdown`, `Reload`, `Report` —
`crates/volucer-worker/src/message.rs`), per-listener `Stats`
(`crates/volucer-server/src/listener.rs`), and the process-wide `volucer_watch::Registry` that
owns the one reload path.

### 4.2 The design: one channel, one dispatcher

The premise is narrower than "build an RPC framework". Everything already arrives on one pipe; the
only open question is **what an inbound frame from the server means**, and the answer is a two-way
branch that already exists:

- a frame belonging to the **request in flight** → it is the response, and the loop proceeds exactly
  as today (`RESPONSE` → `BODY` → `END`, terminating at `END`);
- **anything else** → it is a call from the server, which gets an answer and changes nothing about
  the response exchange.

That second arm is what already happens. `CONTROL` gets an answer and the loop continues; `PING`
gets `PONG`; `LOG`, `METRIC` and `EVENT` are absorbed without derailing the exchange. A
`CALL`/`RETURN` pair is *another arm on that same match*, not a new transport, not a new port, and
not a new socket class. The direction is **worker → server** (§4.3): the application asks, the server
answers. The user-facing rule is one sentence: **while a request is in flight, a frame that is not
part of the response is a call.**

**Client side is small, and it can be non-reentrant.** `Worker::call()` writes the `CALL` and reads
frames until its own id comes back, handling a `PING`, a `CONTROL` or another `CALL` on the way
exactly as the main loop handles them — the same shape as the existing dispatch, with `RETURN` arms
added. Nothing needs to interrupt the application, because:

- worker-initiated calls only make sense from inside a request (that is when there is state to
  inspect, and when the server has a reader waiting);
- a `CALL` that arrives while the worker is idle is therefore impossible by construction, and
  `Worker::call()` raises rather than hanging if it is attempted — see §4.3.

So the SDK never needs a nested pump inside handler execution. It needs a dispatch loop that answers
the arms, which it already has, plus one method that adds an arm of its own.

**Server side is where the design work is, and all of it is one thread.** There is exactly one
reader per worker, on the lifetime exchange thread (`exchange.rs:76`), and it is blocked on
`recv()` **between** requests — nothing reads the pipe then, so telemetry and control frames are
only ever drained mid-exchange and only while a request is in flight. Four consequences:

1. **Handle calls in `read_response`, and answer them from the registry.** `codec.rs:735` currently
   ends in `other => Err(WorkerError::NotA { expected: "a response frame", … })`, which is *fatal* —
   an unknown type mid-response kills the worker. A `CALL` arm has to be handled before that, or it
   behaves as a protocol failure. The arm writes a `RETURN` rather than a `CALL`: the server answers
   here, and the only thing that originates a call is the worker (§4.3).
2. **The exchange thread does not need to know what a KV store is.** It hands the call to a handler
   and writes back whatever comes out, so the frame layer does not learn about services and the
   service layer does not learn about framing. §5's service bag is what the handler set is built
   from.
3. **The call inherits the request's deadline, and the deadline is a kill.** `request_staged`'s own
   documentation is the reason this is the one real cost: a read failure means "the application may
   be running it right now", and the pool's answer is to kill the worker to make the pending read
   return. A worker blocked on a slow `lock.acquire` is therefore a worker that can be killed
   *mid-call*, and the server must not conclude "not held" from that — see §6.3, where the same
   ambiguity is already an explicit error variant. The fix is a per-call deadline enforced *inside*
   the handler, so that "the backend was slow" is an answer rather than a kill.
4. **A server-side panic takes the worker with it.** The handler runs on the exchange thread, so an
   escaping panic is a poisoned or dead worker. Handlers must be `catch_unwind`-guarded, and that is
   a rule rather than a nicety because the failure mode is a request path that dies silently.

`CALL` is **worker → server** only. A server never originates one — it already has `REQUEST` for
that — so a `CALL` arriving *at* the server is a protocol error, and the server's arm for it writes a
`RETURN`. Anything a worker says on its own keeps the semantics it already has (`LOG`/`METRIC`/`EVENT`
are unsolicited and absorbed), and the response remains the worker's way to answer the work it was
given.

**Why not a separate socket.** It was the earlier recommendation in this document, and it is worse
for a reason that has nothing to do with effort: a second socket makes the worker's liveness
ambiguous. Close the data pipe and the worker's `read` returns EOF and it exits — that is the whole
shutdown mechanism, and `docs/worker-protocol.md` is explicit that it is "shutdown by
construction", with no signal to catch. Adding a channel the worker also blocks on destroys that
property. It also costs a second accept loop, a second handshake, a second protocol to version, and
its own address plumbing, to reach a process the server is *already* connected to over a channel
that was measured faster than the alternatives (hazard 46 — a Windows pipe read is ~8 ms for PHP
against ~0.09 ms on a socket; the socket transport already exists for that case and is selected
automatically).

**Hazard: a call is served while the caller's own slot lock is held, so a call handler may not take a slot
lock.** Found by the first end-to-end test of the call plane, which hung until the request deadline killed
the worker: `workers.list` walked every slot with `lock()`, and the call was being served on the exchange
thread of a worker whose request was holding slot 0 — so it waited for its own lock. Nothing in the design
above is wrong; what was missing is that **a call runs inside a request**, which the existing invariant
"the slot lock is never held across a request" had stopped being true of. The rule for handlers is
therefore: a slot lock may only be taken by something that is not serving a request, so a handler reads
pool state with `try_lock` and reports what it could not observe rather than waiting for it. That is not a
degradation — a slot that is locked is a slot serving a request, which is exactly `idle: false`, and it is
not restartable either (`WorkerPool::restart` refuses it with `AllBusy`).

**Hazard: an undrained channel stalls a call at the deadline.**
`ChannelExchange::request` posts an exchange and waits on `recv_timeout`, so a worker blocked
answering a server call cannot have the next *request* written to it — the send succeeds, the
answer never comes, and the request dies at its deadline with the worker then killed. The rule that
makes this sound is what §6.3 already learned from RoadRunner's own lock implementation: **a call
that ran out of time has unknown state.** The server may drop the operation and keep the worker
(reporting the failure), but it must **not** kill the worker as the timeout strategy while calls are
outstanding — killing closes stdin, the worker exits, and the response that was in progress is lost
too. This is the one policy that has to be written for this design and it is not obvious from
reading either side alone.

**Why "one request in flight" survives this.** The three places that state it
(`docs/worker-protocol.md`, *Correlation*; `crates/volucer-server/src/pool.rs:12`;
`crates/volucer-fcgi/src/client.rs:7`) are making two separate claims, and only the second is at
stake here. The first — no request *multiplexing*, so no demux table — is untouched: there is still
one request in flight. The second — that the worker "only ever handles `HELLO`, `REQUEST`, `PING`
and `CONTROL`" (`docs/worker-protocol.md`, *Which types a worker may receive*) — becomes false, and
the doc has to say so rather than quietly gaining an arm. Neither claim is about the *response*
being one exchange, which is what actually has to hold.

**An earlier draft of this section recommended a separate relay socket, and was wrong about it.**
The reasoning it gave — "a nested pump that re-enters the same read path" — overstated the client
and misplaced the server difficulty. The client needs a dispatch arm, not a reentrant pump, and the
server's cost is one `catch_unwind` and one deadline policy on a thread that already exists.
Preproduction and one channel in hand make this the only sensible choice, and the rest of §4 is
written for it.

### 4.3 The message set on the one channel

Two new types, and one optional field on an existing one. Nothing else changes.

| # | Name | Direction | Correlated | Carries |
| --- | --- | --- | --- | --- |
| 15 | `CALL` | worker → server | yes | `name` (`"<namespace>.<verb>"`), JSON params, `request_id` = the call's own id |
| 16 | `RETURN` | server → worker | yes | `ok`, JSON result or an error, and `unknown`, matching `request_id` |

**The direction is worker → server, and it is worth stating plainly because the opposite reading is
just as plausible on first contact.** A call is the *application* asking the server for something —
`kv.get`, `lock.acquire`, a metric — which is the only direction that has a reason to exist: the
server already has a complete way to ask a worker to do work, and it is `REQUEST`. So a call changes
nothing about the response exchange it arrives during, and the id it carries is allocated by the
**worker**, counting down from `u32::MAX`.

`CALL` ids are therefore allocated from a range disjoint from request ids — request ids are the
server's and start at 1 and increment (`FIRST_REQUEST_ID`) — which keeps "a stray correlation bug is
visible instead of silently matching" true (`crates/volucer-worker/src/lib.rs:53`), the property that
made 0 reserved. Two consequences follow, and both are load-bearing:

- **A server never originates a call**, so a `CALL` arriving *at* the server is a protocol error rather
  than something to serve — `docs/worker-protocol.md`'s "which types may be received" table now says so
  on both sides.
- **A worker only ever calls while a request is in flight.** The server's reader for a worker is parked
  on `recv()` between requests, so an answer to a call made outside a request has nobody to arrive at.
  `Worker::call()` refuses rather than hanging on that, because a hang there is a worker killed at the
  *request's* deadline with nothing in the log naming the call responsible.

`HELLO_ACK` gains one optional field, `runtime` (`"php"`/`"python"`/`"ruby"`), which §7.2 needs
anyway and which lets the server apply the right policy without guessing. Additive, but see §4.7 —
nothing here is versioned against a deployed peer.

**What the discarded relay design would have added, and does not.** No second socket, therefore no
cross-wiring hazard between two protocols sharing one header, no second handshake, no second version
to negotiate, and no crate extraction to keep a second protocol from depending on the data-plane
crate: the frame primitives stay in `volucer-worker` because only one protocol uses them. What
replaces the old §4.4 is the method registry below.

### 4.4 The method registry

Methods are `"<namespace>.<verb>"`: `kv.get`, `kv.set`, `lock.acquire`, `lock.release`,
`jobs.push`, `metrics.observe`, `workers.list`, `worker.reset`, `config.describe`, `server.version`.
Params and results are JSON (the codec stays behind its boundary; msgpack remains available for the
same reason it does in the worker protocol).

The registry maps a namespace to a handler, and the handler set is *the* thing a new feature
registers with — which is §5's shape. It lives in `volucer-server`, is built by `serve` from the
configured services, and the exchange thread holds an `Arc` to it. **A method that is not registered
is a `RETURN` error naming the method**, not a dropped frame and not a dead worker.

`workers.list`, `worker.reset` and `config.describe` need no new service at all — they read the pool
and the configuration that already exist — so a control plane is available from the first commit
here rather than after KV is written. `volucer workers` and `volucer reset` become CLI clients of
it. `config.describe` is **redacted** (§4.6), because the answer includes driver config and would
otherwise hand out credentials over a channel any worker can write to.

### 4.5 Security: the channel is now a remote-execution surface

Routing calls over the worker channel removes the socket and its permission bits, so the only thing
between an application and `worker.reset` is the pipe the server opened. That is not a hole — a
worker already runs application code as the same user — but three things follow, and the second is
the one that would be forgotten:

- **Capability is per-worker, not per-address.** There is no address to protect, so the registry
  must be selected per *backend*: a `worker` backend for the public HTTP listener gets read-only
  namespaces, and `worker.reset` / `config.describe` are available only where configured. A global
  method table would give every worker every capability.
- **`config.describe` is redacted** with a per-service allow-list, because its answer includes
  driver configuration and would otherwise hand out credentials over a channel any worker can write
  to. RR's `rpc.Config` does exactly this and it is a cautionary example, not a precedent.
- **The calls are refused when `[rpc]` is absent.** No namespace is registered, so nothing is
  reachable, and an application that tries gets a named error rather than silence. This replaces
  "the relay is off unless configured" with something stronger: there is no surface to be off.

### 4.6 What this unlocks immediately

- `volucer workers` and `volucer reset` as CLI clients of the method registry, which is exactly what
  RR's CLI is (`roadrunner/internal/cli/workers/command.go:32`, `informer.List`). Neither needs a
  service: they read the pool (`restart(index)`, `crates/volucer-server/src/worker_pool/mod.rs:653`)
  and the configuration that already exist.
- The `status`/health endpoint (§6.1) can borrow the same `ready` notion.
- Every §6 service has somewhere to live from the day it is written.

### 4.7 Versioning, now that nothing is deployed

`protocol_version` stays `1` and gains no "v2" — it is the *frame* version, and the frame did not
change. What changed is the message set, and the honest record of that is
`docs/worker-protocol.md` plus the SDK's version, not a protocol bump. This is only safe because
nothing is released: with deployed workers in the field, a new message type is **fatal** to an old
SDK (`docs/worker-protocol.md`'s error table makes an unexpected type unusable-stream, not merely a
warned-and-skipped frame), so this would have been the one change in this document that could not be
rolled out server-first. Writing it down now is what stops it being discovered later.

---

## 5. The extension point, which is the one structural lesson

RR's extensibility comes from `endure`, a runtime DI container, and a plugin model where any plugin
can request any other. That is the right idea for Go and the wrong one for Rust: we do not need a
container to discover types, and a dynamic plugin ABI would give up static dispatch, the MSRV gate,
`cargo package --workspace` and cross-compilation in one step.

The Rust-shaped equivalent is a **compile-time service registry with a lifecycle trait**, plus
cargo features to choose what is compiled:

```rust
// crates/volucer-server/src/service.rs
pub trait Service: Send + Sync {
    fn name(&self) -> &'static str;
    /// Report readiness to the status endpoint. Cheap, non-blocking.
    fn ready(&self) -> Ready;
    /// Stop, with the same deadline discipline the rest of the server uses.
    fn stop(&self);
}
```

`serve` builds a `Services` bag, starts whatever the configuration asks for, hands the bag to the
listeners that need it, and stops it in one place alongside the existing shutdown sequence
(`crates/volucer/src/main.rs:1048`). A listener that wants metrics reads them from the bag; a
call handler that wants a queue asks the bag. Three properties follow, and each is a bug the
alternative has:

- **No service can be started twice or leaked.** The bag is the only list, so "the server shut down
  and left a Kafka consumer running" is not expressible.
- **A call cannot reach a service that was not configured**, because the bag is what the method
  registry was built from.
- **A build without a driver has no code for it**: `cargo install volucer` must not link a Redis, a
  Memcached and an AMQP client for a deployment that uses the in-memory queue. Features, grouped
  per driver, with `memory` in the default set.

The **configuration shape** follows the feature boundary rather than nesting everything under
`[server]`: `[metrics]`, `[status]`, `[services]`, `[kv]`, `[lock]`, `[jobs]`, `[grpc]`, each
`deny_unknown_fields`, each optional. `[server]` stays about the request path. This mirrors RR's
top-level section names, which is a coincidence worth having: an operator reading both
documentations recognises the words.

**There is no `[rpc]` table**, because there is no listener to configure: the method registry
travels on the worker channel that already exists, and §4.4 is about which methods a *backend* may
call. That is a per-backend setting, so it belongs beside the backend:

```toml
[backends.app]
kind    = "worker"
command = "php vendor/sailantis/azera-volucer/azera-worker.php"
# Namespaces this backend's workers may call. Absent = none, which is the safe default.
services = ["kv", "lock", "jobs", "metrics"]

[backends.ops]
kind     = "worker"
command  = "php bin/ops-worker.php"
services = ["workers", "config"]
```

A **worker with no `services` key can call nothing**, which is the right default for a backend
facing the public and means "add KV" is a one-line diff rather than a new exposure.

---

## 6. The services

Each section answers the same four questions: what it is, what it attaches to, what it copies
faithfully, and what is genuinely hard.

### 6.1 Metrics and status — first, and not negotiable

**Why first.** It is the only feature whose absence makes every other feature harder to land: there
is currently no way to see whether a worker pool is healthy, a queue is draining, or a lock is
contended, and `volucer-observe` today is request identity, W3C trace context and an access log
(`crates/volucer-observe/src/lib.rs`) with no registry at all. It also needs no calls, no driver
and no new protocol, so it can land while the design of everything else is still being argued.

**What already points at it.** `absorb_log` / `absorb_metric` in
`crates/volucer-worker/src/codec.rs:720` already decode `LOG` and `METRIC` frames and then do
almost nothing with them — `METRIC` is attached to the response and dropped, `EVENT` is a
`tracing::trace!` (`codec.rs:721`). The PHP SDK already has `emitMetric`, `emitLog` and `recycle`. The PHP SDK already has `emitMetric`, `emitLog` and `recycle`. So the wire path
for "an application publishes a metric" exists and is tested; what is missing is a place for the
sample to go.

**Design.**

- New `volucer-metrics` crate: a registry of counters, gauges and histograms, keyed by name and
  label set, cheap enough to update from a request path (atomics and sharded maps; a histogram is a
  bucket array of atomics).
- A **hand-written** Prometheus text exposition encoder rather than the `prometheus` crate. The
  format is `# HELP` / `# TYPE` / `name{label="v"} value`, it is a page of specification, and the
  crate brings `protobuf` for OpenMetrics which we do not want in the graph — a decision consistent
  with the static-file layer and the access log, both hand-written for the same reason.
- `[metrics] address`, off by default; **its own listener**, never a route on the HTTP listener,
  because that listener may be TLS-only and faces the public.
- `[status] address` with `GET /health` and `GET /ready`, which reports per-service readiness from
  the bag. Cheap, and the thing a container orchestrator asks for.
- **Worker `METRIC` frames become samples.** `absorb_metric` publishes into the registry, so a
  PHP application's `emitMetric` reaches Prometheus without the application knowing the endpoint
  exists. `LOG` keeps its current behaviour (attached to the response, already forwarded to the
  front-end's error log).
- **Names.** Plugin metrics keep RoadRunner's names with the `rr_` prefix, because operators'
  dashboards are written against them (see §3). The pool set is the same shape RR emits for
  `http`, `grpc` and `jobs`:
  `rr_http_total_workers`, `rr_http_worker_state{pid,state}`,
  `rr_http_workers_ready`, `rr_http_workers_working`, `rr_http_worker_memory_bytes{pid}`,
  `rr_http_request_total{status}`, `rr_http_request_duration_seconds{status}`,
  `rr_http_requests_queue`, `rr_http_uptime_seconds`.
  Our own additions — a FastCGI front-end, the ACME path, reloads — get names in the same
  namespace: `rr_fastcgi_request_total`, `rr_acme_renewals_total`, `rr_reloads_total`.
  Note that RR requires the `http_metrics` middleware to be listed for the HTTP metrics; volucer
  should emit them whenever `[metrics]` is present, and be explicit about the divergence in §9.3.
- **`[metrics] collect`** for application-declared metrics, pushed with a `metrics.observe` call,
  so `emitMetric` can be upgraded from fire-and-forget to a
  declared histogram without a protocol change.

**What is hard.** Nothing conceptually; the discipline is in the cardinality budget. A label
derived from a request path is how a metrics endpoint takes down a Prometheus server, so labels
come from a closed set (status class, method, site name) and the registry caps distinct series per
metric with a logged refusal beyond the cap.

**Sizing.** S–M. Registry + encoder + two listeners + wiring `absorb_metric`.

### 6.2 KV

**Attaches to.** The call path (§4), plus a `Service`.

**Config.**

```toml
[kv.default]
driver = "memory"           # memory | redis | memcached | file
# [kv.default.redis]
# address = "redis://127.0.0.1:6379"
# db = 0
# pool_size = 16
# [kv.sessions]
# driver = "file"
# path = "/var/lib/volucer/kv/sessions.db"
```

Deliberately flatter than RR's `driver` + untyped `config` pair: a driver-tagged enum, which is the
same pattern `BackendConfig` uses and which makes an invalid key an error at parse time rather than
a silently-ignored one.

**Copied faithfully.** Per-item TTL; PSR-16 method set (`get`, `set`, `has`, `delete`, `clear`,
`getMultiple`, `setMultiple`, `deleteMultiple`, TTL accessors); a key namespace per storage so two
applications sharing one Redis do not collide; the serializer being pluggable on the *client* side
rather than fixed on the server, because that is where the E2E-encryption case lives.

**Drivers, in order.** `memory` (a sharded map with a TTL sweeper — useful on its own for
single-node caching, and the reference implementation for the trait) → `redis` (`redis-rs`, already
the right shape for both KV and the lock in §6.3, so one dependency serves two features) →
`file` (an embedded store; see the hazard below) → `memcached` (a small text-protocol client; the
weakest of the four because RR's own memcached driver cannot report TTL, so it is last and
optional).

**Hazard.** RR's boltdb driver has a `permissions: 0755` default on a *database file*, which is
world-readable where it should not be. If volucer ships a file-backed driver, the default is `0600`
and the check is on the file rather than on the directory — the same rule plan.md hazard 73 settled
for certificates.

**Sizing.** S for `memory`, M for `redis` and the trait, S more per additional driver.

### 6.3 Locks

**Attaches to.** KV's connection handling (the Redis case shares a client), and the call path.

**Config.**

```toml
[lock]
driver = "memory"           # memory | redis
# [lock.redis]
# address = "redis://127.0.0.1:6379"
# read_timeout = "5s"
```

**Copied faithfully, including the parts that are easy to get wrong:** `ttl = 0` means held until
released; `update_ttl` replaces the remaining TTL rather than extending it; a second *read* lock
with the same owner id is refused; `wait = 0` is exactly one attempt bounded by `read_timeout`; the
key prefix is configurable but fixed per storage; and — the one that matters most — **a command
that failed or timed out during acquisition returns "lock state unknown", not "not held"**, because
treating an unknown as a failure is how two processes end up in the same critical section. That
error is a distinct variant, not a generic one.

**Hazard, and it must be in the README, not only here.** A TTL lock is not a correctness primitive
under a paused process: a worker that stalls past its TTL holds a lock the server believes has
expired, and no amount of care on the server side fixes it. RoadRunner's locks have **no fencing
tokens**, which means they cannot stop a late writer — the classic Kleppmann objection. So:

- The docs say what a lock is *for* (avoiding duplicated work, coalescing a cache miss) and what it
  is **not** for (protecting an invariant whose violation corrupts data). That is a stronger
  statement than RR makes and a more useful one.
- Where a real invariant is needed, the answer is the storage's own conditional write — Redis
  `SET NX`, a unique index, a compare-and-swap — and the KV API should expose a
  `set_if_absent` so that path exists.
- **Single-node locks work today and need no external service**, which is worth having for the
  common deployment before the Redis case is written.

**Sizing.** S for `memory`, M for the Redis script and its tests, S for the call methods.

### 6.4 Jobs

The largest item, and the one to be most careful about promising. Split by *driver*, which is where
the cost actually lives:

| Driver | Client | Verdict |
| --- | --- | --- |
| `memory` | none | **ship** — the reference implementation, enough for a single node and for tests |
| `redis` | `redis-rs` (shared with §6.2/§6.3) | **ship** — the honest multi-node default, with a Lua script for reserve/ack atomicity |
| `file` | embedded store | probably, for durability without a broker |
| `amqp` | `lapin` | later |
| `sqs`, `kafka`, `nats`, `beanstalk`, `nsq`, `google_pub_sub` | one large client each | **not in this plan** — see §9.4 |

**The engine** is driver-independent and is the part worth designing carefully. It copies RR's:

- **one internal priority queue** shared by all pipelines, of bounded `pipeline_size`, with
  `num_pollers` tasks pulling from it — and with the limitation **stated in the config
  documentation rather than discovered**: the queue is in memory, so a crash loses jobs that were
  reserved from the driver but not yet delivered, however durable the driver is;
- **reservation with a TTL**, ack/nack, requeue, and `auto_ack` as a per-job flag whose danger is
  documented where it is set;
- **named pools** selected by a reserved header, with an unknown pool name negatively-acking rather
  than silently routing;
- **retry, backoff and dead-lettering left to the application**, because RR leaves them there and
  inventing a server-side policy would make two applications' semantics fight;
- **trace context propagated as job headers**, once §6.7 exists;
- **`jobs.push` as a call** for the producer side, and the worker protocol for the consumer
  side (below).

**The consumer side reuses the worker pool, and this is the concrete integration.** A consumer is a
persistent process that booted once and is handed work repeatedly — which is exactly what
`WorkerPool` already does: boot, handshake, hand over one unit of work, read the answer, recycle on
`keep_alive = false` / `max_requests` / `peak_memory_bytes`
(`crates/volucer-server/src/worker_pool/mod.rs`, and `docs/worker-protocol.md`'s *server's side of
the contract*). So:

- A **`job` exchange** joins the existing `request` exchange in
  `crates/volucer-server/src/worker_pool/exchange.rs`. Same slot leasing, same timeouts and kill
  discipline, same recycling rules; only the payload differs.
- The protocol gains a small message set for it — the job payload in, and a terminal frame carrying
  `ack` / `nack` / `requeue` plus the failure reason and whether to requeue. It is deliberately
  *not* a new `REQUEST` with a magic URI: a worker must be able to tell a job from an HTTP request,
  because the per-request state reset differs.
- `[jobs.pool]` reuses `WorkerConfig` and therefore every knob it already has (workers,
  `max_requests`, `memory_limit_mb`, `checksum`, `transport`), plus RR's `max_jobs` alias is
  **not** copied — `max_requests` already means this and means it explicitly (see the field's own
  comment at `crates/volucer-core/src/config.rs:1795`).

**PHP integration is the point of the feature for this organisation.** `sailantis/azera-queue`
already defines `Backend\Contract\ReservableQueueInterface`, a `Worker\Worker`, a
`Dispatch\Dispatcher` and a failed-job provider. A `VolucerQueue` backend implementing that
interface against the call path means an Azera application gets a queue by changing one binding. That
is worth more than seven broker drivers, and it is the reason to do this at all.

**Sizing.** L. Engine + memory driver + protocol messages + pool exchange: the bulk. Each further
driver: M to L, and each is really its own decision.

### 6.5 What jobs and the call path imply for `volucer-observe`

Not a feature, a consequence worth planning for: once there are spans from a queue push and a job
consume, the trace has to continue across the call boundary, and the current observability story
is request-scoped by construction (`volucer-observe`). The call metadata should carry `traceparent`
from the beginning — it costs a field now and is a protocol change later.

### 6.6 gRPC — three tiers, and the schema is not the server's problem

`docs/plan.md` §6 (*"gRPC — a named non-goal"*) settled the shape and recorded a trigger. This
section answers the question it left open — *static or dynamic `.proto`?* — and the answer is that
**it is a false choice**. The build step belongs to the *application*, and the server can route,
frame and answer without parsing a single `.proto`. The only thing that genuinely needs descriptors
is **reflection**, which is optional and separable.

#### What was already verified in this tree

Five facts, all read from source rather than from documentation, and together they move this from
"XL, gated, probably a rewrite" to something with a small first step:

1. **`h2` 0.4.20 and `http-body` 1.1.0 are already in `Cargo.lock`**, as hyper's own dependencies.
   No protobuf crate is present at all.
2. **hyper 1.12 surfaces trailers on the receive side for both protocols.**
   `hyper-1.12.0/src/body/incoming.rs:176` (the h1 channel) and `:209` (`h2.poll_trailers`) both
   yield `Frame::trailers`, and `Frame::data`/`trailers` are distinct variants, so a
   frame-preserving wrapper cannot leak trailers into the data stream.
3. **A body that preserves frames already exists here.**
   `volucer_upstream::incoming_body` is `BodyExt::map_err(body, …).boxed_unsync()`
   (`crates/volucer-upstream/src/client.rs:248`) — deliberately *not* `collect`, and deliberately
   not `BodyExt::boxed`, because that would require `Sync` and an h2 body is not.
4. **Trailers already survive the proxy path, needing no code.** `translate_response` copies only
   `{status, headers, body, version}` (`crates/volucer-upstream/src/translate.rs:152`) — it never
   inspects a frame — and the front-end hands that same body straight out:
   `outgoing.body(answer.body)`, with the comment *"`UnsyncBoxBody` is the reason this needs no
   conversion: an origin's body is exactly what the response type is now"*
   (`crates/volucer-http/src/listener.rs:1323`). `UpstreamBody` and `ResponseBody` are the same
   type alias. **This one was a reading and is now a measurement** — see below.
5. **A gRPC endpoint is the same listener, not a new one.** `serve_connection` is generic over the
   stream and builds a `hyper_util` auto builder with ALPN-aware protocol detection
   (`crates/volucer-http/src/listener.rs:990`), so HTTP/2 with prior knowledge — which the project
   has already proven by hand — arrives at the *existing* `service_fn`. A gRPC route is
   `/pkg.Service/Method`, which `path_prefix` already expresses
   (`crates/volucer-core/src/config.rs:333`).

#### The trailers claim, measured

Finding 4 was the load-bearing one — `plan.md` §6 says gRPC is gated because trailers are absent,
and the whole "Tier A is small" argument is that they are *not*. So it was tested rather than
argued, in `crates/volucer-upstream/tests/trailers.rs`: a plaintext origin answers with a chunked
response whose trailer section carries `grpc-status: 0` and `grpc-message: OK`, and the test
asserts the fields reach the caller **with the body intact**.

It passes. Three things about the test are worth carrying into the design:

- **The `Trailer` declaration and the fields are asserted separately**, because "the declaration
  was stripped" and "the fields were lost with it" are the two readings that a single assertion
  cannot tell apart. `Trailer` is in `NEVER_FORWARDED`, so it is stripped on purpose, and the
  fields arrive anyway — which is correct, since they travel as frames and not as headers.
- **A control asserts that a response with no trailers reports none.** Without it, a `trailers()`
  that returned `Some` for any reason would pass the main test — hazard 67's shape.
- **The claim is falsified by handing back an empty body**, which fails the trailer assertion
  specifically. So the test can fail, and fails for the reason it exists.

**So the correction to `plan.md` §6 is now measured, not argued.** That section's finding is right
about the *code* — nothing here constructs a trailer, and the three `grep -i trailer` hits really
are a strip list — and wrong about the *effort*: carrying one already works, and what is missing is
the construction side alone.

**And the measurement found something the reading did not.** The connector speaks HTTP/2 **only
over TLS**: `negotiated_h2` is set from the ALPN result and nothing else ever sets it
(`crates/volucer-upstream/src/client/connect.rs:264`, and `TlsIo`'s `Connection` impl at `:338`),
so a plaintext or `unix:` origin is pinned to HTTP/1.1. gRPC's transport is HTTP/2, so tier A needs
either TLS with ALPN or an explicit h2c path that does not exist yet. This is a decision for tier A
rather than a defect in the proxy — the h1 path carries the trailers correctly today — but it is
invisible until something needs h2c, which is exactly why it is written down here.

#### What the server actually has to do, and what needs a schema

The distinction that dissolves the question:

| Step | Needs the schema? |
| --- | --- |
| Read `:path` as `/pkg.Service/Method` | **no** — it is a string |
| Route it (`path_prefix`, or a service table) | **no** |
| Forward `grpc-timeout`, `grpc-encoding`, `grpc-accept-encoding`, `te: trailers` | **no** — headers |
| Pass request DATA frames through as opaque bytes | **no** |
| Pass reply DATA frames back | **no** |
| Write `grpc-status` and `grpc-message` as trailers | **no** — two header names |
| Map `RST_STREAM` and connection failures to a status | **no** |
| Reflection, JSON transcoding, validation, typed handlers | **yes** |

The reason rows four and five are schema-free is the part worth stating plainly: gRPC's inner
framing is **length-delimited, not schema-dependent** — one compression-flag byte, a big-endian
`u32` length, then that many bytes — so a server that never decodes a message still knows exactly
where each one ends. The schema is needed to *interpret* bytes, not to *carry* them.

#### The three tiers

**Tier A — a transparent gRPC pass-through (no protobuf crate at all).**
Route `content-type: application/grpc*` to a Unix-socket origin and relay the call. What it needs
is the **construction** side of trailers — a small `Body` that yields `Frame::trailers` for the two
status headers — which is a shape this codebase already hand-writes at
`crates/volucer-http/src/static_files.rs:806`. That is roughly a forty-line type plus tests.

The one thing tier A must settle first is the **transport**, and this is now measured rather than
open: `crates/volucer-upstream/tests/h2c.rs` shows that **`Connected::negotiated_h2()` is the whole
mechanism**. `client-legacy` reads that answer off the connector's response and dispatches to its
HTTP/2 client instead of its HTTP/1.1 one, and `ConnIo` already forwards `connected()` to whatever
`Io` it wraps — so an h2c path for a plaintext origin is a decision about *when* to answer
`negotiated_h2`, not new machinery. The probe asserts both halves, because only the negative case
makes the positive one mean anything: with the claim, hyper opens with the HTTP/2 connection preface;
without it, the same client sends `POST … HTTP/1.1`. Falsified by making the claim a no-op, which fails
the positive case and leaves the control passing.

What tier A needs, then, is the *policy* rather than a mechanism: a configuration key saying "this
origin speaks h2", and `connect.rs` answering `negotiated_h2` for the plaintext and unix origins so
marked. That is a handful of lines and one new key — see §10 item 3, which is now a decision about
vocabulary rather than about feasibility. TLS with ALPN already works and needs nothing.

This is what makes volucer a gRPC *server* for runtimes that already speak it — Node's
`@grpc/grpc-js`, Go, Python's `grpcio` — **using the same `upstream` backend §7.1 is adding anyway**.
It is the cheapest gRPC capability in this document and it should land first.

**Tier B — the application as the gRPC handler (still no schema on the server).**
`CALL`/`RETURN` (§4.2) carries the method path and the raw message bytes; the application decodes
with whatever it already uses. Needs:

- Tier A's frame-preserving construction side, so the reply's trailers reach the client;
- a **service allow-list in configuration** — `services = ["greeter"]` beside the backend — so a
  route cannot claim an arbitrary path. This is *not* `.proto` loading; it is the same shape as the
  namespace allow-list §4.5 already introduces for KV and locks;
- an application-facing contract in the SDK, e.g.
  `GrpcServiceInterface { handle(string $method, string $bytes): string }`, with the status and
  message supplied separately so a failed call is expressible.

The server never parses a message. Note that RoadRunner needs `protoc-gen-php-grpc` only because
*its* worker wants typed DTOs; the transport does not require them, and neither will this.

**Tier C — reflection (the only descriptor consumer).**
`grpc.reflection.v1.ServerReflection/ServerReflectionInfo` answers with a serialized
`FileDescriptorSet` for the requested symbols, so `grpcurl` and `grpcui` can introspect without
being handed `.proto` files. This is where runtime parsing lives: `protox` compiles `.proto` text to
a `prost_types::FileDescriptorSet` **purely in Rust, with no `protoc` binary**, and the set is cached
in the service bag (§5). Reflection is a small *service*, not a request-path dependency, and it may
legitimately be off.

#### The decision, restated as two independent ones

Replace *"static or dynamic `.proto`"* with:

1. **Message encoding: always server-opaque.** The server carries bytes and writes two header
   names. This is not a trade-off; there is nothing to weigh, and the `prost`/`tonic` codegen
   pipeline is never on the critical path.
2. **Service discovery: reflection, optional, runtime-parsed.** `protox` → `prost-types` →
   `ServerReflection`, for tooling only. Turning it off costs a caller the ability to introspect,
   which is exactly what happens against any non-reflective server today.

#### What remains genuinely hard

- **A gRPC call as *work* for a worker is the other direction.** `CALL`/`RETURN` is server→worker,
  for the application to reach the server, and preserves one request in flight. A gRPC call is a
  second unit of *work* delivered to a worker, which is either a second exchange or true
  multiplexing — the one thing here that does contradict the one-request-in-flight invariant.
  §10 keeps it open. **Tiers A and B do not need it**: A never involves a worker, and B reuses the
  in-flight request that is already there.
- **Framing code gets its own step with its own tests**, and nginx is the evidence — HTTP/2 to an
  origin arrived only in 1.29.4 and has since had two memory-safety CVEs in that path, one of them
  specifically about a crafted request proxied to an HTTP/2 **or gRPC** backend. The mitigating
  fact worth noting: this server does not *construct* frames, hyper does, so the exposure is the
  construction side of trailers and the status mapping rather than a hand-written h2 state machine.

**Sizing.** Tier A: **S–M plus one transport decision** — the trailer construction is small and
measured, but an h2c path for a plaintext gRPC origin does not exist and is the real cost (§10
item 3). Tier B: **M** (a config surface, an SDK contract, tests). Tier C: **M** (`protox` + a
reflection service). The worker-as-work question: **L** on its own, and no longer on the critical
path for either A or B.

### 6.7 OTLP export

Not on the original list, and adjacent to `metrics` in a way that invites confusion, so it is named
so it can be excluded on purpose. `docs/plan.md` §6 already decided the shape: an exporter *layer*
on top of the existing request ids and trace context, via `tracing-opentelemetry` and
`opentelemetry-otlp` over **OTLP/HTTP on 4318**, which avoids `tonic` and `prost` entirely and is
what RoadRunner defaults to for the same reason. It should land **after** metrics, because the
metric registry is what makes the service bag and the listener pattern exist, and **before** gRPC,
because gRPC spans are the reason anyone asks for it.

---

## 7. Python/Django and Ruby/Rails

Two tiers. The first is almost free and gets the runtimes running this month; the second is the
native answer and needs protocol work. Recommending both is not a hedge — they answer different
questions, and Tier 1 is what lets Tier 2 be judged on evidence.

### 7.1 Tier 1 — supervise their own server, which already exists

Every Python and Ruby application server already implements "boot once, serve many" and already
speaks HTTP over a **Unix socket**:

- Python: gunicorn (`--preload`), uvicorn, hypercorn, granian — all support
  `--bind unix:/run/app.sock`.
- Ruby: Puma (`preload_app!`), Falcon (`preload.rb`), Pitchfork (reforking), Unicorn
  (`preload_app true`) — all bind a Unix socket, and all call `app.call(env)` per request.
- Django's boot-once handle is `get_wsgi_application()` / `get_asgi_application()`; Rails' is
  `Rails.application`, a Rack app whose `call(env)` returns `[status, headers, body]`.

volucer **already** reaches an HTTP origin, including over a Unix socket —
`kind = "upstream"`, `origin = "unix:/run/node.sock"`, documented in the README's *Backends*
section. So the runtime request is not "add a protocol"; it is **"supervise the origin"**:

```toml
[backends.django]
kind    = "upstream"
origin  = "unix:/run/app.sock"
command = "gunicorn myapp.wsgi --bind unix:/run/app.sock --workers 4"
# readiness probe: volucer waits for the socket to answer before serving
ready_timeout = "30s"
```

That is a new field on an existing backend rather than a new backend, and it is worth doing even
if Tier 2 lands, because it is also how a Node, Go or Rust origin gets supervised. What it needs:

- `command` + `env` + `working_dir` + restart policy on `UpstreamConfig`, and a readiness gate so
  the first request does not 502 while Django is still migrating;
- the same lifecycle guard the worker pool uses (job object on Windows, process group on Unix) so
  an orphaned gunicorn does not survive volucer;
- `RequestScope::OriginScoped` already describes this correctly and `check` already prints it
  (`crates/volucer-core/src/config.rs:1373`) — no change needed, which is a nice confirmation that
  the enum was right.

**Cost: S–M per runtime, and mostly in the supervisor, not the runtime.** And it is honest: HTTP
framing is a real cost, and it is the cost every Rails and Django deployment already pays.

### 7.2 Tier 2 — a native worker protocol for another language

This is the interesting one, and the good news is that **the protocol is already almost
language-neutral.** The frame, the CRC-32C, the message numbers, the body chunking and the
handshake have nothing PHP-specific in them; what is PHP-specific is *metadata and adapter work*:

| Where | PHP-specific today | What it should become |
| --- | --- | --- |
| `HELLO_ACK` | `php_version`, `sapi`, and `runtime` carrying PHP bootstrap text | `runtime` (`"php"`/`"python"`/`"ruby"`), `runtime_version`, and optionally `framework`; keep `php_version` deprecated for one version |
| `RequestHeader` | `script_filename` is a CGI notion meaning "the absolute script routing chose" | rename to `entrypoint`, documented as "routing metadata, not a file to execute" — Django and Rails must not treat it as a path |
| `RequestEnd` | `peak_memory_bytes` from `memory_get_peak_usage(true)` | optional; `0` means "this runtime does not report it", and the pool must not recycle on a value it did not get |
| `RESPONSE` | `body_len` declared before the body, so responses are **buffered** | acceptable for v1; a chunked/unknown-length response is a protocol change and is listed in §10 |
| per-request reset | the adapter clears `$_COOKIE`, sessions, request scope | runtime-specific by nature: Django needs `close_old_connections()` and transaction cleanup; Rails needs `Rails.application.executor.wrap` (which also returns ActiveRecord connections to the pool) |
| stdout | is the protocol channel, so the SDK captures `echo` | Python and Ruby must do the same: any print/puts to stdout corrupts framing. This is the single highest-risk adapter mistake and belongs in both SDKs' READMEs. |

**The two concrete protocol changes are additive and backward-compatible** — an optional JSON field
with a `serde` default, which a v1 SDK ignores. `docs/worker-protocol.md` should say so explicitly,
because "the field names are part of the protocol" is otherwise read as "no field may be added",
and the distinction between *adding an optional field* and *renaming one* is exactly what a
versioning note is for.

**What each SDK must implement**, mirroring `sailantis/volucer-php-sdk`'s split
(`Worker`/`Request`/`Response`/`Frame`/`Header`/`MessageType`): all **fourteen** message types
(`HELLO`, `HELLO_ACK`, `REQUEST`, `RESPONSE`, `BODY`, `EVENT`, `LOG`, `METRIC`, `CONTROL`,
`CONTROL_ACK`, `END`, `PING`, `PONG`, `ERROR`), CRC-32C and the `no_checksum` negotiation,
body-chunk length enforcement with `LAST`, the "absorb unsolicited frames without derailing the
exchange" rule, and `VOLUCER_WORKER_SOCKET` support so Windows gets the socket transport.

**Then the adapters, which are the real work:**

- **Python**: `to_wsgi_environ()` building `REQUEST_METHOD`, `SCRIPT_NAME`, `PATH_INFO`,
  `QUERY_STRING`, `CONTENT_TYPE`, `CONTENT_LENGTH`, `SERVER_NAME`, `SERVER_PORT`,
  `SERVER_PROTOCOL`, `wsgi.url_scheme` (from the `https` flag, since the child has no socket),
  `wsgi.input` as a real file-like object, and `wsgi.multithread`/`multiprocess`/`run_once`.
  Sync WSGI first — one thread, one request in flight, which matches the protocol exactly. **ASGI
  is not a small extension**: concurrency inside one worker needs multiplexed requests on one
  connection, which is a protocol change (§10), so do not promise it in the first release.
- **Ruby**: `to_rack_env()` must produce a `Rack::Lint`-compliant env, including `rack.input`
  responding to `gets`/`each`/`read`/`rewind` and a body responding to `each`; and every dispatch
  must be wrapped in `Rails.application.executor.wrap` (or the `Reloader` in development, so code
  reload keeps working) or the connection pool is exhausted.

**Precedent, honestly stated.** Nobody has done this for volucer's protocol, and the nearest thing
is negative evidence: `pygoridge` is a Python Goridge client that exists and is **archived**, and
there is **no Ruby Goridge client at all** after six years of RoadRunner. That is a warning about
demand, and it is why Tier 1 comes first: it makes the runtimes work without depending on anyone
adopting a bespoke SDK. The counter-argument for Tier 2 is real though — it removes HTTP framing
from the hot path, and it is what Granian proves is viable for Python (a Rust server that defines
its own interface rather than making Python parse HTTP), the difference being that Granian embeds
CPython via FFI while volucer's whole design is to spawn it.

**Sizing.** Protocol change: S. Each SDK: M. Each adapter: M, and the Django and Rails ones are
where the framework knowledge is.

### 7.3 What not to do

- **Not FastCGI for Python or Ruby.** FastCGI exists here and would need no new code, but the
  Python bridges (`flup`) are unmaintained and Ruby's are legacy. Recommending a dead bridge
  because it is free is how a plan becomes technical debt.
- **Not an embedded interpreter.** `docs/plan.md` §1 rejected embedding PHP for reasons that apply
  verbatim to CPython and MRI (no C-level SAPI, lost isolation, a rebuild per version), and hazard
  53 measured the spawn path against the embedded one. The answer for Python and Ruby is the same
  as it was for PHP.
- **Not a protocol per language.** One protocol, three SDKs, and the §5 feature flag deciding what
  is compiled. A `python` backend kind would be a mistake for exactly the reason
  `RequestScope::OriginScoped` gives: volucer does not reach into somebody else's runtime.

---

## 8. Order, and why

Each step is a prerequisite of the next thing that needs it, and nothing later is started to make
progress on something earlier look bigger.

| # | Step | Depends on | Size | Why here |
| --- | --- | --- | --- | --- |
| 1 | **Metrics + status** | — | S–M | No calls, no driver, no protocol change; makes everything after it observable |
| 2 | **`CALL`/`RETURN` on the worker channel + `workers.list` / `worker.reset`** | 1 | M | The branch on inbound frames (§4.2) and the method registry. Two message types, one match arm, one `catch_unwind`, one deadline policy — and a control plane exists from the first commit |
| 3 | **KV: `memory`, then `redis`** | 2 | S, M | Smallest service on the call path; proves the registry shape |
| 4 | **Locks: `memory`, then `redis`** | 2, 3 (Redis client) | S, M | Shares the Redis client; carries the fencing warning |
| 5 | **Jobs: engine + `memory` + `redis`** | 2 | L | Biggest item; the consumer-side exchange is the interesting part |
| 6 | **OTLP export** | 1, 5 (spans across a push) | M | The layer plan.md §6 already designed |
| 7 | **gRPC Tier A** (trailers constructed, pass-through to a Unix-socket origin) | §7.1's `upstream` supervision | S–M | No protobuf crate, no worker involved, no new listener — hyper already speaks h2 here |
| 8 | **gRPC Tier B** (application as the handler, via `CALL`/`RETURN`) | 2, 7 | M | Reuses the in-flight request, so it does not touch the one-request invariant |
| 9 | **gRPC Tier C** (reflection) | 8, and §10 item 2 | M, optional | The only descriptor consumer; may legitimately be "not yet" |
| — | **Runtimes, Tier 1 (`upstream` supervision)** | — | S–M | Independent of 1–9 and can run in parallel. Highest value per unit of work in this document |
| — | **Runtimes, Tier 2 (protocol + SDKs)** | 2 | M each | Only after Tier 1 shows a measured reason |

Steps 1–2 are the ones worth doing regardless of how much of the rest is ever built: a metrics
endpoint and a control plane are what make a server operable, and neither requires believing
anything in §6 or §7. Step 2 is also the whole of the "should it be a socket?" question, and its
answer is now *no* — which means every later step is a registry entry rather than a transport.

Note the shape of the end of the list. gRPC is **no longer one XL item behind a gate**: it is three
items of S–M each, the first of which needs no schema, no worker and no new listener. The XL item
that remains — work delivered to a busy worker — is real and is §10 item 8, but nothing above it
depends on it.

---

## 9. Divergences, stated as divergences

RoadRunner is the reference, not the specification. Where copying it would copy a mistake, this
document says so, and the reason it can say so is that the mistakes are visible in its own source
and docs.

### 9.1 No runtime plugin container

RR's `endure` DI container and dynamic plugin registration are not reproducible in Rust without
giving up static dispatch, the MSRV job, `cargo package --workspace` and cross-compilation.
§5 is the substitute: a lifecycle trait, a service bag, and cargo features.

### 9.2 Locks are not a correctness primitive, and will not be advertised as one

RR's locks have TTLs and no fencing tokens. A late writer after a TTL expiry is not prevented, and
calling that a distributed lock without qualification is how an application acquires a data
corruption bug. volucer will ship locks, document exactly what they are for, expose
`set_if_absent` on KV as the primitive that *does* protect an invariant, and put the warning in the
README rather than only in the design document.

### 9.3 Metrics are on when configured, not when a middleware is listed

RR requires `http_metrics` in the middleware list before HTTP metrics exist, which means the
default observation of a fresh deployment is nothing. volucer emits them whenever `[metrics]` is
configured. The names stay RR-compatible; the activation rule does not.

### 9.4 Seven broker drivers is not a feature

RR ships AMQP, Kafka, SQS, Beanstalk, NATS, NSQ and Google Pub/Sub. Each is a large client
dependency, a set of offset and acknowledgement semantics to reproduce faithfully (Kafka's
consumer-group commits can skip unfinished records in a partition after a crash — from RR's own
docs), and a compatibility surface that would be the largest part of this project. volucer's
position: **one in-memory driver, one shared Redis driver, and a clean driver trait** so that a
broker nobody here runs is a contribution rather than a commitment. An application that needs
Kafka today needs `jobs.kafka`, and volucer should say that plainly instead of half-shipping it.

### 9.5 The call path is capability-scoped, and `config.describe` is redacted

RR binds `rpc.listen` to loopback with no authentication and exposes `rpc.Config`. §4.5 is
deliberately different: the registry is selected per backend, so a public HTTP worker does not get
`worker.reset`, and configuration is described with credentials elided by name. The difference is a
refusal rather than a warning.

### 9.6 gRPC carries bytes; it does not learn your schema

RoadRunner loads `.proto` at runtime and wires the descriptors into routing *and* reflection. That
makes the schema a server concern, and it is why its gRPC plugin is the heaviest in the tree. §6.6
takes the opposite position: **the server carries bytes, writes two header names, and never parses
a message.** Routing is a path string, the inner framing is length-delimited and therefore
schema-independent, and the descriptors are needed only by an optional reflection service. The
practical consequence is that a `.proto` change is an application deploy and never a server one,
and that no build step enters the server's dependency graph.

The `upstream`-origin tier of that design is also the honest answer for gRPC today: a runtime that
already speaks gRPC is reached the same way a Django or Rails app is (§7.1), and volucer does not
need to be in the message path to be useful.

### 9.7 The gate is the construction side of trailers, not gRPC itself

`docs/plan.md` §6 recorded "every call would look failed" as the reason gRPC was gated. That was
correct about *the code* and wrong about *the effort*, and the distinction is worth keeping
because it is the kind of claim that gets quoted long after the code has moved. What the tree
lacks is any code that **constructs** a trailer; what it already has is hyper surfacing them on
ingress (h1 and h2), a frame-preserving body wrapper, and a response path that hands an origin's
body to the client without conversion. So the residual risk is one small `Body` implementation and
the status mapping — not a hand-written HTTP/2 state machine, which is where nginx's two CVEs
actually live. §6.6 carries the citations.

---

## 10. Open decisions

1. **The service allow-list's granularity.** Tier B of §6.6 routes a gRPC call by its `:path`
   (`/pkg.Service/Method`). Is the allow-list a list of literal service names, a prefix table, or
   the same `path_prefix` routing used for HTTP — and does a call whose method is not in the list
   answer `UNIMPLEMENTED` or `NOT_FOUND`? The first is what a gRPC client expects from a server
   that does not implement a method; the second leaks whether the service exists.

2. **Reflection, and whether descriptors are ever loaded.** §6.6 makes runtime `.proto` parsing a
   property of one optional service (`protox` → `prost-types` → `ServerReflection`). The question
   that remains is whether that service is worth a protobuf dependency *at all*: without it, a
   `grpcurl` user supplies their own `.proto`, which is what they already do against every
   non-reflective server. Deciding "not yet" is a legitimate outcome and removes the only
   protobuf crate from the graph.
3. **How a plaintext gRPC origin says it speaks h2.** The mechanism is settled by measurement —
   `Connected::negotiated_h2()` is what makes hyper use its HTTP/2 client, and the connector already
   forwards that answer, so the code is a handful of lines. What is left is the *vocabulary*: a new
   configuration key on an `http://` or `unix:` origin declaring it speaks h2 with prior knowledge. The
   candidates are a `protocol` field beside `origin` (`protocol = "h2c"`), or inferring it from the
   route that selected the backend. The first is explicit and the second needs no key but makes an
   origin's protocol depend on where a request came from, which is the kind of implicit coupling this
   project has refused elsewhere. **One deployment decision, not an open technical question.**
4. **Streaming responses in the worker protocol.** `RESPONSE` declares `body_len` up front, so an
   application must buffer. That is fine for HTML and wrong for a large download or a
   Server-Sent Events endpoint. Is a chunked response (unknown length, terminated by `LAST`) a v2
   feature, and does it land before or after a work-delivery exchange?
5. **ASGI/Django Channels.** Multiplexed requests per worker is a protocol change, not an SDK
   feature, and it is the same change a gRPC call as *work* needs. Do they share one design?
6. **Redis as one dependency for three features.** KV, locks and jobs all want a Redis client, and
   three drivers sharing one connection pool is better than three pools — but it also couples their
   feature flags. Is `redis` one feature that enables all three drivers, or three features over one
   crate?
7. **`jobs.pool.command` vs the server's command.** RR lets the jobs pool override the server
   command so consumers are a different script from request workers. volucer's `[server]` has no
   single command — backends carry their own — so the question is whether `[jobs.pool]` is a
   `WorkerConfig` (a fifth backend, effectively) or a distinct table.
8. **Which runtime gets Tier 2 first.** Django's adapter needs more explicit cleanup than Rails'
   (Rails' `executor.wrap` does the connection work for you), so if the answer is "the harder one
   proves the protocol", it is Django; if it is "the easier one ships", it is Rails.
9. **Second unit of work, or multiplexing?** A gRPC call as work (§6.6) and ASGI (item 5) both need
   a worker to be handed something while it is busy. The cheapest shape is a second *exchange* —
   the same `CALL`/`RETURN` discipline pointed the other way, work in and answer out — and the
   expensive one is genuine stream multiplexing with per-stream ids. Which, and does it land once
   for both? Note this is the *only* thing left that contradicts one request in flight, and it is
   off the critical path for every gRPC tier in §6.6.
10. **Whose deadline is a kill?** §4.2 item 3 and its hazard: a call blocking past the request's
    deadline currently means the worker is killed. The policy (report the call as *unknown state* and
    keep the worker, versus kill) is a decision, not a detail, and §6.3's error variant depends on it.
