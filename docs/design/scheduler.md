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

- at once, if its `Start` went to an *earlier session* of the worker (the worker either
  received it before registering again, and then lists it, or never will);
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

This puts one duty on a restarted daemon: any work it does not list in its first
heartbeat on the new stream is requeued at once, so that work must have stopped.
Re-adopting work across a daemon restart is **planned**. Today a restarted daemon
lists nothing of its predecessor's, and before its `Hello` its driver ends every run
that predecessor left on the node (issue #155). That holds only when a daemon is
started again on the same node with the same scratch directory; the runs of a daemon
that is not are left running, unfenced (see
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
  its `gpu` count, and what its platform asks of a worker (`Request::needs`).
- A placement round walks the queue in order and gives each operation to the first
  live worker, in name order, whose capabilities satisfy its platform and whose free
  room (capacity minus bookings) fits the whole request on every axis. A worker is live
  if it was heard from within G. Matches are memoised per platform request in a round.
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

- **Drivers in placement.** Only workers with a driver for the lease kind are
  feasible.
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
  of times before callers see `INTERNAL`.

## Outcomes

A daemon reports `OK` with an `ActionResult`, or a failure status. The server maps it
to the scheduler's outcome and to what callers see:

| Report | Scheduler outcome | Callers get | Action cache |
|---|---|---|---|
| `OK`, all outputs stored | `Completed` | the `ActionResult` | written if exit code 0 and not `do_not_cache` |
| `OK`, an output not stored | `Failed(Infra)` | `INTERNAL` | not written |
| `DEADLINE_EXCEEDED` | `Failed(Timeout)` | `DEADLINE_EXCEEDED` | not written |
| `INVALID_ARGUMENT` | `Failed(Invalid)` | `INVALID_ARGUMENT`, with the daemon's reason | not written |
| anything else | `Failed(Infra)` | `INTERNAL` | not written |
| (none: refused unrun) | `Refused` | `FAILED_PRECONDITION`, with the reason | not written |

A run that exits non-zero (a failing test) is still `Completed`: callers get its
result, but it is not cached. The action-cache entry is committed before the callers
are answered, so a caller that asks again at once gets a hit. Today an infrastructure
failure is answered at once; retrying it is planned (above).

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
For many servers (**planned**), the scheduler runs on the leader of a replicated
control log:

- `Commit` effects become log proposals; `Committed` inputs come from the log's apply
  path, on every replica in the same order.
- A new leader starts a new term, so its lease ids order after every earlier lease,
  and the log decides which result of an operation wins.
- Submissions are committed too, so a new leader inherits the queue (not yet: today a
  leader change would lose queued work).
- Any server front accepts `Execute` and relays it to the leader through the same
  `Dispatch` trait the front already calls.

## Where to look

- `crates/kbf-sched/src/scheduler.rs`: the state machine.
- `crates/kbf-sched/src/fence.rs`: G, T and the worker-side `SelfFence` clock.
- `crates/kbf-server/src/farm.rs`: carrying out effects, the result path, the
  action-cache write.
- `crates/kbf-sched/tests/sim_cell.rs`: the scheduler in a simulated cell, over a seed
  sweep.
- [simulation.md](simulation.md): every scheduler simulation, the invariants they
  check, and the scenarios still to build.
