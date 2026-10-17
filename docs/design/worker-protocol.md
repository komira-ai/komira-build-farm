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
  port, so a worker needs only outbound connectivity to the server's worker
  listener. With several servers (**planned**) that one stream goes to the leader
  ([deployment-topology.md](deployment-topology.md)).
- **Mutual TLS.** The daemon connects only to `https://` URLs and presents its own
  certificate; the server's worker listener verifies it against a client CA, and
  requires the certificate to name the node the stream speaks for (see
  [Node identity](#node-identity-and-the-deny-list)).
- **No blob bytes on this stream.** The session carries only small control messages.
  A daemon reads inputs and writes outputs with `ByteStream` calls to the same worker
  listener, over the same mutual TLS, each call admitted on its own (see
  [Blobs on the worker listener](#blobs-on-the-worker-listener)). A daemon needs no
  path to REAPI.
- **Versions.** The server accepts protocol versions N-1 and N. Version 1 is the first,
  so today it accepts exactly 1. Fields and messages may be added within a version as
  long as a peer that ignores them keeps working.

| Daemon sends (`DaemonMessage`) | Server sends (`ServerMessage`) |
|---|---|
| `Hello` | `Welcome` |
| `Heartbeat` | `HeartbeatAck` |
| `Offer` (not read yet) | `LeaseOffer` |
| `Result` | `Start` |
| `NodeStatus` | `ResultAck` |
| `MapRequest` | `Cancel` |
| | `MapUpdate` |

## A session

```
daemon                                  server
  | Hello (version, node_id, report) -->|  checks; registers the node (new session)
  |<---------- Welcome (interval, epoch)|  drops leases of another epoch
  | Result (each unacknowledged one) -->|  resent before the first Heartbeat
  | NodeStatus (OS, kernel, Xcodes) --->|  kept as the node's newest
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

The server checks the version, the node id, that the client certificate names that
node id and neither is denied ([below](#node-identity-and-the-deny-list)), the capacity
entries and the entries placement matches (`arch`; a repeated single-valued entry; a
countable entry that is not a whole number). If any fails, it ends the stream with an
error status (`FAILED_PRECONDITION` for an unaccepted version, `PERMISSION_DENIED` for
a certificate that does not name the node or a denied one, `UNAVAILABLE` while the
deny list cannot be read, `INVALID_ARGUMENT` otherwise). A stream that sends no `Hello` within 10 seconds ends
`DEADLINE_EXCEEDED`. On success it answers `Welcome` with the version it will speak,
the heartbeat interval in milliseconds, and its **lease epoch** (see
[Server restarts and the lease epoch](#server-restarts-and-the-lease-epoch)).

**Only the first `Hello` of a stream registers the node.** It opens a new session, and
from then on every `Start` for this node goes to this stream. A `Hello` resent on the
same stream (the daemon's node report changed) must carry the stream's `node_id`: one
that names another node ends the stream `PERMISSION_DENIED`, and so does one the deny
list now refuses. Otherwise it changes the node's capacity and capabilities and nothing
else; a resent report that fails the other checks is ignored. A newer stream from the
same node replaces the older one: messages still arriving on the old stream are ignored
from then on.

`Hello.instance_id` names the daemon process: the daemon draws 128 random bits when it
starts and sends them, as 32 hex digits, in every `Hello` on every stream. A restarted
daemon, a second daemon started with the same certificate and a daemon on a cloned
machine each draw their own. The scheduler uses it to decide which leases a new
session's first heartbeat may give up at once (invariants 6 and 7
[below](#invariants)), and the server logs a warning whenever a node's stream comes
from another process than its earlier stream's. An empty `instance_id` (a daemon that
predates the field) matches no process, so every registration it makes counts as
another process's.

`Hello.predecessor_instance_id` (field 8, issue #22) names the daemon process that ran
on the node before this one, whose runs this one's drivers killed before its `Hello`.
The daemon binary writes its own id to `<--data-dir>/leases/daemon-instance` once every runtime
is built (each driver sweeps the scratch directory as it is built), and reads the id
there before it writes its own; empty on a first start, or without `--data-dir`. The
server takes the claim only when it names the process the node's previous stream came
from and that stream has closed, and then gives up at once the leases whose `Start`
went to that process and which the new session's first heartbeat leaves out, as farm
faults ([scheduler.md](scheduler.md#retries-of-farm-faults)). Otherwise it keeps the
handover grace (invariant 7). A server that predates the field ignores it.

The daemon refuses a `Welcome` whose interval is zero or whose double is not shorter
than its fence time T, since a gap of two intervals must be noticed well before it
fences.

### The storage map: `Welcome.map`, `MapUpdate`, `MapRequest`

A daemon knows the frontend's name and nothing else: it finds the storage nodes in the
storage map ([architecture-v2](architecture-v2.md), section 6), which the frontend sends.

- `Welcome.map` (field 4) is the map the frontend holds when the stream opens. It is
  absent from a server that holds none (a farm of one member), and the daemon then routes
  by none.
- `MapUpdate { map }` is sent on the stream each time the frontend's map changes, whole,
  with the newest map at the time it is sent. A change that follows another quickly may
  be sent as the later one alone.
- `MapRequest { at_least }` (daemon to server) asks for the map. The daemon sends it when
  it waits for a generation it does not have: a storage node answered `STALE_MAP`
  (architecture-v2, section 6) naming a generation newer than the daemon's. The server
  answers with a `MapUpdate` of the map it holds now, which may be older than `at_least`
  when this frontend has not seen that map yet; the daemon goes on waiting and a later
  `MapUpdate` reaches it. A server that holds no map does not answer. `at_least` is for
  the log; the answer does not depend on it.

The daemon keeps the newest map it was sent, whichever message brought it. A map of the
same or an older generation changes nothing, so a push that arrives late, twice or out of
order is harmless, and so is one that was never sent: the next `Welcome`, `MapUpdate` or
answer to a `MapRequest` carries the map. A map the daemon cannot read (an enum value
this build does not name, a generation of 0, a repeated member) is logged and not taken;
the session goes on.

### Node phases

`Hello.phase` (`NodePhase { phase, lease_id, reason }`) says what the node is doing
besides running leases (`docs/design/fleet-updates.md` section 3.3). It is absent, or
`PHASE_SERVING`, on a node that takes leases, and the daemon resends `Hello` whenever
it changes. The whole-machine runtime on a Mac
(`kbf_driver_native::whole_machine`) sets the session phases and the quarantine; the
daemon takes them through `Daemon::with_phase`. The daemon itself says
`PHASE_MEMORY_PRESSURE` (`Daemon::with_memory_backstop`) when no runtime phase is
in force:

| Phase | Meaning | `lease_id` / `reason` |
|---|---|---|
| `PHASE_PREPARING` | switching the Mac's GUI session to the user of a whole-machine lease it holds (`session-login`: auto-login, then a userspace restart, which ends the daemon too), then running its action there | the lease |
| `PHASE_RESTORING` | switching back after that lease's action ended (`session-idle`, the same restart), and cleaning up; the lease's `Result` is sent once that is done. A daemon restarted while the action ran (a crash or an update, not a switch) says this too: it killed the lease user's processes before its `Hello`, and the lease's `Result` says it was lost (`LOST_DAEMON_RESTARTED`); the node never runs the action again itself, and the server runs it again as a farm fault | the lease |
| `PHASE_QUARANTINED` | a cleanup failed; the node takes no lease until an operator repairs it | why |
| `PHASE_MEMORY_PRESSURE` | the node holds a lease and its memory stall time (`/proc/pressure/memory`, the `some` line's `avg10`) is above the daemon's `--memory-pressure-pause-percent` (default 10); it takes no new lease until the pressure falls, and runs the ones it holds. Sent by any daemon on any driver, but only while a lease runs: with none the node serves, whatever the pressure, so a node paused by pressure that others cause is never left idle for good | |

The server takes the phase of every `Hello`, the stream's first included, before it
places any work. A node that is not serving is offered no lease, and is listed in
`GET /v1/nodes` with its `phase` (and a quarantine in `needs_attention`, logged once as
an attention item). While a node is silent in `PHASE_PREPARING` or `PHASE_RESTORING`,
the server keeps the lease the phase names for the session switch grace (10 minutes)
rather than the lease grace G ([scheduler.md](scheduler.md#node-phases)). When the
restarted daemon's first `Hello` names the lease, the server sends its `Start` again
on that stream (same lease id, same action), and the daemon's runtime takes the lease
up at the step it had reached; an action that had started is not run again (see
`PHASE_RESTORING`). A phase that names a lease of this server's term that the server
does not hold on the node is answered with a `Cancel` for it. A preparing or
restoring phase without a lease ends the stream `INVALID_ARGUMENT`; a phase value the
server does not know is read as a quarantine that says so.

On the daemon's side, a `Cancel` for a lease it is not running, and a `Welcome` whose
epoch is not the term of the lease its phase names, have every runtime kill that
lease: the whole-machine runtime, which keeps a lease across its own restarts, then
cleans up after it.

### Node identity and the deny list

Only the newest stream of a node counts, and that stream receives the node's leases,
with their inputs, and returns results the server writes to the action cache. So a
stream must not be able to speak for a node other than its own (issue
[#79](https://github.com/komira-ai/komira-build-farm/issues/79)).

**The binding rule.** Under mutual TLS the daemon's client certificate must carry
**exactly one DNS name in its subjectAltName extension**, and that name must equal the
`node_id` of every `Hello` on the stream, byte for byte, case included: issue the
certificate with the node id spelled exactly as the daemon's `--node-id` spells it. The subject's common name is not read; a certificate that
names no DNS name, or several, is refused. A certificate taken from one node can
therefore impersonate only that node. With OpenSSL, the client certificate's extension
is `subjectAltName = DNS:<node id>`.

In plain text (no TLS flags) there is no certificate and no binding. Mutual TLS is
optional: without the three TLS flags `kbf-server` serves the worker listener in
plain text on any `--worker-listen`, and off loopback its start line carries a
one-line warning (` warning=plaintext-off-loopback:worker`). A deployment whose
daemons run on other machines turns mutual TLS on in its config; today's daemon
dials only `https://` ([#412](https://github.com/komira-ai/komira-build-farm/issues/412)
tracks plain text). In plain text a resent `Hello` must still carry the stream's
`node_id`.

**The deny list** (`kbf-server --worker-deny-list FILE`, mutual TLS only) refuses
certificates and nodes. Each line is blank, a `#` comment, or one entry:

```
serial 0A:1B:2C          # a certificate serial, hex; colons, case and leading zeros ignored
spki-sha256 <64 hex>     # SHA-256 of the certificate's DER SubjectPublicKeyInfo
node mac-07              # a node id, whatever certificate it presents; a storage
                         # certificate with this DNS name too (see "Storage nodes")
```

`openssl x509 -noout -serial` prints a certificate's serial;
`openssl x509 -noout -pubkey | openssl pkey -pubin -outform DER | sha256sum` its
public key hash, which outlives a reissue with the same key. The server reads the file
at start (a bad file stops it), and looks at it **again at every check**: each first
`Hello` (reconnects included), each resent `Hello`, each `Heartbeat`, each `Result`,
each `NodeStatus` and each blob call of a daemon, and each `Register` and `Heartbeat`
of a storage node (below). A check reads the file's metadata (device,
inode, size, mode, owner, modification and change time) and reads and parses the file
again only when that changed since the last read. An entry added while a denied daemon is connected ends its stream
`PERMISSION_DENIED` at the next of those it sends (a `Result` is refused, so it never
reaches the action cache, and the stream ends without a `ResultAck`; a `NodeStatus`
is refused, so the operator API keeps the node's last status from before the entry),
and every reconnect is refused; no
restart is needed. The server reads nothing more from a stream it has ended. A blob
call is refused `PERMISSION_DENIED` from the first one after the entry is added, on a
connection already open too. Replace the file atomically (write a new file, then
rename it over the old one): an in-place edit that keeps the size and lands within
the filesystem's timestamp granularity may go unseen until the next change.

**Storage nodes.** The same file applies to the storage nodes that call
`kbf.map.v1.Membership` on this listener. A storage certificate is refused `Register`
and `Heartbeat` `PERMISSION_DENIED` when its serial or its public key is listed, or when
**any DNS name in it** is a listed `node`. So a `node` entry is not only a daemon's node
id: it also refuses a storage certificate that carries that DNS name, and where a host's
daemon and its storage half share a DNS name, `node mac-07` takes both off the farm. To
refuse one of the two, list its `serial` or `spki-sha256`. A storage certificate's IP
names are never matched against `node` entries; list such a certificate by serial or
key. A refused storage node's row lapses after the member TTL (`--member-ttl-secs`,
15 s), its member goes `down`, and after the dwell (`--member-dwell-secs`, 300 s) it
leaves the map and its blobs move to other owners.

A file that cannot be read or parsed after start refuses every check `UNAVAILABLE`
(fail closed) until it is fixed. That ends **every** connected daemon's stream within
one heartbeat interval, and refuses their reconnects, so it takes the whole farm off
line: their leases are placed again only after the grace period G, once the file is
readable and the daemons are back. It refuses every storage node's `Register` and
`Heartbeat` the same way. A node retries those with backoff, but one that cannot beat
within the member TTL lapses and goes `down`, and one kept out for longer than the
dwell leaves the map and starts re-replication of its slice. A typo in the file is
therefore an outage for storage as well as for daemons: fix it within the dwell, and
replace the file atomically so a half-written one is never read.

**Lifetimes bound what the list misses.** There is no CRL or OCSP: a certificate left
off the list verifies until it expires or the cell CA is replaced. Issue node
certificates with short lifetimes (30 days is a starting point) so a missed entry is
bounded.

### Blobs on the worker listener

Besides `Worker.Session`, the worker listener serves the REAPI `ByteStream` service
(`Read`, `Write`, `QueryWriteStatus`) and the `ContentAddressableStorage` service
(`FindMissingBlobs`, `BatchUpdateBlobs`, `BatchReadBlobs`, `GetTree`) over the farm's
one cache, so a blob a daemon writes there is the blob clients read on the REAPI
listener, and the other way round.
It is the daemon's only CAS: `kbf-daemon --cas` (and `kbf-cell daemon --cas`) must be
an `https://` URL, normally the same as `--server`, and the daemon dials it with its
own CA, certificate, key and server name. A plain `http://` URL is refused at start.

Each call of either service is admitted by one rule before anything of it is served
(a streamed request, such as a `Write`, before its first message is read):

- the listener serves mutual TLS; on a plain-text worker listener every call is
  refused `UNAUTHENTICATED`;
- the call's connection presented a client certificate the client CA verified (else
  the TLS handshake fails, or the call is `UNAUTHENTICATED`);
- the certificate carries exactly one DNS subjectAltName, which is the call's node
  (else `PERMISSION_DENIED`). A blob call names no node id, so there is nothing to
  compare the name with, as `Hello` has;
- neither its serial, its public key nor that node is on the deny list, looked at
  again for this call (`PERMISSION_DENIED`; a list that cannot be read or parsed
  refuses `UNAVAILABLE`).

The server logs each refused call at WARN with its code and reason. An admitted
call is served as the REAPI listener serves it (the chunking calls of
`ContentAddressableStorage` answer `UNIMPLEMENTED` on both). `Execution`,
`ActionCache` and `Capabilities` answer `UNIMPLEMENTED` on this listener. Today's
daemon CAS client calls only `ByteStream`; FindMissingBlobs and the batch calls are
there for the transfer engine planned in
[worker-cache.md](worker-cache.md#4-the-shared-transfer-engine).

The worker listener also serves `kbf.worker.v1.Segments` (`Begin`, `PutRange`,
`Commit`, `Locate`, `ReadRange`), each call admitted by the same rule: a daemon uploads
an action's outputs as whole segments with one commit, and reads blobs as ranges of
the objects holding them ([write-path.md](write-path.md#4-segment-upload-the-daemon-worker-listener)).
An upload belongs to the connection that began it: a call naming it on another
connection is `NOT_FOUND`.

Not checked: that the node is registered or connected, or that it reads only the
inputs of leases it holds. A certificate the deny list does not refuse can read any
blob whose digest it names, and write blobs.

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
| `lease_id` | `(term, seq)`; ordered by term first; no two server processes grant leases of one term |
| `kind` | the lease kind, the value of the platform property `kbf-lease`: `action` (default) or `whole_machine` |
| `action_digest` | the action to run; its inputs are fetched from the CAS; the lease's `Result` echoes it |
| `millicpus` | CPU booked for the lease, in thousandths of a CPU; 0 means not booked |
| `memory_bytes` | memory booked for the lease; 0 means not booked. A run after a memory kill books the raised amount ([scheduler.md](scheduler.md#memory-kills)) |
| `heartbeat_seq` | the newest heartbeat of this stream the server had taken when it sent the `Start`; 0 before the first, which names the stream's `Hello` |
| `valid_for_ms` | how long after sending that heartbeat (or `Hello`) the daemon may still act on the `Start`: 14 000; 0 means no bound |
| `device_id` | field 8: the iOS device booked for the lease, by the `id` its `ios.device` `Hello` entry names; empty when the action asked for none. Sent only to a daemon whose `Hello` lists that device ([ios-devices.md](ios-devices.md#54-booking)). Today's daemons ignore it (none reports a device) |

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
is not running reaches every runtime as a kill of a lease it does not run, which changes
nothing, except in the whole-machine runtime, which restores the node after a lease it
kept across a restart ([Node phases](#node-phases)).

### `Result` and `ResultAck`

`Result { lease_id, status, action_result, action_digest, memory_kill, lost }`
reports one lease.
`action_digest` echoes the `Start`'s (unset for a lease whose `Start` the daemon did
not keep, and from a daemon that predates the field). `status` is `OK` when
the action ran, whatever its exit code, and then `action_result` is set; otherwise it
says why the action could not run:

| Status | Meaning |
|---|---|
| `OK` | the action ran; its outputs, stdout and stderr are already in the CAS |
| `DEADLINE_EXCEEDED` | the action ran past its timeout and was stopped. `action_result` may hold the stdout and stderr digests of what it wrote before then (REAPI's partial result, already in the CAS); the server passes on those two digests whose blobs it holds, and nothing else, to the client in the `ExecuteResponse`, and never caches them |
| `INVALID_ARGUMENT` | the action cannot run as written (an image named by tag, an output that is also an input); the client's error |
| `FAILED_PRECONDITION` | an input blob is not in the CAS (with a `MISSING` violation), or no driver serves the lease kind |
| `ABORTED` | the lease was killed on the server's `Cancel` |
| `UNAVAILABLE` | contact was lost before the lease started; with `lost` set, the farm lost the run (below); with `memory_kill` node pressure, the node killed it for memory |
| `INTERNAL` | the farm failed (a kernel OOM kill, a lost container, a dirty node) |

`memory_kill` says which memory ran out when the lease was killed for memory. The
server reads it only on a `Result` whose status is not `OK`, and then in place of the
status code ([failure-classes.md](failure-classes.md), 6.1; the scheduler's rules are
in [scheduler.md](scheduler.md#memory-kills)):

| `memory_kill` | Meaning | What the server does |
|---|---|---|
| `MEMORY_KILL_UNSPECIFIED` (0) | not killed for memory, or not known | the status code decides, as above |
| `MEMORY_KILL_OWN_LIMIT` (1) | the action passed its own memory limit (the lease's cap) and was killed for it | runs it again one rung up the memory ladder (4, 16, 32 GiB, a whole host; `--memory-ladder-gib`) up to the largest node, and keeps the raised booking as its action key's floor; at the largest node, answers that the action needs more memory than any node offers |
| `MEMORY_KILL_NODE_PRESSURE` (2) | the node killed it while it was under its own limit: a node-wide out-of-memory kill, or the node's backstop | runs it again with the same booking (twice at most) and counts the kill against the node; never raises the booking |

The daemon sets `memory_kill` from its driver's error: the native driver's memory
watch sends `RESOURCE_EXHAUSTED` with `MEMORY_KILL_OWN_LIMIT`; the container driver
reads the lease cgroup's `memory.events` and sends `RESOURCE_EXHAUSTED` with
`MEMORY_KILL_OWN_LIMIT` for a kill at the lease's own cap, or `UNAVAILABLE` with
`MEMORY_KILL_NODE_PRESSURE` for a kill by the node's `actions/` limit or the host
([failure-classes.md](failure-classes.md), 6.1). Every other `Result` sends 0.

`lost` (field 6, issue #22) says the farm lost the run, and with it the result. The
server reads it, like `memory_kill`, only on a `Result` whose status is not `OK`, and
then in place of the status code; the daemon sends it with `UNAVAILABLE`, which a
server that predates the field takes as the farm's failure:

| `lost` | Meaning | What the server does |
|---|---|---|
| `LOST_UNSPECIFIED` (0) | not lost, or not known (a value this server does not know is taken so too) | the status code decides, as above |
| `LOST_DAEMON_RESTARTED` (1) | the daemon restarted while the action ran: the restarted daemon killed what was left of it before its `Hello` (the whole-machine runtime, whose lease record outlives the daemon) | runs it again as a farm fault, preferring another node ([scheduler.md](scheduler.md#retries-of-farm-faults)) |
| `LOST_FENCED` (2) | the daemon fenced the run: no heartbeat was acknowledged within T | the same |

The server accepts at most one `Result` per operation, only from the node holding
the operation's current lease, and only if the `Result` names no action other than the
one that lease runs. An `OK` result must have every output already in the
CAS (every output file, stdout and stderr, every output tree and the files it names);
otherwise the server treats it as an infrastructure failure, because accepting it would
hand callers files nobody can fetch. An accepted `OK` result with exit code 0 is written
to the action cache, unless the action is `do_not_cache`, **before** the operation's
callers are answered.

The server answers every `Result` that names a lease with `ResultAck { lease_id,
accepted }`. `accepted` is true when the `Result` became the operation's result, and
for a memory kill or lost run the scheduler took (the operation then runs again, or is
answered).
It is false when the lease is unknown (a lease of another
process is), held by another node, no longer the operation's current lease, when the
`Result` names another action, or when the operation already finished (a duplicate).
A copy of a memory kill already taken is refused: its lease is no longer current.
A refused result never reaches the action cache.

Until a daemon receives the `ResultAck` for a lease, it keeps the `Result`:

- every `Heartbeat` lists the lease in `running`, so the scheduler does not take the
  lease as lost while its result is on the way;
- every new stream resends it right after `Welcome`, before the first `Heartbeat`;
- a `Result` produced while disconnected goes out the same way.

Once acknowledged, accepted or not, the daemon forgets it.

### Server restarts and the lease epoch

A single-node server keeps its scheduler, and so its leases, in its process. When it
restarts, a daemon that reconnects within its fence time can still hold leases of the
old process: runs that go on until their fence, and results the old process never
acknowledged. Nothing in a lease id said which process granted it, and every process
numbered its leases from `(1, 0)`; so the new process could grant the same lease id
to the same node for another operation. The daemon then ignored the new `Start` (its
`Result` was still unacknowledged) or took it as a resend of the old run, and the
server took the old run's `Result` as the new operation's: its callers got another
action's result, and it was written to the action cache under their action's digest
(issue [#137](https://github.com/komira-ai/komira-build-farm/issues/137)). Three rules
close this, each on its own:

1. **A lease id names one lease, ever.** Each server process grants its leases under a
   term of its own, picked at start. Under the in-memory metadata log (tests only) it is the start time in
   milliseconds on the wall clock, times 2^16, plus 16 random bits. It orders after
   every earlier process's term while the wall clock does not step back across a
   restart, and differs from it unless a restart lands on an earlier start's very
   millisecond and the random bits match. Under the Raft metadata log it is the Raft term
   the log elects the server in at start, which is greater at every start.
   The server knows only the leases it granted, so a `Result` of an earlier process's
   lease is refused, and a lease of another term listed in a heartbeat is never
   cancelled (it may belong to a newer leader).
2. **The epoch in `Welcome`.** `Welcome.epoch` names the record of leases the server
   answers for. Under the in-memory metadata log (tests only) that is the process, so the server names
   its term; under the Raft metadata log it names the log id, the same at every start over
   one `--meta-dir`, and never 0. The daemon remembers the epoch
   of the newest `Welcome` with every lease it acts on. On a `Welcome` that names
   another epoch, before it resends anything or reads a `Start`, it kills the runs of
   the earlier epoch's leases (they are listed until they stop, like a cancelled run)
   and forgets their results without sending them: no server of the new epoch can
   accept them. A `Welcome` with epoch 0 (a server that predates the field) drops
   nothing, and a lease granted while no epoch was named is kept. Because the log id
   outlives leaders and their terms, a change of leader drops nothing: the daemon
   keeps its running leases and resends their unacknowledged results. **Planned:**
   the new leader knows the committed grants from the log and accepts them
   ([deployment-topology.md](deployment-topology.md#daemons-straight-to-the-servers-one-stream-to-the-leader)).
   **Today** the scheduler's state is not in the log, so after a restart under
   the Raft metadata log the old runs are orphans: rule 1 refuses their results and
   cancels none of them. Results are never doubled; runs may be. A client's new
   `Execute` of the same action can run beside its orphan, and the server, which
   counts the node's capacity as free, can book new work beside the orphans: at most
   the bookings of the leases the node held at the restart, for at most each orphan's
   action timeout (the driver's default when the action names none).
3. **A `Result` names its action.** The daemon echoes the `Start`'s `action_digest`
   in its `Result`, and the server refuses a `Result` whose `action_digest` is set and
   is not the action of the operation it granted the lease for, whatever the lease id
   says. A `Result` without one (an older daemon) is checked on the lease alone.

**Compatibility.** The fields are additions within version 1. An older daemon ignores
`epoch` and sends no `action_digest`: rule 1 alone keeps it safe, but it lets an old
epoch's run go on until it ends or fences, and resends an old epoch's result, which
the server refuses. An older server sends epoch 0 and ignores `action_digest`, and
grants every lease from term 1, so it keeps the bug: upgrade servers and daemons
together.

### `ResourceUsage`

A runtime may report what an action used as a `ResourceUsage` message (user and system
CPU time in microseconds, peak resident memory of any one process in bytes, wall time in
microseconds; zero means not measured). It travels inside the `ActionResult`, in
`execution_metadata.auxiliary_metadata`, as an `Any` with type URL
`type.googleapis.com/kbf.worker.v1.ResourceUsage`, so it reaches the server and the
action cache with the result.

### `NodeStatus`

What software the node runs, for operators: the OS name, version and build, the
kernel release (Linux), the `kbf-daemon` version, the ready Xcode builds (Mac), and
every installed Xcode with its state, why it is not ready and the command that fixes
it (`xcodes`, Mac; issue #164), and the container images the daemon was pinned to
and checked present at start (`container_images`, container driver: see
[daemon.md](daemon.md#the-container-driver)). It routes no work, so it is not part of the node report and does not change
`report_hash` (see [fleet-updates.md](fleet-updates.md) section 3.1). The daemon reads
`sw_vers` on a Mac and `os-release` and the kernel release on Linux; the Xcode builds
are the node report's `xcode` entries, from the driver that discovers them, and
`xcodes` comes from the same driver. A field it cannot read is empty.

The daemon sends `NodeStatus` on every stream after the resent `Result`s and before
the first `Heartbeat`. The server keeps the newest one per node, from the node's
current stream only (one from a replaced stream is ignored, and one the deny list now
refuses ends the stream, see [above](#node-identity-and-the-deny-list)), in memory, and lists it
in the operator API's `GET /v1/nodes` ([api.md](../api.md)). A server that predates
the message ignores it as an empty message; a daemon that predates it is listed
without software, and a server that predates `xcodes` or `container_images` ignores it.

When the driver's report changes mid-session (today: the native driver's first
survey of its Xcodes ended, which the daemon does not wait for before `Hello`, so its
first `Hello` advertises no Xcode and its first `NodeStatus` lists each as
`XCODE_STATE_NOT_SURVEYED`; or a later survey, every few minutes, found one became
ready or stopped being so), the daemon resends
its `Hello` on the stream if the node report changed (the server takes a resent
`Hello` as the node's new report, so placement sees the new `xcode` entries), and then
sends `NodeStatus` again. Re-detecting the other software mid-session is **planned**
(fleet-updates.md section 3.1, re-detection).

### `Offer`

`Offer { millicpus, memory_bytes }` is a placeholder for the capacity a daemon offers
after its protected floors. The server does not read it yet.

## Invariants

The protocol is built so that these hold under any interleaving of messages, reconnects,
daemon restarts and server restarts:

1. **Nothing runs without a committed lease.** Work starts only on `Start`, and `Start`
   is sent only after the grant commits.
2. **One result per operation.** The server accepts a result only from the current
   holder, at most once, and only for the action that holder's lease runs; the daemon
   never runs a lease again while its result is unacknowledged. A lease id names one
   lease across server restarts, so no result of an earlier server process can be
   taken for a lease the current one granted.
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
   Under mutual TLS, only a stream whose certificate names the node can become it.
6. **A daemon lists everything it holds in its first heartbeat on a new stream.** The
   scheduler requeues at once any lease whose `Start` went to an earlier session of the
   same daemon process (`Hello.instance_id`) and that the new session's heartbeat
   leaves out. (Re-adopting running work across a daemon restart is **planned**; today
   a restarted daemon lists nothing, and ends what its predecessor left running
   before its `Hello`: see [daemon.md](daemon.md#when-the-daemon-is-killed).)
7. **Only a daemon process speaks for its own leases.** Two processes may register as
   one node: a restarted daemon, or two daemons holding the node's certificate (a
   cloned machine, a second daemon started, a node replaced while the old one runs).
   A lease whose `Start` went to another process than the current session's is
   requeued only once `HANDOVER_GRACE` (T + 5 s) has passed since the scheduler last
   heard the node before that process was replaced: by then the replaced process,
   whose stream is no longer acknowledged, has fenced (issue #140). A process that
   was killed fences nothing: its runs are ended by each run's keeper as the process
   dies, and by the next daemon started on the node, before that one's `Hello` (issue
   #155); see [daemon.md](daemon.md#when-the-daemon-is-killed) for what neither
   reaches. The one exception is a process proven stopped by its successor
   (`Hello.predecessor_instance_id`, its stream closed): its leases go at once.

## Planned

- `Start` carries the lease's fence policy, so hermetic work runs on through a lost
  connection (`RUN_ON`) instead of self-fencing.
- A message for "started", so the scheduler can tell a running lease from one still
  being prepared.
- Prefetching an offered lease's inputs.
- `NodeStatus.devices`: every iOS device the node knows, with its state and fix, and
  the daemon's use of `Start.device_id` ([ios-devices.md](ios-devices.md#54-booking)).
- Drain and resource-change messages.
- With several servers ([deployment-topology.md](deployment-topology.md)): the daemon
  is given the servers as one DNS name with a record per server, or as a list of
  addresses, dials them directly, with no balancer in between, and holds its one
  stream to the leader. A follower does not serve or relay the session: it ends the
  `Session` stream with a status naming the leader, and the daemon dials that server.
  A follower that knows no leader answers `UNAVAILABLE`, and the daemon tries the next
  server. The daemon retries forever, with a backoff that grows to a bound. This part
  is built: `--server` is repeatable, each host is resolved again every round, and the
  wait doubles with jitter up to `--reconnect-max-ms` (see
  [daemon.md](daemon.md#reaching-a-server)); the redirect is planned. On
  failover the daemon reconnects to the new leader and keeps its running leases (the
  epoch names the log, above). Its blob calls go to the worker listeners of all the
  servers, with reads spread across them.
