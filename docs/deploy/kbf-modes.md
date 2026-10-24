# All-in-one vs distributed

`kbf` is one binary with five modes. A farm is either one process on one host
(`all-in-one`) or several processes on several hosts (`frontend`, `storage`, `daemon` and
`storage-and-daemon`). This page says what each mode runs, which ports it uses and who
dials whom, when to choose which, how to grow from one host to many, and how each mode
stops. Every flag and default below is the one in the code; settings the page does not
name are in the config reference at kbf.dev/docs/config/.

The units that start each mode, and the rules `kbf-deploy` checks them against, are in
[kbf-units.md](kbf-units.md). Installing a host is [kbf-installer-setup.md](kbf-installer-setup.md).

## The modes

A mode is chosen with `kbf --mode=<mode>`, or with `mode = "..."` in `/etc/kbf/kbf.toml`
(`kbf` alone reads the file). `kbf --mode=<mode> --help` lists the mode's flags, and a mode
refuses by name the flags that belong to another.

| mode | what the process runs | listens on | state it keeps |
|---|---|---|---|
| `all-in-one` | the frontend and a build daemon, in one process | REAPI, worker listener, optional operator API | the scheduler's state store; blobs in a cold store it opens itself |
| `frontend` | REAPI, the scheduler, the worker listener, the operator API; no blobs | REAPI, worker listener, optional operator API, optional enrollment listener | the scheduler's state store |
| `storage` | the storage service: the cache and the metadata log | the storage port | blobs and the metadata log |
| `daemon` | a build node: it runs actions | nothing | a data directory of leases and input caches |
| `storage-and-daemon` | a storage node and a build node in one process | the storage port | as `storage` and `daemon` |

**`all-in-one`.** The frontend runs as `kbf frontend` would, except that it keeps its own
blobs, and the daemon runs beside it with the in-process data plane: its blobs move
through calls into the server half, not over the network. The daemon's control plane is a
remote daemon's: it dials the server half's worker listener on loopback over mutual TLS.
Linux only, container driver only: any other `--driver` is refused, and so is `--cas`.

**`frontend`.** It serves REAPI (Execute, the scheduler, operations) and the worker
listener's work streams, and it routes each cache call to a storage node. It holds no blobs
and refuses every flag of a store. `--storage` is required: the URL of one storage node.
`--state-store` is required too, a Postgres URL (a `sqlite://` one still starts in this
release; see "Growing from one host to many"): a frontend given none, by the flag or by `state-store` in `kbf.toml`, exits 2 naming the flag
before it listens on anything, and `kbf --mode=frontend --check` exits 2 the same way. With
the TLS flags the frontend keeps the farm's storage map and routes every call by it;
`--storage` is then only the bootstrap node.

**`storage`.** It serves the storage service over gRPC and nothing else: no REAPI, no
scheduler, no operator API (only `GET /metrics`, on `--metrics-listen`, off unless given).
It has no state store; it reaches one only through a frontend. Without `--frontend` it is a
farm of one member. With `--frontend <url>` it registers with that frontend, heartbeats to
it, and takes the storage map from it.

**`daemon`.** The build node. `--server` (required, repeatable) names the frontend's
worker listener. It holds no store and no credential. `--driver` is required: `container`
on Linux, `native` on a Mac.

**`storage-and-daemon`.** The `storage` and `daemon` flags in one command line, in one
process: the daemon half reads and writes the node's own storage through calls, with
no socket between. Linux only, container driver only. With `--frontend` the node registers
and the daemon routes each blob by the frontend's map: a blob this node owns goes in-process,
any other goes over mutual TLS to its owner. Without `--frontend` the node is the whole
farm.

### The one-port proxy

