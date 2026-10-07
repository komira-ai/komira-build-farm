# The worker protocol: `kbf.worker.v1`

`kbf.worker.v1` is the protocol between `kbf-daemon` and `kbf-server`. Its definition
is [worker.proto](../../crates/kbf-proto/proto/kbf/worker/v1/worker.proto); this
document explains the rules both sides keep and why. The scheduler side is in
[scheduler.md](scheduler.md), the daemon side in [daemon.md](daemon.md).

## Shape

```
service Worker {
  rpc Session(stream DaemonMessage) returns (stream ServerMessage);
}
```

- **One stream per daemon, opened by the daemon.** No daemon listens on an inbound
  port, so a worker needs only outbound connectivity to the farm's address.
- **Mutual TLS.** The daemon connects only to `https://` URLs and presents its own
  certificate; the server's worker listener verifies it against a client CA.
- **No blob bytes on this stream.** A daemon reads inputs and writes outputs through the
  REAPI `ByteStream` service on separate connections. The session carries only small
  control messages.
- **Versions.** The server accepts protocol versions N-1 and N. Version 1 is the first,
  so today it accepts exactly 1. Fields and messages may be added within a version as
  long as a peer that ignores them keeps working.

| Daemon sends (`DaemonMessage`) | Server sends (`ServerMessage`) |
|---|---|
| `Hello` | `Welcome` |
| `Heartbeat` | `HeartbeatAck` |
| `Offer` (not read yet) | `LeaseOffer` |
| `Result` | `Start` |
| | `ResultAck` |
| | `Cancel` |

## A session

```
daemon                                  server
  | Hello (version, node_id, report) -->|  checks; registers the node (new session)
  |<----------------- Welcome (interval)|
  | Result (each unacknowledged one) -->|  resent before the first Heartbeat
  | Heartbeat (seq, hash, running) ---->|
  |<------------------ HeartbeatAck(seq)|
  |<------------------------ LeaseOffer |  placed, not committed: run nothing
  |<----------------------------- Start |  committed: run it
  | Result ---------------------------->|
  |<------------------------- ResultAck |
  | Heartbeat (lists a lease given up)->|
  |<------------------------- Cancel    |  not held here: stop it
```

### `Hello` and `Welcome`

`Hello` carries the protocol version, a `node_id` (stable, unique within the farm), the
daemon's version string, the **node report** and its hash.

