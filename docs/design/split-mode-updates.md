# Updating a split-mode farm without failing a build

Status: design, nothing in this document is built. It is read against `feature/arch-v2` and these pull requests, fetched
when this was written: #857, #879 and #887 (the frontend handover, open, stacked), #803 (the rollout state machine, open), #886
(merged: a restarted node whose index is whole registers `Active`) and #860 (open: a wiped node rebuilds its index while
`CatchingUp`). Labels as in `architecture-v2.md`: **[F]** read in code or a PR, **[I]** inference, **[V]** verify before
building on it, **[D]** a design choice made here, **[U]** a claim about a program we do not own, proved only by the
end-to-end test named beside it. **[owner]** marks a decision that is the owner's.

## 0. The plan in one page

The owner's constraints: nothing restarts the frontend by itself (a restart fails every build in flight, buck2 never sends an
`Execute` again once its stream broke), and the farm is never idle, so there is no quiet moment. **One standard frontend update
process everywhere, with Envoy in front**, on a single box, in split mode and in all-in-one mode:

1. **Start the new frontend on another port.** Envoy, with a static config naming both instances, sends new connections and
   streams to it.
2. **The old frontend drains**: it fails its health check, sends GOAWAY, finishes its in-flight requests and exits. Clients
   reconnect through Envoy.
3. **The frontend coordinates the nodes asynchronously.** Each storage-and-daemon node and daemon-only host updates when it is
   idle or drained, in any order, driven by the frontend; every pair of versions in the farm is within N/N-1 (3.2), so order
   does not matter for correctness. At most one storage node is out at a time (R=2). Macs follow the daemon rule.
   There is no queue phase: after the switch the new frontend dispatches as always.
4. **Rollback is the same mechanism with the old release as the target.**
5. Several frontends (Postgres) are updated one at a time the same way. Envoy itself updates by its native hot restart.

What checking this against the handover code found (section 2.9):

- **Envoy replaces #879's listener passing (SCM_RIGHTS) and nothing else.** GOAWAY and drain, the `done/` rows and the
  follower, `Farm::fence`, `Store::demote`, the kept epoch, the cordon rows and `READY=1` all stay.
- **The central problem is the state-store writer.** With Envoy the two frontends overlap in time, and SQLite allows one
  writer per process lifetime (an exclusive `flock`) and #857 fences the old writer out at the next generation. The
  resolution proposed (option d, 2.9) is that **the overlap need not include two writers**: the writer baton moves once,
  as in #879, and the old frontend becomes a read-only relay because its daemons move with the baton and keep their leases. The
  old frontend then drains for as long as its clients' streams last without writing a row. The options that let it keep writing
  (a: relax SQLite, b: Postgres, c: forward its writes) are laid out with costs; they are needed only if the owner wants the
  old frontend to keep its daemon streams.
- **The claim "a long `WaitExecution` reattaches by operation name" is true of Bazel and false of buck2.** buck2 never calls
  `WaitExecution` (4.1). With Envoy the old backend's streams are not cut, so buck2 does not need to reattach; reattach is the
  fallback if the old frontend crashes, and only Bazel uses it.
- **Envoy's own update is the weak point**: hot restart kills the parent's connections at `--parent-shutdown-time-s`, default
  900 s, which fails any build stream older than 15 minutes unless the flag is raised (2.5.4).

Decisions: **D1 [owner]** the writer model (2.9); **D2 [owner]** dispatch to nodes not yet updated; **D3 [owner]** forced drain
of the least-loaded node; **D4 [owner]** where Envoy runs and how it updates (2.5.4).

## 1. Scope

**Split mode** is a frontend host (REAPI, worker and operator API listeners, scheduler, state store, UI), N
`storage-and-daemon` nodes at R=2 (storage half and daemon half in one process, one cache), and daemon-only machines (the Macs,
a cloud daemon group). `architecture-v2.md` section 11 calls this "our farm".

Not here: **all-in-one** (one process holds the frontend, storage and daemon; its handover needs the storage log and the
in-process daemon handed over too, and has its own design); OS, Xcode, kernel and image updates (`fleet-updates.md`); first
install of a host. The pull updater (a signed manifest, an unprivileged client and a small root helper per host) is the
mechanism that delivers a release to a host and restarts a unit; it is specified elsewhere and unbuilt, and this document
says what it must do for this sequence.

## 2. The sequence

### 2.1 Rules the sequence keeps

- **Never two storage nodes at once, and never a second until the first is `Active` and repaired.** With 6 nodes at R=2 every
  pair of nodes shares about one blob in fifteen, so two nodes down at once loses blobs, not only availability. **[I]**
- **Drain, never kill.** A lease or an `Execute` stream is never ended by an update. A lease killed spends the operation's loss
  budget (`loss_requeues`; the third loss refuses the operation).
- **Every gate fails closed**: an absent metric is a failed gate.
- **The record is written before the action**, so a controller that restarts resumes at the host's observed phase.
- **A failure holds the run.** It does not retry by itself and does not skip the host. A person decides.
- **Capacity floor**: a node is cordoned only if the rest meet `min_serving`; a node still holding leases at the wait limit (step 3 of 2.6) is uncordoned and the run goes `Pending`.

### 2.2 One state machine

```text
Idle
 └─ Preflight ─────────── refused: Idle, nothing touched
     └─ Staging              every host fetches and verifies the release in parallel; nothing is installed
         └─ FrontendPhase    start the new frontend behind Envoy; the old one drains    (2.5)
             └─ NodePhase    idle → update → rejoin, one host at a time                 (2.6, 2.7)
                 └─ Settling the old frontend answers its last streams and exits (runs alongside NodePhase)
                     └─ Done
 any phase ── Held (a person acts) | Paused (between hosts) | Pending (a host or the capacity floor says later)
```

The frontend phase is the same on a single box, in split mode and in all-in-one mode. What differs is what the baton carries
(2.5.5) and, if the owner chooses a different writer model (2.9), the overlap of writers.

### 2.3 Who owns each step

| Step | Owner (component) | Needs |
|---|---|---|
| Poll for a release | frontend: update poller (6.2) | read-only access to the release source |
| Preflight | frontend: planner (2.4) | `kbf --version --json` of each tarball; build facts of every running host |
| Staging | each host: update client (`kbf-node`) and root helper, offline verification | session to the frontend |
| Slot, plan, ordering, resume | the **new** frontend: rollout controller (6.1) | `update/*` rows |
| Start the new frontend, switch, drain | frontend host: the helper starts the idle instance on its own ports; the baton moves (#879); Envoy routes by health; the old process drains | units `kbf-frontend@{a,b}` (#887), Envoy static config naming both |
| Cordon, lease count | scheduler (`Farm`), `cordon/<node>` rows (#879) | none new |
| Planned-down, member states | `Members` | operator-settable `down` (2.6, step 3) |
| Open write count, repair debt | storage node: heartbeat fields | not built |
| Activate, confirm, revert | the host's root helper | a signed order in its inbox |
| Rejoin | scheduler: first claim by the node | uncordon |

### 2.4 Preflight

Reads, changes nothing. Any failure ends the run `Idle` with the reasons.

Inputs: the signed manifest per pool and platform, whose `meta` is `kbf --version --json` of the tarball's binary (`commit`,
`protocol`, `min_protocol`, `state_schema`, `profile`, `version`; **[F]** `crates/kbf-cli/src/version.rs`) plus the fields of
3.6; and the same facts of every running host (each reports them in registration). Checks:

1. **Freshness**: the offer's generation is above the newest the host has seen, and an order is newer than the last by (epoch, then id); there is no serial floor (a rollback is the old release offered again with a higher generation).
2. **Every host is known and connected.** A host that reports no build halts the plan with its name; it is never skipped.
3. **Order** (rule in 3.3): the frontend moves first, except a downgrade across a protocol difference, where the nodes move first.
   Refused as a flag day if no order keeps every host inside the frontend's accepted range at every instant.
4. **Schema**: the rule of 3.4. A release whose schema step is not expand-only is refused here.
5. **Handover wire** equal to the running frontend's, else the frontend phase is a cold restart (3.5); and **Envoy's config names both instances' ports** (`kbf-deploy envoy check`).
6. **`meta_log`**: if the release raises the nodes' log machine version, node rollback is "wipe and rebuild" (5.2); the UI says so
   before the click.
7. **`unit_rev`**: unit files and flags are installed by a held script, never by the updater. A release that needs a newer
   unit than a host has is refused (the holder refuses a different state path, but only after the successor started).
8. **Storage gate** G_S (2.7). The metrics it reads do not exist yet; until they do the plan cannot start a node.
9. **Capacity**: with one node out, the rest meet `min_serving` (policy), or the operator accepts the slowdown.

### 2.5 Frontend phase: one standard process, Envoy in front

#### 2.5.1 What stands in front

Envoy is a static front that routes by listener to two backends and never changes during an update. **[F]** `kbf-deploy envoy`
already renders one for a Kubernetes client front (a Tailscale operator Ingress in front of one Envoy that routes REAPI calls by
method to the servers over TLS to the farm's CA; `grpc_health_check`, `healthy_panic_threshold` 0, routes with `timeout: 0s`, HCM
`stream_idle_timeout` 7200 s). That kit is the REAPI half; the rest below is new.

| Listener | Carries | Envoy filter | Backend cluster | Health check |
|---|---|---|---|---|
| REAPI | Execute, WaitExecution, CAS, AC, ByteStream | HTTP connection manager, h2, TLS terminated here | `kbf_reapi`: instances `a` and `b` | `grpc_health_check` on the same port (**[F]** `grpc.health.v1` on the REAPI listener answers `/readyz`) |
| Worker | daemon Session streams, storage `Register`/`Heartbeat` | **TCP proxy, TLS passthrough** | `kbf_worker`: instances `a` and `b` | HTTP check of `/readyz` on the instance's API port (`alt_port`) |
| Operator API and UI | `kbf cluster`, the update page | **not behind Envoy** (below) | each instance's own port | |

- **Two instances, fixed ports.** `kbf-frontend@a` and `@b` keep one template but differ in their listen ports (the instance name
  selects a flags file; flags, not environment). Envoy's config names both and does not change between updates. The idle
  instance is stopped, so it fails the health check and gets no traffic.
- **REAPI route**: one cluster, both endpoints, `lb_policy` round robin; `timeout: 0s` on every route; `stream_idle_timeout` above
  the frontend's progress interval (**[F]** the frontend sends every open `Execute` its operation again every
  `--progress-interval-secs`, so a quiet action keeps the stream active); no `max_stream_duration`, no `max_connection_duration`.
  Retry policy on the unary and `Execute` routes: `retry_on: unavailable,connect-failure,refused-stream`, a small `num_retries`,
  a previous-host exclusion, so a call that reaches an instance that has just stood down is retried on the other before the client sees it **[U]**.
