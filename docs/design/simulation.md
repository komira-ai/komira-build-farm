# Simulation: the scheduler's scenario catalog

kbf's scheduler must answer every operation exactly once and never run work where it
may not run, however workers, networks, operators and servers fail. Example tests pin
single interleavings; deterministic simulation explores many. This document lists what
simulation exists today, the invariants every simulation step must check, and the
scenario families still to build, with the harness each extends and the scheduler
mutants each must catch. It is a plan: the scenarios below that are not marked
**exists** are not written yet. Each family lands in its own pull request, with its
mutants planted and shown red.

What the scheduler does is in [scheduler.md](scheduler.md); the messages and the
daemon's side of fencing are in [worker-protocol.md](worker-protocol.md) and
[daemon.md](daemon.md); cordon, drain and rollouts are in
[fleet-updates.md](fleet-updates.md) (sections 3.3 and 4).

## 1. What exists today

| Harness | Where | What it drives | What it checks |
|---|---|---|---|
| `kbf-sim` kernel | `crates/kbf-sim` | `StateMachine` nodes on a virtual clock, a seeded ChaCha8 stream, a bus with delay, drop, duplicate, reorder and partitions; a SHA-256 trace hash | its own tests: bus faults (`tests/bus.rs`), one seed one trace (`tests/determinism.rs`), a toy counter that a seeded partition breaks (`tests/toy_counter.rs`) |
| Raft simulation | `crates/kbf-raft/tests/sim` | 3 voters and 1 learner, partitions, crashes part-way through an input's effects, log compaction (including asks past the applied index, which the core must refuse) and restarts from the latest snapshot | Raft safety after every step, a snapshot folds in only applied entries, liveness after the heal with no peer left behind a snapshot base; 500 seeds in CI, more in an ignored test |
| `sim_cell` **exists** | `crates/kbf-sched/tests/sim_cell.rs` | the scheduler on the kernel: a leader, a control log node and two workers with a `SelfFence`; 48 seeds | `Start` only after commit; each operation answered once, by its newest grant; no self-fenced work twice at once; a reboot and a daemon restart inside G; a `Start` lost on a live session; a lease hidden from the running set; replay |
| `sim_platform` **exists** | `crates/kbf-sched/tests/sim_platform.rs` | the scheduler alone with an in-process log: Linux x86-64, Linux arm64 and a one-core Mac, two random outages each; 64 seeds | grants satisfy the platform on a live worker; answered once; a refusal only after a stated reason and an unbroken unservable run; the refusal tick exactly; replay |
| `sim_cordon` **exists** | `crates/kbf-sched/tests/sim_cordon.rs` | the scheduler alone, three always-up workers, random cordon, drain and uncordon; 64 seeds | no grant to a cordoned worker; a drain never kills; a cordon never refuses; drain states true; all work runs once uncordoned; replay |
| `kbf-sim-cell` | `crates/kbf-sim-cell` | nothing yet: the planned home of the whole cell (servers, daemons, object stores) | none |

Measured on the dev box in a debug build: `sim_cell` 0.3 s, `sim_platform` 2.2 s,
`sim_cordon` 3.9 s.

What the existing sims do not cover:

- **Contention.** Every request is one core and 1 GiB at QoS `ci`, on workers that are
  rarely full. Nothing checks QoS order, multi-axis packing, GPUs, the 256-grant round
  limit, dedup joins, QoS promotion or capacity changes under load.
- **The daemon's rules.** The `sim_cell` worker has no `Start` window (W), handles no
  `Cancel`, never suspends and never sends a `Hello` from a stale session out of order.
