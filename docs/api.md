# The operator API

`kbf-server` serves an HTTP/JSON API under `/v1` for the Fleet UI, scripts and
clients ([fleet-updates.md](design/fleet-updates.md) section 11.4), and
[`/healthz` and `/readyz`](#get-healthz-and-get-readyz) for a front's health check (the REAPI
listener answers the same readiness over [`grpc.health.v1`](#grpchealthv1-on-the-reapi-listener)), and
[`/metrics`](#get-metrics), the cache's upload-path metrics in the Prometheus text format. It listens on its
own address, given with `--api-listen`; without the flag there is no API. The start
line then ends with ` api=<addr>`. The listener speaks HTTP/1.1, and HTTP/2 in clear
text (h2c, with prior knowledge); every rule below holds for both.

Reads answer anyone who reaches the address: bind it where only operators do. The
exceptions are the build pages, [`GET /v1/invocations/{id}`](#get-v1invocationsid-and-below)
and below, and [`GET /v1/blobs`](#get-v1blobsdigest), which run the REAPI policy and are
off without it. Writes need a token (see [Who may write](#who-may-write)). The roles of
[fleet-updates-security.md](design/fleet-updates-security.md) section S9 (`admin` for
operators, a short-lived `rollout` for CI) are **planned**; the token is the one
operator credential until they land.

Every body is `application/json`, except a blob's bytes and `/metrics`. What the server keeps here is
**in memory**, like the scheduler's state: a restart forgets it, and each daemon
reports again after its next `Welcome`.

## Who may write

A write (cordon, drain, uncordon) can take every node in the fleet out of service, so
it must come from an operator. Where a request comes from does not show that:

- **Every server host is also a worker.** Build actions run on it, and they can reach
  loopback: the native driver on Linux enforces no network isolation, and the macOS
  sandbox allows loopback.
- **A reverse proxy on the host** (`tailscale serve`, nginx) makes every caller it
  forwards a loopback peer.
- **A browser on the host** can POST to a loopback address from any page it opens,
  without a preflight, if the API accepts a "simple" request (`text/plain` or a form
  body, or no body).

So a write must pass every one of these gates, in this order:

| Gate | Refused with |
|---|---|
| the peer is loopback, so the token never crosses a network in clear | `403` |
| the request has no `Origin` header (browsers send one with a cross-origin POST) | `403` |
| the server was started with `--api-token-file` (without it, writes are off) | `403` |
| `Authorization: Bearer <token>` with the file's token (the SHA-256 digests of both are compared in constant time, so neither the token's bytes nor its length show through timing) | `401`, with `WWW-Authenticate: Bearer` |
| `Content-Type: application/json` (parameters allowed), which a page cannot send cross-origin without a preflight the API never answers | `415` |

**The token file.** `--api-token-file <path>` names a file holding one token of at
least 32 visible ASCII characters (surrounding whitespace, such as the trailing
newline, is dropped), in at most 4096 bytes; `openssl rand -hex 32` makes one. The
file must be a regular file (a FIFO or device is refused at once, without waiting on
it) owned by the server's user with mode exactly `0600` or `0400`; otherwise the
server refuses to start. The token is read once at start: it is never on the command
line, in the start line or in a log, and a change to the file takes effect at the
next start.

**Run `kbf-server` as its own user.** Keep the token out of the build actions'
reach: a build action that can read the file can write. On a host that is also a
worker, `kbf-server` must run as a different user from `kbf-daemon` and from every
user leases run as. The native driver runs actions as the daemon's user, so a `0600`
token file owned by a user the server shares with the daemon is readable by every
build on that host.

**Through a reverse proxy.** A remote operator reaches the API through a proxy on the
server's host that terminates TLS (`tailscale serve`, nginx). The proxy is what makes
the loopback gate pass, so the token is what stands between the network and the
writes: forward the `Authorization` header unchanged, and expose only what the
operators' network should reach. A proxy that adds an `Origin` header to every
request turns writes off.

## `GET /v1/nodes`

The server that answers, then every node registered since it started, in node-id
order, with its software and where it is in placement:

```json
{ "server": { "version": "0.1.0+0123456789ab", "commit": "0123456789ab" },
  "nodes": [
  { "node_id": "mac-1", "connected": true, "software": {
      "os_name": "macOS", "os_version": "15.1", "os_build": "24B83", "kernel": "",
      "daemon_version": "0.1.0", "xcode_builds": ["15F31d", "16C5032a"],
      "xcodes": [
        { "app": "/Applications/Xcode_15.4.app", "build": "15F31d", "state": "ready",
          "reason": "", "fix": "" },
        { "app": "/Applications/Xcode_16.1.app", "build": "16B40",
          "state": "license_not_accepted",
          "reason": "/usr/bin/xcodebuild -license check exited with exit status: 69: You have not agreed to the Xcode license agreements.",
          "fix": "sudo /Applications/Xcode_16.1.app/Contents/Developer/usr/bin/xcodebuild -license accept" },
        { "app": "/Applications/Xcode_16.2.app", "build": "16C5032a", "state": "ready",
          "reason": "", "fix": "" } ],
      "container_images": [],
      "received_at_unix_ms": 1791370000000 },
    "needs_attention": [
      "Xcode 16B40 (/Applications/Xcode_16.1.app) installed but not ready: /usr/bin/xcodebuild -license check exited with exit status: 69: You have not agreed to the Xcode license agreements.; fix: sudo /Applications/Xcode_16.1.app/Contents/Developer/usr/bin/xcodebuild -license accept" ],
    "placement": { "state": "draining", "deadline_unix_ms": 1791371800000,
                   "leases": ["117399224320012061.42"] } },
  { "node_id": "old-1", "connected": false, "software": null, "needs_attention": [],
    "placement": { "state": "serving" } }
] }
```

| Field | Meaning |
|---|---|
| `server.version` | the `kbf-server` build that answers, as `--version` and the start line print it: the package version, `+`, and the commit |
| `server.commit` | the commit it was built from, 12 hex digits (`git rev-parse --short=12`), or `unknown` for a build without a git checkout |
| `node_id` | the id its daemon registered with |
| `connected` | whether its newest stream is still open |
| `software` | the newest `NodeStatus` it sent ([worker-protocol.md](design/worker-protocol.md#nodestatus)); `null` from a daemon that predates it. An empty string or list is a value the node could not read |
| `software.received_at_unix_ms` | when the server received it, by the server's clock |
| `software.xcode_builds` | the Xcode builds actions can use on it: only these route work |
| `software.xcodes` | every installed Xcode, ready or not: `app`, `build` (empty if not known), `state` (`ready`, `license_not_accepted`, `first_launch_not_run`, `metal_toolchain_missing`, `failed`, `not_surveyed` from the daemon's start until its first survey of its Xcodes ends (never advertised; the daemon sends its status again when the survey ends), or `unknown` for a state this server does not know), `reason` (the check that failed and its answer, from the daemon's last survey that changed this Xcode's `DEVELOPER_DIR`, build or state; a survey that differs only in its reason is not sent, so when another check later fails with the same build and state, this is still the earlier check's text) and `fix` (the command an administrator runs on the node, when one is known). Empty from a daemon that predates it |
| `software.container_images` | the container images the node was pinned to (`kbf-daemon --image`, container driver), as `docker://<repo>@sha256:<digest>`, sorted: each was in its image store, as one architecture's manifest, when the daemon started (a daemon that finds one missing does not start). Status only: no work is placed by image. Empty when none is pinned, on a Mac, and from a daemon that predates it |
| `needs_attention` | what an operator must do on the node, one line per item: today each installed Xcode that is not ready, as `Xcode <build> (<app>) installed but not ready: <reason>; fix: <command>`. An Xcode `not_surveyed` keeps the item the same app had in the node's previous status (as when a restarted daemon sends its first status), until a survey reports it; one with no previous item has none. The server also logs each item once at `WARN` (target `kbf_server::attention`) when it appears, and at `INFO` when a status reports it gone (its Xcode ready, changed or removed); a status that repeats an item, or lists its Xcode `not_surveyed`, logs nothing. An item is the same while its Xcode's app, build, state and fix are, so a reason that changes alone is shown here but not logged again. The previous status is kept in the server's memory: after a server restart, each item is logged again when its node reports it; kbf has no alert delivery yet (issue #189) |
| `placement.state` | `serving`; `cordoned` (no new lease, its leases run on); `draining` (cordoned, waiting for its leases until `deadline_unix_ms`); `drained` (cordoned, no lease left: either its leases ended, or the node disconnected and, after the lease grace, its leases were given up and requeued to run elsewhere; check `connected`); `drain_paused` (the deadline passed with leases still running: they run on, and nothing proceeds until an operator acts) |
| `placement.leases` | while draining or paused: the leases it still holds, as `term.seq` (each server process has its own term, the Raft term under the Raft metadata log: [worker-protocol.md](design/worker-protocol.md#server-restarts-and-the-lease-epoch)) |

## `POST /v1/nodes/{node}:cordon`, `:drain`, `:uncordon`

An operator takes a node out of placement, drains it, or returns it
([fleet-updates.md](design/fleet-updates.md) sections 3.3 and 4.2):

- `:cordon`: the scheduler offers the node no new lease; its leases run on. A drain
  already under way is kept.
- `:drain`: cordons the node and waits for its leases to end. The body may be
  `{"deadline_secs": N}`; without a body the deadline is 30 minutes. With no lease
  left the node is `drained`. If the deadline comes first the drain **pauses**: the
  leases run on, nothing is killed or given up, and the node stays `drain_paused`
  even after they end, until an operator drains it again (a new deadline) or
  uncordons it.
- `:uncordon`: returns the node to placement and ends any drain.

The answer is the node, as `GET /v1/nodes` lists it, after the action. `404` for a
node that never registered or an unknown verb, `400` for a drain body of another
shape, and `401`, `403` or `415` from the gates of [Who may write](#who-may-write).

A cordon names the node, not its stream: a node that reconnects (say, after a reboot)
is still cordoned. Work that only cordoned nodes could run waits, and its callers are
told so (`every live worker that can run it is cordoned: <nodes>`). It is **not**
refused for it, however long the cordon lasts: a cordon is temporary by intent, and
refusing would fail builds. An uncordon places it at once. The scheduler's unservable
wait (`--unservable-wait-secs`) still refuses work that no connected node, cordoned
or not, could run, and the time spent waiting for a cordon does not count toward it.
A server restart forgets every cordon, as it forgets the rest of the scheduler's
state, and every start logs a warning that says so. The protocol has no drain
message yet: the daemon is not told, and the server simply sends it no new `Start`.

## `GET /v1/frontend/drain` and `POST /v1/frontend/drain`

Draining the frontend itself, for a rollout that restarts it (the nodes are drained
with `:drain` above). Buck2 does not recover an Execute stream that breaks after it
started, so a restart that cuts a stream fails its action: the frontend is drained, and
restarted when it has no stream left.

**`GET /v1/frontend/drain`**: a read, open like `GET /v1/nodes`:

```json
{ "draining": false, "open_executions": 3, "queued_operations": 1 }
```

- `draining`: a drain has begun, by this endpoint or by SIGTERM or SIGINT. It is not
  undone.
- `open_executions`: Execute and WaitExecution operation streams open now
  (`kbf_front_open_executions`: a stream counts from the moment it is made, before its
  first operation is read, until it ends or its client leaves).
- `queued_operations`: operations in the scheduler's queue, with no lease yet. With
  `open_executions` it tells idle (both 0) from busy, and a stream that waits for a
  worker (`open_executions` above 0, `queued_operations` above 0, no lease running)
  from one that runs. `kbf_front_executes_started_total` (see the metrics below) tells a
  client that has just come from one that is still there.

**`POST /v1/frontend/drain`**: begins the drain, a write that passes every gate of
[Who may write](#who-may-write) and takes no body (or `{}`), and answers the same status
with `draining` true. Calling it again changes nothing; SIGTERM or SIGINT after it
changes nothing either. The drain:

- turns `/readyz` to `503` (`stopping`) and the health service to `NOT_SERVING`;
- refuses every new Execute `UNAVAILABLE`, which Buck2 retries when it opens a call;
- sends the REAPI listener GOAWAY, so it takes no new connection;
- keeps every open Execute and WaitExecution stream until its action is done;
- ends the server, exit 0, when the REAPI connections have closed. The worker listener
  and this API stay up until then, so daemons go on reporting results.

A frontend that has nothing open exits at once, after answering the request. What ends
a drain that waits too long: `--shutdown-timeout-secs` when a signal began it (the
default waits for ever), or a second SIGTERM or SIGINT, which ends every open stream
`UNAVAILABLE` (the action fails) and stops the server within a few seconds. Under
systemd that means `TimeoutStopSec=infinity` in the unit: a stop timeout would SIGKILL
the drain, and the unit templates in `kbf-deploy` set it.

`kbf cluster` (on the frontend's host) calls this API: `kbf cluster status [--frontend]`,
`kbf cluster drain <node> [--deadline-secs N] --token-file F`, `kbf cluster drain
--frontend --token-file F` and `kbf cluster undrain <node> --token-file F` (the API's
`:uncordon`). It prints the answer's JSON, exits 0 on a 2xx and 1 otherwise, and takes
`--api` for the listener (default `127.0.0.1:8982`). A frontend drain has no undrain:
restart it.

## `GET /healthz` and `GET /readyz`

For the health check of a front or proxy (a TLS-terminating ingress, Envoy) in front
of the REAPI listener. Both are reads, open like `GET /v1/nodes`, and are served only
when `--api-listen` is given. The routes are served by the same task as the REAPI
and worker listeners, which are bound before it starts and stop when it ends, so any
answer means both are bound.

**`GET /healthz`**: the process is alive. It reads nothing, and answers `200` for as
long as the server runs, during a stop too:

```json
{ "status": "alive", "version": "0.1.0+0123456789ab", "commit": "0123456789ab" }
```

**`GET /readyz`**: the server is ready to serve. `200` when every check passes, `503`
otherwise; the body lists each failing check and why:

```json
{ "ready": false, "version": "0.1.0+0123456789ab", "commit": "0123456789ab",
  "failing": [ { "check": "store",
                 "reason": "a read of kbf/1791370000000000000/readyz-probe had no answer within 2000 ms" } ] }
```

| Check | Fails when |
|---|---|
| `stopping` | the server has received SIGTERM or SIGINT, or `POST /v1/frontend/drain` has begun a drain. It answers `503` from that moment, and for the rest of the drain; open Execute streams are kept until their action is done, and only the shutdown timeout (`--shutdown-timeout-secs`) or a second signal ends them |
| `leader` | the server does not hold the scheduler role. A single server runs every role and always holds it; the check is there for a replicated control log, whose followers fail it. **Planned**: a second readiness path, without this check, for the servers that may serve reads ([deployment-topology.md](design/deployment-topology.md#build-clients-one-name-routed-by-method)) |
| `store` | the object store does not answer a one-byte read of the key `<prefix>readyz-probe` within `--readyz-store-timeout-ms` (default 2000; it bounds the gRPC health service's probe too), or answers with an error. The probe only reads: no poll writes, deletes or lists. Nothing writes that key, so "not found" is the expected answer and passes, as does the object's bytes or a range past its end |

`version` and `commit` are those of `GET /v1/nodes`' `server` field. Each `/readyz`
makes one read of the store, so poll it at the interval the front needs, not faster.

## `grpc.health.v1` on the REAPI listener

For a proxy that health-checks its backends over gRPC (Envoy's `grpc_health_check`, a
gRPC load balancer), the REAPI listener (`--listen`) serves the standard
[gRPC health checking protocol](https://github.com/grpc/grpc-proto/blob/master/grpc/health/v1/health.proto),
`grpc.health.v1.Health`, whether or not `--api-listen` is given, and over the
listener's TLS when `--reapi-tls-cert` and `--reapi-tls-key` are given. Its answer is
`/readyz`'s: `SERVING` when every check of the [table above](#get-healthz-and-get-readyz)
passes, `NOT_SERVING` when one fails. It gives no reason; `/readyz` names the failing
checks.

It answers for these service names, all with the same status: `""` (the server as a
whole), `build.bazel.remote.execution.v2.Capabilities`,
`build.bazel.remote.execution.v2.ContentAddressableStorage`,
`google.bytestream.ByteStream`, `build.bazel.remote.execution.v2.ActionCache` and
`build.bazel.remote.execution.v2.Execution`.

| Method | Answer |
|---|---|
| `Check` | the status, after one store probe (as one `/readyz`). Another service name is `NOT_FOUND` |
| `List` | every name above with the status, after one store probe |
| `Watch` | the status at once, then each time it changes. It evaluates again as soon as a stop signal arrives or the scheduler role changes, and probes the store every 5 s. Once the server is stopping it sends `NOT_SERVING` (if that was not its last message) and ends the stream `UNAVAILABLE`, so an open Watch does not hold up the drain. Another service name is sent `SERVICE_UNKNOWN`, and the stream stays open until the server stops |

On SIGTERM or SIGINT the status turns `NOT_SERVING` before the REAPI streams are
ended and the listener stops accepting. After that the listener takes no new calls,
so a `Check` during the drain fails or times out instead of answering; either way the
proxy marks the server down.

The REAPI authentication policy (`--reapi-auth-policy`, [reapi-auth.md](reapi-auth.md))
does not run on these calls: a health check needs no credentials, whatever the policy.
Every other call on the listener, a path it does not serve included, still runs it.
The store probe waits at most `--readyz-store-timeout-ms`, like `/readyz`'s.

**Planned:** with several servers, the answers follow the two readiness paths, ready
to serve reads and leader; which service name reports which is not chosen
([deployment-topology.md](design/deployment-topology.md#build-clients-one-name-routed-by-method)).

## `GET /metrics`

The cache's upload path in the Prometheus text exposition format (version 0.0.4,
`Content-Type: text/plain; version=0.0.4; charset=utf-8`): what the CAS services did,
and how long each stage of a store took. A read, open like `GET /v1/nodes`; it names
no blob, build or node. Every listener that serves the cache counts into the same
families: the REAPI listener (clients) and the worker listener (daemons' uploads of
outputs and reads of inputs). The counts are in memory and start at 0 with the
process.

Each request family counts from the start of its handler to its answer. A call whose
caller goes away before the answer counts as an error.

| Family | Type | What it measures |
|---|---|---|
| `kbf_bytestream_write_requests_total`, `_errors_total` | counter | ByteStream Write calls answered (or dropped), and those that failed |
| `kbf_bytestream_write_bytes_total` | counter | blob bytes received, counted as each message arrives, so a slow Write shows progress before it ends |
| `kbf_bytestream_write_in_flight` | gauge | Writes begun and not yet answered |
| `kbf_bytestream_write_seconds` | histogram | Write latency, end to end |
| `kbf_bytestream_write_present_total` | counter | Writes answered at once because the blob was already durable |
| `kbf_batch_update_requests_total`, `_errors_total`, `_bytes_total`, `_in_flight`, `_seconds` | as above | BatchUpdateBlobs calls; `kbf_batch_update_blobs_total` counts the blobs they sent |
| `kbf_find_missing_requests_total`, `_errors_total`, `_in_flight`, `_seconds` | as above | FindMissingBlobs calls; `kbf_find_missing_digests_total` the digests asked about, `kbf_find_missing_missing_total` those answered missing |
| `kbf_execute_requests_total`, `_errors_total` | counter | Execute calls ended, and those that ended in an error status or were dropped by their caller before the done operation (an operation that finished with a failed `ExecuteResponse` is not one) |
| `kbf_execute_in_flight` | gauge | Execute and WaitExecution streams open |
| `kbf_front_open_executions` | gauge | Execute and WaitExecution operation streams alive: raised when the stream is made (before its first item is read), lowered when it ends or its caller drops it. A call still being authorized or prepared, and one answered from the action cache, is not in it. `GET /v1/frontend/drain` reports it |
| `kbf_front_executes_started_total` | counter | Execute calls the front took (not refused because it is draining), cache hits included |
| `kbf_execute_queued` | gauge | Execute streams the scheduler accepted whose operation has not left `QUEUED` |
| `kbf_execute_prepare_seconds` | histogram | an Execute from its arrival to the call of the scheduler: authorization, action cache lookup, reading the action and its inputs, the held-inputs check, the booking; observed only for an Execute that reaches the scheduler |
| `kbf_execute_dispatch_seconds` | histogram | the scheduler's submit call, from the call to its return |
| `kbf_execute_queued_seconds` | histogram | from the scheduler's acceptance to the stream's first update that is not `QUEUED` |
| `kbf_execute_total_seconds` | histogram | an Execute from its arrival to its last response, its error or the departure of its caller; cache hits included |
| `kbf_store_blobs_seconds` | histogram | one store of verified blobs (a Write's, or a batch's): the query for blobs already held, their touch, and the wait until every segment holding its blobs is written: the linger, the wait for a PUT slot, each PUT attempt with the backoff between attempts, the footer read, and the commit with any wait behind commits queued ahead of it |
| `kbf_segment_put_seconds` | histogram | one PUT attempt of a segment's object; a PUT tried again is observed once per attempt |
| `kbf_segment_footer_read_seconds` | histogram | one segment's footer read back after its PUT: two range GETs, the trailer and then the footer |
| `kbf_segment_commit_seconds` | histogram | one segment's `PutBlobs` commit to the metadata log, including any wait behind commits queued ahead of it |
| `kbf_segment_blobs`, `kbf_segment_bytes` | histogram | blobs, and bytes (footer included), per segment written |
| `kbf_object_get_seconds` | histogram | one range GET of a blob being read (ByteStream Read, GetActionResult, a BatchReadBlobs run); footer reads are not in it |
| `kbf_meta_commit_seconds{command}` | histogram | every metadata-log commit the cache makes, by command (`put_blobs`, `touch`, `put_action`, `object_unreachable`, `tick`, `collect`, ...): with the Raft metadata log, the Raft log's commit latency |
| `kbf_meta_query_seconds` | histogram | every metadata-log query the cache makes |
| `kbf_execute_presubmit_seconds` | histogram | an Execute from the handler's start to its first stream item (the first `QUEUED` operation cannot be sent sooner): authorization, the action-cache lookup, the action and command reads, the input-tree walk and the scheduler's submit. Failed and abandoned calls are observed too |
| `kbf_execute_presubmit_in_flight` | gauge | Executes inside that window now: during a stall with idle slots, this is the number of requests the scheduler has not seen |
| `kbf_execute_walk_seconds`, `kbf_execute_dir_reads_total` | histogram, counter | the input-tree check of one Execute, whole (directories read concurrently or taken from memory, subtrees skipped, presence checks, any wait for a check another request is running), and the Directory blobs read from the store, counted as each read starts |
| `kbf_execute_submit_seconds`, `kbf_sched_lock_wait_seconds` | histogram | the scheduler's submit called from an Execute, and the part of it spent waiting for the scheduler's lock |
| `kbf_sched_queued`, `kbf_sched_leases` | gauge | operations waiting for a lease, and leases granted or running on all nodes, as of the last submit or scheduler tick (at most a tick old) |
| `kbf_tier_in_flight`, `kbf_tier_wait_seconds`, `kbf_tier_call_seconds` | gauge, histogram | write-tier file calls queued on or running on the blocking pool; the wait of one for a pool thread; and one whole, hand-over to answer. Always printed, 0 without a write tier; a wait near the call time says the pool is full |
| `kbf_input_directories_cached_total` | counter | Directories the input check took from the decoded-directory cache: the blob is touched and its presence checked, nothing is read |
| `kbf_input_directories_skipped_total` | counter | Directories skipped with everything under them: an earlier check verified them and the retention guarantee of that check still runs |
| `kbf_input_directories_joined_total` | counter | Directories found being checked for another request, and waited for |

Latency buckets run from 1 ms to 60 s; size buckets from 1 KiB to 1 GiB by fours;
the blobs-per-segment buckets from 1 to 16384 by fours.

With a local write tier (`--hot-store`, [write-path.md](design/write-path.md#6-the-local-write-tier)),
these families follow; without one they are absent. An acknowledged blob on the
tier is on that server's disk and not yet in the object store, so the unflushed
families are what a disk loss would take, and the oldest age is what an alert
watches.

| Family | Type | What it measures |
|---|---|---|
| `kbf_write_tier_staged_total` | counter | segments staged: synced to a block file, then committed with `PutStaged` |
| `kbf_write_tier_full_total`, `kbf_write_tier_errors_total` | counter | segments written to the store instead, because the tier was at its budget, or its disk failed before the commit |
| `kbf_write_tier_flushes_total`, `kbf_write_tier_flush_failures_total` | counter | staged objects flushed (PUT, footer read back, `ObjectFlushed` committed), and flush attempts that failed and are retried after a wait (1 s doubling to 60 s) |
| `kbf_write_tier_flush_conflicts_total` | counter | staged objects whose key in the store held another object: they stay staged and are read locally until an operator acts (alert on any) |
| `kbf_write_tier_corrupt_total` | counter | staged objects with a record that failed its digest on the tier's disk: not uploaded, marked unreachable, healed by a re-upload |
| `kbf_write_tier_local_reads_total` | counter | blobs read from the tier's disk (a block, or a flushed copy) |
| `kbf_write_tier_evicted_total` | counter | flushed copies evicted, least recently read first |
| `kbf_write_tier_budget_bytes`, `kbf_write_tier_used_bytes` | gauge | the hot store's budget, and the bytes counted against it: blocks not yet flushed, reserved ranges included |
| `kbf_write_tier_flushed_copy_bytes` | gauge | flushed copies kept for local reads, in what the budget leaves over |
| `kbf_write_tier_unflushed_bytes`, `kbf_write_tier_unflushed_blocks` | gauge | acknowledged bytes, and the blocks holding them, not yet in the object store |
| `kbf_write_tier_oldest_unflushed_seconds` | gauge | the age of the oldest of those blocks (since it was created, or since the restart that recovered it); 0 when there is none |

In `kbf-all-in-one`, when the hot store is a directory on the filesystem of `--data-dir`,
the daemon half's input cache and the server half's hot store are one cache
([architecture-v2.md](design/architecture-v2.md#10-locality)): the input cache's
directory is the node's only local copy of a blob, the tier deletes a block once it is
flushed (so `kbf_write_tier_flushed_copy_bytes` stays 0), and the input cache evicts its
entries until they and the tier's blocks fit its budget: after each input it fills, and
within about a second of the tier growing past what the budget leaves it (uploads alone).
Entries a running lease pins are never evicted, so a hot store budgeted at or above the
input cache's budget leaves room for those alone (the daemon warns once). These families
follow; in every other process they are absent.

| Family | Type | What it measures |
|---|---|---|
| `kbf_local_cache_input_bytes` | gauge | the input cache's entries |
| `kbf_local_cache_storage_bytes` | gauge | what local storage holds on that disk: the write tier's blocks |
| `kbf_local_cache_fill_bytes` | gauge | both, each byte once: what the one budget counts |

**Reading a stall with idle slots.** `kbf_sched_queued` high and `kbf_sched_leases` below the
nodes' slots means work is waiting that placement cannot use (look at the node's booking and
`GET /v1/nodes`). `kbf_sched_queued` low while buck2 has many Executes open means they have not
reached the scheduler: `kbf_execute_presubmit_in_flight` counts them, `kbf_execute_walk_seconds`
and `kbf_execute_submit_seconds` (with `kbf_sched_lock_wait_seconds`) split where they wait, and
`kbf_tier_wait_seconds` near `kbf_tier_call_seconds` says the blocking pool is the queue. An
Execute that spends 10 s or more before its first operation is also logged at `WARN` with its
action, instance, invocation id and those phase times.

**Reading a slow upload.** Each ByteStream Write sends one blob, so a daemon's output
tree of N files costs N Writes, but not N segments: the cache groups blobs from
concurrent Writes and batches into one segment, sealed when it is full or
`--write-linger` after its first blob, and a blob of `--write-own-segment-bytes` or
more gets a segment of its own. Over a window, with `rate()`:

- Writes per second is `rate(kbf_bytestream_write_requests_total)`; with
  `kbf_bytestream_write_in_flight` near the daemon's upload limit, it is that limit
  over the mean Write latency
  (`rate(kbf_bytestream_write_seconds_sum) / rate(kbf_bytestream_write_seconds_count)`).
- The mean of each stage (`_sum` over `_count` of `kbf_segment_put_seconds`,
  `kbf_segment_footer_read_seconds`, `kbf_segment_commit_seconds`) says which one the
  Write waits on: the object store's PUT, its GETs, or the metadata log. A PUT tried
  again counts once per attempt, so `kbf_segment_put_seconds` can count more than
  the segments written (`kbf_segment_blobs`' count). What `kbf_store_blobs_seconds`
  holds beyond the three is the held-blob query and touch, the linger, the wait for
  one of `--write-puts-in-flight` PUT slots, and the backoff between PUT attempts.
  What a Write holds beyond its store is the wait for admission under
  `--write-bytes-in-flight`, receiving and hashing the bytes and authorizing the
  caller (none timed on its own), and the check whether the blob is already held: its query and any touch also show in `kbf_meta_query_seconds` and
  `kbf_meta_commit_seconds{command="touch"}`.
- `kbf_segment_blobs` with a mean of 1 says no grouping happened: uploads arrived
  more than a linger apart, or were large enough for segments of their own, so each
  paid a PUT, two GETs and a commit of its own.
- `kbf_bytestream_write_present_total` against the requests says how many uploads
  were of blobs already held.

The daemon logs the other side, per lease: its phase times (`lease phases:`) and,
from its transfer engine, what it read and uploaded (`transfers:`), joined on `lease=`
([daemon.md](design/daemon.md#where-a-leases-time-went)). The server logs, per lease
report that carries usage (an OK run or any failed one the daemon measured, for
example an out-of-memory kill, a timeout or a lost run), `action usage:` with the same cgroup fields, the action's
shape `key`, its `cfg`, the `outcome` and the `lease`, and names the `key` on each
`lease granted` line ([daemon.md](design/daemon.md#the-servers-action-usage-line)).

## `GET /v1/invocations/{id}` and below

The build pages: what the farm did for one build, read from the invocation records
([invocations.md](design/invocations.md#4-the-read-api)). The records are kept **in
memory** within `--invocation-memory` (section 3.5 there): a restart forgets them, and
at the bound idle records are removed. A record gone, or never made, is `404` on every
route.

| Route | Answers |
|---|---|
| `GET /v1/invocations/{id}` | the invocation: its header fields and counters, links to the routes below, its slowest actions and its phase totals |
| `GET /v1/invocations/{id}/actions` | a page of its action rows, in the order first seen; `?outcome=ok\|failed\|timed_out\|refused`, `?how=cache_hit\|cache_hit_execute\|joined\|ran`, `?requeue_reason=<reason>`, `?memory_kill=none\|own_limit\|node_pressure`; `?sort=wall_ms\|execution_ms\|queue_ms\|input_fetch_ms\|output_upload_ms`, longest first |
| `GET /v1/invocations/{id}/actions/{digest}` | one action's row, with the outputs, stdout, stderr and usage copied into it, and whether each blob is still held |
| `GET /v1/invocations/{id}/artifacts` | a page of the outputs of its actions, in row order |
| `GET /v1/invocations/{id}/logs` | a page of the stdout and stderr of its actions, failed actions first |
| `GET /v1/invocations/{id}/metrics` | its aggregates |

**Off unless the REAPI policy names it.** Pages hold build outputs and logs. The routes
run the policy of `--reapi-auth-policy` ([reapi-auth.md](reapi-auth.md)): its
`authenticationPolicy` on the request, then its `invocations.getAuthorizer` with every
instance name the invocation's calls used; all must allow (a refusal is `403`).
Without that key, or without a policy file, every request is `404` and the start line
says ` invocations=off` (before ` blobs=`); with it, ` invocations=on`.
`--invocations-open` turns the routes on for every caller the policy accepts (and the
blob route with them), for a listener only trusted people reach.

**`{id}`** is an invocation id as a client sent it (1 to 128 bytes of
`[A-Za-z0-9._-]`, starting with a letter or digit) or `<id>~<n>` (`n` from 2), the
record of another caller of the same id ([invocations.md](design/invocations.md#24-ids-are-claims-not-secrets)).
It is matched as the bytes of the path: a percent-encoded character (`%3C`, `%6F`) is
`404`, as is anything else.

**The invocation:**

```json
{
  "id": "0b8c...", "url": null,
  "tool": "buck2", "tool_version": "0.1.0", "correlated_invocations_id": null,
  "instances": [""],
  "first_seen": "2026-10-11T09:00:00.000000Z", "last_seen": "2026-10-11T09:04:12.031008Z",
  "state": "open", "complete": true, "rows_capped": false, "actions": 812,
  "counts": {"execute": 812, "cache_hit_execute": 40, "joined": 3, "ran": 769, "failed": 2,
             "ac_hit": 5120, "ac_miss": 812, "blobs_asked": 9000, "blobs_missing": 300,
             "bytes_uploaded": 1234567, "bytes_downloaded": 89012345},
  "collisions": ["0b8c...~2"],
  "links": {"actions": "/v1/invocations/0b8c.../actions", "artifacts": "...", "logs": "...",
            "metrics": "..."},
  "phase_totals": {"execution_ms": {"sum": 912345, "rows": 769},
                   "queue_ms": {"sum": 20431, "rows": 769}},
  "top_slowest": {
    "wall_ms": [{"action": "<digest>", "wall_ms": 61234, "href": "/v1/invocations/0b8c.../actions/<digest>"}],
    "execution_ms": [{"action": "<digest>", "execution_ms": 60011, "href": "..."}]}
}
```

Times are RFC 3339 in UTC to the microsecond; durations are integer microseconds;
digests are `<sha256 hex>-<size>`. `state` is `idle` after 10 minutes with no event and
no operation in flight, else `open`. `url` is `null` (the link template,
`--invocation-url-template`, is planned).

**`phase_totals`** sums, over the rows the farm ran for this invocation (`how` =
`ran`), each phase's milliseconds (see `durations` below; `wall_ms` is not summed):
`sum`, over the rows that measured the phase, and `rows`, how many did (a mean is
`sum / rows`); a phase no such row measured is left out. **`top_slowest`** lists, for `wall_ms` and for
`execution_ms`, the 20 such rows that took longest, longest first (ties in row order),
each with its `action`, its milliseconds and its `href`. Both are computed from the
rows at each request.

**An action row** (on `/actions`, and as the start of `/actions/{digest}`):
`action`, `instance`, `target`, `mnemonic`, `configuration` and `action_id` (each
`{"value", "truncated"}` or `null`), `how`, `operation`, `attempts` (each with `lease`,
`worker`, `queued`, `granted`, `started`, `ended` and `end`: `{"result": "ok"}`,
`{"requeue": "<reason>", "reason": "<text>"}` or `{"failure": "<class>", "reason":
"<text>"}`, `null` while held), `outcome` (`{"ok": {"exit_code": n}}`, `{"failed":
{"code": n, "message": "..."}}`, `{"timed_out": {}}`, `{"refused": {"reason": "..."}}`,
or `null` in flight), `result` and `command` (digests or `null`) and `href`, the row's
own route; then the keys added since (none of the keys above changed):

- `shape`: what the server read from the action's `Command` when it was queued, or
  `null` (a cache hit, or a Command not read): `key` (the shape key, 16 hex digits, as
  the `action usage:` line writes it), `cfg` (the configuration hash an output path
  named, or `null`) and `target`, the target the action's own `buck-out` paths name
  (`{"label": "cell//pkg:name", "truncated", "kind": "target"|"anon"|"bxl"}`), or
  `null` when they name none (a remotely run test whose outputs sit under
  `buck-out/v2/test/execution/` and whose program is not a `buck-out` path): it is
  read, never guessed. The row's `target` above is what the client's
  `RequestMetadata` said, a hint only (buck2 sends none).
- `durations`, in whole milliseconds, each left out when not measured: `wall_ms`, from
  the first lease's queueing to the end of the lease that ended the operation
  (requeues included); `queue_ms`, queued to granted summed over its leases so far;
  `execution_ms`, `input_fetch_ms` and `output_upload_ms`, each `start` to `completed`
  of the REAPI times of the run of the lease that ended the operation. All but
  `queue_ms` are left out until a lease ends the operation (accepted or failed; a
  requeued lease's end is not the last), and for one that ended with no lease ending
  it, so once given they never change; `queue_ms` only grows, by each lease granted
  after a requeue.
- on each attempt: `memory_kill` once it ended (`none`, `own_limit` or
  `node_pressure`, read from how it ended); `booked_memory_bytes` for a lease killed
  for memory (the one that ended the operation too), `cap_bytes` for an own-limit kill
  (the largest node that could run it), `raised_memory_bytes` for an own-limit
  requeue (the next booking); and, for a lease whose report the scheduler took,
  `reapi_times` (each of REAPI's nine `ExecutedActionMetadata` times it has, by name
  without `_timestamp`, as its stamped result would carry them: `queued` is the
  operation's first submission, and the worker start and completion are the server's
  when the daemon sent none) and `usage` (`wall_us`, each cgroup counter the daemon
  read: `cpu_usage_us`, `cpu_user_us`, `cpu_system_us`, `cpu_periods`,
  `cpu_throttled`, `throttled_us`, `memory_peak_bytes`; `parallelism`, CPU time over
  wall time; `numa_bytes`, `[{"node", "bytes"}]`). A key not measured is left out,
  never zero.

A timed-out run's stdout and stderr (those the CAS holds) are its row's `stdout` and
`stderr`; its `result` stays `null`, since a partial result is never stored. `/actions/{digest}` adds `outputs` (each `path`, `digest`, `executable`,
`directory`, `available` and `href`), `outputs_truncated`, `stdout` and `stderr` (each
`{"digest", "available", "href"}` or `null`), `usage` (or `null`) and `blobs`
(`action` and `command` links). An output directory is listed as its Tree digest.

**`available`** says whether the cache still holds the blob, read from its index
without touching it (a page keeps no blob); `null` if the index does not answer.
**`href`** links the blob route, `/v1/blobs/{digest}?instance=<the row's
instance>&name=<file name>`, with `&inline=text` for a log of at most 4 MiB; every
`href` is `null` while the blob route is off.

**`/artifacts`** lists each output as an output of `/actions/{digest}` is, with its
`action`. **`/logs`** lists each action whose result has a stdout or a stderr:
`action`, `href`, `failed` (anything but OK with exit code 0), `outcome`, `stdout` and
`stderr`. The failed ones come first, in row order, then the others.

**`/metrics`**: `counts` as above; `queue_micros` (queued to granted) and `run_micros`
(granted to ended) over the leases of the rows the farm ran for this invocation
(`how` = `ran`; a joined row's leases count where its twin ran), each with `count`,
`min`, `p50`, `p90`, `max` and `sum` (`null` when empty); `reruns` by requeue reason;
`usage_actions`, the rows whose result carries usage, and over them `cpu_micros`
(user plus system) and `peak_memory_bytes`, each `{"sum", "max"}`, `null` when no row
does. They are computed from the rows at each request. `p50` and `p90` are approximate:
never below the true value and at most 1/16 above it (exact below 16 µs); the rest are
exact.

**Filters and order of `/actions`.** `?requeue_reason=` keeps the rows with a lease
given up for that reason (`silent`, `replaced`, `reconnected`, `not_started`,
`session_switch`, `out_of_memory`, `node_memory_pressure`, `restarted`, `lost`,
`infra`), whatever the action's end: an action killed for its own limit, run again and
then OK is kept by `out_of_memory`. `?memory_kill=own_limit` (or `node_pressure`) keeps
the rows with a lease killed that way; `none`, the rows with no lease killed for
memory, cache-hit rows (which have no leases) included. `?sort=<phase>` lists the rows that measured that phase (`durations` above),
longest first, ties in row order; its pages resume after the last row listed. No row
is listed twice; a row whose `queue_ms` grows past that point after it was passed (it
was requeued and granted again) is not listed again, and a row that measures the
phase only after the pages passed its place (its operation ended since) is not listed.
Filters combine, and `next_page` carries them all.

**Pages.** `/actions`, `/artifacts` and `/logs` take `?page_size=` (100 by default,
1 to 1000) and `?page_token=`, the `next_page_token` of the page before, with the same
filter; the answer is `{"actions": [...]}` (or `artifacts`, `logs`) and, unless it is
the last page, `next_page_token` and `next_page`, the route's path with the same filter,
page size and that token: the link to follow. A token is opaque: it names the record it
was made for, its filter and a position. Rows are only appended, so a build still
writing never makes a page skip or repeat a row. Outputs and logs are listed as each
row held them when a page passed it: an action still running then is not listed when
it ends (read its row, or start again). On `/logs`, rows added after the failed ones
ran out are listed with the rest, failed or not.

| Code | When |
|---|---|
| `200` | the answer |
| `400` | a parameter the route does not take, one given twice, a `page_size` outside 1 to 1000, a filter value not listed above, or a page token that does not decode, was made for another route or another filter, or names a position past the rows |
| `401` | the policy's `authenticationPolicy` refuses the request |
| `403` | `invocations.getAuthorizer` refuses one of the invocation's instance names: each one its calls used and each one a row holds, on every route |
| `404` | the routes are off: `{"error": "<why>", "off": "invocations"}`; no such record is held (never made, removed at the memory bound, or lost at a restart), a page token's included even if a record has been made since under the same key: `{"error": "no_record", "id": "<id>", "records_since": "<time>"}` (below); no such route, `{id}` is not an id or `<id>~<n>`, or no row for the action: `{"error": "<why>"}` |
| `503` | the record gained an instance name while it was read: send the request again |

**A record that is not held** answers `{"error": "no_record", "id": "<id>",
"records_since": "<time>"}` ([invocations-api.md](design/invocations-api.md#410-a-record-that-is-gone)).
`records_since` is the later of the server's start and the `last_seen` of the newest
record of a client's id removed at the memory bound (removed minted records do not
count): a build whose calls went on after it would still be held, so its `404` means
the server saw no call with that id; a build that ended before it may have been lost.
The `off` body of a route that is switched off is never this one.

## `GET /v1/blobs/{digest}`

A CAS blob's bytes over HTTP, for a browser: the outputs, stdout and stderr a build's
page links to ([invocations.md](design/invocations.md#46-blobs-over-http)).

```
GET /v1/blobs/<sha256 hex>-<size>?instance=<instance name>&name=<file name>&inline=text
```

**Off unless the REAPI policy names it.** Unlike the rest of this API, the route does
not answer whoever reaches the address: blobs hold build outputs and logs, which can
hold secrets. It runs the policy of `--reapi-auth-policy`
([reapi-auth.md](reapi-auth.md)): its `authenticationPolicy` on the request, then its
`blobs.getAuthorizer` with the `instance` parameter. Without that key, without the
build pages' `invocations.getAuthorizer` (blobs are what the pages link to, and are off
with them), or without a policy file, every request is `404` and the start line says
` blobs=off` (before ` api=`), with the body `{"error": "<why>", "off": "blobs"}`;
with both, ` blobs=on`. `--invocations-open` fills in `allow` for either key the file
leaves out.

| Parameter | Meaning |
|---|---|
| `{digest}` | the blob: 64 lowercase hex digits, `-`, the size in bytes |
| `instance` | the instance name the authorizer is asked about; absent is the empty name. The cache does not separate blobs by instance ([reapi-auth.md](reapi-auth.md#authorizers)), so it gates the call only |
| `name` | the file name a browser saves the bytes under; absent or empty sends none |
| `inline=text` | show the bytes in the browser as text instead of downloading them; `text` is the one value |

A parameter not in this table, one given twice, or another `inline` value is `400`.

**The bytes.** The blob is read whole through the cache (ByteStream Read reads in pieces
and checks as it goes, so it holds no blob): the whole blob is checked against its digest before a byte is sent (a stored
object whose bytes are wrong is `503` and is never sent), and the read counts as a use
of the blob for its retention, as a ByteStream Read does. The answer is
`application/octet-stream` with `Content-Disposition: attachment`, and with `name`,
`attachment; filename*=UTF-8''<name>`: the name percent-encoded as RFC 8187 says, so a
quote, a `;` or a line break in it can never end the value or add a parameter. With
`inline=text` it is `text/plain; charset=utf-8` and `inline` (with the same
`filename*`), for a blob of at most 4 MiB (4194304 bytes); a larger one is `400`, and
still downloads without `inline`.

**Memory.** Like a ByteStream Read, a read holds the whole blob in memory, because the
cache keeps one digest per blob and can check only all of it; a second read to stream
from would send bytes nobody checked. So the route keeps a budget: the blobs of all the
answers it is reading or still sending hold at most 1 GiB (1073741824 bytes, the largest
blob the cache accepts) at once. An answer gives its share back when its bytes have
been sent or dropped (the client went away), and a failed read gives it back at once.

- **Waiting.** A read whose blob does not fit in what is left waits in line, first come
  first served, before a byte is read: a large read at the head of the line is not
  passed by smaller ones behind it, so a stream of small reads cannot starve it (and
  they wait while it does). A read still waiting after 10 seconds is `503` with
  `Retry-After: 1` and the body `{"error": "<why>", "busy": true}`; a blob that cannot
  be read is `503` without either, so a client can tell busy from broken.
- **Too large.** A digest that claims more than 1 GiB is `404` at once, with the limit
  in the error: the cache refuses such a blob on write, so it never holds one, and no
  retry can change that. ByteStream Read answers the same digest `NOT_FOUND`.
- **Slow clients.** A client holds its blob's share until its answer is sent or it goes
  away; the listener has no write timeout.

**Never a page.** Every answer of the route, the errors and `404` included, carries
`X-Content-Type-Options: nosniff` and `Content-Security-Policy: sandbox`, so stdout that
holds HTML is shown as text and never runs as a page on the API's origin.

**Ranges.** One range of `Range: bytes=...` (`a-b`, `a-`, `-n`) is answered `206` with
`Content-Range: bytes a-b/<size>`; a last byte past the end is read as the end. A range
that starts at or past the end (or any range of an empty blob) is `416` with
`Content-Range: bytes */<size>`. Several ranges, another unit or a malformed value are
ignored, as RFC 9110 allows, and the whole blob is sent. Every `200` and `206` says
`Accept-Ranges: bytes`.

| Answer | When |
|---|---|
| `200`, `206` | the bytes, as above |
| `400` | a malformed digest or query, or `inline=text` for a blob over 4 MiB |
| `401` | the authentication policy refuses the request |
| `403` | `blobs.getAuthorizer` refuses the instance name |
| `404` | the route is off, the blob is not held, or the digest claims more than 1 GiB |
| `416` | the range starts past the end |
| `503` | busy: the blob's share of the memory budget was not free within 10 seconds (`Retry-After: 1`, `"busy": true`); or the blob is held but its bytes cannot be read (missing from the store, or failing their digest), or the store or the metadata log does not answer |
| `500` | the cache's own records disagree |

Errors are JSON, `{"error": "<why>"}`, as everywhere in this API.

## `STALE_MAP` on a storage node's gRPC calls

Not part of the HTTP API: the answer a storage node (`kbf storage`) gives over gRPC to a
caller that routed a call by an older storage map than the node holds
([architecture-v2](design/architecture-v2.md) section 6). It is listed here because
callers (the frontend, the daemons) and tools read it from the wire.

**The caller's generation** is on every call: field 1 (`map_generation`) of every
`kbf.storage.v1` request (`ActionStore.Get`, `Prepare`, `Commit`), and the ASCII metadata
entry `kbf-map-generation` (the decimal generation) on every other call:
`ContentAddressableStorage.FindMissingBlobs`, `ByteStream.Read`, `Write` and
`QueryWriteStatus`, and each `kbf.worker.v1.Segments` call. A call with no entry is
generation 0, "no map seen". An entry that is not a decimal `u64` is `INVALID_ARGUMENT`.

**The node's judgement**, made before the call reads anything (so no answer, not even
"already stored", is given under a placement the node no longer holds). A node that is
one of several (it is given a source of maps) and has no map yet fetches one the same way
and serves no call, whatever the caller's generation, until it has one: `UNAVAILABLE`. A
node that is not part of a multi-member farm has no such source and judges nothing.

| the caller's generation | the node |
|---|---|
| equal to the node's | serves the call |
| older than the node's (0 included) | answers `STALE_MAP` |
| newer than the node's, or the node has no map yet | fetches the map, waiting at most 2 seconds, then judges again: serves it if the node is now at the caller's generation, `STALE_MAP` if past it; `UNAVAILABLE` if no map came |

**The encoding of `STALE_MAP`:** the gRPC status `FAILED_PRECONDITION` with the trailing
metadata entry `kbf-map-generation: <n>` (decimal), the node's generation. The message
is words for a log and is never matched. `FAILED_PRECONDITION` alone is not `STALE_MAP`:
an action-cache write refused for a closure blob the node does not hold answers that code
too, and carries no such entry. A reader takes the status for `STALE_MAP` only with both
the code and the entry, and takes the entry for a number only if it is decimal.

**What the caller does:** fetches a map of generation `<n>` or newer (the next `MapUpdate`
on its session, or a `MapRequest`: [worker-protocol.md](design/worker-protocol.md)) and
runs the call again, once. A second `STALE_MAP`, or a fetch that brings no newer map, is
treated as `UNAVAILABLE`. `UNAVAILABLE` for a caller ahead of the node is the node's
lost push: the caller's map may be the right one, so the caller tries again, and the
node's next push makes the call succeed.

## `UNAVAILABLE` from a storage node that is catching up

Also gRPC, not HTTP. A storage node that is rebuilding its index (it registered with
`catching_up` set in `kbf.map.v1.RegisterRequest`, and is `CATCHING_UP` in the map) may
hold a blob it has not indexed yet, so it never answers that something is missing:

| call | while catching up |
|---|---|
| `ByteStream.Read` of a blob it does not find | `UNAVAILABLE`, not `NOT_FOUND` |
| `FindMissingBlobs` with any digest it does not find | the whole call `UNAVAILABLE` (an answer naming fewer digests would say the rest are held) |
| `ActionStore.Get` with no entry | `UNAVAILABLE`, not an empty answer |
| `ActionStore.Prepare` or `Commit` refused for a closure blob it does not find | `UNAVAILABLE`, not the `ABSENT` refusal |

Every other answer is the usual one: hits are served, writes are taken, and the other
refusals stand. The message begins `catching up:`; it is words for a log and is never
matched. A caller treats the node as an owner that could not say, and asks the next owner
(a router answers `NOT_FOUND` only if every owner did). The node stops at the first
heartbeat after its rebuild that sends `catching_up` false: the frontend makes it `ACTIVE`
in the next map generation.

## Planned

`/v1/software`, `/v1/rollouts` and the per-node update state of
[fleet-updates.md](design/fleet-updates.md) section 11.4.
