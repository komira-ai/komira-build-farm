# Updating a frontend or an all-in-one host behind Envoy, without failing a build

Status: design, not implemented (the handover code it builds on, #857, #879 and #887, is merged on arch-v2). One update process for every shape: Envoy stays in front
of kbf on the host (also on a single box, also for `kbf all-in-one`); a new kbf process is
started on other ports; Envoy sends **new** connections and streams to it only once it is
ready and serving; the old process keeps answering what it already holds, then fails its
health check, finishes and exits. This replaces the first version of this note, which moved
the listening sockets between processes by fd passing, and an intermediate version that
parked requests while the new process caught up. **No request is parked, queued or refused by
a mode or a flag. Section 5.3 states exactly what a call can wait for: a call that lands in the
switch is held by its own open call for at most the switch time `T_s`, a measured gate. That is
a residual hold, stated as one, and whether it is acceptable is the owner's question (Q1).**

File references are to `origin/feature/arch-v2` (symbols are the authority, line numbers
rot). "Inference" marks a claim read from code shape or from Envoy's documentation and not
yet from a test; section 12 lists every inference the spike must turn into a measurement.

## 1. Goal and contract

1. Builds already running finish on the old process.
2. New requests go to the new process, from the moment Envoy sees it ready.
3. The old process exits when it holds nothing.
4. Envoy updates by its own hot restart (section 7), under the same three rules.
5. No phase of an update parks, queues or refuses a client request, and no flag or mode holds one.
   The one residual is a call that lands in the switch: it waits inside its own open call for at
   most `T_s` (gate: p99 under 1 s). That is a bounded hold, not an absence of one (section 5.3, Q1).

No client build may fail because of an update. Buck2 sends one `Execute` per action,
retries only the opening of that stream (5 attempts, sleeps of about 0.1, 0.2, 0.4 and 0.8 s
plus up to 50 ms), never calls `WaitExecution` or `GetOperation`, and fails an action whose
stream breaks after it started (`remote_execution/oss/re_grpc/src/client.rs`, read on
`main`; upstream facebook/buck2#1569 is open; the farm's record is kbf #791). So the old
process must answer every `Execute` it accepted, to the end, and Envoy must never cut one.

What each party sees:

| party | during a kbf update |
|---|---|
| Buck2 | Its connection to Envoy never changes. Open `Execute` streams continue on the old process. A call that Envoy still sends to the old process after the successor took the roles is answered through the old process (section 5.2), not refused. No GOAWAY comes from kbf: Envoy keeps the downstream connection (section 8). |
| daemons (remote, and the in-process one of all-in-one) | The old process ends their streams. They redial the same worker address, which is Envoy's, and reach the successor. Leases are kept (the epoch is kept) and nothing is double-booked (section 6). A redial that reaches the old process in the short window before it goes not-ready is refused and retried by the daemon's own loop; a daemon is not a client call. Together with the `unready` settle this window must stay well under the 40 s fence (section 6.2, measured in the spike). |
| operator | `kbf handover` starts the other instance and, with `--wait`, polls the old one until it is gone and shows `draining: N streams, N relays, oldest M min`. Operator reads (`/readyz`, `/v1/...`) are by instance on loopback; the public view is Envoy's. |

SQLite with the single-writer lock, `Store::demote` and the generation fence is enough for
the scheduler state; no Postgres. (Postgres has no `demote`, `kbf-state/src/store.rs`, and is
for two serving frontends.)

Storage nodes and daemon nodes of split mode are not part of this sequence: the frontend
updates each of them when it is idle or drained, in any order, under the N / N-1 rule of
section 9. This note covers the frontend, and the two halves an all-in-one process has in
addition (storage and daemon).

## 2. What was re-checked, and where the code disagrees with the brief

| claim | result | where |
|---|---|---|
| kbf sends nothing on a long Execute and has no HTTP/2 keepalive | **Not true on arch-v2.** Every open `Execute` and `WaitExecution` stream is re-sent its operation every `--progress-interval-secs` (default 30), and both listeners send an HTTP/2 PING every `--http2-keepalive-interval-secs` (default 30, timeout 20). Progress frames are data frames, so they reset Envoy's stream idle timer; a PING is connection-level and does not (inference). The Envoy overrides below stay, because both flags can be 0. | `kbf-server/src/config.rs` (`progress_interval_secs`, `http2_keepalive_*`), `kbf-front/src/execution.rs` (`progress`), `kbf-server/src/serve.rs` (`StreamTiming`, `keepalive`) |
| the longest action is 1 h | **Not a bound.** The container driver's default timeout is 1 h but an action's own `timeout` replaces it and is not capped. The envoy kit's 3900 s (1 h plus the 300 s unservable wait) is the default case only. | `kbf-driver-container/src/runtime.rs` (`timeout_of`, `default_timeout`), `crates/kbf-deploy/src/envoy/check.rs` (`SILENT`) |
| #857 is merged and stays needed | Yes: `Fenced`/`meta/generation`, `Store::demote` (SQLite), `done/<op>` rows with `DONE_FLOOR` (600 s); the old process answers its open streams from `Finished` rows it can read without knowing how the result was made. | `kbf-state/src/fence.rs`, `store.rs`, `sqlite/mod.rs`; `kbf-server/src/farm/durable.rs`, `kbf-front/src/finished_row.rs` |
| the handover halves of #857/#879/#887 | **Merged on arch-v2.** `Farm::quiet/fence/end_worker_streams/follow` (`farm/handover.rs`), the `handover/` module (control socket, `SCM_RIGHTS` fd passing, `READY=1` notify), `StateStoreUri::open_after_handover(generation)` which opens with `EpochPolicy::Keep`, and `kbf-frontend@` units. `StateStoreUri::epoch_policy` (the plain `open`) still returns `Bump` for every SQLite start, so an all-in-one start has no `Keep` path until it is given one. `Farm::fence` is one-way: there is no unfence. Section 11 says what is reused. | `kbf-server/src/farm/handover.rs`, `kbf-server/src/handover.rs`, `kbf-server/src/state_store.rs`, `kbf-server/src/farm/durable.rs` |
| `Drain::begin` is one step | Yes: it turns readiness off, refuses new Executes and sends GOAWAY in one call, once. Section 5.1 replaces it with a staged object. | `kbf-server/src/drain.rs` |
| **the frontend can answer a call for another process** | **Only storage.** `kbf-front/src/proxy` forwards CAS, ByteStream and action-cache calls to storage nodes; there is no Execute or WaitExecution forwarder. Section 5.2 needs one. | `kbf-front/src/proxy/mod.rs` (module doc: storage calls only, GetTree not yet; Execute is the scheduler's call and reaches storage through `Backend::routed`) |
| the meta log can be read by a second process | **No.** Single writer by flock, never compacted, a directory holding a snapshot is refused; every start replays the whole log. There is no follower open. | `kbf-server/src/raft_log/mod.rs` (`Locked`, module doc "Not yet: snapshots"), `docs/design/metadata-snapshots.md` |
| the hot tier can be opened by a second process | **No.** Open deletes every block file no staged object names, so a second opener removes the first's active blocks. | `kbf-tier/src/open.rs` |
| a write call is answered only when durable | Yes: a ByteStream `Write` or `BatchUpdateBlobs` buffers (admitted by bytes), is grouped into a segment, PUT, committed by one `PutBlobs`, and only then answered. | `kbf-front/src/cache/write_group.rs` |
| `/readyz` and the gRPC health service agree | Yes: both evaluate `Checks::failing`; `Readiness::stop` flips both; `Readiness::set_leader(false)` exists for a process that is up but must not be picked. | `kbf-server/src/health.rs`, `grpc_health.rs` |
| kbf-deploy renders an Envoy config | Yes, for a **Kubernetes client front**, not a box: one cleartext h2c listener behind a Tailscale ingress, one STRICT_DNS cluster of servers over TLS to the farm CA, route timeout 0, stream idle timeout 7200 s, no `max_stream_duration`, `healthy_panic_threshold` 0, gRPC health check (5 s interval), a checker that models those properties. It has no circuit breakers, no retry policy, no worker route, no hot restart, and refuses a route whose action has anything but a cluster and timeouts. Section 4 builds on it. | `crates/kbf-deploy/src/envoy/{render,check,config}.rs` |
| `/readyz` is only the flags | **No.** `Checks::failing` also runs a store probe (a read of the probe digest through the cache, `STORE_PROBE_TIMEOUT` 2 s) besides the `stopping` and `leader` flags. After the cut that probe would run through `HandoverStorage` or a frozen store, so section 5.1 pins `p`'s readiness explicitly. | `kbf-server/src/health.rs` (`failing`, `StoreProbe`) |
| a cordon is durable | **Yes for an operator's cordon.** `farm/durable.rs` keeps a `cordon/<worker>` row (written by `Durable::cordoned` on cordon, drain and uncordon) and recovery reads it back (`cordons`, `restore_cordons`), so a restarted process serves a node as the operator left it. The `kbf-sched/src/cordon.rs` module doc still says "gone after a server restart" and is stale. **Still new for this design**: the takeover writes the predecessor's cordon row, and deletes the successor's own node id's row, in the takeover's state-store transaction (section 6.2); today no takeover path writes a cordon. | `kbf-server/src/farm/durable.rs`, `kbf-server/src/farm/handover.rs`, `kbf-sched/src/cordon.rs` |
| the Welcome epoch is the state store's | The Welcome carries `Farm::lease_epoch` ("this process's term, or the Raft log's id"); `SqliteStore::open` also bumps its own recorded epoch. Which one `Keep` must hold constant is checked in T-7 (section 6.2). | `kbf-server/src/worker.rs`, `kbf-state/src/sqlite/mod.rs` |
| the REAPI listener advertises unlimited streams | **No.** `max_concurrent_streams` is `StreamTiming`'s `streams`, at most `MAX_STREAMS` (`i32::MAX / 65536`, 32767); the configured value is what Envoy sees. Section 3.1 derives the breaker from it. | `kbf-server/src/serve.rs`, `streams.rs` |
| a real Envoy is available to tests | Yes: CI fetches a pinned Envoy and validates the kit's render with `--mode validate`. | `.github/workflows/ci.yml`, `crates/kbf-deploy/tests/envoy_validate.rs` |

## 3. Topology on one box

One Envoy owns the public ports. kbf instances `a` and `b` listen on loopback port blocks.
Envoy's configuration is **static and lists both instances for ever**; a kbf update changes
no Envoy file and never reloads Envoy. Routing changes by health, nothing else.

```
 Buck2 --TLS--> Envoy :reapi --h2/TLS(CA)--> cluster kbf_reapi  { a:reapi, b:reapi }
 daemons --mTLS--> Envoy :worker --tcp-----> cluster kbf_workers { a:worker, b:worker }
 operators ------------------------------------> instance API on loopback (a or b)
 Envoy admin: loopback only
```

**Ports.** The public REAPI and worker ports are the ones clients and daemons dial today and
move to Envoy, so no client or remote daemon changes. Each instance gets three loopback ports
from one configured base: `a` base+0 (REAPI), +1 (worker), +2 (API); `b` base+10, +11, +12.
They are fixed at install time because the Envoy file is static; an ephemeral port would need
a dynamic endpoint source. `install-units` refuses a base whose six ports collide with each
other, with Envoy's, or with a listener already bound.

**REAPI listener (L7).** Envoy terminates the client-facing TLS (REAPI clients present no
certificate, `--reapi-tls-cert` doc in `kbf-server/src/config.rs`; the REAPI policy runs in
kbf and reads no peer certificate) and routes **per stream**. L4 would route per connection and
leave a client's long-lived connection, and every new stream on it, on the old process, which
is what the GOAWAY of the first design was for. Upstream is h2 over TLS to the farm CA, with the
kit's per-host SNI and SAN match. Routes are the kit's `ROUTES`.

**Worker listener (L4).** `tcp_proxy` to `kbf_workers`, no TLS in Envoy: the worker listener
is mutual TLS and `peers.admit` matches the client certificate's node id to `Hello.node_id`
(`kbf-server/src/worker.rs`); terminating it would lose the identity. Remote daemons keep
dialing the one address they dial today. The in-process daemon of all-in-one dials the same
public address (`--server=https://<host>:<worker port>` in
`templates/linux/modes/kbf-all-in-one.service`), so after it detaches it reaches the successor
the same way. Routing the worker port by instance number directly (no Envoy) was rejected: every
update would change the address a remote daemon must use.

**Clusters.** One cluster per listener, both instances in it, equal priority. Both are
healthy together for a few seconds during every update, **on purpose**: the old instance is
kept ready until Envoy has seen the successor healthy, so there is never a moment with no
ready backend and never a local 503 from Envoy; what the old instance cannot serve itself in
that overlap it answers through the successor (section 5.2).

**Health.** The signal is kbf's readiness, which is `/readyz`:

- `kbf_reapi`: the kit's existing `grpc.health.v1` check on the REAPI port, empty service
  name. It is `/readyz`'s evaluation (`grpc_health.rs`); reusing it keeps the checker and the
  kbf-it test that pins the service names (`crates/kbf-it/tests/envoy_check.rs`).
- `kbf_workers`: an HTTP check of `/readyz` on the instance's API port (`health_check_config.port_value`),
  plain loopback HTTP, since the worker cluster has no TLS of its own. `--api-listen` is already in
  the unit and is required here.
- Envoy's own view of the flip is read from its admin interface on loopback
  (`GET /clusters?format=json`; inference, the spike checks the field names). The JSON has no
  "healthy" field: a healthy host shows no failed flags, and a host not yet checked shows
  `failed_active_health_check` and `pending_active_hc`. So `i` needs **positive evidence**: in both
  clusters the host is present, neither flag is set, and two consecutive reads at least one check
  interval apart agree. A parser that treats "no failed flag" as healthy is a mutant (E6). The
  admin socket of an Envoy hot restart answers for one epoch only; a handover started while an old
  epoch is alive waits for that epoch's drain to begin, or is refused (section 7). The settle of
  section 5.1 is **derived**, `interval x unhealthy_threshold + timeout` plus one interval, not a separate
  default, and is the fallback when no admin address is configured. `--envoy-admin` is a flag
  of `kbf frontend` and `kbf all-in-one`, loopback only.
- The worker cluster's check is `/readyz` on the API port, which is a separate unauthenticated route
  (`health.rs`); `check` and a kbf-it test pin that it answers without a token. It does not prove the
  worker listener accepts mTLS, so the worker cluster also gets a TCP connect check on the worker
  port, and a worker listener that fails to bind folds into `Checks::failing` as a `worker` flag.
- `interval` 1 s, `no_traffic_interval` 1 s, `timeout` 1 s, `unhealthy_threshold` 2, `healthy_threshold` 1.
  The kit's 5 s and 10 s are for a remote cluster; here they would be the update gap
  (section 5), and a loopback check is free. Starting values: the spike sets them.

### 3.1 The Envoy settings that keep a long Execute alive

Each row is a default that would cut a stream (or a limit that would refuse one), what the
kit sets today, and what the box render sets. "Check" is a rule `envoy check` must enforce, with a
mutant (section 10).

| setting | Envoy default | kit today | box render | check |
|---|---|---|---|---|
| route `timeout` | 15 s | `0s` on every route (`ROUTE_TIMEOUT`) | `0s`, REAPI routes | kept (`route-timeout`) |
| route `max_stream_duration` / `idle_timeout` | none / the HCM's | absent | absent | kept (`max-stream-duration`, `idle-timeout`) |
| HCM `stream_idle_timeout` | 300 s | `7200s` (`LONG_STREAM`) | `7200s` for every route except `Execute` and `WaitExecution`; those two routes carry route-level `idle_timeout: 0s` (overrides the HCM value), because an action's own timeout is uncapped and can exceed 2 h, and a stalled progress task or `--progress-interval-secs=0` would otherwise cut it. `kbf frontend` and `kbf all-in-one` refuse `--progress-interval-secs=0` when `--instance` is given. | kept (`idle-timeout`), plus **new** `execute-idle-off` |
| HCM `request_timeout` | disabled | absent | absent: a 64 MiB write streams | kept (`request-timeout`) |
| cluster `close_connections_on_host_health_failure` | false (and `tcp_proxy` only) | absent | false on both clusters. A failed active check removes the host from selection; Envoy then drains the host's HTTP connection pool gracefully (no new streams, open ones run to completion; inference, E4 asserts it on the pinned Envoy: no RST_STREAM and no error GOAWAY reaches the upstream of an open stream). The flag is documented for `tcp_proxy` only. | **new** (`health-keeps-streams`) |
| cluster `healthy_panic_threshold` | 50 % | `0` | `0`: with both instances unready Envoy answers 503, it never sends to a fenced one | kept |
| circuit breakers `max_requests`, `max_connections`, `max_pending_requests`, `max_retries` | modest: 1024 connections per cluster is documented; the others, 1024 and 3, are from the cluster reference and are not re-checked here | **absent**: one open Execute is one request, so about 1000 concurrent actions would be refused with an overflow (the same gap exists in the Kubernetes front) | raised on the DEFAULT priority threshold (a HIGH one would not apply) to at least the farm's concurrent-action ceiling (a config key, default 65536); the REAPI listener advertises a finite `max_concurrent_streams` (section 2), so Envoy opens one upstream connection per that many streams and `max_connections >= ceiling / max_concurrent_streams` | **new** (`breakers`, both inequalities; E5 holds more silent streams than the product) |
| `tcp_proxy` `idle_timeout` (worker) | 1 h | n/a | `0s`: a daemon's session stream is quiet between heartbeats and must not be cut | **new** (`worker-idle`) |
| upstream/downstream connection idle, `max_connection_duration` | 1 h with no streams / none | default | default (only idle connections close) | refuse a `max_connection_duration` |
| HTTP/2 upstream keepalive | none | none | `connection_keepalive` 30 s / 20 s, as kbf's own listeners | **new** |
| route `retry_policy` | none | none; the checker refuses any extra action field | `retry_on` `connect-failure,refused-stream` only, on `Execute` and the read-only unary calls. It covers a connection that fails before a stream starts (for example an old instance that has just exited); it is not a hold and no part of the update depends on it | **new** (`retry-routes`) |
| HCM `drain_timeout` | 5 s | default | default (it times Envoy's own GOAWAY, section 7) | none |

## 4. Building on kbf-deploy's envoy kit

No parallel renderer. `envoy/render.rs` already holds the parts to reuse: `ROUTES`,
`ROUTE_TIMEOUT`, `LONG_STREAM`, `upstream_tls`, the cluster builder, the JSON writer that
makes equal settings equal bytes, and `check.rs` with its route model. Changes:

1. `render` gets a topology: `Front` (today's Kubernetes objects) and `Box`. `Box` writes
   only `envoy.json` (no manifests), with the two listeners, two clusters, admin on loopback.
2. `Server` is keyed by host **and port** (today a host named twice is `TwiceNamed`, and the
   TLS socket match is keyed by host); both instances share the box's name and differ in port.
3. New config keys for `Box`: the public ports, `KBF_BACKEND_PORT_BASE`, the client-facing
   certificate and key, the breaker ceiling and the longest-action bound (section 7).
   The closed-key-set rule of `config.rs` stays.
4. `check` gains the rows marked new in 3.1 and relaxes one rule: `RouteAction` may carry a
   `retry_policy`, but only on the routes named in 3.1, and only with `retry_on` in the closed set
   `connect-failure,refused-stream`.
   `check` also verifies that both instances appear in both clusters with disjoint port blocks.
5. The hot-restart unit and its start script (section 7) are rendered by `kbf-deploy linux`,
   next to the kbf units.

The Kubernetes front keeps its own rollout for Envoy (a Deployment rolls its pods); hot restart is
the box path. The settings it lacks (breakers; the retry policy is box only) are fixed
in step 4.


## 5. The update sequence

Two instances of one unit template, `kbf-frontend@a|b` or `kbf-all-in-one@a|b`, each with its
own port block (a drop-in that `install-units` writes per instance, since configuration is
flags). Instance letter `i`, predecessor `p` (the other letter).

### 5.1 Drain is staged, and readiness goes last

`Drain::begin` splits into two one-way stages, each idempotent, in `kbf-server/src/drain.rs`:

| stage | what changes | reversible | by |
|---|---|---|---|
| `hand-over` | `p` starts answering every **new** call through `i` (5.2); scheduler quiesced and fenced; worker streams ended; state store demoted; (all-in-one: the storage block of 6.3). **Readiness is pinned ready**: a `handed_over` flag makes `Checks::failing` skip the store probe (which would run through `HandoverStorage` or a frozen store, up to its 2 s timeout, and could fail during `T_s`) and skip the `worker` flag; only `stopping` and `leader` are evaluated. A watchdog (below) is the only thing that may turn `p` unready besides `i` and SIGTERM. | no, after the fence | `i`, over the API |
| `unready` | `Readiness::stop`: `/readyz` and the gRPC health answer not-ready. Envoy stops choosing `p` for new streams and connections; open streams are untouched (section 3.1). | yes, until `p` exits | `i`, after Envoy shows `i` healthy (or the derived settle as the fallback); the watchdog; SIGTERM |

SIGTERM and `POST /v1/frontend/drain` without a handover body keep today's meaning: `Drain::begin`
as it is, readiness off, new `Execute` refused, GOAWAY to the listener (`api.rs`, `FRONTEND_DRAIN`).
A kbf that is stopped, not updated, still drains the old way; it is not an update path.
**Watchdog.** After `released`, `p` probes `i`'s API and REAPI listeners every second. If `i` is not
serving within `--handover-watchdog-secs` (default 15, above the measured `T_s` plus the start
timeout of the roles latch, below the fence), or either listener becomes unreachable, `p` goes
`unready` on its own, so Envoy has a clean 503 to answer and Buck2's open retries apply, rather than
`p` staying chosen while it refuses everything. This replaces "only `i` can send `unready`". A failpoint
test kills `i` after `released` and asserts `p` flips; the mutant is a watchdog that never fires.

There is no `goaway` stage: `p` has no downstream connection to tell (Envoy owns it), and a
host Envoy no longer picks gets no new stream.

**The rule that removes every gap: `p` turns not-ready only after `i` is ready and Envoy routes
to it.** The two are healthy together for a few seconds. At every instant at least one backend
that can answer any call is healthy, so Envoy never has to answer with its local 503 and the
client never sees an update.

### 5.2 `p` answers for `i` while Envoy still sends calls to `p`

After `hand-over` and until Envoy flips, `p` still receives calls, because it is still ready.
It does not refuse them (a refusal is a request the client must retry) and does not hold them
(5.3). It answers them **through `i`**:

| call after the cut | what `p` does | code |
|---|---|---|
| `Execute`, `WaitExecution` (new) | opens the same call to `i`'s REAPI listener over the farm-CA TLS with the node's identity, forwards the request metadata, relays the response stream. `i` authorizes again. A duplicate `Execute` joins the same action (identical hermetic actions are joined in the scheduler), so a client that is later routed to `i` for the same action is safe. | **new**: an Execute relay beside `kbf-front/src/proxy`, which today forwards storage calls only |
| `FindMissingBlobs`, `GetActionResult`, `GetCapabilities` | `i` (authority questions are never answered from frozen state) | `HandoverStorage` (6.3) |
| `BatchReadBlobs`, `ByteStream.Read` | `p`'s frozen state first, then `i` | `HandoverStorage` |
| `ByteStream.Write`, `BatchUpdateBlobs`, `UpdateActionResult` | `i` (a write that is already open at the cut finishes on `p`, 6.3) | `HandoverStorage` |
| worker session of a daemon | refused UNAVAILABLE: `p` has no scheduler; the daemon's own redial loop reconnects, through Envoy, to `i` or to `p` again until it goes not-ready. Together with the case "`i` is dead and the watchdog has not fired yet" (5.1) this is the only refusal in the sequence, and the first is to a daemon, whose fence is 40 s. | `kbf-server/src/worker.rs` |

**The relay's own failure.** `Execute` through `i` returns the operation name `i` assigned; `p` keeps it with
the relayed stream. If the stream to `i` breaks (restart of `i`, a `Bump`), `p` falls back to following that
operation from the `done/` rows as it does for its own streams, using the operation id, so a relay is not a
weaker class of stream than a call served by `p`. A relayed `Execute` keeps `p` alive for as long as the
action runs, so relays are counted (`kbf_handover_relayed_total`, and `draining: N relays` in `kbf handover`)
and `kbf handover` refuses the next update while any is open. The settle is short (derived, section 3) to keep
the number small; E6 asserts a relay count against the settle.

Calls already open at the cut are not moved: an `Execute` or `WaitExecution` stream stays on
`p`, which answers it from the `done/<op>` rows `i` writes (5.4 step 5) until it ends; a read
in progress finishes from frozen state.

**Why a relay and not readiness-first or a refusal.** Turning `p` not-ready at the cut leaves
Envoy with no healthy backend until `i` is up and seen (about the takeover time plus one check
interval), and Envoy answers that with a local 503 it does not retry (inference); Buck2 retries
an `Execute` it could not open for only about 1.5 s. A trailers-only UNAVAILABLE from `p` that
Envoy retries onto `i` is the same wait with a retry timer in the middle (the client's call is
delayed until a retry lands) and needs a retry policy on `Execute` that must be correct for a
long stream. The relay needs one new component and no timer.

### 5.3 What a call can wait for (no request is held)

There is no queue mode, no park, no admission gate and no `--handover-park-secs`. **That is not "no call
waits"**: a call that lands in the switch waits, inside its own open call, on `i`'s roles latch for up to `T_s`.
It is a bounded hold with no object that holds it, and it is the owner's call whether that satisfies "no request
is held" (Q1). A call is only ever: served by `p` as usual (before the cut, or opened before it); relayed to `i` (5.2);
or served by `i` (once Envoy routes there). The residual wait is one thing:

> **Switch time `T_s`**: from the route flag at `p` to `i` holding every role and having
> caught up (components in 6.3: `p`'s last meta batch, lock handoff, `i`'s catch-up and tier open,
> scheduler recovery; not `p`'s segment PUTs, 6.3). A relayed call that reaches `i` inside `T_s` is an open call on `i` whose
> handler has not started yet, exactly as any call is slow at a process that has just started
> serving. Nothing is stored, nothing is bounded by a flag, the call's own deadline governs.

`T_s` is made small by moving everything slow in front of the cut (`i` starts, loads and
warms before step 1; section 6.3 lists what each store can do in advance) and is **measured, not
assumed**: `i` records it per start (`kbf_handover_switch_ms`, and `handover/switch_ms` in the
state directory), `kbf handover` refuses to start an update when the last recorded value is above
`--handover-switch-budget-ms` (default 1000, a starting value for the spike) or absent
(`refused switch-too-long`, `refused unmeasured`), and the spike (section 12) must show a p99 under
the budget on a realistic host before any PR that cuts a store lands. If it does not, that store
is not handed over in place, and the answer for that host is split mode (section 15, Q1).

The honest limit: a single-copy store with an exclusive writer lock has a first instant at which
`p` no longer commits and a later one at which `i` does. Nothing in an update can make that interval
zero; this design aims at a few hundred milliseconds inside one call, gates on a measurement, and says so.

### 5.4 Sequence

0. `kbf handover` (or `systemctl start` of the idle instance). `i` does all its slow, lock-free
   work first (certificates, flags, runtime checks), binds its loopback ports, serves `/readyz`
   not ready (`Readiness::set_leader(false)`), and warms what it can (6.3). Its handlers for
   `Execute` and the storage calls wait on a roles-held latch. It cannot take the state store:
   the file lock is held.
1. `i` calls the predecessor's API (`POST /v1/frontend/drain`, `{"handover": {"peer": ...}}`,
   loopback peer, bearer token: the existing gates of `api.rs`; the port is `p`'s, from the port
   table). `p` runs the **prepare** checks, all reversible (6.3: disk, `T_s` budget, tier seal of
   the reversible part). A refusal (a different state path or address table, both instances
   active, a failed check) answers with a reason, `i` exits non-zero, `p` has not changed.
2. `p` runs `hand-over`: forwarding on, quiesce, fence, end worker streams, (storage block),
   demote the state store. Its answer is `released` with the generation it held.
3. `i` takes the next generation, `Fenced::take(store, Some(generation))`, with the epoch kept
   (`EpochPolicy::Keep`, reached today through `StateStoreUri::open_after_handover(generation)`, which `frontend.rs` calls; the plain
   `open` and `epoch_policy` are `Bump` for SQLite, so all-in-one calls the handover open; the lease epoch in the Welcome is held constant, section 6.2), recovers, takes the storage
   roles in the order of 6.3, opens the latch, `set_leader(true)` and says `READY=1`. From the latch
   opening `i` serves everything directly.
4. Envoy marks `i` healthy within one check interval. `i` reads that from the admin interface
   (both clusters, positive evidence, section 3), waits the derived settle, and calls `p`: `unready`. Envoy
   stops choosing `p` within `interval x unhealthy_threshold + timeout`; calls that raced the flip
   are relayed (5.2), not refused.
5. `p` follows its open `Execute` and `WaitExecution` streams from the `done/<op>` rows `i`
   writes, until each ends (`kbf_frontend_relay_lost_total` counts one it cannot find three times
   in a row: the only way this path fails a build, and it must be zero in every test but the
   planted one). `p` exits when no stream is open (its own and its relays), no `Result` is
   unacknowledged and, for all-in-one, no lease is held.

If `i` never becomes ready before step 2 nothing has changed. If `i` dies after `released`, `p`
still answers its own streams from the rows; the watchdog (5.1) turns `p` unready within its bound, Envoy
answers a clean 503 and Buck2's open-time retries apply; systemd restarts `i` as a cold start, which bumps
the epoch, so the daemons rerun their leases: slower. A relayed stream falls back to the `done/` rows by operation id (5.2)
and breaks only if `i` never recorded the operation; a call that had not opened a stream is retried by Buck2.
That window (up to the watchdog bound plus a cold start, which replays an unbounded meta log) is the one committed
failure that can fail a build; it is in the failure table.

**Why the same process for a frontend and all-in-one.** The sequence, ports, health flip, relay
and Envoy file are identical; all-in-one adds the storage and daemon halves of section 6.
`kbf frontend` and `kbf all-in-one` call one `handover::Driver` with a list of cut stages (section 11).

## 6. All-in-one: what the process still has to hand over

Envoy removes the listener transfer. It does not remove the problem that scheduler, storage and
daemon share a process. Everything below is kept from the first version, with the listener,
GOAWAY and park parts rewritten.

### 6.1 Blockers (re-checked on arch-v2)

| # | blocker | evidence |
|---|---|---|
| B1 | A running action dies with its daemon process: the keeper holds a pipe only the daemon holds. So the old process lives until its leases end. | `kbf-daemon/src/keeper.rs` (the container driver runs actions with crun only; Podman is dropped) |
| B2 | The meta log is single-writer by flock; there is no read-only open, no follower open and no `demote`; it is never compacted, so a start replays all of it. The answer is the follower of 6.4. | `kbf-server/src/raft_log/mod.rs` (`RaftLogError::Locked`) |
| B3 | The cold `file://` store takes an exclusive `LOCK`. | `kbf-objstore/src/fs_store.rs` (`FsStoreError::Locked`) |
| B4 | The hot tier removes block files no staged object names at open. | `kbf-tier/src/open.rs` |
| B5 | The daemon half dies with the server half: `with_daemon` ends the daemon when `serving` ends. | `kbf-all-in-one/src/run.rs` |
| B6 | The in-process daemon's data plane is `LocalDataPlane` over the old process's cache. The control plane is a loopback mTLS worker session. | `kbf-all-in-one/src/plane.rs`, `kbf-node` |
| B7 | Two daemons on one host collide: the start sweep removes every container labelled with the node id and every lease directory; the `daemon-instance` claim; container names; the cache dir lock. | `kbf-driver-container/src/runtime.rs` (`sweep`), `kbf-node/src/instance.rs` |
| B8 | The scheduler books by worker (`free() = capacity - booked`); two workers on one box would double-book it. The predecessor's worker is kept out of placement by a cordon. Cordon rows are durable on arch-v2 (section 2) but no takeover writes one, so today a restart of `i` with `p` still detached would register `p`'s daemon uncordoned at full capacity. | `kbf-sched/src/scheduler.rs`, `cordon.rs` |
| B9 | All-in-one starts with `Bump`; a bump makes every daemon kill its runs. | `kbf-server/src/state_store.rs` |
| B10 | `Drain` is one-way and `Farm::fence` has no unfence (merged, `farm/handover.rs`). Section 5.1's staging puts the reversible work (prepare) in front of the fence. | `kbf-server/src/drain.rs` |
| B11 | The detached daemon fences itself after 40 s without an acknowledged heartbeat; the successor's `LEASE_GRACE` is 60 s. | `kbf-daemon/src/config.rs` (`FENCE_AFTER`), `kbf-sched/src/fence.rs` |
| B12 | The scheduler's recovery reads the state store, which `p` holds locked until `hand-over`; its time is part of `T_s`. | `kbf-state/src/sqlite/mod.rs`, `kbf-sched/src/scheduler/recover.rs` |

### 6.2 The detached daemon

- **Process shape.** After `released` the server half stops serving as scheduler and storage but the
  process does not exit; the daemon half runs on as a detached worker (B1, B5). The process exits when
  the conditions of 5.4 step 5 hold. A second SIGTERM ends open streams and abandons leases, which fails the
  builds whose streams `p` holds; it is the operator's explicit choice.
- **Control plane.** No new mechanism. The detached daemon's stream is ended by `hand-over`; it
  redials `--server`, which is Envoy's worker address, and reaches `i`. Between `hand-over` and
  `unready` a redial can reach `p` again (it is still ready); `p` refuses worker sessions in that window
  (5.2) and the daemon's redial loop (`--reconnect-ms`) converges. The time from the cut to `i` serving worker
  sessions, with the daemon's backoff, is a spike item and part of the `T_s` check: it must stay well under the 40 s fence. Same node id and instance id, so the
  scheduler sees the same process; it resends its Hello and unacknowledged Results before its first
  heartbeat, and keeps its runs only if the Welcome epoch equals its leases' epoch, hence `Keep` (B9). The Welcome epoch is
  `Farm::lease_epoch`, not the SQLite store's own epoch, which every open bumps; the `Keep` path must hold
  `lease_epoch` constant across the SQLite open, and T-7 asserts the Welcome epoch is unchanged across a handover.
- **Data plane.** At detach the daemon's data plane switches from `LocalDataPlane` over `p`'s cache to the
  remote plane every remote daemon uses, aimed at `i`'s REAPI listener (B6). An action that finishes after
  the cut uploads its outputs to `i` and reports its `Result` once; nothing writes into `p`'s stores
  after the demote.
- **Lease adoption, not double booking.** The successor's recovery stores each lease
  `Held{booked, session: u64::MAX}` with `awaited[worker] = now + LEASE_GRACE`, and adopts it when
  the worker registers under the same id (`kbf-sched/src/scheduler/recover.rs`, from the first
  version; re-read before the PR).
- **The successor's daemon is a different worker.** `--node-id=<host>-<instance>`, one certificate
  per instance, per-instance daemon directory `<data-dir>/node-<instance>/` only when `--instance` is
  given (existing hosts keep their layout); container lease terms and cgroup names carry the
  instance (B7). Work, not a flag: audit `runtime.rs` for any name that is not owner-scoped.
- **Cordon (row and recovery exist; the takeover write is new), and its reset.** The `cordon/<worker>` row, its
  encoding and its recovery are merged (`farm/durable.rs`, `restore_cordons`; #879). New here is the write in the same
  state-store transaction as the takeover, and the delete of the successor's own row (PR 2, section 14).
  Recovery applies the row before the worker listener accepts a session, so the detached daemon of `p` never
  registers uncordoned, including after a crash and restart of `i`. T-7 kills and restarts `i` with `p` still detached
  and asserts `p`'s worker is granted nothing. Node ids alternate (`-a`, `-b`, `-a`), so in the same transaction as the takeover the
  successor **deletes the cordon row of its own node id** and writes the predecessor's; when the
  predecessor exits its worker and cordon rows are deleted. A takeover clears a manual cordon on that
  node id; the operator cordons again.
- **Capacity.** The successor's daemon registers `total - booked_by(predecessor)` and resends its
  Hello as the predecessor's leases end, driven by lease-end events it observes, never a timer, and
  never above `total`. Open: whether two units together can exceed the box through the per-unit
  cgroup caps (Q4); the capacity PR does not ship before it is answered.
- **Daemon cache.** The detached daemon stages no new inputs, so it releases the cache dir lock at
  detach; the successor's daemon takes it.
- **Remote daemons** of an all-in-one host get the same cut and redial through Envoy, are not cordoned,
  and are placed on at once.

### 6.3 Storage: who writes, who reads, and why no write waits

Alternative A (kept): **demote in place, early**. `p` gives up the writer role of the meta log,
the cold store and the hot tier at the cut, keeps what it needs to read, and `i` is the only
writer from then on. B (`p` stays the storage server until its last stream ends, `i` writes
through it) is **rejected**, with the reasons that now matter: it makes `i`'s every read and
write cross a second process for as long as `p` lives (hours); it still needs the same switch
later, at an instant nobody chooses, when `p` has gone idle and `i`'s callers are all that is
left, so the one residual wait of 5.3 would land on `i`'s own callers instead of on calls that
`p` is already relaying; and `p` could not release its locks, so the next update could not start.
C (`kbf frontend` plus separate daemon hosts) stays the answer for a site that will not accept
the measured `T_s` of A.

The ordered cut, in `p` at `hand-over`, each step a flag flip or a lock release; "advance" says what `i`
does before the cut so the step costs little:

| step in `p` | what | advance work in `i` (before step 1 of 5.4) |
|---|---|---|
| route flag | new storage calls go through `HandoverStorage` to `i` (5.2) | `i` binds its REAPI listener; its handlers wait on the roles latch |
| tier seal | flush the active block into cold; tier read-only and open so a `ByteStream.Read` in flight finishes (`Cache::seal`). The reversible part (seal the active block, drain the flusher) runs at prepare. | none: B4, `i` must not open the tier before the seal |
| writes already open | Two cases, split by whether the body has been fully received at the route flag. (a) **Body still arriving** (a large upload, a slow client): `p` keeps receiving it, and once complete hands the buffered blobs to `i` as an ordinary write; `p` starts no segment PUT after the flag. A client slower than `T_s` is unaffected. (b) **Segment PUT already started**: it completes through `p`'s open store handle (objects are immutable and renamed in atomically, keys carry a per-start epoch so two writers never collide, `kbf-front/src/cache/write_group.rs`) and its `PutBlobs` **commit is forwarded to `i`'s meta log**; the caller is answered when that commit is durable. **Decision (was Q6)**: the cold-store lock is split into an exclusive open-and-sweep lock and a shared put lock, so `p`'s demote releases the exclusive lock at once and its started PUTs hold only the shared one; `i` takes the exclusive lock at start (no sweep needed: objects are only added) and its own PUTs take the shared one. Then `p`'s PUTs are not a component of `T_s`. An object-store cold tier has no lock at all. If the spike finds the shared lock unsafe for `file://`, the fallback is `p` finishing started PUTs before releasing, which becomes a named component `T_put` with its own gate (p99 under 2 s), and PR 6-9 do not ship without that number. | `i` accepts forwarded commits (an idempotent `PutBlobs` by object key); verified by T-4 |
| meta log | `RaftMetaLog::demote` (6.4, "Demote"): set `closed`, abort the tick task, take the host mutex (waits for the in-flight batch), release `log.lock` while still holding it, keep the applied `MetaState` queryable for frozen reads. After it no path in `p` can append (`commit_batch` and `raise_machine_version` both check `closed` under the host mutex) | `RaftMetaLog::open_follower` (new, 6.4): a read-only tail of the log, applying whole records as `p` writes them. At the cut `i` applies only the tail `p` wrote since its last poll, takes the lock with `Follower::promote`, and opens the store from the follower's cursor with the warm machine. Without it `i` replays the whole log inside `T_s` (B2). |
| cold store | `FsStore::demote`: release `LOCK`, keep reads | none: `i` blocks on the lock and takes it on release |
| state store | `store.demote()` last | `i` opens nothing; the rows are read at takeover (B12): measured inside `T_s` |
| daemon cache | release the cache dir lock (6.2) | none |

**Lock and latch order (explicit).** In `i`, strictly: (1) meta-log lock, once `p` has demoted it, and from that
instant `i` accepts **forwarded commits**, which bypass the roles latch (a forwarded commit belongs to a caller
that is part of the cut; making it wait on the latch is a deadlock and is a planted mutant); (2) the exclusive
cold-store lock (immediate with the split lock); (3) tier open (after `p`'s seal); (4) scheduler recovery;
(5) the roles latch opens. A forwarded commit names a segment object that `p` has already written; the cold
store is one shared namespace for `p` and `i` (`file://` on the same directory, or an object store), which is
a stated requirement and a `check` rule (same cold-store URI in both drop-ins).

**Frozen reads rule.** After the seal, `p`'s frozen read path reads the cold store and its own applied
`MetaState` only, never a hot-tier block: `i`'s tier open runs the orphan sweep and may remove any block no
staged object names. T-5 seals, opens the tier on `i`, then reads a hot-tier digest through `p`.

`HandoverStorage` is one `Arc<dyn Storage>` behind an atomic swap, used by the REAPI services and
by `LocalStore::over`. After the route flag: authority questions (`FindMissingBlobs`,
`GetActionResult`) go to `i`, never to frozen state; reads of a named digest try frozen state then
`i`; writes go to `i`. None of them is held in `p`: each is an open call on `i`, which answers when its
roles latch opens (5.3).

**Components of `T_s`**, each measured separately: `p`'s last meta batch (and, only under the fallback of the writes row, `T_put`); the lock releases; `i`'s meta-log promote (6.4.2: the final tail, `open_from` and the election; the whole replay only on the fallback); tier open on `i`; the scheduler's recovery from the state rows (B12).

**What `i` can and cannot do in advance, stated as claims to measure.** The follower open of the meta log and
the read-only open of the tier are the two pieces that decide `T_s`: without a follower open the meta log's
replay is in `T_s` and grows with the host's age (never compacted, no snapshots yet), and without a read-only
tier open (an open that does not run the orphan sweep) the tier open is too. If the spike measures a
`T_s` above the budget even with both, the log snapshots of `docs/design/metadata-snapshots.md` come first,
and all-in-one is not updated in place until they do (Q1).

**How `p` reaches `i`.** `i`'s REAPI listener is a known loopback port (the other block of the
table), reached over the farm-CA TLS with the node's identity. It is the same kind of hop as the proxy to a
storage node (`kbf-front/src/proxy`).

**Prepare (reversible).** At step 1 of 5.4, before the cut, `p` seals the reversible part of the tier, checks free
space on the cold, meta and state directories, and evaluates the `T_s` budget of 5.3. It refuses
(`refused disk`, `refused switch-too-long`, `refused unmeasured`) and nothing has changed. Each successor start
records its measured `T_s`. The first start into the template is a cold restart that records it.

**Committed cut block**, in the order of the table above. A failure after the route flag is committed: the
process fails closed (`drain.close()`, non-zero exit, no marker), systemd restarts it as a cold start, its builds
fail and their actions rerun, and `kbf_handover_demote_failed_total` alerts. Each stage has a failpoint test.


### 6.4 The meta log follower

This replaces "demote, then `i` replays the log". Replay is 40 to 50k entries a second in the spike (#933:
2.2 s at 100k entries, 10.0 s at 400k, 726 MiB), so a replay inside the cut passes the 1 s gate from about 45k
entries, and the log is never compacted, so it only grows. The follower moves the replay out of the cut: `i`
opens the meta directory as a follower before the cut, tails committed entries while `p` writes, and at the cut
applies only what `p` wrote since its last poll.

**Facts the design stands on** (arch-v2, re-read before PR M1):

- The segment files are append-only and every record is `len | crc32c(len, payload) | payload`; the CRC covers the
  length. Nothing flocks a segment; `log.lock` is a separate advisory flock that only the writer takes. A second
  process can read the directory while the writer runs.
- Nothing tails today. `Store::open` is a writer-side open (it truncates a torn tail, fsyncs, deletes dead segments
  and `*.tmp`); `Storage::load` re-scans whole files; `scan` is private. `Host::open_stored` builds a fresh core at
  `commit = base` and re-applies every stored entry through the election onto the machine it is given. There is
  no read-only store, no follower open and no way to apply entries to `MetaMachine` outside `Host`.
- The log is a single voter. An entry is committed when it is durable (the writer syncs, then applies, then
  answers). So a follower may treat every whole, CRC-valid, contiguous record as committed and never reads the
  raft `commit` index. **This is a property of the single-voter log; if the meta log ever gets a second voter the
  follower must read a commit mark and this section is reopened (Q13).**

**API** (kbf-server `raft_log`, over a read-only view in kbf-store):

```rust
impl RaftMetaLog {
    /// No `log.lock`, no write of any kind. Reads RECORD, rejects a dir holding a snapshot (until 6.4.4),
    /// scans every live segment read-only, applies it, and returns with a cursor at the end.
    pub fn open_follower(dir: &Path, opts: FollowerOptions) -> Result<Follower, FollowerError>;
    /// Sets `closed`, aborts the tick task, takes the host mutex (so the in-flight batch has finished), releases
    /// `log.lock` while holding it, keeps the applied `MetaState` readable. Idempotent. (This is `demote` of 6.3.)
    pub fn demote(&self) -> Result<(), DemoteError>;
}
impl Follower {
    /// Reads from the cursor, applies whole records, stops at the first short or CRC-bad record in the last
    /// segment. Never writes. Bounded work per call (`max_entries`), so a caller can observe lag.
    pub fn catch_up(&mut self) -> Result<CatchUp, FollowerError>;   // { applied, behind_bytes, at_tail }
    /// Takes `log.lock`, reads the final tail, opens the store from the cursor, elects, and returns the writer.
    /// Consumes the follower; on `Err` nothing was written except by the store open (6.4.3).
    pub fn promote(self, wait: Duration) -> Result<RaftMetaLog, PromoteError>;
    pub fn state(&self) -> Arc<RwLock<MetaState>>;   // frozen-read parity with the writer
}
```

`Follower` drives the same `Machine::apply` the host drives, in log order, so there is one decode path and one
version gate. It never proposes `Bootstrap`, never ticks, and its `MetaState` is not served to callers: before the
cut `p` answers every call, after it `i` does.

**Demote: no second writer after the lock is released.** Today the only guard against a write after the log is
closed is `closed`, checked in `commit_batch` under the host mutex, and `Drop` sets it before it takes the mutex.
`raise_machine_version` takes the host mutex and calls `propose` with no such check, and `demote(&self)` keeps the
host and its open append fd. So, as the code stands, a raise after the lock is released would append to the
segment `i` now owns, and "`p` refuses a raise from the moment prepare starts" would be a promise by the caller
only. The change (PR M3, a kbf-server change to `raft_log/mod.rs`):

1. `demote` does what `Drop` does, in the same order: `closed.store(true)`, abort the ticker, `drop(shared.lock())`
   is replaced by **holding** the host guard, take the lock file out of `lock`, drop it (the flock is released),
   then release the guard. Every writer takes the host mutex and checks `closed` under it, so after the flock is
   released no batch and no raise can write; one that was waiting on the mutex sees `closed` and is refused.
2. `lock: Option<File>` is a plain field today and `demote` takes `&self`, so it becomes `Mutex<Option<File>>`
   (taken only by `demote` and `Drop`; never on a commit path). A demoted handle's `Drop` finds `None` and only
   repeats the idempotent steps.
3. `raise_machine_version` calls `still_open(&self.closed)` after taking the host mutex and before it encodes,
   exactly as `commit_batch` does. It returns `MetaLogError::Unavailable` once closed.
4. **`demote` does not drop the append fd.** Dropping it would need the host to be an `Option` that every user of
   `Shared::lock` unwraps. The fd is harmless: nothing can write through it (point 3 and the batch check), and
   `i` opens segments by name. The cost is that `p`'s core, its entries and its `MetaState` stay in memory until the
   handle drops at `p`'s exit (hours); the memory estimate of 6.4.2 counts it. Freeing the host at demote is a
   follow-up if the spike's memory number needs it.

`p`'s refusal of a raise from the start of prepare stays, as a reversible, early answer; it is no longer the
safety.

#### 6.4.1 File-level safety

| hazard | rule | why it is safe |
|---|---|---|
| partial record visible (header without payload, payload cut short, a large `write_all` seen in part) | `get_record` returning `None` at the end of the last segment means "no more yet": keep the cursor, apply nothing, retry on the next poll. **This is looser than `scan`'s rule**, which calls a bad record at the tail of the last segment torn only if no valid record follows it (`!any_record(&bytes[off+1..])`) and otherwise `Corrupt`. A reader that sees a multi-record `write_all` in part sees a page-order prefix, so "retry" is right at the live tail. At `promote`, after the lock, the store applies `scan`'s rule itself (a valid record after bad bytes is `Corrupt`, never a truncation point), so `open_from` cannot truncate valid entries. `get_record` returns `None` for a length above `MAX_PAYLOAD` and for a CRC mismatch alike, so a damaged length in the live tail looks like a partial record forever: the follower counts polls with `file_len > cursor` and no progress, and reports those bytes in `behind_bytes`, so it ends as `refused meta-follower-behind` at prepare (and a bad record that never completes is then `Corrupt` at `promote`) | the CRC covers the length, so a prefix of a record cannot validate; a follower never applies a prefix |
| the same damage in a segment that is not the last | `FollowerError::Corrupt`, fail-stop | the writer's own open refuses it too (`StoreError::Corrupt`) |
| torn tail left by a writer that died | follower never truncates, and cannot tell a stalled writer from a dead one | resolved by `promote`: holding `log.lock` means no writer exists, and `Store::open_from` truncates there (6.4.3) |
| entries must be contiguous | each record's `index == prev + 1` (the check `scan` makes); a gap or repeat is `Corrupt` | a follower that skips an index applies a state the writer never had |
| rotation | at the end of segment `N` (cursor equals `file_len`) the follower lists the directory and looks for a segment with a greater `seq` and `first == last_index + 1` (the writer's continue path). Order: list, then **re-read the tail of `N`**, then move on. A new segment that exists but is empty means "retry". `scan` and `persist_entries` also have a **replace path** (a new segment whose `first` is at or below an earlier segment's, which truncates the log) and a dead-segment sweep; the follower implements neither, and meets either as `Corrupt` (`first <= last_index`, or a sealed segment whose length differs from the one recorded). For a single voter neither is written, with one exception: a **cold restart of `p` while the follower runs** (`Store::open` truncates a torn tail, the election appends a blank entry at a higher term, and a segment may be created). The follower must survive that: the truncation starts at the first bad record and the follower applied only whole ones, so its cursor is never past the new end; the blank entry is an ordinary entry with a higher term, which the follower applies and records as its last term (6.4.3, step 3). T-M11 runs it | a record can land in `N` between the follower's last read and its listing; a partial record in `N` once a later segment exists is `Corrupt`, because the writer rotates only after a whole record |
| visible before durable | the follower applies bytes that are in the page cache but not yet synced | harmless here: the follower and the writer share one kernel. A process death keeps the page cache, and a machine death kills the follower too and the successor starts cold. A durable-watermark file was considered and not taken: it adds a second file with its own ordering rule to guard a case the one-host rule excludes. This is also why meta and follower must be on the same host and the same local file system (a `check` rule, as for the cold-store URI) |
| fsync order | unchanged. The writer appends, then `sync_data`, then applies, then answers; segment creation is created, appended, fsynced, directory fsynced | the follower only reads. It can be ahead of an acknowledgement, never ahead of what a cold start would read after a process crash: a whole CRC-valid record is kept by `Store::open` (it truncates only from the first bad record) |
| `Store::open` of a dying old writer truncating something the follower applied | cannot happen: the truncation starts at the first short or bad record, and the follower applied only whole ones | |
| the machine version | the follower runs the machine's own `apply`. A `MachineVersion` entry raises the version for what follows, as for the writer. A version above this binary's `RUNS`, an entry that does not decode, or a control command makes `failed` true, and the follower stops with `FollowerError::Unsupported { log, runs }` | this is before the cut, so it is a reversible `refused meta-follower` at prepare. `p` also refuses `raise_machine_version` from the moment prepare starts, so the version cannot move under a follower; a raise already committed is applied in order |
| the follower's `hardstate` | **not read while following, read once at `promote`, after `log.lock`** (6.4.3, step 3). `p` writes it by temp file and rename and changes it whenever it restarts or re-elects, so any earlier view is stale. The follower records only the last entry's term, from the records it applies | the election needs `hard.term` at least the last entry's term (`Raft::restore` refuses otherwise) and a new term above what `p` persisted; both hold only for a read after the lock |

#### 6.4.2 What `catch_up` costs, and why `T_s` now depends on the tail only

The follower's first pass is a full read of history. It runs in the background, from `i`'s start, so it is not in
`T_s`; `i` is ready for the cut when `behind_bytes` is under `--meta-follow-lag-max` (prepare refuses
`refused meta-follower-behind` otherwise, reversible). With the same single-writer log, the cut then costs:

`T_meta = p's last batch + lock release + final tail (entries since the last poll) + hardstate read + open_from (including its syncs) + Log::restore validation + election (including its fsyncs)`

- **final tail.** The follower polls every `--meta-follow-interval-ms` (default 20). The tail is what `p` committed
  in one interval plus the batch in flight. An idle farm writes one 36-byte `Tick` a second; a `Tick`-only log
  replays at 2 to 3M entries/s (44 ms for 100k, 332 ms for 1M in the spike), so its tail is microseconds. A busy
  log replays at 40 to 50k entries/s; a tail of a few hundred entries is about 10 ms. These are estimates from the
  spike's replay rates, not measurements: the spike has no follower, so its "tail only" cost is the open item
  of 12, item 2.
- **lock release.** `p` aborts the tick task, waits for the in-flight batch (each commit already syncs before it
  returns, so there is no separate flush) and drops the lock. The second-open refusal is under 0.1 ms. The spike's
  writer `drop` is 3.6 ms at 10k and 255 ms at 400k entries; it includes freeing state. `demote` keeps the state, so
  the lock is released before any free (INFERENCE: measure).
- **`open_from` and election.** With the cursor as input the store reads the active segment from the cursor only
  (6.4.3), then the election appends one blank entry and fsyncs. Not free of syncs: `Store::open_from` still runs
  `fs.sync_dir()` and syncs the last segment, as `Store::open` does (store.rs, the end of `open`), the election
  persists the new hard state (temp file, sync, rename, directory sync) and the blank entry (append, sync). Count
  that as about five fsyncs, each a few ms on the farm's disks (an estimate; the spike's follower includes them, or
  the 250 ms gate cannot be checked at 400k entries). There is no read of the directory.
- **`Log::restore` validation.** The core checks every entry it is given (contiguous, terms never fall), a pass over
  the whole in-memory Vec, with no I/O. At 400k entries it is a few milliseconds (INFERENCE; the spike measures it).
  It is in `T_meta` because it is the one remaining O(history) step. If it is not small, `restore_applied` takes the
  follower's checked entries and skips it (the follower has verified contiguity; the terms it records).
- **Memory, for the follower's life.** The core keeps the whole log in memory (`Log { base, entries }`;
  `Store::open` returns every entry). `p` holds `MetaState` plus every entry; `i`'s follower holds the same two,
  because it must hand the core the entries at promote. So during the follow the box holds about twice what a single
  host holds (the spike's 726 MiB at 400k entries was a replay's resident size; the follower's peak per process is of that order,
  an estimate to measure), and `p`'s share is not freed at `demote` (point 4 of Demote). Prepare checks it:
  `refused meta-follower-memory` if free memory is below twice the follower's resident size plus a margin (reversible).
- **Without any of this** the same cut is the spike's 2.2 s at 100k and 10.0 s at 400k.

Gate (a proposal for the spike to confirm): `T_meta` p99 under 250 ms at 400k entries with a writer committing at
the highest rate the farm sees, so the meta log is a quarter of the 1 s budget of 5.3. The number is recorded per
start like the rest of `T_s`.

#### 6.4.3 Promotion keeps the warm state

`Follower::promote(wait)`:

1. `try_lock` `log.lock` until `wait` expires (`PromoteError::Locked`, nothing changed). Taking the lock is the
   happens-before: `p` released it after its last append.
2. **After** the lock, `catch_up` once more. Reading before the lock would leave a window for one more batch;
   after it, no writer exists, so a partial record here is a real torn tail.
3. `Store::open_from(cursor)`: the sealed segments before the cursor are immutable (a single voter never
   truncates or rewrites; the replace branch of `persist_entries` does not fire), and the follower verified them,
   so the store trusts them if each one's name and `file_len` equal what the follower recorded. It reads the active
   segment from the cursor offset, truncates and syncs a torn tail, sets `active`, `last` and `next_seq`, and runs
   the writer-side clean-up (dead segments, `*.tmp`). If any recorded length differs, it falls back to the full
   `Store::open` and a full replay: slower, never wrong, and counted in `kbf_meta_promote_fallback_total`.
   **Hardstate and term.** Step 3 reads `HARD_STATE` here, after the lock and after the final `catch_up`, never from
   a value the follower saw earlier (`p` may have restarted and re-elected during the follow, so its term can be
   above 1, and above the term of any entry the follower applied). `Raft::restore` refuses a log whose last entry
   term is above `hard.term` (`ConfigError::BadLog`); `promote` reports that as `PromoteError::Diverged`. The
   election then campaigns at `hard.term + 1` (the core's own rule), which is above both the term `p` last
   persisted and any vote it recorded, so "every start's term exceeds the earlier ones'" holds exactly when the
   read is post-lock. T-M13 plants a re-election of `p` during follow.
4. `Host::open_resumed(config, store, stored, machine, applied)`, over a new core constructor
   `Raft::restore_applied(config, hard, base, entries, applied, entropy)`. **What it is, and what it costs**: it is
   `Raft::restore` with `commit = applied` and `applied = applied` instead of `commit = base.index`. `base` stays the
   store's base (index 0 until snapshots exist), `entries` is the **whole** log from `base + 1`, handed over from the
   follower (moved, not copied), and the machine is the follower's warm one. So `snapshot_base` and the store's
   `base` agree, and a later `Host::snapshot` or `compact` compacts against a real base. The alternative that was
   considered, a core whose base is only the last `LogId`, makes `snapshot_base` disagree with the store's `base`
   (`persist_entries` checks `first <= self.base`) and lets a later compaction work against a base that is not
   a snapshot; it is rejected. Cost: the memory of 6.4.2 (the follower holds every entry, doubling the host's
   footprint for the follow; the promoted core holds them anyway, as a cold start's does) and one validation pass over
   them (`Log::restore`, in `T_meta`). No replay and no second apply: the election's blank entry is the only entry
   that applies after `applied`. The invariant `applied == last whole index in the store` is asserted; a mismatch is
   `PromoteError::Diverged` and the process takes the committed-cut failure path (cold start). This is the one seam
   that stops a promoted follower from replaying the log or from applying onto a warm state twice.
5. Elect (term `hard.term + 1`, blank entry), skip `Bootstrap` (already applied), `keep_answers`, read the farm time,
   start the tick task.

The compatibility gate sits at step 0 of `open_follower`: the binary's `RUNS` must be at least the version the log
has committed (`FollowerError::Unsupported`), so a newer-writer or rolled-back case refuses at prepare, not at the cut.

#### 6.4.4 Snapshot and compaction while following

`MetaMachine` takes no snapshot today (`snapshot` is `unreachable!`, `restore` returns `NoSnapshots`, nothing in kbf-server calls `Host::snapshot`), although `Store` and `scan` already read snapshot files, skip covered segments and sweep dead ones; a directory with a snapshot is refused by the follower. So segments are never deleted and a follower never loses a file. `open_follower` refuses a
directory that holds a `snapshot` file and `catch_up` checks for one at each poll (`FollowerError::Snapshot`): a
follower must not tail across a format it does not read. When the planned snapshots land
(`docs/design/metadata-snapshots.md`), the follower becomes snapshot-aware in its own PR (M5):

- At open: read `snapshot` first (it is replaced by rename, so a reader sees a whole old or whole new file), restore, then list segments and
  start at `base + 1`. A segment that holds the base and later entries is whole in the directory, so read from the
  right offset.
- One rule for "covered by the base": the function `Store::open` uses to skip dead segments, shared by the
  follower. A segment removed after a snapshot can reappear after a crash (removals are not synced); both skip it.
- While following: an open fd survives an unlink, so a follower mid-segment keeps reading. A follower that re-opens
  by name and gets `ENOENT`, or whose next segment is missing (`first > last + 1`), or whose cursor is below the new
  base, re-reads `snapshot`, restores, and continues from `base + 1`. The loop ends only when a newer snapshot base
  made progress; it is bounded, and `Raced` after three passes with no progress is a reversible refusal.
- Snapshots bound the follower's first pass and the fallback of 6.4.3 to the post-snapshot log; without them both
  are O(history), the first in the background and the second only on a failed trust check.

#### 6.4.5 Failure cases

| case | what happens |
|---|---|
| follower falls behind before the cut (slow disk, a burst, apply slower than commit) | `catch_up` reports `behind_bytes`; prepare refuses (`refused meta-follower-behind`), nothing changed. If it falls behind inside the cut, `promote` applies the larger tail: correct, `T_s` grows, the budget check records it |
| the follower fails before the cut (`Unsupported`, `Corrupt`, an I/O error) | reversible: prepare refuses with the reason and `p` is unchanged. The next start of `p` would refuse the same directory, so a `Corrupt` is an alert, not a retry |
| `p` dies mid-cut, before the lock is released | the kernel drops the flock at exit; `promote` takes it, applies every whole record, truncates the torn one. Callers were answered only after sync, so no answered write is lost; an unanswered write is lost or kept, as after any crash. The difference from a cold start is time, not outcome. The farm-level row (section 10: `p` dies mid-follow) is unchanged: its marker and `--acknowledge-crash` are about leases |
| `p` released the lock and `i` dies before `promote` finishes | the directory is unlocked and consistent (a torn tail at most); the next start is a cold start. No file was written before the lock |
| `promote` fails after the lock (open_from, `Diverged`, election) | the lock drops with the follower; `i` exits non-zero, no marker, the cold start serves. This is the existing committed-cut rule |
| compaction races (after snapshots) | 6.4.4: re-read the snapshot, bounded passes, refusal rather than a silent skip |
| `p` is killed and restarted while the follower runs (torn tail truncated, hard state bumped, blank entry at a higher term, a possible new segment) | the follower applies only whole records, so its cursor is within the truncated file; it applies the blank entry, keeps contiguity and never applies an index twice; a sealed segment whose length is below the recorded one is `Corrupt`, and `promote`'s recorded-length check falls back to the full open. T-M11 |
| a raise or a batch after `demote` released the lock | refused under the host mutex (`closed`); the file length is unchanged. T-M12 |
| `p`'s tick task still running at the cut | the cut aborts it before the lock release, so no `Tick` lands after the lock; a mutant plants one |

#### 6.4.6 Why not the other shapes

- Replay on `i` at the cut (the first version of this note): O(history) in `T_s`, 10 s at 400k entries.
- `i` opens the directory as a second writer: refused by the flock, and sharing it would double-apply.
- `i` copies the directory: the spike measured a full replay over a copy (2.5 s at 100k, 9.7 s at 400k), and a copy
  taken under a writer is a torn copy.
- Warm state shipped from `p` over a socket: a second protocol with its own version problem, to avoid reading a
  file `p` already writes in a framed, checksummed format.

## 7. Envoy's own update: hot restart

Envoy is updated, and its configuration changed, by **hot restart**, never by `systemctl restart`
(which cuts every open stream). The unit runs Envoy's restarter script (the supported wrapper:
SIGHUP re-runs the start script with the next `--restart-epoch`). `ExecReload` sends that SIGHUP.

What Envoy does (Envoy's hot restart documentation): the new process initializes, runs its first
health checks, asks the old one for its listen sockets (reuse-port sockets are passed by worker
index), listens, and tells the old process to drain. Existing connections are never transferred. The
old process sends its HTTP/2 connections a GOAWAY, lets open streams finish, and is shut down after
`--parent-shutdown-time-s`; whatever is open then is closed.

Settings, and why each is not the default:

| flag | default | value | reason |
|---|---|---|---|
| `--parent-shutdown-time-s` | 900 | the longest action plus 15 minutes | at expiry the old process is closed with every stream still open; 900 s would fail every action over 15 minutes |
| `--drain-time-s` | 600 | 60 | below the parent shutdown time as Envoy requires; it only schedules how soon connections are told to go, not when streams are cut (open ordinary streams are left to complete, per the draining documentation) |
| `--drain-strategy` | `gradual` | `gradual` (unless E8 shows Buck2 mishandles the two-step GOAWAY) | `immediate` removes the first, lossless announcement and sends the final GOAWAY at once; streams opened in that race are the only Buck2-visible risk and recover only through the 5-attempt open retry (about 1.5 s). `drain_timeout` stays 5 s. E8 opens Executes in a loop across a real hot restart and counts failed opens |
| `--base-id` | 0 | fixed, unique on the host | the epochs' shared memory |
| `--skip-hot-restart-on-no-parent` | off | set | a start after a crash of the first process is a normal start |

**The longest action is not a bound** (section 2): an action names its own timeout. Either the farm caps it
(a `--max-action-timeout` on the server, refused at Execute above the cap, which makes the Envoy value a
derived number and a check) or Envoy's hot restart can cut an action longer than the parent shutdown time
(Q2). The box config carries `KBF_LONGEST_ACTION_SECS`; the kit's check refuses a render whose
parent-shutdown value is below it.

**Worker sessions at a hot restart.** A `tcp_proxy` connection gets no GOAWAY; the draining documentation covers HTTP
connection managers only. So every daemon session (remote daemons and the in-process one) stays pinned to the
parent epoch, quiet by design, and all of them are cut at the same instant at `--parent-shutdown-time-s`. Daemons
redial with jittered backoff (a small daemon change, PR 4) so the 6 h mark is not a herd, and the epoch is kept so
no lease is lost (the fence is 40 s). Until then the parent carries worker traffic against its own stale health view,
so a kbf handover during that window may route a redial to the parent's view of `p`; the handover is therefore refused
while a parent epoch is alive (rule below), not merely warned about.

**What a client sees at an Envoy update.** This is the one place a client sees a GOAWAY. Envoy's HTTP
connection manager announces a shutdown to a drained HTTP/2 connection and sends the final GOAWAY after
`drain_timeout` (5 s default; inference: the two-step announcement is the sequence RFC 9113 describes and
the one the split-mode note names as the fix for the GOAWAY race). Streams already open run to their end on
the old process; new streams on the old connection before the final GOAWAY are accepted by it; Buck2 has no GOAWAY
handling of its own, so what it does with a refused-at-GOAWAY stream is the same open question as before
(the split-mode update note, `docs/design/split-mode-updates.md` on branch `docs/split-mode-updates`, not yet on arch-v2, section 4.1, U1) and now concerns Envoy updates only, which are rarer than kbf updates. The
real-Buck2 build through an Envoy hot restart is a test (T-E8).

**Rules.** An Envoy hot restart and a kbf handover are serialized by one lock file the updater and
`kbf handover` both take, so neither starts while the other is mid-way. A second Envoy hot restart while the
first parent is alive is refused by the updater (it reads `server.hot_restart_epoch` and the parent's
liveness from the admin interface). Whether Envoy itself limits concurrent epochs is **not verified** (the
hot-restarter documentation is silent; the source may name a small fixed number of concurrent processes); the spike reads
the pinned Envoy's source and runs epochs 0 to 3 with live parents. The refusal says why: `blocked by parent
epoch N, expires in X`. With a parent shutdown of the longest action plus 15 minutes (6.25 h under the Q2 cap), a
restart can be refused for hours while any long action runs, so **nothing time-critical may depend on a hot restart**:
certificate renewal does not (below).
Envoy's binary is versioned and staged like kbf's (section 9). The new Envoy epoch needs no change on the kbf side: its static
config lists `a` and `b`.

**Certificates.** Envoy holds the client-facing certificate. A renewal needs Envoy to read it again: a hot
restart (blocked for hours by a long action, above) or a file-watched SDS secret (`path_config_source` on a
watched file; no xDS server, but it is an SDS source, and the owner excluded xDS: asked as Q3). Recommendation:
the file-watched secret, because renewal must not wait for the longest action; the spike tests that a renewed file is
picked up by a running Envoy without a restart and without cutting a stream. Fallback if the owner says no: hot restart plus an alert at
a third of the certificate's life that a renewal is blocked. The backends keep their own live reload (`docs/deploy/tls-renewal.md`).

## 8. What a client sees, end to end

A kbf update, Buck2 first:

| time | event | Buck2 |
|---|---|---|
| t0 | `i` starts, not ready, warms | nothing |
| t1 | `p` runs `hand-over` and stays ready | open `Execute` streams continue on `p` (answered from `done/` rows once `i` has the scheduler); a new call that Envoy still sends to `p` is answered through `i` (5.2), slower by at most `T_s` (5.3); the connection to Envoy is unchanged |
| t2 | `i` holds every role, is ready; Envoy sees it healthy in at most one interval | new streams go to `i` |
| t3 | `i` sees itself healthy in Envoy, settles, tells `p` `unready`; Envoy stops choosing `p` | nothing: no frame reaches Buck2, and no stream is cut (3.1) |
| t4 | `p` exits when its last stream ends | nothing |

No GOAWAY is needed from kbf and none reaches Buck2: Envoy owns the downstream connection, and `p` is
never sent a GOAWAY in an update. Streams open on `p`'s connection continue because a failed health check
removes a host from selection and Envoy drains its connection pool gracefully, so open streams run to
completion (3.1; inference, E4 measures it on the pinned Envoy). There is no window with no healthy backend, so there is no Envoy-generated 503 and no
reliance on Buck2's 1.5 s open-time retries. The one added wait, `T_s`, is gated at p99 under 1 s, and only for a call that arrives between t1 and t2 or is still sent to `p` between t2 and t3.

For the in-process daemon the cut is `hand-over` ending its stream, then a redial through Envoy, as in 6.2.

## 9. Units, command, marker

Mirrors `kbf-frontend@.service` (merged, #887), for both modes.

- Template `kbf-frontend@.service` and `kbf-all-in-one@.service`, `Type=notify` (`READY=1` when the instance
  holds the store and its probes pass), `TimeoutStartSec` finite (all-in-one: `max(120, 3 x recorded replay)`),
  `KillSignal=SIGTERM`, `TimeoutStopSec=infinity`, `Restart=on-failure`, `DefaultInstance=a`, no `Conflicts=`
  between instances, `RestartPreventExitStatus=2`. A per-instance drop-in carries the port block, `--node-id`,
  `--instance` (the rendered drop-in is the only per-instance difference). `install-units` refuses when the
  free memory is below both instances' `MemoryMin` (an idle process and a draining one must fit).
- `kbf-envoy.service`: the restarter with `ExecReload`, `OOMScoreAdjust` below kbf's (an Envoy killed is every open
  stream cut), versioned binary path. `systemctl stop kbf-envoy` is documented as the outage it is.
- `kbf handover --unit kbf-frontend|kbf-all-in-one [--wait]`: reads `ActiveState` of both instances; exactly one active:
  start the other; none: start `@a` unless a marker blocks it; both active or transitioning: refuse, exit 1. It
  runs only `show` and `start`, never `stop` or `restart`. With the Envoy lock (section 7).
- **Binary path.** The updater stages `/opt/kbf/<sha>/kbf`, `ExecStart` runs the `current` symlink, so a running
  process's `/proc/self/exe` (which keepers re-execute) stays its versioned file; garbage collection keeps every
  version a running process maps plus the newest two. Order: stage, check both instances are not active, flip
  the symlink, then `kbf handover --wait`. A refusal before the flip leaves the symlink alone and reports
  "draining: N leases, oldest M minutes".
- **Detached marker (all-in-one).** `/run/kbf-all-in-one/detached-%i`, written in the demote block before
  `released`, removed on a clean exit. `ExecCondition=kbf all-in-one precheck --instance=%i` exits non-zero when it exists, so a
  crashed detached process is not restarted into a takeover of the live instance (which would be an update
  backwards); `start_or_take_over` repeats the check; `kbf handover --acknowledge-crash <instance>` is the only way
  back. While detached the unit reads `active (running)` and the process keeps `STATUS=` and a read-only status file current.
- **Long drains.** The update rate is bounded by the longest action on the old instance; `--max-detached-hours`
  (default 12) alerts and never forces. A second SIGTERM to the old instance fails the builds it holds; the doc says so.
- **N / N-1.** The windows of the split-mode update note (`docs/design/split-mode-updates.md` on branch `docs/split-mode-updates`) section 3 apply unchanged: the old instance reads, for
  three looks each, the `done/`, `op/`, `lease/` and queue rows the new one writes, so row encodings are frozen across one
  step and the SQLite schema follows expand/contract. Envoy's config is version-independent, so a kbf update is not
  also an Envoy compatibility question.

## 10. Failures

"Class": reversible (before the cut) or committed.

| point | class | Buck2 | operator / daemon | recovery |
|---|---|---|---|---|
| `i` fails or is refused before it calls `p` | reversible | nothing | `systemctl start` fails | `p` unchanged; fix, retry |
| prepare refuses (disk, flush, `T_s` budget, unmeasured) | reversible | nothing | `i` exits with the reason | retry |
| `i` dies after `released`, before serving | committed | the watchdog (5.1) turns `p` unready within `--handover-watchdog-secs`; until then new calls to `p` fail UNAVAILABLE (retried by Buck2); relayed streams follow `done/` rows by operation id | cold start (replays the meta log, `TimeoutStartSec` up to `max(120, 3 x replay)`) bumps the epoch; the detached daemon reruns its leases | callers of `p` are answered from rows; this is the one window that can fail a build |
| `T_s` above its budget | committed | calls in the window are slower; none is dropped | `kbf_handover_switch_ms`; the next update is refused (`refused switch-too-long`) | fix the slow component; see 6.3 |
| `i` never shows healthy in Envoy (admin API or check broken) | committed | none: `p` stays ready and keeps relaying; no capacity is lost | the derived settle timer runs as the fallback; alert on `p` still relaying | repair Envoy's check; `p` has no way back, so `i` keeps serving |
| a demote stage fails | committed | open streams on `p` end UNAVAILABLE | `p` exits non-zero, cold start | actions rerun |
| `p` dies mid-cut, meta lock not yet released | committed | its open streams and relays break | `i`'s `promote` takes the freed lock, applies the whole records, truncates the torn tail (6.4.5) | no answered write is lost |
| `p` dies mid-follow | committed | its open streams and relays break (the process holds them) | its daemon and keepers die (B1); marker stays | `--acknowledge-crash` |
| **Envoy crashes or is killed** | n/a, **new** | every open `Execute` breaks | systemd restarts it; kbf unaffected; worker sessions redial | the cost of the uniform topology: a process in front of every build. Mitigation: low `OOMScoreAdjust`, a memory limit, no `restart`, a watchdog |
| **Envoy hot restart: parent closed at `--parent-shutdown-time-s`** | n/a, **new** | streams still open then break | `kbf_envoy_parent_cut` | set the time above the longest action (Q2) |
| Envoy hot restart: worker sessions cut at parent shutdown | n/a, **new** | none | every daemon redials at once (jittered); epoch kept, no lease lost within the 40 s fence | jittered redial (7); E7 holds a worker session past drain time and sees the cut |
| a second Envoy hot restart or a kbf handover while a parent epoch is alive | n/a | none | refused: `blocked by parent epoch N, expires in X` | wait, or Q3's renewal path that needs no restart |
| a health check flaps ready on a fenced instance | committed | relayed calls | `kbf_handover_relayed_total` rising after `unready` | the check includes the stop flag |
| two updates back to back | n/a | n/a | `kbf handover` refuses when both instances are active | bounded by the longest action |

Reading of the table: the update fails a build only through the first-version rows (an operation with no trace
for three looks, a second-signal escalation, a crash of the process that holds the stream, a committed demote failure,
a successor that dies with relays open) plus the two Envoy rows. The earlier write-drain expiry row is gone: no write is
cut by a timer. Envoy is a single process in front of every `Execute`; that is the price of one update path
everywhere and the project has taken it.

## 11. Reuse from the merged handover code (#857, #879, #887)

All of this is on arch-v2. Build on it; do not re-implement it.

- **Kept**: `Farm::quiet/fence/follow/end_worker_streams` and the durable `done/` and `cordon/` rows
  (`farm/handover.rs`, `farm/durable.rs`), `open_after_handover` and `EpochPolicy::Keep`, `Fenced`/`Store::demote`,
  `READY=1` (`handover/notify.rs`, `ready.rs`), the daemon-side redial, the `Stop`/`Latch` pair, the
  `kbf-frontend@` units. `Drain` is wrapped by the staged object (5.1), not replaced in place.
- **Dropped from the Envoy path** (merged, so this is code left unused by it, not code never written): the control
  socket and `SCM_RIGHTS` wire (`handover/wire.rs`, `incoming.rs`, `outgoing.rs`), `listen.rs`' adopt and spare dup, the
  address check against the holder's listeners, GOAWAY as the client's switch, and the "connections wait in the
  backlog" step. The coordination the wire carries (refusal reasons, the generation) is the API call of 5.4 and the
  state store's own generation. Nothing is deleted by this note; Q12 decides whether the wire stays.
- **New**: the Execute relay (5.2); `HandoverStorage` (6.3); `RaftMetaLog::open_follower`, `Follower::promote`, `Host::open_resumed` over `Raft::restore_applied` (6.4) and a read-only tier open (6.3);
  the takeover's cordon write and own-row delete (6.2); the split cold-store lock (6.3); the `handed_over` readiness pin and the watchdog (5.1);
  an unfence or a reversible prepare stage in front of `Farm::fence` (5.1, B10).
- **Open**: whether the fd-passing path stays as a no-Envoy shape (Q12 of section 15). It is merged and has a crash
  matrix in flight (#900), so trimming is now a removal of tested code, not a choice not to merge it.

The assembly of `frontend.rs` (`Prepared`, `Trio`, `hello`, `take_over`, `start_or_take_over`) becomes a
`handover::Driver` both modes call, with a golden of its call sequence for the frontend taken before the refactor.
`kbf all-in-one` gains `--instance`, `--envoy-admin`, `--handover-watchdog-secs`,
`--handover-switch-budget-ms`, `--handover-follow-interval-ms`, all off without `--instance`.
Configuration is flags; the only environment variable is `NOTIFY_SOCKET`.

## 12. Spike gating the design (PR 0, throwaway, not merged)

Answers, with a real pinned Envoy and stub or real kbf, before any merged PR:

1. **Envoy**: a silent stream longer than the default 15 s route timeout survives with the box settings; the default
   300 s idle timeout is not reached with progress frames; a failed health check leaves an open stream alone and stops
   new streams within the settle; with both hosts healthy and one turning not-ready there is no moment with a local 503;
   `GET /clusters?format=json` shows each host's health flag as the design reads it; more than 1024
   concurrent silent streams through default breakers fail and through the box settings do not;
   `close_connections_on_host_health_failure` on the worker cluster cuts a daemon's session (so it stays false); a hot restart
   with a 60 s drain time leaves a silent stream open past it and closes it at the parent shutdown time; what a second hot
   restart inside the first's window does: the pinned Envoy's hot-restart source read, and epochs 0 to 3 run with live parents;
   a `tcp_proxy` session that survives drain time and is cut at parent shutdown; a file-watched SDS secret renewed in a running Envoy; an open
   silent stream on `a` when `a` fails the gRPC check completes with no RST_STREAM or GOAWAY error; the `/clusters` flags of a freshly added host
   (`pending_active_hc`); route-level `idle_timeout: 0s` on a 3 h silent stream (scaled).
2. **kbf, the switch**: `T_s` and each component of it (6.3) on a realistic host, p99 over repeated cuts: with and without a
   meta-log follower (final tail, hardstate read, `open_from` and election with their fsyncs, `Log::restore` validation, and the follower's resident memory, at 10k, 100k and 400k entries, under a writer at the farm's highest commit rate, against the 250 ms gate of 6.4.2; and whether `Store::open`'s full segment read, not machine apply, is the larger part of today's open); with and without a read-only tier open; the longest in-flight segment PUT with large blobs; the
   scheduler's recovery on a busy state file. `RaftMetaLog::demote` keeps its applied state readable. Whether a segment PUT through
   a handle that has released the cold-store lock is safe for `file://`. Whether the split cold-store lock (shared put, exclusive open) is safe for `file://`. Cut-to-`i`-serving-worker-sessions time with the daemon's
   backoff, against the 40 s fence. The result is a number against `--handover-switch-budget-ms`, and the go or no-go for A in 6.3.
3. **kbf, the relay**: a relayed `Execute` through `i`'s listener before its roles latch opens completes after it, with the
   client's own deadline honoured and nothing queued outside the open call.

## 13. Tests

Each test names the defect it catches and the line that turns it red; a mutant is planted and seen red on the farm
before a PR goes up. Unit tests sit in the library they cover; real Envoy tests use the pinned binary (CI fetches
it); container tests run in the crun container job (Podman is dropped; kbf runs containers with crun only), everything else runs in `test-x86` and `test-arm`.

| id | test | catches | mutant |
|---|---|---|---|
| E1 | the box render is golden, passes `envoy --mode validate`, and `envoy check` passes it | a render Envoy refuses or the checker misreads | add an unknown field |
| E2 | `envoy check` fails each defect of 3.1 alone: route timeout default, `max_stream_duration`, idle timeout default, default breakers, `close_connections_on_host_health_failure` true, worker idle default, a retry policy on `ByteStream` or with `unavailable`, one instance missing, overlapping ports, a parent-shutdown below the longest action | a render that silently cuts or refuses a stream | each setting returned to Envoy's default |
| E3 | real Envoy and two stub servers: a silent `Execute`-shaped stream longer than the (scaled) timeouts completes, including a silent stream longer than the HCM idle timeout with progress 0 | the 15 s and idle defaults, an action longer than the idle timeout | route timeout left at default; the idle timeout left on the Execute route |
| E4 | stub `a` turns not-ready while `b` is ready: no new stream reaches `a` within the settle, a silent 10 min (scaled) Execute-shaped stream open on `a` completes with no RST_STREAM or error GOAWAY upstream, no 503 is ever returned, for the REAPI (L7) and worker (L4) clusters | health failure cutting a stream; a daemon session cut; a gap | `close_connections_on_host_health_failure` true on the worker or REAPI cluster; `a` turned not-ready before `b` is ready |
| E5 | more silent streams than `ceiling` and than `max_concurrent_streams x max_connections` are held open through the box render | the 1024 cap; a connection cap hit before the request cap | breakers left default; `max_connections` default |
| E6 | `kbf frontend` a/b with Envoy: an `Execute` open across the handover completes; calls issued continuously through it see zero errors and zero Envoy 503; `kbf_handover_relayed_total` is non-zero and matches the settle; `i` does not fire `unready` on a host with `pending_active_hc` | the update gap; a premature `unready` | `unready` sent at the cut instead of after `i` is healthy; relay replaced by a refusal; "no failed flag" read as healthy |
| E7 | real hot restart: a silent stream outlives `--drain-time-s`, new streams reach the new epoch, a worker `tcp_proxy` session survives drain time and is cut at parent shutdown, the parent is gone at the parent-shutdown time; a kbf handover while a parent epoch is alive is refused; with a parent shutdown below the stream's length the stream is cut and the test goes red | the default 900 s; a handover that trusts a stale epoch's health view | parent shutdown at 900 s |
| E8 | before the last PR: one real Buck2 build through a kbf handover, and one through an Envoy hot restart | Buck2's behavior at GOAWAY and at a relayed open | skip the relay |
| T-R | the relay: `Execute` and `WaitExecution` after the cut reach `i`, metadata forwarded, `i` authorizes again, the response stream is relayed to the end; a call that reaches `i` before its latch opens completes after it with no queue object outside the call; the client's deadline ends a call that never starts | a lost call, a hold bounded by a flag, a bypassed policy | answer from `p`'s own scheduler; drop the metadata |
| T-D | `Drain`: `hand-over` alone leaves `/readyz` ready and the gRPC health SERVING, **also with `i`'s latch closed for longer than the 2 s store-probe timeout**; `unready` flips both; no handover body keeps today's `begin` (GOAWAY included); each stage idempotent | a split that loses an effect, readiness off too early | flip readiness in `hand-over`; run the store probe through `HandoverStorage` |
| T-A | the API call: refusal reasons; a different state path refused; the answer carries the generation; no handover body keeps today's meaning | a refusal that proceeds | accept without comparing |
| T-1, T-3 | `RaftMetaLog::demote` (the lock is released after the in-flight batch and state stays readable) and `FsStore::demote`; the second open succeeds while reads work | B2, B3 | skip the unlock; release before the batch ends |
| T-M12 | after `demote`: `raise_machine_version` returns `Unavailable`, a `commit` returns `Unavailable`, and the active segment's length and the directory hash are unchanged; a raise queued on the host mutex while `demote` runs is refused, not applied; a second open of the directory succeeds and its writes are not interleaved with any from the demoted handle | a second writer after the lock release | delete the `still_open` check in `raise_machine_version` (red: the file grows); set `closed` after the lock release instead of before; take the host mutex after releasing the flock |
| T-M1 | `scan_readonly` over a segment cut at every byte offset of the last record: whole records apply, a prefix never does, no file changes (a hash of the directory before and after), and the same cut in a non-last segment is `Corrupt` | a follower that applies a prefix or repairs the directory | skip the CRC check; call `truncate` |
| T-M2 | the follower and the writer on one directory: a writer thread commits while the follower polls; at a pause both `MetaState`s are equal, and `index` is contiguous | a skipped or doubled entry | drop the `index == prev + 1` check |
| T-M3 | rotation: the writer crosses a small `segment_bytes`; the follower sees the new segment empty (retry), then filled; a record landing in the old segment between its last read and its listing is applied | a lost record at the segment edge | move on without re-reading the old tail |
| T-M4 | version: a `MachineVersion` raise during following is applied in order; a log above `RUNS` and an undecodable entry give `Unsupported`; `p` refuses `raise_machine_version` after prepare (the safety is T-M12) | a follower that runs on past `failed` | ignore `failed` |
| T-M5 | `promote`: after it, `MetaState` equals a cold `open_dir` of a copy of the same directory, the term exceeds the old one, no entry is applied twice (a counting machine), and applied entries before the cursor are not re-read (a counting `Fs`) | a replay at takeover, a double apply | pass the cold machine; read every segment in `open_from` |
| T-M11 | `p` is killed mid-batch (child, SIGKILL) and restarted on the same directory while the follower runs: the follower stays contiguous, applies the new blank entry, never applies an index twice (a counting machine), and a shortened sealed segment is `Corrupt` | a follower that double-applies or reads past a truncation | drop the `index == prev + 1` check; trust a recorded length that shrank |
| T-M13 | `p` restarts and re-elects during follow (term above 1), then `promote`: `hard.term` is read after the lock, the new term is `hard.term + 1`, and `Raft::restore` accepts; a planted stale read gives `BadLog`/`Diverged` | a promotion at a term not above the old process's | read `HARD_STATE` before taking `log.lock`; hard-code term 1 |
| T-M6 | crash matrix: the writer is killed (SIGKILL in a child) at each failpoint of a commit (before write, after write before sync, after sync); `promote` truncates a torn tail, keeps every acknowledged entry, and equals a cold open | a lost acknowledged write; a follower ahead of the cold start | truncate before the lock; apply a prefix |
| T-M7 | trust check: truncate or replace a sealed segment between follow and promote: `open_from` falls back to the full open, the counter increments, the state is correct | trusting a changed prefix | skip the length check |
| T-M8 | fall behind: a slow `Fs` makes `behind_bytes` large; prepare refuses; the tail still applies after the cut when forced; a live-tail record with a damaged length (above `MAX_PAYLOAD`) is retried, counted in `behind_bytes`, and ends as `refused meta-follower-behind`; at `promote` the same bytes with a valid record after them are `Corrupt` and nothing is truncated | a silent cut over a long tail | report `at_tail` always true |
| T-M9 | (with snapshots, M5) a snapshot and segment removals during following: mid-segment follower continues; a follower below the base restores; a removed segment that reappears is skipped by both | a stale cursor; two rules for "covered" | follower uses its own rule |
| T-M10 | the cut with the tick task: no `Tick` after the lock release | a late write by `p` | abort the ticker after the unlock |
| T-4, T-5 | `Cache::seal` and the orphan pass; `HandoverStorage` (`FindMissing` and action-cache lookups to `i`, named reads frozen then `i`, a write open at the cut finishes and its commit lands once on `i`, a write after the cut lands on `i`); a segment PUT held open across the cut commits exactly once on `i` with no deadlock; a 64 MiB `ByteStream` write whose body straddles the cut, with a client slower than `T_s`, completes; seal, open the tier on `i` (sweep), then read a hot-tier digest through `p` | B4, a stale hit, a lost or doubled write, a lock-order deadlock | answer from frozen state; commit on `p` after the demote; forwarded commit waits on the latch; `p` reads a tier block after the seal |
| T-6 | real stages with a failpoint at each cut-block index: fail-closed outcome, no marker, cold start serves; state demote last | a partial demote | move `store.demote` earlier |
| T-7 | `Keep` on the real start path, the Welcome epoch unchanged across a handover (SQLite open included); recovered lease books on the predecessor's node id; successor registers `total - booked`; two cycles a, b, a, the second gets grants; **kill and restart `i` with `p` still detached: `p`'s worker is granted nothing** (durable cordon) | B8, B9, the stale cordon, a lost cordon | `Bump`; skip the cordon delete; keep the cordon in memory only |
| T-8 | the real start sweep against a fake container runtime holding two owner labels | B7 | share the data dir |
| T-9 | golden of the Driver's call sequence for the frontend, plus the existing suite | a refactor changing both sides | reorder two stages |
| T-10 | all-in-one with a remote daemon: a long action finishes on the old process; its output readable through the successor; an `Execute` longer than every keepalive completes (container, x86) | the whole contract | kill the old process at `released` |
| T-11 | an action finishes after the cut, local and remote: its outputs upload to `i`, its `Result` is accepted once, nothing is written into `p`'s stores | unacknowledged results, a write into a demoted store | keep the local data plane |
| T-12 | `i` dies after `released`: `p` goes unready within `--handover-watchdog-secs` (failpoint), a cold start serves, callers of the old one get answers | the unrecoverable window; `p` chosen while refusing everything | the old exits at `released`; a watchdog that never fires |
| T-13..15 | injected ENOSPC at prepare refuses and the old keeps serving; `T_s` over the budget refuses the next update; the short-fence flag: under budget zero reruns, over it the leases fence and rerun and no caller sees an error from the old one | disk-full reversibility, the budget, the fence interaction | raise the fence; record no `T_s` |
| T-16 | SIGKILL a detached old: the successor is unaffected, `precheck` refuses the restart, `--acknowledge-crash` clears | restart into takeover | write the marker after `released` |
| T-17 | a load of writes, reads and `Execute` calls running across the whole cut: every call completes, none is refused by `p` (a daemon redial is not a call), the longest call is within `T_s` plus its normal latency | any hold, park or refusal in the sequence | add a sleep in the cut; refuse writes at the route flag |

A kbf-it test pins that `/readyz` answers without a token on the API listener.

Failpoints and the short-fence flag live behind a cargo feature with a gate test that fails when a release build enables
it. No test pins a wrong answer.

## 14. PR split

#857, #879 and #887 are merged on arch-v2, so every PR stacks on arch-v2; none waits for a re-implementation of the
farm half. PRs that touch `frontend.rs` wait for the answer to Q12. PR 1, R, E1 and E2 are in progress and not merged at
this refresh. Sizes are estimated changed lines.

| PR | content | size |
|---|---|---|
| 0 | spike, section 12, not merged | 500 |
| | decision (Q5) after PR 0 | |
| E1 | envoy kit `Box` topology: render, config keys, `Server` by host and port, new checks, E1/E2 | 700 |
| E2 | real-Envoy tests E3-E5, E7 against the render | 500 |
| 1 | `Drain` staged, the API body and status, T-D, T-A | 400 |
| R | the Execute relay beside `kbf-front/src/proxy`, T-R | 600 |
| 2 | `handover::Driver` out of `frontend.rs`, T-9; the farm half exists (merged), so this adds the takeover's cordon write and own-row delete, T-7's restart case, and `Keep` for an all-in-one start | 500 |
| 3 | frontend wiring: `--instance`, `--envoy-admin`, predecessor call, ports, settle and unready | 400 |
| 4 | `kbf-deploy`: unit templates and per-instance drop-ins, `kbf-envoy.service` with the restarter, binary path, `kbf handover`, lock file; daemon jittered redial | 700 |
| C | dropped: #879 delivered the durable `cordon/<worker>` row and its recovery; the takeover write moved to PR 2 | 0 |
| 5 | E6, E8 and T-17 for the frontend | 400 |
| M1 | kbf-store: one read-only scan shared by `Store::open` (which adds the repairs) and the follower; read-only `Fs` view; T-M1 | 400 |
| M2 | kbf-server: `open_follower`, `Follower::catch_up`, cursor, rotation, version gate; T-M2..4, T-M8 | 600 |
| M3 | `Store::open_from`, `Raft::restore_applied` and `Host::open_resumed`, `Follower::promote`, `RaftMetaLog::demote` (closed-then-host-then-unlock, `Mutex<Option<File>>`, the `still_open` check in `raise_machine_version`); T-M5..7, T-M10..13, T-1 | 900 |
| M5 | snapshot-aware follower, after the snapshot PRs of `metadata-snapshots.md` land; T-M9 | 300 |
| 6-9 | all-in-one: `FsStore::demote`, the wiring of M1-M3 (flags `--meta-follow-interval-ms`, `--meta-follow-lag-max`), T-3, `Cache::seal`, read-only tier open and `HandoverStorage` (T-4, T-5, forwarded commits), the detached daemon with the remote data plane and per-instance data dir (T-8, T-11), the wiring, cordon reset and capacity (T-6, T-7, T-12..16) | about 3500 |
| 10 | all-in-one `kbf-it` T-10, T-12, T-17, container CI placement | 500 |

PRs 6 to 9 are inert until their flags are given. Everything from PR M1 and PR 6 on is gated on the spike's `T_s` answer, and the spike carries a throwaway follower (read-only scan, tail, open from a cursor) so the tail-only time of 6.4.2 is measured, not estimated. M5 waits on the snapshot PRs; until then the follower refuses a directory with a snapshot.

## 15. Open questions

1. **Is a bounded in-call switch time acceptable as "no request is held"?** The design has no park, queue,
   admission gate or refusal flag, but it does hold a call that lands in the switch for up to `T_s` (gate: p99 under 1 s), and says so as a residual, not as an absence. A single-copy store
   with an exclusive writer lock cannot do better, so literal zero wait exists only in split mode (storage nodes routed and
   replicated, frontend relay only). Recommendation: accept `T_s` with the gate; if the spike misses it, all-in-one is
   not updated in place and the supported no-wait shape is `kbf frontend` plus storage and daemon nodes (Alternative C).
2. **The longest action.** Cap an action's timeout farm-wide (`--max-action-timeout`) so Envoy's parent-shutdown time is a
   derived number, or accept that an action longer than it is cut by an Envoy hot restart. Recommendation: cap, default 6 h
   (the Execute-route idle timer is off either way, 3.1; with the cap a hot restart is blocked up to 6.25 h, so renewal must not use it, Q3).
3. **Certificate renewal at Envoy**: a file-watched SDS secret (`path_config_source` on a watched file, no xDS server; does the
   owner count it as xDS?) or hot restart. Recommendation: the file-watched secret: a hot restart can be blocked for hours by a long
   action (section 7), and a renewal inside that window is an expiry incident. (The split-mode note also leans to a file-based secret source.) If the answer is no, hot restart with an alert.
4. **Cgroup caps across two all-in-one units** (`actions/` `cpu.max`, `--actions-memory-max-gib` are per unit).
   Recommendation: one `actions/` parent shared by both instances; verify before the capacity PR.
5. **Is all-in-one worth the storage work?** Alternative C already updates a fleet with the frontend half and a no-wait
   storage update. Recommendation: decide after PR 0; build the frontend half first either way.
6. **Cold store and the PUT lock** (6.3): **decided in the note** as the split lock (exclusive open and sweep, shared put) so
   in-flight PUTs are not in `T_s`; fallback `T_put` with its own gate if the spike finds it unsafe. Open for the owner only if `file://` is to be dropped
   instead. Recommendation: keep `file://`, split lock.
7. **Node id per instance**: two certificates (no change to `peers.admit`) against a suffix rule. Recommendation: two certificates.
8. **The split-mode update note's E4 to E6** (successor "accepts and holds"; old goes NOT_SERVING at the offer) contradicts 5.1 and
   5.2. This note's version is the later decision; that note needs the same relay and the same readiness order.
9. **#743** (control plane as calls, paused): if it lands the detached daemon's control plane becomes a feature, not a redial.
   Recommendation: do not land it before this is built.
10. **Health check kind for `kbf_reapi`**: the gRPC check (kit today; on the REAPI port, over TLS) against an HTTP
    `/readyz` check on the API port. Recommendation: the gRPC check, it is already specified and checked and is the same evaluation.
11. **Forwarded caller identity in the relay**: forward request metadata and let `i` authorize again (this note), or forward a signed
    assertion from `p`. Recommendation: forward and re-authorize; `i` trusts nothing `p` says.
12. **Keep the merged fd-passing path (#879/#887) as a no-Envoy shape, or remove it?** It is on arch-v2 now, with the
    units and a crash matrix (#900) in flight. Recommendation: keep the farm half, the `Fenced` generation and `open_after_handover`
    (the Envoy path uses them) and remove the control socket and fd-passing wire once the Envoy path has shipped for the
    frontend; a second update path is a second thing to test and to explain. Until then both exist and Envoy hosts do not
    use the wire. Waiting on the owner's answer; PRs touching `frontend.rs` wait for it.
13. **Single voter assumption of the follower** (6.4): the follower treats a whole CRC-valid record as committed because
    durable equals committed with one voter. Recommendation: accept it and record it in the follower's module docs and a
    test that fails if `RaftMetaLog` is configured with more than one voter; a multi-voter log reopens 6.4.
14. **Durable-watermark file** (6.4.1) for a follower that outlives the writer's machine: not taken, because the follower and
    writer share a kernel. Recommendation: do not build it unless meta moves off the local file system.
15. **Compaction first or follower first?** Snapshots bound the first pass and the fallback (6.4.4) but are a larger change.
    Recommendation: follower first (it removes replay from `T_s`); snapshots when the background first pass or the fallback
    is measured too long.

Reviewed and not taken: (a) readiness off before the cut with Envoy's health check as the only gap cover (5.2: a window with
no healthy backend, a local 503, longer than Buck2's retry window); (b) `p` stays the storage server and `i` writes through it
(6.3: a second hop for hours and the same switch later); (c) a refusal of calls after the cut that Envoy retries (a hold under
another name, and needs a retry policy on `Execute`); (d) routing the worker port by instance number without Envoy (every
update would change a remote daemon's address); (e) L4 for REAPI (new streams on an existing connection would stay on the
old process); (f) a Welcome-carried extended daemon fence as the default (it weakens the only partition safeguard); (g) retries
on `ByteStream` and `BatchUpdateBlobs` (Envoy buffers the request); (h) queue or park modes of any kind (the project's decision).
