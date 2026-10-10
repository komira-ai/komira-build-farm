# Deployment topology

This document records the shape the maintainers decided for running kbf with more
than one `kbf-server`: where servers run, where their durable state lives, how build
clients reach them, and how daemons reach them. Most of it is **planned**. The
[Built and planned](#built-and-planned) section names exactly what exists in the code
on `main` today; nothing else in this document does.

Elastic or stateless servers (any number of interchangeable processes behind a
balancer) are out of scope. The farm runs a small, fixed set of servers. At high
availability (three servers) every server serves the read path and accepts upload
bytes, and one of them, the leader, does everything else: every metadata write,
`Execute` and `WaitExecution`, scheduling and the daemons' worker streams.

## The shape

```
 build clients (Bazel, Buck2)                     worker nodes (kbf-daemon)
          |                                                 |
          | REAPI over TLS, to one constant name            | kbf.worker.v1 over mutual TLS,
          v                                                 | straight to the servers' names
 +------------------------------+                           | (never through the front)
 | tailnet ingress              |                           |
 | constant name, automatic     |                           |
 | certificate; ends TLS        |                           |
 +------------------------------+                           |
          | plain HTTP/2                                    |
          v                                                 |
 +------------------------------+                           |
 | HTTP/2 proxy, routes by      |                           |
 | gRPC method:                 |                           |
 |  reads, uploads -> any server|                           |
 |    ready to serve reads      |                           |
 |  everything else -> leader   |                           |
 +------------------------------+                           |
          | plain gRPC                                      |
          v                                                 v
 +------------------+   +------------------+   +------------------+
 | storage host A   |   | storage host B   |   | storage host C   |
 | fixed address,   |   | (HA, later)      |   | (HA, later)      |
 | stable DNS name  |   |                  |   |                  |
 | kbf-server       |   | kbf-server       |   | kbf-server       |
 |   LEADER:        |   |   follower:      |   |   follower:      |
 |   reads, upload  |   |   reads, upload  |   |   reads, upload  |
 |   bytes; every   |   |   bytes; commits |   |   bytes; commits |
 |   metadata write,|   |   via leader;    |   |   via leader;    |
 |   Execute,       |   |   redirects      |   |   redirects      |
 |   worker streams |   |   daemons to A   |   |   daemons to A   |
 | Raft log on      |<->| Raft log on      |<->| Raft log on      |
 | local disk       |   | local disk       |   | local disk       |
 +------------------+   +------------------+   +------------------+
          |                      |                      |
          +----------------------+----------------------+
                                 |
                       object store (blob bytes)
```

Today only host A's role exists, in one process, with its state in memory (see
[Built and planned](#built-and-planned)). The first deployment (v0) is that single
server; the split between reads and the leader applies from three servers on.

### Servers on dedicated storage hosts

Each `kbf-server` runs on a dedicated storage host with a fixed address and a stable
DNS name. Worker nodes do not run servers. The set of servers changes only when an
operator changes it.

### Durable state: a Raft log on local disk

The control state (leases, operations, nodes) and the metadata state (the CAS index,
the action cache, farm time) are applied from a Raft log
([ADR 0001](../adr/0001-consensus.md)) kept on each server's local disk.

- **First, one voter.** One server, one voter: every entry commits once it is on that
  server's disk. A restart replays the log, so the server comes back with its index,
  its action cache and its record of nodes and leases.
- **Later, three voters on three hosts** for high availability: one leader and two
  followers. A follower holds the replicated state, serves the read path from it and
  accepts upload bytes ([below](#reads-and-uploads-on-every-server)); it serves no
  other client call and no daemon session, and it takes over when it is elected. The
  voter set grows from one to three through learners (add a learner, let it catch up,
  promote it), one server at a time.

Blob bytes stay in the object store, as today ([storage.md](storage.md)).

### Reads and uploads on every server

The heavy traffic of a build cache is blob bytes. So that it does not all land on one
host, at high availability it is shared across the three servers, and the leader
keeps only the small, ordered part, so the leader scales with the farm. (This was an
open question, followers and blob traffic; the maintainers decided it on 2026-10-10.)

| Served by | Calls |
|---|---|
| any server ready to serve reads | the read path: `ByteStream.Read`, `BatchReadBlobs`, `FindMissingBlobs`, `GetActionResult`; and upload bytes: `ByteStream.Write`, `BatchUpdateBlobs` |
| the leader only | every metadata write (blob and action-cache commits), `Execute`, `WaitExecution`, scheduling, the daemons' worker streams |

- **An upload on any server.** The server that receives the bytes verifies them,
  writes them to the object store as segments ([storage.md](storage.md#writes)), and
  then sends the leader the small metadata commit naming where they are. The upload
  succeeds once the leader has committed it. If no leader can be reached, the upload
  fails `UNAVAILABLE`; the bytes it wrote are unreferenced and are removed like any
  other orphan object.
- **Reads from the local replica.** A server answers a read from the metadata state it
  has applied from the log, and reads the bytes from the object store. A read that
  must record a touch ([storage.md](storage.md#retention-and-touches)) sends the touch
  to the leader as a commit before it answers, as an upload does; an entry needs one
  at most once per touch quantum (a day by default), so these commits are rare.
- **The staleness rule.** A follower may be behind the leader. It may answer a false
  "missing" (a blob committed on the leader that it has not applied yet); that is
  harmless, because the client uploads the blob again. It must never answer a false
  "present". Two things hold that:
  - object garbage collection's grace period (the delay before a condemned object is
    deleted) is far longer than any replication lag, so a blob a lagging follower
    still lists as present has bytes in the store for the whole time it can be behind;
  - where a stronger guarantee is needed, a follower first asks the leader for its
    commit index and waits until it has applied up to it (a read index), then answers.

### Build clients: one name, routed by method

Bazel and Buck2 are configured with one remote address, and Buck2 sends everything to
it, so the farm must look like one endpoint.

- **A tailnet ingress** with a constant DNS name and an automatically issued
  certificate (for example, Tailscale's Kubernetes operator ingress) terminates the
  clients' TLS.
- **An HTTP/2-capable proxy behind it** (for example, Envoy) health-checks every
  server and routes by gRPC method, the request's path: the read and upload calls
  [above](#reads-and-uploads-on-every-server) go to any server that is ready to serve
  reads, and every other call goes to the leader: `Execute`, `WaitExecution`,
  `GetCapabilities`, `GetTree`, `QueryWriteStatus`, and `UpdateActionResult` (which a
  client is refused). The proxy does not look inside a request.
- **Two readiness paths.** The proxy needs two answers from each server, so the
  operator API listener is planned to serve two readiness paths:
  - **ready to serve reads:** the server is not stopping, its store probe passes, and
    it is synced: it knows a current leader and has applied the log up to the commit
    index that leader last sent it. Any synced server, leader or follower, passes;
  - **leader:** `/readyz` as it is today, whose `leader` check passes only on the
    server that holds the leader role, for writes and `Execute`.

  The second path's name is not chosen. A server stops being ready to serve reads
  when it falls behind or loses touch with the leader.
- **`kbf-server` serves plain-text gRPC behind the front**, as the
  [Security model](../../ARCHITECTURE.md#security-model) already describes. TLS is the
  front's job.

### Daemons: straight to the servers, one stream to the leader

Daemons do not go through the client front.

- A daemon knows the servers' DNS names and dials them directly, over the worker
  listener's mutual TLS ([worker-protocol.md](worker-protocol.md)).
- It holds **one** worker stream, to the leader.
- A follower that receives a daemon's session does not serve it: it answers with a
  redirect naming the leader, and the daemon dials that server.
- **On failover** the stream to the old leader ends. The daemon reconnects, reaches
  the new leader (directly, or through a follower's redirect), and registers again.
  Lease ids carry the granting server's term, and the scheduler refuses results from
  leases it does not hold, so leases a stale leader granted cannot be confused with
  the new leader's (issues
  [#137](https://github.com/komira-ai/komira-build-farm/issues/137) and
  [#140](https://github.com/komira-ai/komira-build-farm/issues/140); see
  [scheduler.md](scheduler.md#leases)).
- While no stream is acknowledged, the daemon's fence clock runs as it does today:
  self-fenced work stops T = 40 s after the newest acknowledged heartbeat was sent. A
  failover that takes longer than that, from the daemon's point of view, stops its
  self-fenced work.

### A durable node registry

The record of nodes becomes durable state in the control log:

- a node that is absent (no stream) is remembered, not forgotten;
- an absent node raises an alert;
- an absent node gets no work;
- a node that reconnects recovers its place: its report, labels and placement state
  (cordoned, draining) are as they were.

## Built and planned

| Part | Today (on `main`) | Planned |
|---|---|---|
| Servers | one `kbf-server` process runs every role (`--role=all`) | one server per dedicated storage host; three for HA |
| Raft core | `kbf-raft`: a sans-IO core with election, replication, commit and learners; a single voter elects itself and commits alone (`crates/kbf-raft/tests/scripted.rs`); simulated with 3 voters and a learner. Snapshots, membership changes, PreVote and CheckQuorum are not built | the same core, with snapshots and single-server membership changes |
| Raft in the server | none: no crate depends on `kbf-raft`, and it has no disk storage | the log and its snapshots on each voter's local disk; `kbf-server` applies control and metadata state from it |
| Metadata | `MemoryMetaLog`, in the server's memory; lost at restart. With `--store=s3` each start writes under a fresh key prefix | applied from the log, so a restart keeps it |
| Leases | each process picks its own term at start (wall-clock milliseconds times 2^16 plus 16 random bits); `Welcome.epoch` names it; a daemon drops leases of another epoch; leases of an earlier process are refused (#137); another daemon process's leases are kept for the handover grace (#140) | the term comes from the Raft log (see [Open questions](#open-questions)) |
| Readiness | `GET /healthz` and `GET /readyz` on the operator API listener (`--api-listen`; [api.md](../api.md#get-healthz-and-get-readyz)). `/readyz` is 503 once a stop signal arrives, when a read-only store probe fails or times out, and when the server does not hold the scheduler role, a flag a single server always holds. There is one readiness path, and no `grpc.health.v1` service | two readiness paths: the Raft role sets the leader flag, so `/readyz` is 200 only on the leader; a second path, not yet named, is 200 on any synced server ([above](#build-clients-one-name-routed-by-method)) |
| Reads and uploads on followers | none: one server serves every call, and the cache commits its own metadata | at three servers, any synced server serves `ByteStream.Read`, `BatchReadBlobs`, `FindMissingBlobs` and `GetActionResult` from its applied state, and writes upload bytes to the object store before sending the leader the metadata commit; a read index where a stronger guarantee is needed ([above](#reads-and-uploads-on-every-server)) |
| Follower redirect | none: there are no followers, and `kbf.worker.v1` has no redirect | a follower answers a daemon's session with a redirect naming the leader |
| Daemon's servers | one `--server` URL; on a broken stream the daemon waits `--reconnect-ms` and dials the same URL again | the daemon is given the servers' names and follows a redirect to the leader |
| Client front | proven only with `tailscale serve` in its HTTPS mode, on the server's host, in front of a loopback REAPI listener (pull request [#246](https://github.com/komira-ai/komira-build-farm/pull/246); see the [Security model](../../ARCHITECTURE.md#security-model)); it routes by nothing, as there is one server | a tailnet ingress and an HTTP/2 proxy that routes by gRPC method, after the probes below pass |
| Node registry | in the server's memory: a node whose stream closed stays listed with `connected: false` until the server restarts, and gets no work once it has not been heard from for G; a restart forgets every node. `kbf-alert` exists, but nothing raises alerts yet | durable, as [above](#a-durable-node-registry) |
| Daemons' blobs | the drivers read and write blobs with `ByteStream` on the mutual-TLS worker listener (`--cas`, `https://` only), each call checked against the node certificate and the deny list; daemons need no path through the front ([worker-protocol.md](worker-protocol.md#blobs-on-the-worker-listener)) | the same, on the leader's worker listener |

## Probes before relying on the front

The ingress and proxy pair is not proven. Each of these must pass through the real
pair (ingress, then proxy, then a plain-text REAPI listener), as the `tailscale serve`
probe did for its front:

1. **A Buck2 remote-only build** gets through capabilities, uploads, Execute and
   results.
2. **A 64 MiB ByteStream round trip**: a write and a read back with a matching
   SHA-256.
3. **A 300 s silent stream**: a queued Execute and a WaitExecution that carry no
   message for 300 s, then end with the server's answer, not the front's cut.

**The proxy's stream idle timeout must exceed the longest silent stream.** An Execute
or WaitExecution stream sends a message only when its operation's stage changes;
`kbf-server` sends no progress messages and sets no HTTP/2 keepalive on the REAPI
listener. A stream can therefore stay silent for an action's whole queue wait (up to
the unservable wait, 300 s by default, for work no live node can run) or its whole
run (the container driver's default action timeout is one hour). The ingress's own
idle timeout bounds these streams the same way.

## Open questions

1. **What a failover keeps.** [worker-protocol.md](worker-protocol.md#server-restarts-and-the-lease-epoch)
   plans for `Welcome.epoch` to name the replicated log, which outlives leaders, so a
   change of leader drops nothing: a daemon keeps its running leases and resends
   their results to the new leader, which knows the committed grants from the log.
   The alternative is for the epoch to name the leader's term, so every failover
   drops every running lease, as a server restart does today. The first keeps work
   across a failover; the second is what is built. Not decided.
2. **How the daemon is told the servers.** A repeated `--server` flag, one DNS name
   with a record per server, or a list learned from committed state after the first
   connection. Not decided.
3. **The redirect's form.** A status on the ended `Session` stream carrying the
   leader's name, or a message within `kbf.worker.v1`. Either is an addition to
   version 1. A follower that knows no leader (an election in progress) has nothing
   to name; whether it answers `UNAVAILABLE` and the daemon tries the next name is
   not decided.
4. **Readiness on a follower.** `/readyz` is served on the operator API listener
   ([api.md](../api.md#get-healthz-and-get-readyz)) and fails its `leader` check
   when the server's leader flag is clear; nothing clears it yet. It must clear as
   soon as the server stops being the leader. Whether a `grpc.health.v1` service on
   the REAPI listener is also wanted is not decided.
5. **The proxy-to-server hop.** The [Security model](../../ARCHITECTURE.md#security-model)
   has `kbf-server` serve plain-text REAPI on loopback or on a mesh interface whose
   traffic is already encrypted, and plans a bind guard that allows only the front's
   hop. When the proxy runs on another machine than the storage hosts, that hop must
   travel over the mesh, or the guard and the model need another answer.
6. **Snapshots off the host.** With a single voter, losing that host's disk loses the
   log. Whether snapshots are also copied to the object store, so a replacement host
   can rebuild, is not decided.
