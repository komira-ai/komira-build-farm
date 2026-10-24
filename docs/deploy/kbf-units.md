# The units of each `kbf` mode

Each mode of `kbf` has one static systemd unit, shipped in the `kbf-installer` package
(`packaging/linux/units/`: `kbf-frontend.service`, `kbf-storage.service`,
`kbf-daemon.service`, `kbf-storage-and-daemon.service`, `kbf-all-in-one.service` and the
shared `kbf.slice`; the modes are described in [All-in-one vs distributed](kbf-modes.md);
[architecture-v2.md](../design/architecture-v2.md), sections 1 and 11). The files carry no
host values: the mode's flags are in `/etc/kbf/kbf.toml`, which `kbf-installer setup`
writes ([kbf-installer-setup.md](kbf-installer-setup.md)), and the units are installed
disabled and enabled for the host's mode by `kbf-installer`. `kbf-deploy linux render-units`,
`check-units` and `install-units` rendered, checked and installed units from a settings
file; they were retired, and nothing renders a unit now.

## Choosing the mode: `--mode`, `kbf.toml`, enrollment

A `kbf` process takes its mode from a flag, never from a word: `kbf --mode=frontend|storage|daemon|storage-and-daemon|all-in-one`,
or `mode = "..."` in `/etc/kbf/kbf.toml` (`kbf` alone reads it). The flag wins over the file and
the start says so on stderr; with neither the start exits 2 naming both. `kbf --mode=<mode> --help`
lists the mode's flags. There is no `node` mode and no `enroll` word.

- **Flags.** A start that gives flags of its mode (`--cold-store`, `--server`, ...) runs on them
  and does not read the file. (The packaged units give `--mode=<mode>` and read the file.)
- **`kbf.toml`.** `kbf --mode=storage|daemon|storage-and-daemon` with at most `--config FILE`
  reads the host's file. On the first start, with the token file `enroll.token_file`
  names, it makes its keys, sends the token and a request for the roles the mode needs
  (`storage` the storage role, `daemon` the daemon role, `storage-and-daemon` both) and writes
  `node.json`; later starts renew the certificates without the token. `[storage]` and `[daemon]`
  in the file are settings only.