- **Health check for the REAPI cluster during an update**: `interval` 1 s, `healthy_threshold` 1, `unhealthy_threshold` 1
  (the kit's 5 s and 2 would leave a 10 s window), `healthy_panic_threshold` 0 as in the kit (no healthy host means a
  local `503`, not a call to a draining one). **[F]** `close_connections_on_host_health_failure` is documented as
  supported only for `tcp_proxy`, and is left unset for the worker cluster: a health failure there must not cut the
  daemons' connections. **[U]** that a health failure on an HTTP cluster leaves in-flight streams and the upstream connection
  alone; test E1 proves it on the pinned Envoy.
- **Worker listener**: the daemons' and storage nodes' mTLS is checked by the frontend against the peer certificate and a deny
  list (**[F]** `docs/reapi-auth.md`: the worker listener keeps its own mutual TLS), so Envoy must not terminate it. TCP proxy
  with `idle_timeout` 0 (the default is an hour) passes the connection through; the frontend sees the daemon's certificate.
- **Operator API and UI stay off Envoy.** **[F]** `api.md`: a write needs a loopback peer and a bearer token, "because ... a
  reverse proxy on the host makes remote callers loopback". Envoy on the same host would make every caller loopback, so the
  API is addressed directly, per instance, and the controller publishes which instance is active.

**TLS.** Envoy terminates the REAPI TLS (**[F]** `reapi-auth.md`: a front "terminates TLS and balances load"; the REAPI policy
reads no certificate). On a single box, Envoy holds the tailnet certificate files and reloads them without a restart through a
file-based secret source; in the Kubernetes kit the Tailscale Ingress proxy terminates the tailnet certificate and Envoy
speaks cleartext h2 to it. Envoy re-encrypts to the frontend with the farm CA and checks the instance's name (the kit does);
both instances' certificates carry their names. **[V]** the cert-reload mechanism of the pinned Envoy.

#### 2.5.2 The sequence

