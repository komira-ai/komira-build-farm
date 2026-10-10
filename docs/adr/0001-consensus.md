# ADR 0001: Consensus

Status: proposed; accepted when merged.

## Context

kbf keeps its authoritative state in replicated logs:

- the **control log**, whose state machine holds leases, nodes, jobs, runs, alerts
  and cloud instances;
- the **metadata group**, whose state machine holds the CAS index, the action cache,
  pinned references and farm time.

Both start in one Raft group, as two state machines with separate snapshots. Metadata
moves to its own group when it needs to, so the implementation must run more than one
group in a process. A deployment starts with a single voter, whose log is on its
server's local disk, and grows to 3 voters on 3 hosts for high availability
([deployment-topology.md](../design/deployment-topology.md)). Membership changes go
through a learner: add the new server as a learner, let it catch up, promote it, then
remove the old voter if one is being replaced.

Every decision kbf makes is tested by deterministic simulation (`kbf-sim`): one seed
names exactly one run, and a failing seed is a complete bug report. The consensus code
runs inside that simulator, so the deciding criterion is:

- **no hidden clock, random source or threads** inside the consensus code;
- **time and randomness are inputs** the caller supplies (the simulator's virtual clock
  and seeded stream; in production, the real ones);
- **message passing is ours**: the library hands messages to the caller and accepts
  them from it; it never opens a socket or spawns a task to deliver them.

Secondary criteria: maintained, passes our `cargo deny` policy (`deny.toml`), small
dependency tree, and an interface we could swap behind.

## Options

### 1. raft-rs (`raft` crate, from TiKV)

Source: <https://github.com/tikv/raft-rs>; docs: <https://docs.rs/raft/0.7.0/raft/>.

The interface is sans-IO and fits the criterion well. `RawNode::tick` advances logical
time, `RawNode::step` takes a message, and `RawNode::ready` returns the messages to
send, the entries to persist and the committed entries to apply. The caller owns
storage, transport and the clock.

It fails the criterion in one place: randomness.
`Raft::reset_randomized_election_timeout` draws the election timeout from the
thread-local, OS-seeded generator. In the latest release, 0.7.0, that is
`rand::thread_rng().gen_range(..)` in `src/raft.rs`
(<https://docs.rs/raft/0.7.0/src/raft/raft.rs.html>); on `master` it is `rand::rng()`.
`Raft::reset` calls it on every term change, so it runs during `step` and `tick`.
There is no way to inject the generator:

- `Config::validate` refuses `min_election_tick >= max_election_tick`, so the jitter
  cannot be switched off;
- the only override, `set_randomized_election_timeout`, is `#[doc(hidden)]` and
  documented "For testing leader lease". A caller would have to detect every internal
  reset and overwrite the value after each `step` and `tick`.

Two seeds that should replay identically would diverge on election timing, which is
exactly where Raft bugs live.

Packaging also fails our policy:

- 0.7.0 is the latest release, and `master` has moved on without a new one;
- 0.7.0 depends on `protobuf ^2` unconditionally (through `raft` and `raft-proto`),
  even with the `prost-codec` feature. The protobuf 2.x line carries a RustSec
  advisory (uncontrolled recursion when parsing; fixed only in protobuf 3.7.2,
  <https://rustsec.org/packages/protobuf.html>), which `cargo deny` refuses;
- it also brings `rand 0.8`, `slog` and `thiserror 1`, second copies of crates we
  already use;
- `deny.toml` refuses git dependencies, so taking `master` would mean vendoring a fork.

### 2. openraft

Source: <https://github.com/databendlabs/openraft>; docs: <https://docs.rs/openraft>.

Openraft is maintained, async, and pluggable: the `AsyncRuntime` trait supplies
`spawn`, `sleep`, `Instant` and `thread_rng`, and the `RaftNetwork` trait carries
messages.

Determinism, though, is a property of the whole async stack, not of the interface:

- in the latest stable release, 0.9.25, `Raft::new` spawns the core loop, a tick task
  and a state-machine worker
  (<https://github.com/databendlabs/openraft/blob/v0.9.25/openraft/src/raft/mod.rs>,
  `core/tick.rs`). The order in which they interleave is the executor's;
- it uses `tokio::sync` channels and mutexes directly;
- `tokio` and `rand` are unconditional dependencies, and the tokio runtime's
  `ThreadLocalRng` is `rand::rngs::ThreadRng`.

A replayable run would need a deterministic single-threaded executor and timer wheel
of our own under it, and a guarantee that no task ordering leaks in. The main branch
(0.10, alpha releases only) adds a `DeterministicRng` wrapper and abstract channels,
which narrows the gap but is not released.

### 3. A small implementation of our own, in `kbf-raft`

A sans-IO Raft core in the shape every kbf core already has: a `kbf_types::StateMachine`
whose inputs carry the farm time and the entropy for election jitter (`kbf_sim::NodeInput`
already delivers both), and whose outputs are messages to send, entries to persist and
entries to apply. Scope:

- leader election with PreVote and CheckQuorum;
- log replication and commit;
- learners and single-server membership changes;
- snapshots.

The specification is the Raft paper (<https://raft.github.io/raft.pdf>) and Ongaro's
dissertation (<https://github.com/ongardie/dissertation>). raft-rs and etcd's raft
show the proven decomposition (`tick`/`step`/`ready`).

## Decision

Option 3: kbf implements its own sans-IO Raft core in `kbf-raft`.

The deciding criterion rules out raft-rs as released (hidden election randomness, no
injection point) and openraft as designed (determinism lives in an executor we would
have to write). Taking raft-rs anyway would mean a vendored fork that rewrites its RNG
use and its protobuf layer, and maintaining that fork. A Raft core with a narrow scope
is a few thousand lines, and kbf's simulator is built to find its mistakes.

## Consequences

- **The consensus core has no clock, random source, threads or I/O.** Time arrives as
  `FarmTime` and randomness as input entropy, so a simulation seed replays it exactly.
  Log storage and snapshot files are a separate layer in `kbf-raft` that carries out
  the core's outputs.
- **We own correctness.** T7b wires the core into `kbf-sim`. It must check Raft's safety
  properties on every step across a seed sweep with partitions, drops, duplicates,
  reordering and restarts: election safety, log matching, leader completeness and state
  machine safety. Each check is shown red on a planted mutant:
  - acknowledge before commit;
  - grant two votes in one term;
  - commit an entry from an earlier term by counting replicas (the paper's Figure 8).
  Where they apply, test scenarios are ported from raft-rs and etcd's raft (both
  Apache-2.0, with attribution).
- **Narrow scope.** Membership changes go one server at a time (dissertation section
  4.1), not through joint consensus. Anything beyond the scope above needs its own
  decision.
- **Groups are values.** A process holds as many groups as it needs, so moving
  metadata to its own group is a deployment change, not a library change.
- **No new dependencies** for consensus.
- **Reversible.** The core's interface is the same shape as raft-rs's. If raft-rs
  releases an injectable random source and drops protobuf 2, or openraft ships a
  stable deterministic runtime, swapping stays inside `kbf-raft`.