- **Exit 2** (a restart cannot fix it): a bad `kbf.toml`, a missing or open token file, a token
  that does not grant a role the mode needs (the frontend's typed reason is printed), and a
  `node.json` whose roles do not cover the mode (the mode changed after enrollment; the node is
  never enrolled again on its own). **Exit 1** (may pass): the frontend is down or busy, the
  clock is off; the start waits 5 seconds, doubling to 5 minutes, and tries again.
- **`--check`** (`storage`, `daemon`, `storage-and-daemon`): checks the host and exits without
  starting or enrolling anything. 0 for a valid `kbf.toml` with its token file and no `node.json`
  yet, or an enrolled node that covers the mode; 2 for a bad config, a missing token file or a
  `node.json` that does not cover the mode.

## How the units are checked

- **The files.** `crates/kbf-integration-test/tests/package_units.rs` reads every packaged
  unit with the same unit parser the docs check uses (`kbf_unitfile`, systemd's reading:
  continuations, the last assignment wins, an empty assignment resets a list) and holds it to
  the rules below. Each rule has a mutant in that test (a defect planted in a copy of the real
  file that the rule must name), and a rule added without one fails the test.
- **The host.** `kbf --mode=<mode> --check`, which the units run as an `ExecStartPre=` (and
  every start runs again), reads what only the host knows, as the process that will run: the
  directories above a `file://` `--hot-store` can be entered (`traverse`), the cgroup keeps
  every CPU and NUMA node of the node (`cpuset`), and a build node's environment holds no
  `AWS_*` variable and the cold store's key file, if there is one, is owned and moded as its mode
  needs (`credential`). The code is `crates/kbf-cli/src/preflight`; a problem prints on standard
  error and exits 2 (a restart cannot fix it) or 75 (it may heal).
- **The flags.** Whether `kbf` takes a unit's flags is `kbf --mode=<mode> --check`'s
  answer, and `crates/kbf-cli/tests/package_units.rs` parses each packaged unit's flags with
  `kbf`'s own parser.

Drop-ins on the host (`/etc/systemd/system/<unit>.d/*.conf`) are the operator's and are not
checked by the package test.

## The rules the packaged units follow

| rule | what it refuses |
|---|---|
| `continuation` | a comment line inside a continued directive (below) |
| `exec` | an `ExecStart=` that runs a program outside the installer's tree, or another mode than the unit's name |
| `slice` | a unit not in `kbf.slice` |
| `delegate` | a mode that runs builds without `cpu`, `cpuset`, `memory` and `pids` delegated, or with one of them in `DisableControllers=` |
| `cpuset` | `AllowedCPUs=` or `AllowedMemoryNodes=` on a mode that runs builds, or on `kbf.slice`: the daemon places leases on every CPU and NUMA node of the node |
| `caps` | a mode that runs builds (`daemon`, `storage-and-daemon`, `all-in-one`) with `MemoryMax=`, `MemoryHigh=`, `CPUQuota=` or `AllowedCPUs=` on its unit: builds are capped by the daemon's own `actions/memory.max`, and a limit on the unit caps the supervisor with them |
| `swap` | a mode that runs builds with `MemorySwapMax=` on its unit: its leases live in the unit's cgroup and may swap |
| `oom` | a mode that runs builds without an integer `OOMScoreAdjust=` below 0, the build containers' score (a host-wide OOM kill takes the highest score first) |
| `credential` | a key given to the unit any way but `kbf.toml`: an `EnvironmentFile=`, an `Environment=` naming a key, secret, token or password, a systemd credential, or an ordering after a MinIO unit |
| `restart` | a mode unit without `Restart=on-failure`, `RestartSec=5` and, in `[Unit]`, `StartLimitIntervalSec=0`: systemd's defaults (100 ms, 5 starts in 10 s) end a quickly repeating failure in `start-limit-hit` with the unit down. `on-failure` restarts no clean exit, so a frontend that was drained and stopped by an operator stays down |
| `stop` | a mode that drains on SIGTERM (every mode but `daemon`, which has a finite stop) without `TimeoutStopSec=infinity` and `KillSignal=SIGTERM` (below) |
| `kill-mode` | a mode that runs builds (`daemon`, `storage-and-daemon`, `all-in-one`) without `KillMode=mixed`: with the default `control-group` a stop sends SIGTERM to the actions in their sub-cgroups together with the daemon, and the drain ends at once |

The test has more rules than these (`install`, `network`, `gate-*`, `user`, `nofile`,
`notify`, `conflicts`, `memory`, ...); its header lists what each catches.

**`delegate`.** systemd reads `Delegate=` this way (systemd.resource-control(5), and
`config_parse_delegate` in systemd's `src/core/load-fragment.c`): a boolean delegates
every controller (`yes`) or none (`no`); a list adds its controllers to the ones
already delegated; an empty value turns delegation on with no controller. So
`Delegate=yes` followed by `Delegate=memory` still delegates every controller, cpuset
among them, and the rule passes it. A slice cannot delegate: systemd enables on
`kbf.slice` every controller a unit under it delegates, unless the slice's
`DisableControllers=` lists it. The slice sets only `MemoryMin=`.

**`continuation`.** systemd.syntax(7): "When a comment line or lines follow a line
ending with a backslash, the comment block is ignored, so the continued line is
concatenated with whatever follows the comment block." That holds from systemd 241.
systemd 240 skips such a comment only when the `#` starts the line, and 239 and
older join the comment into the value (`config_parse` in `src/shared/conf-parser.c`
before the change that fixed systemd issue 10598), so its words become arguments of an
`ExecStart=`. The units put every comment above its directive, and the rule refuses
one inside.

## What each mode's unit does

A mode's flags are its section of `kbf.toml` (`kbf --mode=<mode> --help` lists them;
`crates/kbf-cli/tests/golden/kbf-<mode>.toml` is one full file per mode); the unit only
names the mode, the user it runs as, its limits and its restart and stop behavior. A
daemon's `--server` must name the frontend (the name in the frontend's worker certificate),
not the node itself: dialing its own address, where nothing listens, it never registers.

### A listen address that appears late

`network-online.target` does not wait for a VPN or overlay interface, so a listen address
on one (a listener's address in `kbf.toml`) can be missing for a few seconds after boot, and the bind
fails with "address not available". kbf retries that one error, and only that one, for up
to 90 seconds (250 ms, doubling to 2 s between attempts, each wait logged with the
address), below the units' `TimeoutStartSec=120`; every other bind error (address in use,
permission) is at once fatal. This covers every listener of `frontend`, `storage`,
`storage-and-daemon` and `all-in-one`. To order a unit after the service that brings the
address up (`tailscaled.service`, say), put `After=` and `Wants=` in a systemd drop-in on
the host, `/etc/systemd/system/<unit>.d/after.conf`; the packaged units carry no option for it.

