# kbf architecture

This document describes how kbf is put together: what each part does, how a request
moves through the farm, where state lives, and how the code is tested. It describes
the code on `main`. Parts that are designed but not built yet are marked
**planned**; nothing marked planned exists in the code today.

The design documents under [docs/design](docs/design) go deeper:

| Document | Covers |
|---|---|
| [scheduler.md](docs/design/scheduler.md) | operations, leases, fencing, QoS, placement, accounting |
| [simulation.md](docs/design/simulation.md) | the scheduler's simulations: what exists, the invariants every step checks, the scenario families to build and the mutants each must catch |
| [storage.md](docs/design/storage.md) | the CAS and action cache, segments, metadata, retention, the object store interface |
| [worker-protocol.md](docs/design/worker-protocol.md) | `kbf.worker.v1`: messages and the rules both sides keep |
| [daemon.md](docs/design/daemon.md) | `kbf-daemon`, the container driver, output collection, limits |
| [capabilities.md](docs/design/capabilities.md) | node reports, ISA levels, matching an action to a node |
| [mac-node-provisioning.md](docs/design/mac-node-provisioning.md) | a Mac as a worker: baseline, provisioning, the signed daemon artifact, headless settings, updates, join and leave (**planned**) |
| [macos-vms.md](docs/design/macos-vms.md) | what runs on bare metal on a Mac and what in a macOS VM, VM sizing and scheduling, the VM driver, GPU tests (**planned**) |
| [macos-vm-guests.md](docs/design/macos-vm-guests.md) | a VM guest's first-boot setup, capture inside the guest, guest networking, image identity (recipe and content digests), the VM helper's uid and signing, the probes a real Mac must run (**planned**) |
| [fleet-updates.md](docs/design/fleet-updates.md), [fleet-updates-security.md](docs/design/fleet-updates-security.md) | keeping node software current: rolling updates, MDM on Macs, Linux host updates, bare-metal GPU and app-install isolation, the Fleet UI; its security model: threat model, root helpers, signing keys, the MDM gate, enrollment (**planned**) |
| [deployment-topology.md](docs/design/deployment-topology.md) | where servers run, the Raft log on local disk, the client front that routes to the leader, daemons dialling the servers directly, what is built and what is planned, the probes the front must pass (**planned**) |
| [mdm-backend.md](docs/design/mdm-backend.md) | MDM as a pluggable backend behind `kbf-mdm-gate`: the three operations the server uses, erase only by an operator's hardware-key-signed request, macOS 27 update progress, network reachability, moving the MDM, kbf's own configuration management (**planned**) |
| [egress-cache.md](docs/design/egress-cache.md) | pinned downloads served by the farm: fetch lists, the mirror store and its re-index, adopting client uploads, the one fetcher with egress, verification, failure states and alerts (**planned**) |

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
  listener over mutual TLS (all three, or none for plain text), and
  `--worker-deny-list` for the certificates and nodes it refuses;
- `--store=memory` or `--store=s3` with `--s3-endpoint`, `--s3-bucket`, `--s3-region`,
  `--s3-prefix` and `--s3-conditional-put`; the S3 key pair comes from the standard
  `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` variables, never from the command line;
- `--heartbeat-interval-ms` (default 5000), the interval daemons are asked to
  heartbeat at;
- `--finished-retention-secs` (default 60), how long a finished operation is kept
  for WaitExecution before its name is `NOT_FOUND`;
- `--shutdown-timeout-secs` (default 10), how long a stop (SIGINT or SIGTERM) waits
  for REAPI clients to disconnect after their open Execute and WaitExecution streams
  that are not done are ended `UNAVAILABLE` (a finished operation a stream is waiting
  on is still sent first);
- `--api-listen`, off unless given: the operator API, HTTP/JSON under `/v1`
  ([docs/api.md](docs/api.md)). Reads are open, so bind it where only operators
  reach it; writes need the token in `--api-token-file` (owner-only file), come from
  loopback, and carry no `Origin` header. Run `kbf-server` as a user other than
  `kbf-daemon`'s and its lease users', or builds on the same host can read the token.