`kbf-proxy` is the public front door of a frontend host. It terminates TLS on one port,
checks the client certificate when one is presented, and routes each HTTP/2 stream (not
each connection) to a frontend of its pool on loopback, telling the frontend who the
caller is. REAPI, worker sessions, the operator API and enrollment all arrive on that one
port; a path the proxy does not know is answered by the proxy and never forwarded. The
proxy exists so that a frontend can be updated without failing a build: the frontends of
its pool (`[[proxy.frontend]]` tables of the `[proxy]` table) run on their own, and an
update drains one at a time through the proxy's admin API. The proxy starts, adopts and
swaps no frontend: the two-slot supervision it once had was removed, and a `[proxy]` table
with the `upstream` table is refused. It fronts `kbf --mode=frontend` only; an `all-in-one`
process cannot sit behind it. The update of a pool is described in
[kbf-updater.md](kbf-updater.md).

### The installer's role

`kbf-installer setup --mode=<mode>` makes a host ready for one mode: the accounts and
directories, `/etc/kbf/installer.toml` and `/etc/kbf/kbf.toml`, the first operator token on
a frontend or all-in-one host, the state database's schema, and the mode's units. A
`storage`, `daemon` or `storage-and-daemon` host is given the frontend's URL and an
enrollment token and enrolls itself at its first start. A `frontend` or `all-in-one` host
does not enroll; its certificates and addresses are the operator's, and `setup` takes them as
flags. A host from the packages can run the same verb as `kbf setup --mode=<mode>` (without the
proxy's flags). Details: [kbf-installer-setup.md](kbf-installer-setup.md).

## Ports and who dials whom

The defaults, from the flags themselves. A listener whose default is loopback is for one
host; a farm sets routable addresses.

| listener | flag | default | modes |
|---|---|---|---|
| REAPI | `--listen` | `127.0.0.1:8980` | `frontend`, `all-in-one` |
| worker listener | `--worker-listen` | `127.0.0.1:8981` | `frontend`, `all-in-one` |
| operator API | `--api-listen` | off unless given | `frontend`, `all-in-one` |
| enrollment | `--enroll-listen` | off unless given | `frontend` |
| storage service | `--storage-listen` | `127.0.0.1:8982` | `storage`, `storage-and-daemon` |
| storage metrics | `--metrics-listen` | off unless given | `storage`, `storage-and-daemon` |
| the proxy's public port | `ListenStream=` of `kbf-proxy.socket` | `8980` | proxy hosts |
| the proxy's frontends | `base_port` of each `[[proxy.frontend]]` table | none | proxy hosts |

Some defaults come from tools around the binary, not from `kbf`:

- `kbf-installer setup` writes `api-listen = 127.0.0.1:8982` into `kbf.toml` of a frontend
  or all-in-one host when it is not given. `kbf cluster --api` defaults to the same address.
- The units that `kbf-deploy` renders give a storage node port 8983 (`KBF_STORAGE_PORT`),
  because 8982 is the operator API's on a frontend host; the enrolled node takes its storage
  port from the frontend's configuration. The renderer's REAPI, worker and operator
  ports are 8980, 8981 and 8982.
- Behind the proxy, a frontend of the pool listens on its `base_port` (REAPI),
  `base_port + 1` (worker), `+ 2` (operator API), `+ 3` (admin calls, which a frontend no
  longer serves) and `+ 4` (enrollment); the `base_port`s of two frontends are at least five
  apart. `--instance` no longer moves a frontend's listeners: the flag is accepted and
  ignored.
- A daemon opens no listening port.

A listener serves plain text unless its TLS flags are given. The REAPI and worker listeners
accept plain text on any address, and off loopback the start line carries a warning. A
storage node refuses plain text off loopback.

### Who dials whom

| caller | callee | port | how it is secured |
|---|---|---|---|
| a REAPI client (Buck2) | the frontend, or the proxy | `--listen`, or the proxy's public port | TLS with the frontend's REAPI certificate; the REAPI authentication policy (`--reapi-auth-policy`) decides who may call |
| a daemon | the frontend's worker listener (`--server`) | `--worker-listen` | mutual TLS; the daemon's certificate names the role `daemon` |
| a daemon | the storage nodes the map names | each member's registered address | mutual TLS |
| a storage node | the frontend's worker listener (`--frontend`) | `--worker-listen` | mutual TLS; the certificate names the role `storage`, with names that cover the host of `--advertise-addr` |
| the frontend | the storage nodes | each member's registered address | mutual TLS; the frontend's certificate names the role `frontend` |
| a storage node | its peers | each peer's registered address | mutual TLS; copies a write to the blob's second owner |
| an operator or `kbf cluster` | the operator API | `--api-listen` | a write must come from a loopback peer (or through the proxy, with its secret) and carry the operator token as a bearer token; reads are open to whoever reaches the address |
| a new node | the enrollment listener | `--enroll-listen` | TLS that authenticates the server only; the node pins the CA from its token |
| a storage node | the cold store | the object store's endpoint | the S3 key pair, below |

Four things follow.

- Clients never talk to storage or to daemons; the frontend does that for them. Daemons
  and storage nodes never talk to clients.
- A storage node dials the frontend, and the frontend dials the node back at the address it
  registered. That address is `--advertise-addr`, or the address the node bound, which must
  then not be a wildcard.
- A daemon's blob traffic goes to storage directly once the frontend has sent it the map
  (in `Welcome`, and in each `MapUpdate`). A daemon-only node starts on its `--cas` leg to the
  frontend's worker endpoint until the map reaches it.
- The operator API is plain HTTP/JSON under `/v1`. With a proxy it is reached through the
  public port. Without one, bind `--api-listen` where only operators can reach it, and run
  `kbf cluster` on the frontend's host: a write from another host is refused (403).

### The cold store and its keys

The cold store is the durable home of every blob: a `file://` directory on one host, or an
`s3://` bucket for a farm.

- **Registration.** The operator registers the farm's store with the frontend
  (`/v1/object-stores/...`, or `kbf cluster object-store`). The routes exist when the
  frontend runs with `--object-store-secrets-dir`, which needs `--api-listen`,
  `--api-token-file` and `--state-store`. The endpoint, bucket and prefix are kept in the state
  store. The secret key is kept only in that directory, one file of mode 0600 per
  connection, and it never enters the state store, a backup of it, or `kbf.toml`. The
  endpoint must be `http://`: an `https://` endpoint is refused, because the S3 client does
  not speak TLS yet.
- **A node gets the keys by asking.** An enrolled storage node's `NodeConfig` carries the
  store's URI (`cold_store`) and never a key. After it registers with the frontend, the node
  calls `ColdStore.GetStoreConfig` on the worker listener, with its storage certificate, and
  opens its cache on the answer. It holds the key in memory only, with core dumps off. The
  registered store must be the store the node's `--cold-store` names (endpoint, bucket and
  prefix), or the node keeps its own and reports `identity_mismatch`. The keys are probed
  before the cache opens on them.
