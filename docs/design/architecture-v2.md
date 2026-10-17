# kbf architecture v2: frontend, state store, storage, daemon

Status: design, nothing built. It supersedes the leader-elected server cluster in `cluster.md`. Labels: **[F]** fact checked in the code at `200d7289` or on `main`, **[D]** owner decision, **[I]** inference, **[V]** verify before building on it. Numbers are arithmetic from stated hardware, not measurements; section 13 names the benchmarks to run first.

## 1. Decisions

- **[D]** kbf is four parts, as Buildbarn is frontend, scheduler, storage and worker.
  - **Frontend.** Stateless when the state store is Postgres (on SQLite its queue lives in the file on its own disk, section 4.3). Runs behind a load balancer (LB) in an autoscaling group; its DNS name is the one address clients, daemons and storage nodes know. Serves REAPI, runs the scheduler logic and the UI. Holds no durable state of its own beyond the state store (scheduler rows and the UI's index rows) and the object store (UI history objects, `ui-data.md` 5.2).
  - **State store.** The queue, leases (each action handed out once), operations and membership. An interface: a backend is any implementation of the `kbf-state` trait that passes the conformance suite (4.1). SQLite (a single frontend, and always `all-in-one`) and Postgres (required for several frontends, allowed for a single one) are built first; the `--state-store` URI's scheme names the backend.
  - **Storage.** CAS, action cache (AC), SSD hot store, background flush to the cold store (MinIO or S3). Sharded by digest with a replication factor R (default 2). No leader. Storage nodes do not schedule.
  - **Daemon.** Runs actions.
- **[D]** Modes of the one `kbf` binary: `frontend`, `storage`, `daemon`, `storage-and-daemon` (the daemon reads and writes local storage in-process), `all-in-one` (frontend, storage, daemon and a SQLite state store).
- **[D]** Kept: no per-action reservations (admission by CPUs, memory ladder); telemetry-driven cgroup sizing; NUMA placement; trusted-network and isolated-storage modes both tested; flags, not environment variables; no service bootstraps itself; tests never depend on timing; no compatibility with earlier formats.
- **[D]** This replaces the leader-elected design in `cluster.md`, the formation records in PR #750, the leader parts of PR #751, and the Envoy/xDS part of PR #746.

## 2. Answers to the discovery questions

- **How do daemons find storage nodes?** They do not look. A daemon knows the frontend name. The frontend sends the storage map (members plus hashing rule) in `Welcome` on the daemon's existing Session stream and pushes every change.
- **How does the frontend find storage nodes?** It reads membership from the state store. It never probes.
- **How do storage nodes find anything?** Same as daemons: they register with a frontend and hold a stream to it, and the map is pushed down it.
- **Does the frontend distribute work across storage nodes?** No. It places actions on daemons. It touches storage only to proxy client AC lookups and CAS bytes, routed by digest.
- **Does daemon-frontend traffic go through storage?** No. Work goes frontend to daemon directly. Daemons fetch inputs from, and upload outputs to, storage directly. Daemon-to-frontend traffic never carries bytes.

## 3. Crate mapping

- **[F]** Frontend: `kbf-front` (REAPI), `kbf-auth`, `kbf-sched`, `kbf-server`. State store: the `ControlRecord` log and a new `kbf-state` trait (no SQLite or Postgres backend exists). Storage: `kbf-meta`, `kbf-tier`, `kbf-segments`, `kbf-objstore`, `kbf-front/src/cache/*`. Daemon: `kbf-daemon`, `kbf-node`, `kbf-transfer`, `kbf-driver-*`.
- **[F]** `kbf-front` owns both the REAPI services and the whole cache write path (`cache.rs:182`); storage is cut out along `cache/*` (P2). Scheduler state is not in the Raft log today and `farm.rs` runs under one `Mutex<State>`.
- **[F]** The `Blobs` trait (`kbf-server/src/local.rs:36-40`) is the in-process seam `kbf-all-in-one` uses. The storage-and-daemon plane is a `Cas` implementation over the local store, modelled on it.

## 4. State store

### 4.1 Contract

A backend is admitted only if it passes one conformance suite, run against an in-memory fake and every backend.

Each clause and how the first two backends, SQLite and Postgres, meet it is in 4.3.

- **C1.** Linearizable compare-and-set per key, with a version.
- **C2.** Atomic transaction over several keys.
- **C3.** Strictly monotonic counter (lease sequence, map generation, boot counters).
- **C4.** TTL keys with expiry decided by the store's clock, observable as an event, on an injectable clock in tests.
- **C5.** Ordered prefix scan and a change feed that resumes from a revision with no skipped rows: a poller never advances past a revision that can still commit. Polling by revision is the baseline; push (watch, `LISTEN/NOTIFY`) is only an optimization, because a notification is not queued for a listener that is disconnected and is not delivered through transaction-mode poolers. If used, it is a coarse wake with no per-row payload: commit-time NOTIFY serializes commits and would hurt at hundreds of commits per second.
- **C6.** Durable acknowledgement across restart and failover.
- **C7.** Group commit, because about 5 durable writes per action **[I]** make an ungrouped fsync the limit. Budget **[I]**: 9000 actions in about 2 h is about 1.25 actions/s, about 6 writes/s; a burst of 300 actions/s is about 1500 writes/s; heartbeats are one row per daemon session, negligible. One fsync per write caps near 200 to 1000 writes/s, so 1500 writes/s needs real batching. Commits are batched within a bounded window (target 5 ms, a hard limit configurable and exported as a metric), and every `Start` and `Result` acknowledgement waits for its commit. A transaction may be marked background (the UI index rows, `ui-data.md` 5.2): a backend applies it only in a batch with no foreground transaction waiting, or, once it has waited 250 ms or 16 batches (whichever comes first), in the next batch regardless, so the index cannot starve under a sustained burst; either way at most one per batch, never ahead of a foreground transaction already queued, and it refuses one over 256 rows or 256 KiB. The suite's load test asserts two things with background transactions offered at 512 rows/s beside 1500 foreground writes/s: `Result` commit p99 stays within the bound, and the background rows are applied at no less than the offered rate with a bounded background queue; a planted backend that never applies a background transaction must fail it. So a backend admitted later can neither put background work on the acknowledgement path nor starve it.
- **C8.** Bounded, ordered range scan under a prefix (`ui-data.md` 5.2, Q20): the live keys between a lower and an upper bound (each included, excluded or open, clipped to the prefix), walked forward in C5's key order or in reverse, at most a limit of them, and a resume token that continues exactly past the last key in the scan's direction; each page is one snapshot of committed transactions, the pages of a scan are not, so a key committed behind the token is not returned and one ahead of it is. Reverse serves newest-first lists over keys that embed an ascending time, and with a limit of 1 under an included upper bound it is the last key at or before that bound. A page returns at most the limit; a backend reads up to one live key more (which decides the token) plus the expired keys in that stretch not yet reaped.

A backend is any implementation of the `kbf-state` trait that passes this suite; SQLite and Postgres are built first, and others (for example a cloud key-value or database service) may follow on the same terms. There is still no built-in Raft or replicated store of kbf's own. **[I]** C5 (one gap-free feed) and C7 (group commit) are the hard clauses for other stores: one whose stream order or TTL is per partition or imprecise must keep its own revision counter and `expires_at` checks.

### 4.2 Contents

Operations; a queue indexed by platform, size class and QoS (a claim never scans unrelated entries); `inflight/<action_key>` dedup; leases; one `session/<daemon>` row; members; the map; whole-machine holds; memory-ladder floors; loss marks; repair records; the UI's index rows and failure pins (`ui/`, `ui-data.md` 5.3). Three rows exist for a frontend that hands over (section 11): `meta/generation`, the writer generation every write of the scheduler is checked against; `done/<operation>`, how a finished operation ended, kept for the finished retention (at least ten minutes) so a process that did not finish it can answer the callers it kept; and `cordon/<node>`, how an operator placed a node (cordoned, or draining until a wall-clock deadline), kept until the node returns to placement.

### 4.3 Backends

`--state-store=sqlite:///path` or `--state-store=postgres://...` for the first two backends; each backend claims its URI scheme. `ControlRecord` (`Lease | Result | Refusal`, "commit before Start", `work.rs:323-330` **[F]**) becomes rows written through the `kbf-state` trait.

**Flag details (as built).** Only `sqlite:///absolute/path` is wired. Any other value, `postgres://` included, is refused with fixed text that never quotes the value (a mistyped URL holds a password, and the command line is world-readable); the Postgres backend of `kbf-state` plugs in behind the `Store` trait when it is wired, with its own way to take a password. `all-in-one` takes `sqlite:///...` only and, with no flag, `<data-dir>/state.db`. The state directory a start makes is 0700 and the files in it 0600.

- **SQLite.** Used by `all-in-one` (always; a file beside its data) and by a single `frontend`. The process takes an exclusive advisory lock (`flock`) on a lockfile beside the database, `<file>.lock`, held for the life of the process, and does not rely on `locking_mode=EXCLUSIVE` (which would also disable WAL readers). A second frontend on the same file refuses to start; so does a `--state-store=sqlite://` on a network filesystem, which the docs forbid (locks and fsync are unreliable there). Operator inspection: the database is in WAL mode, so a read-only `sqlite3 -readonly` open, or a `.backup`, works while the frontend runs and never blocks it; writes by an operator tool are refused while the lock is held. Every frontend start on SQLite bumps the epoch (a restart is a store restart), and `kbf state restore` is offline, requires the lock, and bumps the epoch in the restored file, so a restored or swapped file can never resume old leases. Losing the disk loses the queue; the docs say so plainly. Not highly available. The SQLite writer is one thread: claims, `Start`/`Result` CAS and counters run inside it, and WAL checkpoints can stall it briefly, which is expected and covered by the commit-latency bound in C7.
- **Postgres.** Required when several frontends sit behind a load balancer; a single frontend may also use it. High availability is the operator's Postgres setup; `synchronous_commit` must be on.

| Clause | SQLite | Postgres |
|---|---|---|
| C1 CAS | Single writer; `UPDATE ... WHERE version = ?` inside `BEGIN IMMEDIATE` | `UPDATE ... WHERE version = $1`, or `SELECT ... FOR UPDATE` |
| C2 transaction | One `BEGIN IMMEDIATE` transaction | One `SERIALIZABLE` transaction, retried on serialization failure; claim transactions are the exception (C7) |
| C3 counter | Row incremented in the writer transaction | Sequence, or a row incremented under `FOR UPDATE` |
| C4 expiry | `expires_at` compared with a clock the store layer owns (the single process), injectable in tests; a reaper task | `expires_at` compared with `clock_timestamp()` in the database (`now()` is the transaction start time); a reaper task |
| C5 feed | Monotonic revision column; polling by revision | Revision allocated from a single counter row under its row lock, in the writing transaction, so revisions commit in order; or the poller reads only revisions below a commit horizon (no open transaction older than it). A bare sequence is not enough: it can commit out of order and a poller would skip a row. Polling is the baseline; `LISTEN/NOTIFY` only as a coarse wake. Test: interleave two commits so the later revision commits first, and the poller still sees both |
| C6 durability | WAL, `synchronous=FULL` | `synchronous_commit = on` |
| C7 group commit | One writer thread batches queued writes into one transaction and one fsync | Batched transactions; group commit by the server's WAL flush; claim transactions run READ COMMITTED with `FOR UPDATE SKIP LOCKED` (SKIP LOCKED gives an inconsistent view by design, and under SERIALIZABLE a row updated after the snapshot would fail instead of being skipped); safety comes from the `op.state == Queued` check plus the version CAS |

### 4.4 Epoch, expiry and outage

- **Epoch** is the store incarnation, bumped on restore, failover or replacement. `Welcome.epoch` carries it and a daemon seeing a new epoch kills leases from the old one **[F]**. A failover that did not bump it would leave stale leases running. Lease id is `(epoch, seq)`.
- **Renewal is per daemon session.** One `session/<daemon>` row is extended per interval; a `lease/<id>` row is a fencing token written at claim and commit only. **v1:** the row carries a TTL of the lease grace and a heartbeat renews it once per third of the grace, however many leases the daemon holds.
- **Post-outage grace.** After recovery or failover every expiry timer restarts with a grace window; otherwise an outage makes all leases look expired and requeues all running work. Red test: a fake clock stopped across a simulated outage requeues nothing inside the window. **v1:** the scheduler's deadlines are in memory, so a write the store cannot take marks an outage; until a write takes again nothing is given up or granted, and the first one that takes restarts every timer with one lease grace.
- Expiry is judged by the store's clock, never a frontend's (SQLite: the one frontend process that owns the file).

## 5. Scheduler

**v1: one frontend** runs the scheduler as `farm.rs` does today, writing every change through the `kbf-state` trait; memory is a cache, the store is the truth. Global first-fit, holds and the ladder are unchanged. **[I]** Buck2 does not re-submit a lost Execute: it retries only opening the stream (up to 5 attempts), and a stream that breaks after it started fails the action (`remote_call_error`); it never calls WaitExecution. So a frontend that goes away must not break its streams. On SIGTERM it refuses new Execute calls UNAVAILABLE (retried at open, so they reach another frontend) and keeps every open stream until its action is done (`--shutdown-timeout-secs`, default no deadline); every open stream is sent its operation again every `--progress-interval-secs` and the listeners send HTTP/2 PINGs (`--http2-keepalive-*`) so no idle timeout cuts it. What must be durable is still small: operations, leases, membership, the map, the epoch.

**Several frontends: per-frontend pull-claim.** Placement is global first-fit under one mutex **[F]**, so N frontends cannot share it. A frontend owns the daemons connected to it and claims for their free slots:

- Predicate: platform, CPUs, ladder floor, cordons, whole-machine holds. Floors and holds come from a watch-fed cache; each frontend keeps a head cache per queue partition.
- **Batch claim:** one transaction claims k entries for k free slots, starting at a random offset in the top entries to bound contention.
- Transaction: require `op.state == Queued` at the version read, take the next `seq`, write `Leased(id)` and `lease/<id>`, delete the queue entry. A loser fails the first check and moves on, so one live lease per op is a store invariant.
- Lost: global best-fit and strict cross-daemon QoS fairness (QoS order within a partition is kept). The alternative, an elected placement leader, is what is being removed.

**Fencing.** Every `Start` and `Result` is a CAS on `op.lease == id`; a stale daemon gets `FENCED`. With several frontends a second fence is required: `session_gen` is bumped by CAS on every Hello, and a frontend writes lease changes for a daemon only while its `session_gen` matches, so an old frontend cannot requeue a moved daemon's live leases.

**Guarantee.** At most one live lease and one committed result per operation. Execution is at-least-once: a partitioned daemon can run past expiry and its result is fenced out; content-addressed outputs make a stale upload junk, not corruption.

**Reconnect.** A daemon's `Hello` carries `instance_id` and `predecessor_instance_id` **[F]**. The new frontend bumps `session_gen`, lists the daemon's leases and reconciles with its reported running set (the `worker.rs:1-52` reconcile **[F]**): still-running leases are re-bound, unknown ones, including claimed but never delivered, are requeued.

**Poison actions.** Lost-lease requeues have their own cap (`loss_requeues`), separate from `attempt`; the daemon's report distinguishes the cause where it can; over the cap the operation is refused. **v1:** a lease given up as silent, replaced, restarted or reconnected without it counts, the count is a `loss/<op>` row written with the give-up, and the third loss refuses the operation through the log with the reason (FAILED_PRECONDITION); a worker's own report, the memory ladder, a GUI session switch and a `Start` that never arrived keep their separate budgets.

**Waiters.** The operation is durable at submit. A waiter is held by the frontend with the client's stream and reached through the change feed, or in-process when committer and waiter are the same frontend (the v1 case). A client of a dead frontend re-attaches with `WaitExecution` through the LB.

## 6. Discovery, membership and the storage map

- Every node is configured with `--frontend <dns>` plus certificate flags; storage nodes and daemons add `--advertise-addr` where the bind address is not routable. Nothing else.
- **[F]** `connect.rs` already re-resolves DNS and backs off to 30 s.
- **Registration.** A storage node or daemon opens a stream to any frontend and sends role, `instance_id`, advertise address, capacity, zone label and protocol version. The frontend writes `member/<id>` with a TTL; the member id and boot counter (section 8.4) are assigned by the store, so no node picks its identity. Membership lives in the store, so a reconnect to another frontend loses nothing.
- **Storage nodes do not talk to the state store.** They reach it only through the frontend. This keeps the store's attack surface to frontends.
- **Heartbeats** refresh the TTL through the frontend; expiry is store-judged on the injectable clock reused from PR #746.
- **Map** = `{generation, members[{id, addr, zone, weight, state}], rule, R, min_protocol, denied}`. States: `joining`, `catching-up`, `active`, `draining`, `down`. `Welcome` carries it and a new `MapUpdate` message pushes changes; storage nodes get it down their registration stream.
- **Dwell.** A member whose TTL lapses becomes `down` but stays in the map for a dwell (reusing `TierDown`/`TierUp` and `--hot-store-abandon` from `cluster.md` 4.4 **[F]**) before it leaves and its slice is re-replicated. This prevents flapping. A generation bump is one CAS on `map`; two frontends racing produce one winner and one retry.
- **Staleness protocol.** Every storage RPC carries the caller's generation. A node that sees an older one answers `STALE_MAP` with the current number and the caller refetches; a node that sees a newer one fetches it. A lost push is therefore harmless.
- **Admission.** Holding a valid storage-role certificate that names the node gets it a member entry (`active` or `catching-up`, below; `joining` once the join handoff exists). "Names the node" is checked under mutual TLS at `Register`: the certificate's DNS or IP subjectAltNames must cover the host of the `instance_id` and the host of the advertise address (`host` or `host:port`, an IPv6 literal in brackets; DNS names compared exactly up to ASCII case, IPs as parsed addresses, a wildcard name covers nothing), so one storage certificate cannot register another node's instance or address. A `Heartbeat` is checked against the address stored for the member, before the boot is compared, so a certificate that does not name that address learns nothing of the member's boot or state and cannot promote it (it can still tell a live row it does not hold, PERMISSION_DENIED, from one that lapsed, NOT_FOUND; member ids and states are in the map every node gets). A refusal is PERMISSION_DENIED and says to reissue the certificate with the node's names. `--worker-deny-list` (serial, public-key hash or node id, as for a daemon) is applied to `Register` and `Heartbeat` as well, and a list that cannot be read refuses them UNAVAILABLE. In plain text nothing is bound. Not bound: `weight` and the number of instances one certificate may register. A joiner takes writes at once, is never rank 1, and is read last; it becomes a read owner in rank order only once `active`, after it has caught up (section 8.4). Who may hold a storage certificate is the operator's CA policy. **Registration and promotion.** A registration that says the node's index is whole (a first start, or a restart whose meta log already has its rebuild) is written `active`, so a rolling restart leaves every member `active`; one that says the node is rebuilding is written `catching-up` and goes `active` on its first beat that says it is done. Whatever makes a member `active` writes `meta/members_formed` (permanent) in the same transaction. No registration writes `joining`: that state is set by the join handoff of 8.4, which is not built, through `Members::set_state`. An operator promotes a joiner with `POST /v1/members/{id}` (`kbf cluster member <id> --state active`; `docs/api.md`), which calls `Members::set_state`. A joiner promoted before it holds its slice: a read that gets `NotFound` from an owner tries the next owner (`kbf-route` read walk), but an old copy-holder pushed out of the top `r` is reachable only through the previous generation's list, so its blobs read as missing once the map moves on; writes of the slice go to the joiner as primary and are re-sent until it fills.
- **Hashing.** Rendezvous (highest-random-weight) hashing over digest, member id and weight; owners are the top R. This is the consistent-hashing family Buildbarn uses for sharding: removing a member remaps only the blobs that resolved to it, and there is no ring state to coordinate. Cost is one hash per member per digest, negligible at 6 to 100 members. Zone-aware second owner is optional and off by default.

## 7. Traffic

- Client to LB to frontend, always. Client AC and CAS: frontend to the owning storage node, pass-through, never buffered. Work: one daemon stream to a frontend, no bytes. Input fetch and output upload: daemon directly to owners via the map. Flush: storage to the cold store.
- **Proxy, not redirect.** REAPI ByteStream has no redirect, so stock clients could not follow one **[I]**; proxying keeps one address and one authentication point.
- One routing library implements `Cas` over the map for frontend and daemon, with an in-process arm for `storage-and-daemon`; `kbf-transfer` keeps its flight dedup across endpoints.
- Prefer an L4 LB for daemons; raise idle timeouts for L7; gRPC keepalive stays below the LB idle timeout.

## 8. Storage

- **Writes: chain replication.** From a 1 GbE frontend or Mac, sending to all R owners halves unique upload (about 58 of 117 MB/s) **[I]**. A writer sends each blob once to the primary (rank-1 owner), which forwards to the replica over the storage fabric and acknowledges when both hold it on the hot tier. With the replica down, the primary acknowledges after its own durable write and records `repair/<digest>` in its journal and, through a frontend, in the state store, so the record survives the primary. "Live owner" is judged by the map generation, so a stale view gets `STALE_MAP`, not a silent single copy. **As built (`kbf-storage` `replicate/` and `repair/`):** the blob streams through the primary into its store's put stream and on to the replica, never held whole; the replica verifies the digest and commits first, and the primary's own copy becomes present only after that (and, if the replica could not be reached, only after a `repair` record is durable in the primary's journal), so a blob present on a primary has a second copy or a durable debt, and a crash in between leaves it absent and the writer writes again. If the map names the primary down, the next owner takes the write and owes the primary its copy. The frontend is told of a debt after the journal has it and never waited for. A worker settles a debt only when every current write owner holds the blob; it asks the owner first (`FindMissing`) and sends the blob only if it is missing.
- **Reads.** Owners in rank order, then the previous generation's owners, then the cold store through an owner's index. Non-first hits trigger rate-limited read-repair. FindMissing is split by owner and asks the primary only; a replica is not asked on a miss, since cold builds are mostly misses.
- **AC.** Sharded by action digest, with the small `ActionResult` inline. An entry is written only after its closure is acknowledged: for a client `UpdateActionResult` the AC owner checks the closure with batched FindMissing to the other owners; for an action's own result the daemon supplies the acknowledgements. Reads present-check the result blob only (cached per generation). Closure blobs flush before the AC entry.
- **Loss marks.** When a hot tier is abandoned, digests it held and never flushed get `loss/<digest>`; an AC read whose result blob is marked is a miss. Residual: a later-evicted closure blob gives a client a missing output and a re-execute, not corruption. The lost-blob gauge makes it visible.
- **Join and drain.** Generation g+1 is published; readers use g first until handoff finishes. A joining node is a write owner at once but never rank 1, and read last; old owners copy changed blobs, throttled; then it is `active`. Drain is the reverse and is not a kill. A node drops a blob only after every new owner confirms it, or the cold-store PUT is acknowledged and the index entry committed.
- **Rebuild.** A new owner has no index for blobs a previous owner flushed and rebuilds from self-describing footers (`startup_pass.rs` does it for `file://` **[F]**; listing S3/MinIO is new work **[I]**), staying `catching-up` until done.
- **Object keys.** `AllocEpoch` is a global write (`state.rs:25` **[F]**); ids become `(node id, boot counter)`, the counter from C3 at registration, which removes it from the data path and avoids collision with the old server's objects.
- **One flusher.** The rank-1 owner flushes; the replica evicts after the flush is confirmed and flushes only if the primary is down past the dwell. Otherwise both write the same blobs into different objects and double PUT bytes and S3 cost. Built: `kbf_storage::flush::OneFlusher` is the gate (rank 1 of `write_owners` under the map the node holds, fail closed without a map or member id, and held while a caller has shown the node a newer generation than it holds); `Cache::flush_tier` skips a sealed object none of whose digests the node flushes. An object that holds both ranks is flushed whole, so its replica digests ride along. Not built: the replica's evict-after-confirm, which needs a way to forget a sealed staged object in the metadata log (`EvictObjects` skips staged objects) and a confirm call from the primary.
- **Segments.** One `Segments.Begin/PutRange/Commit` session per owner, with a minimum segment size and a bound on open sessions so segments do not shrink to about 1/(6/R) of today's **[I]**.

## 9. Security

- mTLS on every hop with the role (frontend, storage, daemon) in the certificate; storage accepts only expected roles per endpoint. Certificates and keys come from files; nothing in environment variables. Clients use TLS at the LB.
- Daemon identity and deny-list checks (`identity.rs` **[F]**) must be repeated on storage nodes, because the worker listener's `ByteStream` moves there. The frontend issues a short-lived signed storage-access grant in `Welcome`; storage nodes verify it with the frontends' public key (provisioned as a file). The deny entries (certificate serial, public-key hash or node id, one canonical form each) ride in the map, so a denied daemon loses storage access when the generation turns.
- **Trusted-network:** daemons and storage trust each other on the private network; mTLS stays on.
- **Isolated-storage:** only storage nodes hold cold-store credentials. Daemon-only nodes (the Macs, a cloud daemon ASG) hold none and reach bytes only through storage. A test proves a daemon-only process cannot reach the cold store.
- **Credential gap on `storage-and-daemon`.** A node that holds cold-store credentials and also runs untrusted actions is not isolated unless storage runs under a different uid from actions and the credentials are unreadable by the action's uid. Without that, they are trusted-network only. Decided: separate uids, with a test that an action cannot read the credential (section 15).
- The state store is reachable only from frontends (a SQLite file is opened by one frontend only).
- **UI history.** Frontends write UI history objects (`ui-data.md` 5.2) with a credential limited to the history prefix (or a separate bucket), never one that reads or writes `cas/`, so isolated-storage mode still keeps CAS credentials on storage nodes only. The proof is at the credential layer: a test against MinIO shows that an object-store client built from the frontend's configured history credential is refused a GET and a PUT of a `cas/` key, and goes red with the cold-store credential. The history writer's prefix-scoped store also refuses a `cas/` key, as defence in depth, not as the proof.

## 10. Locality

`storage-and-daemon` nodes keep a local read cache of files they produced or fetched; hashed owners stay authoritative and the cache is never consulted for correctness. **One cache, not two:** on `storage-and-daemon` and `all-in-one` nodes the daemon's input cache and the node's local storage (its shard plus this local read cache) are the same cache on the same SSD, never two copies of a blob. Every cache of today stays: the daemon input cache, shared read-only trees, the hot store, the cold store and the action cache. With 6 nodes and R=2 about 2/3 of input bytes are remote even there **[I]**; at 10 GbE that is fine, and SATA SSD is the tighter limit. Each daemon exports its local and remote byte fraction so locality-aware claim ordering later has data.

## 11. Deploy shapes and operation

- **Single box.** `kbf all-in-one --data-dir ./kbf-data`: one process, `file://` cold store, SQLite state store, no LB, no formation step.
- **Our farm.** Frontend on the client box (1 GbE) with a SQLite state store, six `storage-and-daemon` Broadwell nodes (10 GbE), two `daemon` Macs (1 GbE); one CA, `--frontend`, certificate flags and `--advertise-addr` on each node. The single frontend is a scheduling single point of failure; running actions finish.
- **Cloud.** Frontend ASG behind an LB, Postgres state store, storage on local-NVMe instances, S3, a daemon ASG scaled on queue depth. Storage scale-in is a drain, not a kill; storage is not autoscaled on a metric in v1.
- **Frontend handover.** (Units, command and what clients see: `frontend-handover.md`.) `kbf frontend --handover-socket` lets a new process take over a running frontend on one machine without failing a build (`kbf_server::handover`). The new process asks over the control socket (mode 0600, the same user: `SO_PEERCRED`); the serving one refuses a different listen address or state file, and otherwise offers its three listeners as `SCM_RIGHTS` and the store's generation (it goes on serving). The new one adopts each socket after checking it listens where its own flags say (so the TLS and plain-text rules of the flags still describe what is served), builds what does not need the store, and says `ready`. Only then does the serving one stand down: it stops accepting REAPI calls (GOAWAY; a call that raced it is UNAVAILABLE, which Buck2 asks again), waits a bounded time for the calls between their request and their queueing and the results being taken, fences its farm, ends the daemons' streams and stops the worker and operator listeners, and gives up the state store (the writer stops and the file lock is released; the reader stays). The new one then takes the next generation, expecting exactly the one it was told, keeps the epoch (the daemons that reconnect keep the runs of their leases), serves from the sockets it holds (connections that arrived meanwhile wait in their backlog) and sends `READY=1` to the service manager once its probes pass. The old one answers the Execute and WaitExecution streams it kept open from the `done/` rows the new one writes, shows an operation executing once it holds a lease, answers UNAVAILABLE for an operation the store has no trace of for three looks in a row (never leaves a caller waiting), and exits when its REAPI connections have closed. Without a state store, or without the flag, a restart drains as before.
- **Growth.** Adding a node is one command with no edit on existing nodes. The README is the smoke test.
- **Operation.** `/metrics`, `/healthz`, and `/readyz` meaning store reachable and map loaded. Metrics: queue depth by platform, claim wait, idle CPUs, under-replicated blobs, handoff backlog, hot-store fill, flush lag, lost blobs. `Hello` and the map carry protocol versions (`min_protocol`); store schema changes expand first and contract a release later so rollback is safe; upgrade order is store, storage one at a time, frontends, daemons.

## 12. Failure modes

- **Frontend dies:** the LB drops it; daemons re-resolve DNS and reconcile; clients re-attach by operation name; a claim never delivered is requeued.
- **Store unavailable:** no new claims; running actions continue; grace on recovery; a failover bumps the epoch.
- **Daemon partitions past expiry:** duplicate run, result fenced, outputs idempotent.
- **Storage node dies:** reads from the replica, one-copy writes with a durable repair record, drop from the map after the dwell. Both owners of an unflushed blob lost: loss mark, AC miss, rerun.
- **Cold store down:** flush backs off, hot tier absorbs, then writes refuse (`cluster.md` 4.4). UI history buffers within its memory bounds, then drops and counts; builds are unaffected (`ui-data.md` 5.2).
- **Handover fails.** Before the new frontend says `ready` the serving one is untouched (the new one exits, or its connection drops). After it has released the store the serving one cannot take it back: if the new one dies there it is started again as any start is, which bumps the epoch (the daemons run their leases again), and the old one still answers its callers from the rows the new one writes. A crash of the old one mid-handover ends its streams, as any crash does; the new one proceeds, because the kernel released the file lock.
- **Client box dies (v1):** the whole control plane is down; accepted.

## 13. Phased plan (each PR lands a test that fails first)

- **P0** This document; `cluster.md` marked superseded.
- **P1 `kbf-state`.** Trait, fake, SQLite backend, suite C1 to C7; scheduler writes through it. Gate: the store-commits benchmark (group commit at 1500 writes/s, commit latency within the C7 bound) passes before P1 merges. Red: non-linearizable fake fails C1; caller-clock expiry fails C4; restart replays acknowledged claims; stale `complete` rejected; a second frontend on the same SQLite file refuses to start; `--state-store` is rejected when no compiled-in backend claims its scheme, or when several frontends are configured with SQLite. The store-commits gate runs with a synthetic background load (about 80 rows/s in background transactions of at most 256 rows, plus a 512 rows/s phase standing in for the UI's day-close fold, `ui-data.md` 5.2) beside the 1500 foreground writes/s, and asserts that the background rows were applied at the offered rate (C7); `ui-data.md` U3 reruns it with the real history writer.
- **P2 `kbf-storage`.** Cut `cache/*` out; `storage` mode; one-member map; `storage-and-daemon` with the in-process `Cas`. Red: the REAPI conformance suite passes through a separate storage process; on `storage-and-daemon`, an input fetched by the daemon is stored once on disk, not twice (the one-cache rule).
- **P3 Membership and map.** Register, heartbeat, `Welcome.map`, `MapUpdate`, rendezvous, dwell, `STALE_MAP`. Red: removing a member remaps only its blobs; TTL expiry on a fake clock.
- **P4 Sharding, R=1.** Router, per-owner Segments. Red: a blob exists only on its owner.
- **P5 R=2.** Chain replication, repair records, one flusher, loss marks, AC ordering. Red: kill an owner mid-build and it finishes; AC write refused with a missing closure blob.
- **P6 Rebalance.** Join, drain, previous-map read, handoff, S3 footer scan. Red: a join mid-build never yields a false miss.
- **P7 Security.** Storage-side identity, grants, uid separation, isolated-mode test.
- **P8 Cutover.** New frontend and storage run against the same MinIO while the old server serves; storage rebuilds its index; daemons flip by pointing `--server` at the frontend name; rollback is flipping back (cold store append-only, new keys carry node ids).
- **P9 Several frontends.** Batch pull-claim, `session_gen`, grace, poison cap, `WaitExecution` from a second frontend. Red: two claimers, one winner; a result committed on B reaches a waiter on A. Fake clock and deterministic interleaver. P9 runs against the fake store only; the real two-frontend Postgres test is in P10.
- **P10** Postgres backend passing P1's suite, including the C5 interleaved-commit test; a real two-frontend test on Postgres (two claimers, one winner; a result committed on B reaches a waiter on A); mixed-version tests; the UI pages (built after `ui-data.md` U3 on the single frontend) proved against Postgres and two frontends.

Before P1 merges, benchmark store commits per second with group commit at 1500 writes/s, with P1's synthetic background load. Before P4 and P5, benchmark on the farm (none run yet): no-op action latency p50/p99, proxy MB/s at R=1 and R=2 on the 1 GbE box, AC lookup latency, PUT bytes with one flusher versus two.

## 14. Existing documents and PRs

- **`cluster.md` (#744).** Carries over: 3.1/3.2, 4.3, 4.4, the `Takeover` epoch idea. Replaced: 5.2, 5.3, 6.
- **#750:** superseded; no-vote and `drain`/`undrain` are void with the Raft design; formation records are void.
- **#751:** leader tracking, fencing and leader finding are void; the REAPI proxy, UI scope, registration and mTLS flags carry into a trimmed frontend doc.
- **#746:** registration shape and injectable-clock registry reused; xDS dropped. **#747:** void with the Raft design. **#743:** compatible with the in-process `Cas`; rebase on P1 and P2.
- **`deployment-topology.md`** and `storage.md` 432-460 describe the opposite and need a rewrite after P2.

## 15. Decisions

All decided by the owner; nothing is open.

1. **State store.** An interface: a backend is any implementation of the `kbf-state` trait that passes the conformance suite (4.1); SQLite and Postgres are built first, chosen by `--state-store=sqlite:///path` or `--state-store=postgres://...` (amended by the owner, 2026-10-16). `all-in-one` always uses SQLite (a file beside its data). A single `frontend` (for example on one client box) uses SQLite, or Postgres if the operator prefers. Several frontends behind a load balancer require Postgres. A frontend on SQLite takes an exclusive lock on the file; a second frontend on the same file refuses to start. There is no built-in replicated store of kbf's own, no Raft, and no `state` mode; etcd, Redis or a cloud service would be a backend only by passing the suite.
2. **Execution semantics.** At-least-once execution with fenced results.
3. **Write durability.** Acknowledge when both copies exist, by chain replication over the fast fabric; degrade to one copy only with a durable repair record.
4. **The six Broadwell nodes.** Separate uids for storage and actions on `storage-and-daemon` nodes, with a test that an action cannot read the cold-store credential; trusted-network mode otherwise.
5. **Placement.** Single-frontend scheduler in v1; per-frontend pull-claim only when a second frontend exists.
6. **Closure and rebalance.** Write-time closure check plus a result-blob present-check and loss marks on read; read under the previous map with background handoff.
7. **Frontend placement on our farm.** On the client box for now; move it to a 10 GbE node if the proxy benchmark shows the NIC is the limit.
8. **Caches.** Every cache of today stays: the daemon input cache, shared read-only trees, the hot store, the cold store and the action cache. On `storage-and-daemon` and `all-in-one` nodes the daemon's input cache and the node's local storage (its shard plus its local read cache) are one cache, never two copies on one SSD (see section 10).

## Appendix: points considered and rejected

- **Writer sends to all R owners:** halves upload on 1 GbE; chain replication instead.
- **Both replicas flush:** correct but doubles PUT bytes; one flusher.
- **Three frontends as store members:** breaks scale-in and statelessness.
- **Storage nodes reading the store:** wider attack surface; the frontend pushes the map.
- **Per-lease heartbeats and per-daemon queue scans:** store write and read cliffs; per-session renewal, indexed partitions, batch claims.
- **`LISTEN/NOTIFY` as the feed:** not queued for disconnected listeners, not delivered through transaction-mode poolers, and it serializes commits; polling baseline.
- **A built-in Raft store for leases:** not offered. Redis, etcd or a cloud key-value or database service only as a `kbf-state` backend that passes the suite (4.1).
- **Any-owner AC hit with W=1 and no loss marks:** unbounded missing-output risk; write ordering and loss marks added.
- **Certificate alone admits a hash owner:** `joining` and `catching-up` states added.
- **Lease-id fencing alone:** enough for one frontend only; `session_gen` in P9.
- **Asking the replica on a FindMissing miss:** an extra round trip on every genuinely missing blob.
- **Unlimited read-repair:** turns reads into writes; rate-limited.
