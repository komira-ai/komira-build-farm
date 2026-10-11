# Scheduler

The scheduler decides which worker runs which action, and makes sure that however
workers, networks and leaders fail, each operation is answered exactly once. This
document covers the scheduler core (`kbf-sched`), how `kbf-server` carries it out
(`kbf-server/src/farm.rs`), and what is **planned**. Lease handling on the daemon side
is in [daemon.md](daemon.md); the messages are in [worker-protocol.md](worker-protocol.md).

## Shape: a pure state machine

`kbf_sched::Scheduler` implements `kbf_types::StateMachine`. It is fed `Input`s, each
carrying the farm time, and returns `Effect`s for the caller to carry out in order:

| Input (`kbf_sched::Event`) | Meaning |
|---|---|
| `WorkerUp { worker, capacity, caps }` | a worker registered (the first `Hello` of a stream): a new session |
| `Capacity { worker, capacity, caps }` | a registered worker's report changed (a `Hello` resent on the same stream) |
| `Heartbeat { worker, running }` | a heartbeat on the worker's newest stream, with the leases it holds |
| `Submit { waiter, request }` | a caller asks for an action to run |
| `Committed(record)` | a record the scheduler asked to commit is committed (fed in log order) |
| `Started { operation, lease }` | the holder started the operation |
| `Report { operation, lease, outcome }` | the holder reports how it ended |
| `Tick` | time passed: expire silent workers' leases, run one placement round, note and refuse work no live worker can run |

| Effect (`kbf_types::Effect`) | The caller must |
|---|---|
| `Commit(record)` | append the record to the control log and feed it back as `Committed` once committed |
| `Start(start)` | send `Start` for a committed lease to its worker |
| `Answer(answer)` | answer every waiter of a finished operation |
| `Waiting(waiting)` | tell an operation's waiters why no live worker can run it, or that one can again |
| `Refuse(refusal)` | answer every waiter of a refused operation (`FAILED_PRECONDITION`) |

The core reads no clock and draws no random number, so the same inputs always give
the same decisions. That is what lets every replica of a log apply it identically
and lets the simulator replay a seed exactly.

In `kbf-server` the control log is the process itself: a `Commit` is treated as
committed the moment it is appended and fed straight back. Every input is followed by
a `Tick`, so placement happens as soon as something changes, and a timer ticks once a
second when nothing does. All of this runs under one lock in input order.

## Operations

An operation is one execution of one action that one or more callers wait on. Its
states:

```
Queued -> Leased -> Running -> Completed | Failed
  |          ^         |
  |          +---------+  (lease given up: back to Queued, granted again later)
  +-> Refused             (no live worker could run it for the unservable wait)
```

- `Leased { committed: false }`: placed, the grant proposed, no `Start` sent.
- `Leased { committed: true }`: the grant is committed and `Start` has been emitted.
- `Running`: the holder said it started. `kbf.worker.v1` has no message for this yet,
  so `kbf-server` never feeds `Started` and its operations stay `Leased` until their
  result; callers see `EXECUTING` from the moment the `Start` is sent.