- **A rotation reaches a node at its next start.** There is no hot swap; until then the
  frontend shows the node as `pending`.
- **A fallback.** A node that has an `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` of its
  own gives up on the frontend after four tries and starts on those. A node without them
  waits for the frontend, as it waits for the registration.
- **Who never holds a key.** A daemon holds no store and no credential. A frontend serves no
  blob itself. In `all-in-one` the cold store is a directory in the units, so there is no key.

The units that `kbf-deploy` renders for `storage` and `storage-and-daemon` still give the key
pair through `EnvironmentFile=<KBF_ETC>/s3.env`; see [kbf-units.md](kbf-units.md).

## When to choose which

### All-in-one: one host

Choose `all-in-one` for a development box, a trial, or a small team that has one build host.
It is the least to run: one process, one unit, no enrollment, no registration. What it
cannot do:

- **No replication.** There is one storage member, so there is one copy of each blob; the
  replication factor is 2 but is clamped to the number of members. Losing the host's disk
  loses the cache and the action cache.
- **No high availability.** The host is the frontend, the storage and every builder. A
  restart of the process restarts all three.
- **It stops all at once.** A stop drains the frontend half, as below, and the daemon half
  goes with it; a deploy of a new `kbf` is a restart of the whole farm.
- **Builds run beside the farm's state.** The container driver runs each action as a
  subordinate id of the process's user, but no test yet proves an action cannot read what
  that user holds. Use it on a trusted host only, as
  [kbf-units.md](kbf-units.md) says.