**`kbf-daemon`** (crate `kbf-daemon`) runs on each worker machine. It opens one
outbound mutual-TLS stream to a server, reports what the machine is, heartbeats, and
runs the leases the server starts. It never listens on a port. Execution goes through
a `Runtime` trait, and the binary's `--driver` flag picks the implementation:
`container` runs each action in a fresh rootless Podman container (crate
`kbf-driver-container`, Linux only), `native` runs it as plain processes, for Macs
(crate `kbf-driver-native`), and `fake` runs nothing, for bring-up. The binary is built
from crate `kbf-node`, not from the `kbf-daemon` library crate, because the drivers
depend on that library and a binary in it that named them would be a cycle Cargo
refuses.

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
| `kbf-server` | no | the server binary: wires front, scheduler and storage together; its side of the MDM gate (`mdm`) |
| `kbf-proto` | no | generated code for REAPI and `kbf.worker.v1` |
| `kbf-mdm-api` | no | what `kbf-server` and `kbf-mdm-gate` share: generated `kbf.mdmgate.v1` (status, enforce, withdraw, profile; no erase), the names both check, Apple's catalogue parser and its at-most-daily reader |
| `kbf-daemon` | no | the daemon: session loop, lease manager, CAS client, input and output trees |
| `kbf-driver-container` | no | the rootless Podman execution driver |
| `kbf-driver-native` | no | the native execution driver: plain processes, for Macs |
| `kbf-node` | no | the `kbf-daemon` binary: flags, and the driver `--driver` names |
| `kbf-mdm` | no | `kbf-mdm-gate`, the only holder of the Mac MDM's API key: its verbs and caps over mutual TLS, operator-signed erase requests, the `MdmBackend` trait and its NanoHUB client ([mdm-backend.md](docs/design/mdm-backend.md)) |
| `kbf-updater` | no | the root helper that verifies and installs signed software sets on a node ([fleet-updates-security.md](docs/design/fleet-updates-security.md) S3, S4.1); Linux only for now |
| `kbf-mac-session` | no | the Mac's root helper that gives every lease its own throwaway user and admits an administrator only with the MDM gate's signed grant ([fleet-updates-security.md](docs/design/fleet-updates-security.md) S4.2, S4.3, S5.2); serves on macOS only |
| `kbf-guest` | no | the agent inside a macOS VM guest ([macos-vms.md](docs/design/macos-vms.md) section 6): a versioned protocol over a Unix socket, one command per boot behind a per-boot token, its process group killed on exit, `Kill` or timeout; the virtio socket and the VM driver that calls it are **planned** |
| `kbf-sim` | no | the deterministic simulation kernel |
| `kbf-it` | no | integration tests, repository lints, the end-to-end harness |
| `kbf-coverage` | no | the coverage ratchet CI runs |
| `kbf-alert` | no | alerts with their exact fix; the alert book (raise and resolve with hysteresis) and the outbox, both pure modules; the webhook notifier, which keeps the outbox in a file. Nothing raises alerts yet |
| `kbf-store`, `kbf-sim-cell` | no | placeholders (**planned**: storage engine, whole-cell simulation) |

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
A finished operation is kept for `--finished-retention-secs` (60 by default), in which
WaitExecution streams its done operation, so a client whose Execute stream broke still
gets a result the action cache does not keep; then it is forgotten, and the next
Execute for it is answered from the action cache. An operation name is `operations/{term}-{n}`: the server process's term
(the one its leases carry) and a count that starts at 0 in each process. A name from
an earlier process, or any other name, is `NOT_FOUND`, so a client still holding one
after a restart is never attached to another action's operation.

See [scheduler.md](docs/design/scheduler.md) and
[worker-protocol.md](docs/design/worker-protocol.md).

## Where state lives

| State | Today | Planned |
|---|---|---|
| CAS index, action cache, farm time | `MetaState` behind `MemoryMetaLog`, in the server's memory | the same state machine applied from a Raft log on local disk: one voter, then three |
| Blob bytes | segments in an object store (in memory, or an S3 bucket) | the same, with garbage collection and multiple stores |
| Leases, operations, workers | `Scheduler` in the server's memory; a control record "commits" when appended | the same state machine fed from a replicated control log, kept on the servers' local disks |
| Node reports | read at `Hello`: `cpus`, `mem_gib` and `gpu` are booked; `arch`, `cpu.features`, `os` and the other exact keys are matched against each action's platform; held in memory, so a restart forgets every node | operator labels, report changes noticed by hash; a durable node registry that remembers absent nodes, alerts on them and restores them on reconnect |

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