### A frontend name that does not resolve yet

The frontend's name (a daemon's `--server`, a storage node's `--frontend`) can be a name that a VPN or overlay network's resolver answers, and
that resolver may start after `network-online.target`. A failed lookup at start does not end
either half of `storage-and-daemon`: the storage half's channel to `--frontend` is lazy, so
the lookup is part of its first registration, and a failed registration waits (1 s, doubling
to 30 s, each wait cut by a random share of up to half, as the daemon's is, so nodes that lost
one frontend do not return in step) and tries again until the frontend answers, before it opens its
cache; the daemon half resolves `--server` at every connection round, reports a failed
lookup as a failed attempt, waits and resolves again, and never stops trying. (`daemon`
does the same.) A node that cannot find its frontend therefore stays up and logs each
failure; it is not restarted by systemd, and needs no restart when the resolver comes up.
To order the unit after the resolver's service anyway, so the first tries are not
wasted, use a drop-in on the host (`/etc/systemd/system/<unit>.d/after.conf` with
`After=tailscaled.service` and `Wants=tailscaled.service`, say).

`storage` and `storage-and-daemon` units are not ordered after a MinIO unit: their cold
store is an `s3://` endpoint that need not be on the host, and `After=` on a unit that
does not exist does nothing. On a host with a local MinIO, order the unit after it in a drop-in
(`/etc/systemd/system/<unit>.d/after.conf` with `After=` and `Wants=`). (The converged `kbf-server.service` still starts after `MINIO_UNIT`.)

### Exit 2 and exit 75: what systemd restarts

A start that fails exits 2 or 75. The `storage`, `storage-and-daemon` and `all-in-one`
units carry `RestartPreventExitStatus=2` and `RestartSec=5`, so exit 2 stays down and exit 75
(`EX_TEMPFAIL`) is started again five seconds later, below systemd's default start limit
(five starts in ten seconds).

- **Exit 2, refused.** The default for any failure kbf does not name below: a flag, a
  certificate or key path that cannot be read, a metadata directory that is not a log, a
  log of several voters, a path or permission that is wrong. A restart reads the same
  bytes and fails the same way.
- **Exit 75, not ready.** Named by the failure's type, never by its text: the metadata
  or cold-store directory held by a process that is still stopping (its `LOCK`), a file
  system call that failed because the device was busy, read-only or full, and a
  registration whose boot the node's metadata log has set already (`StaleWriter`, below: only a node that did not report its history, or one past the clamp).
  A frontend or daemon that fails to start is not covered: `frontend` and `kbf-server`
  exit 2 for every start error, and the daemon half's own codes are unchanged.