### Distributed: a farm

Choose a farm for more than one build host, for any need to survive the loss of a host, or
to keep the scheduler's state on a host that runs no builds. A farm is one frontend (alone or behind
the proxy), storage nodes, and daemons: `storage-and-daemon` on the Linux hosts that have
disks and CPUs, and `daemon` on hosts that only build.

- **Replication.** The map places each blob on `r` owners, and `r` is 2. It is not a flag.
  A write goes to the primary and is copied to the second owner. With one storage member the
  map has one owner.
- **A member that is down.** A member's row lives `--member-ttl-secs` (15 by default, at least
  3) after its last heartbeat; nodes heartbeat at a third of that. Then the member is
  `down` in the map and writes go to the other owner. It stays in the map for
  `--member-dwell-secs` (300 by default); a member that registers again in that time takes its
  place back. After the dwell it **leaves** the map and its digests go to other owners.
  Both flags need `--state-store`.
- **A member that is restarted on purpose.** Waiting for the TTL costs a connection attempt
  per write and refuses action-cache commits. Instead, before the restart, run
  `kbf cluster member <id> --state planned-down [--for-secs N] --token-file F`
  (`POST /v1/members/{id}` with `{"state": "planned_down"}`). The member goes `down` at once,
  one new map generation, and the dwell is counted from that call. `--for-secs` defaults to
  900 and is at most 86400; a heartbeat after it has passed returns the member to `active`, so
  a restart that never happens strands nothing. The member registering again after the
  restart, or `--state active`, undoes it. `GET /v1/storage` shows `"planned_down": true`.
- **A draining member.** `--state draining` takes a member out of the write path without
  taking it `down`; it is read first. The state of every member is in `GET /v1/members`.
- **Macs.** A Mac is a node only: `daemon` with the `native` driver, enrolled like any other
  node. Macs build what Linux cannot (Xcode, iOS), and hold no blobs; `all-in-one` and
  `storage-and-daemon` refuse any driver but `container`, so a Mac is never one of those. Its
  `kbf.toml` is rendered by `kbf-mac-provision`. See
  [kbf-installer-setup.md](kbf-installer-setup.md) for the verbs.
- **A frontend is one process; several may share a store.** Frontends over one Postgres
  store share it and none fences another (see
  [state-store-cutover.md](state-store-cutover.md)); a SQLite file is held by one process, so
  only one frontend can use it. A farm with one frontend schedules nothing while it is down;
  with several behind the proxy, the others carry on, and a frontend is updated by a roll
  ([kbf-updater.md](kbf-updater.md)).
- **A `storage-and-daemon` host is for a trusted network.** Its unit runs as the build user,
  which can read the cold store's key from the process's environment, as `all-in-one`'s user
  can read the farm's state. Hosts that must not share a user with a key run `daemon` and
  `storage` apart.

Linux build hosts need `crun`, subordinate ids and a delegated cgroup; see
[linux-build-host.md](linux-build-host.md). Storage hosts, their disks and memory are in
[storage-node.md](storage-node.md).

## Growing from one host to many

There is no verb that converts an all-in-one host into a farm. You add the farm's parts and
move the host's role, in this order. What the code supports is below; what it does not is in
the `Planned:` paragraph.

