# High availability

This document is the design for running kbf on more than one `kbf-server` so that the
farm survives losing a server: a replicated Raft log, a failover that keeps running
leases, reads and uploads on every server, membership changes, and snapshots copied to
the object store. **Everything in it is planned.** Section 1 names what exists on
`main` today; nothing else here does. File and line references are to `main` at the
commit this document was written against (the merge of #280).

The shape of the deployment (storage hosts, the client front, daemons dialing servers
directly) is in [deployment-topology.md](deployment-topology.md). The CAS, the action
cache, retention and touches are in [storage.md](storage.md). The choice of a Raft
core of our own is [ADR 0001](../adr/0001-consensus.md). Fencing (G and T) is in
[scheduler.md](scheduler.md#fencing-g-and-t) and the daemon's side of it in
[worker-protocol.md](worker-protocol.md#server-restarts-and-the-lease-epoch).

Correctness comes before speed throughout: where a choice trades a guarantee for
latency, this design keeps the guarantee and measures the latency.

## 1. Where we start

Only the Raft core exists, and nothing outside its own tests calls it.

| Part | On `main` |
|---|---|
| Raft core | `kbf-raft`: sans-IO election, replication, commit, learners in a fixed `Membership`, a snapshot base with `restore` and `compact` (#262). PreVote, CheckQuorum, InstallSnapshot and membership changes are not built (`crates/kbf-raft/src/lib.rs:16-21`). The simulation runs 3 voters and a learner over 500 seeds; compaction is held under `compaction_floor` (`tests/sim/checks.rs:180`) because a follower behind the base cannot be served |
| Log storage | none: `kbf-store` is an empty stub |
| Peer transport | none: `kbf-proto` has only `kbf.worker.v1` |
| Metadata | `MemoryMetaLog`, a `MetaState` behind a lock (`crates/kbf-server/src/main.rs:62-63`); its contract says `query` runs "on the leader" (`crates/kbf-front/src/meta_log.rs:6-7, 33`) |
| Control log | the farm's own loop: `Effect::Commit` is fed straight back as `Event::Committed` ("Single node: appended is committed", `crates/kbf-server/src/farm.rs:727`). The `LeaseOffer` for a grant is sent before that, at `farm.rs:723-725`; the daemon only logs an offer (`crates/kbf-daemon/src/daemon.rs:471-476`) |
| Scheduler | a pure, deterministic `StateMachine` (`crates/kbf-types/src/state.rs`); submissions are not committed, so a new leader would not inherit the queue (`crates/kbf-sched/src/lib.rs:41-44`); `OperationId` is a counter local to the scheduler (`scheduler.rs:231, 453`); a finished operation is kept for `FINISHED_RETENTION` (60 s, `scheduler.rs:44`) so that a `WaitExecution` gets its result |
| Term and epoch | `process_term()` is wall-clock milliseconds times 2^16 plus 16 random bits (`farm.rs:50-67`); `Welcome.epoch` is that term (`worker.rs:133`); a daemon kills and forgets every lease of another epoch (`daemon.rs:426-461`) |
| Farm state outside the scheduler | waiters, `started` (the `Sent` that `holder()` requires, `farm.rs:685-700`), finished operations' callers (`finished`, `farm.rs:196-198`), links, node status, the node registry, rollouts (`MemoryRolloutStore`): all in memory. Every start logs `STATE_IN_MEMORY` (`serve.rs:146-148`) |
| Farm time | milliseconds since the process built its `Farm` (`farm.rs:257-260`); `Cache::tick` and `Cache::collect` are never called by the server |
| Object prefix | `--store=s3` writes under `<prefix><start time>/` on every start (`config.rs:138, 290-292`) |
| Readiness | `/readyz` with a `leader` flag that defaults to true; `set_leader` has no caller (`health.rs:46-78`) |
| Fencing constants | G = 60 s, T = 40 s, `LEADER_LEASE_MARGIN` = 5 s, `T + margin < G` checked at compile time (`crates/kbf-sched/src/fence.rs:23-45`); the same margin sets `HANDOVER_GRACE` (`fence.rs:54`) and enters the `START_VALIDITY` assertion (`fence.rs:74-77`). The header assumes "leaders acknowledge only inside their leader lease" (`fence.rs:13-14`), which nothing provides |
| Daemon | one `--server` URL, a fixed reconnect wait (`--reconnect-ms`), one `--cas` URL |

Open pull requests this design builds on: #300 (the daemon retries forever across every
address of every `--server`), #299 (`grpc.health.v1` on the REAPI listener), #298 (the
deployment decisions in the docs), #226 (REAPI TLS, for the proxy-to-server hop), and
#284/#283 (memory kills and the doubled booking, whose per-action floors are state this
design replicates).

## 2. What the maintainers decided

These are settled and this design does not reopen them:

- Servers run on storage hosts at fixed addresses with stable DNS names.
- Each voter keeps its Raft log on its own local disk, on its own partition. Snapshots
  are also copied to the object store.
- Six nodes run **5 voters and 1 learner**; every member serves reads.
- **A failover keeps running leases.** The lease epoch names the replicated log, not a
  leader, and daemons resend their results to the new leader.
- Daemons are given the servers as one DNS name with a record per server, or a list.
  Each holds one worker stream, to the leader. A follower redirects it with a status
  naming the leader, or answers `UNAVAILABLE` when it knows none. Daemons retry
  forever with bounded backoff.
- Any server serves the read path (`ByteStream.Read`, `BatchReadBlobs`,
  `FindMissingBlobs`, `GetActionResult`) and accepts upload bytes: it writes them to
  the object store, then commits the metadata through the leader. The leader keeps
  every other metadata write, `Execute`/`WaitExecution`, scheduling and the worker
  streams. Daemons' blob reads are spread across all servers too.
- The front (an HTTP/2 proxy such as Envoy) routes by gRPC method and health-checks the
  servers through `grpc.health.v1` and two readiness paths: serving reads, and leader.
  The proxy-to-server hop is TLS with an internal CA (#226).
- A follower may answer a false "missing", never a false "present".
- Priority on every node: object store > `kbf-server` > `kbf-daemon` > builds. The
  converged profile (object store, server and daemon on one host) is supported, beside
  server-only and build-only profiles.
- Rollout: 1 kbf node, then 3 (3 voters; a planned node kill during a real build must
  succeed), then 6 (5 voters + 1 learner), each phase running beside an existing farm
  until the last ([section 14](#14-rollout-beside-an-existing-farm)).

Three of these settle points where the existing documents and open pull requests
disagreed. The writer of this design flagged them, and the maintainers decided:

- **Converged hosts are supported.** Object store, server and daemon may share a host.
  Dedicated roles (server-only and build-only hosts) are preferred later, as the farm
  grows, but are not required.
- **The proxy-to-server hop is TLS** with an internal CA. #226 is revived for it,
  rather than left open as an option.
- **A failover keeps leases.** The documents that say an old leader's leases are
  refused ([section 19](#19-documents-this-corrects)) are corrected to this.

## 3. The design in one page

**One Raft log carries every input that changes durable farm state.** Every server
applies the same entries, in the same order, to the same pure state machines:
`MetaState` (metadata) and a new `FarmMachine` that wraps the scheduler and the farm
state that today sits outside it. All replicas therefore hold the same state, and a
periodic digest proves it. **Only the leader acts**: it sends `Start`s, answers
waiters, acknowledges daemons, and runs ticks and garbage collection. Followers apply,
serve reads, take upload bytes and redirect daemons.

Two facts about the code make this the cheapest correct choice:

1. The scheduler already promises determinism: "applying the same inputs in the same
   order to equal states yields equal states and equal effects"
   (`crates/kbf-types/src/state.rs`). If the input that caused an `Effect::Commit` is
   itself committed, the record is a function of the committed prefix, so it is
   committed too. At apply time every replica feeds `Effect::Commit` back as
   `Event::Committed`, which is what `farm.rs:727` does today; under input replication
   that comment becomes true. No second consensus round per lease.
2. The daemon needs no protocol change to keep leases. It compares the epoch stored
   with each grant against the newest `Welcome`'s (`daemon.rs:455-461`). With
   `Welcome.epoch` = the log id, fixed for the life of the log, a failover is the same
   epoch and nothing is superseded.

Five rules carry the safety argument; each has a test that fails without it
([section 16](#16-pull-request-plan)):

- **R-ack. Nothing leaves the leader before it is committed.** No operation name is
  returned, no `LeaseOffer` or `Start` is sent, no `HeartbeatAck`, `Welcome` or
  `ResultAck` goes out until the entry behind it has committed. A leader that cannot
  reach a quorum cannot commit, so it cannot acknowledge, and fencing needs no leader
  lease ([section 5.3](#53-fencing-without-a-leader-lease)).
- **R-takeover. A new leader commits a `Takeover` entry before it acts.** Applying it
  resets every worker's last-heard time, so G counts from the takeover.
- **R-epoch. `Welcome.epoch` is the log id**, a random non-zero 64-bit value
  committed in the log's first entry.
- **R-local. A commit resolves only after the entry has applied on the server that
  asked**, so a server's answers always reflect its own write.
- **R-fail-stop. Anything that would break durability or determinism stops the node**:
  an fsync error, an entry it cannot decode, a corrupt record in the middle of the log,
  a snapshot whose digest does not match, a state digest that disagrees with the
  leader's.

## 4. The replicated log

### 4.1 One group, two machines

As ADR 0001 decided: one Raft group, one total order, two state machines with
separate snapshot sections. Groups are values, so splitting metadata into its own
group later is a deployment change. An entry's payload:

```
Payload::Blank                          exists: a new leader's first entry
Payload::Config(Membership)             new in kbf-raft (section 6)
Payload::Command(bytes) = LogCommand, encoded:
  Bootstrap { log_id, store_prefix, format, machine_version }   the log's first command
  Meta(kbf_meta::Command)               Tick, AllocEpoch, PutBlobs, PutAction, Touch,
                                        ObjectUnreachable/Reachable, Collect
  Farm(FarmBatch { now, inputs })       scheduler and farm inputs, batched
  Report { input, action_record }       a Result and its action-cache entry, one entry
  Takeover { term, now }                a new leader's first command (section 7)
  Checkpoint { index, digest }          determinism check (section 4.9)
  MachineVersion(u32)                   gates behavior changes (section 4.7)
```

### 4.2 The farm machine

`FarmMachine` is a pure state machine, in `kbf-sched` or a new pure crate, holding
everything a new leader needs that lives today in `farm.rs:187-206` or beside it:

| Moves into `FarmMachine` (replicated) | Stays leader-local |
|---|---|
| the `Scheduler` itself: operations, queue, in-flight leases, workers and their sessions, held starts, cordons, reservations | stream senders and `links` |
| each waiter's record: the operation it waits on, its name, action key, lease kind and queue time | each waiter's `watch` channel (the client's stream) |
| the `started` table: per lease, the operation, action digest and `Stamp` that `holder()` checks | `newest_beat` and other per-stream bookkeeping |
| the node registry (#235: durable) and node status | the rollout driver's I/O (it proposes records; [below](#rollouts-and-mdm)) |
| per-action memory floors and the doubled-booking requeue (#284) | |
| finished operations, kept for a retention window | |

**Wall-clock values enter the log as values, never as reads.** `Stamp` holds
`SystemTime`s (`crates/kbf-server/src/stamp.rs:13-18`). The leader stamps the queue and
start times into the input that produces them, and apply copies them; nothing in
`FarmMachine` reads a clock, a random source or a hashed map's iteration order.

**When `started` is written.** The scheduler emits `Effect::Start` while it applies the
committed grant, on every replica, and `FarmMachine` records the `Sent` entry at that
same apply step, before the leader puts the `Start` on the wire. So every leader that
has the grant has its `Sent` too, and a `Result` for it is never refused for want of
one. A `Start` the old leader recorded but never sent is handled by the existing rule:
the new session's first heartbeat does not list it, and it is requeued at once
([section 7](#7-failover-step-by-step)).

**Waiters.** A `Submit` carries no waiter id; `FarmMachine` assigns one at apply time
from a committed counter, so ids are the same on every replica and never collide
across leaders. The client's stream is leader-local: a waiter whose channel is gone (its
leader died) stays in the replicated state, and its answer is kept with the finished
operation. A `WaitExecution` on a later leader finds the waiter by name and attaches a
new local channel; attaching is not logged, because it changes no decision. The
scheduler has no input for a waiter leaving, so nothing about a client's stream
closing needs to be replicated either.

<a id="rollouts-and-mdm"></a>**Rollouts and MDM.** The rollout driver does I/O, so it
does not move into the machine. It stays on the leader and proposes small records
(a step started, a step done, a rollout paused) that the machine applies. From
[PR 25b](#16-pull-request-plan), in phase 2, a failover pauses a rollout, and the new
leader resumes it from the committed records. Before PR 25b (phase 1) the records are
in memory, and a restart of the single server abandons a running rollout; it is started
again by hand.

### 4.3 Committed before answered

R-ack, made concrete:

- **`Execute`** proposes the `Submit` and returns the operation name only after it has
  committed and applied. A queued operation is a promise, so it survives a failover;
  the cost is one commit on every `Execute`, which is measured
  (`execute_submit_commit_seconds`).
- **`LeaseOffer`** moves to after the grant commits. Today it is sent before
  (`farm.rs:723-725`); a minority leader would offer leases that never commit. The
  daemon only logs offers, so this is cheap, and it gets its own pull request and
  mutant.
- **`HeartbeatAck`, `Welcome`** go out only after the `FarmBatch` that carries the
  heartbeat or the `WorkerUp` has committed.
- **A `Result`** is committed as one `Report` entry carrying both the scheduler input
  and the `PutAction` record. Applied, it feeds the scheduler; if the scheduler accepts
  the result, the same apply step runs `MetaState::execute(PutAction)`. "Result
  accepted" and "action-cache entry written" become one entry, which removes today's
  gap between `Farm::report` and `commit_action_record`
  (`crates/kbf-front/src/cache.rs:481`). The result blob is written to the CAS, with its
  own `PutBlobs` entry, before the `Report` is proposed, so log order keeps an
  action-cache entry from naming an absent blob.
- **A duplicate `Result`** (the daemon never got the old leader's `ResultAck`) is
  acknowledged as a duplicate, `accepted: true`, without a second record. Today it would
  be refused, because only this process's leases are in `started`.

### 4.4 Farm time

The leader stamps `now` into every `FarmBatch` and `Meta` entry. Apply takes
`max(applied_now, entry.now)`, the rule `MetaState::Tick` already follows. A new leader
reads the applied `now` as its base and stamps `base + (monotonic now − takeover
instant)` from then on. Farm time therefore never goes backwards across leaders, and it
stands still during an election, which can only push a deadline later: the safe
direction for G. `Farm::now()` counted from the process start (`farm.rs:257-260`) goes.
The leader commits a `Tick` every second.

### 4.5 Ids and names

- **Log id.** A random non-zero `u64` in the `Bootstrap` command. Zero is refused,
  because the daemon reads epoch 0 as "no epoch named" (`daemon.rs:423`).
- **Raft term.** `LeaseId.term` becomes the Raft term; `seq` is per term, from the
  machine. One leader per term makes `(term, seq)` unique. `process_term()` goes.
- **Operation names** become `operations/<log_id>-<waiter>`, parsed under any leader
  of the log. A name from another log (a disaster restore, or today's process-term
  names after the first upgrade) is `NOT_FOUND` once.
- **Object prefix.** `<prefix><log_id>/`, recorded in `Bootstrap`, replaces
  `<prefix><start time>/`.

### 4.6 Encoding

prost messages in `kbf-proto` (`proto/kbf/log/v1/log.proto` and `snapshot.proto`),
with explicit conversions in a codec module. The domain crates stay free of codec
dependencies (`kbf-meta` keeps only `kbf-types` and `thiserror`).

- No `map` fields: maps are encoded as repeated fields in key order, so the bytes of a
  snapshot are a function of the state. Two servers snapshotting the same index write
  the same bytes.
- Enums are closed. An unknown value or command is a decode error, which is fail-stop,
  never a default.
- Golden bytes are committed for every variant; a round trip `decode(encode(x)) == x`
  runs over generated values.
- **Forwarded commands are idempotent**, so a forward retried across a leader change
  needs no session table. Checked against `crates/kbf-meta/src/state.rs`: a second
  `PutBlob` of a held, reachable blob is `Duplicate { kept }` and changes only the
  touch time; `Touch` is idempotent; `ObjectUnreachable` never downgrades `Corrupt`;
  `PutAction` replaces with the same record; a retried `AllocEpoch` wastes an epoch,
  which is harmless. A test applies every forwardable variant twice and compares
  states, and the forwarder treats `Duplicate` as success.
- **Idempotence does not cover reordering.** A forward can be delayed or retried across
  a leader change and land after a newer command. For every forwardable command but
  one, landing late is harmless: it is a write the sender saw succeed, or a mark that
  only raises. The exception is `ObjectReachable`, which today removes the mark
  whatever set it (`state.rs:248-250`). A late one would clear a newer loss mark, and
  every server would answer "present" for bytes that are gone. Nothing sends it today
  (only `ObjectUnreachable` has a caller, `crates/kbf-front/src/cache.rs:690`), so the change lands before
  any caller does: every applied `ObjectUnreachable` stamps the mark with its own log
  index, its **generation**, whether or not it raised the reason. `ObjectReachable
  { object, generation }` names the generation the sender read before it probed the
  store, and it clears the mark only if that is still the mark's generation. Otherwise
  it is a no-op. The generation is part of the snapshot section. PR 2 adds it, with a
  test that applies a delayed `ObjectReachable` after a newer `ObjectUnreachable` and
  finds the mark still there.

### 4.7 Versions and upgrades

Input replication makes every change to apply logic a consensus change: two binaries
that place work differently, given the same heartbeat, would diverge. Two committed
versions gate that:

- **Format version** (in `Bootstrap`, raised by a `MachineVersion` entry): which entry
  and snapshot encodings may appear.
- **Machine version**: which apply behavior runs. A change to `FarmMachine` or
  `MetaState` behavior ships behind the version number it introduces: a binary
  carries the apply path of the committed version and of the next one, and switches at
  the entry that raises it. The leader proposes `MachineVersion(n + 1)` only after
  every member, learners included, has reported on its peer stream that it supports
  `n + 1`. A node being added is checked at `add-learner`: the leader asks it over the
  peer listener which versions it supports, and the tool refuses a node that cannot
  apply the committed format and machine version ([section 6](#6-membership)). While a
  change is pending, the leader proposes no `MachineVersion`.

Two checks keep an ungated change from shipping:

- a **golden-log replay** test in CI replays a recorded log under the new binary at
  the old machine version and compares the state digest at every checkpoint with the
  recorded one;
- **checkpoint digests** in production ([section 4.9](#49-determinism-checks))
  catch what the test missed, as a fail-stop on the diverging node.

A stop-the-world upgrade (drain, stop all, start all) is always allowed and is the
fallback when a change cannot be gated.

### 4.8 Storage on the local disk

`kbf-store` (empty today) on the dedicated Raft partition:

- Segmented, append-only log files; every record is length, CRC-32C, payload.
- The hard state is written to a temporary file, fsynced, renamed, and the directory
  fsynced.
- `PersistEntries` truncates and then appends; a truncation writes a new segment and
  never rewrites in place. Every persist is durable before any later `Send`, the core's
  contract (`crates/kbf-raft/src/message.rs:63-87`). Group commit: while one fsync is in
  flight, the next batch gathers, for at most 20 ms.
- At open, a torn tail (a bad CRC at the end) is truncated: safe, because nothing past
  the last fsync was acknowledged. A bad CRC in the middle is fail-stop.
- **An fsync or write error is fail-stop**: the node leaves the group (not ready, an
  alert), exits non-zero, and is restarted with backoff. It never acknowledges an
  entry it could not make durable.
- **A free-space floor.** Below it, the leader refuses new proposals with `UNAVAILABLE`
  and forces a snapshot and compaction. Snapshots are triggered by log bytes as well
  as entry count.

### 4.9 Snapshots and determinism checks

**Local snapshots.** Every replica snapshots on its own, every 100 000 entries, 64 MiB
of log or 10 minutes, whichever comes first (a lean, [section 18](#18-open-decisions)).
A snapshot holds the base `LogId`, the membership, both versions, the `MetaState`
section and the `FarmMachine` section, and its SHA-256. `MetaState` holds the whole CAS
index, so a snapshot can reach hundreds of MiB: encoding and decoding stream to and
from the file, and are never built as one buffer. The first version pauses apply while
it streams the state out; `snapshot_pause_seconds` is measured, with a budget of 1 s at
the target index size. If the measurement exceeds the budget, a follow-up moves the
large maps to persistent (structurally shared) maps so a snapshot copies a root instead
of pausing.

**Copies in the object store.** A server whose newest local snapshot is newer than the
newest copy uploads it to `<bucket>/raft/<log_id>/snap/<index>-<term>.snap`, then writes
the manifest `<index>-<term>.json` (SHA-256, size, membership). The manifest goes
last, so a half-written copy is never visible. Because snapshot bytes are
deterministic, two servers uploading the same index write the same object; a follower
uploads by preference so the leader does not spend the bandwidth. The newest 5 copies
are kept, plus one a day for 7 days. **A retained copy pins every object it
references**: [section 4.10](#410-deleting-objects) deletes no object while a retained
copy older than its condemning `Collect` still exists. So every retained copy can be
restored, and the cost is that condemned bytes stay up to 7 days longer. A copy is
deleted manifest first, then the snapshot object, and never while it is the newest copy
(S1 rests on that).

**Compaction bound (S1).** No server compacts its local log past the newest snapshot
whose manifest is in the object store. Then for any lagging follower the leader can
always send either entries or "fetch copy X, then the entries after X". With the
object store down, compaction stops and the free-space floor is the backstop; the
object store being down already stops the farm's blob traffic.

**InstallSnapshot by reference.** `Message::InstallSnapshot { term, base, membership,
locator, sha256 }` carries a pointer, not bytes. The follower's core emits
`Effect::Restore { base, locator }`; the host fetches the copy, checks its SHA-256
(a mismatch is fail-stop for that install, retried from the next copy), restores both
machines and calls `Raft::snapshot_installed(base)`; the core then answers with a match
at `base`. This lifts the sim's `compaction_floor`. If the copy is deleted (it aged out
of retention) while the follower is fetching it, the fetch fails with "not found". The
follower reports the failed install, and the leader sends a new `InstallSnapshot`
naming its newest copy, which S1 keeps present. A copy the follower has already fetched
and checked is restored from its local file, whatever happens to the store copy
afterwards.

<a id="49-determinism-checks"></a>**Determinism checks.** Every 10 000 entries the
leader proposes `Checkpoint { index, digest }`, the digest of its own state at
`index`. Each replica computes its own at that index when it applies the entry and
reports the result in its next append response.

- A follower whose digest differs turns off "serving reads", raises a page and stops
  applying. It never heals itself. Recovery: remove it from the membership, wipe its
  partition, add it back under a **new** server id, and let it restore from a store
  copy.
- If the follower digests that disagree with the leader's are a quorum of the
  voters, the leader is the likelier odd one out: it steps down and pages instead of
  continuing to schedule.

### 4.10 Deleting objects

Garbage collection is not wired today. When it is, on the leader only:

1. `Collect` is committed, so every replica removes the same entries.
2. A snapshot that includes the `Collect` is copied to the object store, and its
   manifest written.
3. The **condemn delay D** passes (1 hour by default).
4. Every retained copy older than the `Collect` has aged out of retention
   ([section 4.9](#49-snapshots-and-determinism-checks)) and been deleted. Retention is
   not shortened for this: the retained copies pin the objects they reference.
5. Only then are the objects deleted.

Without steps 2 and 4, restoring an older copy would say "present" for a deleted
object. With the default retention, step 4 is the one that waits longest: a condemned
object is deleted about 7 days after its `Collect`.
D must be far longer than the longest time a server may serve reads while behind,
`READ_BEHIND_BOUND` ([section 8](#8-reads-and-uploads-on-every-server)); the two
constants are checked against each other at compile time.

## 5. Election, fencing and reads

### 5.1 Election

- **Ticks** of 100 ms; Raft heartbeats every 2 ticks; **election timeout 20 to 40
  ticks** (2 to 4 s, a lean: loaded converged hosts favour the longer range). The
  timeout must tolerate the fsync p99.999 measured on a busy converged host in phase 2;
  it is fixed after that measurement.
- **PreVote** (dissertation 9.6): a node that rejoins after a partition, or restarts,
  cannot raise the term and depose a healthy leader.
- **Leader stickiness**: a node refuses a vote or prevote while it has heard from a
  leader within the minimum election timeout, **except for an election that a
  TimeoutNow started**. The transfer target campaigns at once, with no prevote, and
  marks its vote requests as a transfer (the dissertation's "disruptive" flag, section
  4.2.3). A voter grants such a request past stickiness, under the usual log check.
  Without the exemption, voters that just heard from the old leader refuse the target,
  the transfer fails, and an unplanned election follows two election timeouts later.
  Only a node that received TimeoutNow sets the flag, and only the current leader
  sends TimeoutNow, so the flag gives a partitioned node no way to depose a healthy
  leader.
- **CheckQuorum**: a leader that has not heard from a quorum within an election
  timeout steps down. Stepping down clears `set_leader`, ends every `Execute` and
  worker stream `UNAVAILABLE` through the existing `Closer`
  (`crates/kbf-front/src/execution.rs:183`), and stops ticks and collection.
- The Raft loop (tick, persist, send) runs on its own thread, apart from the REAPI
  runtime, so a burst of client calls cannot starve it.

### 5.2 The commit latency budget

R-ack ties every daemon's contact to commit latency. A daemon heartbeats every 5 s (the
default `--heartbeat-interval-ms`) and stops self-fenced work once its newest
acknowledged heartbeat was sent more than T = 40 s ago. A quorum-wide stall in commits
longer than about T minus two heartbeat intervals therefore fences every daemon at once.
That outcome is safe (no work runs twice) but expensive, so:

- the target is a commit p99.999 under 1 s, with an alert at p99 over 100 ms;
- the fsync count is the group-commit rate (at most about 50 a second), not the
  heartbeat count;
- the simulation runs a quorum-wide fsync stall just under and just over that bound
  ([section 15](#15-simulation)): under it no daemon fences; over it every daemon
  fences, no lease runs twice, and the alert fires.

### 5.3 Fencing without a leader lease

`fence.rs:13-14` argues that `T + LEADER_LEASE_MARGIN < G` keeps two copies apart
because "leaders acknowledge only inside their leader lease". Raft gives no such lease,
and this design does not add one: leases are clock-bound and break under process stalls
and with leadership transfer. R-ack replaces the premise:

1. A daemon sends heartbeat h at real time s. The leader acknowledges h only after the
   entry carrying h has committed.
2. A committed entry is in every later leader's log (Leader Completeness). A later
   leader L′ cannot append entries of an earlier term once elected, so L′ held h's
   entry before it won, and its takeover happens at real time r ≥ s.
3. Applying L′'s `Takeover` sets every worker's last-heard time to the takeover's farm
   time. L′ re-dispatches the worker's leases no sooner than G of its own clock after
   r.
4. The daemon stops self-fenced work at s + T at the latest, counted from the send time
   (`crates/kbf-daemon/src/contact.rs:5-10`).
5. So two copies are kept apart if G, as measured by L′'s clock, exceeds T as
   measured by the daemon's clock: `G · (1 − ρ) > T`. Here ρ is the relative rate
   error between the clock of the leader that re-dispatches and the clock of the
   daemon, over a minute, because T is measured on the daemon and G on the leader. The
   existing 5 s `LEADER_LEASE_MARGIN` is that budget (ρ up to about 8 %). No agreement
   between clocks is assumed, only their rates.

The constant is renamed `CLOCK_RATE_MARGIN` and keeps its 5 s value. It appears in
three places in `fence.rs`, and all three headers are rewritten to this argument:

- `T + CLOCK_RATE_MARGIN < G` (`fence.rs:13-14, 42-45`): as above.
- `HANDOVER_GRACE = T + CLOCK_RATE_MARGIN` (`fence.rs:46-54`): a replaced daemon
  process's stream is not acknowledged after the replacement commits, so it stops
  self-fenced work within T of its last acknowledged send, on its own clock. The
  scheduler waits `HANDOVER_GRACE` on the leader's clock; the margin is again the
  rate budget.
- `W + T + CLOCK_RATE_MARGIN < START_GRACE` (`fence.rs:63-77`): the `Start` and the
  heartbeat the scheduler had heard when it sent it are both in the committed log.
  The heartbeat's stamp is at most the `now` of the entry that carries the grant,
  under the max rule of [section 4.4](#44-farm-time), so the argument holds unchanged
  across leaders. Commit latency does not enter it, because W is counted on the
  daemon from its own send.

The compile-time assertions stay. A process stall on the old leader is harmless: it
delays acknowledgements, and the daemon counts from send time.

### 5.4 Read index

ReadIndex (dissertation 6.4), for the few reads that need linearizability. A follower
asks the leader for its commit index; the leader confirms it still leads with one
heartbeat round, and answers only after an entry of its own term has committed; the
follower waits until it has applied that index. No lease reads: they would bring the
clock assumption back. Used, if testing shows a need, for the result read in
`GetActionResult`. `QueryWriteStatus` does not need it: it goes to the leader
([section 10](#10-the-front-and-health)), and it answers complete or 0
(`crates/kbf-front/src/bytestream.rs:180`). A lost touch does **not** need one: under
R-local the forwarded `Touch` has applied locally before the re-query, so the re-query
sees the loss.

### 5.5 Leadership transfer and a planned stop

TimeoutNow (dissertation 3.10). `SIGTERM` on the leader:

1. stop accepting new proposals (forwarded commits and worker inputs answer
   `UNAVAILABLE`; the daemon's next try reaches the new leader);
2. pick the voter with the highest match index, bring it up to the commit index, send
   it TimeoutNow; its election is exempt from stickiness
   ([section 5.1](#51-election));
3. wait for step-down, at most two maximum election timeouts;
4. `Closer`: end `Execute` and worker streams `UNAVAILABLE` (worker streams with the
   redirect naming the new leader);
5. exit.

Because fencing needs no leader lease, the old leader has no lease to give up before
the transfer: once the target is elected, the old leader cannot commit, so it cannot
acknowledge.

## 6. Membership

- **Single-server changes** (dissertation 4.1) as `Payload::Config` entries. A config
  takes effect when it is appended, not when it commits. At most one change may be
  uncommitted, and a leader commits its own term's `Blank` before it proposes a change
  (the fix for the single-server-change bug in Ongaro's 2015 errata). `Config` stops
  being static: the membership is the newest config entry in the log or snapshot.
- **Addresses live in the config entry**: `Member { id, peer_addr, worker_addr,
  reapi_addr, role }`. Every server names the same leader address in a redirect, and no
  per-host file can drift. Server ids are never reused.
- **Operations**, through the operator API and an admin command (its name is chosen
  in PR 31): `status`, `add-learner`, `promote`, `remove`, `transfer`. Production changes run from a reviewed script that
  defaults to a dry run. The tool's refusals, all of them:
  - any change while another change is uncommitted;
  - `add-learner` of a node that cannot apply the committed format and machine version
    ([section 4.7](#47-versions-and-upgrades)), or under a server id used before;
  - `promote` unless the learner is serving reads and its match index has stayed within
    1 000 entries of the commit index for 60 s;
  - `promote` while any voter is unhealthy (it would raise the quorum with no healthy
    node to meet it);
  - `remove` of a healthy voter while another voter is unhealthy;
  - `remove` of the last voter;
  - a change into a 2-voter configuration unless both remaining voters are healthy and
    caught up;
  - in a 3-voter group, `remove` of an unhealthy voter unless a learner already passes
    the `promote` test, so the 2-voter window lasts one commit (lean (a) of
    [open decision 11](#18-open-decisions)).

  While a voter is unhealthy, the tool therefore still allows `status`, `transfer` to a
  healthy caught-up voter, `add-learner`, `remove` of a learner, and `remove` of the
  unhealthy voter: everything the replacement paths below need. A learner does not
  count toward the quorum, so adding one never reduces the slack.
- **A leader removing itself** steps down after the removal commits. A removed node
  exits after it applies its own removal.
- **A node that lost its log** (a wiped partition, a corrupt log, a digest mismatch)
  always comes back as a new learner with a new id after its old id is removed. Reusing
  a voter id after losing its log could let it vote twice in one term.

**Paths.**

| From | Steps | Window |
|---|---|---|
| 1 voter (phase 1) to 3 | add B and C as learners; let them catch up; promote B; promote C | 2 voters, quorum 2, for the minutes between the promotions; the tool measures it and refuses to open it unless A and B are healthy |
| 3 to 5 + 1 (phase 3) | add D, E, F as learners; promote D, then E; F stays a learner | none: quorum 3 of 4, then 3 of 5 |
| replace a dead voter, 5 + 1 | remove the dead voter (4 voters, quorum 3); promote F (5 voters); add a new learner | slack 1, then 2 |
| replace a dead voter, 3 voters | add the new node as a learner while the dead voter is still a member, and wait until it is caught up; remove the dead voter (2 voters, quorum 2); promote the new node | the 2-voter window lasts one commit, because the new node is already caught up |

The log id, and so the daemons' epoch, never changes along any of these paths.

## 7. Failover, step by step

### 7.1 Unplanned: the leader is killed mid-build

1. **t0.** The leader process (or host) dies. Peer streams, daemons' worker streams and
   the front's streams to it break by reset, or go silent if the host vanished.
2. **Election**, 2 to 4 s. A follower's timer fires, its prevote and vote succeed, and
   it appends a `Blank`.
3. **Takeover.** Right after the `Blank` commits, the new leader proposes
   `Takeover { term, now }`. Applied on every replica, it:
   - sets every worker's last-heard time to `now`, so G counts from the takeover;
   - marks every worker session stale and every worker unplaceable until its next
     `WorkerUp`, so no `Start` goes to a stream that lived on the dead leader;
   - leaves every operation, waiter and grant in place.
4. **Leader-ready.** Once `Takeover` has applied, CheckQuorum holds and the store probe
   passes, the server sets `set_leader(true)`: `/readyz/leader` and the health service
   `kbf.leader` turn serving, and the front moves the leader cluster within one health
   interval. The tick loop and (once wired) collection start.
5. **Daemons reconnect.** A daemon reaches a follower, which ends the stream with the
   redirect, or reaches the leader directly. `Hello` becomes a logged `WorkerUp` (a new
   session); after it commits, `Welcome { epoch = log id }`. The epoch is unchanged, so
   `new_epoch` drops nothing.
6. **Results resent.** The daemon resends every unacknowledged `Result`
   (`daemon.rs`, unchanged). The new leader's `holder()` reads the replicated `started`
   table and finds the lease. It commits the `Report`, then acknowledges; a result the
   old leader had already committed is acknowledged as a duplicate.
7. **First heartbeat of the new session.** The daemon lists every lease it holds. A
   listed lease keeps running. A grant it does not list is requeued at once: the old
   stream is dead, so no `Start` from it can still arrive, and the existing rule for
   "a `Start` that went to an earlier session of the same process" (`kbf-sched`
   `Event::WorkerUp` and `Event::Heartbeat`) already decides it.
8. **Workers that never reconnect** (the dead host's own daemon, on a converged host):
   G after the takeover, their leases are requeued.
9. **Clients.** `Execute` and `WaitExecution` streams on the dead leader break. The
   client retries `WaitExecution(name)`; the front sends it to the new leader, which
   finds the operation and attaches a local channel, then sends the current stage and
   later the result. If the operation finished meanwhile, the kept answer is sent. A
   client that re-runs `Execute` instead joins the running twin if the request is
   joinable, or gets an action-cache hit if the result committed; a non-joinable
   re-`Execute` queues a second operation, which is correct but wasteful. Which of these
   Buck2 and Bazel actually do is measured first ([PR 0](#16-pull-request-plan)), not
   assumed.

**Budget:** detect and elect 2 to 4 s, takeover under 1 s, daemon reconnect under 3 s
(redirects are followed at once, and backoff is capped at 2 s while a daemon holds
leases), first acknowledged heartbeat within one interval: about 10 s against T = 40 s.
Kill-to-leader-ready over 10 s is an alert.

### 7.2 Planned: the leader is stopped

The `SIGTERM` sequence of [section 5.5](#55-leadership-transfer-and-a-planned-stop).
The gap is one round trip plus the `Takeover` commit; daemons are redirected to the
target by name, and no self-fenced run comes anywhere near T.

### 7.3 A follower dies

Scheduling is untouched. The front drops it from the read cluster; reads and uploads it
was serving fail and the clients retry on another server. A daemon reading blobs from it
moves to the next address.

## 8. Reads and uploads on every server

**`RaftMetaLog`** implements the existing `MetaLog` trait, so `kbf-front` does not
change:

- `query` runs on the server's own applied `MetaState` (the contract text in
  `meta_log.rs:6-7, 33` changes to say so);
- `commit` proposes on the leader, or forwards the command to the leader on a follower
  (peer RPC `ForwardCommit`), and **resolves once the entry has applied on this
  server**. The leader returns the entry's `(term, index)`; if the entry this server
  applies at that index has another term, the forward was lost and the answer is
  `Unavailable`, so the caller retries;
- with no leader known, or a forward that fails, the answer is
  `MetaLogError::Unavailable`, which becomes `UNAVAILABLE` and is never turned into
  "absent".

**Serving reads (the read gate).** A server serves reads only while all of these hold:
it knows a leader; it received an append from that leader within `READ_STALENESS`
(5 s); it has applied up to the commit index that append carried; its store probe
passes; its newest checkpoint digest matched; it is not stopping. The leader passes
while it is leader-ready. **With no leader known within `READ_STALENESS`, no server
serves reads**: read calls answer `UNAVAILABLE`, never a stale answer.

How far behind a server serving reads can be is not exactly `READ_STALENESS`. A leader
that has lost its quorum still passes its own gate until CheckQuorum steps it down,
which takes up to about two maximum election timeouts (8 s). During that time a follower
on its minority side keeps receiving appends from it, and so passes too. Both can miss
entries the new majority commits for that long. The bound is therefore
`READ_BEHIND_BOUND = READ_STALENESS + 2 × maximum election timeout` (13 s), and the
constant checks below use it.

**Why a server never answers a false "present".**

- Its applied state is a prefix of the committed log. It can only be behind, never
  ahead.
- **A touch that commits** applies locally before the answer (R-local), so the answer
  includes every earlier commit, collections too, and a lost touch makes the re-query
  see the loss.
- **A touch that is skipped** (the entry was touched within the one-day quantum, by the
  follower's view): the leader's view of that entry's last touch is at least as recent,
  and `Collect` removes an entry only once it is `min_ttl` plus one quantum old. The
  leader can have collected it only if the follower is more than `min_ttl` (7 days)
  behind. The server refuses at start a retention whose `min_ttl` is not at least 1 000
  times `READ_BEHIND_BOUND`.
- **Bytes** are deleted only after the condemn delay D, and only in the order of
  [section 4.10](#410-deleting-objects); `READ_BEHIND_BOUND` times 100 must be less
  than D, checked at compile time.
- **An object found unreachable** on the leader but not yet marked on a follower: the
  follower reads the bytes itself, sees the store's error, and commits its own
  `ObjectUnreachable`. Until then `FindMissingBlobs` may report it present, exactly as
  the leader did before it found the loss; the window is bounded by
  `READ_BEHIND_BOUND`, and `Execute`'s input check (`execution.rs:378`) runs on the
  leader. An object the store loses is a store failure, not a deletion by kbf. The rule
  "never a false present" covers what kbf deletes; a store loss is answered "present"
  until some server's read sees it, and on every server for at most
  `READ_BEHIND_BOUND` after its mark commits.

**A false "missing" is harmless to a client, not to a daemon.** A client uploads the
blob again. A daemon reading an action's inputs would fail the action. So a daemon that
gets `NOT_FOUND` from a server retries on another server serving reads, ending at the
leader, before it reports a missing input ([section 9](#9-daemons)).

**Uploads on any server.** `ByteStream.Write` and `BatchUpdateBlobs` write segments
under the server's **own writer epoch** (#230), read the footer back, and forward
`PutBlobs` to the leader; the answer comes after the local apply, so a
`FindMissingBlobs` on the same server sees the upload. `Cache::open` splits in two:
reads are served at once, and the writer epoch is allocated lazily by a forwarded
`AllocEpoch` on the first upload. Until it lands, uploads answer `UNAVAILABLE`. A
follower can therefore start and replay its log before it has a writer epoch. It
serves reads once the read gate holds, which needs a known leader.

**On the leader only:** `Execute`, `WaitExecution` (`Dispatch::submit` and `wait` on a
follower answer `UNAVAILABLE`), worker streams, action-cache writes (through `Report`
apply), ticks and collection.

## 9. Daemons

On top of #300 (every address of every `--server`, names re-resolved each round,
jittered backoff):

- **Redirect.** At the top of `WorkerService::session` (`worker.rs:98`), a server that
  is not leader-ready ends the stream with `UNAVAILABLE` and trailer metadata
  `kbf-leader: <worker_addr>`, taken from the replicated membership. With no leader
  known it sends plain `UNAVAILABLE`. A daemon without redirect support treats either
  as "try the next address", so the change is compatible within version 1.
- **Following it.** The daemon dials the named address at once, if it is one of its
  configured or resolved names and verifies under the CA; a redirect is not counted as
  a failure, and one redirect per round prevents loops.
- **Backoff.** After any session that reached `Welcome`, and while the daemon holds
  leases, each try waits at most 2 s. #300's 30 s cap applies only to a daemon holding
  none, since 30 s is most of T.
- **Ack timeout.** No `HeartbeatAck` within 3 heartbeat intervals ends the stream and
  reconnects. This covers a host that went silent without a reset. `Contact` counts T
  from send time, so reconnect timing does not weaken fencing.
- **Admission limit.** The leader limits `Hello`-to-`Welcome` setups in progress (a
  semaphore, default 32) and answers `UNAVAILABLE` beyond it, so a reconnect storm after
  a failover queues in the daemons' backoff, not in the leader's commit path.
- **Blob reads spread.** `--cas` becomes repeatable (or the same per-server DNS name).
  Reads go round-robin across servers serving reads; `UNAVAILABLE` moves to the next;
  `NOT_FOUND` is retried on the others, ending at the leader, before the daemon reports
  a missing input. Uploads go to any server. The server side, `WorkerBlobs`, is already
  the same `ByteStreamService`.

## 10. The front and health

| Cluster | Calls (by `:path`) | Health service |
|---|---|---|
| `kbf-leader` | `Execution/Execute`, `Execution/WaitExecution`, `Capabilities/GetCapabilities`, `ActionCache/UpdateActionResult`, `ByteStream/QueryWriteStatus` | `kbf.leader` |
| `kbf-reads` (every member, the learner included) | `ByteStream/Read`, `ByteStream/Write`, `BatchReadBlobs`, `BatchUpdateBlobs`, `FindMissingBlobs`, `GetActionResult`, `GetTree` | `kbf.reads` |

- **Leader-ready** (`kbf.leader`, `/readyz/leader`, and `/readyz` as an alias): not
  stopping, the role is leader, `Takeover` applied, CheckQuorum satisfied, store probe
  passing.
- **Serving reads** (`kbf.reads`, `/readyz/reads`): the read gate of section 8.
- `""` answers as `kbf.reads`. #299 answers `""` and the five REAPI service names with
  `/readyz`'s answer today; it gains the two named services.
- Health checks every 1 s; unhealthy after 2 failures, healthy after 1 success. The
  front retries `UNAVAILABLE` only on idempotent read calls: the unary ones
  (`BatchReadBlobs`, `FindMissingBlobs`, `GetActionResult`) and a `ByteStream.Read`
  that has not yet sent a response message. A `Read` that has sent data is not retried
  by the front; the client resumes it at its offset. Never on `Execute`.
- The front's stream idle timeout is raised above the longest silent stream
  ([deployment-topology.md](deployment-topology.md#probes-before-relying-on-the-front)).
- The proxy-to-server hop is TLS with an internal CA (#226).
- **The peer listener** (Raft messages, `ForwardCommit`, `ReadIndex`, `TimeoutNow`) is
  its own listener with mutual TLS under an internal CA, the peer's server id bound to
  its certificate's name. It is never behind the front and never under the REAPI
  authentication policy. Every server must load the same REAPI policy and deny list;
  the deploy kit checks that their digests match.

## 11. Converged hosts

Priority object store > `kbf-server` > `kbf-daemon` > builds, as systemd settings:

| Unit | Memory | `OOMScoreAdjust` | `CPUWeight` | Other |
|---|---|---|---|---|
| object store | `memory.min` = its working set | −900 | 1000 | `MemorySwapMax=0`, `IOWeight` high |
| `kbf-server` | `memory.min` = log cache + both machines + twice the largest snapshot | −800 | 800 | `MemorySwapMax=0`, `IOWeight` high on the Raft partition |
| `kbf-daemon` | its leaf's `memory.min`, as #280 | −500 | 300 | |
| `actions/` | `memory.max` = RAM − (object store + server + OS + 4 to 8 GiB) | default; `memory.oom.group` per lease | 100 | `IOWeight` low |

Swap is off for the object store and the server: a swapped-out leader stretches its
commit latency toward the bound of [section 5.2](#52-the-commit-latency-budget). Build
hosts keep their swap for builds. The Raft partition is separate, so builds' writeback
cannot stall its fsync; fsync latency is watched there.

## 12. Failure catalogue

| Failure | Design | Report | Recover |
|---|---|---|---|
| Node dies mid-commit | answers go out only after local apply of a committed index; commands are idempotent; `Submit` is committed before the name is returned | `raft_uncommitted_dropped_total` (entries a new leader truncated) | client retries; daemon resends |
| Leader on the minority side of a partition | R-ack: it cannot commit, so it acknowledges nothing; CheckQuorum steps it down | `raft_checkquorum_stepdowns_total` | automatic; `Closer` ends its streams |
| Isolated follower rejoins | PreVote and stickiness | `raft_prevote_rejected_total` | automatic |
| Leader flapping (overload, fsync stalls) | 2 to 4 s timeouts, PreVote, a dedicated Raft thread, `MemorySwapMax=0` | more than 3 leader changes in 10 minutes | find the noisy neighbour |
| Slow follower | flow control per peer; the read gate; InstallSnapshot by reference past the base | `raft_peer_lag_{entries,seconds}` | automatic |
| Raft partition full | free-space floor: proposals refused, snapshot forced; a persist error is fail-stop | `raft_disk_free_bytes` | free space; the node rejoins from its log or a copy |
| Torn write, corrupt log | CRC per record; torn tail truncated; corruption in the middle is fail-stop | page | wipe; rejoin under a new id |
| Clock jumps | Raft timing and farm time use the monotonic clock; farm time is the committed base plus elapsed; only the clock rate matters to fencing | `farm_time_skew_ms` | none needed |
| Two servers both look like the leader to the front | each server checks its own role at call time; a stale leader cannot commit, so its `Execute` returns no name | `front_wrong_role_rejects_total` | the front's health converges |
| Daemon reconnect storm | admission limit; jittered backoff; immediate redirect | `worker_sessions_rejected_busy_total` | automatic |
| Restoring an old snapshot | deletion ordering of section 4.10; SHA-256 checked before install | `snapshot_age_seconds`, upload failures | install the newest copy |
| Membership change during a failure | one change at a time, `Blank` first, the tool's refusals, learners first | change pending more than 60 s | re-propose; the tool is idempotent |
| Object store down | Raft runs on; compaction stops (S1); uploads retried | the store probe in readiness | automatic |
| Mixed versions in a rolling upgrade | format and machine versions; unknown entries fail-stop | `raft_machine_version` | upgrade the laggard |
| Replica divergence | checkpoint digests | page | remove, wipe, re-add under a new id |
| Every voter loses its disk | restore from the newest store copy into a **new log id**: daemons kill old-epoch runs, which is right because the log tail is lost | page | the restore runbook |

## 13. Observability

**Metrics.** Raft: term, role, leader id, commit and applied index, per-peer lag in
entries and seconds, elections, prevote rejections, CheckQuorum step-downs, append round
trip, fsync latency histogram, log bytes since the last snapshot, free space. Snapshots:
age of the newest local and store copy, size, pause, upload result. Requests:
forward-commit and read-index latency and errors, `execute_submit_commit_seconds`.
Readiness: both paths per server. Workers: daemons connected per server (0 on
followers), redirects served, sessions refused by the admission limit. Takeover:
duration, leases kept, leases requeued (and why), results resent, duplicates
acknowledged, results refused. Determinism: checkpoint mismatches. Daemon side: time from
stream loss to `Welcome`, `NOT_FOUND` retries on another server.

**Alerts.**

| Alert | Fires when | Level |
|---|---|---|
| No leader | more than 5 s | page |
| Quorum at risk | healthy voters = quorum | page |
| Persist fail-stop, checkpoint mismatch | any | page |
| Membership change pending | more than 60 s | page |
| Leases requeued during a failover for a daemon that reconnected | more than 0 | page |
| Leader flapping | more than 3 leader changes in 10 minutes | warn |
| Follower lag | more than half of `READ_STALENESS` for 1 minute | warn |
| fsync | p99 more than 100 ms | warn |
| Store copy | newest older than 1 h, or uploads failing | warn |
| Raft partition | more than 80 % full | warn |
| Kill to leader-ready | more than 10 s | warn |
| A self-fenced run stopped | rate above 0 | warn |

Every election, step-down (with its reason), takeover (with its summary) and membership
change is also one structured log line.

## 14. Rollout beside an existing farm

Each phase runs kbf beside an existing farm on the same six hosts until the last; the
object store is shared, with kbf in its own buckets.

| Phase | kbf | Existing farm | Gate to the next phase |
|---|---|---|---|
| 1 | 1 node: a single-voter Raft log, converged | 5 nodes | the phase-1 PRs merged; a planned restart of the kbf server during a real build keeps its leases; 7 days of real builds with no failure caused by kbf |
| 2 | 3 nodes: 3 voters | 3 nodes | the [acceptance test](#17-phase-2-acceptance-test) passes, with its negative control red; then 7 days of the 3/3 split with no failure caused by kbf |
| 3 | 6 nodes: 5 voters + 1 learner | retired | |

**Running side by side.**

- **Hosts are in exactly one farm.** A host moves by draining it in one farm (cordon,
  wait for its leases) and joining it to the other. No host runs both farms' daemons.
- **Clients choose by configuration.** A named set of builds (lean: the CI lanes of
  one repository first, then more) points at kbf's front; the rest stay on the
  existing farm. The caches are separate; nothing copies results between them.
- **Comparison.** For the same commits built on both farms: per-target success, action
  failures attributed to the farm, cache hit rate, and p50/p95 action latency. A build
  that fails on kbf and passes on the existing farm is investigated before the next
  phase.
- **Rollback.** One reviewed script, dry-run by default, points the kbf client set back
  at the existing farm's front and cordons and drains kbf. kbf's state stays on disk,
  so a later roll-forward resumes it.

**The first upgrade from today's server** changes the epoch once (process term to log
id) and the operation names, so it kills running leases. It runs with kbf drained.
Rolling back from the Raft log to today's in-memory metadata loses the index: the
cache is cold and the objects already written are orphaned under the old prefix. The
binary keeps `--meta-log=memory` for one release so that rollback exists.

## 15. Simulation

`kbf-sim-cell` (a stub today) becomes the whole cell on the `kbf-sim` kernel:

- N servers, each a Raft core, the codec, `MetaState`, `FarmMachine` and an in-memory
  log store with crash points between every write and fsync;
- a fake object store, with a missing manifest answering 404 and slow uploads;
- fake daemons that use the real `Contact` and `new_epoch` code;
- fake clients that `Execute`, `WaitExecution`, `FindMissingBlobs`, read and write;
- faults: drops, duplicates, reordering, partitions, crashes part-way through effects,
  slow and failing fsync, clock-rate skew, leader kills and transfers, membership
  changes, snapshot installs.

**Properties checked after every step, over a seed sweep:**

1. **Replica equality.** Two replicas that have applied index i have equal digests at i.
2. **One run.** No operation ever has two unfenced daemon runs at once.
3. **Leases kept.** A failover in which no daemon is silent for T requeues no lease a
   daemon listed.
4. **Exactly one result.** Every resent result is accepted once; the action-cache entry
   exists if and only if the result record won.
5. **No false present.** Whenever a server answers "present" at time t, the object is
   readable in the fake store from t until at least `min_ttl` later, unless the fake
   store lost it (a loss fault). After a loss fault, no server answers "present" later
   than `READ_BEHIND_BOUND` after the loss mark commits. GC never deletes it earlier.
6. **Monotone time.** `now` never decreases along the log.
7. **Liveness.** Under eventual stability, every submitted operation is answered.
8. **Raft safety.** The four Figure 3 properties, with membership changes and snapshot
   installs.

**Cases.** Leader killed mid-commit, mid-grant and mid-result-acknowledgement; a leader
partitioned from its quorum while it keeps worker streams; a slow follower crossing the
snapshot base; a full disk on the leader; a quorum-wide fsync stall just under and just
over T minus two heartbeat intervals; clock-rate skew within the margin (must be safe)
and past it (expected to violate property 2: the test asserts that it does, which
documents the bound); a membership change with a crash; a reconnect storm of 50 daemons;
the 1 → 3 → 5 + 1 rollout path; a voter replaced.

**Mutants a verifier plants**, each of which must turn a property red:

| Mutant | Red |
|---|---|
| acknowledge a heartbeat before its entry commits | one run, under a partition |
| `Takeover` does not reset last-heard times | one run |
| `Welcome.epoch` = the Raft term | leases kept |
| `Effect::Commit` fed back only on the leader | replica equality |
| a `Sent` entry recorded after the `Start` is sent, not at apply | exactly one result |
| a duplicate `Result` refused | exactly one result, or liveness |
| config change before the term's `Blank` | Raft safety under changes |
| compact past the newest store copy | liveness (a follower stuck behind the base) |
| `max(now)` dropped from apply | monotone time |
| a follower answers while outside the read gate | no false present |
| objects deleted before the snapshot copy | no false present after a restore |
| redirect counted as a failure, with a 30 s cap | leases kept |
| `LeaseOffer` sent before commit | a daemon sees an offer for a grant that never commits |
| stickiness applied to a TimeoutNow election | a planned transfer fails over to an unplanned election (transfer seeds) |
| `ObjectReachable` clears the mark whatever its generation | no false present, with a forward delayed past a newer loss |
| condemned objects deleted while an older retained copy references them | no false present after restoring that copy |

## 16. Pull request plan

Each PR is small, about one thing, and lands on `main` on its own. "Red without" is the
test that fails without the PR; "Mutant" is the defect a verifier plants to see it go
red. Sizes: S (under 300 lines), M (300 to 800), L (over 800, to be split if review asks).

**Phase 1: a durable single voter that keeps leases across a restart.** These come
first, because phase 1 needs no multi-node mechanism.

| # | PR | Depends on | Size | Red without | Mutant | Sim |
|---|---|---|---|---|---|---|
| 0 | Probe, no code: how Buck2 and Bazel react to a dropped `Execute` stream (reset and `UNAVAILABLE`) and to `NOT_FOUND` from `WaitExecution`, through the real front | — | S | n/a | n/a | n/a |
| 1 | This document; ADR 0001 status | — | S | n/a | n/a | n/a |
| 2 | `log.proto` and the `Command` codec; the loss mark's generation in `kbf-meta` ([section 4.6](#46-encoding)) | 1 | M | golden bytes; round trip of every variant; every forwardable command applied twice equals once; a delayed `ObjectReachable` applied after a newer `ObjectUnreachable` leaves the mark | swap two field tags; default an unknown value; clear the mark whatever its generation | n/a |
| 3 | `kbf-store`: segmented log, hard state, CRC, torn tail, fail-stop on fsync error | 1 | M | a fault-injecting filesystem: a torn write at every byte offset, a crash between write and fsync, an fsync error | return before fsync; skip the directory fsync; accept a bad CRC in the middle | n/a |
| 4 | Raft host loop over `Storage` and `Transport` traits | 3 | M | crash at every effect boundary recovers to a log that satisfies the Figure 3 checks | send before persist; apply before commit | crash seeds on the existing Raft sim |
| 5 | `RaftMetaLog`, one voter, behind `--meta-log=raft --raft-dir`; `Bootstrap` with log id and stable `<prefix><log_id>/`; farm time from the committed base; leader-only `Tick` | 2, 4 | M | `cold_restart` keeps blobs and action-cache entries | start-time prefix; farm time from the process `Instant` | n/a |
| 6a | `FarmMachine`, step 1: waiter records and the `started` table move into a pure struct; no behavior change | 1 | M | two machines fed the same inputs are equal; existing `kbf-server` tests stay green | a hashed map in the struct (clippy and the equality test) | n/a |
| 6b | `FarmMachine`, step 2: node registry and node status | 6a | S | registry survives in the machine; equality | drop a field from equality | n/a |
| 6c | `FarmMachine`, step 3: memory floors and the doubled-booking requeue | 6a, #284 | S | equality over generated OOM sequences | forget the floor on requeue | n/a |
| 7 | Control codec: every `FarmInput`, `Takeover`, `Report`, `Checkpoint`, `MachineVersion` | 2, 6a, 6b, 6c | M | golden bytes; round trip | as PR 2 | n/a |
| 8 | `kbf-sim-cell` skeleton: one server over PRs 3 to 7, fake daemons, store and clients; properties 1 to 7 | 4, 7 | M | n/a (new harness) | the section 15 mutants that apply to one node | crash and restart seeds |
| 9 | `LeaseOffer` only after the grant commits | 1, 8 | S | a `FakeDaemon` sees no offer for a grant whose commit was refused (`GateLog`) | send the offer before commit | sim-cell: offer for an uncommitted grant |
| 10 | Control inputs through the log: `Submit` committed before the name is returned; heartbeats and `WorkerUp` batched; R-ack for `Welcome`, `HeartbeatAck`, `ResultAck`; `Effect::Commit` fed back at apply; `Takeover` on restart | 5, 6b, 6c, 7, 8, 9 | L | **a server restart mid-lease: the daemon reconnects, its lease keeps running, its result is accepted** (red today: `STATE_IN_MEMORY`) | acknowledge before commit; skip the `Takeover` reset | sim-cell restart seeds |
| 11 | `Welcome.epoch` = log id (non-zero); `LeaseId.term` from Raft; duplicate `Result` acknowledged | 10 | S | the restart test with the epoch check; a resent, already-committed result is acked accepted | epoch = term; refuse the duplicate | sim-cell: property 3 |
| 12 | `Report` entry: result and `PutAction` atomic | 10 | S | a crash between the two today leaves an accepted result with no action-cache entry; after, a crash at every point leaves both or neither | commit the two separately | sim-cell: property 4 |
| 13 | Operation names `operations/<log_id>-<waiter>`; the existing finished-operation window (`FINISHED_RETENTION`, `farm.rs`'s `finished`) replicated and lengthened | 10 | S | `WaitExecution` after a restart finds the operation (`NOT_FOUND` today), and a finished one returns its result | parse under the wrong log id; drop the kept answer | n/a |
| 14 | Snapshot codec for both machines; local snapshots; compaction; store copy with the manifest last; S1 | 5, 6b, 6c, 10 | M | `restore(snapshot at k) + replay(k..n)` equals `replay(0..n)`; a fresh server restores from the store copy after compaction | write the manifest first; compact past the copy; drop `next_epoch` from the snapshot | sim-cell: snapshot-copy failures |
| 15 | Checkpoint digests and fail-stop on mismatch | 14 | S | a planted divergence stops the node and pages | compare at the wrong index | sim-cell: divergence seeds |
| 16 | Phase-1 metrics and alerts (section 13, single-node subset) | 10 | S | each alert fires on its planted condition | n/a | n/a |

**Phase 2: three voters.**

| # | PR | Depends on | Size | Red without | Mutant | Sim |
|---|---|---|---|---|---|---|
| 17 | Core: PreVote and stickiness, with the exemption for a transfer-flagged election ([section 5.1](#51-election)) | 1 | S | scripted: a rejoining node does not raise the term; a vote request flagged as a transfer is granted inside the stickiness window, an unflagged one is refused | skip the stickiness check; apply stickiness to a flagged request | partition-heal sweep |
| 18 | Core: CheckQuorum | 17 | S | scripted: an isolated leader steps down within one election timeout | never step down | minority-leader seeds |
| 19 | Core: single-server membership, learner promotion, addresses in the config entry | 1 | M | scripted add, promote, remove; the errata scenario has two leaders without the `Blank`-first rule | drop `Blank`-first; allow two pending changes; apply config on commit | random changes under faults with the Figure 3 checks |
| 20 | Core: InstallSnapshot by reference | 14 | M | a follower behind the base catches up; `compaction_floor` is lifted | accept a snapshot older than the applied index; skip the digest check | snapshot-install seeds |
| 21 | Core: ReadIndex | 18 | S | a read token from a deposed leader never resolves | resolve without a heartbeat round; before an own-term commit | linearizable-read checker |
| 22 | Core: leadership transfer (TimeoutNow); the target campaigns with the transfer flag of PR 17 | 18 | S | the target wins within one election timeout, with every voter inside its stickiness window, and no committed entry lost; a lagging target is caught up first | transfer to a lagging voter without catch-up; drop the transfer flag (the transfer fails and an unplanned election follows) | transfer under drops |
| 23 | Peer transport `kbf.peer.v1`: own listener, internal-CA mutual TLS, id bound to the certificate name | 4 | M | 3 in-process servers elect and replicate over loopback; a wrong-name peer is refused | skip the name check | n/a |
| 24 | Multi-server harness: N real `Cell`s over PR 23, a shared store, kill, partition, transfer | 23 | M | n/a (harness) | n/a | n/a |
| 25 | Follower inert; leader-only acting; `Takeover` after election; `set_leader` from the role; workers unplaceable until `WorkerUp`; `Closer` on step-down | 10, 18, 24 | M | 3 `Cell`s: kill the leader; leases kept, results accepted | placement on a stale session; `leader` defaulting to true | sim-cell failover seeds, N = 3 and 5 + 1 |
| 25b | Rollout records through the log: the leader's rollout driver proposes step started, step done and paused; the records replace `MemoryRolloutStore`; a failover pauses a rollout and the new leader resumes it | 7, 25 | S | 3 `Cell`s: kill the leader mid-rollout; the new leader resumes it from the next undone step, and no step runs twice | keep the records leader-local; resume from the first step | sim-cell failover seeds with a rollout |
| 26 | Follower reads and forwarded commits; `Cache::open` split; lazy `AllocEpoch` | 21, 25 | M | a follower answers `FindMissingBlobs`; an upload to a follower is visible on the leader; with no leader, `UNAVAILABLE`, never absent | resolve at the leader's apply instead of the local one; map `Unavailable` to missing | no-false-present checker |
| 27 | The read gate; `READ_STALENESS` and `READ_BEHIND_BOUND` with their constant checks; two readiness paths; health services `kbf.leader` and `kbf.reads` | 26, #299 | S | a follower is serving reads and not leader-ready; a lagging follower is not serving reads; with no leader for `READ_STALENESS`, no server is serving reads | one status for both; gate on "knows a leader" only; serve reads with no leader | n/a |
| 28 | Worker redirect on the server; the daemon follows it; 2 s backoff while holding leases; admission limit | 19, 25, #300 | M | a daemon dialing only a follower reaches the leader in one round; 50 daemons reconnect without a refused commit | redirect counted as a failure; follow an address outside the membership | daemon-reconnect seeds |
| 29 | Daemon ack timeout | 28 | S | a daemon facing a black-holed leader reconnects before T | no timeout | black-hole seeds |
| 30 | Daemon `--cas` spread with the `NOT_FOUND` fallback ending at the leader | 26 | S | an input missing on a stale follower is still fetched; reads go on when one server dies | no fallback; always the first address | n/a |
| 31 | Membership admin API and its admin command, with the refusals of section 6 | 19, 25, 27 | M | 1 → 3 in the `Cell` harness while a fake build runs; a dead voter of 3 replaced by the add-learner-first path; each refusal tested, and each allowed step of both replacement paths accepted | promote an un-caught-up learner; open a 2-voter window with an unhealthy node; refuse `add-learner` while a voter is dead | sim-cell rollout path |
| 32 | `SIGTERM`: transfer, wait, `Closer`, exit | 22, 25, 28 | S | a graceful leader stop with an open `Execute`; `WaitExecution` on the new leader completes | `Closer` before the transfer | n/a |
| 33 | Format and machine versions; golden-log replay test in CI | 7, 23 | M | a mixed-version cluster never sees an entry it cannot apply; an ungated behavior change fails the replay | propose before every member supports it | n/a |
| 34 | Leader-only collection with the deletion order of section 4.10 (retained copies pin their objects) and the constant checks | 14, 26, 27 | M | restoring the newest copy after a collection references no deleted object; restoring the oldest retained copy does not either; an `InstallSnapshot` whose copy is deleted mid-fetch is retried from the newest copy | delete before the snapshot copy; delete while an older retained copy references the object | property 5 with GC on |
| 35 | Phase-2 metrics and alerts (the rest of section 13) | 25 | S | each alert on its planted condition | n/a | n/a |
| 36 | Front configuration in the deploy kit: method routing, both health checks, the TLS hop (#226), idle timeout; per-server policy digest check | 27, #226 | S | `kbf-it` through a real front: the 64 MiB round trip, the 300 s silent stream | n/a | n/a |
| 37 | `kbf-it` m3 cell and the acceptance script | 20, 25b, 28 to 36 | M | section 17 | the negative control of section 17 | n/a |
| 38 | Docs: the corrections of section 19 | 37 | S | n/a | n/a | n/a |

**Phase 3** adds no mechanism: the 5 + 1 profile in the simulation and the harness,
and the admin steps of section 6. (Rollout records are replicated in phase 2, PR 25b.)

## 17. Phase-2 acceptance test

A `kbf-it` cell (`m3`) on real processes and hosts.

**Setup.** Three storage nodes in the converged profile, each running a voter; the
shared object store with kbf in its own buckets and a **fresh log id**, so the cache is
cold and actions really run; the front with both clusters, the TLS hop and peer mutual
TLS; at least two daemons on other hosts as well as the converged ones. Before run C,
the object store's layout is checked to survive the loss of one host; if it cannot, run
C is reported BLOCKED rather than skipped.

**Workload.** A cold build of a named set of komira targets through kbf, running at
least 10 minutes, including the long-running real Buck2 sleep test action, so leases are
certainly running at every kill.

**Runs**, each repeated, rotating the victim until every node has been killed at least
once, and each at a moment with at least 4 leases running:

- **A, planned:** `systemctl stop` on the leader (transfer, then `Closer`).
- **B, hard kill of the server:** `kill -9` of the leader's server process only; its
  daemon stays up and must reconnect within T.
- **C, host loss:** power off the leader's host: server, daemon and object-store node.
- **D, follower:** `kill -9` of a follower, then restart it after it has fallen behind
  the snapshot base.

**Pass criteria, every run:**

1. The build succeeds with zero action failures caused by the failover, and its outputs
   are byte-identical to a reference build.
2. No daemon logged "leases of an earlier epoch dropped", no self-fenced run stopped,
   and leases requeued for daemons that reconnected = 0 (from the metrics). In run C,
   the dead host's leases are requeued only after G.
3. No action key ran on two leases at once (daemon logs: start and end per lease).
4. Every resent result was accepted once or acknowledged as a duplicate, never refused;
   the action cache holds every result.
5. Client logs show whether each client sent `WaitExecution` or re-ran `Execute`; both
   resolve without rerunning an action that was running.
6. Kill to leader-ready under 10 s; every daemon reconnected within 15 s; followers
   serving reads again within 30 s.
7. A second identical build is 100 % action-cache hits.
8. The killed node rejoins, from its log or a store copy, and serves reads again.
9. Final checkpoint digests are equal on all three nodes, and a scan after the run
   reads every blob any server answered "present" for.

**Negative control.** Run B again with the daemon built to drop leases on any
`Welcome` (the PR 11 mutant). The test must fail on criterion 2. A gate is proven only
by a red run.

## 18. Open decisions

Each lists the options and the lean.

1. **Heartbeats in the log.** (a) Every heartbeat, batched; (b) only changes, with
   liveness kept on the leader. **Lean (a):** replicas stay identical and fencing stays
   provable; the cost is group commits, not one fsync per heartbeat.
2. **Fencing basis.** (a) R-ack, acknowledge after commit, no leader lease; (b) a
   clock-bound leader lease. **Lean (a).**
3. **Election timeout.** (a) 2 to 4 s; (b) 1 to 2 s, faster failover, more flapping on
   loaded converged hosts. **Lean (a)**, fixed after the phase-2 fsync measurement.
4. **Redirect form.** (a) `UNAVAILABLE` with a `kbf-leader` trailer; (b)
   `FAILED_PRECONDITION` with a status detail. **Lean (a):** older daemons already treat
   it as "next address".
5. **Where member addresses live.** (a) In the config entry; (b) in each server's
   configuration. **Lean (a).**
6. **Upgrades.** (a) Format and machine versions, rolling; (b) stop-the-world for every
   change. **Lean (a)**, with (b) as the fallback for ungateable changes.
7. **Daemon backoff while holding leases.** (a) 2 s cap while holding leases, #300's
   30 s otherwise; (b) one global cap. **Lean (a).**
8. **Snapshot cadence and retention.** **Lean:** every 100 000 entries, 64 MiB or 10
   minutes; keep 5 copies plus one a day for 7 days. Retained copies pin the objects
   they reference (section 4.10), so condemned bytes are deleted about 7 days after
   their `Collect`. The alternative, deleting older copies at each `Collect`, frees
   bytes after D but leaves no restorable copy older than the last collection.
9. **Restore when every disk is lost.** (a) New log id, so running leases die; (b) keep
   the log id. **Lean (a).**
10. **The 2-voter window in 1 → 3.** (a) Accept a window of minutes, guarded by the tool;
    (b) bootstrap a fresh 3-voter log from a snapshot (a new epoch: leases die once).
    **Lean (a).**
11. **Replacing a dead voter in a 3-voter group.** (a) Learner caught up first, then
    remove, then promote; (b) remove first. **Lean (a).**
12. **Retention of finished operations.** Today a finished operation is kept for
    `FINISHED_RETENTION` (60 s), in memory. (a) 1 h; (b) 24 h; (c) keep 60 s. **Lean
    (a):** a client reconnecting after a failover must still find it.
13. **Routing of `QueryWriteStatus`, `GetTree` and `GetCapabilities`.** **Lean:**
    `QueryWriteStatus` and `GetCapabilities` to the leader, `GetTree` to the read
    cluster.
14. **Swap for the server and the object store on converged hosts.** (a) Off, with
    `memory.min`; (b) allowed, with a larger margin. **Lean (a).**
15. **Which node is the phase-3 learner.** **Lean:** the node with the least disk or I/O
    headroom, or the one most often down for maintenance; it serves reads either way.
16. **Phase-1 client set and the gate length.** **Lean:** one repository's CI lanes
    first; 7 days per gate.
17. **Kill types the acceptance test requires.** **Lean:** A, B and D required; C
    required unless the object store's layout cannot survive a host loss, in which case
    it is reported BLOCKED and fixed before phase 3.
18. **Who runs membership changes in production.** **Lean:** a maintainer, through a
    reviewed dry-run-by-default script that calls the admin API.

## 19. Documents this corrects

When the implementing PRs land (PR 38), and not before, because these describe `main`:

- [deployment-topology.md](deployment-topology.md), "Built and planned", Raft core:
  the snapshot base merged in #262; only InstallSnapshot and the store copy are
  missing. The "Daemons" bullets: leases of an old leader are kept, not refused (the
  open #298 records the decision).
- [storage.md](storage.md) and `crates/kbf-front/src/meta_log.rs:6-7, 33`: `query`
  runs on the server's own applied state, not on the leader.
- `crates/kbf-sched/src/fence.rs:13-14, 46-54, 63-77`: the premise becomes R-ack
  ([section 5.3](#53-fencing-without-a-leader-lease)), `LEADER_LEASE_MARGIN` is
  renamed `CLOCK_RATE_MARGIN`, and the headers of `T + margin < G`, `HANDOVER_GRACE`
  and `START_VALIDITY` are restated.
- [ADR 0001](../adr/0001-consensus.md): its scope gains ReadIndex, leadership transfer
  and InstallSnapshot by reference (done in this PR).
