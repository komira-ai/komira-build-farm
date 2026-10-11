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

The preferred layout gives each role its own hosts: servers on storage hosts, build
work on build hosts. Servers, the object store and daemons may also share the same
hosts; that [converged topology](#converged-topology) is supported too.

## The shape

```
 build clients (Bazel, Buck2)                     worker nodes (kbf-daemon)
          |                                                 |
          | REAPI over TLS, to one constant name            | kbf.worker.v1 over mutual TLS,
          v                                                 | straight to the servers (one
                                                            | DNS name or a list)
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
          | gRPC; TLS with an internal-CA                   |
          | certificate when on another host                |
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

### Servers on storage hosts

Each `kbf-server` runs on a storage host with a fixed address and a stable DNS name.
In the preferred layout these hosts are dedicated: build hosts run daemons and no
server. The [converged topology](#converged-topology) runs both on the same hosts.
The set of servers changes only when an operator changes it.

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

- **Snapshots go to the object store too.** Each snapshot of the log is written on
  the server's local disk and also copied to the object store, so a host whose disk is
  lost can be rebuilt from the newest snapshot there. With a single voter this is what
  stands between a lost disk and a lost farm state: what the log committed after the
  newest copied snapshot is still lost with the disk. With three voters a replacement
  catches up from the leader as a learner, and the copy is a further backstop.

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

- **Daemons' blob reads too.** A daemon's input reads go to any server, not only the
  leader: the daemon spreads its `ByteStream.Read` calls across every server it is
  given, on each server's worker listener ([below](#daemons-straight-to-the-servers-one-stream-to-the-leader)).
  The same staleness rule holds for them. A daemon's output writes are upload bytes,
  and follow the upload rule above.

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
  when it falls behind or loses touch with the leader. A server's `leader` check
  fails as soon as it stops being the leader, not at its next election.
- **`grpc.health.v1` on the REAPI listener.** Beside the two readiness paths, each
  server serves the standard gRPC health service on its REAPI listener, so a proxy
  can health-check a server over gRPC on the port it routes to. This is built: today
  every service name it serves gives `/readyz`'s one answer (an unknown name is answered as [api.md](../api.md) describes)
  ([api.md](../api.md#grpchealthv1-on-the-reapi-listener)). Planned: its answers
  follow the two readiness paths, ready to serve reads and leader; which service
  name reports which is not chosen.
- **The proxy-to-server hop.** TLS for the farm's client-facing name is the front's
  job. Each server's REAPI listener serves the proxy over TLS with a certificate from
  the farm's internal CA (`--reapi-tls-cert`, `--reapi-tls-key`; the listener's TLS
  is built, the proxy is not), which the proxy trusts; a server whose proxy runs on the same host may serve it plain text on
  loopback instead. A plain-text REAPI bind on any other address stops the server at
  start unless `--reapi-plaintext-bind` is given (built; the start line then carries
  a warning), and a worker listener off loopback needs mutual TLS
  ([Security model](../../ARCHITECTURE.md#security-model)). The same internal-CA certificate lets a client that trusts that CA
  dial a server directly. It is a server certificate only, so it encrypts the hop and
  names the server but identifies no caller; the REAPI authentication policy
  ([reapi-auth.md](../reapi-auth.md)) decides who may call, over TLS as over plain
  text. See the [Security model](../../ARCHITECTURE.md#security-model).

### Daemons: straight to the servers, one stream to the leader

Daemons do not go through the client front.

- **How a daemon is given the servers.** Either one DNS name with an address record
  per server, or a list of addresses (or names). The daemon dials them directly,
  over the worker listener's mutual TLS ([worker-protocol.md](worker-protocol.md)).
  This is built: `--server` is repeatable and each host is resolved again on every
  round of attempts ([daemon.md](daemon.md#reaching-a-server)).
- It holds **one** worker stream, to the leader.
- **The redirect.** A follower that receives a daemon's session does not serve it:
  it ends the `Session` stream with a status that names the leader, and the daemon
  dials that server. A follower that knows no leader (an election in progress) has
  nothing to name: it answers `UNAVAILABLE`, and the daemon tries the next server.
- **Retry forever.** A daemon never gives up on the farm. When a dial fails, a
  stream ends, or no server names a leader, it tries the next server, and after a
  pass over all of them it waits and starts again, with a backoff that grows to a
  bound and stays there. Only a stop signal ends the daemon. This is built
  ([daemon.md](daemon.md#reaching-a-server)): the bound is `--reconnect-max-ms`, and
  an `UNAVAILABLE` answer moves the daemon to the next address at once. The redirect
  above is not.
- **A failover keeps running leases.** `Welcome.epoch` names the replicated log,
  which outlives leaders and their terms, so a change of leader does not change the
  epoch, and the daemon keeps its running leases. It reconnects, reaches the new
  leader (directly, or through a follower's redirect), registers again, and resends
  the results the old leader did not acknowledge; the new leader knows the committed
  grants from the log and accepts them. Lease ids carry the granting leader's term,
  and the scheduler refuses results from leases it does not hold, so a lease a stale
  leader granted but never committed cannot be confused with the new leader's
  (issues [#137](https://github.com/komira-ai/komira-build-farm/issues/137) and
  [#140](https://github.com/komira-ai/komira-build-farm/issues/140); see
  [scheduler.md](scheduler.md#leases)). A restart of a single server keeps its
  leases the same way once its log is on disk.
- While no stream is acknowledged, the daemon's fence clock runs as it does today:
  self-fenced work stops T = 40 s after the newest acknowledged heartbeat was sent. A
  failover that takes longer than that, from the daemon's point of view, stops its
  self-fenced work.
- **Blob calls on every server.** A daemon's blob calls go to the worker listeners
  of all the servers it is given, not only the leader's; its reads are spread across
  them ([above](#reads-and-uploads-on-every-server)).

### A durable node registry

The record of nodes becomes durable state in the control log:

- a node that is absent (no stream) is remembered, not forgotten;
- an absent node raises an alert;
- an absent node gets no work;
- a node that reconnects recovers its place: its report, labels and placement state
  (cordoned, draining) are as they were.

## Converged topology

Servers, the object store and daemons may run on the same hosts: each host runs a
`kbf-server`, a node of the object store and a `kbf-daemon`. This is a supported
topology, not only a stopgap; the layout with dedicated roles above is preferred,
because there a runaway build cannot press on the farm's state. What changes:

- **The memory split.** On a dedicated build host, `--actions-memory-max-gib` is the
  host's RAM minus 4 to 8 GiB ([linux-build-host.md](../deploy/linux-build-host.md#headroom---actions-memory-max-gib)).
  On a converged host it must also leave room for the object store's node and
  `kbf-server`: RAM minus what those two use at their peak, minus the same headroom
  for the OS and the daemon. The cap is set per host. The node reports the lower of
  this cap and its memory, so the scheduler books only what builds may use.
- **The Raft log on its own partition.** The log's disk is not shared with build
  scratch, swap or the object store's data, so builds that fill a disk cannot stop
  the log from being written, and the log's writes to disk do not queue behind
  build I/O.
- **What else changes against dedicated roles.** A host's failure takes a server, a
  share of the object store and a worker at once, so the three-server shape needs
  three converged hosts to keep a quorum through one loss. Placement may put work on
  the leader's host; the leader's own processes are protected only by the memory
  split above. Everything else is the same: the client front, the daemons' direct
  dials, the redirect, the readiness paths and the read path do not depend on which
  host runs what.

## Built and planned

| Part | Today (on `main`) | Planned |
|---|---|---|
| Servers | one `kbf-server` process runs every role (`--role=all`) | one server per storage host, dedicated or [converged](#converged-topology); three for HA |
| Raft core | `kbf-raft`: a sans-IO core with election, replication, commit and learners; a single voter elects itself and commits alone (`crates/kbf-raft/tests/scripted.rs`); simulated with 3 voters and a learner. Snapshots, membership changes, PreVote and CheckQuorum are not built | the same core, with snapshots and single-server membership changes |
| Raft in the server | none: `kbf-server` depends on none of `kbf-raft`, `kbf-store` and `kbf-log`. Its host loop (`kbf_raft::Host`) runs over `Storage`, `Transport` and `Machine` traits, with only an in-memory `Storage`. `kbf-store` keeps a Raft log and hard state on the local disk (segmented, CRC-checked, a torn tail cut at open, fail-stop on an I/O error) but does not yet implement that trait. `kbf-log` encodes every `kbf-meta` command as a `kbf.log.v1` entry; nothing writes such entries yet | the log and its snapshots on each voter's local disk, snapshots also copied to the object store; `kbf-server` applies control and metadata state from it |
| Metadata | `MemoryMetaLog`, in the server's memory; lost at restart. With `--store=s3` each start writes under a fresh key prefix | applied from the log, so a restart keeps it |
| Leases | each process picks its own term at start (wall-clock milliseconds times 2^16 plus 16 random bits); `Welcome.epoch` names it; a daemon drops leases of another epoch; leases of an earlier process are refused (#137); another daemon process's leases are kept for the handover grace (#140) | the term comes from the Raft log, and `Welcome.epoch` names the log, so a failover keeps running leases and daemons resend their results to the new leader ([above](#daemons-straight-to-the-servers-one-stream-to-the-leader)) |
| Readiness | `GET /healthz` and `GET /readyz` on the operator API listener (`--api-listen`; [api.md](../api.md#get-healthz-and-get-readyz)). `/readyz` is 503 once a stop signal arrives, when a read-only store probe fails or times out, and when the server does not hold the scheduler role, a flag a single server always holds. There is one readiness path. The REAPI listener serves `grpc.health.v1.Health` with `/readyz`'s answer, the same for every service name ([api.md](../api.md#grpchealthv1-on-the-reapi-listener)) | two readiness paths: the Raft role sets the leader flag, so `/readyz` is 200 only on the leader; a second path, not yet named, is 200 on any synced server; `grpc.health.v1` gives the same two answers ([above](#build-clients-one-name-routed-by-method)) |
| Reads and uploads on followers | none: one server serves every call, and the cache commits its own metadata | at three servers, any synced server serves `ByteStream.Read`, `BatchReadBlobs`, `FindMissingBlobs` and `GetActionResult` from its applied state, and writes upload bytes to the object store before sending the leader the metadata commit; a read index where a stronger guarantee is needed ([above](#reads-and-uploads-on-every-server)) |
| Follower redirect | none: there are no followers, and `kbf.worker.v1` has no redirect | a follower ends a daemon's session with a status naming the leader; one that knows no leader answers `UNAVAILABLE` |
| Daemon's servers | one or more `--server` URLs, each host resolved again every round (a DNS name may carry a record per server); the daemon tries every address in turn, moving on at once after a refused connection, a TLS failure or `UNAVAILABLE`, and waits a jittered, doubling time, at most `--reconnect-max-ms` (30 s), between failed rounds; it never stops trying ([daemon.md](daemon.md#reaching-a-server)). No redirect is followed: there is none to follow | the daemon follows a follower's redirect and dials the leader it names |
| Client front | proven only with `tailscale serve` in its HTTPS mode, on the server's host, in front of a loopback REAPI listener (pull request [#246](https://github.com/komira-ai/komira-build-farm/pull/246); see the [Security model](../../ARCHITECTURE.md#security-model)); it routes by nothing, as there is one server | a tailnet ingress and an HTTP/2 proxy that routes by gRPC method, after the probes below pass |
| REAPI listener TLS | `--reapi-tls-cert` and `--reapi-tls-key` serve the REAPI listener over TLS with a server certificate only; without them it serves plain text. The `--reapi-auth-policy` layer runs on either | the proxy-to-server hop over TLS with an internal-CA certificate on every server whose proxy is on another host, which clients on the private network may also use to reach a server directly |
| Node registry | in the server's memory: a node whose stream closed stays listed with `connected: false` until the server restarts, and gets no work once it has not been heard from for G; a restart forgets every node. `kbf-alert` exists, but nothing raises alerts yet | durable, as [above](#a-durable-node-registry) |
| Daemons' blobs | the drivers read and write blobs with `ByteStream` on the mutual-TLS worker listener (`--cas`, `https://` only), each call checked against the node certificate and the deny list; daemons need no path through the front ([worker-protocol.md](worker-protocol.md#blobs-on-the-worker-listener)) | the same, on the worker listener of every server: reads spread across all of them ([above](#reads-and-uploads-on-every-server)) |

## Probes before relying on the front

The ingress and proxy pair is not proven. Each of these must pass through the real
pair (ingress, then proxy, then a REAPI listener over TLS with an internal-CA
certificate), as the `tailscale serve` probe did for its front:

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

## Decided questions

These were open; the maintainers decided them on 2026-10-10. The last column says
what of each decision is on `main`; the rest is **planned**.

| Question | Decision | Built |
|---|---|---|
| What a failover keeps | running leases: `Welcome.epoch` names the replicated log, and daemons resend their results to the new leader ([daemons](#daemons-straight-to-the-servers-one-stream-to-the-leader)) | none: the epoch names the server process, so a restart drops every running lease |
| How the daemon is told the servers | one DNS name with a record per server, or a list of addresses | both: `--server` is repeatable, and each host is resolved again every round ([daemon.md](daemon.md#reaching-a-server)) |
| The redirect's form | a status on the ended `Session` stream naming the leader; a follower that knows no leader answers `UNAVAILABLE` and the daemon tries the next server; daemons retry forever, with a bounded backoff | the daemon's side but the redirect: it moves to the next address on `UNAVAILABLE` and retries forever with a backoff bounded by `--reconnect-max-ms`. No server sends a redirect, and the daemon follows none |
| Readiness on a follower | the two readiness paths, plus `grpc.health.v1` on the REAPI listener ([build clients](#build-clients-one-name-routed-by-method)) | `grpc.health.v1` on the REAPI listener, with `/readyz`'s one answer ([api.md](../api.md#grpchealthv1-on-the-reapi-listener)); the second readiness path is not |
| The proxy-to-server hop | TLS with a certificate from an internal CA, which clients may also use to reach the servers directly ([build clients](#build-clients-one-name-routed-by-method)) | the REAPI listener's TLS: `--reapi-tls-cert` and `--reapi-tls-key` serve it with a server certificate, and `grpc.health.v1` is served over that TLS too. No proxy is deployed in front of it yet |
| Snapshots off the host | copied to the object store ([durable state](#durable-state-a-raft-log-on-local-disk)) | none: `kbf-store` keeps a log on the local disk, but no snapshots are built and the server runs no Raft log |