1. **The state store.** `all-in-one` defaults to `postgres:///kbf?host=/run/postgresql`,
   the database `kbf` on a local socket, reached as the process's own user. It also accepts
   `sqlite:///absolute/path` for this release only: a start on it works and logs a warning
   that SQLite is removed in the next release (see the upgrade notes). A frontend has no
   default and refuses to start without `--state-store`. A farm's frontend uses Postgres:
   `kbf-installer setup` with no `--database` provisions the role and database in one run
   (the database `kbf` and the role `kbf-server` on the local socket; the schema follows once
   the server admits the logins, which on a stock server means a second run after the printed
   lines are added), prints the `pg_hba.conf` and `pg_ident.conf` lines the logins need,
   refuses with the command that installs Postgres when `psql` or the server is missing, and
   never edits either file; a role you made by hand is passed with `--database`. Postgres must
   flush commits before it acknowledges them (`synchronous_commit` on) or the start is
   refused. A URL with a password is refused: the command line is readable by every user on
   the host, so the role is reached by the socket and peer authentication. The state store
   also holds the storage members, the enrollment registry and the object-store registration.
2. **The CA and certificates.** `all-in-one` already needs mutual TLS between its two halves.
   A farm needs one CA and one certificate per node, each with exactly one role (`frontend`,
   `storage` or `daemon`, as `urn:kbf:role:<role>`). The frontend runs the enrollment
   listener: `--enroll-listen`, `--ca-dir`, `--frontend-name` (repeatable) and `--ca-signer`
   (the socket of the `kbf-signer` process that holds the CA key; `--ca-dev-file-signer` is
   for development). The operator makes a token with `POST /v1/enroll/tokens`, and a node
   enrolls with it at its first start and renews at half of its certificate's life (7 days by
   default, `--enroll-cert-lifetime-secs`). A node host needs no hand-issued certificate. The
   table of what each certificate must contain is in [kbf-units.md](kbf-units.md).
3. **The cold store.** `file://` stays on one host only. Choose an `s3://` store every
   storage node can reach, register it with the frontend as above, and give each node the
   same `--cold-store` URI. A fresh prefix is a fresh cold store.
4. **The frontend.** Run `kbf --mode=frontend` on its own host, or behind the proxy (a pool of
   frontends given as `[[proxy.frontend]]` tables, written by hand: `kbf-installer setup` sets up
   no proxy). Bind `--listen` and `--worker-listen`
   to addresses the nodes reach, give them the TLS flags, and set `--storage` (required) to
   the first storage node, with `--storage-client-cert`, `--storage-client-key` and
   `--storage-ca` for the mutual TLS to it. Add `--api-listen` and `--api-token-file` for
   operators.
5. **The nodes.** On each host: `kbf-installer setup --mode=storage-and-daemon` (or `daemon`)
   with `--frontend-url` and `--token-file`. The node takes its flags from `kbf.toml` and the
   frontend's configuration: `--frontend` (the worker endpoint), `--advertise-addr`,
   `--storage-listen` on all addresses at the storage port, the three certificate flags,
   `--hot-store`, `--meta-dir` and, for a daemon, `--server`. `kbf --mode=<mode> --check`
   checks the host and exits.
6. **Promote the new members.** In a farm formed more than a minute ago, a node it has not
   seen before registers as `joining` and stays so until `kbf cluster member <id> --state
   active`, unless the frontend runs with `--auto-promote`. Promoting does not copy any blob
   to the member. Read the risk in the operator API's section on `POST /v1/members/{id}` in
   `docs/api.md` before you promote a joiner whose slice is not empty.
7. **Move the old host.** Stop `all-in-one` (see below), then start the same host as
   `storage-and-daemon`, or as `frontend`. Do not run `all-in-one` and a frontend on one host
   at once: both bind `--listen` and `--worker-listen` at the same defaults.

What changes in the flags: the frontend gains `--storage`, `--storage-ca` and its client
pair, `--enroll-listen` and its companions, and `--object-store-secrets-dir`, and loses
every flag of a store. The daemon moves to its own process and takes `--server` for the
worker listener and no `--cas` when it is in a `storage-and-daemon` process. The storage
half gains `--frontend`, `--advertise-addr`, `--storage-zone` and `--storage-weight`.

