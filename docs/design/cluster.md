# One binary, three subcommands, two modes

> **Superseded by [architecture-v2.md](architecture-v2.md).** kbf is now four parts (frontend, state store, storage, daemon) with a stateless frontend behind a load balancer, digest-sharded storage and no elected leader, so the leader-elected scheduler (5.2), the Raft join protocol and voter selection (5.3) and Envoy routing (6) are replaced. Carried over: the two tested modes, trusted-network and isolated-storage (3.1, 3.2), peer fetch, the flush race and hot blobs (4.3), `TierDown`/`TierUp` with the dwell and `--hot-store-abandon` (4.4), and the `Takeover` epoch idea. The rest of this file is kept as history.

Status: design, nothing built. Revised again on 2026-10-15 for the owner's decisions on subcommands, discovery and staged routing, and for the answers to the open questions (section 10). Labels: **[F]** fact checked against `main` while writing this revision, **[F#744]** carried from the first draft of this document and not re-checked, **[I]** inference, **[D]** owner decision (2026-10-15), **[V]** verify before building on it.

This revision replaces the roles, data-path and leader-only-upload parts of the first draft. It does not modify `ha.md` (PR #303); section 11 says how the two relate.

## 1. Decisions

- **[D]** One `kbf` binary with three subcommands. There is no roles system.
  - `kbf server`: the server half only, a storage node. Scheduler-capable, hot store on local SSD, holds object-store credentials, flushes to the cold store, joins the cluster. It builds nothing.
  - `kbf daemon`: the build node only. No server half, no listener, no Raft directory, no hot store and no credentials. It finds the cluster through `--join` and connects to server nodes.
  - `kbf all-in-one`: server and daemon in one process. The daemon talks to its own server half through the in-process data and control path, with no TLS and no network.
  - This replaces `--build`, `--non-voting`, `--daemon-only` and `--leader-candidates`. A "server node" below means a `server` or `all-in-one` process.
- **[D]** `--electable` marks a server or all-in-one node as eligible to vote and lead. It is refused with `kbf daemon`. A server or all-in-one node that is not electable is a full non-voting member: it serves, stores and (all-in-one) builds, and never leads.
- **[D]** No host lists in configuration. `--join=<addresses or a DNS name>` names a few seed members; any one reachable is enough and stale entries are skipped. Membership lives in the Raft configuration, so the cluster is its own directory: a joining node asks any member and learns the membership and the current leader. The first node is started once with `--bootstrap` (explicit, to prevent two clusters). The voter count is a small fixed cluster setting (default 3, may be 5), and the leader promotes electable members to keep it, replacing dead voters; with 100 servers there are still 3 to 5 voters. The deploy tooling renders the seed list from the fleet registry; nobody types hosts (section 5.3).
- **[D]** Client routing is staged (section 6). Stage 1, the cutover, needs no kbf code: Envoy has a static list of the electable servers, rendered by the deploy tooling (3 to 5 entries), with an active health check about every second on each server's leader-ready check (`/readyz`), and outlier ejection. All client traffic goes to the one server that passes. Stage 2: kbf serves Envoy's endpoint discovery (xDS/EDS) naming the current leader, so Envoy follows membership changes without a config edit; the health check stays as the guard against a stale answer. Servers register by joining the kbf cluster, Envoy learns from the cluster, and callers know only Envoy's DNS name. An optional floating IP, held by the leader, serves simple single-subnet sites. The load balancer never discovers servers on its own.
- **[D]** Two modes, both always supported and both tested: *trusted-network* (every node is `kbf all-in-one`, three electable) and *isolated-storage* (storage nodes are `kbf server --electable`, build nodes are `kbf daemon`). They are two configurations of the same binary. **Nothing in the code branches on the mode**: it branches on the subcommand, on `--electable`, and on which implementation of an interface a node holds. A branch on "mode" is a design bug.
- **[D]** The same interfaces serve both modes: daemon to server (data plane and control plane, each with an in-process and a remote implementation and one conformance suite), and server to server (Raft replication, leader discovery, peer blob fetch).
- **[D]** Kept from the first draft: a leader-elected scheduler on a Raft log among the voters; clients are routed to the leader; unflushed hot blobs may be lost with a node; admission by CPUs with the memory ladder; flags, not environment variables; tests never depend on timing; no compatibility with earlier formats.
- **[I]** Stages. Stage 1 elects one leader with a strictly increasing epoch; queue and leases stay leader-local, so a failover loses them (as a leader change already does, `scheduler.md`). Stage 2 replicates the scheduler (PR #303's `FarmMachine`).
- **[I]** The voter count is mutable after bootstrap (`--voter-count` at bootstrap, `kbf cluster set-voters` afterwards). The owner said "small fixed cluster setting"; the mutable part and both names are my inference; the mutable count itself is decided (section 10, item 13).
- **[I]** Membership changes are single-server changes through the Raft configuration (add a non-voting member, promote, remove), one at a time. Non-electable members are Raft learners from their first join; they receive the log but in v1 forward lookups and commits to the leader (section 5.3).
- **[D]** Scaling note: one leader schedules everything. Splitting scheduling per pool or platform is future work and is not designed here.

## 2. The two modes side by side

| | Trusted-network mode (today's shape) | Isolated-storage mode (future) |
|---|---|---|
| Nodes | Every node is `kbf all-in-one`. Six today, each with MinIO. Three (or five) are `--electable` and become the voters; the rest are non-voting members. | Dedicated storage nodes are `kbf server --electable` (or non-electable), each with MinIO. Every build node is `kbf daemon`. |
| Credentials | Object-store credentials on every node. | Only on storage nodes. |
| Security boundary | The network (for example a tailnet). A stated, accepted risk (section 3.1). | No credentials on build nodes, plus a network policy making the object store reachable only from storage nodes (section 3.2). |
| Daemon to server, data | In-process, to its own node's server half (`all-in-one`). | Remote, to a storage node. |
| Daemon to server, control | In-process while its own half is the leader; otherwise remote to the leader. | Remote, to the leader. |
| Where a build's outputs sit | The building node's hot store. | The hot store of the storage node the daemon uploaded to. |
| Who fetches from whom | A node needing a blob fetches it from the holder, or from the cold store once flushed. | Daemons fetch from storage nodes. Storage nodes fetch from each other or the cold store. |
| Loss of one node | May lose unflushed outputs and a build slot. | May lose unflushed outputs of a storage node. A build node holds no blob; its loss costs leases only. |

The data path is one mechanism (section 4). Only the transport between a daemon and a server half differs.

## 3. Security

### 3.1 Trusted-network mode: the risk, stated plainly

Every node holds object-store credentials and every node runs untrusted build code beside them. A container escape, or a malicious action, on any one node yields credentials that can read, overwrite and delete the whole cold store, which is the build cache. Any process that can reach a node's ports can speak to that node. Mutual TLS between nodes stays on, but its certificate authority and keys live in the same network, so it protects against accidental cross-talk and not against a hostile process on the network or a compromised node. Anyone with a shell on any node holds the credentials for the whole store.

This is a stated, accepted and documented risk, for organizations whose network (for example a tailnet) is already the trust boundary. `SECURITY.md` and the deploy guide say so in these words and never call the mode hardened. The design neither weakens nor strengthens it, with two cheap exceptions that are required, because they remove a hole that costs nothing to close:

- **Action isolation on an all-in-one node.** An action is a child of the all-in-one process. It must not inherit the server's credentials. Requirements: the action environment is built from an allowlist, never inherited; credentials are read from a file named by a flag (argv is world-readable, and a file is not copied to children), opened by the server half only. Running actions under a uid other than the server's, so that `/proc/<ppid>/environ`, the credential file and process memory are unreadable, is the deployment guidance and a question for the owner (question 3). Without a separate uid the boundary is stated as "one action can read the credentials", not hidden. Test: a canary variable and a canary credential file in the server's environment never appear in an action's environment or command line, and with the separate uid the action cannot open the file. [I]
- **No flag drops inter-node TLS.** A second way to run the same link is a second code path to test. Rejected in v1 (appendix).

### 3.2 Isolated-storage mode: the boundary, and what it does not defend

Invariants:

- **I1. Credentials live only on server nodes.** Flag parsing refuses any store, credential or cold-store flag together with `kbf daemon`. A flag that would give a daemon node a credential is an error, never ignored. The invariant must hold at runtime, not only through linking, because the one binary links both halves. Today the daemon crates name `kbf-objstore` only as a dev-dependency [F: `crates/kbf-daemon/Cargo.toml`, `crates/kbf-transfer/Cargo.toml`].
- **I2. A daemon node starts and builds with no credentials present, and never uses them if they are.** It does not construct an object-store client on any code path. Inputs and outputs go through a server node. Ambient credentials count: provider-standard variables, credential files, and instance-metadata or workload-identity endpoints.
- **I3. The object store, and the instance-metadata endpoint, are reachable only from where policy allows.** The object store is reachable only from server nodes; the metadata endpoint is unreachable from build nodes. This is network policy, not code. Code assumes it and the farm drill proves it.
- **I4. The daemon protocol exposes no credential and no store location.** No operation returns a credential, a presigned URL or a cold-store URL. Errors a server returns to a daemon carry a code from a closed enumeration and a fixed message, never text from the store client.
- **I5. Certificate classes are separate.** Daemon, Raft and peer-fetch identities carry distinct classes in the certificate. The Raft listener and the peer listener refuse a daemon certificate; the worker listener refuses a server certificate used as a daemon. [I]

What isolated mode does and does not defend. It defends the object store's credentials and direct access: compromising a build node gives nothing to steal from it. It does **not** make a compromised build node harmless. An escaped daemon still holds a daemon certificate and can:

- read any blob whose digest it knows or guesses, through a storage node (reads are not tied to a lease today [V]);
- write blobs, which are content-addressed and verified, so a bad blob is stored under its own digest and harms no one else's;
- write action results, which is the serious one: a daemon can write an action-cache entry naming blobs it chose, and other nodes would trust it. Clients never write the action cache [F: `docs/reapi-auth.md`, "Where kbf differs"]; only a daemon does.

Mitigations, in order of cost:

1. **Result writes are tied to a lease [V].** The server accepts an action result from a daemon only for the action digest of a lease that daemon holds at the current epoch. A daemon then cannot write entries for actions it was not given. It can still lie about the result of its own action; that trust in the hermetic execution of its own lease is the remaining stated risk. Required before isolated mode is called supported.
2. **Per-lease read authorization** (a daemon reads only the digests in its leased input closure and the outputs it is producing). Costs a closure lookup per read. Stage 2, and a question for the owner (question 4): until it lands, the docs state that a compromised build node can read any blob it can name.
3. Distinct certificate classes (I5), so a build node cannot reach the Raft, peer or operator listeners.

### 3.3 Tests that prove the invariants

Each test is red first and has a mutant that must turn it red. Tests of absence are paired with tests of presence, because a sandbox with nothing in it cannot catch code that reads credentials only "if present".

- **S1, flag refusal (I1).** `kbf daemon` together with each of `--cold-store`, `--hot-store`, `--raft-dir`, `--advertise`, `--electable`, `--bootstrap` and each credential flag exits non-zero naming the flag. Mutant: ignore the flag.
- **S2, construction guard (I1, I2).** In a test build the daemon path is given an object-store factory that panics when built or called. A daemon node completes a full build through a server node with the factory never called. Mutant: the daemon builds the store at startup.
- **S3, absence (I2).** A daemon node runs with an empty environment, an empty home directory and every credential-file path failing to open. It starts, connects, runs an action and uploads its outputs. Control run: a server node in the same sandbox fails to start, which shows the sandbox really holds no credentials.
- **S4, presence (I2).** The same run, with canary credentials in the environment and in a file the node could read, for a daemon node and for a trusted-mode all-in-one node. The canary must never appear in an action's environment or output, in logs, in an error string, in any RPC frame between nodes, or in a request to the fake store; and the fake store sees zero requests from the daemon node. Mutant: the daemon reads the variable. A canary string misses transformed leaks, so the error-text check (S6) is an exact-equality one.
- **S5, no object-store traffic from a build node (I2, I3).** In the simulator the fake object store records the caller identity of every request. Under a full isolated-mode build, requests come only from server nodes, and the fake does record the storage nodes' requests (negative control). Mutant: a daemon-side direct fetch. The identity is declared by the test, so S5 guards the code and not the network; the farm drill (S8) is the only proof of the network rule.
- **S6, error surface (I4).** For each store failure the fake can inject, the text a daemon receives equals a fixed expected string for its code; the test compares exactly. A second test lists the daemon-facing RPC methods and rejects any whose response type has a field carrying a URL or credential. The list check cannot see behaviour, so a review rule for new RPCs says the same.
- **S7, certificate classes (I5).** A daemon certificate presented to the Raft listener and the peer listener is refused; a server certificate used on the worker listener as a daemon is refused. Mutant: accept any certificate from the CA.
- **S8, farm drill (I3).** In isolated topology: apply the policy, run a real build to success, then from a build node try to open the object-store port and the metadata endpoint and require refusal or timeout; from a storage node the object-store connection must succeed. The build alone proves nothing, because a missing policy leaves a green build; the failing probe is the control. A held script prints the policy for the owner to apply.
- **S9, dependency lint (I1).** `layering.rs` gains a normal-dependency rule: no daemon-side crate names `kbf-objstore` or the S3 backend. This is a tripwire. It does not count as proof, because the one binary links both halves; S1 to S4 carry the weight.
- **S10, trusted mode states its risk.** The start line and `/readyz` of a trusted-mode node include the facts `credentials: present` and `boundary: network`, derived from configuration (a server node holding credentials is "present"; the boundary is "network" whenever any `all-in-one` node holds them). It is a string match and guards only against silently presenting the weaker mode as the stronger one.

## 4. Data path

### 4.1 The index already exists

No new blob-location index is needed. The replicated state machine holds it [F: `storage.md`, `write-path.md` section 6, `crates/kbf-front/src/cache/tier.rs`, `cache/flush.rs`]:

- the CAS index: digest to `Location { store, object, offset }`;
- the staged table: object to tier, committed length and state;
- `AddTier { server, tier }`: which server owns which tier, and `AllocEpoch`, a new writer epoch at every start;
- `PutStaged` (an acknowledged group of blobs), `ObjectFlushed` (after the PUT and footer read-back), `FlushConflict`, `ObjectUnreachable { Missing | Corrupt }`;
- `BlobAnswer::Present { location, staged_on }`, plus `Unavailable` and `Absent`.

So the replicated blob-location index is the Raft log that stage 1 adds anyway. The design adds around it: each server node commits its own tier and takes its own writer epoch through the leader; a peer service to read a staged block; `TierDown` and `TierUp` for dead holders (4.4); and the read rules of 4.3. The first draft's leader-local index, rebuilt from node reports after a failover, is dropped: after every election it would be empty and correctness would depend on report timing.

The leader is on the metadata path only: one small commit per upload group (digests and offsets, a few dozen bytes per blob, group-committed). Bytes never pass through it. Two costs are stated: a leader outage stalls uploads, because an upload is acknowledged only after its commit applies; and log and snapshot growth is one entry per upload group, for which PR 0 produces a size estimate and the compaction budget [V].

### 4.2 Upload (both modes)

1. The daemon (or an Envoy-routed client) sends blobs to a server half. In trusted mode that is the daemon's own node, in-process. In isolated mode it is a storage node over mutual TLS. If the chosen server node fails mid-upload the daemon retries the whole group against another; nothing was acknowledged, and a repeated blob is harmless because storage is content-addressed.
2. The server half verifies the digests, appends to its tier block and syncs it [F: `write-path.md` 6.1].
3. It commits `PutStaged { tier, ... }` through the leader. A non-leader forwards the command. The caller is answered after the commit applies on the forwarding node.
4. A background flusher seals the block, re-hashes each record, PUTs the object and commits `ObjectFlushed`. From then on any server node can read the object from the cold store.

An acknowledged blob is on one node's synced disk and in the committed log, not in the cold store [F: `write-path.md` 6.3]. Losing that disk before the flush loses the blob. That is the accepted loss [D]. A minority-side or deposed leader cannot commit `PutStaged`, so it cannot acknowledge: this closes the acknowledged-but-unregistered window.

Forwarded commands are at-least-once across a leader change, so each has a stated idempotency rule:
- `AllocEpoch`: every apply yields a new epoch, so two applies never hand out the same one. A node that never saw the reply asks again and uses only the epoch it receives. Skipped numbers are harmless.
- `AddTier`: keyed by `(server, tier)`; a repeat is a no-op [F: the tier opens register-if-new, `cache/tier.rs`].
- `PutStaged`: an already-applied command is detected by the existing "a log can apply a command and report failure" handling [F: `cache/tier.rs`, comment on answering a failed range]. Whether the forwarded path keeps that guarantee across a leader change is **[V]** and gets its own failover test (PR 5).

### 4.3 Read and peer fetch (both modes)

A server half serving a read asks for `BlobAnswer`: from its applied state if it is a leader, else from the leader (a small lookup RPC; non-voting members and voter followers use the same path in v1 so there is one code path). Then:

| `staged_on` | Source |
|---|---|
| This node's tier | The local block file. Served without asking anyone, when the caller already knows the digest, because the bytes verify against it. |
| Another node's tier | **Peer fetch**: a ranged `ReadBlock { object, offset, len }` to the holder. The receiver hashes the bytes against the digest; a mismatch counts as `Unavailable` and is never cached. |
| none (flushed) | A ranged read of the cold store. The holder may read its own `.seg` file. |

- **Bounded fallback order** on a peer fetch: the holder, then the cold store if the object has flushed, then report missing. A fetch has a timeout, and a timeout is the same as unreachable.
- **No leader known.** A lookup that needs the leader answers `UNAVAILABLE`, never a stale "absent". Local-tier reads of a known digest do not need it. Builds still stall during an election because nothing is scheduled; that is accepted.
- **The peer service** sits on its own mutual-TLS listener, apart from the worker and REAPI listeners. The server identity is bound to the certificate, and daemon certificates are refused (I5). It is read-only and serves only staged blocks and `.seg` files the node holds.
- **Flush race.** A reader may be told "staged" while the flush completes. The holder then answers "flushed, use the cold store"; the reader retries once against the cold store and never commits `ObjectUnreachable { Missing }` for an object that was staged when it asked [F: `write-path.md` 6.4 and its flusher rules].
- **Daemon input fetch.** A daemon asks its server half, which does the lookup and streams the bytes. In trusted mode the daemon never calls a peer itself, so the daemon-to-server interface is the same in both modes.
- **Hot blobs.** A popular output, such as a toolchain tree, is served by its one holder until it flushes. v1 adopts the cheap remedy: a node that fetched a blob from a peer keeps it in its input cache (the separate cache of `write-path.md` 6.6, never a holder and never in the index). Replicating blocks to a second tier before the acknowledgement is already designed [F: `write-path.md` 6.3, "later, with three servers"]. It doubles storage-node write load in isolated mode, so it is a cost and a flag, not v1.
- **Locality** (stage 2): the leader's placement can prefer the node that holds an action's largest input, since the index already says where bytes are. A soft preference, measured before it is kept.

### 4.4 Loss of a node

The unflushed blobs of a lost node are lost [D]. The system says so rather than serving around it.

- **Down, disk intact (restart).** The tier recovers staged objects against the committed table [F: `write-path.md` 6.4]. Until the node is back its objects are unreachable from peers.
- **Declared down (new).** When a holder has been unreachable beyond `--tier-down-after`, or an operator says so, the leader commits `TierDown { tier }`. The state machine then answers `Unavailable` for every blob staged on that tier. The leader's clock only triggers the proposal; the committed entry is deterministic and no replica compares clocks. The trigger reads an injectable clock, so the simulator drives it in logical time. To stop a flapping holder from writing a log entry per flap, a flip needs a minimum dwell since the last one, and the flip count is a metric. Without `TierDown`, a dead holder's blobs read `Present` with nobody able to fetch them, for an unbounded time.
- **Writes to a down tier (the rule).** `PutStaged` naming a down tier is **rejected** with a distinct error. A holder that was partitioned and alive, on reconnecting, must first finish its own recovery check, take a new writer epoch, and commit `TierUp { tier }`, then retry. `TierUp` never brings back an object already marked `ObjectUnreachable`. A blob re-uploaded elsewhere while the tier was down is committed at a new location, which wins; the old block becomes an orphan the holder's restart sweep deletes [F: `write-path.md` 6.4, orphans]. An old copy is never wrong, only unreferenced. A minority-side holder cannot commit anything, so the rule matters only after the partition heals.
- **Disk gone.** `--hot-store-abandon=<id>` [F: `write-path.md` 6.4] marks the tier's objects `ObjectUnreachable { Missing }` permanently. A re-upload heals each entry.
- **What clients and actions see.** `FindMissingBlobs` reports `Unavailable` blobs as missing, so clients re-upload. `GetActionResult` is a miss when a closure blob is `Unavailable` [F: `storage.md`, "three answers"]; its per-output lookup cost is measured and the answer cached per applied index. An `Execute` with an `Unavailable` input fails with the REAPI missing-blob precondition. An intermediate output named only by an action-cache entry is recomputed when the entry misses. Whether the build client re-executes on a download failure is **[V]** (PR 0).
- `TierDown` and `TierUp` are new log commands: they raise the log format and the machine version and need the version gate of PR #303 section 4.7. Red first: a blob staged on a down tier is served as `Present`.

### 4.5 Isolated mode, same mechanism

Storage nodes are the only holders, so every blob is on a storage node's hot store or in the cold store. Build nodes hold an input cache and nothing else. A storage node's loss is a section 4.4 case; a build node's loss loses no blob. All build I/O crosses the network; storage-node NICs and disks are the first bottleneck, and the remedy is more storage nodes, not code. The set of storage nodes a daemon uses: in v1 the server nodes it learned from the cluster (5.3), or the voters if the daemon is told to prefer them (question 5); later the leader's `Welcome` carries the server-node list as a hint [V: protocol field]. With no leader, a daemon node uses the membership it already learned, or the seeds; scheduling is stalled anyway.

## 5. Node resolution, discovery, leader and epoch

### 5.1 Subcommands and flags

```
# server: storage node, holds credentials, never builds (isolated mode)
kbf server --node-id=ID --advertise=ADDR --join=SEEDS --cluster-id=CID --electable \
    --raft-dir=... --hot-store=file://... --cold-store=s3://...

# all-in-one: server and daemon in one process (trusted mode)
kbf all-in-one --node-id=ID --advertise=ADDR --join=SEEDS --cluster-id=CID --electable \
    --raft-dir=... --hot-store=file://... --cold-store=s3://... --reserve-cpus=N

# daemon: build node, no server half, no credentials (isolated mode, Macs)
kbf daemon --join=SEEDS --cluster-id=CID --reserve-cpus=N

# the first node of a new cluster, started once
kbf all-in-one --bootstrap --voter-count=3 --electable --node-id=ID --advertise=ADDR ...
```

`SEEDS` is a few addresses or a DNS name, comma separated; it is the only host input anywhere (5.3). Resolution is a pure function of (subcommand, `--electable`), and nothing else decides a node's type:

| Resolved | Rule |
|---|---|
| Server half present | subcommand is `server` or `all-in-one` |
| Daemon present | subcommand is `daemon` or `all-in-one` |
| Eligible to vote and lead | server half present and `--electable` |
| Non-voting member | server half present and not `--electable` |
| Builds | daemon present |
| In-process data and control path | `all-in-one`: its daemon talks to its own server half in-process, no TLS, no network |

The flags `--build`, `--non-voting`, `--daemon-only` and `--leader-candidates` do not exist. `--electable` only marks eligibility. Whether a node holds a vote now is a fact of the Raft configuration (5.3), shown in `/readyz`, and never a flag.

Refused at startup, naming the flag:

- `--electable` with `kbf daemon`.
- `kbf daemon` with any of `--advertise`, `--raft-dir`, `--hot-store`, `--cold-store`, `--bootstrap`, `--voter-count` or a credential flag (S1).
- A node with no configuration yet (and every daemon) that has neither `--cluster-id` nor `--accept-any-cluster` (5.3); `--cluster-id` with `--bootstrap` (bootstrap generates the id);
- `--join` missing, unless `--bootstrap` is given; `--join` empty or with a duplicate entry; `--join` together with `--bootstrap` (the first node has nobody to join).
- `--bootstrap` when `--raft-dir` already holds a cluster (so a restart with the flag left on fails loudly, and the deploy tooling drops it after the first start); `--bootstrap` without `--electable` (the first voter must be eligible); `--bootstrap` with `kbf daemon`.
- `--voter-count` without `--bootstrap` (it is a cluster setting, set once there and changed later by a committed operator command, 5.3); `--voter-count` of 0 or above 7.
- A server or all-in-one node without `--advertise`, `--node-id`, `--raft-dir`, `--hot-store` or `--cold-store`.

Warned at startup and shown in `/readyz`, not refused: a voter count that is even or below 3 (an even count adds no fault tolerance over the odd count below it; 1 is the single-node pilot shape, tolerating no loss).

Carried from the first draft [F#744] where still true:
- The resolved subcommand, eligibility, current vote status and cluster id are logged and shown in `/readyz`.
- **Cluster id guard.** Bootstrap generates a cluster id, stored in the Raft configuration and carried on every Raft and join message. A mismatch is rejected and reported. A node's first join is checked against an expected id given by `--cluster-id` (5.3). This replaces the candidate-list hash: nobody types a list, so nothing can differ between nodes by typo.
- **Wiped disk.** A node with an empty `--raft-dir` and no `--bootstrap` is not a member and does not vote or campaign. It joins like any new node (5.3), as a non-voting member, and can be promoted again only after it has caught up. `currentTerm` and `votedFor` are durable before any reply. A wiped disk therefore cannot form a second quorum from forgotten votes.
- A failed log append is fatal for that voter.
- The registry of nodes never supplies Raft membership. The deploy tooling reads the fleet registry only to render the seed list.
- The single-server pilot's log is not imported; that server becomes a `kbf daemon` node, or a server or all-in-one node if it has a disk.

### 5.2 Leader and epoch

Carried from the first draft [F#744] and unchanged: a `Takeover` entry per leader term; `Welcome.epoch` is the committed index of that entry (strictly increasing, replacing the wall-clock `process_term()`, which exists at `crates/kbf-server/src/farm.rs:86` [F]); leader-ready means Raft leader, CheckQuorum satisfied and `Takeover` committed; step-down drops all leader-local scheduler state; the daemon refuses a lower epoch and kills leases of a different one; heartbeats stay out of the log; lease deadlines are leader-clock values. `set_leader` having no caller is [F#744]. Stage 2 adopts PR #303. Both stages run in both modes. Prerequisites in `kbf-raft`: PreVote, CheckQuorum, a network transport, single-server membership change (5.3) and, before compaction, InstallSnapshot [F#744].

One leader schedules everything. That is the scaling limit of this design. Splitting scheduling (per pool or platform, each with its own leader) is future work and is not designed here.

### 5.3 Discovery and joining

**The cluster is its own directory.** Membership lives in the Raft configuration: every server and all-in-one node, with its advertise address, its eligibility and whether it votes. No file, flag or registry lists the hosts.

**Seeds.**
- `--join` names a few seed members, as addresses or a DNS name. Any one reachable seed is enough. Stale entries (a decommissioned host, a name that no longer resolves) are skipped, each with a bounded timeout, and retried in rounds with backoff up to 30 s [F#744 connect loop]. The seed list is not a membership list and is not compared across nodes, so it can differ from node to node and be out of date without harm.
- A running member needs no seed to restart: it reads its own Raft configuration from `--raft-dir`. Seeds matter to a node that has no configuration yet (a new server node) and to every daemon.

**Joining protocol (server and all-in-one nodes).**
1. The new node contacts a seed with `Join { expected cluster id, node id, advertise address, eligibility }`. The expected id is the node's stored id if it has one, else `--cluster-id`; a node with neither must be started with `--accept-any-cluster` or it refuses to start (5.1). The deploy tooling knows the id of the cluster it deployed and renders `--cluster-id` with the seeds, so a DNS name that resolves to two clusters cannot attach a node to the wrong one. A member with a different id answers with its id and the joiner refuses. Any member answers: a non-leader replies with the membership it knows, the **configuration index** of that membership, and the current leader's address (or "no leader known, retry").
2. The joiner takes the answer with the highest configuration index among the seeds it reached and confirms it with the leader that answer names: the leader's own answer is authoritative, and an answer the leader does not confirm is discarded. The node then sends the same request to the leader. The leader validates it (cluster id matches; node id not in use by a different address), then commits a configuration change adding the node as a non-voting member, one change at a time.
3. The node receives the log (or a snapshot, once compaction exists, PR 10) and replicates as a non-voting member. It now serves, stores and builds like any member. Non-electable nodes stay here for good.
4. A seed that answers with a different cluster id from another seed makes the node refuse to join and report both ids, even when `--accept-any-cluster` was given (that flag only lets a node with no expectation take the first id it hears; it never overrides a disagreement). Two clusters are never merged by a join.
5. **Removed members.** A removed node learns of its removal from the committed configuration (or from a leader's reply), then stops answering `Join` and membership queries, stops serving peer fetch, and refuses to restart into the old identity: its Raft state is fenced, and reusing its disk requires an explicit wipe or a new node id, after which it joins as a new node (5.1, wiped disk). A removed node that never learned (it was down) may still answer a stale membership with an old configuration index and an old leader; step 2 (highest index, confirmed by the leader) is what makes that harmless, and a stale seed that is a removed member is just another stale entry. Removing the leader makes it step down after the change commits, and the remaining voters elect a new one.

A daemon uses the same seed protocol (including steps 1, 2 and 4), minus the configuration change: it asks any reachable member for membership and the leader, then connects to the leader for the control plane and to server nodes for data (4.5). It keeps the membership it learned in memory for the life of the process and refreshes it from the leader's `Welcome`; after a restart it starts from the seeds again. A daemon pins the cluster id it first learns (or `--cluster-id`) for the life of the process: it refuses a seed, a leader or a `Welcome` carrying another id, so a restart or a stale DNS answer cannot move it to a second cluster and mix leases.

**Voters.**
- The **voter count** is a small fixed cluster setting stored in the Raft configuration: default 3, 5 allowed, set with `--voter-count` at bootstrap and changed afterwards only by a committed operator command (`kbf cluster set-voters`) **[I: mutable setting and both names are mine]**. The command commits the new count as a setting and returns; the leader then converges one single-server change at a time (3 to 5 is two promotions, 5 to 3 is two removals, each started after the previous one committed). A count above the electable supply is accepted and warned in `/readyz`, never rejected. With 100 server nodes there are still 3 to 5 voters.
- The leader keeps the number of voters at the setting by promoting electable, caught-up non-voting members, one single-server change at a time. It does so at start (the bootstrap node alone is a voter; the first two electable nodes to join are promoted). **A new cluster is not fault tolerant until the third voter is promoted:** with one voter nothing can be lost, and with two voters (quorum 2) the loss of either stops the cluster. `/readyz` shows `voters 2 of 3` until then. When a voter has been unreachable beyond `--voter-replace-after` (default 60 s) the leader first promotes a replacement, which must be electable, reachable and caught up (below) at that moment, and only then removes the dead voter as the next and only change in between. The transient voter count is the setting plus one (an even count, for example 4 voters with quorum 3), so the cluster tolerates no additional failure until the removal commits; if the added voter fails before the removal, the dead voter is still removed next, and no other change may start in between. The quorum never shrinks below a majority of the old set mid-change. The trigger reads an injectable clock, as `TierDown` does (4.4); the committed entries are deterministic and no replica compares clocks. Flapping is bounded by a minimum dwell between changes.
- **Promotion bound.** A candidate is promoted only when its match index is within `--promote-lag` entries (default 1000) of the leader's commit index, or it has applied the latest snapshot, for `--promote-rounds` consecutive heartbeat rounds (default 5), and it is reachable now. So a fresh wiped disk cannot become a voter before it has the log. The test for a wiped voter disk uses these two values: a node behind by more than `--promote-lag` is not promoted, and the same node is promoted once it is within it for the stated rounds.
- If so many voters die at once that no leader can be elected (two of three), nothing can promote anyone: scheduling stops and nothing flushed is lost. Recovery is an explicit operator act on a surviving member (documented, tested in the simulator), and is never automatic [I].

**Bootstrap safety.**
- A cluster begins only with `--bootstrap`, started once on one node. Without the flag, a node with no configuration joins and never creates a cluster, so a mistyped seed list or a fleet restart cannot make two clusters.
- `--bootstrap` is refused if `--raft-dir` already has a cluster. Two nodes bootstrapped by mistake have two different cluster ids; a node with an id never accepts a configuration from another id, and a joiner that hears both refuses (step 4).
- The deploy tooling (`kbf-deploy`, from the fleet registry) renders the seed list for every node and the bootstrap flag for the first, once. Nobody types hosts.

**Daemons, in addition.** A node that is not leader-ready ends a `Session` promptly with `UNAVAILABLE`. The connect loop re-resolves every name, tries every address and backs off up to 30 s [F#744], so with a few seeds it finds the leader within a round. Later the `kbf-leader` trailer of PR #303 section 9 shortens the search; it is followed only to an address already learned from the cluster, only if it verifies, at most once per round, with backoff capped at 2 s while leases are held.

**Non-voting members.** They are members of the Raft configuration and receive the log, so they know the membership and the leader without asking. In v1 their lookups and commits (`PutStaged`, `AllocEpoch`, `AddTier`, `TierUp`) still go to the leader, as for voter followers (4.3): no read gate, no staleness bound. The cost is a leader lookup per read of a non-local blob, to be measured in PR 6. Answering lookups locally under a staleness bound (PR #303 section 8) is stage 2, justified only by that measurement. They never lead.

**A node cut off from every member** keeps running actions, holds results and retries; a flag sets a ceiling after which it kills them. A cut-off all-in-one node also keeps serving peer fetch to nodes that reach it and local reads of known digests; it does not schedule or commit.

Raft traffic uses its own mutual-TLS listener and is never behind Envoy.

### 5.4 Relation to the lock-service pattern

kbf embeds what a lock service such as Chubby, ZooKeeper or etcd provides to the systems built on it, inside the binary:

| Lock-service piece | In kbf |
|---|---|
| The lock | Raft leadership: at most one leader-ready node per term |
| The lock's sequence number, used to fence stale holders | The `Takeover` entry's committed index, which is `Welcome.epoch`; daemons refuse a lower one (5.2) |
| The lock file (who holds it, and the group's membership) | The leader's address and the membership, both in the Raft log (5.3) |
| Watches | Members answering "who leads and who is a member", and pushing that to Envoy over xDS (6, stage 2) |

Why embedded: kbf is open source and has to run anywhere, including trusted-network mode with no other infrastructure. An external coordinator would still need its own seeds and its own operators, so it would move the bootstrap problem and add a second system to run and secure.

Future work only, not designed here: an optional external election backend (etcd, or a Kubernetes Lease) behind the same leadership interface, for organisations that already run one. The fencing epoch would still come from kbf's own log, so nothing downstream of the interface changes.

## 6. Client routing

Clients (build clients, CI runners) reach the leader through Envoy. The load balancer never discovers servers on its own. In plain words: servers register by joining the kbf cluster; Envoy learns the servers from the cluster (stage 2) or from a rendered list (stage 1); callers know only Envoy's DNS name and never a server address.

**The safety argument, for both stages.** Only the leader passes the leader-ready check, because the check is Raft leader, CheckQuorum satisfied and `Takeover` committed (5.2). So a list that names non-leaders as well is safe: a non-leader fails the check and is never routed to. The list only has to contain the leader, not to be right about which node it is.

**Stage 1: static list plus the leader check (cutover, no kbf code).**

- Envoy's client cluster is a static cluster listing the electable servers, as rendered by the deploy tooling from the fleet registry (3 to 5 entries, the same nodes the deploy tooling already marks `--electable`). Adding or removing an electable server re-renders Envoy's config and reloads it; that is rare and is the only reason stage 2 exists.
- Active health check on every entry, interval about 1 s, on the leader-ready check (`kbf.leader`, backed by `/readyz`), with a low unhealthy threshold and outlier ejection on top. All REAPI traffic goes to the one entry that passes. With no entry passing (an election, or a lost quorum) Envoy has no healthy upstream and clients get `UNAVAILABLE` and retry.
- `panic_threshold: 0`, so when nothing passes Envoy does not fall back to routing to failing hosts.
- **The bound on a stale answer.** A deposed leader that is cut off keeps passing the check until CheckQuorum fails, up to an election timeout, plus one check interval for Envoy to notice. In that window two hosts can pass at once, and a request that reaches the old one cannot be acknowledged, because the old one cannot commit `PutStaged` or any other write (4.2); it answers `UNAVAILABLE` and the client retry goes to the other host. Reads that reach it are served from its tier and verify against their digests. So the window costs retries and never a wrong acknowledgement.
- Failover time: election timeout (2 to 4 s) plus the check interval and thresholds (about 1 s times 1 or 2) gives under about 8 s of `UNAVAILABLE` that clients retry. The simulator asserts the order of events (election, then the new leader passes the check), not the duration.

**Stage 2: kbf serves xDS/EDS.**

- Envoy is configured with the same seed list as every node and an xDS management cluster over it (details in 6.1). Every server and all-in-one node serves Envoy's endpoint discovery service (EDS). The answer names the **current leader** as the only endpoint of the client cluster, taken from the Raft state. A non-leader answers with the leader it knows; with no leader known, it answers with an empty endpoint set that Envoy treats as "no healthy upstream", and clients get `UNAVAILABLE` and retry. On a leadership change every connected node pushes a new version on the open stream. It needs only that Envoy can reach one seed, so it works for CI runners over a tailnet, where no virtual IP exists, and Envoy follows membership changes with no config edit.
- **The health check stays as the stale-answer guard.** Active health checks on the leader-ready check stay: a stale EDS answer pointing at a node that is no longer leader-ready is caught within the check interval, and the not-ready node answers `UNAVAILABLE`. With one endpoint, a failing check leaves none, and `panic_threshold: 0` then gives "no healthy upstream"; that is the intended behaviour, and it means the health check can only remove the leader, never choose another node. EDS alone chooses.
- Stage 1 and stage 2 differ only in where the endpoint list comes from (rendered file, or EDS); the health check, retry table and bound above are the same, so moving to stage 2 changes no client behaviour.

**Common to both stages.**

- **Optional floating IP.** For a simple single-subnet site, the leader-ready node holds a virtual IP (VRRP-style: add the address and send gratuitous ARP). It is taken after `Takeover` commits and released at step-down or when CheckQuorum fails. Limits, stated: it works only on one layer-2 segment (no cloud networks that block gratuitous ARP); gratuitous ARP is lossy; on a partition the old leader keeps the address until CheckQuorum fails, up to an election timeout, so two nodes can briefly both hold it. Fencing rule: the node removes the address and closes its client listeners **before** its step-down completes, never after; a node that cannot remove the address (the interface call fails) exits. Connections already open to a deposed leader are closed at release; a slow node still answers `UNAVAILABLE`, so clients retry rather than succeed on a stale node. Clients then need no Envoy. It is off by default, needs the address and interface as flags and the capability to configure them, and does not replace Envoy routing. Test: a fake interface in which both nodes hold the address at once, a client write to the stale node gets `UNAVAILABLE`, and the stale node's listeners are closed.
- **One cluster in v1:** the leader only. All client traffic goes to the leader, which serves from its tier, by peer fetch, or from the cold store. Non-voting members are never in the client cluster (in stage 1 the rendered list holds electable servers only). This keeps the first draft's choice and records that it adds client load to the leader's NIC while there is one client box.
- Widening to a second cluster for blob traffic on any ready server node (health `kbf.reads`) is a later change to what the list or EDS returns, allowed only after peer fetch, `TierDown` and the lookup path pass in both modes.
- Retries by method class: idempotent reads (`FindMissingBlobs`, `GetActionResult`, `BatchReadBlobs`, ByteStream `Read`, `GetCapabilities`) on `unavailable`, `connect-failure`, `reset`; `WaitExecution` on `unavailable`, `connect-failure`; `Execute` and writes only when nothing was sent.
- **Cold start.** Stage 1 needs nothing but the rendered list, so a cold Envoy starts routing as soon as one listed server passes the check. Stage 2 adds a static xDS management cluster over the seed list; a cold Envoy start with no seed reachable and no cached config serves nothing until one seed returns, stated plainly. In neither stage does Envoy invent a leader: the static list is never a fallback that bypasses the check, so a non-leader cannot be routed to by being listed.
- **Envoy loses its xDS stream (stage 2).** Envoy keeps serving its last-known endpoints and reconnects to the seeds in rounds. If the leader moved meanwhile, the old endpoint answers `UNAVAILABLE` (not leader-ready) and fails the health check; clients see errors until xDS returns. No data is at risk.
- **Envoy as a single point of failure.** One Envoy is a single point of failure for clients, not for the farm; daemons and peers never go through it. Later, run two stateless Envoys behind the one DNS name. They hold no state beyond their config, so any one serves any request and either can restart or fail. Until then, one Envoy on the client box.
- Planned restarts of the leader transfer leadership in stage 2.

### 6.1 xDS details (stage 2)

State here what an implementer needs; each item has one golden test.

- **Variant.** Aggregated discovery (ADS), state-of-the-world, one stream; only the EDS resource type is served. No delta in v1.
- **Names.** One client cluster, `kbf_leader`, defined in Envoy's static bootstrap with type `EDS` and `eds_cluster_config` pointing at ADS; the `ClusterLoadAssignment` has the same name. One endpoint: the leader's client address.
- **Version and nonce.** The version string is derived from `term` and the leader's node id (for example `term-nodeid`), never from a local counter, so a stale node's answer cannot outrank a newer one: Envoy applies the highest it has seen from whichever node it is connected to, and an answer for a lower term is never sent after a higher one on the same stream. The nonce is a per-stream counter; a NACK is logged with the version.
- **Endpoint fields.** Health status `HEALTHY` for the named leader and an empty assignment when no leader is known; locality unset; no priority levels.
- **Channel security.** The xDS channel uses the same mutual TLS as other client traffic, with a certificate class for Envoy (distinct from daemon and server classes, I5).
- **A non-leader serving EDS** answers from its Raft state with the leader it knows and the term it knows it in; it never invents a leader.

## 7. In-process versus remote

- Two interfaces, `DataPlane` and `ControlPlane`, each with an in-process and a remote implementation, and one conformance suite per interface run over every implementation [D]. Each suite checks: the same inputs give the same outputs and the same epoch behaviour on a leader change; a killed channel ends in the same daemon state; a corrupt blob mismatches the same way.
- **Data plane.** An all-in-one node always uses the in-process one to its own server half. A daemon node always uses the remote one. It never switches on a leader change.
- **Control plane.** The scheduler runs only on the leader. An all-in-one node's daemon uses the in-process control plane while its own half is leader-ready, and dials the leader remotely otherwise. On gaining leadership it opens an in-process session after `Takeover` commits; on losing it the same daemon redials the new leader. **This differs from the plain reading of "control in-process" and is flagged (question 1).** The alternative, a relay in every server half, adds a hop and a second path.
- Main uses a mutual-TLS loopback for the control plane, and only the data plane is in-process [F#744]. Until the in-process control plane lands (PR 11), the leader's own daemon keeps the loopback: correct, but short of "no TLS between a node's own halves". The gap is stated.
- Stage 1: the new epoch kills leases including the in-process ones. Stage 2: running actions survive and are adopted from `Hello`.
- An all-in-one node opens a worker listener to serve remote daemons and peers; the single-node "no worker listener" text of the all-in-one design is amended when that design lands. The one-node farm remains a degenerate cluster: one voter, one `kbf all-in-one --bootstrap --voter-count=1` node (warned, 5.1).

## 8. Failure modes

"Same" means the behaviour and the test are identical in both modes; the test runs in both.

| Failure | Trusted | Isolated | Test (simulator, logical time) |
|---|---|---|---|
| Leader crash | Election; new `Takeover`; the new leader starts passing Envoy's check (stage 2: and EDS is pushed). Its own daemon and tier go with it. Stage 1 loses queue and leases; stage 2 keeps them | Same; no build slot lost, since storage nodes hold none | epochs strictly increase; stage 2: leases survive |
| Minority or isolated leader | CheckQuorum steps it down; it cannot commit `PutStaged`, so it cannot acknowledge | Same | minority leader not leader-ready; no ack |
| Rejoining partitioned node | PreVote stops deposing | Same | term unchanged |
| Two nodes believe they lead | Stale one cannot commit `Takeover`; daemons reject lower epochs | Same | at most one leader-ready per term |
| Flapping | PreVote, stickiness, 2 to 4 s timeout | Same | no term churn |
| Slow follower | Majority commit does not wait | Same | catches up after InstallSnapshot |
| Voter append fails or disk full | Fatal: step down, exit | Same | injected error stops the voter |
| Wiped voter disk | Empty log: not a member; rejoins as a non-voting member and is promoted only after it is within `--promote-lag` for `--promote-rounds` | Same | empty-log node never votes; not promoted while beyond the lag, promoted once within it |
| Cluster id mismatch (bootstrapped twice) | Raft and join messages with another id are rejected; a joiner hearing two ids refuses and reports both; `--bootstrap` refused on a data dir that has a cluster | Same | two bootstrapped nodes never merge; joiner refuses |
| Seed list all down or stale | Members unaffected (they read their own configuration). A new node or a daemon retries in rounds with backoff and starts nothing until a seed answers; stale entries are skipped | Same; daemons keep the membership learned in memory while running | joiner waits, then joins when one seed returns |
| Voter death | Leader promotes an electable caught-up member (transient count is the setting plus one), then removes the dead voter as the next change; the voter count returns to the setting. Two of three dead: no leader, no promotion, operator recovery | Same | dead voter replaced; quorum never below a majority mid-change |
| Fewer electable nodes than the voter count | Runs with fewer voters; warned in `/readyz` | Same | promotion stops at the electable supply |
| Holder lost | Unflushed outputs lost. `TierDown`, then `Unavailable` and re-upload or re-run. Its builds re-run | Same, storage nodes only. A build node's loss loses leases only | holder kill per mode |
| Holder partitioned but alive | Cluster sees `TierDown`; it serves its own daemon locally; on heal it must `TierUp` before `PutStaged` | Same | write to a down tier rejected |
| Peer fetch slow or corrupt | Timeout or hash mismatch: `Unavailable`; fall back holder, cold store, missing | Same | corrupt peer answer refused |
| Holder flaps | Minimum dwell between `TierDown` and `TierUp`; flip metric | Same | no log entry per flap |
| Cold store unreachable | A node builds from tier and peers; flush backs off; uploads fail `UNAVAILABLE` when the tier budget is full [F: `write-path.md` 6.1] | Storage nodes the same; build nodes unaffected | build completes with the store down |
| Daemon cut off from all servers | Holds results, retries, kill ceiling | Same | ceiling kills after the bound |
| No leader known | Lookups `UNAVAILABLE`; local-tier reads of known digests still work. Non-electable nodes need the leader for every non-local read (4.3), so they serve only local digests while it is down | Daemon nodes use the membership already learned, or the seeds | local read during election |
| Credentials leaked from a node | Every node is a source: accepted risk | Only storage nodes; build node holds none | S1 to S4 |
| Compromised build node | n/a (all nodes hold credentials) | Can read by digest and write results for its own lease; not the object store | S5, S7, lease-bound result test |
| Envoy routes to a stale or listed non-leader | Not-ready node fails the check and is never routed to; a deposed leader still passing cannot acknowledge a write and answers `UNAVAILABLE` (bound in 6) | Same | fake-backend routing |
| Envoy cold start, nothing reachable | Stage 1: routes as soon as one listed server passes. Stage 2: no cached config and no seed: serves nothing until a seed returns | Same | cold Envoy, seeds down, then one up |
| Daemon sees seeds of two clusters | Pins the first id; refuses the other and reports both | Same | daemon restart against a second cluster refused |
| Removed member still running, used as a seed | Stops answering membership; if it does answer, its lower configuration index loses and the leader does not confirm it | Same | stale answer discarded; reused disk joins as a new node |
| Envoy loses its xDS stream (stage 2) | Keeps last-known endpoints, reconnects to the seeds; a moved leader gives `UNAVAILABLE` until xDS returns | Same | stream drop, leader move, then recovery |
| EDS served by a non-leader (stage 2) | Names the leader it knows, or an empty set with no leader | Same | follower EDS equals leader EDS |
| Mixed versions | Version gate before `TierDown` | Same | no new entry before the gate |

Surviving one node loss needs 2 of 3 voters; two losses stop scheduling and lose nothing flushed. In isolated mode with storage-node voters, losing build nodes never affects quorum. Client view on a stage-1 failover: queued work is lost, running work is killed, `Execute` streams break. **[V]** How the build client reacts is checked first (PR 0).

## 9. Phased plan

Each PR's first test fails before the change. No test depends on wall time. **Every behaviour test runs in both modes from PR 4a on**: PR 1 and 2 have no cluster to run, so their tests are unit-level; the meta-test starts enforcing at 4a. The testkit offers `Cluster::trusted(n)` and `Cluster::isolated(storage, builders)` (both build clusters by bootstrap and join, never by a host list) and a `both_modes!` macro, and a meta-test greps the behaviour suites and fails on any test not wrapped. Farm drills assert end state, not duration, and run on the trusted farm today and on an isolated topology as soon as one exists.

0. **Spike.** Drive a real build client against a single leader, break the stream, and record the recovery. Confirm `GetActionResult` checks output presence and how the client reacts to a download failure of an `Unavailable` output. Estimate log and snapshot growth per upload group. Measure the lookup RPC cost. Edits this document.
1. **Resolver and refusals,** plus the testkit constructors and the `both_modes!` meta-test. Red first: `--electable` with `kbf daemon`, `--bootstrap` on an existing data dir, a missing `--join` and `kbf daemon` with store flags are accepted today (S1). The old flags are removed.
2. **One binary.** Server and daemon libraries behind one entry point. Red first: a daemon node does not reach a leader reachable only behind a non-leader seed. S2, S3, S4, S9 land here, including the action-environment scrub for all-in-one nodes.
3. **Raft safety in `kbf-raft`** (small PRs): CheckQuorum, PreVote, wiped-disk rule, fatal append.
4. **Network transport and a bootstrapped single-node cluster** with the cluster id guard, tested with both topologies. Certificate classes and S7 land here.
4a. **Join and bootstrap.** `--bootstrap` (refused on an existing cluster), `--join` with seeds and stale-entry skipping, the join protocol, single-server membership changes, non-voting members as learners. Red first: a second `--bootstrap` makes a second cluster; a joiner hearing two cluster ids joins one of them silently; a node with no `--cluster-id` attaches to either cluster behind one DNS name. Needs InstallSnapshot only once compaction exists.
4b. **Voter maintenance.** The voter-count setting and `kbf cluster set-voters`; the leader promotes caught-up electable members (`--promote-lag`, `--promote-rounds`) and replaces a dead voter (promote first, remove next, nothing in between); `voters N of M` in `/readyz`; the dwell; the unelectable-supply warning. Red first: a dead voter is never replaced; a wiped node is promoted while behind; a second change starts between the add and the removal.
5. **Epoch, readiness, follower refusal.** `Takeover` index as `Welcome.epoch`, `set_leader` wired, step-down clears state, lower epochs rejected; followers refuse sessions. Behind a flag until the drill passes. Red first: wall-clock epochs are not ordered; followers accept sessions today.
6. **Per-node tier over the Raft log.** `AllocEpoch`, `AddTier` and `PutStaged` forwarded from any server node, with the idempotency rules of 4.2. Red first: a non-leader's `PutStaged` is not forwarded. Test, both modes: an upload through node B is readable from node C through B's block with zero blob bytes through the leader (a counter on the leader's blob bytes).
7. **`TierDown` and `TierUp`** with the version gate, the down-tier write rule and the dwell. Red first: a blob on a down tier is served as `Present`. Lands before peer fetch is exposed to clients.
8. **Peer fetch.** `ReadBlock` service, digest check at the receiver, flush race, fallback order, input-cache keep. Red first: a flushed-while-reading object commits `ObjectUnreachable`; a corrupt peer answer is accepted.
9. **Envoy routing, stage 1 then stage 2.** Stage 1 (the cutover, no kbf code): the deploy tooling renders Envoy's static list of electable servers with the leader-ready health check and outlier ejection; fake-backend routing test (only the passing host gets traffic; a listed non-leader never does; a deposed leader that still passes cannot acknowledge a write), cold-start test and the retry table. Stage 2: kbf serves EDS from every server node naming the leader; golden tests (one per item of 6.1), stream-drop test, cold-start-without-seed test. One cluster. The optional floating IP lands here behind its own flag, tested with a fake interface.
10. **InstallSnapshot,** then compaction allowed; joins of a new node then stream a snapshot.
11. **Control-plane trait:** in-process and remote implementations, the conformance suite and the role-change switch. **Data-plane trait:** in-process and remote, conformance suite; the remote one is what isolated mode needs. Red first: a daemon node cannot read or write blobs without cold-store access. Result writes tied to a lease (3.2) land with the remote data plane.
12. **Drills.** Trusted: kill the leader under a real build; kill a holder under load. Isolated: S8, then the same kills.
13. **Stage 2:** replicated scheduler from PR #303, trailer redirect, `Welcome` server list, leadership transfer, learner streaming if the lookup measurement justifies it, per-lease read authorization, locality hint, optional second-tier block replication.

## 10. Questions decided by the owner

All fourteen questions of the earlier draft were answered on 2026-10-15; each answer is a **[D]** decision and none is open.

1. **[D]** The control plane is in-process only while the node is the leader and remote otherwise. The node's own blobs stay local either way.
2. **[D]** The leader commits `TierDown` for a dead holder and `TierUp` when it returns. `--hot-store-abandon` is the only permanent marking.
3. **[D]** Actions run under a separate uid on trusted all-in-one nodes.
4. **[D]** Isolated mode: result writes are tied to a lease now; per-lease read authorization comes in stage 2.
5. **[D]** Daemon-only nodes send data to the leader in v1; spreading it across servers comes later.
6. **[D]** Non-voting members forward lookups to the leader in v1, until measured.
7. **[D]** Second-tier block replication before the acknowledgement is a stage-2 option in both modes.
8. **[D]** The single-server pilot's log and unflushed hot blobs are abandoned at the switch.
9. **[D]** Stage 1 loses queued and running work on failover, as an interim.
10. **[D]** One Envoy (with the kbf gateway) on the client box for now; two stateless ones behind the one DNS name later.
11. **[D]** The wiped-disk rule (rejoin as non-voting, promote after catch-up) is adopted. Recovery when most voters die is operator-driven, never automatic.
12. **[D]** Verify-on-read first; acknowledge-after-flush for `UpdateActionResult` only if misses turn out to be frequent.
13. **[D]** The voter count is changeable after bootstrap (default 3).
14. **[D]** `--cluster-id` is required on a first join unless `--accept-any-cluster` is given.

## 11. Relation to PR #303 (`ha.md`)

- Absorbed as stage 2: the replicated scheduler (`FarmMachine`), epoch as log id, `Takeover`, the trailer redirect, the version gate and learner flow stay specified there.
- Superseded for v1: the two-cluster front (`kbf-reads` on any server). With per-node hot stores and a read path that needs `TierDown` and peer fetch first, v1 routes client traffic to the leader (section 6, staged); widening is the later config change.
- Superseded: growth from 1 to 3 voters by `--raft-join`. Here `--join` takes seeds, membership lives in the Raft configuration, the leader promotes electable members and `--bootstrap` starts a cluster (section 5.3). The learner flow of `ha.md` is the mechanism for non-voting members.
- Added here: the node types and resolver, daemon nodes, the two modes and their security invariants, the stage-1 epoch from a `Takeover` index, the wiped-disk rule, the tier loss semantics, staged Envoy routing and its settings, the lock-service mapping (5.4).

## Appendix: what changed, and review points not adopted

Changed from the first draft: the roles system is gone (three subcommands `server`, `daemon`, `all-in-one`; `--electable`; seeds and `--bootstrap` instead of flags `--build`, `--non-voting`, `--daemon-only` and a candidate list); leader-only uploads and "followers serve no reads" are replaced by per-node hot stores over the existing replicated index; the leader-local index is dropped; security, loss semantics and the two-mode rule are new.

Review points not adopted, and why:

- *Make every server node a voter, to avoid non-voting nodes.* Rejected: the owner decided that non-electable nodes are full non-voting members. Forwarding lookups to the leader is the v1 answer.
- *Cut the in-process control plane from v1.* Not cut. The interface and conformance suite land first and the in-process implementation is sequenced late (PR 11), because the owner decided it; only the order is the judges' point.
- *Cut the `Welcome` server list.* Deferred to stage 2, not cut: v1 uses the membership learned from the cluster.
- *Treat the simulator's caller-identity check as proof.* It is kept as a code tripwire (S5) and the farm probe is the proof (S8); the judges' point that identity is declared by the test is accepted.
- *Treat the dependency lint as proof.* Accepted as a tripwire only (S9).
- *Drop the `/readyz` mode text because it is a string match.* Kept as S10, with its limit stated; it exists so the weaker mode cannot be shown as the stronger.
- *A flag to drop inter-node TLS in trusted mode.* Rejected: a second way to run one link.
- *The first draft's leader-local blob-location index, rebuilt from reports.* Rejected: it is empty after every election and correctness would depend on report timing.
- *Follower reads without ReadIndex; gating readiness on the local daemon; a wall-clock epoch first; the `/v1/nodes` and metric names.* Rejected in the first draft for the reasons recorded there [F#744]. The seed list, learners and `--bootstrap` were rejected there and are adopted now by the owner's decision.
- *Make `always_log_health_check_failure` the only health-check mode.* Not adopted: a single-endpoint cluster with a failing check is already "no healthy upstream", and keeping the check catches a stale EDS answer; 6 states the failover bound instead.
- *Reject `--voter-count` above the electable supply.* Not adopted as a rejection: the setting is accepted and warned (5.3), because supply can grow later.