## One endpoint, a leader and its followers

Today kbf runs as one `kbf-server` process. The shape decided for more than one
server (**planned**) is in [deployment-topology.md](docs/design/deployment-topology.md),
with what exists today and the probes still to run. In short:

- **Servers on dedicated storage hosts,** each at a fixed address with a stable DNS
  name. Durable state is a Raft log on each server's local disk: one voter first,
  then three voters on three hosts for high availability (one leader, two followers).
  Elastic or stateless servers are not part of the design.
- **One name for the farm.** Bazel and Buck2 are configured with one remote address,
  and Buck2 sends everything to it. A tailnet ingress with a constant name and an
  automatic certificate terminates the clients' TLS; an HTTP/2 proxy behind it
  health-checks the servers and routes every request to the one that reports ready,
  the leader. `kbf-server` serves plain-text gRPC behind it (see
  [Security model](#security-model)).
- **Only the leader answers.** The leader of the control log serves REAPI and holds
  every worker stream, so one place sees all free room on all workers and places every
  action. A follower holds the replicated state, reports not ready, and takes over when
  it is elected.
- **Daemons dial the servers directly.** A daemon's worker stream does not go through
  the front: it dials the servers' DNS names over mutual TLS and holds one stream, to
  the leader. A follower answers a daemon with a redirect naming the leader. On
  failover the daemon reconnects to the new leader. A daemon's blob reads and writes
  go to the worker listener too (`--cas`), not through the front.
- **Locality comes from placement.** The front cannot see what a request is about,
  so kbf gets locality inside: daemons report which inputs they hold, and placement
  prefers a worker that already has an action's inputs.

What already holds for this shape in the single-server code: the seams above, lease
ids that carry the granting process's term, a scheduler that refuses results from
leases it does not hold, a lease epoch in `Welcome` that makes a daemon drop leases a
restarted server never granted, and a daemon protocol in which only the newest stream
of a worker counts. See [scheduler.md](docs/design/scheduler.md#more-than-one-server).

## Security model

Build clients and daemons reach `kbf-server` by different paths, and each path has its
own protection.

**Build clients go through a front.** TLS for Bazel and Buck2 ends at a front that
holds a real certificate for the farm's client-facing name: a load balancer, or a
proxy on a WireGuard mesh such as `tailscale serve` in its HTTPS mode. `kbf-server`
has no TLS of its own on the REAPI listener and none is planned. It serves REAPI as
plain-text gRPC behind the front, bound to loopback (front on the same host) or to
the mesh interface, whose traffic WireGuard already encrypts. With several servers
(**planned**), the front is a tailnet ingress followed by an HTTP/2 proxy that routes
only to the leader ([deployment-topology.md](docs/design/deployment-topology.md)); that
pair is not yet probed.

- **Today:** the REAPI listener (`--listen`, default `127.0.0.1:8980`) serves plain
  text, checks no credential and accepts any bind address. Whoever reaches the port
  can read action inputs and outputs, write the CAS and Execute actions.
- **Planned:** bearer-token authentication, checked by `kbf-server` itself behind the
  front; the front passes the `Authorization` header through and does not check it.
  The caller's identity decides its role. A peer address does not identify a caller
  here: a proxy on the same host connects from loopback, whoever its client is.
- **Planned:** a bind guard. `kbf-server` refuses a plain-text, unauthenticated REAPI
  bind that other machines could reach, and allows the front's hop: loopback, or an
  address the operator names as the front's.
- **A front that works: `tailscale serve` in HTTPS mode.** A probe with
  `tailscale serve` 1.102.4 ran its HTTPS mode (`tailscale serve --https=<port>
  http://127.0.0.1:<reapi>`) in front of a plain-text REAPI listener on loopback.
  That version has no `h2c://` backend scheme; the `http://` backend carried gRPC.
  Through it, over TLS, these all worked: GetCapabilities, FindMissingBlobs, a 64 MiB
  ByteStream write and read back with a matching sha256, BatchUpdateBlobs and
  BatchReadBlobs (up to 1 MiB each), Execute, and WaitExecution on a finished action.
  A Buck2 remote-only build got through capabilities, uploads and Execute (the probe's
  fake driver wrote no outputs, so the action itself failed on missing outputs).
  Configuring `tailscale serve` needs root or a Tailscale operator on the host.
  `kbf-server` sees every such connection as coming from loopback. The HTTPS mode can
  add Tailscale identity headers (such as `Tailscale-User-Login`; not tested by the
  probe); `kbf-server` does not read them.
- **Not supported for REAPI: `tailscale serve`'s TLS-terminated TCP mode**
  (`--tls-terminated-tcp`). Its certificate verifies, but it negotiates no ALPN, and
  gRPC clients require `h2`. Buck2 fails with "HTTP/2 was not negotiated"; gRPC's C
  core (for example Python `grpcio`) fails with "Cannot check peer: missing selected
  ALPN property".
- **Long silent streams.** Through the HTTPS front above, a queued Execute and a
  WaitExecution stream for an action no node could run carried no messages for 300 s;
  both then ended with `FAILED_PRECONDITION` when `--unservable-wait-secs=300` failed
  the action. That front did not cut them. Another front whose idle timeout is shorter
  than the longest queue wait would cut these streams.

**Daemons do not go through the front.** A daemon's `--server` (the flag's help calls
it "the kbf-server front") and its `--cas` both name the worker listener, not the
client front above. The worker listener keeps its own mutual TLS end to end. With
several servers (**planned**), daemons dial the servers' own names, with no balancer
in between, and a follower redirects them to the leader
([deployment-topology.md](docs/design/deployment-topology.md)).

- Daemons connect only over mutual TLS (`https://` URLs; the daemon refuses anything
  else). The server's worker listener serves mutual TLS when given a certificate, key
  and client CA. A daemon's certificate must name its node id as its one DNS
  subjectAltName, so a certificate can speak only for its own node, and a deny list
  (serials, public keys, node ids), looked at again at every `Hello`, `Heartbeat`, `Result`, `NodeStatus` and blob call, refuses
  leaked or retired certificates without a restart. There is no CRL or OCSP; short
  certificate lifetimes bound what the list misses. See
  [worker-protocol.md](docs/design/worker-protocol.md#node-identity-and-the-deny-list).
- **Built: blobs on the worker listener.** The worker listener also serves
  `ByteStream` (Read, Write, QueryWriteStatus) over the farm's cache. A daemon's real
  drivers, and the integration cell's daemon, read inputs and write outputs there:
  `--cas` must be an `https://` URL, normally the same as `--server`, and the daemon
  presents the same certificate as on its worker stream; plain `http://` is refused
  at start. Each blob call is admitted on its own: the certificate must carry exactly
  one DNS subjectAltName (the node), and neither the certificate's serial or public
  key nor that node may be on the deny list, which is looked at again for every call
  (its metadata at each call, the file read again only when that changed). A denied
  daemon is refused PERMISSION_DENIED from its next blob call, on a connection
  already open too. Nothing else of REAPI is served there (Execute, the action cache,
  `ContentAddressableStorage` and `Capabilities` answer UNIMPLEMENTED), and on a
  plain-text worker listener every blob call is refused UNAUTHENTICATED. So a daemon
  needs neither the front nor a REAPI token. See
  [worker-protocol.md](docs/design/worker-protocol.md#blobs-on-the-worker-listener).
- **Not built:** a check that a blob call's node is registered or connected, or that
  it reads only the inputs of its own leases; any certificate the deny list does not
  refuse can read every blob whose digest it names, and write blobs.

**Inside the farm:**

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
  versions, remote-only, twice; the second build must be all remote cache hits. A
  Buck2 action that sleeps then holds a lease in flight while the node is drained
  through the operator API, which must list that lease. Its
  daemon uses a test-only runtime that runs actions as plain processes, so it proves
  the protocol and the cache path, not isolation.
- **Repository lints.** Workflows may only use hosted runners and pinned actions from
  an allowed set; public text may carry no machine addresses or home paths.