**Planned:** a conversion of the blobs, the action cache and the queue of an `all-in-one`
host into a farm. Today the farm starts with an empty cache and its own scheduler state;
in-flight work does not move, and the farm's new state store is given an epoch of its own,
so the daemons that reconnect see an epoch they did not know and kill the runs of the
leases they held before. Copying a joining member's slice to it
(the join handoff, architecture-v2.md section 8.4) is not built either.

## Stopping and draining

| mode | on SIGTERM or SIGINT | what bounds it |
|---|---|---|
| `frontend` | drains, by the sequence below | `--drain-lb-wait` (10 s by default) and `--drain-deadline` (900 s by default); `--shutdown-timeout-secs` (0 by default: no deadline); a second signal |
| `all-in-one` | the frontend half drains; the daemon half does not | as `frontend`; the daemon half stops when the server half has |
| `storage` | stops | none of its own |
| `daemon` | drains | `--drain-deadline-secs` (600 by default); a second signal |
| `storage-and-daemon` | drains the daemon half, then stops storage | `--drain-deadline-secs` (600 by default); a second signal |

**The frontend.** Buck2 does not recover an Execute stream that breaks after it started, so
a stop that cuts a stream fails its action. On the first signal, or on
`POST /v1/frontend/drain` (`kbf cluster drain --frontend --token-file F`), the frontend
(which has a state store) takes itself out of any load balancer in five steps
([kbf-units.md](kbf-units.md#draining-a-frontend-behind-a-load-balancer)): `/readyz` turns to
503 while everything is still served; after `--drain-lb-wait` it refuses each new Execute with
`UNAVAILABLE` (which Buck2 retries), takes no more work from the state store, and sends
GOAWAY on the REAPI, worker and operator API listeners; it keeps every open Execute and
WaitExecution stream until its action is done, at most `--drain-deadline`; then it ends the
worker streams and the streams left `UNAVAILABLE` and exits 0. `GET /v1/frontend/drain`
reports the phase. A second signal skips the waits, and `--shutdown-timeout-secs` running
out does the same. A frontend with no state store (a test setup) drains at once: `/readyz`
503, new Execute calls refused, GOAWAY on the REAPI listener, and exit 0 once the REAPI
connections have closed. Under systemd that needs `TimeoutStopSec=infinity`; the units that
`kbf-deploy` renders set it.

**`all-in-one`.** The process has the frontend's drain and no drain of its own for builds.
`--drain-deadline-secs` is used by `daemon` and `storage-and-daemon`, and `all-in-one` stops
its daemon half with the server half: the daemon keeps running leases while the server half
drains, and when the server half has stopped the daemon stops and abandons the leases still
running. Updating an `all-in-one` host therefore waits for every build to finish or cuts it.

**`storage-and-daemon`.** The signal handlers are installed before anything starts. On the
first signal the daemon half takes no new lease, lets the running ones finish and report, and
waits for their acknowledgements, for at most `--drain-deadline-secs`. A second signal ends
the drain at once and abandons the leases still running. Only then does the storage half
stop, because a running action reads and writes its blobs through it, and the process exits
0 (kbf#1087). A signal while the storage half is still starting, for instance waiting for
a frontend that is down, abandons the start and exits 0.

**`daemon` and `storage`.** A daemon drains as above and then exits 0. A storage node serves
until the signal and stops.

A stop of a storage member that is not planned makes the frontend wait for the TTL before it
marks the member `down`; use `planned-down` first, as above. To empty a build node before
maintenance without stopping it, `kbf cluster drain <node>`.

## Related pages

- [kbf-units.md](kbf-units.md): each mode's packaged systemd unit and how it is checked.
- [kbf-installer-setup.md](kbf-installer-setup.md): `kbf-installer setup`, the state database,
  the proxy host and backup.
- [update-a-host.md](update-a-host.md): updating a host.
- [storage-node.md](storage-node.md): storage hosts, disks, memory and the cold store on a
  single host.
- [linux-build-host.md](linux-build-host.md): what a Linux build host installs and gives
  the daemon.
- The operator API, with its gates, in [api.md](../api.md).
