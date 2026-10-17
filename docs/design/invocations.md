# Invocations: a build's id, its record, its API and its page

**The primary consumer is an agent**: an automated client, such as an AI coding agent
or a CI bot, that runs a build, reads the `Build ID` buck2 prints (or the link in a
failed action's message, 5.2), and diagnoses the build through the API: which actions
failed and why, their stderr and stdout, the command that ran and where, the artifacts,
and what each action used. It should get each of those in one or two calls, through a
client that speaks the API for it ([What an agent needs](invocations-api.md#what-an-agent-needs), 4.8).
The HTML page (5.1) is a convenience for a person and is built last (section 9).

This document designs the server side of that: the id, the record, where the record is
kept, the read API, the client and the link. A rich UI is out of scope; the API is what
an agent, a script or a UI reads. The agent-facing part (what an agent needs, the
failures view 4.7, the client 4.8, read tokens 4.9 and a record that is gone 4.10) is
in [invocations-api.md](invocations-api.md), under the same section numbers.

**Everything in this document is planned** unless it says "today". Section 1 lists what
exists.

Vocabulary: an **invocation** is one run of a build tool (one `buck2 build`, one
`bazel test`), REAPI's word for it. The page a person opens is the *build page*; its
record is the *invocation record*.

## What an agent needs

Moved to [invocations-api.md](invocations-api.md#what-an-agent-needs): the flows an
agent runs (did it pass, which actions failed and why, their logs, the command, the
artifacts, the metrics), each in one or two calls, and the three limits it must know.

## 1. What exists today

Read on `main`:

- **The id is read and recorded in memory.** `kbf_front::RequestMetadataLayer` reads
  every REAPI call's `RequestMetadata` header into a `CallMetadata` (section 2.2), and
  Execute puts its `tool_invocation_id` on its `execute` trace span (`invocation`, `-`
  when absent). The services and the farm send each call that carries an id, and the
  lease events and outcome of its operations, to `kbf_front::Recorder` (3.4), which
  keeps the records in memory within `--invocation-memory` (3.5, stage 1). The read
  API of section 4 serves them (`kbf_server::http_invocations`, PR 4), off unless the
  policy names `invocations.getAuthorizer` or `--invocations-open` is given (4.3); its
  `url` is `null` until `--invocation-url-template` (PR 6). A call without an id is
  not recorded (the minted id of 2.3 comes with its link, PR 6).
- **Per-operation state lives about a minute.** The scheduler keeps a finished operation
  for `FINISHED_RETENTION` (60 s, `kbf-sched/src/scheduler.rs`); then WaitExecution
  answers NOT_FOUND. All scheduler state is in memory and is lost on restart.
  `FarmMachine` (`kbf-server/src/machine.rs`) holds the waiters, the started leases and
  the node registry beside it, with the same retention.
- **Reruns are log lines.** `kbf-sched/src/requeue.rs` names each requeue (`Silent`,
  `Replaced`, `Reconnected`, `NotStarted`, `SessionSwitch`, `OutOfMemory`,
  `NodeMemoryPressure`); they are drained to tracing and not stored.
- **Results are durable, unindexed.** Every OK result, a non-zero exit included, is
  stamped (`Stamp`: worker, queued, worker start and completion times) and stored as an
  `ActionResult` blob in the CAS (`Cache::prepare_action_result`). Its outputs, stdout and
  stderr are CAS digests inside it. An action-cache entry (`result: Digest`) is written
  only for exit 0 without `do_not_cache`. A failed action's result blob therefore has no
  index entry pointing at it once the operation is dropped.
- **Usage is carried, rarely filled.** `kbf.worker.v1.ResourceUsage` (user and system CPU,
  peak memory of one process, wall time) rides in
  `ActionResult.execution_metadata.auxiliary_metadata`. Only the test-only local runtime
  fills it; the native and container drivers do not
  ([scheduler.md, Accounting](scheduler.md#accounting)).
- **Retention.** `kbf_meta::Retention`: blobs at least 7 days after their last touch,
  touches at most once a day, action-cache entries 30 days. Collection is never committed
  by the server and segments are never deleted
  ([storage.md, Planned](storage.md#planned)); **Pins**, named retention roots, are
  planned there too.
- **A blob over HTTP** (PR 5, `kbf-server/src/http_blobs.rs`): `GET /v1/blobs/{digest}`
  on the operator API serves a CAS blob's verified bytes, `?inline=text` up to 4 MiB,
  under the REAPI policy's `blobs.getAuthorizer`, and is `404` without that key or
  without `invocations.getAuthorizer` (4.3, 4.6).
- **The operator API** ([api.md](../api.md)) serves `GET /v1/nodes`, the node actions,
  `/healthz`, `/readyz`, the build pages and the blob route on `--api-listen`, HTTP/1.1
  or h2c, without TLS. Its other reads answer anyone who reaches the address; the REAPI
  auth policy ([reapi-auth.md](../reapi-auth.md)) runs on the build pages and the blob
  route only (4.3). Its writes need the `--api-token-file` token and a loopback peer.
- **No credential is checked on a read.** The policy file builds the `allow`, `deny`,
  `any` and `all` authentication policies only (`kbf-auth/src/policy.rs`; `jwt`,
  `tlsClientCertificate` and the rest are refused as not built yet). None reads a
  credential, so a request is accepted or refused whoever sends it: an agent on
  another machine can read only a server open to everyone who reaches it (4.9).
- **A timed-out action's logs are not listed.** The farm answers a timeout with the
  partial ActionResult the daemon sent, but the recorder keeps a listing only for an
  action that ran to an exit (`kbf-server/src/farm/record.rs`), so a timed-out row
  has no stdout or stderr digest (PR 12 lists it).
- **The client text channel is unused.** `ExecuteResponse` is built with `result`,
  `cached_result` and `status` only; `message` and `server_logs` are empty.

## 2. The id

### 2.1 Where it comes from

REAPI clients send a binary header,
`build.bazel.remote.execution.v2.requestmetadata-bin`, holding a serialized
`RequestMetadata`: `tool_details` (name, version), `action_id`, `tool_invocation_id`,
`correlated_invocations_id`, `action_mnemonic`, `target_id`, `configuration_id`.
`tool_invocation_id` "ties multiple actions together to a final result": it is the
invocation id.

**The invocation id is the client's `tool_invocation_id`.** The server does not mint one
for a client that sends one. The reason is that a client prints only its own id: a
server-minted id cannot reach the user without a client-side hook (section 5).

What the pinned clients send. buck2's row is captured on the wire (a buck2 build of the
M1 sample project against a kbf-server; the header is kbf-front's golden test,
`crates/kbf-front/tests/data/buck2-execute.requestmetadata`); Bazel's is read from
source and not yet captured:

| Client | `tool_invocation_id` | other fields |
|---|---|---|
| buck2 (release 2026-10-01, captured) | the UUID it prints as `Build ID: <uuid>` | `tool_details` = `buck2`, version `0.1.0`; `action_id`, `correlated_invocations_id`, `action_mnemonic`, `target_id`, `configuration_id` all empty |
| Bazel (from source) | the command id (`--invocation_id`) | `correlated_invocations_id` = build request id; `action_id`, mnemonic, label, configuration when known; real version |

In that capture buck2 sent the same header on every GetActionResult, FindMissingBlobs,
BatchUpdateBlobs, BatchReadBlobs and Execute call; the build was too small to use
ByteStream. Read from source, it also sends it on UpdateActionResult and ByteStream Read
and Write, not on GetCapabilities, and it does not call WaitExecution. A caller can
choose the id ahead of time: buck2 takes it from `BUCK_WRAPPER_UUID` when set (seen in
a second capture: the printed `Build ID` and the header's id were the value given).

### 2.2 Accepting it

- Parsed once per call by a tower layer in `kbf-front`, beside (not inside) the
  authentication layer, and put in the request's extensions as a `CallMetadata`.
- **The id's characters are `[A-Za-z0-9._-]`, 1 to 128 bytes, and the first is a letter
  or digit.** buck2's UUIDs, Bazel's UUIDs and the server's own `kbf-<uuid>` fit. The set
  has no character that needs escaping in a URL path, and none that HTML or a header
  treats specially, so the id is used in a URL or a `Location` as it is. It also leaves
  `~` free as the separator of 2.4, so no client id can equal a derived key. The rule on
  the first character rules out `.` and `..`, which browsers and proxies normalize away
  in `/build/..`, so such an id's link would resolve to another page.
- **A bad header never fails a call.** A header over 8 KiB, one that does not decode, or
  an id that is empty, longer than 128 bytes, has a character outside that set or does
  not start with a letter or digit is
  treated as absent and counted (`kbf_invocation_metadata_invalid`). A build must not
  break because of bookkeeping. "Over 8 KiB" is the decoded `RequestMetadata`; a bad
  id alone drops the id and keeps the header's other fields. Today the count is read
  from the layer (`RequestMetadataLayer::invalid_count`) and each case is logged at
  debug level; no endpoint exports it (farm-wide metrics are a separate design, 4.4).
- Text fields (`target_id`, mnemonic, configuration) are kept up to 512 bytes each and
  truncated past that, marked truncated.

### 2.3 The fallback when no id is sent

A call without an id is not attributed to any client invocation. What happens depends on
the call:

- **Execute**: the server mints an id, `kbf-<uuid v4>`, for that one operation: an
  invocation of one action. Its link goes back in the answer's messages (section 5.2),
  which is the only way the user learns it. **A minted invocation is kept in memory
  only**, in both stages of 3.5: it is never written as a chunk and never pinned, so an
  id-less client costs no log traffic per action. It is the first evicted (3.5), and is
  dropped one hour after its operation ends; its page then answers `404`.
- **GetActionResult, CAS, ByteStream**: not recorded per call; counted in farm-wide
  totals only. Minting an invocation per cache read would make one record per call.

The server does not group id-less calls by connection or peer: a buck2 daemon keeps one
connection for every command it runs, so a connection is not a build.

### 2.4 Ids are claims, not secrets

`tool_invocation_id` is chosen by the client. Two consequences:

- **Squatting.** Any caller can send another build's id. The record is keyed on the id
  alone, so the URL needs nothing else, but it keeps the `public` authentication
  metadata of its first caller as its *owner*. A call carrying the same id from a caller
  whose `public` metadata differs is recorded under its own record, keyed
  `<id>~<n>` (n counting from 2), and the owner's page lists those as `collisions`. `~` is
  outside the id's character set (2.2), so `<id>~<n>` never equals an id a client sent;
  the API accepts exactly those two forms in `{id}` and answers `404` to anything else. Without
  a policy file every caller's metadata is empty, so all callers are one owner, as they
  are for authorization today.
- **Not a capability.** A UUID is hard to guess but is printed to consoles and CI logs;
  reading a page is decided by the authorizer (section 4.3), never by knowing the id.

## 3. The record

### 3.1 Per invocation

| Field | Source |
|---|---|
| `id` | section 2 |
| `tool`, `tool_version` | `tool_details` of the first call |
| `correlated_invocations_id` | first non-empty value seen |
| `owner` | `public` metadata of the first caller (2.4) |
| `instances` | instance names its calls used |
| `first_seen`, `last_seen` | server wall clock; `last_seen` is the last *event* of 3.4 (a call, a lease grant, a requeue, a finish), not only the last call |
| `state` | `open`; `idle` after 10 minutes without an event **and** with no operation in flight; never "finished": REAPI has no end-of-build call |
| counters | Execute calls, of which answered from the cache, joined to a running twin, ran, failed; GetActionResult hits and misses; FindMissingBlobs blobs asked and missing; CAS and ByteStream bytes uploaded and downloaded |
| `complete` | false when any event was dropped (3.4) or a server restarted while it was open |

### 3.2 Per action

One row per action digest per invocation, in the order first seen.

| Field | Source |
|---|---|
| `action` | Action digest (SHA-256 and size; kbf accepts SHA-256 only) |
| `instance` | the call's instance name |
| `target`, `mnemonic`, `configuration`, `action_id` | `RequestMetadata`, when sent (buck2 sends none today) |
| `how` | `cache_hit` (GetActionResult), `cache_hit_execute` (Execute answered from the AC, operation `operations/cached/...`), `joined` (attached to a twin's operation), `ran` |
| `operation` | the REAPI operation name |
| `attempts` | each lease: lease id, worker (node id), queued, granted, started, ended, and how it ended: result, or the requeue reason (`requeue.rs`), or the failure class (`log.proto` `Failure`) with the daemon's reason text, capped at 1 KiB |
| `outcome` | `ok` with exit code; `failed` with the gRPC status code and message; `timed_out`; `refused` with the reason |
| `result` | the ActionResult blob digest (from `OpState::Completed`, or the AC entry for a hit) |
| `command` | the Command digest, read from the Action blob by the recorder (off the call path) for every row, cache hits included; absent when the CAS does not hold the Action (buck2 does not upload it for a cache hit) |
| `outputs` | copied from the ActionResult: each output file's path, digest and executable bit, and each output directory's path and tree digest; at most 1,000 per row, then `outputs_truncated: true` |
| `stdout`, `stderr` | the two digests, copied from the ActionResult (absent when the result has none) |
| `usage` | the `ResourceUsage` values, copied when the result carries them |

**The row copies the result's listing, not its bytes.** Paths, digests and usage are
copied when the row is written, so the page can list an action's outputs and logs for as
long as the record is kept (3.6), after the ActionResult blob and the bytes it names have
left the CAS. Only the bytes then become `available: false`. A joined action's attempts are those of the
operation it joined; the row stores the operation name and the attempts are copied into
every invocation that waited on it, so each page stands alone after the operation is
dropped.

The record does not hold the Command (its arguments and environment can hold secrets);
the action detail links the Action and Command blobs like any other blob.

### 3.3 Cost per action

Measured on the `kbf.invocation.v1` encoding by
`crates/kbf-invocation/tests/bytes_per_action.rs`, on a 1,000-action event stream shaped
like a buck2 build (a UUID id and no other `RequestMetadata`, per-caller operation names,
eight nodes, buck2 output paths of 66 to 70 bytes, both log digests on every result, no
`ResourceUsage`). The test builds the stream; none is captured from a client until the
recorder lands (PR 3). Each time is a signed offset in microseconds from `first_seen`,
and each digest its 32 raw bytes and size.

- core row (Action digest, instance, operation name, one attempt with its lease, worker
  and four times, outcome, result digest): 158 B, against the 130 B estimated for delta
  times (the test offsets each time from `first_seen`; the stored chunk offsets them from
  its base time, 3.5);
- plus each output 44 B and its path (estimated 45 B), and 76 B for the stdout and
  stderr digests (estimated 90 B);
- plus a rerun 44 B (estimated 40 B); plus `RequestMetadata` text when sent, not
  measured (buck2 sends none), estimated 50 to 200 B.

The stream's average row (1.875 outputs, a rerun in 50 actions) is 444 B; a
GetActionResult hit with the same listing is 378 B.

So about 300 to 700 B for an action with a few outputs. At the pilot rate of about 4
operations a second (the comment on `FINISHED_RETENTION` in `scheduler.rs`) that is
about 100 to 240 MB a day before cache hits (about 150 MB at the measured 444 B);
recording GetActionResult hits multiplies it by the hit rate. This volume is why the
record is not written into the replicated log (3.5).

**A cap per invocation**: 1,000,000 action rows (about 700 MB at the high estimate).
Past it, actions are counted but not rowed, and the record says `rows_capped: true`.
That is more than the default memory bound (3.5), so in memory the bound is reached
first; 3.5 says what happens then.

### 3.4 The write path

- Each event (call seen, lease granted, requeue, finished) goes to a recorder task through
  a bounded channel. **A full channel drops the event and never waits**: dispatch and
  cache reads are never slowed by the record. A drop marks the invocation
  `complete: false` and increments `kbf_invocation_events_dropped`.
- The channel holds at most 65,536 events and, together, at most `--invocation-memory`
  bytes of them (`Event::bytes`: each event and what it owns, its listing's outputs
  most of all; an estimate, like the records' bytes); past either it drops. Each event's listing is cut where it is built,
  at one output past a row's 1,000 (3.2), so that the row still says
  `outputs_truncated`. So the channel holds no more than the bound, and the recorder
  as a whole (records and channel) about twice it.
- Only calls that carry an id make events, and only operations such a call waits on
  (the farm notes each one as the call's Execute is submitted, and forgets it at the
  operation's end). For any other call or operation a seam costs a branch or a set
  lookup, and no event and no listing is built; with the recorder off, nothing is
  noted.
- The recorder's task applies events on a task of its own, beside the tick and the
  listeners.
- The seams that emit: the new metadata layer (calls and their ids); Execute's submit and
  its cached and joined answers (`kbf-front/src/execution.rs`); GetActionResult
  (`action_cache.rs`); the lease grant and `Sent` in `FarmMachine`; the requeue drain that
  logs today (`take_requeues`); the outcome in `farm.rs`, which also gains the daemon's
  INFRA reason that is now only in a WARN line.
- The recorder holds open invocations in memory. In stage 2 (3.5) it flushes each one's
  new rows as a **chunk** when they reach 256 KiB, when the invocation turns `idle`, or
  120 s after the oldest unflushed row, whichever comes first.

### 3.5 Where it is kept

Two stages:

**Stage 1, in memory** (PRs 3 and 4). The recorder keeps records in memory, bounded by
`--invocation-memory` (default 256 MiB). Records are lost on restart and the page says
so. This is honest and useful for the first deployments, which run the in-memory metadata log (tests only)
and lose the index at restart anyway; stage 2 lands right after the client and the
driver metrics (section 9).

At the bound, in this order:

1. evict minted one-action invocations (2.3), least recently seen first;
2. evict `idle` invocations (3.1), least recently seen first; an invocation with an
   operation in flight is never idle, so a build waiting on one long action is kept;
3. if only open invocations remain, the largest stops taking rows: its actions are still
   counted, and it says `rows_capped: true`. A row that still does not fit is not
   taken, and its invocation says `rows_capped: true` too. One build at the
   1,000,000-row cap (3.3) does not fit in 256 MiB, so in stage 1 a very large build is
   capped by memory first.

The bound holds after every event, for everything an event adds, not only new rows:
rows already taken keep filling in (attempts, the outcome, the listing) after their
invocation stops taking rows. Each addition is admitted like a row, making room in
the order above; what still does not fit is left out, and the invocation says
`complete: false`: a listing keeps the outputs that fit (and the row says
`outputs_truncated: true`); an attempt, an outcome's message or reason, an instance
name, a later Execute's row and a call's wait on its operation are not set; a call
whose instance name is left out is left out whole (no counter, no `last_seen`, no row,
no wait), since a reader is asked about the names the invocation holds; and a new
invocation is not made, so its caller's next call makes it incomplete, as after a
dropped call (3.4).

The bound counts each invocation (its header, its rows and its entries in the
recorder's indexes) and each operation the invocations' calls wait on (its attempts
and its waiting calls), as the bytes the allocator is asked for: the types' sizes,
the row lists' capacity, every copy of an id, and B-tree nodes estimated from the
standard library's node layout; every other list is allocated to its length. Not
counted: the first node of each index (about 2 kB together), the allocator's own
overhead, and the dropped calls of invocations not made yet, held to a sixteenth of
the bound besides it (and to 1,024; past either the oldest is forgotten). A counting
allocator in the tests measures the records' heap against the bound after every
event, across 30 to 300 open builds with twins, requeues, 5 KB failure texts and
1,001-output listings; 10 to 100 builds of 500 to 3,000 actions that each wait
alone; a rolling window of 20,000 operations, each joined by minted twins and
finished 500 later; header-only invocations; and cache hits, at bounds from 200 kB
to 20 MB, and 50 kB for the rolling window. The highest it measured is 1.08 times
the bound at 200 kB and above (the rolling window, at 200 kB) and 1.12 at 50 kB,
where the uncounted first nodes are about 5% of the bound; the tests fail past
1.25.

In stage 2 a flushed row is in a chunk and leaves memory, so the bound covers only
unflushed rows and the header fields; step 3 then applies only when flushes fail
(section 7).

**Superseded (owner, 2026-10-16):** stage 2 below is replaced by `ui-data.md` 5.2:
invocation chunks are kept, cut by the same rule, but as immutable objects under
`history/` in the object store, indexed by rows in the state store, with no root blob,
no pin on the Raft log and no log-format bump. The text is kept as history.

**Stage 2, durable** (PRs 7 and 8). The CEO's ruling (2026-10-12): "Yes we should
switch to durable build records without waiting for HA." So stage 2 runs on today's
metadata log, the Raft metadata log: a single-voter Raft log on the server's own disk
(`kbf-server/src/raft_log`), which a restart already keeps. The pins, with their
closure and their compare-and-set, and the chunks as CAS blobs land now, ahead of the
high-availability design (PR #303, open); that design inherits them as they are,
because they were shaped for a replicated apply from the start (below).

- **What it depends on.** The single-voter log takes no snapshot today and is never
  compacted (`raft_log/machine.rs`: the metadata state has no snapshot encoding), so a
  start replays every entry ever written, pin updates included. Flushes add about 650
  entries an hour at the pilot rate (two per flush, below), so replay grows without bound until the
  metadata-snapshot work lands; that is a separate design note in flight
  (`docs/design/metadata-snapshots.md`), and it is what bounds replay. Stage 2 does not
  wait for it: a longer start is the cost, not a lost record.
- **The in-memory metadata log keeps nothing, by design.** Its index is in memory, so a
  restart forgets every blob and every pin with it; records are lost at a restart
  there, as in stage 1. The binary no longer offers it (it always runs the Raft log,
  `--meta-dir` or `<hot store>/meta`); it remains for tests. Durable records need the
  Raft log.

- A chunk is a `kbf.invocation.v1.Chunk` protobuf, stored **as a CAS blob** through the
  existing `Cache` write path: verified, packed into segments, in the object store like
  any blob. No new storage layer.
- The invocation's root (its header fields and the ordered list of chunk digests) is a
  small blob too, rewritten at each flush.
- **Formats.** Every chunk and every root carries a schema version, the invocation id,
  the chunk sequence number (in a root: the sequence number of its newest chunk) and
  absolute base timestamps; a row's times are deltas from its chunk's base (3.3). The
  root carries, for each chunk, its digest, row count and earliest and latest time,
  and for the invocation the set of targets and of statuses seen, the metrics of 4.4
  precomputed over the flushed rows, and the positions (chunk, offset) of the failed
  rows. So a derived cross-build index (3.7) is filled from roots with no migration of
  the stored records, and the metrics route and the failed-first logs route read the
  root instead of scanning every chunk.
- The log holds one **pin** per invocation, `invocation/<id>`: the Pins of
  [storage.md](storage.md#planned). A pin keeps its root and chunks from collection.
  **The pin command carries its closure.** An update is
  `{id, prev_root, new_root, added_chunk_digests}` with the hold-until time: about 40 B
  per new chunk (a SHA-256 and a size), so a large build does not rewrite its whole
  chunk list in the log at each flush. `MetaState` keeps each pin's current root and
  the closure accumulated from its updates. **Collection parses no blob**: it stays a
  pure function of the replicated state. Today `collect` reads only `MetaState`'s own
  maps (`kbf-meta/src/state.rs`), and in the high-availability design (PR #303, open)
  every replica applies the committed `Collect`; a collection that read roots from the
  object store would do I/O inside a replicated apply, and its result would depend on
  what each replica's read returned (a store unreachable from one replica, a corrupt
  read on another), so replicas could remove different entries. Pinned failed-action outputs
  (3.6) are likewise listed in the pin command, not discovered by parsing.
  The closure is also what is left of the chunk list when a root is lost or corrupt
  (section 7): it names every chunk, and each chunk's sequence number gives the order.
- **A pin update is a compare-and-set on `prev_root`.** It is refused, and changes
  nothing, when the pin's current root is not `prev_root` (empty for the first flush of
  an invocation), or when `new_root` is not in the blob index (the `PutBlobs` that
  stores it commits first). The case it prevents: a deposed leader's update to root R2
  (adding chunk C2) commits after the new leader's recorder has read the pin at R1; that
  recorder writes R3, which lists R1's chunks and its own new one but not C2. Applied
  blind, R3 replaces R2 and C2's rows drop out of the record with no error and no
  `complete: false`. With the check, R3's update names R1, is refused, and the recorder
  rebuilds its root from the pin's current root (R2) and retries (section 7, failover).
- **The log cost of a flush** is two entries: one `PutBlobs` for the chunk and the root
  (`Cache::store_blobs` commits one per segment batch, and both blobs are stored in one
  call) and one pin update: about 200 to 300 B, estimated. With the flush rule of 3.4, a
  farm with N invocations open at once writes at most 30 flushes an hour for each, plus
  one per 256 KiB of rows: at the pilot rate (about 4 actions a second, about 6.4 MB of rows an hour at the
  measured 444 B of 3.3) and N = 10, about 325
  flushes, about 650 entries and under 100 KB of log an hour, against 14,400 rows an
  hour if rows went into the log. A 10 s flush would be twelve times that per open build;
  this is why the time floor is 120 s and an idle build is flushed once.
- This keeps the object store's contract (`kbf-objstore`: kbf never asks a store whether
  something exists; every key is known from the log) because chunks are ordinary CAS
  blobs reached from a pin.
- **The pin command is a new log format**, and the bump is made for the invocation pins
  alone: `log.proto` is closed and `kbf-log` knows `Format::V1` only, and a reader
  already refuses what it does not know (a `Format` other than its own, or a command
  `oneof` it cannot name, is a decode error, fail-stop; `kbf-log/src/lib.rs`). So an
  older server never skips a pin update and goes on with a different state. The
  general Pins of [storage.md](storage.md#planned) (a failure's logs, an operator's
  named root) and the high-availability design (PR #303) add their commands later, in
  their own bumps, on the same rule.

A crash loses at most the unflushed window (up to 120 s or 256 KiB) of each open
invocation; the next server marks those invocations `complete: false`.

### 3.6 Retention

**Superseded (owner, 2026-10-16)** for durable records: retention is by object class
and index-row sweep (`ui-data.md` 5.3), with failure logs pinned as `ui-data.md` 5.4
says; the per-invocation pin below is no longer planned.

- The record: `--invocation-retention` (default 30 days after `last_seen`), the pin's
  hold-until time.
- **What the pin covers** by default: the root and its chunks. Nothing else.
- **The artifacts are not pinned by default.** The ActionResult, Action and Command
  blobs, the outputs, stdout and stderr keep the CAS's own retention (7 days after their
  last touch today). Pinning every output of every build for 30 days would hold most of
  the cache. Because the row copies the listing (3.2), the page still lists every
  output and log for the 30 days, with `available: false` once the bytes are gone; the
  directory contents behind an output tree are not copied, so they are unavailable once
  the tree blob is.
- `--invocation-pin-outputs=failed` (planned) adds to a failed action's pin closure its
  ActionResult blob, its output files and trees, its stdout and stderr: the case worth
  keeping. These digests are listed in the pin command (3.5), so collection still reads
  no blob. Not settled yet: whether the files inside a failed action's output tree are
  pinned too. Collection does not read the Tree blob (3.5), so if they are, the
  recorder reads the tree off the call path and the pin command lists the files; the
  cost (one digest per file) is decided with the Pins design (PR 7).
- Until collection lands nothing is deleted, so retention is a promise of a floor, not a
  ceiling.

### 3.7 Cross-build queries

**Superseded (owner, 2026-10-16):** cross-build queries are now planned: `ui-data.md`
sections 5 and 6 build this derived index as `ui/` rows in the state store, rebuilt
from history objects, with no Raft and no pins.

The routes of section 4 read one invocation by its id, and nothing else. Listing,
search and trends across builds (every build of a target, failures this week) are
**not in this plan**. When they are needed they come from a derived index on each
server: embedded, pure Rust, filled at each flush from the root (3.5) and rebuilt by
walking pins to roots to chunks. It is never authoritative and never replicated through
Raft: a server that loses it, or a new leader after a failover, rebuilds it, and a
wrong index costs a wrong list, never a lost record. It is added when the first list or
search screen is scheduled. Raft holds only the pins.

## 4. The read API

On the operator API listener (`--api-listen`), JSON, the conventions of
[api.md](../api.md). Wall times are RFC 3339 UTC strings; durations are integer
microseconds; digests are `"<sha256 hex>-<size>"`.

### 4.1 Routes

| Route | Answers |
|---|---|
| `GET /v1/invocations/{id}` | the invocation (3.1), with `url`, and links to the routes below |
| `GET /v1/invocations/{id}/failures` | what an agent reads first (4.7): the invocation's state and counts, and a page of its failed, timed-out and refused actions, each with its reason, its attempts, its command and the tails of its stderr and stdout |
| `GET /v1/invocations/{id}/actions` | a page of action rows (3.2); `?outcome=failed\|ok\|timed_out\|refused`, `?how=...` |
| `GET /v1/invocations/{id}/actions/{digest}` | one action: its row, with its outputs, stdout, stderr and usage as copied into it (3.2) and whether each blob is still available |
| `GET /v1/invocations/{id}/artifacts` | a page of output files and directories across its actions |
| `GET /v1/invocations/{id}/logs` | a page of stdout and stderr links, failed actions first |
| `GET /v1/invocations/{id}/metrics` | aggregates (4.4) |
| `GET /v1/blobs/{digest}?instance=<name>&name=<file>` | a blob's bytes (4.6; built, PR 5) |

`{id}` is an id of 2.2 or a derived `<id>~<n>` key of 2.4, matched as bytes; anything else
is `404`. `instance` is a query parameter because REAPI instance names may contain `/`.
A record that is gone (its pin expired, it was deleted, or in stage 1 it was evicted or
lost at a restart) answers `404` on every route, a page token made for it included
(4.5), with the body of 4.10. A route that is switched off (4.3) answers `404` too, but
with another body, `{"error": "<why>", "off": "invocations"}` (or `"blobs"`), never
4.10's: a client must not read "this server serves no records" as "no record of your
build", and the client gives the two different exit statuses (4.8). Today the blob
route's off answer is `{"error": "<why>"}` alone (`kbf-server/src/http_blobs.rs`); PR 4
adds `off`. There is no route across invocations (3.7).

### 4.2 Shapes

```json
GET /v1/invocations/0b8c...-...
{
  "id": "0b8c...",
  "url": "https://build.example.com/build/0b8c...",
  "tool": "buck2", "tool_version": "0.1.0",
  "correlated_invocations_id": null,
  "instances": [""],
  "first_seen": "2026-10-11T09:00:00Z", "last_seen": "2026-10-11T09:04:12Z",
  "state": "idle", "complete": true, "rows_capped": false,
  "counts": {"execute": 812, "cache_hit_execute": 40, "joined": 3, "ran": 769,
             "failed": 2, "ac_hit": 5120, "ac_miss": 812,
             "bytes_uploaded": 1234567, "bytes_downloaded": 89012345},
  "collisions": [],
  "links": {"failures": "/v1/invocations/0b8c.../failures",
            "actions": "/v1/invocations/0b8c.../actions", "artifacts": "...",
            "logs": "...", "metrics": "..."}
}
```

```json
GET /v1/invocations/{id}/actions/{digest}
{
  "action": "e3b0...-142", "instance": "",
  "target": null, "mnemonic": null, "configuration": null,
  "how": "ran", "operation": "operations/7-1042",
  "attempts": [
    {"lease": 88, "worker": "node-a", "queued": "...", "started": "...", "ended": "...",
     "end": {"requeue": "silent"}},
    {"lease": 91, "worker": "node-b", "queued": "...", "started": "...", "ended": "...",
     "end": {"result": "ok"}}
  ],
  "outcome": {"ok": {"exit_code": 1}},
  "result": "9f86...-512",
  "outputs": [{"path": "out/lib.a", "digest": "...", "executable": false,
               "available": true, "href": "/v1/blobs/...?instance=&name=lib.a"}],
  "stdout": {"digest": "...", "available": true, "href": "/v1/blobs/..."},
  "stderr": {"digest": "...", "available": true, "href": "/v1/blobs/..."},
  "usage": {"cpu_user_micros": 1200000, "cpu_system_micros": 80000,
            "peak_memory_bytes": 512000000, "wall_micros": 1500000},
  "blobs": {"action": "/v1/blobs/e3b0...-142", "command": "/v1/blobs/..."}
}
```

`outputs`, `stdout`, `stderr` and `usage` come from the row (3.2), so they are there after
the ActionResult blob is gone; `available` says whether the bytes still are. `usage` is
`null` when the result carries no `ResourceUsage`, which today is every result from a
real driver. An output directory is listed as its tree digest, on the artifacts route
too; expanding a tree there one level per page entry, not recursively, is planned
(today its `href` serves the Tree blob).

### 4.3 Who may read

Build pages hold stdout, stderr and outputs, which can hold secrets, so they cannot have
the operator API's "anyone who reaches the address" rule.

- The invocation and blob routes run the **REAPI auth policy** (`--reapi-auth-policy`),
  the same `Authenticator`: its `authenticate` takes the call's `http::request::Parts`,
  which an HTTP request on the API listener has too. One policy, one set of variants.
- Two new authorizer keys in the policy file: `invocations.getAuthorizer`, asked with the
  invocation's instance names and every row's (all must allow; the recorder never sets
  a row of a name the record does not hold, and the pages do not rely on it), and
  `blobs.getAuthorizer`, asked with the `instance` parameter. Either may be an
  `instanceNamePrefix` or any built authorizer.
- **Fail closed.** If the policy file has no `invocations` key, or there is no policy
  file, the invocation and blob routes answer `404`, with the `off` body of 4.1, and the
  start line says `invocations=off`. The two keys are separate: with `invocations` and no
  `blobs`, the pages answer, their `href`s are `null`, `/v1/blobs` answers that `404` and the start line
  says `blobs=off`. **This is a deliberate exception** to today's policy file, where a
  slot the file does not name is left at allow: these keys guard stdout and outputs, so
  a missing key means off. `--invocations-open` turns them on without a policy (it puts
  `allow` in whichever of the two keys the file leaves out), for a listener
  bound where only trusted people reach it, the same choice the operator API asks for
  today, made explicit.
- Today only `allow`, `deny`, `any` and `all` authentication is built; `jwt` and
  `tlsClientCertificate` are planned. Until they land, a public deployment protects its
  pages with an authenticating proxy in front of the listener, and an agent on another
  machine has no credential to present: 4.9 is what it needs. A planned `invocationOwner`
  authorizer allows a caller whose `public` metadata equals the record's owner. That
  needs a new authorizer input: today an authorizer is asked with an instance name only,
  and this one must also see the record's owner; it lands with its own interface change,
  not in this plan's PRs.
- Writes: none in this design except `DELETE /v1/invocations/{id}` (planned, 3.6, 7),
  under the operator API's existing write rules (loopback peer and token).

### 4.4 Metrics

Per invocation, from the record: in stage 2 the recorder computes them at each flush
and stores them in the root (3.5), and the route adds the rows not yet flushed, so it
reads no chunk. In stage 1 there is no root: the route computes them from the rows the
recorder holds, under its lock, at each request (`Invocation::metrics`); keeping them
updated as each row is written would cost memory the bound of 3.5 would have to count,
for every record, read or not. Percentiles come from a mergeable histogram
(`kbf_invocation::Histogram`), not from sorting rows, so that stage 2 can merge the
root's with the unflushed rows', and so **p50 and p90 are approximate**: the histogram has
power-of-two buckets each split into 16 equal sub-buckets, and a percentile is reported
as the largest value its sub-bucket can hold, so it is never below the true value and at most
1/16 (6.25%) above it (exact below 16 µs). Min, max, sum and counts are exact. The
values: counts as above; queue time and
run time (granted to ended) as min, p50 (approximate), p90 (approximate), max and sum; reruns by reason; CPU-seconds
and peak memory summed and maxed over actions that carry usage, with
`usage_actions: n` saying how many did. Farm-wide metrics (a scrape endpoint) are a
separate design.

### 4.5 Paging

- `?page_size=` (default 100, maximum 1000) and `?page_token=`; the answer carries
  `next_page_token` and `next_page` (the route's path with the same filter, page size
  and that token, the link an agent follows), both absent on the last page.
- Rows are append-only and in first-seen order, so a token is the opaque position
  (chunk, offset) plus the filter it was made for; a page never skips or repeats a row
  while the build is still writing. A token used with another filter is `400`. In
  stage 1 the position is a row index and the token names the record it was made for
  (a serial no other record of the server has had). Outputs and logs are listed as
  each row held them when a page passed it, so an action still running then is not
  listed on the artifacts or logs route when it ends; on the logs route, rows added
  after the failed ones ran out are listed with the rest, failed or not.
- A token used after its record's pin has expired (or the record was deleted or
  evicted) answers `404`, as the record's own route does; a position is never
  resolved against chunks the record no longer holds.

### 4.6 Blobs over HTTP

- `GET /v1/blobs/{digest}` reads through the same `Cache` read path as ByteStream Read,
  so bytes are verified against the digest before they are sent.
- `Content-Type: application/octet-stream` and `Content-Disposition: attachment`, so a
  browser does not render an artifact as a page. `name`, when given, is sent as RFC 6266
  `filename*=UTF-8''<percent-encoded name>`, never spliced in raw.
- `?inline=text` serves `text/plain; charset=utf-8`, up to 4 MiB. The pages link it for
  stdout and stderr only; the route itself cannot tell which blob a digest is, and the
  headers below make any blob safe to show as text.
- Every blob answer carries `X-Content-Type-Options: nosniff` and
  `Content-Security-Policy: sandbox`, so stdout that contains HTML is shown as text and
  never runs as a page on the API's origin.
- `Range` requests are honored. A malformed digest is `400`, an absent blob `404`.
- Each read is a touch, as on ByteStream.

### 4.7 The failures view

In [invocations-api.md](invocations-api.md#47-the-failures-view):
`GET /v1/invocations/{id}/failures`, what an agent reads first.

### 4.8 The client: `kbf build show`

In [invocations-api.md](invocations-api.md#48-the-client-kbf-build-show): one command
that prints the failures view with every link made absolute, and reads any link it
printed.

### 4.9 Reading from another machine: tokens

In [invocations-api.md](invocations-api.md#49-reading-from-another-machine-tokens).

### 4.10 A record that is gone

In [invocations-api.md](invocations-api.md#410-a-record-that-is-gone): the `404` body
on every route, how an agent tells a lost record from one the farm never saw, and how
the gap closes when PRs 7 and 8 land.

## 5. The link

### 5.1 The configured URL

`--invocation-url-template <url>`, for example
`--invocation-url-template 'https://build.example.com/build/{id}'` on one deployment, or
`'http://farm.example:8081/build/{id}'` on another; there is no default and no default
host. Rules:

- It must contain `{id}` exactly once; `{id}` is replaced by the id, whose characters
  (2.2) need no encoding.
- It must be an `http` or `https` URL after substitution. TLS is optional: the listener
  serves clear text, and a deployment that wants `https` puts a TLS-terminating proxy in
  front, as for the operator API today.
- A bad template stops the server before anything is bound (exit 2), naming the flag.
- Without it, the API's `url` fields are `null` and no message carries a link.

The server serves `GET /build/{id}`: `303 See Other` to `/v1/invocations/{id}` (PR 6),
or a minimal HTML page of links (no script) when the request accepts `text/html`
(PR 10). The `Location` is built only from an id that passed 2.2. The HTML page escapes
every value it prints (the id, target, mnemonic, worker, reason texts, paths), carries
`X-Content-Type-Options: nosniff` and a `Content-Security-Policy` that allows no
script. A proxy may map any external path to it.

### 5.2 What a buck2 user sees

**Today, with no client change**, the link is the template with the id buck2 already
prints:

```
Build ID: 0b8c2f0e-...                 <- printed by buck2 at the start
https://build.example.com/build/0b8c2f0e-...   <- the same id in the template
```

buck2 prints `Build ID:` at the start and again at the end with its default console
(superconsole); with `--console simple` it prints it at the start only.

buck2 also writes the id with `--write-build-id <path>` and as `trace_id` in
`--build-report`, so a CI step can print the link after the build.

**On a failed action, the link is printed by buck2 itself.** The server writes

```
Invocation: <invocation url>  Action: <invocation url>/actions/<digest>
```

in two places, because open-source buck2 reads a different one for each kind of failure:

- **`ExecuteResponse.message`, on every Execute answer.** For an OK status with a non-zero
  exit, buck2 shows it as `Info:` under the action's error.
- **Appended to `status.message`, on every non-OK status**: failed, timed out (today
  `the action timed out`, `DEADLINE_EXCEEDED`), missing inputs, and refusals alike. For a
  non-OK status buck2 shows the status message and drops `ExecuteResponse.message`:
  its gRPC client turns any non-OK status into an error that carries the status
  message, and that message becomes the action's error text.
  The link is appended after the existing text, separated by a newline, never in place
  of it, so any client that matches the text still sees it unchanged (buck2's internal
  build matches `OUTMISS` and `Execution timed out`; open-source buck2 matches neither,
  since its client returns the error before that check).

A successful build prints no link.

**What stock buck2 cannot do**: print a server-chosen URL on success. Its
`Buck UI: <url>` line is compiled in for Meta's internal build only, and no `.buckconfig`
key, `[buck2_re_client]` setting or plugin adds a URL. A configured `http_headers` value
cannot carry a per-build id either: the RE connection belongs to the daemon and outlives
commands.

**Changes outside kbf that would close the gap** (none needed for this design):

1. Upstream buck2: a `[buck2] build_url_template` (or similar) that prints
   `Build URL: <template with the trace id>` in open-source builds, where it now prints
   `Build ID:`.
2. Upstream buck2: fill `action_id`, `action_mnemonic`, `target_id` and
   `configuration_id` in `RequestMetadata`, which it sends empty, and the real version in
   `tool_details`. Until then the page groups by action digest and shows no target names.
3. A wrapper around buck2 that sets `BUCK_WRAPPER_UUID` and prints the link first. That is
   a deployment's choice; kbf ships no buck2 wrapper.

**Caveats.** The farm learns of a build only through REAPI calls. A build served
entirely from buck2's local caches makes no call, and its page is `404` with the body
of 4.10, whose `records_since` tells it from a record lost at a restart. Actions deduplicated across two concurrent buck2
commands, and downloads made late by buck2's materializer, may carry the other command's
id or none (read from source, not measured).

### 5.3 Bazel, later

Bazel sends a full `RequestMetadata`, so its invocations are recorded and its failed
actions carry the link the same way. Bazel prints a link on every build only through the
Build Event Service: with `--bes_backend` and `--bes_results_url=<base>/build/` it prints
`Streaming build results to: <base>/build/<invocation id>`. A BES ingester
(`google.devtools.build.v1.PublishBuildEvent`) would add what only the client knows:
targets, tests and their results, named artifacts. **Not planned in this plan**; the
recorder takes events through one interface keyed on the invocation id, so a BES
ingester is a second writer and not a second record. buck2 does not emit BES.

## 6. Privacy

- Recorded: the RequestMetadata fields of 2.1, the caller's `public` authentication
  metadata (never `private`), and what the farm itself did. Not recorded: peer
  addresses, other headers, Command arguments or environment.
- `target_id` and `configuration_id` reveal a repository's layout; outputs and logs can
  hold anything a build prints. Both are behind the authorizer (4.3).
- The invocation id joins the existing `execute` tracing span (with instance and
  caller), so server logs can be correlated with a page.
- An operator removes a record with `DELETE /v1/invocations/{id}` (planned): the pin goes,
  and collection (once it runs) frees chunks not shared with other records. Artifacts are
  CAS blobs and may be shared by other builds, so deleting a record does not delete them.

## 7. Failure modes

The stage 2 and failover rows below are superseded by `ui-data.md` 5.2, which states
the failure modes of durable records as objects.

| Event | Effect on records |
|---|---|
| Recorder channel full | events dropped, builds unaffected, invocation `complete: false` |
| Memory bound reached | evict minted, then idle invocations; then the largest open one stops taking rows, `rows_capped: true` (3.5) |
| Server crash or restart | stage 1, and the in-memory metadata log (tests only) always (by design): all records lost, a page answers `404` with `records_since` the new start (4.10); stage 2 on the Raft metadata log: at most the unflushed window (120 s or 256 KiB) of each open invocation lost, marked `complete: false`; minted invocations lost; the start replays the whole log until the metadata snapshots land (3.5) |
| Object store down at flush | chunk kept in memory and retried; past `--invocation-memory`, oldest unflushed rows dropped and `complete: false` |
| Failover (HA, planned) | the new leader's recorder reads each open invocation's pin (its current root) and continues in new chunks; each pin update is a compare-and-set on that root (3.5), so a deposed leader's update that commits late makes the next update fail, and the recorder rebuilds from the pin's current root and retries, never overwriting it; the old leader's unflushed window is lost and the invocation says `complete: false` |
| Chunk lost or corrupt (the store has no bytes for it, or they fail their digest) | the page shows the surviving chunks' rows and a gap marker where the lost chunk's rows were (its row count and time range, from the root), and the invocation says `complete: false`; nothing restores it: the server alone writes a chunk (as it does a root), so no client holds its bytes to upload again |
| Root lost or corrupt | the root's own fields are gone: the 3.1 header fields (`tool`, `owner`, `first_seen`, the counters), the per-chunk row counts and time ranges, the precomputed metrics and the failed-row positions; the page answers them as `null` and says `complete: false`. The rows are recoverable: the pin's closure (3.5) still lists every chunk's digest, and each chunk carries its invocation id and sequence number, so the server reads the listed chunks in sequence order, shows their rows, and recomputes the metrics and the failed-first order by scanning them (the one case where 4.4 reads chunks); the read check (4.3) is asked with the instance names of those rows. A chunk lost as well shows as a gap with no row count or time range. An invocation still open on the server gets a whole new root at its next flush, from the header fields the recorder holds in memory (3.5) |
| ActionResult, output or log blob collected | still listed from the row (3.2) with `available: false`; its `href` answers `404` |
| Malformed or hostile header | treated as absent (2.2); never fails the call |
| Squatted id | recorded under `<id>~<n>` (2.4) |

## 8. Alternatives rejected

- **A farm-minted id for every build.** The user cannot learn it: no REAPI field shown on
  success, and buck2's printed id is its own. Kept only for the id-less Execute fallback.
- **Rows in the replicated log.** 100 to 240 MB a day into the log, in every snapshot
  and checkpoint digest, and a log format change per field. The Raft library supports
  snapshots and compaction (`kbf-raft`), but the server's metadata log takes no
  snapshot today (`raft_log/machine.rs`: its `snapshot` is `unreachable!`), so it is
  never compacted until the metadata-snapshot work lands (3.5).
- **A second authoritative store for records or cross-build queries.** Raft holds the
  pins, the CAS holds the rows; a list or search across builds is a derived index that
  can be rebuilt from them (3.7).
- **Records as separate objects in the object store.** Needs a new key space whose keys
  the log must still record (the objstore contract) and its own retention; CAS blobs plus
  a pin reuse both.
- **Grouping by connection.** One buck2 daemon connection serves many commands.
- **Digest-only pages, as Buildbarn's browser.** Useful for one action (and section 4.6
  is that), but they do not answer "what did my build do".

## 9. Implementation plan

Each is one pull request, S (under a day) or M (a few days). Each test says which defect
it catches and the mutant planted to see it go red. **The numbers are names, not the
order**: code and tests cite them (`section 9, PR 3`), so they are kept; 12 to 14 are
new. The list runs in the order the work lands: the agent's path first (the read API,
the failures view, the client, real CPU and memory), then durability on today's
single-voter log (CEO, 2026-10-12, 3.5), then the link, tokens for agents on other
machines, retention, and the HTML page for people last.

**Landed**: 1, 2, 3 and 5.

- **1. (S) Parse `RequestMetadata`.** The `kbf-front` layer and `CallMetadata`; the id on the
  `execute` span. Tests: a header encoded by `prost` is read back field for field; a
  golden header captured from one real buck2 build against a test server is read with the
  expected UUID (catches reading the wrong header name or a wrong field number: plant
  field 2 for 3, the golden goes red); oversize, undecodable ids and ids with a
  character outside `[A-Za-z0-9._-]` (`~`, `<`, `/`, `%`) are absent and the call still
  succeeds (plant "refuse on decode error", the call test goes red; plant the old
  `[!-~]` set, the `~` and `<` cases go red); `.`, `..` and `-x` are absent (plant
  dropping the first-character rule, the `..` case goes red). Also settles the open fact in [failure-classes.md](failure-classes.md) O2 about
  which fields buck2 sends.
- **2. (S) The record model, pure.** `kbf.invocation.v1` protos and an `Invocation` type
  built only from events; no clock, no I/O. Tests: every `how` and outcome; a requeued
  then finished action has two attempts in order; a joined action copies its twin's
  attempts; the row cap; squatting goes to `~2`; the row keeps output paths, digests and
  the stdout and stderr digests (plant "drop outputs from the row", the listing test goes
  red). Measures bytes per action (3.3) on a recorded event stream and states it.
- **3. (M) The in-memory recorder** wired to the seams of 3.4, with the bounded channel and
  memory bound. Tests: a full channel drops and marks `complete: false` without blocking
  (plant an awaiting send, the test's timeout catches it); eviction takes minted, then
  the least recently seen idle record; an invocation whose only operation has run for
  longer than the idle time, with no call, is not idle and not evicted (plant
  "`last_seen` = last call", red); at the bound with only open invocations the largest
  says `rows_capped` and memory stays under the bound; an end-to-end run with fake
  daemons (`kbf-server`'s test support) yields the expected rows for a hit, a run, a
  requeue and a failure.
- **5. (S) Blobs over HTTP** (4.6). Tests: bytes equal ByteStream's; a store returning
  corrupt bytes yields an error, not the bytes (plant skipping verification, red);
  `Range`; a `name` with `"`, `;` and non-ASCII comes back as a well-formed `filename*`
  (plant raw splicing, red); `nosniff` and `sandbox` on every answer, `inline` included
  (plant dropping them, red); `inline` refused for a blob over 4 MiB.

**Next, in this order**: 4, 12, 13, 11, 7, 8, 6, 14, 9, 10.

- **4. (M) The read API** (4.1 to 4.5, and the `404` body of 4.10) and the policy keys (4.3). Tests: paging
  while rows are appended never skips or repeats (property test); a token with another
  filter is `400`; with no `invocations` key the routes are `404` with the `off` body
  of 4.1 and never 4.10's `no_record` (plant "default allow", it goes red; plant "4.10's
  body for a route that is off", the body check goes red); with `invocations` and no `blobs`, `href`s are `null`; an authorizer
  refusing one instance refuses the page; `{id}` with `<`, `%3C` or `/` is `404`; a
  page token for a record evicted after the token was issued is `404` (plant "resolve
  the token's position anyway", red); following each page's `next_page` from the
  first visits every row once and the last page has none (plant "`next_page` without
  the filter", red: a `400`); after a restart a `404` body's
  `records_since` is the server's start, and after an eviction it is the evicted
  record's `last_seen` (plant "`records_since` = start only", the eviction case goes
  red).
- **12. (M) The failures view and log tails** (4.7), and the timed-out action's listing
  (section 1: the recorder lists the partial result the farm answers a timeout with).
  Tests: a timed-out action is listed, with the tail of its partial stderr (plant
  "failed only", the route lists only `failed` outcomes: red; plant "listing for an
  exit only", the tail check goes red); an OK status with exit 1 and a refused action
  are listed, and an action that passed after a rerun is not (plant "outcome is OK"
  without the exit code, red); the order is first-seen across kinds and stays stable
  while rows are appended (plant "sort by kind", red); a 10 MiB stderr comes back as
  its last 16 KiB with `truncated: true` and `bytes` 10485760 (plant no cap, red;
  plant the head for the tail, red); a tail that cuts a multi-byte character starts
  with U+FFFD and the answer is valid JSON (plant a strict decode, red: the route
  errors); with 40 failures whose tails are 16 KiB each and `page_size` 100, the first
  page ends before the entry whose tails would pass 1 MiB, its tails total at most
  1 MiB, and following `next_page` lists every failure exactly once, each with its tail
  (plant no budget, red: the first page's tails pass 1 MiB; plant "repeat the entry on
  the next page", red: an entry is on two pages; plant "drop the entry past the
  budget", red: one is on none); with the blob route's budget held by another read
  past its wait, the entry says `tail_omitted: "busy"` and the page still answers
  (plant "fail the page", red); each page carries the invocation answer with its
  `links`, and a failed action's `outputs` with their `href`s (plant "header without
  links", red); a corrupt log blob gives no tail, never its bytes (plant skipping
  verification, red); from a Command whose environment holds a marker value, `command`
  carries the arguments and the variable's name and no byte of the value (plant "copy
  the environment", the marker check goes red); with `invocations` and no `blobs`,
  tails and `command` are `null` (plant "tails without `blobs`", red); a Command blob
  gone gives `command_available: false`.
- **13. (S) The client, `kbf build show`** (4.8): one command; no other subcommand.
  Tests, against a test server holding a recorded failed build:
  `kbf build show <id>` output parses as the failures route's shape, read into the
  server's own response types (plant a renamed field in the client, red); the failed
  build's answer holds each failed action's stderr tail inline, byte for byte the
  route's (plant "omit tails", red); every link in the answer (`href`s, `links`,
  `next_page`) is absolute on `--server`, and each, read with the same token in the
  `Authorization` header, answers `200` from the test server (plant a wrong path in
  one link, red: a `404`; plant "leave links relative", red); a build with more
  failures than a page has `next_page`, and `show` of it gives the rest (plant "drop
  the page token", red: the first page again); `show <blob link> -o <file>` writes
  bytes equal to the blob, a blob link without `-o` exits 2, and a body whose SHA-256
  differs from the link's digest exits 1 and leaves no file (plant "skip the digest
  check", red); a 10 MiB log's link with `inline=text` is still written with `-o`
  (plant "keep `inline`", red: the route's `400`); a `404` with 4.10's body exits 3,
  a `404` with the `off` body exits 5, and a `401` exits 4, each body printed (plant
  "exit 3 for every 404", red; plant "exit 1 for every error", red); an action link
  gives its id and digest, and the request goes to `--server`, not to the link's host
  (plant "follow the link's host", red: a second test server bound for the link sees a
  request); a token file readable by its group is refused (plant dropping the mode
  check, red); the config file's `server` is used when `--server` is absent and the
  flag wins when both are given (plant "config wins", red); the output is the same
  bytes under a pseudo-terminal as under a pipe (plant "pretty-print on a terminal",
  red).
- **11. (S, a driver track) `ResourceUsage` from the native and container drivers**, already
  planned in [scheduler.md](scheduler.md#accounting); until then `usage` is `null`.
  Tests: an action that burns a known CPU time and allocates a known amount reports at
  least those (plant "report zeros", red), in each driver's own tests.
- **7. (M) The pin command and its log format**: superseded by `ui-data.md` 5.2 (no pin
  command, no log-format bump; the text below is kept as history). On today's single-voter
  the Raft metadata log log (3.5), ahead of the HA work (PR #303), which inherits it. The
  format bump is made for the invocation pins alone; the general Pins of
  [storage.md](storage.md#planned) add their commands later, in their own bumps. The
  command is `{id, prev_root, new_root, added_chunk_digests}` with its hold-until time,
  applied as a compare-and-set on `prev_root`; `MetaState` keeps each pin's root and
  its accumulated closure, and `Collect` keeps every digest in a live pin's closure
  (3.5). Bounded replay needs the metadata snapshots (`docs/design/metadata-snapshots.md`,
  in flight); this PR does not wait for them. Tests: a log of the new format replays
  pins, their closures and their hold-until across a restart of a single-voter
  the Raft metadata log server (plant "pins in memory only", red); a reader of `Format::V1`
  refuses the new format rather than skipping the entry (plant "skip unknown", red); a
  pin update with a stale `prev_root` is refused and leaves the pin unchanged (plant
  "skip the check", red); a pin update whose `new_root` is not in the blob index is
  refused (plant "skip the index check", red); a collection keeps every digest of a
  live pin's closure, including a chunk named only by an earlier update, and frees them
  once the pin expires (plant "keep only the last update's chunks", red); a collection
  run with the object store unreachable (every read fails) gives the same result as
  with it reachable (plant "parse the root", red).
- **8. (M) Durable records**: superseded by `ui-data.md` 5.2 and its phase U3 (chunks as
  objects under `history/`, indexed in the state store; the text below is kept as history).
  Was: 3.5 stage 2, on the Raft metadata log: chunks as CAS blobs,
  roots, the flush rule of 3.4, recovery marking open invocations incomplete. Tests:
  restart keeps every flushed row, and the failures view and `records_since` of 4.10
  answer from it; with the in-memory metadata log (tests only) a restart keeps no record and
  `records_since` is the new start (by design; plant "`records_since` from the
  retention whatever the log", red: it is not the new start); a crash loses at most the window; an idle invocation is flushed once and then
  writes nothing (plant a 10 s timer, the log-entry count goes red); a minted
  invocation (2.3) writes no chunk and no pin (plant "flush minted invocations like any
  other", the log-entry count goes red); every chunk and root carries the version, id,
  sequence number and base times of 3.5, and a root carries per-chunk counts and time
  ranges, targets, statuses, metrics and failed-row positions (a decode test of each
  field; plant "omit the failed-row positions", the failed-first test goes red); the
  metrics route and the failed-first logs route on a flushed invocation read the root
  and no chunk (count the store's reads; plant "scan every chunk", red); a chunk
  deleted from the store, or corrupted, leaves a page of the surviving rows with a gap
  marker and `complete: false` (plant "skip a missing chunk silently", the
  `complete` check goes red); `kbf-sim-cell` gains the property "every pinned root's
  chunks are readable".
- **6. (S) `--invocation-url-template`, the link on failed actions and the `/build/{id}`
  redirect.**
  Tests: template validation (no `{id}`, two, non-http) exits 2; a failed Execute's
  response carries both links in `ExecuteResponse.message`; a timed-out, a
  missing-input and a refused answer each have a status message that starts with its
  text of today and ends with the link (plant "replace the message", the prefix check
  goes red; plant "message only in `ExecuteResponse`", the non-OK cases go red); the
  id-less fallback mints a `kbf-` id whose page exists; `GET /build/{id}` answers `303`
  to the API route.
- **14. (M) Read tokens**, with issue 227 (4.9). Tests: a listed read token reads a
  page; no header, or a wrong token, is `401` with `WWW-Authenticate: Bearer` (plant
  "accept any bearer", red); a read token is refused on Execute, on a CAS write and on
  an operator API write (plant "role unchecked", red); the write token is not accepted
  on a read (plant "accept the write token", red); a policy naming `bearerToken` whose
  token file is readable by others, or malformed, stops the server at start with exit
  2 (plant "start with no tokens", red: the server starts); a bad file at a re-read
  refuses the tokens the old file held (plant "keep the last good set", red); a policy
  whose `execute` and `cas` authorizers are `allow` still refuses a read token on
  Execute and on a ByteStream Read (`RefuseReadRole`, 4.9; plant "wrap only when the
  slot is unset", red); an API listener serving plain text off loopback starts, and
  its start line's `warning=plaintext-off-loopback:` token names `api` (plant "refuse
  to start", red; plant "no warning", red).
- **9. (S) Retention and delete**: pin expiry, `DELETE`, `--invocation-pin-outputs=failed`.
  **Blocked on collection**, which the server does not run today (section 1). The
  failed actions' digests ride in the pin command (3.6). Tests: an expired pin is
  collected; a page token issued before its pin expired answers `404` after (plant
  "serve the token's position", red); a pinned failed action's ActionResult and stderr survive a
  collection that frees its successful neighbour's; after its neighbour's blobs are
  collected the neighbour's page still lists its outputs with `available: false` (plant
  "read outputs from the ActionResult", red).
- **10. (S) The HTML page of `/build/{id}`.** Tests: a record whose target, mnemonic,
  reason text and output path contain `<script>`, `"` and `&` renders them escaped
  (plant one unescaped field, red); the page has no `<script>` element and carries
  `nosniff` and the CSP (plant dropping them, red); without `Accept: text/html` the
  answer is still the `303`.

PRs 4, 12 and 13 give an agent the build it just ran: what failed, why, its logs and its
command, in one call (`kbf build show`), until the server restarts (4.10). 11 fills CPU
and memory; 7 and 8 make records survive a restart on the Raft metadata log; 6 puts the
link in failed actions' messages; 14 lets an agent on another machine read; 9 keeps a
failed action's outputs; 10, the page for people, comes last.

## 10. Open questions

Each with a recommendation; none is decided here.

1. **Whose id names the page: the client's `tool_invocation_id` or a farm-minted one?**
   Recommend the client's: it is the `Build ID` buck2 already prints and Bazel's
   invocation id, and a farm id cannot reach the user.
2. **Calls with no id.** Recommend 2.3: a one-action invocation for an id-less Execute,
   linked in its response; cache and CAS calls counted farm-wide only.
3. **Reading pages before real authentication exists.** Recommend fail closed (4.3): the
   routes are off unless the policy file names an `invocations` authorizer or
   `--invocations-open` is given; a public deployment fronts the listener with an
   authenticating proxy until the read tokens of 4.9 (or `jwt`) land.
4. **How long to keep a build.** Recommend the record 30 days, and with it the list of
   every action's outputs and logs (paths and digests are copied into the row, 3.2);
   their bytes on the CAS's own retention (7 days after the last touch), so after a week
   a page lists them as unavailable; failed actions' results, outputs and logs pinnable
   by flag (3.6). Pinning every build's outputs would hold most of the cache.
5. **Durable home.** Recommend CAS chunks plus one pin per invocation in the log (3.5),
   the pin carrying its closure and updated by compare-and-set; cross-build lists from
   a derived index, never a second authoritative store (3.7); flushed at 256 KiB, at
   idle or every 120 s, with the in-memory stage shipped first. **Decided** (CEO,
   2026-10-12: "Yes we should switch to durable build records without waiting for
   HA."): the pins and chunks land on today's single-voter the Raft metadata log log, in a
   format bump of their own, before the HA work, which inherits them (3.5).
6. **The link on a successful buck2 build.** Recommend: today, the printed `Build ID` in
   the template and the link in failed actions' messages; send the upstream buck2 change
   (5.2, items 1 and 2); kbf ships no buck2 wrapper (its client, `kbf build show`,
   reads a build after it ran and wraps nothing).
7. **Record cache hits as rows?** Recommend yes, capped (3.3): a page of a fully cached
   build would otherwise be empty, and "why was this a hit" is half of what people ask.
8. **The human path.** Recommend `/build/{id}` for the page (the word users say) and
   `/v1/invocations/{id}` for the API (REAPI's word).