**[F]** The mechanics of the baton are those of `docs/design/frontend-handover.md` (#887) minus the sockets. In order:

| # | Step | Owner | Detail |
|---|---|---|---|
| E0 | Gate | controller | no other handover running (the idle unit is `inactive`); the state file is backed up (`VACUUM INTO`); preflight clean; Envoy's config names both instances (`kbf-deploy envoy check`) |
| E1 | Flip | frontend helper | install the release; flip `current` by atomic symlink rename; do not restart the serving instance |
| E2 | Standby | frontend helper | `systemctl start kbf-frontend@<other>` on its own ports. **[F]** #879 already has the shape: the successor "builds everything that does not need the store" before it asks to take it. It binds its listeners, answers health `NOT_SERVING`, and says `READY=1` when it can take the baton. |
| E3 | Take | new process | over the control socket: `hello <version>`; the old process answers `offer <generation>` (no sockets) |
| E4 | Switch | both | at the offer the old flips its health to `NOT_SERVING` and sends GOAWAY (**[F]** the existing drain, #793/#810); the new flips to `SERVING`. Envoy sees both within one 1 s interval. The old still answers calls that reach it until it stands down; the new **accepts and holds** calls (a held `Execute` has not been acknowledged and has not been written) until it is the writer. |
| E5 | Stand down | old process | wait up to `--handover-quiesce-secs` (10) for calls between request and queueing; fence the farm; end the worker streams; `Store::demote`; answer `released`. A call that reaches it after this is answered `UNAVAILABLE`, which Envoy retries on the new instance. |
| E6 | Serve | new process | open the store at the told generation, keep the epoch, answer the held calls. Daemons redial through Envoy and reach it with their leases. |
| E7 | Settle | controller | every daemon and storage member connected at E3 is connected again; the map generation did not move; `kbf_frontend_relay_lost_total` did not rise; a probe cycle passed |
| E8 | Confirm | helper | cancel the revert timer; record `last_good` |
| E9 | Settling | old process | answers its open streams from the `done/` rows, then exits (the existing drain: exit 0 when streams reach zero). May take as long as the longest build. |

After E8 the run is in `NodePhase` and `Settling` at once. Nothing may stop the old process meanwhile; the next frontend
update waits for it (both instances active is refused).

#### 2.5.3 The one hold: the writer handoff

Between E4 and E6 (normally under a second, bounded by the 10 s quiesce) the new process cannot write, because the old one is
still the writer. It **holds** the calls Envoy sends it: a held call is an open HTTP/2 stream with no response and no row yet. This
is the baton of 2.9, not a queue mode: from E6 the new frontend serves every call as any frontend does. If the new process dies
in the gap, the held streams break and buck2 fails those actions (5.4). It replaces #879's "connections wait in the backlog",
which lost nothing but needed the listening socket to be passed. Above the quiesce bound `kbf_handover_gap_seconds` alerts (2.8).

#### 2.5.4 Envoy's own update, and where it runs

Native hot restart (**[F]** Envoy docs): the new process initializes fully, including health checks, then asks the old for its
listening sockets and tells it to drain; existing connections are **not** transferred; the old process drains for
`--drain-time-s` (default 600) and is shut down at `--parent-shutdown-time-s` (default 900), "independent of the drain time",
and any connection still open at that point is closed. So **with the defaults every build stream older than 15 minutes through
the old Envoy fails when Envoy is updated**. Required: `--parent-shutdown-time-s` of at least the longest build (hours), and a
gradual drain so clients are sent GOAWAY and reconnect to the new Envoy as their calls end. The clients' reconnect through the new
Envoy is the same GOAWAY race as 4.1, one hop earlier, and has the same test. Hot restart is for a host running Envoy under a hot
restarter; the Kubernetes kit runs Envoy as a Deployment, where the equivalent is a rolling update with a very long
`terminationGracePeriodSeconds` and a pre-stop drain, and **there is no hot restart across pods**. **D4 [owner]**: where Envoy
runs. Lean: on the frontend host under the hot restarter for the single-box farm; the kit unchanged for Kubernetes with the grace
period raised; Envoy updates are rare, announced, and never part of the frontend run.

#### 2.5.5 All-in-one and the same process

The same Envoy, the same two instances, the same E0 to E9, with two additions that belong to the all-in-one update design: the
storage log (`kbf-meta`, single-writer) is handed over with the store, and the old process's **in-process daemon detaches**:
it dials the new instance through Envoy as a remote daemon, reports its results and uploads its outputs there; its leases are in
the state store so the new instance does not double-book the box. This document does not design that; it requires that the baton
of E3 to E6 also carries those two things.

#### 2.5.6 Several frontends

With Postgres behind Envoy (or any load balancer) each frontend is updated in turn by E2 to E9 minus the baton (there is none): start
the new instance, wait for health, drain the old one, wait for its streams, stop it. The rollout slot is held by a new-release
frontend only (a controller of the old release checks `update/slot.epoch` and does nothing). Node steps begin once one frontend
is new. Frontends of both releases serve nodes of both, so the contract of 3.2 holds in all directions.

### 2.6 The node step: any order, when idle or drained

This is what the node-rolling unit implements. The new frontend's controller drives it and the nodes need not be taken in any
fixed order: the frontend picks whichever candidate is idle or drained first. **At most one storage-and-daemon node is in a step
at a time** (2.1: with R=2 two nodes down lose blobs). Daemon-only hosts have no storage coupling and may be in a step in parallel,
up to the capacity floor. Order is free because the frontend is already at the new release and every pair of versions is within
N/N-1 (3.2), including an R=2 pair that spans two releases. (A daemon-only host runs the same steps minus 3, 4 and 7.)

**Selection.** Candidates are hosts at the old release, not `Held`; a storage candidate also needs no other storage node in a step. The controller looks every
heartbeat. A candidate is picked when it is **idle** (2.7) and the gates hold. If none is idle after `idle_wait` (default 2
minutes from E8 or from the last node's uncordon), it picks the candidate with the fewest leases and cordons it, so a farm that
is never idle still drains one node at a time (**D3**, below). Idle nodes are never skipped for a busier one.

| # | Step | What happens | Leaves the step when | If it fails |
|---|---|---|---|---|
| 1 | Claim | CAS `update/slot` to this host, write `update/host/<id>` = `Selected`. Re-check G_S. | slot held | nothing changed |
| 2 | Cordon | `POST /v1/nodes/{node}:cordon`, atomic in the scheduler (and the row persists). A node already at zero leases is cordoned in the same step it is picked, so no lease can arrive between "idle" and "cordoned". | placement `cordoned` | nothing changed |
| 3 | Leases | wait for the node's leases to end by themselves. Idle for the daemon half is zero leases and no claim granted but undelivered. | zero leases | after the limit (15 min Linux): uncordon, `Pending` (capacity beats update) |
| 4 | Planned-down | `POST /v1/members/{id}` `{"state":"down","reason":"restart"}`: writes the `down/<id>` mark with the dwell and bumps the map in one transaction; every router stops choosing this node as a write primary. Wait for the node's open write sessions and the frontend router's writes in flight to it to reach zero. | writes in flight zero | undo (member returns by its next heartbeat), uncordon, `Pending` |
| 5 | Gate | re-evaluate G_S with this node out | true | undo as above; `Held` if a second member is `Down` |
| 6 | Activate | order to the host: the helper writes `pending_confirm`, flips `current`, restarts the unit; the revert timer arms when the new process starts | helper `ACTIVATED_UNCONFIRMED` | the helper reverts itself |
| 7 | Rejoin storage | the new process registers (boot counter larger): `Active` at once if its meta log has `RebuildDone` (#886), `CatchingUp` if it rebuilds (#860; reads answer `UNAVAILABLE`, never `NOT_FOUND`) | `Active` | intact node not `Active` in the health limit (120 s): revert. Rebuilding node: wait while it makes progress, never revert |
| 8 | Repaired | the debts of the downtime are settled | G_S true with this node in | wait up to 5 min, then `Held`, node stays cordoned, no revert (the binary is healthy) |
| 9 | Confirm | order `CONFIRM`; helper cancels its revert timer | `CONFIRMED` | the armed timer fires at its deadline |
| 10 | Rejoin | uncordon; the scheduler places on the node again | first lease granted to the node, or one heartbeat with nothing waiting for it | node stays cordoned, alert |

Restart deadline: the `down/<id>` mark lives for the dwell (300 s default **[F]**), and a node not back registered by then leaves
the map and its slice is re-replicated. The controller holds the run and alerts at dwell minus two TTLs instead of waiting for
the dwell to decide. **[V]** how long a start takes on a full node; no measurement exists.

**Step 4 is a gap in the code and decides whether builds fail.** **[F]** `kbf-route` sends a write "to the first write owner
that is not `Down` in the map, and to that one only". A member goes `Down` after its row's TTL lapses (15 s after the last
heartbeat). So for up to 15 s after the process stops, writes whose primary is this node fail `UNAVAILABLE`; buck2 retries
an upload 5 times with 100 ms to 5 s back-off (about 1.5 s) and then fails the action. Reads are fine: a refused connection
fails over to the next owner at once. Today `Members::set_state` refuses `Down` (`NotSettable`). **[D]** An operator-settable
`down` with a reason is the smaller change; the alternative, letting the router fail a write over on a refused connection,
changes the write path's "that one only" guarantee and the repair accounting. It must not be `Draining`: that starts the
handoff of the node's slice, a movement of the whole shard for a one-minute restart.

**Storage graceful stop. [V]** The storage half should stop accepting, finish open streams to a bound, then exit (the frontend does
this with `--shutdown-timeout-secs`). Whether the node does on SIGTERM is not established in code I read. Step 4's wait for
zero write sessions makes the stop safe for writes either way; reads that break mid-stream resume on the next owner
(**[F]** `kbf-route`: a read that breaks after its first byte "goes on from the bytes already passed").

**D2 [owner]: does the new frontend dispatch to nodes not yet updated?** Under the baton (2.9, option d) every daemon redials the new
frontend at once, so by default yes: the protocol rule (3.3) makes it safe and capacity is not reduced during an update that may last
hours. Read literally the plan gives old nodes no new work until they update; the knob `new_work_only_to_updated` does that, at the cost
of capacity falling to the updated set until the nodes update, which the capacity floor and the step-3 wait limit bound. **Lean: yes, dispatch (the default).**

**D3 [owner]: pure opportunism or forced drain of one node.** Waiting only for a node to be idle can starve when the farm never
is. Lean: the two-stage rule above (opportunistic for `idle_wait`, then cordon the least-loaded node); it never has more than
one node cordoned, so capacity loss is bounded at 1/N, and it falls back to `Pending` at the limit.

### 2.7 Idle, defined

| Half | Idle means | Source |
|---|---|---|
| Daemon (a daemon-only host, or the daemon half) | zero leases; no claim granted and not yet delivered; for a Mac, no whole-machine hold | scheduler state |
| Storage | **no in-flight writes** to the node, and **the other R=2 copy is serving** | below |

Reads never make a storage node idle and are not waited for: a read in flight finishes or resumes on the next owner.

**G_S, the storage gate** (all true, evaluated with the node marked out):

1. every other member is `Active` and heartbeated within a TTL (a silent member counts as down);
2. the aggregate repair debt is zero, so the other copy of everything this node owns is complete;
3. no member is `CatchingUp` or planned-down;
4. after step 4: the node reports zero open write sessions and the frontend router has zero writes in flight to it.

**[F]** None of these metrics exist: `under_replicated` is only planned in `architecture-v2.md`, repair debts are journalled in
each primary and the frontend is told through a `NoLedger` today, and a node reports no write-session count. So the gate fails
closed and node updates by this path cannot start before the node reports `open_writes` and `repair_debt` in its heartbeat and
the frontend sums them (unit U3).

### 2.8 The handover gap, and capacity during node steps

- **The handover gap** (E4 to E6): calls are held in the new process. Limit: the quiesce bound, 10 s. The metric
  `kbf_handover_gap_seconds` is set at E6; above 10 s it is a page-level alert, because a held stream is the next thing to fail. The
  alert says: `Frontend switch to <commit> held calls for <s> s (bound 10 s): <n> streams at risk; the old instance is <state>.`
- **Capacity during node steps**: the controller cordons a node only if the remaining nodes meet `min_serving`. A node that still
  holds leases at the wait limit (15 minutes on Linux, 60 on a Mac) is uncordoned and the run goes `Pending`; the UI shows which
  host and why. There is no separate queue alert: operations waiting for capacity are the scheduler's ordinary claim wait, already a
  metric.

### 2.9 The writer problem, and what of #857, #879 and #887 stays

**The problem as the owner stated it.** With Envoy the old and new frontends genuinely overlap. If the old frontend keeps its
daemon streams while it finishes, it still writes the store (results, leases, session renewals, `done/` rows) while the new one
serves and dispatches. **[F]** SQLite is built to refuse that twice: the process takes an exclusive `flock` for its life
(`architecture-v2.md` 4.3: a second frontend on the file refuses to start; `Store::demote` is the only way the lock moves), and
#857's `Fenced` adds a check on `meta/generation` to every transaction, so the old handle's writes fail the moment the new
one opens at `g+1`.

**What the overlap needs, precisely.** Two things overlap in time: the old frontend's *client streams* and its *daemon streams*.
In #879 the second is not an overlap: at stand-down the old frontend ends the worker streams; each daemon redials, reaches the new
frontend with the epoch kept, and keeps its leases and runs (**[F]** #879's test finishes a build in flight across a handover).
After `released` the old frontend writes nothing: it reads the store through the reader connection `demote` keeps and answers its
client streams from `done/` rows. So the writers do not overlap even though the processes do. The owner's description holds only
if the old frontend keeps its daemons.

**Options**, with costs:

| Option | What changes | Costs | Buys |
|---|---|---|---|
| **(d) One writer at a time, daemons move with the baton** (#879's model; lean) | nothing in `kbf-state`; the baton protocol of #879 without the sockets | the 1 s gap (held calls, 2.5.3); the strand window if the new process dies right after `released` (cold start, epoch bump, actions rerun); a daemon's running actions are not drained (they continue and are re-bound); per-operation memory lost (K13) | built and tested; the SQLite guard keeps its meaning; Envoy needs no knowledge of ownership |
| (a) Relax SQLite: no `flock`, SQLite's own cross-process write locking (WAL, serialized writers, `busy_timeout`), fencing per row and operation, not per store | the SQLite backend loses its single-writer guard; `Fenced` becomes per-resource (`op.lease` already is; add `session_gen` per daemon, `architecture-v2.md` section 5); both `Farm`s run the scheduler over one queue, which is P9 (pull-claim, a waiter on any frontend, duplicate timers and reapers made safe); group commit (C7) runs per process and the processes serialize on SQLite's lock; the guard against two frontends on one file (or a network filesystem) is gone | L to XL; almost the whole of P9, so it costs about what Postgres costs, without the HA | the plan as stated, on SQLite |
| (b) Postgres for any deployment that updates without downtime | P9 and P10 of `architecture-v2.md`: backend, `session_gen`, a `draining` role (claims nothing, serves its leases), the fence replaced | L to XL; **the owner said SQLite only for now: this contradicts it** and adds a database to run | the plan as stated, true overlap, no strand window, several frontends, HA |
| (c) The old frontend forwards its remaining writes to the new one (the new one is the only writer from the switch) | a `ProxyStore` in `kbf-state`: the old process reads through `demote`'s reader and forwards each `Txn` over the control socket (a `Txn` is opaque keys and conditions, so the transport knows no row); the fence belongs to the writer process, the old frontend's authority is its rows (`op.lease == id`, `session/<daemon>.owner`); the new frontend must not touch leases of sessions the old owns, and its reaper must wait for the old's forwarded renewals | M to L; the old frontend cannot finish if the new one is down; two releases speak the `Txn` wire (frozen, like the handover wire); the failure matrix doubles (new dies while old forwards) | the old frontend keeps its daemons until each host updates; no lease handover |

**(d) is the lean, for these reasons.** It is already built and tested; it keeps the one guard SQLite's whole design rests on; it
asks nothing new of the state store; (a) and (b) both need P9, which is not built, and (a) trades a safety property for it; (c)
adds a second moving part to avoid moving daemons, and the daemons move without loss already. **D1 [owner]**: confirm (d) for the
first release, with (b) as the target when the farm needs HA or several frontends, and (c) only if the owner needs the old
frontend to keep its daemons.

**Kept from #857, #879, #887:** the baton protocol on the control socket (`hello`, `offer <generation>`, `ready`, `released`) and its
bounded quiesce; `Farm::fence`; `Store::demote` and the generation fence (right for a single writer); the next-generation open with
the epoch kept; `done/` rows, the follower and `kbf_frontend_relay_lost_total`; operation names that carry the generation;
cordon rows; `READY=1` and `Type=notify`; the existing drain (health `NOT_SERVING`, GOAWAY, exit at zero open streams, #793/#810);
`kbf frontend handover` (now: start the other instance in standby and trigger the take); `kbf-frontend@.service` and the
control socket's `RuntimeDirectoryPreserve`.

**Dropped:** `SCM_RIGHTS` and the `Sockets` of `handover/`; the successor's adoption of listeners and its `local_addr` check; the
requirement that both instances have identical addresses (they now differ by port, and the holder's refusal of a different
listen address is removed, while a different state path is still refused); the latches that close the old listeners at once;
"connections that arrived in the gap wait in the backlog" (replaced by hold and retry). **Changed:** the check-units rule
`handover` (instances differ in ports); the successor must serve health and *hold* calls before it holds the store (today it
answers nothing before `ready`); `install-units` writes a flags file per instance.

**Other conflicts and what they need:**

| # | Conflict | Resolution |
|---|---|---|
| K8 | After `released` the old process cannot take the store back; a successor that dies there leaves no frontend until systemd restarts it (cold start, epoch bump) | documented, tested (C3); Postgres removes it |
| K9 | `kbf frontend handover` refuses with both instances active (#887); NodePhase starts while the old one drains | node steps do not need the old instance gone; only the next frontend update waits |
| K10 | #803 plans storage, frontend, daemon, Mac, and its frontend step drains, waits 6 h and swaps (a restart) | frontend first; the step is E0 to E9; per-host steps become 2.6; the ssh transport is replaced by the session, the operator API and the helper's inbox; the engine resumes from `update/host/<id>` |
| K11 | Two rollout records: the in-memory `kbf_types::Rollout` driver and #803's | one record: `update/*` rows; the old phase names survive as display names |
| K12 | The handover `hello` is an exact-match version (**[F]** `wire.rs`) | frozen at 1; growth through optional trailing tokens; a bump is a cold restart (3.5) |
| K13 | Not carried over: memory-ladder history, usage line, recorder state of operations in flight (**[F]** #879) | accepted; never fails a build; unit U9 |
| K14 | The helper's revert after `released` is a cold start of the old release | only across an expand-only change (3.4) |
| K15 | Instance units differing in ports: the helper's `host.toml` names the unit pair and Envoy's static config names both ports | one source of ports: the flags files; `envoy check` verifies Envoy's config names them |

### 2.10 Daemon-only hosts and the Macs

Same steps minus 3-storage details: cordon, wait for zero leases (idle: no leases, no whole-machine hold), activate (a
LaunchDaemon restart through the helper), register `active` at the commit, confirm, uncordon, first claim. A Mac holds no
blob; its loss is capacity only.

- **Opportunistic only by default**: a Mac is updated when it goes idle; the controller does not force-drain one (a Mac lease may
  last an hour: 60 minutes by default). After `mac_force_after` (policy, default off) it cordons the least-loaded Mac as D3 does.
- **A busy Mac is deferred**, the run continues, and "done with N deferred" shows in the UI. This is safe in both orders: a
  Mac at the old release is inside the new frontend's accepted range (3.3).
- **The last Mac of a platform class is updated only when no operation is waiting for that class** (`fleet-updates.md`: a node
  that is the last of its class). Otherwise that work would stall for the restart.
- Quarantine, code signing and TCC grants are the pull updater's concern (a hand-copied binary is refused; ad-hoc signed
  binaries change identity each build, which may reset a grant; **[V]** whether any Mac uses one).

### 2.11 Rollback

**Set the old release as the target; the same mechanism walks back.** The controller offers a new release naming the previous
artifacts with a higher serial (the old directories are still on the hosts: running, previous, one more), preflight runs, and:

- frontend: a handover back (the old release is started as the successor); this works only if the contract of 3.2 holds in the
  reverse direction, which is why every row of it says both ways;
- nodes: the node step, in the same order, idle by idle;
- across a protocol difference the order reverses (3.3): nodes first, so no node ever dials a frontend that does not accept it.

Not reversible, and said so before the click: a release that changed the schema beyond expand (3.4), and a node log raise.
**A node whose log was raised** cannot be reopened by an older binary (**[F]** `MachineVersion::format()` is `None` for a version
it does not know); the walk-back wipes that node's meta directory and rebuilds from segment footers (#860). **[V]** that the
action-cache entries that only that node's log held are on the other owner (#860: the action cache "is not rebuilt").

## 3. Mixed versions

### 3.1 Windows

| Window | Frontend | Nodes | Lasts |
|---|---|---|---|
| W1 (handover) | holder N and successor N+1, one store | N | seconds, then the holder serves leftover streams from rows |
| W2 (node phase) | N+1 | k at N+1, the rest at N | hours |
| W3 (deferred hosts) | N+1 | a busy Mac at N | until it updates |
| W4 (rollback) | each of the above reversed | | |
| W5 (Postgres, option b) | N and N+1 both writing | N | the whole run |

### 3.2 The compatibility contract

Between a release and the one before it, **in both directions**, because rollback and the handover back run the windows
backwards.

| Surface | Between | Rule across one step | Now |
|---|---|---|---|
| Worker stream (`kbf.worker.v1`) | daemon and frontend | The server accepts `min_protocol..protocol` and answers `Welcome` with the daemon's own version (**[F]** `worker.rs`); the daemon accepts only `Welcome.protocol_version == PROTOCOL_VERSION`. A new frontend serves old daemons; a new daemon is refused by an old frontend. Changes are field and oneof-arm additions only. **[V]** an old daemon ignores a `ServerMessage` arm it does not know. | holds for additive changes; no test sends an unknown arm |
| Membership (`kbf.map.v1`) | node and frontend | `Register` refuses a protocol below the frontend's floor (**[F]** `members.rs`) and has no ceiling | one-sided |
| Storage RPCs and REAPI through the map | frontend and node, daemon and node, node and node (chain replication, repair, read repair) | every storage RPC works between N and N+1 in both roles, because an R=2 pair spans the window | no cross-version test |
| Operator API, UI JSON | tools and frontend | additive | by convention |
| State-store **row encodings** | frontend N+1 and holder N (and, in B, both writers) | the holder, demoted to a reader, reads `done/`, `op/`, `lease/` and queue rows the successor writes, for three looks each. In B each reads every row the other writes. Frozen across one step. | not stated anywhere |
| SQLite **schema** | frontends only | rule 3.4 | strict equality, no migration |
| Handover control wire | holder and successor | frozen (K12) | exact match |
| Node meta log (`MachineVersion` V1 to V4) | one node, across its restart | one-way raise at start | no declared field |
| Cold-store objects and footers | any nodes | a node adopts older segments (#860); **[V]** an older node tolerates newer objects | unknown |
| Unit files, flags, certificates | host and release | not updated by the pull path; a release declares `unit_rev` | no declaration |

### 3.3 The order rule

At every instant each node's `protocol` must lie in the frontend's accepted range `[min_protocol, protocol]`. So:

- **Upgrade** (default): frontend first. The new frontend accepts old nodes iff `new.min_protocol <= oldest running protocol`.
  A new `protocol` is fine: old nodes are inside the range.
- **Downgrade across a protocol difference**: nodes first, then the frontend: otherwise the old frontend would refuse the
  nodes that are still new.
- **No order satisfies it**: a flag day. The planner refuses and prints the numbers.

This is simpler than the earlier "nodes first" plan: nodes can never be ahead of the frontend on an upgrade.
**[D]** `architecture-v2.md` section 11 says "storage one at a time, frontends, daemons"; one sentence of it changes when this is accepted.

### 3.4 The schema rule: expand then contract

**Chosen: expand/contract** (as `architecture-v2.md` section 11 already states), not a version gate. **[F]** Today
`schema.rs` accepts a file only when `user_version` equals the binary's, so the new frontend refuses the old file, and a
demoted holder or a rolled-back binary refuses a migrated one. Design:

- The file records `rev` (the newest expand applied) and `floor` (the oldest binary revision that may still open it). A binary
  has its own `REV`. It opens a file iff `floor <= REV`; if `rev < REV` it applies its expand steps in one transaction and
  sets `rev`. A binary older than `rev` opens the file unchanged because expand steps only add (columns, keys, tables with
  defaults) and a binary ignores what it does not know.
- **Contract** (dropping or reinterpreting) raises `floor`. It runs only by an explicit command, never on open, and only in a release
  after every frontend and every holder is at or above the new floor; preflight refuses a contract release while a running
  frontend reports a lower `REV`.
- Row encodings follow the same rule: a new field is read by the old release as absent; an encoding change is a contract.
- Rollback is therefore always safe until the contract step runs, and the UI marks a contract release "not reversible".

Why not a version gate (new behaviour off until every host reports the new version): a gate does not solve the first
open, when the new binary must change the file while the old one still reads it; it needs every host online and known to
raise (a silent Mac blocks it); and once raised it cannot be lowered, which removes rollback. Expand/contract needs none
of that. The rare release that changes behaviour (not shape) ships the behaviour dormant behind a policy row and a later release
flips it: the same "expand first, activate a release later" rule, not a second mechanism.

### 3.5 What is not a handover

Refused by preflight unless the operator accepts failing builds (`--accept-failing-builds`, typed reason, `admin`): a
contract schema change; a handover-wire change; a change of state path (**[F]** the holder refuses); a change of ports or certificates that Envoy's static config must learn
(an Envoy reload, not part of the run). The cold path is `systemctl stop` of the serving instance (it drains: refuses new
`Execute`, keeps open streams, no deadline) and a start of the other.

### 3.6 What `kbf --version --json` adds

Today: `commit`, `min_protocol`, `profile`, `protocol`, `state_schema`, `version`. Add: `handover` (wire version), `meta_log`
(the highest log machine version the binary raises to), `unit_rev`, and the schema pair `rev` and `floor` in place of
`state_schema`. A test pins that each is reported.

## 4. What a client sees

### 4.1 Buck2 and the frontend

**Facts about buck2 [F]** (`remote_execution/oss/re_grpc/src/client.rs` on `main`, read for this document; #791 here read the same file
earlier):

- `execute_with_progress` retries only opening the stream: `MAX_ATTEMPTS` 5, 100 ms doubling to 5 s, with jitter. A stream that breaks
  after it started is returned as an error and the action fails (`remote_call_error`).
- `is_retryable` is true for `tonic::Status` `Unavailable`, `ResourceExhausted`, `Aborted`, `Cancelled`, for transient `io::Error`
  kinds (`BrokenPipe`, `ConnectionReset`, `ConnectionAborted`, `UnexpectedEof`, `TimedOut`) and for an `h2::Error` wrapping one.
- It never calls `WaitExecution`. A broken `Execute` is not re-attached.
- Blob uploads and downloads (ByteStream write and read, batch update and read, `GetActionResult`, `FindMissingBlobs`) are retried by
  the same helper; a ByteStream retry runs the whole blob again.
- It has no GOAWAY-specific handling.

**HTTP/2 and tonic [F]:** after GOAWAY, streams at or below `last-stream-id` run to completion (RFC 9113). The h2 server's
`graceful_shutdown` documentation says "after about 1 RTT, no new requests should be accepted", "it's possible to receive more
requests after calling this method", and the connection closes once all active streams complete. tonic maps `REFUSED_STREAM` to
`Unavailable`, `CANCEL` to `Cancelled`, `NO_ERROR` to `Internal`, anything else to `Unknown`; hyper turns an RST with `NO_ERROR` or
`CANCEL` into a clean end of stream.

**[U] The race is where buck2 can fail a build.** A request sent just as the GOAWAY lands is refused with `REFUSED_STREAM` (tonic
`Unavailable`: retried), or surfaces `NO_ERROR` from the GOAWAY (tonic `Internal`: not in buck2's list, the action fails), or is
cancelled before it was sent (`Cancelled`: retried). I could not establish which one the pinned buck2 produces from the sources.
#887's claim that such a call "gets UNAVAILABLE at open, which Buck2 asks again" has no test. **T6 is the proof.** If red, the
fallback is a two-step GOAWAY (first with the maximum stream id, then a PING round trip, then the final one: the sequence RFC 9113
describes) in `drain.rs`/`handover/outgoing.rs`; no change to buck2.

**Through Envoy.** Two more hops have the same race. Envoy to the frontend: the old backend sends GOAWAY (and fails its health
check) while Envoy may still hold a request for it; Envoy's upstream connection to it drains, new streams go to the other
backend, and the retry policy of 2.5.1 retries a call refused with `UNAVAILABLE` before the client sees it **[U]**. The client to
Envoy: when Envoy itself is hot restarted its old process drains and sends GOAWAY (2.5.4), the client's race is the one above.
Tests E1, E2 and T6 are the proof.

**Client reattach: the evidence.** The claim "a long `WaitExecution` reattaches by operation name" is **true of Bazel and false of
buck2**:
- **buck2 [F]**: never calls `WaitExecution`; a stream broken after it started fails the action (the `client.rs` facts above).
- **Bazel [F]** (`GrpcRemoteExecutor.handleOperationStream`, read on `master`): after an `Operation` arrives that is not done, a
  retry of a broken stream calls `WaitExecution` with that operation's name; `NOT_FOUND` from it means the operation was lost,
  and the next attempt calls `Execute` again (a new operation). The retry is driven by `RemoteRetrier`, whose retriable
  codes I did not read.
- **REAPI**: `WaitExecution` exists for exactly this; the server answers with the operation's current state and keeps the stream open.

So the plan cannot rely on reattach for buck2, and does not: Envoy never cuts a stream because a backend is draining, the old
backend finishes it. Reattach is the safety net for **Bazel** if the old frontend crashes mid-drain: the new frontend must
resolve the operation name from the store (**[F]** names carry the generation, #857; `op/` and `done/` rows) and answer, or
`NOT_FOUND` so Bazel re-executes. Test T20 proves it with the pinned Bazel and shows buck2 failing in the same case (documented).

### 4.2 An `Execute` in flight

**[F]** It stays open and is not told to go away. The old frontend answers it from `done/<op>` rows the new one writes (the follower),
shows "executing" while the operation holds a lease, and ends `UNAVAILABLE` only when the store has no trace of the operation
for three looks (`kbf_frontend_relay_lost_total`). That counter is the count of builds a handover failed and must be zero in every
test but the planted one. A `WaitExecution` is answered the same way. Operation names carry the store generation (**[F]** #857),
so a name is never reused. An operation queued but not leased shows what it showed; it is dispatched by the new frontend.
**Lost, accepted:** memory-ladder history, usage line, recorder state of operations in flight (K13).

### 4.3 A new call

Before E4: served by the old frontend. From E4 Envoy sends new connections and streams to the new instance, which holds them until
E6 (2.5.3); a call that reaches the old instance after its stand-down is answered `UNAVAILABLE` and retried by Envoy on the new one.
Whether a call that races the GOAWAY is retried, by Envoy or by buck2, is T6 and E2.

### 4.4 Daemon worker streams and storage nodes

**[F]** At `released` the old frontend ends the worker streams; each daemon redials the same name, which Envoy's TCP proxy routes to the new
instance (the old one is unhealthy by then). The epoch is kept: a daemon keeps its leases and runs and sends again the results the old frontend did not
acknowledge. A `Result` taken at the moment of stand-down is waited for (`Farm::quiet`); no test holds one in flight (T5 does).

Storage nodes hold no stream: `Register` and `Heartbeat` are unary and membership lives in the store with a 15 s TTL, so a
reconnect to the new frontend loses nothing. A gap past the TTL flaps members `Down` and re-routes writes; T5 asserts the map
generation is unchanged across a handover.

### 4.5 A node update as buck2 sees it

Invisible except through storage calls to that node: reads fail over to the other owner; writes avoid the node from step 4;
an upload mid-stream on it at the stop is retried whole by buck2 (**[F]**, 5 attempts). **[U]** that it completes within the
attempts; T1 measures it. An `Execute` queued because capacity is short keeps its stream open and gets progress every
`--progress-interval-secs`.

## 5. Failure handling and rollback

### 5.1 Principles

- A host reverts **itself** from local state only: a binary that never becomes healthy is put back by the helper's timer or boot
  counter (the window starts when the new process starts, not at the flip, and is capped by the helper's own config).
- The controller **holds** on any failure and touches nothing else. It never retries a revert by itself (a revert from a frontend
  outage and one from a bad build look alike from the host).
- An error from the activate order counts as "swapped, maybe": revert, never retry.
- Rollback is 2.11.

### 5.2 Per step

| Step and fault | What the system does | Operator |
|---|---|---|
| Preflight refuses | `Idle`, reasons, nothing touched | fix the release or take the cold path |
| Staging fails on a host | that host `REFUSED`; the run cannot start | free disk or re-offer |
| E2/E3 standby fails before `ready` (flags, certificates, probes, stopped) | the old frontend serves as it was; Envoy never saw the new one healthy; `current` reverts | journal, fix, re-offer |
| E4 to E6 the new process dies while holding calls | the held streams break and buck2 fails those actions (5.4); the old instance is still the writer and re-enters service (its health returns, `offer` withdrawn) if it has not yet demoted; after `released` it is the strand case below | rerun those builds |
| E5/E6 new process dies after `released` | no frontend until systemd restarts it: cold start, epoch bump, leases rerun; the old process still answers its streams from rows; repeated failure reverts `current` and the cold start runs the old release, **only if** the schema change was expand-only | journal; do not revert if a contract ran |
| Old frontend dies mid-handover | its open streams end as for any crash; the successor proceeds (the kernel freed the lock) | rerun those builds |
| E7 daemons or members do not return | no confirm; the timer fires; the revert is a handover back | |
| Old frontend dies while Settling | the builds on its streams fail; the new frontend is unaffected | rerun; unit U8 |
| Node, step 3 leases do not end | uncordon at the limit, `Pending` | |
| Step 4 or 5 gate fails | undo planned-down, uncordon, `Pending` or `Held` | |
| Step 6 binary exits at once | boot counter reverts before the Nth start; `Held` | journal |
| Step 6/7 starts, never registers | window expires, helper reverts, `REVERTED`, `Held` | |
| Step 7 stays `CatchingUp` | wait while rebuild progresses; a failed rebuild retries with back-off (**[F]** #860); past the rebuild limit `Held`, node stays cordoned, the other owner serves | decide |
| Step 8 debt does not settle | `Held`, cordoned, no revert | look at the other owner |
| A second member goes `Down` unplanned | the controller stops at once, restarts nothing; blobs owned by both are `UNAVAILABLE` until one returns | bring it back first |
| Restart deadline passes | `Held`, alert; the dwell may start re-replication | |
| Confirm lost | the timer fires; node returns to N; `Held` | |
| Revert fails (previous missing) | node stays as is, cordoned, `REFUSED`, page-level alert | manual path |
| Controller (frontend) dies mid-run | on start it reads `update/host/<id>` and resumes; the cordon row persists | |
| Frontend host loses power | everything on it fails; cold start; running actions are re-sent after the epoch | accepted: one frontend is a scheduling single point of failure |

### 5.3 The crash matrix for split mode

For the unit that builds the real-process crash matrix. A case is a SIGKILL, stop, full disk or power cut at the point named,
with real processes. IDs are stable so tests and PRs cite them.

**Frontend:**
- C1 kill the old frontend before `ready`: the successor takes the lock when the kernel frees it and starts cold; builds on the old streams fail.
- C2 kill the successor before `ready`: the old frontend serves; `current` reverts.
- C3 kill the successor after `released`, before it serves (K8): systemd restarts, epoch bump, actions rerun, no operation name reused, the old process answers its streams.
- C4 kill the old frontend after `released` while its follower holds streams: those streams end, the new frontend serves the rest.
- C5 a `Result` in flight at the stand-down: neither lost nor applied twice.
- C6 a new `Execute` every few milliseconds across the handover: zero failures, retries counted (T6).
- C7 the old frontend stuck in quiesce for the full bound: stands down anyway.
- C8 `Store::demote` fails: `failed`, the successor aborts, the old keeps serving.
- C9 control socket missing or another user: refused while something serves.
- C10 both instances active, a second handover: refused.
- C11 a different state path: refused with the reason; different ports are now normal.
- C12 power loss at each of E1 to E6.
- C13 helper dies between the flip and the start of the other instance: reconciles on restart.
- C14 handover N to N+1 then back with a build in flight.
- C15 **hold gap**: the new process holds calls longer than the quiesce bound: counted, alert at 10 s, the held streams are the ones at risk.
- C16 the frontend is killed with operations waiting for capacity: the rows survive; after the cold start they are dispatched; their original streams are gone (those builds fail as for any crash).

**Envoy:**
- C17 the old backend fails health and sends GOAWAY with a build streaming through it for longer than the health interval: the stream is not cut (**E1**, the [U] of 2.5.1).
- C18 both backends unhealthy for a moment: Envoy answers a local `503` (`healthy_panic_threshold` 0), the client retries; no call reaches a draining backend.
- C19 a call reaches the old backend after its stand-down: `UNAVAILABLE`, retried by Envoy on the new one with no client-visible error (**E2**).
- C60 Envoy killed (not hot restarted): every stream through it breaks (documented; builds fail); the frontend and daemons are unaffected and the worker connections reconnect.
- C61 Envoy hot restart with a stream older than 900 s and the default `--parent-shutdown-time-s`: the stream is cut (the planted defect); with the raised value it survives (**E3**).
- C62 the worker cluster's health check fails for the old backend: the daemons' existing connections are not closed (`close_connections_on_host_health_failure` unset) (**E1**).
- C63 Bazel client, the old frontend killed mid-stream: Bazel reattaches by operation name and finishes, or re-executes on `NOT_FOUND`; buck2 in the same case fails its action (**T20**).

**Node:**
- C20 the controller dies after cordon, after planned-down, after activate, after confirm, before uncordon: resumes at the host's phase; no host activated twice.
- C21 the helper dies at each boundary of activate: converges to old or new, never a mix.
- C22 the node's process dies repeatedly after the restart (boot counter), and once after `Active` before confirm.
- C23 power loss of the node during the restart; the dwell expires: slice re-replicated; run held; nothing else restarts.
- C24 a second node killed while the first is planned-down: reads `UNAVAILABLE` not `NOT_FOUND`; controller stops; recovers; no blob lost.
- C25 a lease arrives between "idle" and cordon: impossible by construction (cordon is the pick); the daemon also refuses an activate order with a lease.
- C26 disk full while staging: installed untouched, `REFUSED`.
- C27 intact index: `Active` at once; wiped: `CatchingUp`, reads `UNAVAILABLE`, controller waits for the right one.
- C28 the frontend restarts cold during the node phase: cordon restored from rows, member row survives, heartbeats continue.
- C29 debt metric absent or a member silent: the gate refuses.
- C30 a long log replay on first start: the restart deadline is declared from history.
- C31 write sessions never reach zero (a stuck upload): `Pending` at the limit, node returned.
- C32 a node still holds leases at the wait limit: uncordoned, run `Pending`, nothing restarted.

**Daemon-only:**
- C40 a Mac lease outlives the wait: deferred, not killed, the run goes on.
- C41 the last Mac of a class with work waiting for it: not picked.
- C42 helper dies mid-activate: converges; cordon stays until the controller uncordons.

**Mixed:** C50 each window of 3.1, both directions of 3.2, with a build running through it.

### 5.4 The residual risk, stated

Still able to fail a build: the old frontend crashing while it holds streams (C4); the new process dying while it holds calls in the
gap, or between `released` and serving (C3; those streams do not survive a cold start); a GOAWAY race that neither Envoy nor buck2
retries (U1); Envoy killed, or hot restarted with the default parent-shutdown time (C60, C61); a host power loss. Each has a
metric or a test that asserts on it.

## 6. Who drives it

### 6.1 The controller

**[D]** One driver: a rollout controller in the frontend process (the crate that holds #803's engine), working through the
operator API and the hosts' sessions. The CLI and the UI are its clients. The manual path (a human runs the installer verbs and
`kbf frontend handover`) stays as the fallback; it calls the same installer code.

State in `kbf-state` (one CAS row each; works on SQLite and Postgres):
- `update/desired {serial, track, paused}`;
- `update/slot {holder, order_id, epoch, state}`: no TTL; if the holder dies the slot stays held and the run is `Held`;
- `update/host/<id> {phase, order_id, serial, since}`: what the controller resumes from;
- `update/policy {auto_nodes, auto_frontend, idle_wait, min_serving, new_work_only_to_updated, mac_force_after, accept_failing_builds}`;
- `update/available/<commit>` from the poller.

The controller that runs is the new release's. In A the old frontend's controller is fenced with the rest of the farm; the new
one starts at the host phase it reads and writes `Settling` at E8, so it never repeats the handover it is the result of.

Orders to a host are files in its helper's inbox, written by its unprivileged `kbf` after an order over its session; the helper
verifies offline. **[F]** The messages (`UpdateOffer`, `UpdateOrder`, `UpdateStatus`) are not in `worker.proto` yet. Requirements
added here: an order carries the store epoch and a monotonic id (a restored store cannot replay one; a host refuses a lower id),
a host refuses an activate order while it holds a lease, and the frontend host's `activate` is "flip and start the other
instance" (one fixed argument named by the host's own config), not a restart of the serving one.

### 6.2 The poller

**[D]** A task in the frontend that finds a new attested release and only ever proposes it:

- every few minutes (a flag, default 5), one read-only request per cycle with a conditional request (`ETag`), so a quiet cycle is a `304`;
  the credential, if any, is a read-only token read from a file (a flag, not an environment variable), never logged or stored in a row;
- it looks for the newest successful run of the artifacts workflow on the main branch, and for each platform the farm needs, the
  tarball, `SHA256SUMS` and the attestation bundle;
- it verifies the attestation in-process against a pinned repository and workflow identity, and the tarball's hash, before anything
  is stored; a failure is never offered and an alert names it;
- it reads `kbf --version --json` from the binary (service user, empty environment, no network, a timeout) and rejects one whose commit differs from the run's;
- it runs preflight against the live farm and stores the result, so the UI shows blockers without a click;
- it stores `update/available/<commit>` (hashes, `meta`, attestation time, preflight result, the merge commit subject) and keeps the last few;
- **it never applies.** `update/policy` decides. `auto_nodes: manual` (default) offers; `auto` runs the whole sequence's node steps when preflight is
  clean. `auto_frontend` is separate, defaults to `manual`, and cannot be set to `auto` unless `auto_nodes` is `auto` and the last two runs finished
  without `Held`. A frontend handover is never started by a timer unless an authenticated operator wrote that row;
- it refuses a downgrade (by serial), a commit not on main, and a run with a failed or missing attestation;
- failures it survives: the source unreachable (the offer stays; the UI shows the last good poll; an alert after an hour), a rate limit
  (back off), a malformed answer (log once an hour), a restart (state in rows), a handover (the new frontend starts its own poller; a poll in flight is dropped).

### 6.3 The update page and button

A server-rendered `kbf-ui` page (**[F]** today read-only, no script, over the read API) with a new write path.

**Shows** (from `GET /v1/update`, the JSON twin):
- per host: running release (commit, serial, protocol, since), the available release (commit, attested at, polled at), a "mixed" badge;
- the plan and preflight verdict: order chosen and from which numbers, next host, and **blockers** each with a reason and a fix:
  `schema change needs the cold path`, `release needs node rollback by rebuild`, `repair debt N blobs on <node>`, `a host reports no
  build`, `old frontend still draining 3 streams`, `a member is Down`, `last Mac of its class has work waiting`;
- the run: one row per host with its step (`selected`, `cordoned, waiting for 2 leases`, `waiting for 1 write session`, `restarting`,
  `rebuilding 61%`, `waiting for repair, debt 12`, `confirmed`, `held: reason`), time per step, the frontend phase and the old
  process's open stream count (Settling), and `handover gap`, `relay_lost`, `failed builds during the run`, map generation;
- the policy rows, who changed them last and when.

**Does** (a form with a confirm step that names the plan):

| Button | Triggers | Role | Disabled when (reason shown) |
|---|---|---|---|
| Update | `update/desired` + the whole sequence per policy | `update` | blockers; run active; nothing available |
| Update frontend only | E0 to E8 | `update` | blockers; old instance still draining |
| Update nodes only | node steps | `update` | frontend not new; blockers |
| Pause, Resume | `update/desired.paused` | `update` | no run |
| Skip host | marks a `Held` host failed, frees the slot | `admin` | not `Held`; says the host stays cordoned |
| Roll back | a release naming the previous one | `update` | not reversible (2.11), with the reason |
| Accept failing builds | the cold path | `admin` | never a default; a typed reason |
| Policy | `update/policy` | `admin` | |

The confirm page shows the plan **by digest**; the POST names it; the controller refuses if the plan changed since the page was drawn.

**Auth.** **[F]** The operator API's gates reject a browser by design (loopback peer, no `Origin`, bearer token, JSON content type), and
`fleet-updates-security.md` plans `admin` and a narrow CI `rollout` role (the code has only `Daemon` and `Client`). A button needs a
UI write path first: a session the UI mount makes from the operator's credential (**[V]** which one the fleet uses), a per-session CSRF
token in every form, `SameSite=Strict`, a server `Origin` check, and an audit record naming the actor on every write. **[D]** Roles:
`read` (the page), `update` (the buttons above except skip and accept), `admin` (everything). A CI `rollout` role may offer a release and
may not start the frontend phase or change policy.

## 7. Test plan

End-to-end tests in `kbf-it` (a new cell, `kbf-cell upd`, beside `m1` and `m2`): real `kbf` processes, the pinned buck2,
`//:pause`-style actions, a `file://` cold store, 3 storage-and-daemon nodes at R=2, one daemon-only process, the frontend as
`@a`/`@b` processes with the control socket (the unit text is covered by the existing unit lints). A second cell runs the base
binary and a test-only "N+1" built from the next commit for the mixed-version tests. Each test names the defect and the mutant to plant.

| # | Test | Proves | Defect (mutant) |
|---|---|---|---|
| T1 | `rolling_nodes_under_build_load` | all 3 nodes updated idle by idle while buck2 builds and uploads: zero failed builds, zero `NOT_FOUND`, boot counter +1 per node, never two members non-`Active` at a sampled instant, `lost_blobs == 0` | writes failing in the TTL window (skip planned-down); two nodes at once (drop the slot CAS); `NOT_FOUND` during rebuild |
| T2 | `idle_is_defined_per_half` | a daemon is picked at zero leases and not before; a storage node is not picked with an open write session or with debt; with the other copy's node silent it is not picked | picking on lease count alone (storage); picking with debt |
| T3 | `cordon_survives_restart_and_rejoin_takes_work` | the node comes back cordoned, takes no work until uncordon, then takes work (first lease observed) | reconnect clears the cordon; uncordon without a claim |
| T4 | `node_that_never_becomes_active_holds` | exits at once, never registers, stays `CatchingUp`: boot-counter revert, window revert, hold; other nodes' pids unchanged; builds still pass | missing revert timer; continuing after a failure |
| T5 | `frontend_switch_with_builds_in_flight` | 20 concurrent 30 s builds, a handover with a `Result` held in flight at the stand-down: all succeed, `relay_lost == 0`, epoch and map generation unchanged, no `loss_requeues` | fence after demote; result applied twice; follower look count not reset |
| T6 | `new_calls_across_the_switch` | through Envoy, the pinned buck2 starts a target every 50 ms across the switch: zero failed builds; retries by Envoy and by buck2 recorded. **U1's proof** | GOAWAY semantics nobody retries (no retry policy, no health flip → red); the new instance refusing instead of holding |
| T7 | `hold_gap_and_rows` | the gap metric is set at E6 and stays under the bound; calls held in the new process are answered after `released`; operations waiting for capacity survive a cold start (C16) | answering before `released`; rows not durable |
| T8 | `capacity_floor_and_wait_limit` | saturate the farm during a node cordon: no node is cordoned below `min_serving`; at the wait limit the node is uncordoned and the run is `Pending` | no floor; no limit |
| T9 | `frontend_crash_matrix` | C1 to C19 and C60 to C62, one test each | each case's own defect |
| T10 | `mixed_versions_both_ways` | base and N+1 in every window of 3.1, both directions: N+1 frontend with N nodes, N with N+1 (downgrade order), handover N to N+1 and back, with a build in flight | made-required field; repurposed field; `done/` row tag change; handover wire bump; unknown `ServerMessage` arm crashing an old daemon |
| T11 | `schema_expand_contract` | an N+1 that expands: the N holder still reads, rollback opens the file, the contract release is refused while an N frontend runs | strict-equality open; contract on open |
| T12 | `preflight_refuses` | table: unsigned, older serial, expired, wrong platform, contract schema, protocol flag day, handover mismatch, absent debt metric, silent member, host with no build: each refused and **no process touched** | gate that fails open (absent metric as zero) |
| T13 | `controller_restart_resumes` | kill the frontend cold after cordon, after planned-down, after activate, after confirm: resume at the host's phase, nothing activated twice | engine starting from the top |
| T14 | `second_loss_stops_the_run` | an unplanned node killed while another is planned-down: stop, restart nothing, `UNAVAILABLE`, recover | continuing; counting only planned members |
| T15 | `mac_and_last_of_class` | a busy daemon-only host is deferred; the last of a class with work waiting is not picked | killing a lease; stalling a class |
| T16 | `rollback_walks_back` | offer the previous release: frontend by handover back, nodes idle by idle, builds pass; across a protocol difference nodes go first; a contract or log raise says not reversible / rebuilds | rollback restarting the frontend; wrong order |
| T17 | `poller` | a fake source: newer attested release offered not applied; bad attestation/hash not offered; unreachable shows last good and alerts after the hour; `304` is free; `auto_nodes` never starts the frontend phase; downgrade refused; restart keeps state | applying without policy; unverified artifact stored |
| T18 | `update_page` | every state of 6.3 renders; a blocker disables the button with its reason; POST needs CSRF, same-origin, role and plan digest; a stale digest is refused; every write audited | CSRF missing; enabled with blockers; a changed plan applied |
| T19 | `cold_path_refusals` | contract change or wire change refused unless `--accept-failing-builds`; with it, stop-drain-start leaves open streams to finish | silent cold restart |

| E1 | `envoy_health_failure_keeps_streams` | on the pinned Envoy, an old backend that fails its health check and sends GOAWAY while 20 streams run for longer than the health interval: none is cut; new streams go to the new backend; the worker cluster's connections stay | a health failure cutting streams or worker connections (mutant: set `close_connections_on_host_health_failure`; health interval of 5 s leaves a window) |
| E2 | `envoy_retries_a_standing_down_backend` | a call that reaches the old instance after its stand-down is retried on the new one; the client sees no error | no retry policy (mutant: remove `retry_on`) |
| E3 | `envoy_hot_restart_keeps_old_streams` | a 20-minute stream through Envoy survives a hot restart with `--parent-shutdown-time-s` raised and is cut with the default 900 | the default left in place |
| E4 | `envoy_config_names_both_instances` | `kbf-deploy envoy check` fails when a port in Envoy's config differs from an instance's flags file, when the worker listener terminates TLS, or when a route has a finite timeout | a config that drifts from the units |
| T20 | `bazel_reattach_by_operation_name` | the pinned Bazel (the `m1` cell has one) and the old frontend killed mid-stream: Bazel reattaches with `WaitExecution` and completes or re-executes; buck2 fails its action (the documented difference) | a new frontend that cannot resolve the old one's operation names |

T5, T6 and E1 prove the model's one unproven step: if either cannot be made reliable the model is wrong and that is reported, not worked around.

## 8. Open questions

**[owner]** marks the owner's call; the rest are decided here and open to correction.

- **D1 [owner]** the writer model under Envoy (2.9): **lean (d)**, one writer at a time with the daemons moving with the baton, the old frontend a read-only relay; (b) Postgres when HA or several frontends are needed; (a) not recommended; (c) only if the old frontend must keep its daemons.
- **D2 [owner]** dispatch to not-yet-updated nodes (2.6). **Lean yes**, knob for the literal behaviour.
- **D4 [owner]** where Envoy runs and how it updates (2.5.4): hot restart with a raised `--parent-shutdown-time-s` on the frontend host; the Kubernetes kit has no hot restart. **Lean: single box under the hot restarter; Kubernetes with a long grace period; Envoy updates are never part of the frontend run.**
- **D3 [owner]** forced drain of the least-loaded node after `idle_wait` (2.6). **Lean yes**, one node at a time, `Pending` at the limit; without it a never-idle farm never updates.
- **Q1 [owner] Who signs.** The pull updater has an operator sign each release offline and hosts verify against a pinned root, so a compromised frontend cannot install anything unsigned. The direction here has the frontend poll and apply on a click, which needs a signer: (a) the frontend verifies the attestation and signs with a key it holds (one click; a frontend compromise is a fleet compromise); (b) the operator signs on their machine (strongest; "available" means attested, not signed, one command); (c) a separate signer. **Lean: (b) ships first; (a) opt-in per deployment.** A signed binary runs as the service user, which on a storage-and-daemon node holds the cold-store credential.
- **Q2 [owner]** `auto_frontend`. **Lean: manual until T5, T6 and C1 to C16 have been green nightly for a month**; the poller never changes it.
- **Q3 [owner]** Macs deferred when busy so one long UI test cannot hold the run (2.10). **Lean yes.**
- **Q4** State-schema migration (3.4) is a `kbf-state` change of a few days and is on the critical path of the first release that changes a column. Until then the planner refuses such a release.
- **Q5** Planned-down member state (2.6, step 4): **lean add it**; the router-failover alternative changes the write guarantee.
- **Q6** Node graceful stop and a `Leave` call. **Lean yes.**
- **Q7 [owner]** `auto_nodes` default `manual` until two click-through runs are clean.
- **Q8** One rollout record (K11). **Lean durable `update/*` rows.**
- **Q9** A nightly mixed-version job against the release before the last tag (T10), a precondition for auto.
- **Q10** The handover wire frozen at 1 (K12); say so in the doc that owns `handover.rs`.
- **Q11 [owner]** `--accept-failing-builds`: **lean keep**; without it a contract release has no way out.

## 9. Units, in dependency order

Each lands a test that fails first. Sizes (S under 1 day, M 1 to 3, L 3 to 5 days of agent time) are estimates.

| Unit | What | Size | Depends on |
|---|---|---|---|
| U0 | Land #857, #879, #887, #860; bring #803's engine into the controller's crate | merge | review |
| U1 | The probes first: E1, E2 and T6 (Envoy, GOAWAY and buck2), then T5 and T7 | S-M | U14 |
| U2 | `--version --json` fields (3.6); manifest `meta`; planner (order rule, refusals) | M | U0 |
| U3 | `open_writes` and `repair_debt` in the heartbeat; frontend aggregate and G_S; operator `down` (step 4); node graceful stop | M-L | U0 |
| U4 | `update/*` rows, the controller with resume, the real transport, the node step 2.6 and idle 2.7 (**the node-rolling unit**) | L | U2, U3 |
| U5 | Frontend phase on the helper: `activate` for two instances, E0 to E8, Settling | M | U4, pull-updater helper |
| U6 | Handover-gap metric and alert; the capacity floor and wait-limit rule (2.8) | S | U4 |
| U7 | The crash matrix of 5.3 with real processes | L | U4, U5 |
| U8 | State-schema expand/contract (3.4) and the row-encoding freeze test | M | none |
| U9 | Reduce the old frontend's exposure while Settling; carry per-operation memory across the handover or record the loss | M | U5 |
| U10 | Poller and policy | M | U2 |
| U11 | UI write path, page and buttons | L | U4, U10 |
| U12 | Mixed-version nightly and rollback test | M | U5 |
| U13 | Writer model (b): replace the generation fence with `session_gen`, a `draining` role, accept and serve at once in the new frontend | L | P9, P10 of `architecture-v2.md`, D1 |
| U14 | Envoy: the static config for two instances (REAPI h2, worker TCP proxy), health, retry and timeouts in `kbf-deploy envoy render`/`check`; the hold in the successor and removal of `SCM_RIGHTS`; E1 to E4 | L | U0 |

What this document does not claim: that any of it is built; that buck2 retries a request that races a GOAWAY (T6); that a node's
stop is graceful (Q6); that the repair gate has a metric to read (U3); that a restart fits inside the dwell (C30); or that the
pull updater exists.