- **Server restarts.** No sim replaces the leader's state; the scheduler's state is
  in memory and a restart loses it (issue
  [#137](https://github.com/komira-ai/komira-build-farm/issues/137) was found while
  writing this catalog).
- **Platform and cordon together**, and **rollouts**: no sim drives `RolloutDriver`.
- **Scale.** At most three workers and 40 operations.
- **A uniform checker.** Each file checks its own invariants, some at the end of a run
  only, and failures print the seed but no replay command.

## 2. What is under test

The scheduler on `main` (`crates/kbf-sched`), and the parts of `kbf-server` that
carry it out:

| Behaviour | Code | Simulate now? |
|---|---|---|
| Operation states `Queued -> Leased -> Running -> Completed / Failed`, and `Refused` | `scheduler.rs` | yes |
| Commit before `Start`; one accepted result per operation, by the newest committed grant in log order | `scheduler.rs` (`lease_committed`, `report`, `result_committed`) | yes |
| First fit: queue order, then the first live worker in name order whose caps satisfy the platform and whose free room fits CPU, memory and whole GPUs | `servable.rs` (`fit`) | yes |
| At most `PLACEMENT_ROUND` = 256 grants per tick | `scheduler.rs` (`place`) | yes |
| Request sizes: one core and 1 GiB, or `kbf-book-cpus` / `kbf-book-mem-gib`, plus `gpu` | `kbf-front/src/execution.rs` (the scheduler sees `Resources`) | sizes yes; parsing is the front's tests |
| QoS order: urgency, then submission order; a more urgent joiner promotes | `scheduler.rs` (`queue`, `submit`) | yes |
| In-flight dedup by instance and action digest; only hermetic, cacheable requests join | `input.rs` (`joinable`), `scheduler.rs` (`submit`) | yes |
| Unservable wait: a reason via `Waiting`, refusal after `UNSERVABLE_WAIT` (300 s) of an unbroken run, committed before callers are answered | `scheduler.rs` (`note`, `refusal_committed`), `servable.rs` (`verdict`) | yes |
| Cordon and drain: placement skips, drains wait until a deadline then pause, never kill; work only cordoned workers could run waits and is never refused | `cordon.rs`, `servable.rs` | yes |
| Liveness: a worker unheard for G = 60 s loses its leases | `scheduler.rs` (`expire`, `Worker::alive`) | yes |
| Reconcile with the heartbeat's running set: a lease left out is requeued at once if its `Start` went to an earlier session of the same daemon process, after `HANDOVER_GRACE` if it went to another process, else after `START_GRACE` (= G); a lease with a proposed result is kept | `scheduler.rs` (`reconcile`) | yes |
| `Capacity` (a `Hello` resent on one stream) opens no session | `scheduler.rs` (`Event::Capacity`) | yes |
| `not_held`: leases a heartbeat lists that this term granted and no longer holds there are cancelled | `scheduler.rs`, `farm.rs` (`heartbeat`) | yes |
| Daemon: self-fence T = 40 s from the newest acknowledged send; `Start` valid for W = 14 s from the heartbeat it names; a `Start` for an unacknowledged lease ignored; results resent until acknowledged | `fence.rs`, `kbf-daemon` | modelled in the worker node |
| Server: only the newest stream counts; a replaced stream's heartbeats are dropped | `farm.rs` (`is_current`) | modelled in the leader node |
| Node identity bound to the certificate, deny list | `kbf-server/src/identity.rs` | no: not scheduler state (a duplicate node id is simulated, F2.12) |
| Rollout record: steps, `max_unavailable` counted per rollout | `kbf-types/src/rollout.rs` | yes |
| Rollout driver: cordon up to `max_unavailable`, drain, hand a drained, connected node its update; hold on anything else | `kbf-server/src/rollout.rs` | yes, through its `Fleet` trait |

**Planned, so not simulated** (a scenario for any of these waits for the code): the
infra retry budget (issue
[#22](https://github.com/komira-ai/komira-build-farm/issues/22)), placement scoring and
best fit, learned sizes, protected floors, reclaimed room and preemption of `batch`,
fair turns within a QoS level, drivers in placement, `RUN_ON` on the wire (today the
daemon self-fences every lease), a `Started` message from the daemon, re-adopting runs
across a daemon restart, the replicated log and committed submissions (a new leader
inheriting the queue), `min_serving`, `accept_outage`, the last-of-class pre-gate,
per-pool slots shared across rollouts, canary and soak, and the rollout steps after
`updating`.

## 3. Conventions every simulation keeps

- **A seed is the whole input.** Every random choice comes from one `SimRng` seeded
  by the test; nothing reads a clock or the OS's entropy; every collection is ordered.
  A run records a trace (inputs and effects, as text) and its hash.
- **Replayable from one line.** Each sim file has `fn run(seed) -> World` and an
  ignored `replay` test that runs the seed in `KBF_SIM_SEED`. A failing check panics
  with the seed, the step, the violated invariant and the command:
  `KBF_SIM_SEED=<n> cargo test -p kbf-sched --test <file> -- --ignored --exact replay`.
- **Checked after every input.** The checker (section 4) runs after each input the
  scheduler is fed and each effect it returns, not only at the end. End-of-run checks
  are for liveness alone.
- **A check that cannot fail is not a check.** Each family counts the situations it
  means to reach (a paused drain, a refusal, a promotion, a full round) and asserts the
  sweep reached each at least once, as `sim_cordon` does.
- **Budget.** Each file runs in under 10 s in a debug build on the dev box, so well
  under a minute on CI runners. A large sweep is a bounded CI sweep plus an
  `#[ignore = "long: run with --ignored --release"]` test over many more seeds, with
  the command in the file's header, as the Raft simulation does.
- **Swarm.** In the randomized families each seed also switches fault kinds on or off
  and draws their rates, so rare combinations (a drain during an outage during a
  capacity change) occur without one hand-written scenario each.
- **A real bug is not simulated away.** A sim that finds one gets a public issue with
  the replay command; the scenario is fixed in its own PR with a test that failed
  before, or lands `#[ignore = "issue #N"]`.
- **Coverage.** A new test raises measured coverage of `kbf-sched` and `kbf-server`,
  and the ratchet in `coverage-baseline` must match the measurement exactly, so each
  PR re-measures with the CI coverage command and sets the rows.

## 4. Invariants

One checker, `crates/kbf-sched/tests/sim/check.rs`, shared by every scheduler sim. It
keeps a shadow of what the scheduler was fed and what it emitted (grants, commits,
starts, answers, refusals, waiting reasons, worker heartbeats, cordons) and asserts
the following. "Live" means heard from within G; "grant" means a `Commit` of a
`LeaseGrant`.

### Safety, after every step

| # | Invariant | How it is checked |
|---|---|---|
| I1 | **No lease is granted twice.** Each lease id appears in at most one grant, and a scheduler's lease ids increase. | set of granted ids |
| I2 | **Commit before Start.** Every `Start` names a lease whose grant was committed earlier and is still its operation's current holding. | shadow log |
| I3 | **One holding per operation.** An operation holds at most one lease at a time; every lease the scheduler holds belongs to an operation that is `Leased` or `Running`. | `state`, `leases_on` |
| I4 | **Each operation is finished at most once, and each waiter answered at most once**, by an `Answer` or a `Refuse` for the operation it joined. | per-operation and per-waiter counters |
| I5 | **A result is accepted only from the operation's newest committed grant.** An `Answer`'s lease is the newest grant of its operation before the result record in log order; a result from a given-up, fenced or superseded lease never answers. With a daemon model: the accepted outcome is one the run of that very lease produced (issue #137). | shadow log; outcomes tagged with their lease |
| I6 | **Bookings never exceed capacity at a grant.** At every grant, on every axis the request uses (CPU, memory, GPUs: those it asks a non-zero amount of), what is booked on the worker plus the request fits its capacity. What is booked always equals the sum of the requests of the leases held there. A capacity that shrinks below the bookings is allowed (`Capacity` does not evict), and then no request that uses the overbooked axis is granted there until it fits again; a request that books nothing on that axis may still be (a CPU-only action may go to a node whose GPUs shrank below its GPU bookings: it makes nothing worse). | shadow bookings vs `booked` |
| I7 | **Work goes only where its platform is satisfied.** Every grant goes to a worker whose capabilities, as last reported before the grant, satisfy the request (`kbf_caps::Request::matches`), and which was live. | shadow caps and last-heard |
| I8 | **No grant to a cordoned worker,** in any cordon state, across its sessions. | shadow cordons |
| I9 | **Drain never kills.** A lease on a cordoned worker that stays live and lists it is never given up. Drain states are true: `Drained` only with no lease held, `Draining` only before its deadline and with a lease, `Paused` only at or after its deadline and only an operator moves it on. | shadow, `cordon`, `leases_on` |
| I10 | **Refused only for a stated reason.** A refusal follows a `Waiting` with a reason for that operation; the operation was queued; at every tick of an unbroken run of at least the unservable wait before it, no live, uncordoned worker could run it (platform and whole capacity), and no cordoned one could either. The refusal's reason is the last stated reason with the wait appended. | reference verdict per tick |
| I11 | **Placement follows the queue.** Each round's grants are what a reference first fit gives: queued operations in (urgency descending, operation id ascending) order, each to the first live, uncordoned worker in name order that satisfies its platform and has free room, at most 256. This implies QoS order: an operation is never passed over for a less urgent one that fits on a worker it also fits on. | reference model |
| I12 | **Self-fenced work never runs twice at once** (cell sims, with a daemon model). | run intervals per operation |
| I13 | **Dedup.** A joinable request with an unfinished twin of the same instance and digest joins it; a non-joinable one never joins; a join never lowers QoS and a promoted operation is placed in its new order. | shadow in-flight map |
| I14 | **The queue is the set of queued operations.** Each `Queued` operation not awaiting a refusal is queued once, at its current QoS; nothing else is. | `queued()` vs `state` |
| I15 | **A seed replays.** Two runs of a seed give the same trace hash. | one test per file |
| I16 | **A finished operation is kept for the retention, then dropped.** Its `state` is its finished state until the finished retention after it finished, and none from the first input at or after that; the scheduler holds exactly the unfinished operations and those finished within the retention (issue #165). F4 checks it. | `state`, `waiters`, `operations()` vs shadow |

### Liveness, at the end of a run

| # | Invariant |
|---|---|
| L1 | Once faults and arrivals stop and some live, uncordoned worker could run each queued operation, every operation is finished within a bound the scenario computes (its backlog plus G plus one round), every waiter is answered exactly once, nothing is booked and the queue is empty. |
| L2 | An operation unservable for the unservable wait is refused at the first tick at or after the end of that wait, never later. |
| L3 | A lease lost to a reboot is granted again within one heartbeat and one round of the new session's first heartbeat, not after G. |
| L4 | A lease whose `Start` was lost on a live session is granted again no sooner than `START_GRACE` after the `Start`, and its operation is answered by the newer lease. |

Liveness under sustained load is **not** an invariant today: first fit holds nothing
back for large requests, and there are no fair turns within a level, so a large
request, or `batch` work under a steady stream of more urgent work, can wait for as
long as smaller or more urgent work keeps arriving (F1.5, F1.6). Bounding that waits
for scoring, reservations or fair turns (planned).

## 5. Scenario families

Each scenario lists the generator it needs beyond the family's base world, what it
checks beyond the invariants, and whether it is built now or waits for planned code.

### F1. Capacity, packing and QoS order under contention

**Harness.** The in-process world of `sim_platform` and `sim_cordon` (the scheduler
fed directly, a `Commit` fed straight back, one-second ticks), moved into
`crates/kbf-sched/tests/sim/` with the shared checker, and a new family file
`f1_capacity.rs`. Base world: 4 to 12 workers with mixed capacity (4 to 64 cores, 8 to
256 GiB, 0 to 4 GPUs) and platforms, always heartbeating; arrivals at a rate that keeps
the farm saturated; run times drawn per operation; requests from a size mix (default,
`kbf-book-cpus` and `kbf-book-mem-gib` sizes, memory-heavy, CPU-heavy, 1 to 4 GPUs) and
a QoS mix (`interactive`, `ci`, `batch`, one custom level between `ci` and `batch`).

| # | Scenario | Generator | Checks beyond I1 to I15 |
|---|---|---|---|
| F1.1 | Exact packing | requests whose sizes sum to a worker's capacity exactly | the last request that fits is granted; one more unit is not (I6 at the boundary) |
| F1.2 | Each axis full on its own | CPU-heavy and memory-heavy streams on one pool | a worker full on memory takes no memory request though CPU is free, and the other way round |
| F1.3 | Whole GPUs | GPU requests 1 to 4 on workers with 0, 1, 2 and 4 GPUs | GPUs booked never exceed a worker's count; a request for more GPUs than any live worker has is refused after the wait, with the size reason |
| F1.4 | QoS order when saturated | all four levels arriving while full | reference order (I11); within a level, submission order; an `interactive` arrival is granted at the next release that fits it |
| F1.5 | A large request behind small ones | a 64-core request queued, then a steady stream of one-core requests on 64-core workers | today: the small ones fill freed room and the large one waits while they keep coming (documented, not a failure), and runs once they stop (L1). A wait bound is planned |
| F1.6 | `batch` under steady `interactive` | interactive arrivals at saturation | today `batch` waits; it runs once interactive stops (L1). Fair turns and preemption are planned |
| F1.7 | Dedup and promotion | one key submitted by many waiters at rising QoS; the same digest under another instance; networked and `do_not_cache` twins | one operation per joinable key; promotion moves it in the queue (I13, I14); every waiter answered once with the same outcome; non-joinable twins run separately |
| F1.8 | A join after the twin finished or was refused | resubmit a key at the moment its operation is answered or refused | a new operation is queued, not a join onto a finished one; every waiter answered once (I4, L1) |
| F1.9 | Capacity shrinks below bookings, then grows | `Capacity` events with smaller and larger sizes | running leases are untouched; no grant to the worker until its bookings fit; placement uses the new room at the next round |
| F1.10 | More than one round's worth | 600 small requests arriving in one second on a large fleet | exactly 256 grants in the first round, the rest in the next rounds, in queue order |

**Mutants F1 must catch** (planted one at a time on a throwaway branch, each shown red
by the named scenario):

| Mutant | Where | Caught by |
|---|---|---|
| `fit` checks the worker's capacity, not its free room | `servable.rs` | F1.1, I6 |
| `Resources::fits` ignores memory, or GPUs | `kbf-types` | F1.2, F1.3, I6 |
| the queue is ordered least urgent first (`Reverse` dropped) | `scheduler.rs` | F1.4, I11 |
| within a level, newest first | `scheduler.rs` | F1.4, I11 |
| a promoting join changes `qos` without moving the queue entry | `scheduler.rs` (`submit`) | F1.7, I14 |
| `release` forgets to subtract the booking | `scheduler.rs` | F1.9, I6, L1 |
| a finished or refused operation stays in `in_flight` | `scheduler.rs` | F1.8, I4, L1 |
| `PLACEMENT_ROUND` not enforced, or enforced as 255 | `scheduler.rs` (`place`) | F1.10 |
| no unservable verdict for work behind a full round's cut | `scheduler.rs` (`place`) | F1.10 (I10) |
| `Qos`'s order inverted | `kbf-types` | F1.4 and F1.6 to F1.10 (I14; the checker orders by `urgency()`, not by `Qos`'s `Ord`) |

### F2. Failures: workers, networks, clocks and servers

**Harness.** The cell of `sim_cell` on the `kbf-sim` kernel, with network faults.
`sim_cell.rs` is near the 1,000-line limit, so its leader, log and worker nodes move
to a shared module (`crates/kbf-sched/tests/sim/cell/`) and F2 is a new family file.
The worker node gains the daemon rules it lacks: the `Start` window W (it remembers
the send time of each heartbeat and drops a `Start` that names one sent W or more ago,
or one it never sent), `Cancel`, ignoring a `Start` for a lease whose result is
unacknowledged, an `ABORTED` result for a fenced run, and a frozen state for suspend.
The log node gains a random commit delay, so a grant can still be in the log when its
lease is given up. The leader node gains a restart: it
replaces its scheduler and stream table with fresh ones, as a new process would. No
kernel change is needed: suspend and restart are node-internal timers, as reboots are
in `sim_cell` today.

| # | Scenario | Generator | Checks beyond the invariants |
|---|---|---|---|
| F2.1 | A worker dies mid-lease and never returns | a worker stops at a random time | its leases are requeued at the first tick at or after G past the last time it was heard, not before; granted elsewhere; answered once |
| F2.2 | Heartbeat gaps shorter than G | per-link drop bursts and partitions of 1 to 59 s | no lease is requeued while the worker is heard within G; a self-fenced run stops once T passes since its newest acknowledged send and reports `ABORTED`, as the daemon does; if the scheduler still holds the lease, that result is accepted and the operation fails as an infrastructure failure, answered once (retrying it is #22, planned) |
| F2.3 | The G boundary | ticks at exactly G minus 1 ms, G, and G plus 1 ms after the last heartbeat | the lease is kept at the tick at G minus 1 ms and requeued at the tick at G (`alive` is `now < last_heard + G`) |
| F2.4 | A late `Start` | one `Start` delayed beyond W, the rest on time; the worker's newest heartbeat taken again at `START_GRACE` minus 1 ms, `START_GRACE`, and plus 1 ms after the `Start` was sent | the daemon model drops it unrun; the scheduler keeps the lease at the heartbeat at `START_GRACE` minus 1 ms and requeues it at the one at `START_GRACE`; no run beside the retry (I12) |
| F2.5 | A `Start` lost on a live session **exists** | (`sim_cell`) | L4 |
| F2.6 | Suspend and resume of a worker | a worker freezes for a time below T, between T and G, and above G; its clock jumps on resume | on resume it fences first: no result from a fenced self-fenced run and no heartbeat that renews contact before the fence; below T nothing changes; above G its leases were requeued and its stale results lose (I5) |
| F2.7 | A paused server clock | the leader's clock stops (its process suspended) while workers' clocks run | safety holds: workers fence after T; the leader requeues only G of its own time after resuming |
| F2.8 | Reboot and daemon restart inside G **exists** | (`sim_cell`) | L3, I12 |
| F2.9 | Server restart | the leader restarts with an empty scheduler at a random time; callers resubmit; workers reconnect with running leases and unacknowledged results | every waiter of the new process answered once by a run of its own lease (I5); the new process has its own term, named as the lease epoch in `Welcome`, and workers drop the old epoch's leases on it (issue #137) |
| F2.10 | Stale session messages | duplicated and reordered `Hello`s, a heartbeat of a replaced stream arriving after the new `Hello`, a `Hello` resent on one stream for a report change | only the first `Hello` of a stream opens a session; a replaced stream's heartbeat is not fed; a resent `Hello` requeues nothing |
| F2.11 | Lost and repeated results and acks | drops and duplicates on `Report` and `ReportAck` | results resent until acknowledged; each proposed once per holding; each operation answered once |
| F2.12 | Two daemons claim one node id | two worker nodes (two daemon processes) register as the same worker in turn; on half the seeds the first then dies | only the newest stream's heartbeats count; `Start`s go only to it; the other fences in T; a lease of the other process is kept for the handover grace, then given up (issue #140); no self-fenced work twice (I12) |
| F2.13 | A lease of another term, or of another worker, listed | a worker lists leases of an older and a newer term; a worker with no room lists leases of this term held on other workers | `not_held` names neither foreign lease, and no `Cancel` goes for them; it names each lease held on another worker |
| F2.14 | A daemon crash and its restart | a worker's daemon dies at a random time and starts again at once on a new stream, its leases, results and contact clock lost | the restarted daemon's sweep ends the runs it left before its `Hello`, so their retries, after the handover grace, never run beside them (I12, issue #155); with the sweep off in the daemon model, some seed fails I12 |

Planned, not simulated: the infra retry budget (#22), `RUN_ON` hermetic leases over a
lost connection (the daemon self-fences every lease today; `sim_cell` already models
`RUN_ON` in the scheduler), re-adopting runs across a daemon restart, and a new leader
inheriting the queue.

**Mutants F2 must catch:**

| Mutant | Where | Caught by |
|---|---|---|
| `alive` uses `<=` (one millisecond late), or 2G | `scheduler.rs` | F2.3, F2.1 |
| `reconcile` drops the earlier-session rule (waits G after a reboot) | `scheduler.rs` | F2.8 (L3) |
| `reconcile` requeues an omitted lease at once on a live session | `scheduler.rs` | F2.4, F2.5 (two runs, I12) |
| `reconcile` requeues a lease whose result was proposed | `scheduler.rs` | F2.11 (R: a lease given up against the rules) |
| `reconcile` uses `>` for `START_GRACE` (one heartbeat late) | `scheduler.rs` | F2.4 (R) |
| `Capacity` opens a session | `scheduler.rs` | F2.10 (I12) |
| `result_committed` skips the newest-grant check | `scheduler.rs` | F2.1 with a log that commits out of order (I5), and possibly F2.9 (a server restart) once #137 is fixed. With one term and a log that commits in proposal order it cannot be reached: `report` proposes a result only from the current committed lease, so that result is in the log before any later grant of its operation. F2.6 and F4 do not catch it |
| `report` proposes twice per holding | `scheduler.rs` | F2.11 |
| `lease_committed` starts a superseded grant | `scheduler.rs` | F2.1 with a slow log (I2) |
| `not_held` names leases of other terms | `scheduler.rs` | F2.13 |
| `not_held` ignores which worker holds a lease | `scheduler.rs` | F2.13 (N) |
| the daemon model's W check removed (the test of the sim itself) | the worker node | F2.4 (I12) |

### F3. Platform routing, cordon, drain and rollouts together

**Harness.** Two parts. In `kbf-sched`, one in-process world that joins `sim_platform`'s
platforms and outages with `sim_cordon`'s operator, as family file `f3_fleet.rs`. In
`kbf-server`, a new `tests/sim_rollout.rs` that runs the real `RolloutDriver` and
`MemoryRolloutStore` over a `Fleet` implemented on a `kbf_sched::Scheduler` fed with
virtual time (the trait is public; `kbf-sim` becomes a dev-dependency of
`kbf-server`), with an `Applier` that records hand-overs and refuses some.

| # | Scenario | Generator | Checks beyond the invariants |
|---|---|---|---|
| F3.1 | The last worker of a platform cordoned | cordon the only arm64 worker, and the only Mac, for longer than the unservable wait | work for it waits with the cordon reason and is never refused (I10); other platforms run; it runs after the uncordon, placed by the uncordon itself, not the next tick |
| F3.2 | Cordoned and then silent, and the reverse | the last arm64 worker cordoned, then silent past G; or silent, then back cordoned | the wait becomes refusable when the worker stops being live, and counts from then, not from the cordon; the refusal comes the unservable wait after that |
| F3.3 | A worker dies while draining | drain, then silence past G | its leases are requeued for silence, not for the drain; the drain then reads `Drained`; the rollout driver keeps the node at `draining` while it is disconnected, and hands over the update once it is back, drained |
| F3.4 | A paused drain whose leases end later | a deadline shorter than the leases | it stays `Paused`; a new drain with a new deadline goes to `Drained` at once |
| F3.5 | A rollout over a mixed fleet, `max_unavailable` 1 to 3 | a rollout over 4 to 12 nodes with running work, driver steps at random times | at every step, at most `max_unavailable` nodes out of service by the record, and no more cordoned by the driver; each node goes `cordoned -> draining -> updating` in order; an `updating` node held no lease when handed over |
| F3.6 | An operator acts during a rollout | uncordon a draining node; cordon another; drain one the rollout has not reached | the driver holds the rollout on the uncordoned node and nothing moves after; the operator's own cordon does not count toward `max_unavailable` (the record counts its own nodes) |
| F3.7 | A node reboots during its update | a cordoned node registers again | it comes back cordoned (a cordon names the worker, not the session) and gets no grant |
| F3.8 | Capabilities change while cordoned | a `Capacity` with new caps (an Xcode added) during a drain | after the uncordon, matching uses the new caps |
| F3.9 | A refused hand-over or a failing store | the `Applier` refuses; the store fails a write | the node and the rollout are held; a failed write leaves the node untouched |

Planned, not simulated: the last-of-class pre-gate with `min_serving` and
`accept_outage` (today a rollout may cordon the last node of a class, and F3.1 checks
what happens then: its work waits and is not refused), per-pool slots shared across
rollouts (today two rollouts over one pool may each take `max_unavailable`), canary
and soak, and the steps after `updating`.

**Mutants F3 must catch:**

| Mutant | Where | Caught by |
|---|---|---|
| `Servable::new` ignores cordons | `servable.rs` | F3.1 (I8) |
| a `Cordoned` verdict counts toward refusal | `scheduler.rs` (`note`) | F3.1 (I10) |
| the wait keeps its start across a cordon (`since` kept from cordoned to unservable) | `scheduler.rs` (`note`) | F3.2 (I10, L2) |
| `progress` moves `Paused` to `Drained` when leases end | `cordon.rs` | F3.4 (I9) |
| a drain requeues the worker's leases | `scheduler.rs` | F3.4, F3.5 (I9) |
| `uncordon` does not place at once | `scheduler.rs` | F3.1 |
| the matching memo is keyed by the first request of the round only | `servable.rs` | F3.8, F4 (I7) |
| `Rollout::advance` counts with `>` instead of `>=` (one node too many out) | `kbf-types/src/rollout.rs` | F3.5 |
| the driver acts on the node before recording the step | `kbf-server/src/rollout.rs` | F3.9 |
| the driver hands over an update without reading the placement again | `kbf-server/src/rollout.rs` | F3.6, with the operator's uncordon injected between the driver's two reads |

### F4. Large randomized fleets

**Harness.** The F1 and F3 in-process world with every event kind on, scaled up, as
family file `f4_fleet.rs`, plus a smaller cell version on the kernel (F2's nodes,
20 workers) for network faults. The checker runs after every input; it keeps its
shadow state incrementally so a step costs what changed, not the fleet's size.

Base world: 200 workers drawn from Linux x86-64 at levels v2 to v4, Linux arm64, Macs
with one or two Xcode builds, and GPU nodes; arrivals with random sizes, platforms
(some no worker satisfies), QoS and dedup keys; worker deaths, returns, reboots,
`Capacity` changes; operator cordons, drains and uncordons; per seed, the swarm
switches each fault kind on or off.

| # | Scenario | Generator | Checks |
|---|---|---|---|
| F4.1 | Steady state | arrivals at 70 to 95 percent of capacity | I1 to I15 every step; L1 once arrivals stop |
| F4.2 | Churn | 5 percent of workers die or return each minute | as F4.1; every unservable refusal checked by the reference verdict (I10) |
| F4.3 | Mass reconnect | every worker re-registers within a few seconds | L3; each lost lease requeued once; no operation held twice (I3) |
| F4.4 | Operator storm | cordon, drain and uncordon at random on a tenth of the fleet | I8, I9, I10 |

CI runs a bounded sweep (16 seeds of 2,000 operations, inside the file budget); an
ignored test runs 1,000 seeds of 10,000 operations in a release build. F4 is the net:
each PR that adds it reports which of the F1 to F3 mutants it catches on its own.

## 6. Order of work

1. This catalog.
2. The shared checker and the in-process world (`tests/sim/`), with `sim_platform`
   and `sim_cordon` moved onto them unchanged in what they check, then F1.
3. F3 in `kbf-sched`, then `sim_rollout` in `kbf-server`.
4. The cell nodes moved out of `sim_cell`, the daemon model completed, then F2 (F2.9
   ignored on issue #137 until it was fixed).
5. F4.

Each PR names the mutants it planted, the scenario that went red for each, and the
seed and command that show it.

## 7. Questions for review

- **A wait bound under load.** Should a large request, or `batch` work, have a bound on
  how long smaller or more urgent arrivals can pass it before scoring and fair turns
  land? Today neither has one (F1.5, F1.6).
- **`max_unavailable` per pool or per rollout.** The record counts a rollout's own
  nodes; section 4.1 of fleet-updates.md says per pool, shared by every rollout. The
  per-pool slot is phase P2; until then F3.5 checks the per-rollout count.