The node report is a list of `Capability { key, value }` entries, sorted by key then
value so equal reports encode to equal bytes. A list-valued capability repeats its key
once per value. [capabilities.md](capabilities.md) lists the keys. The report must
carry exactly one `cpus` and one `mem_gib` entry, each a whole number: they are what
placement books against. It must also carry exactly one `arch` entry (`x86_64` or
`arm64`): placement matches each action's platform against the report (see
[capabilities.md](capabilities.md#matching)), and a node of unknown architecture could
not be matched safely. `report_hash` is the SHA-256 of the encoded entries in order.

The server checks the version, the node id, the capacity entries and the entries
placement matches (`arch`; a repeated single-valued entry; a countable entry that is
not a whole number). If any fails, it
ends the stream with an error status (`FAILED_PRECONDITION` for an unaccepted version,
`INVALID_ARGUMENT` otherwise). A stream that sends no `Hello` within 10 seconds ends
`DEADLINE_EXCEEDED`. On success it answers `Welcome` with the version it will speak and
the heartbeat interval in milliseconds.

**Only the first `Hello` of a stream registers the node.** It opens a new session, and
from then on every `Start` for this node goes to this stream. A `Hello` resent on the
same stream (the daemon's node report changed) changes the node's capacity and
capabilities and nothing else; a resent report that fails the checks is ignored. A newer stream from the same node replaces the older one: messages still arriving
on the old stream are ignored from then on.

The daemon refuses a `Welcome` whose interval is zero or whose double is not shorter
than its fence time T, since a gap of two intervals must be noticed well before it
fences.

### `Heartbeat` and `HeartbeatAck`

The daemon heartbeats at the interval `Welcome` named. A `Heartbeat` carries:

- `seq`, counting up from 1 on each stream;
- `report_hash`, equal to the newest `Hello`'s, so the server can notice a changed
  report without receiving it again (comparing it is **planned**);
- `running`: every lease the daemon holds, meaning leases running now **and** leases
  whose `Result` the server has not acknowledged yet.

The server acknowledges a heartbeat with `HeartbeatAck { seq }`, but only one from the
node's newest stream. A heartbeat from a replaced stream is dropped unacknowledged, so
a daemon still talking on an old stream fences on time.

The daemon fences on the **send** time of the newest acknowledged heartbeat (or of the
`Hello` a `Welcome` answered): it kills self-fenced work T = 40 s after it. Counting
from the send time, not the time the acknowledgement arrived, keeps the daemon's
deadline inside the scheduler's: the server heard the heartbeat no earlier than it was
sent. An acknowledgement also covers every earlier heartbeat of the stream.

### `LeaseOffer` and `Start`

`LeaseOffer { lease_id, kind, action_digest }` tells the daemon the scheduler has placed
a lease on it but not committed it yet. The daemon **must not run anything** on an
offer: running on an offer could run a lease the scheduler never commits, or run it
beside the node it is finally placed on. An offer that is never followed by a `Start`
lapses; a `Start` needs no earlier offer. Today the daemon only logs offers;
fetching inputs on an offer is **planned**.

`Start` is sent only after the lease is committed. It carries:

| Field | Meaning |
|---|---|
| `lease_id` | `(term, seq)`; ordered by term first |
| `kind` | the lease kind, the value of the platform property `kbf-lease`: `action` (default) or `whole_machine` |
| `action_digest` | the action to run; its inputs are fetched from the CAS |
| `millicpus` | CPU booked for the lease, in thousandths of a CPU; 0 means not booked |
| `memory_bytes` | memory booked for the lease; 0 means not booked |
| `heartbeat_seq` | the newest heartbeat of this stream the server had taken when it sent the `Start`; 0 before the first, which names the stream's `Hello` |
| `valid_for_ms` | how long after sending that heartbeat (or `Hello`) the daemon may still act on the `Start`: 14 000; 0 means no bound |

The daemon handles a `Start` as follows:

- a `Start` for a lease already running is a resend and changes nothing;
- a `Start` for a lease whose `Result` is still unacknowledged is ignored, because
  running it again could produce a second result;
- a `Start` that arrives `valid_for_ms` or more after the daemon sent the heartbeat it
  names, or that names a heartbeat this stream never sent, is **not run, not reported
  and not listed** (see below);
- if contact is already lost (past the fence deadline), it answers a `Result` with
  `UNAVAILABLE` instead of starting;
- if no runtime serves the lease kind, it answers `FAILED_PRECONDITION`;
- a `Start` without an action digest is answered `INVALID_ARGUMENT`.

**How late a `Start` may be acted on.** The scheduler gives up a lease that a worker
it still hears from leaves out of its running set once the `Start` has been out for G.
A `Start` delayed past that would run beside the operation's retry. The server sent
the `Start` after it took the heartbeat the `Start` names, and the daemon sent that
heartbeat before, so a `Start` the daemon receives within the window of that send was in
flight for less than the window. The daemon measures this on its own clock alone; the
two clocks need not agree, and the `Start` carries no timestamp. The window W = 14 s
keeps `W + T + 5 s < G`. A late `Start` is dropped silently: a `Result` would be taken
as the lease's outcome while the scheduler may still hold the lease, and the scheduler
gives the lease up anyway, as a `Start` that never arrived. The daemon forgets the send
times of heartbeats older than the newest acknowledged one: the server takes
heartbeats in order and acknowledges each after taking it, so no later `Start` names an
older one. To leave the window room, the server's heartbeat interval is at most 7 s.

### `Cancel`

`Cancel { lease_id }` tells the daemon that the server no longer holds this lease on it:
the scheduler gave it up, granted it elsewhere, or finished its operation, yet a
heartbeat listed it. The server sends one for each such lease on every heartbeat that
lists it, so a lost `Cancel` is sent again. Leases of another scheduler term are never
cancelled. The daemon kills the run without waiting for the kill to finish. The lease
stays listed until its run has stopped and its `Result` (normally `ABORTED`) is
acknowledged, and the server refuses that `Result`. A `Cancel` for a lease the daemon
is not running changes nothing.

### `Result` and `ResultAck`

`Result { lease_id, status, action_result }` reports one lease. `status` is `OK` when
the action ran, whatever its exit code, and then `action_result` is set; otherwise it
says why the action could not run:

| Status | Meaning |
|---|---|
| `OK` | the action ran; its outputs, stdout and stderr are already in the CAS |
| `DEADLINE_EXCEEDED` | the action ran past its timeout and was stopped |
| `INVALID_ARGUMENT` | the action cannot run as written (an image named by tag, an output that is also an input); the client's error |
| `FAILED_PRECONDITION` | an input blob is not in the CAS (with a `MISSING` violation), or no driver serves the lease kind |
| `ABORTED` | the lease was killed, or self-fenced when contact was lost |
| `UNAVAILABLE` | contact was lost before the lease started |
| `INTERNAL` | the farm failed (a kernel OOM kill, a lost container, a dirty node) |

The server accepts at most one `Result` per operation, and only from the node holding
the operation's current lease. An `OK` result must have every output already in the
CAS (every output file, stdout and stderr, every output tree and the files it names);
otherwise the server treats it as an infrastructure failure, because accepting it would
hand callers files nobody can fetch. An accepted `OK` result with exit code 0 is written
to the action cache, unless the action is `do_not_cache`, **before** the operation's
callers are answered.

The server answers every `Result` that names a lease with `ResultAck { lease_id,
accepted }`. `accepted` is false when the lease is unknown, held by another node, no
longer the operation's current lease, or when the operation already finished (a
duplicate). A refused result never reaches the action cache.

Until a daemon receives the `ResultAck` for a lease, it keeps the `Result`:

- every `Heartbeat` lists the lease in `running`, so the scheduler does not take the
  lease as lost while its result is on the way;
- every new stream resends it right after `Welcome`, before the first `Heartbeat`;
- a `Result` produced while disconnected goes out the same way.

Once acknowledged, accepted or not, the daemon forgets it.

### `ResourceUsage`

A runtime may report what an action used as a `ResourceUsage` message (user and system
CPU time in microseconds, peak resident memory of any one process in bytes, wall time in
microseconds; zero means not measured). It travels inside the `ActionResult`, in
`execution_metadata.auxiliary_metadata`, as an `Any` with type URL
`type.googleapis.com/kbf.worker.v1.ResourceUsage`, so it reaches the server and the
action cache with the result.

### `Offer`

`Offer { millicpus, memory_bytes }` is a placeholder for the capacity a daemon offers
after its protected floors. The server does not read it yet.

## Invariants

The protocol is built so that these hold under any interleaving of messages, reconnects,
daemon restarts and server restarts:

1. **Nothing runs without a committed lease.** Work starts only on `Start`, and `Start`
   is sent only after the grant commits.
2. **One result per operation.** The server accepts a result only from the current
   holder, at most once; the daemon never runs a lease again while its result is
   unacknowledged.
3. **No silent loss of a result.** A daemon keeps a result, lists it, and resends it
   until the server decides it.
4. **No two copies of self-fenced work.** A daemon stops self-fenced work T after its
   newest acknowledged send; the scheduler re-dispatches no sooner than G after it last
   heard the daemon, and `T + 5 s < G`. A daemon acts on a `Start` only within W of
   sending the heartbeat it names, and `W + T + 5 s < G`, so a late `Start` never runs
   beside the retry of its lease. A lease the scheduler no longer holds is cancelled on
   the next heartbeat that lists it.
5. **Only the newest stream counts.** A replaced stream's heartbeats are neither fed to
   the scheduler nor acknowledged, and every `Start` goes to the newest stream.
6. **A restarted daemon lists everything it runs in its first heartbeat.** The
   scheduler requeues at once any lease whose `Start` went to an earlier session and
   that the new session's heartbeat leaves out. (Re-adopting running work across a
   daemon restart is **planned**; today a restarted daemon runs nothing.)

## Planned

- `Start` carries the lease's fence policy, so hermetic work runs on through a lost
  connection (`RUN_ON`) instead of self-fencing.
- A message for "started", so the scheduler can tell a running lease from one still
  being prepared.
- Prefetching an offered lease's inputs.
- Drain and resource-change messages.
- With many servers: the daemon dials the farm's one address and may learn the current
  server list from the first server it reaches; the front relays the session to the
  scheduler's leader.