**A new or restored state store, and the nodes' boots.** The frontend hands each node its
member id and a boot counter from the store (`boot/<id>`, one more at every registration);
a node's metadata log remembers the latest boot it wrote as, per member, and refuses a boot
that is not after it ("member M boot B is not after boot C, the latest set", no object is
written). A store that counts from 1 (a fresh Postgres, a copy restored from before the
node's later boots) would hand a node that has run before a boot its log has set already.
So a registration carries the member id and boot the node's log last wrote as, and the
store answers with a boot above it: a new instance is given the member id its log names if
no other instance holds it, with its counter raised past the reported boot in the same
transaction that creates the instance (and its joiner mark, when the farm holds data); a
known instance keeps its id and its counter is raised above the log's if it was behind.
The node opens on its first registration, with no restart, and counters are only ever
raised. Two cases still cost restarts: a node that names an id another instance holds is
given a new id (which its log has not set, so it opens at once, but its history under the
old id is not continued), and a node of a build before the report, or one whose boot is
past the clamp of 4096 the frontend raises a counter by in one registration, is refused a
boot its log has set, says on stderr that the state store is older than its log and to
wait, and exits 75. systemd starts it again; that start registers again and takes the next
boot, so a gap of N boots takes N restarts, about N times five seconds plus the start, each
node on its own counter and all in parallel. Restarting the unit by hand only spends one
boot sooner. Do not wipe a node's `--meta-dir` to skip the wait: that throws its index
away. The frontend cannot refuse to start from a store older than its nodes' boots,
because it learns a node's boot only when the node registers, after the frontend is up.

### The state store: Postgres for the frontend

The frontend's state store is Postgres (ruling: SQLite is dropped for it). The frontend
gets `--state-store`, a `postgres://` URI, from `kbf.toml` (`kbf-installer setup` writes it,
as `postgres:///kbf?host=/run/postgresql&user=kbf-server` unless `--database` gives another,
and makes that role and database on the host), and it has no default: a
frontend given none exits 2 naming the flag, and `kbf --mode=frontend --check`, which the
unit runs before the start, exits 2 the same way. (`all-in-one` alone has a default, the
host's socket, `postgres:///kbf?host=/run/postgresql`.) A `sqlite://` URI still starts in
this release and logs a warning that SQLite is removed in the next (see the upgrade notes).
The URI carries no password (kbf refuses one: the command line is readable by every user on
the node), so the database is reached by the socket or by the server user's own role. The
database and its schema are `kbf-installer`'s.

The unit gives no `--handover-socket` (nor a `RuntimeDirectory=` for one): `kbf frontend` has
no handover any more, and the flag, like `--instance`, is accepted and ignored with a
warning. The proxy of `docs/design/frontend-proxy.md` swaps nothing in its place: it balances
over frontends that run on their own. An update of a frontend is a restart; behind the proxy
it is a roll of one frontend at a time (below).

**The epoch, and a restore from a backup.** The state store holds the farm's epoch
(`meta/epoch`), and a frontend that starts keeps it: a restart of a frontend, or the start
of another one over the same store (several frontends sharing one Postgres), does not change
it, so a daemon that reconnects keeps the runs of the leases it holds. The ids are kept the
same way: a frontend reserves blocks of 1024 operation ids and lease sequences in the store
and numbers inside them, so a restarted or a second frontend never hands out an id another
took, and a restart skips the unused rest of its block.

A database restored from a backup holds the epoch of the backup, while the daemons may hold
leases the lost history granted after it. Raise the epoch so they drop those leases:

1. Restore the database and start the frontend over it. No daemon may reach any frontend
   of the store until the restart in step 3 is done: keep the daemons' units stopped, or the
   frontend's worker listener closed. The frontend serves the restored epoch and numbers
   leases from the restored counters, so a daemon that reaches it meanwhile keeps its runs
   and can be granted a lease id (epoch and sequence) that the lost history granted to
   another run.
2. On the frontend's host, run `kbf cluster epoch-bump --token-file F` (`POST
   /v1/epoch/bump`, [api.md](../api.md#post-v1epochbump)). It answers the epoch before the
   bump, the one stored now, and the one this frontend serves.
3. Restart each frontend. A running frontend goes on serving the epoch it started under
   (it logs a warning once its next look at the store finds the newer one); the
   one that starts after the bump serves the stored one, and a daemon that reaches it kills
   the runs of the leases it held and lists none, and the operations run again from the
   restored queue. Start the daemons' units after it.

Bump only when the history was lost or replaced: a bump with no restore costs every run in
flight.

A SQLite state file (`all-in-one`, or a single frontend) is restored with `kbf-deploy state
restore --apply`, and its start keeps the epoch too, so the same steps follow it: start the
frontend over the restored file with no daemon able to reach it, run `kbf cluster
epoch-bump`, then restart the frontend.

### Updating a frontend without failing a build

A frontend runs as `kbf-frontend.service`, one process; an update of it is a restart, and the
stop drains (below). Frontends that share one Postgres state store serve at once and none
fences another, so a farm that must not fail a build keeps more than one and puts
`kbf-proxy` in front of them (`docs/design/frontend-proxy.md`). `kbf-updater` then rolls
them one at a time: it drains a frontend at the proxy, waits until its streams are 0,
retires it (`POST /v1/frontend/retire`, [api.md](../api.md#get-v1frontendretire-and-post-v1frontendretire)),
installs the package, restarts the unit, waits until the proxy lists it ready on the new
commit, and undrains it ([kbf-updater.md](kbf-updater.md)). The proxy starts, adopts and
swaps no frontend, and `kbf-server` has no control socket.

Removed: the socket-handover template (two instances, `kbf-frontend@a` and `kbf-frontend@b`),
the `kbf frontend handover` command that started the idle one, the proxy's two-slot
supervision of frontend children, and the handover code of `kbf-server` with its admin port.
The kbf-installer package no longer ships the template, and no step of this guide enables it.
A pool member beyond the plain `kbf-frontend.service` (the member named `-` in the
inventory) is run from a `kbf-frontend@<name>.service` unit the operator supplies; the updater
restarts that unit ([kbf-updater.md](kbf-updater.md)).
The flags that turned them on (`--handover-socket`, `--instance` and the rest, listed in
[api.md](../api.md#the-admin-calls-removed)) are accepted and ignored with a warning for one
release.

**A frontend and its storage node at a cold start.** The frontend sends `READY=1` (the unit is
`Type=notify`) once it serves and its state store and map probes pass, so `systemctl start`
returns when the frontend can take builds, and fails, at `TimeoutStartSec=120`, when it could
not. It does not wait for the `--storage` node: at a cold power-up the node may come up after
the frontend, and a frontend that waited would be killed at `TimeoutStartSec` and restart, in a
loop. `/readyz` still fails with `storage` listed until the node answers: the proxy sends
nothing to a frontend that would fail every blob call while another is ready, and the frontend
keeps its listeners and its state, so it is in rotation again as soon as the node is back, with
no restart.

### Draining a frontend behind a load balancer

A frontend over a state store drains on SIGTERM, and on `POST /v1/frontend/drain`, in an
order that needs nothing of kbf in front of it: Envoy, HAProxy or a Kubernetes Service takes
it out on its own health check and its connections' GOAWAY (the order and each phase are in
[api.md](../api.md#get-v1frontenddrain-and-post-v1frontenddrain)).

1. `/readyz` answers `503` at once; everything is still served.
2. For `--drain-lb-wait` seconds (default 10, 0 allowed) nothing more happens. Set it to at
   least the load balancer's check interval times its unhealthy threshold, plus the time
   its routes take to change. For Envoy that is `interval` times `unhealthy_threshold` and a
   second or two (1 s times 2, so 10 s has margin); for a pod, the readiness probe's
   `periodSeconds` times `failureThreshold` plus the endpoints' propagation. Unary calls
   have no client retry, which is why they are served here.
3. New Execute calls are refused `UNAVAILABLE` (Buck2 retries them), the frontend takes no
   more queued work from the state store and places none on its nodes, and the REAPI,
   worker and operator API listeners are sent GOAWAY.
4. The frontend waits until no execution is open, at most `--drain-deadline` seconds
   (default 900, at least 1).
5. The worker streams that are left end `UNAVAILABLE` (daemons dial another frontend through
   the load balancer, which keeps their leases), the streams still open end `UNAVAILABLE`,
   and the process exits 0. It is not undone: what starts in its place is a new process.

A build whose action runs longer than the deadline fails when the deadline comes (Buck2
never re-attaches). Update in a quiet window, or raise `--drain-deadline` above the longest
action. The stop timeout of whatever runs the process must be longer than the two waits and
is never what ends the drain: `TimeoutStopSec=infinity` for the unit (below); for a Docker
container `stop_grace_period`, and for a pod `terminationGracePeriodSeconds`, set to
`--drain-lb-wait` plus `--drain-deadline` plus 5 minutes (a pod needs no `preStop` sleep: the
wait is the frontend's own, from the SIGTERM). A frontend that is the only one has nothing
to hand its work to: its queued operations that have not started wait with the rest.

### Stopping a frontend: TimeoutStopSec=infinity

On SIGTERM kbf's frontend drains (the sequence above; `all-in-one`'s drains at once: it
refuses new Execute calls, which Buck2 retries elsewhere, and keeps every open Execute and
WaitExecution stream until its action is done); `--shutdown-timeout-secs` is 0, no
deadline, by default, and a frontend over a state store is bounded by `--drain-deadline`. A
`--shutdown-timeout-secs` above 0 ends a drain that a signal began when it runs out, as a
second signal does, even if that is before the wait or the deadline: leave it at 0.
Buck2 never recovers a stream that breaks mid-action, so a stop that ends the drain
fails every build in flight. systemd kills a unit with SIGKILL when `TimeoutStopSec=`
runs out, 90 s (`DefaultTimeoutStopSec=`) when the unit gives none. So both units have
`KillSignal=SIGTERM` and `TimeoutStopSec=infinity` (so do `storage` and
`storage-and-daemon`: a storage node finishes its large Writes at stop, and systemd's
90 s default would cut them), and rule `stop` of the package test refuses either
missing or changed: it reads `TimeoutSec=` too (it sets the stop timeout as well; the
last of the two wins) and the drop-ins. It asks for the word `infinity`: `0` means no
timeout to systemd as well, but reads as "at once" to a person. A daemon is not held for ever:
it drains on SIGTERM up to `--drain-deadline-secs` (600 by default), then stops, so its
unit has `KillSignal=SIGTERM` and a finite `TimeoutStopSec=660` (the deadline plus 60 s);
a hung daemon still lets a reboot through.

The three units that run the daemon (`daemon`, `storage-and-daemon`, `all-in-one`) also
set `KillMode=mixed`. The actions run in sub-cgroups of the unit, and the default
`control-group` sends SIGTERM to every process in it, so a stop would end them within
seconds and defeat the drain. With `mixed` SIGTERM goes to the daemon alone; if the
drain deadline passes, the daemon ends the leases still running itself, and systemd
sends SIGKILL only to what remains after the main process has exited.

### storage-and-daemon and the cold store's key

One unit and one process (the daemon half reads and writes the node's storage
in-process, one cache on the SSD; architecture-v2.md, sections 1 and 10). The unit reads
the cold store's key from no `EnvironmentFile=`: `kbf.toml` names where `kbf` gets it, and
rule `credential` of the package test refuses every other way of giving a key to a unit.
On the node, `kbf --mode=storage-and-daemon --check` refuses a key file another user owns
or others can read (`crates/kbf-cli/src/preflight/credential.rs`).

The design keeps the key from builds by uid (sections 9 and 15): the key file the unit's
user's alone and each action a subordinate id of that user (the container driver's
`--userns=nomap`), with a test that an action cannot read the key. That test does not
exist yet, and the unit runs as the build user, which can read the key from the process's
environment. So this unit is for a trusted network only, as all-in-one is.

## all-in-one is for a trusted box

`kbf all-in-one` runs builds in the process that holds the farm's state and its cold
store, as one user. The container driver runs each action as a subordinate id of that
user (`--userns=nomap`), but no test yet proves an action cannot read what the user
holds (architecture-v2.md, section 9). Until one does, use it on a trusted single box or
for development only.

Its cold store is a `file://` directory, the single box's shape (section 11), so
the unit holds no cold-store key and rule `credential` applies to it. The default
state store of `kbf all-in-one` is Postgres on the host's socket
(`postgres:///kbf?host=/run/postgresql`).

## A node's certificate

`kbf-deploy` issues no certificate. The farm's one issuer makes every certificate of the
new modes' mutual TLS from the one CA, each with exactly one role, by this table:

| row | role (`urn:kbf:role:<role>`) | extended key usage | names |
|---|---|---|---|
| `frontend-worker` | `frontend` | serverAuth, clientAuth (it also calls storage) | the frontend's names |
| `storage` | `storage` | serverAuth, clientAuth (replication calls) | the node's names: DNS or IP names covering the host of `--advertise-addr` (the bound address without it), which is also the node's instance id; a frontend refuses `Register` and `Heartbeat` from a storage certificate that does not cover it. A wildcard name covers nothing |
| `daemon` | `daemon` | clientAuth | exactly one DNS name, the node id |

The frontend's REAPI listener, the name clients dial, presents a certificate that is not
the farm CA's, so it has no row and `verify-cert --row frontend-reapi` is refused (exit 2).

`kbf-deploy linux verify-cert` checks one certificate against its row, read only,
before a unit is pointed at it:

```text
kbf-deploy linux verify-cert --row storage --cert storage.pem --ca-cert ca.pem
kbf-deploy linux verify-cert --row daemon --cert node.pem --ca-cert ca.pem --node-id node-1
```

- `role`: the subjectAltName holds exactly one URI `urn:kbf:role:<role>`, the row's.
  A storage node reads the same URI to admit each call (architecture-v2.md, section 9);
  a kbf-cli test makes a certificate of each row and reads it back with the storage
  node's own check.
- `eku`: the extended key usage is the row's set exactly; a certificate without the
  extension fails (TLS would take it for every use).
- `name` (`daemon`): its DNS and IP names are exactly `--node-id`. For `storage` nothing
  is compared: `verify-cert` does not check that the names cover the host of
  `--advertise-addr`, so it prints `CERT OK` for a storage certificate a frontend refuses
  at `Register`. Check that by hand before pointing a unit at it:
  `openssl x509 -noout -ext subjectAltName -in storage.pem` must list a `DNS:` or
  `IP Address:` entry equal to that host (a DNS name matches up to ASCII case; a
  `DNS:*.farm.example` entry covers nothing).
- `chain`: it chains to `--ca-cert` now, for each use of the row: as a server under its
  first name, as a client.

It prints a `FAIL <check>:` line per failed check and `CERT BROKEN`, or one
`CERT OK:` line. Exit codes: 0 the certificate is the row's, 1 a check failed, 2 refused
(a file that cannot be read, no certificate, no usable CA, `--node-id` for a row that
is not `daemon`).

Put the certificate and its key where `kbf.toml` names them (`/etc/kbf` by the installer's
layout), the key owned by the unit's user; the start refuses a key another user owns or
others can read (`kbf --mode=<mode> --check`). The frontend reads its worker pair (the
`frontend-worker` row, which is also its client identity toward storage), the CA, and the
REAPI pair, which is not the farm CA's.

## The config file and `--check`

Every mode reads its settings from `/etc/kbf/kbf.toml` (or the file `--config FILE`
names) as well as from the command line. The file is the mode's flags and nothing else:

- one TOML table per setting group (`[listener]`, `[tls]`, `[stores]`, `[identity]`, ...),
  one key per flag, the key being the flag's long name without the dashes in front
  (`--storage-listen` is `storage-listen = "127.0.0.1:9982"` under `[listener]`);
- a value is what the flag takes: a string or a number, `true` for a switch, an array
  for a flag that may be given more than once (`server = ["https://a:8981", "https://b:8981"]`). Names,
  validation and defaults are the flags' own, because the file is turned into the flags
  it stands for before clap parses the command line;
- a flag on the command line replaces the file's key for it;
- a key that is no flag of the mode, or sits in another group's table, is refused naming
  the key and the file; so is a flag the mode refuses on the command line (`--cold-store`
  for `kbf frontend`);
- the file must be a regular file that neither group nor other can write;
- a secret is never a value in the file: the keys that name a key, token or policy hold
  the path of the file that holds it.

When `--config` is not given, `/etc/kbf/kbf.toml` is read if it exists. A file named
with `--config` that does not exist is refused.

`tests/golden/kbf-<mode>.toml` in `crates/kbf-cli` is one full file per mode, generated
from the mode's flag definitions: a setting with a default is a live line holding it, one
without is a comment showing its shape. A flag added to a mode changes its golden, and
fails a test until it has a group.

`kbf <mode> --check` parses the flags and the file, runs the startup checks a start runs,
prints each problem on standard error and exits without starting anything: 0 when nothing
stops the start (standard output says `kbf <mode>: ok`), 2 when it is refused, 75 when it
is not ready yet (a restart may help). `kbf check-args` is gone.

## Enrollment is part of the start

`kbf --mode=storage`, `--mode=daemon` and `--mode=storage-and-daemon` (or `kbf` alone with
`mode = "..."` in the file; a flag overrides the file and the start says so) enroll the host
themselves. There is no `node` mode and no `enroll` word.

- First start, with `enroll.token_file` set and no `node.json`: the start makes the key,
  sends the token and a CSR for the roles the mode needs (storage: storage; daemon: daemon;
  storage-and-daemon: both) and writes `node.json`. The tables `[storage]` and `[daemon]`
  are settings only; they never choose roles.
- Later starts renew without the token. A token that lacks a needed role, or a `node.json`
  whose roles do not cover the mode, exits 2 and names both; the host is never enrolled
  again by itself.
- `--check` passes on a configured host that is not enrolled yet and exits 2 for a bad
  config, a missing token file or an uncovered `node.json`. It enrolls nothing.
- Exit 2: a restart cannot fix it. Exit 1: it may pass (the frontend is down or busy, the
  clock is off); the start retries with a backoff of 5 seconds doubling to 5 minutes.

## Installing a mode's unit

The `kbf` package installs the unit of each mode; `kbf-installer setup` enables the one of the host's mode
([kbf-installer-setup.md](kbf-installer-setup.md)); `kbf-deploy linux install-units` is
retired. There is no uninstall for a mode's unit yet. A node moving from the converged units
(`kbf-server.service`) to `kbf-storage.service` stops the old ones first
(`systemctl disable --now kbf-daemon.service kbf-server.service`): both would hold the
same directories.