- `Completed` / `Failed`: a result from the current lease was committed.
- `Refused`: a refusal was committed while the operation was queued (see
  [Placement](#placement)).

**Retention.** A finished operation (`Completed`, `Failed` or `Refused`) is kept for
the finished retention after its waiters are answered (`FINISHED_RETENTION` = 60 s,
the server's `--finished-retention-secs`), then dropped at the first input at or
after its end (issue #165). While it is kept, `kbf-server` keeps its callers too, and
WaitExecution on its name streams the done operation; once it is dropped the name is
NOT_FOUND. A late input naming a dropped operation (a stale record, report or start)
is ignored, as for any unknown one, so the scheduler holds only the unfinished
operations and those finished within the retention.

**In-flight dedup.** Operations are keyed by `ActionKey`: the REAPI instance name and
the action digest. A new request whose key matches an unfinished operation joins it
as another waiter instead of queueing a second run, if the request is *joinable*:
hermetic and not `do_not_cache`. The same digest under another instance name is
different work and never joins. A more urgent caller that joins raises the
operation's QoS level.

## Leases

A lease is a numbered permission to run one operation on one worker. Its id is
`(term, seq)`: the term of the leader that granted it, and a sequence number within
that term. Ids order by term first, so every lease a newer leader grants is newer than
every lease an older leader granted. A single server's leases live and die with its
process, so each process picks its own term at start: its start time in milliseconds
on the wall clock, times 2^16, plus 16 random bits (`kbf_server::farm::process_term`).
A restarted server therefore never grants a lease id its predecessor granted (issue
[#137](https://github.com/komira-ai/komira-build-farm/issues/137)), and its terms
order after its predecessor's while the wall clock does not step back across the
restart. The replicated log's term replaces this once it is wired.

Two rules carry the scheduler's safety. Each holds whatever order inputs arrive in.

**Commit before Start.** Placing an operation emits only `Commit` of the grant. The
`Start` is emitted when the grant comes back as `Committed`, and never for a grant
that stopped being current in the meantime. A lease the log does not know can
therefore never run, and a new leader replaying the log can never send a `Start`
twice for work it does not know about.

**One accepted result per operation.** A report is turned into a proposed result only
if it comes from the lease the operation currently holds under a committed grant, and
only once per holding. A committed result is accepted only if its lease is the
operation's newest committed grant, in log order, and the operation is not finished.
Results from given-up leases, duplicates and late arrivals are dropped.

### Fencing: G and T

A worker can lose touch with the scheduler while still running work. Two clocks keep
the copies apart:

- **G = 60 s** (`fence::LEASE_GRACE`): the scheduler gives up every lease on a worker
  it has not heard from for G, and requeues the operations.
- **T = 40 s** (`fence::SELF_FENCE`): a worker running *self-fenced* work stops it once
  the newest heartbeat the scheduler acknowledged was **sent** more than T ago.

A heartbeat's send time is no later than the moment the scheduler heard it, so a
worker stops at most T after the scheduler last heard it, and the scheduler
re-dispatches no sooner than G after that. A margin of 5 s
(`LEADER_LEASE_MARGIN`) covers a change of leader; `T + 5 s < G` is checked at
compile time, so changing a constant to break it does not build.

Each lease carries a fence policy (`kbf_types::FencePolicy`):

- `RunOn` for hermetic work: it runs to its timeout and keeps its result. A duplicate
  run after re-dispatch costs only compute, and the second result loses in the log.
- `SelfFence` for networked work, which must never run twice at once.

The scheduler derives the policy from the request (`hermetic` gives `RunOn`). The
`Start` message does not carry it yet, so today's daemon self-fences every lease.

### Reconciling with what workers say they run

Every heartbeat lists the leases the worker holds (running, or finished with a result
not yet acknowledged). A committed lease that the scheduler holds on that worker and
the heartbeat leaves out is requeued:

- at once, if its `Start` went to an *earlier session of the same daemon process* (the
  daemon either received it before registering again, and then lists it, or never
  will);
- if its `Start` went to *another daemon process* that registered as the worker, once
  `HANDOVER_GRACE` (T + 5 s = 45 s) has passed since the scheduler last heard the
  worker before the newest change of process (issue #140);
- otherwise once its `Start` has been out for `START_GRACE` (equal to G), because until
  then the `Start` may still be on its way.

A lease whose result has already been reported is kept either way: that result is on
its way to the log.

The second rule assumes that a `Start` arriving after `START_GRACE` is never run. The
daemon makes this hold: it acts on a `Start` only within `START_VALIDITY` (W = 14 s) of
sending the heartbeat the `Start` names, and `W + T + 5 s < G` is checked at compile
time beside the other fence condition (see [worker-protocol.md](worker-protocol.md)).
The other way round, `Scheduler::not_held` names the leases a heartbeat lists that the
scheduler granted and no longer holds on that worker, and the server sends a `Cancel`
for each.

Sessions matter here. Only the first `Hello` on a stream registers a worker and opens
a new session. The server feeds heartbeats only from a worker's newest stream; a
heartbeat that arrives on a replaced stream is dropped and not acknowledged, so a
daemon still talking on an old stream fences on time.

A registration names the daemon process that made it: `Hello.instance_id`, drawn at
random when the daemon starts, the same on each of its streams, and never kept across a
restart. Only that process can say it no longer runs a lease, so the first rule above
holds only within one process. Two processes can register as one node: the node's
certificate is on a cloned machine, a second daemon was started with it, or a node was
replaced while the old one still ran (node identity is the certificate, not the
process). Then the newer process's heartbeats say nothing about what the older one runs,
and the older one, whose stream is no longer acknowledged, keeps running self-fenced
work until it fences, T after the scheduler last heard it. Its leases are therefore kept
for `HANDOVER_GRACE` from then, and given up by the first heartbeat after that which
leaves them out. A restarted daemon, or a rebooted node, is another process too: the
scheduler cannot tell it from a second daemon, so what it lost is requeued after the
handover grace rather than on its first heartbeat (most reboots take longer than that
anyway). The server logs every change of process on a node. A registration without an
instance id (a daemon that predates the field) is taken as another process's.

This puts one duty on a daemon: it must list everything it holds in its first heartbeat
on each new stream, or what it leaves out is requeued at once. Re-adopting work across a
daemon restart is **planned**. Today a restarted daemon lists nothing of its
predecessor's, and before its `Hello` its driver ends every run that predecessor left on
the node (issue #155), so what is requeued after the handover grace no longer runs there.
The grace rests on the older process fencing T after it was last heard; a process that
was killed fences nothing, so for its runs the restarted daemon's sweep is the guarantee,
and it holds only when a daemon is started again on the same node with the same scratch
directory. The runs of a killed daemon that is not are left running, unfenced (see
[daemon.md](daemon.md#when-the-daemon-is-killed)).

## QoS

A QoS level (`kbf_types::Qos`) says how urgent work is. It is chosen by the user and is
never a platform property, because platform properties are part of the action digest
and urgency must not split the cache.

| Level | Urgency | Meant for | Preemptible |
|---|---|---|---|
| `interactive` | 300 | a person is waiting | no |
| `ci` | 200 | CI and automated clients | no |
| `batch` | 100 | jobs, background work | yes |

A deployment may define more levels with `Qos::custom(name, urgency)`. A custom
urgency places the level among the built-in ones; it may not reuse a built-in name or
urgency. Every level at or below `batch` is preemptible.

The queue is ordered by urgency, most urgent first, then by submission order.

Today the front submits every action at `ci`. Reading the level from a request header
(`x-kbf-qos`) and from REAPI's `ExecutionPolicy.priority` is **planned**, as are
preemption of `batch` work, and fair turns between invocations within one level.

## Placement

What the code does today:

- Each worker's capacity is read from its node report at registration: `cpus` x 1000
  millicores, `mem_gib` GiB and `gpu` GPUs. The whole machine is offered. So are its
  capabilities (`kbf_caps::NodeCaps`, see [capabilities.md](capabilities.md)).
- Every action requests one core and 1 GiB (`kbf_front::DEFAULT_RESOURCES`), or the
  whole cores and GiB its `kbf-book-cpus` and `kbf-book-mem-gib` properties name (see
  [platform-properties.md](../platform-properties.md#kbf-book-cpus-and-kbf-book-mem-gib)),
  its `gpu` count, and what its platform asks of a worker (`Request::needs`). The
  scheduler raises the memory of a new submission to its action key's memory floor,
  if it has one (see "Memory kills" below).
- Each request carries its lease kind (`kbf-lease`: `action` or `whole_machine`). A
  worker is feasible for it only if its node report lists a driver that serves the
  kind (`container`, `native`, `fake` or `local` for `action`, `native-whole-machine` for
  `whole_machine`; see [capabilities.md](capabilities.md#driver-entries)).
- A placement round walks the queue in order and gives each operation to the first
  live, feasible worker, in name order, whose capabilities satisfy its platform and
  that has room for it. An action has room where its free room (capacity minus
  bookings) fits the whole request on every axis and no whole-machine lease is held. A
  `whole_machine` lease has room only on a worker that holds no lease and is at least
  its request large; it books the worker's whole capacity, which its `Start` carries.
  A worker is live if it was heard from within G. Matches are memoised per platform
  request and kind in a round.
- A `whole_machine` lease that fits nowhere holds one feasible worker it could run on:
  the one it held at the last round while that one still could, else the one with the
  fewest leases, then the least booked. No operation after it in queue order (less
  urgent, or as urgent and younger) is placed there, so that worker empties as its
  leases end; operations before it still are. The hold lasts while the operation is
  queued and the worker could run it (live, not cordoned, feasible). No lease is ever
  stopped for it. A large `action` request has no such hold yet (issue #169).
- At most `PLACEMENT_ROUND` = 256 grants are made per round (one log flush per round).
- A booking is released when the operation finishes or its lease is given up.
- Every queued operation is also checked, past the grant limit too, against what live
  workers could give it once their bookings end. If no live worker satisfies its
  platform, or none that does is large enough, it is *unservable*: the scheduler emits
  `Waiting` with a reason (no worker connected; the closest worker and the requirements
  it lacks; or the request that is too large) whenever the reason changes, and
  `Waiting` with none once a live worker could run it again.
- An operation unservable for the unservable wait (`UNSERVABLE_WAIT` = 300 s, the
  server's `--unservable-wait-secs`) is refused: the scheduler takes it out of the
  queue, so nothing places it meanwhile, and commits a `Refusal` record. Once that is
  committed the operation is `Refused` and its waiters are answered (`Refuse`). The wait
  counts from the first round at which the operation was unservable, through changes
  of its reason, and the refusal is proposed at the first round at or after that time
  plus the wait. It restarts whenever the operation is servable again. It is not zero because a worker
  that can run the work is often a moment away: daemons reconnect within seconds of a
  server restart, and a Mac that reboots is gone for minutes. Work that no kbf daemon
  could ever run is refused by the front before it is queued (see
  [capabilities.md](capabilities.md#what-the-code-enforces-today)).

**Planned**, in roughly the order they are needed:

- **Learned sizes** (`kbf-estimator`). Requests are sized from what earlier runs of
  similar actions used, with a margin that shrinks as samples grow, raised quickly
  after an overshoot and lowered slowly. Cold actions get a cautious prior.
- **Scoring.** Among feasible workers: alignment of the request with free room (so
  CPU-heavy work goes where CPU is free and memory-heavy work where memory is free),
  time saved by inputs already on the worker, a penalty for pressure, and best fit to
  keep large holes free for large work.
- **Protected floors.** Capacity a node reserves for other services is subtracted
  before it is offered.
- **Reclaimed room.** `batch` work may use memory that is booked but unused, and is
  preempted at once when more urgent work needs it.
- **Retries.** An infrastructure failure is retried on another worker a bounded number
  of times before callers see `INTERNAL`. Today only a busy node's memory kill is
  rerun (below).

## Memory kills

What the code does today (`kbf_sched::MemoryRun`; the policy is
[failure-classes.md](failure-classes.md), 6.1). A daemon's `Result` says which memory
ran out when a lease was killed for it (`Result.memory_kill`,
[worker-protocol.md](worker-protocol.md#result-and-resultack)); the native and container
drivers set it. The server
turns it into one of two outcomes, and the scheduler decides on the committed result:

- **`Failed(OutOfMemory)`: the action passed its own memory limit.** The operation is
  queued again with its memory booking doubled, rounded up to whole GiB (at least
  1 GiB), and never past the **cap**: the memory of the largest node that could run it
  when the kill is committed, one that is live (cordoned or not), serves its lease
  kind, satisfies its platform and holds its CPU and GPU request; the node of the
  killed run always counts. A doubling past the cap books the cap: 1, 2, 4, 8 GiB on
  a farm whose largest node has 8 GiB. A whole-machine lease doubles what it booked,
  the node's whole memory, so it next needs a node twice as large. A kill of a run that
  booked the cap finishes the operation `Failed(OutOfMemory)`: the action needs more
  memory than any node offers.
- **The memory floor.** Each raised booking is kept as the memory floor of the
  action's key (instance name and action digest), and a later submission of that key
  books at least the floor; a floor never falls. The floors live in the scheduler
  alone, so a server restart forgets them (one more killed run per key), and at most
  `MEMORY_FLOORS` = 65 536 are kept, the one raised longest ago forgotten first.
- **`Failed(NodeMemoryPressure)`: the node killed it while it was under its own
  limit** (a node-wide out-of-memory kill, or the node's backstop). The busy node's
  fault: the node's memory-pressure count (`Scheduler::memory_pressure`) goes up, and
  the operation is queued again with the same booking, at most `FARM_RERUNS` = 2 times
  (3 runs). It never raises the booking or the floor. After the reruns it finishes
  `Failed(NodeMemoryPressure)`.
- **The two budgets do not mix.** A rung of the ladder is not a farm rerun and a farm
  rerun is not a rung, so a long ladder never takes a busy-node kill's reruns, and
  busy-node kills never stop the ladder short of the cap. An operation runs at most
  `1 + ceil(log2(cap / first booking)) + FARM_RERUNS` times. Leases given up for
  silence, replacement, reconnection or not starting count against neither.
- A memory kill committed for a lease the operation no longer holds (it was given up
  and is queued again) changes nothing: no run is recorded and the booking stays.
- Each rerun is a requeue the server logs with its reason ("passed its memory limit
  with 1 GiB booked; it runs again with 2 GiB booked"); a busy node's kill is also
  logged as a warning on the `kbf_server::attention` target with the node's count.

## Outcomes

A daemon reports `OK` with an `ActionResult`, or a failure status. The server maps it
to the scheduler's outcome and to what callers see:

| Report | Scheduler outcome | Callers get | Action cache |
|---|---|---|---|
| `OK`, all outputs stored | `Completed` | the `ActionResult` | written if exit code 0 and not `do_not_cache` |
| `OK`, an output not stored | `Failed(Infra)` | `INTERNAL` | not written |
| `DEADLINE_EXCEEDED` | `Failed(Timeout)` | `DEADLINE_EXCEEDED` | not written |
| `INVALID_ARGUMENT` | `Failed(Invalid)` | `INVALID_ARGUMENT`, with the daemon's reason | not written |
| not `OK`, `memory_kill` own limit | `Failed(OutOfMemory)`: run again with double the memory below the cap | at the cap, `FAILED_PRECONDITION`: "kbf: the action needs more memory than any node offers: ...", with an `ErrorInfo` of reason `ACTION_OUT_OF_MEMORY` | not written |
| not `OK`, `memory_kill` node pressure | `Failed(NodeMemoryPressure)`: run again with the same memory, twice | after the reruns, `INTERNAL`: "kbf farm fault on <node>: ..." | not written |
| anything else | `Failed(Infra)` | `INTERNAL`: "the farm could not run the action"; the daemon's reason goes only to the server's log, a WARN `lease failed` line with the lease, the node, the code and the reason | not written |
| (none: refused unrun) | `Refused` | `FAILED_PRECONDITION`, with the reason | not written |

A run that exits non-zero (a failing test) is still `Completed`: callers get its
result, but it is not cached. The action-cache entry is committed before the callers
are answered, so a caller that asks again at once gets a hit. Today an infrastructure
failure other than a memory kill is answered at once; retrying it is planned (above).

## Accounting

What exists today: a runtime may attach a `kbf.worker.v1.ResourceUsage` to the
`ActionResult` it returns, inside `execution_metadata.auxiliary_metadata`. It carries
user and system CPU time, the peak resident memory of any one process, and wall time,
as the kernel measured them when the action was reaped. The test-only local runtime
fills it; the container driver does not yet. Because it travels inside the result, it
reaches the server and the action cache with it.

**Planned**: one usage record per lease (CPU-seconds, peak memory, wall phases, queue
time, and whether the work ran, hit the cache or joined a twin), kept off the dispatch
path and stored where any query engine can read it. Records feed the size estimator.
Infrastructure retries and preemptions are charged to the farm, not to the user.

## More than one server

The scheduler runs on one server today, and its queue lives only in that process.
In the planned shape ([deployment-topology.md](deployment-topology.md)) the control
log is a Raft log on local disk, with one voter first and three voters on three
hosts later, and the scheduler runs on its leader:

- `Commit` effects become log proposals; `Committed` inputs come from the log's apply
  path, on every replica in the same order.
- A new leader starts a new term, so its lease ids order after every earlier lease,
  and the log decides which result of an operation wins. A failover keeps the leases
  committed before it: `Welcome.epoch` names the log, not the leader, and daemons
  resend their results to the new leader.
- Submissions are committed too, so a new leader inherits the queue (not yet: today a
  leader change would lose queued work).
- Only the leader schedules. At three servers any server serves the read path and
  accepts upload bytes, but the client front routes `Execute` and `WaitExecution` to
  the leader, the one server whose `/readyz` passes its `leader` check, and a follower
  sends every metadata commit to the leader rather than applying it. Every daemon holds its one worker
  stream to the leader, and a follower answers a daemon's session with a redirect
  naming the leader. None of this is built: `/readyz` exists, but its `leader` check
  always passes on the one server, and there is no second readiness path and no
  redirect yet.

## Where to look

- `crates/kbf-sched/src/scheduler.rs`: the state machine.
- `crates/kbf-sched/src/fence.rs`: G, T and the worker-side `SelfFence` clock.
- `crates/kbf-server/src/farm.rs`: carrying out effects, the result path, the
  action-cache write.
- `crates/kbf-sched/tests/sim_cell.rs`: the scheduler in a simulated cell, over a seed
  sweep.
- [simulation.md](simulation.md): every scheduler simulation, the invariants they
  check, and the scenarios still to build.
