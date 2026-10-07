# kbf architecture

This document describes how kbf is put together: what each part does, how a request
moves through the farm, where state lives, and how the code is tested. It describes
the code on `main`. Parts that are designed but not built yet are marked
**planned**; nothing marked planned exists in the code today.

The design documents under [docs/design](docs/design) go deeper:

| Document | Covers |
|---|---|
| [scheduler.md](docs/design/scheduler.md) | operations, leases, fencing, QoS, placement, accounting |
| [storage.md](docs/design/storage.md) | the CAS and action cache, segments, metadata, retention, the object store interface |
| [worker-protocol.md](docs/design/worker-protocol.md) | `kbf.worker.v1`: messages and the rules both sides keep |
| [daemon.md](docs/design/daemon.md) | `kbf-daemon`, the container driver, output collection, limits |
| [capabilities.md](docs/design/capabilities.md) | node reports, ISA levels, matching an action to a node |
| [mac-node-provisioning.md](docs/design/mac-node-provisioning.md) | a Mac as a worker: baseline, provisioning, the signed daemon artifact, headless settings, updates, join and leave (proposed) |

Decision records live in [docs/adr](docs/adr).

## What kbf is

kbf is a remote build and test farm for [Bazel](https://bazel.build) and
[Buck2](https://buck2.build). Build tools talk to it with the
[Remote Execution API v2](https://github.com/bazelbuild/remote-apis) (REAPI): they
upload inputs to a content-addressable store (CAS), look up results in an action
cache (AC), and ask the farm to execute actions that miss. kbf runs those actions on
worker machines and stores the results so the next build, on any machine, can reuse
them.

There is no client-side kbf component. Bazel and Buck2 speak REAPI to `kbf-server`
directly.

## The two programs

kbf ships two binaries.

**`kbf-server`** (crate `kbf-server`) serves REAPI to build clients and
`kbf.worker.v1` to daemons, on two separate listeners. It holds the cache, the
scheduler and the record of which daemons are connected. Today one process runs every
role (`--role=all`). The flags are:

- `--listen` (REAPI, default `127.0.0.1:8980`) and `--worker-listen` (daemons, default
  `127.0.0.1:8981`);
- `--worker-tls-cert`, `--worker-tls-key`, `--worker-client-ca` to serve the worker
  listener over mutual TLS (all three, or none for plain text);
- `--store=memory` or `--store=s3` with `--s3-endpoint`, `--s3-bucket`, `--s3-region`,
  `--s3-prefix` and `--s3-conditional-put`; the S3 key pair comes from the standard
  `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` variables, never from the command line;
- `--heartbeat-interval-ms` (default 5000), the interval daemons are asked to
  heartbeat at.

**`kbf-daemon`** (crate `kbf-daemon`) runs on each worker machine. It opens one
outbound mutual-TLS stream to a server, reports what the machine is, heartbeats, and
runs the leases the server starts. It never listens on a port. Execution goes through
a `Runtime` trait; the container driver (crate `kbf-driver-container`) implements it
with rootless Podman. The `kbf-daemon` binary itself offers only a fake runtime today,
because the driver crate depends on the daemon crate and Cargo refuses the cycle a
binary naming it would create; wiring the driver into a shipped binary is **planned**.

## Crates

kbf is one Cargo workspace. Some crates are **pure**: they hold decision logic, do
no I/O, read no clock and draw no random numbers. Time and entropy are inputs. This is
what lets a simulation seed replay a run exactly (see [Testing](#testing)).

| Crate | Pure | What it holds |
|---|---|---|
| `kbf-types` | yes | digests, platforms, QoS levels, lease ids, farm time, the `StateMachine` trait and `Effect` enum |
| `kbf-sched` | yes | the scheduler state machine and the fence constants |
| `kbf-meta` | yes | the metadata state machine: CAS index, action cache, closure check, retention |
| `kbf-segments` | yes | the segment format and FastCDC chunking |
| `kbf-caps` | yes | CPU capability parsing, ISA levels, request matching |
| `kbf-raft` | yes | a sans-IO Raft core ([ADR 0001](docs/adr/0001-consensus.md)) |
| `kbf-estimator` | yes | placeholder for learned action sizes (**planned**) |
| `kbf-objstore` | no | the `ObjectStore` trait, an in-memory store, an S3 store, the conformance suite |
| `kbf-front` | no | the REAPI services over a `Cache` and a `Dispatch` |
| `kbf-server` | no | the server binary: wires front, scheduler and storage together |
| `kbf-proto` | no | generated code for REAPI and `kbf.worker.v1` |
| `kbf-daemon` | no | the daemon: session loop, lease manager, CAS client, input and output trees |
| `kbf-driver-container` | no | the rootless Podman execution driver |
| `kbf-sim` | no | the deterministic simulation kernel |
| `kbf-it` | no | integration tests, repository lints, the end-to-end harness |
| `kbf-coverage` | no | the coverage ratchet CI runs |
| `kbf-alert`, `kbf-store`, `kbf-sim-cell` | no | placeholders (**planned**: alerting, storage engine, whole-cell simulation) |

A test in `kbf-it` reads the dependency graph and fails if a pure crate depends,
directly or not, on an async runtime, a network crate or a random source, or if its
`lib.rs` stops denying the items listed in [clippy.toml](clippy.toml) (clocks, sleep,
sockets, hashed collections).

## Request paths

### Cache traffic

`Capabilities`, `ContentAddressableStorage`, `ByteStream` and `ActionCache` are served
from one `Cache` (crate `kbf-front`). The cache keeps its index in a metadata state
machine and its bytes in an object store, packed into segments. The promises it keeps:

- **FindMissingBlobs never omits a digest.** A digest that does not parse fails the
  whole call. A blob whose stored copy cannot be read is reported missing, so the
  client uploads it again.
- **Present means durable.** An upload is acknowledged, and a blob reported present,
  only once its bytes are in the object store and its location is committed to the
  index.
- **Every byte is verified,** against its digest, on upload and on read.
- **A hit never points at missing files.** `GetActionResult` answers through the
  closure check: the result and every blob it names must be present and readable,
  or the lookup is a miss.
- **Clients never write the action cache.** `UpdateActionResult` is always
  `PERMISSION_DENIED`; results reach the cache only from a daemon that ran the action.

See [storage.md](docs/design/storage.md).

### Executing an action

```
client                      kbf-server                               kbf-daemon
  | Execute                    |                                         |
  |--------------------------->| 1. AC lookup (closure check): hit -> done
  |                            | 2. Action, Command, input tree in CAS?  |
  |                            |    no -> FAILED_PRECONDITION (MISSING)  |
  |                            | 3. submit: join a running twin, or queue|
  |<-- QUEUED -----------------| 4. place on a worker, propose the lease |
  |                            |--- LeaseOffer ------------------------->| (nothing runs)
  |                            | 5. lease committed                      |
  |<-- EXECUTING --------------|--- Start ------------------------------>| run the lease
  |                            |                                         | fetch inputs (ByteStream)
  |                            |                                         | run, upload outputs
  |                            |<-- Result ------------------------------|
  |                            | 6. holder check, outputs all stored?    |
  |                            | 7. accept, write the AC entry           |
  |                            |--- ResultAck -------------------------->|
  |<-- done (ExecuteResponse) -|                                         |
```

1. **Action cache.** Unless the client set `skip_cache_lookup`, a hit that passes the
   closure check answers at once with `cached_result` set. Nothing runs.
2. **Inputs.** The `Action`, its `Command` and every blob of its input tree must be in
   the CAS; each one that is not becomes a `MISSING` violation in a single
   `FAILED_PRECONDITION`, as REAPI asks.
3. **Submit.** The front hands the request to the scheduler through the `Dispatch`
   trait. If an identical action (same instance name and action digest) is already
   in flight and both may be joined, the caller is attached to it (in-flight dedup).
4. **Place.** The scheduler grants a lease on a worker with room and asks for the grant
   to be committed. The daemon is told with a `LeaseOffer`, on which it runs nothing.
5. **Start.** Only once the grant is committed does the daemon get `Start`.
6. **Result.** The server accepts a result only from the daemon holding the
   operation's current lease, and only if every output it names is already stored.
7. **Answer.** An accepted successful result (exit code 0, not `do_not_cache`) is
   written to the action cache *before* the callers are answered.

`WaitExecution` streams the same stages for an operation name that Execute returned.
A finished operation is forgotten; the next Execute for it is answered from the
action cache.

See [scheduler.md](docs/design/scheduler.md) and
[worker-protocol.md](docs/design/worker-protocol.md).

## Where state lives

| State | Today | Planned |
|---|---|---|
| CAS index, action cache, farm time | `MetaState` behind `MemoryMetaLog`, in the server's memory | the same state machine replicated by Raft |
| Blob bytes | segments in an object store (in memory, or an S3 bucket) | the same, with garbage collection and multiple stores |
| Leases, operations, workers | `Scheduler` in the server's memory; a control record "commits" when appended | the same state machine fed from a replicated control log |
| Node reports | read at `Hello`; only `cpus` and `mem_gib` are used | full capability matching |

Because the index is in memory today, a restarted server forgets every blob. With
`--store=s3` each start writes under a fresh key prefix so it never reads objects a
previous process wrote.

Every piece of state sits behind a trait or a pure state machine, so moving it into a
replicated log changes the implementation behind the seam, not its callers:

- `MetaLog` (in `kbf-front`): `commit` a metadata command, `query` the committed
  state. `MemoryMetaLog` is the single-process implementation; a replicated log
  implements the same two calls.
- `ObjectStore` (in `kbf-objstore`): five calls any S3-compatible store can serve.
- `Dispatch` (in `kbf-front`): how the REAPI front reaches the scheduler.
- `StateMachine` (in `kbf-types`): inputs in, effects out. The server carries out the
  scheduler's `Commit`, `Start` and `Answer` effects. The one place a control record
  is appended and fed straight back as committed is the step a replicated log
  replaces.

## One endpoint, many servers

Today kbf runs as one `kbf-server` process. The multi-server design (**planned**)
keeps one rule: a client sees one address, whatever number of servers stand behind it.

- **One name for the farm.** Bazel and Buck2 are configured with one remote address.
  Buck2 sends everything to that one address, so the farm must look like one
  endpoint. The deployment puts one virtual address (or one DNS name) in front of all
  servers; a plain layer-4 balancer is enough, because every server can answer every
  request.
- **Any server answers.** A server process holds no farm state of its own, only
  handles to shared state: the metadata state machine and the scheduler's control
  log, each replicated by Raft across a small set of voting servers. A server that is
  not a voter still serves requests; voters and serving servers are separate sets.
- **One scheduler.** The leader of the control log places every action, so one place
  sees all free room on all workers.
- **Bytes go to their owner.** The design under discussion assigns each server a share
  of digests (rendezvous hashing, so a server joining or leaving moves only its share);
  a server that receives a blob request forwards it to the owner in one hop, and the
  owner keeps hot blobs in memory and on local disk above the object store.
- **Locality comes from placement.** A balancer cannot see what a request is about, so
  kbf gets locality inside: daemons report which inputs they hold, and placement
  prefers a worker that already has an action's inputs.
- **Daemons need one address too.** A daemon dials the farm's name, registers, and can
  learn the current server list from committed state.

What already holds for this model in the single-node code: the seams above, lease ids
that carry the leader's term, a scheduler that refuses results from stale leases,
and a daemon protocol in which only the newest stream of a worker counts. See
[scheduler.md](docs/design/scheduler.md#more-than-one-server).

## Security model, today

- Daemons connect only over mutual TLS (`https://` URLs; the daemon refuses anything
  else). The server's worker listener serves mutual TLS when given a certificate, key
  and client CA.
- The REAPI listener has no TLS and no authentication yet (**planned**: TLS and
  bearer-token authentication, with the caller's identity deciding its role).
- Only the daemon path writes the action cache, and the metadata state machine itself
  refuses an action-cache write from any role but `Daemon`.
- Actions run without network (`--network=none`) in rootless containers, as described
  in [daemon.md](docs/design/daemon.md).

To report a vulnerability, see [SECURITY.md](SECURITY.md).

## Testing

The project's testing rules are in [CONTRIBUTING.md](CONTRIBUTING.md): every test has
been seen failing on a planted defect (a mutant), and the coverage ratchet in
[coverage-baseline](coverage-baseline) never lets the count of uncovered lines or
branches rise. How the code is tested:

- **Pure cores, unit-tested.** The scheduler, metadata, segment, capability and Raft
  crates are plain state machines with plain tests; each test says which defect it
  catches.
- **Deterministic simulation.** `kbf-sim` runs state machines on a virtual clock with
  a seeded random stream and a message bus that delays, drops, duplicates, reorders
  and partitions. The same seed gives the same trace hash on any machine, so a failing
  seed is a complete bug report. The scheduler runs there with a control log and
  workers that disconnect, reboot and restart (checks: every `Start` names a committed
  lease, every operation is answered once, no self-fenced operation runs twice). The
  Raft core runs there with crashes part-way through its effects (checks: election
  safety, log matching, leader completeness, state machine safety).
- **Contract suites.** Every object store backend must pass the conformance suite in
  `kbf-objstore`; CI runs it against the in-memory store and against MinIO and RustFS
  containers, with and without Object Lock.
- **Real services.** Server tests run the REAPI and worker services over real gRPC,
  including mutual TLS. The container driver's tests run real rootless Podman on
  hosted runners, on x86-64 and arm64.
- **End to end.** The `integration` workflow starts a cell (an S3 store, one
  `kbf-server`, one daemon) and builds sample projects with pinned Bazel and Buck2
  versions, remote-only, twice; the second build must be all remote cache hits. Its
  daemon uses a test-only runtime that runs actions as plain processes, so it proves
  the protocol and the cache path, not isolation.
- **Repository lints.** Workflows may only use hosted runners and pinned actions from
  an allowed set; public text may carry no machine addresses or home paths.
