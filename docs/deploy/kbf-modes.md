# The modes of the one `kbf` binary: rendering and checking their units

`kbf-deploy linux render-units --mode MODE` writes `kbf.slice` and `kbf-<mode>.service`
for one mode of `kbf` (`frontend`, `storage`, `daemon`, `storage-and-daemon`,
`all-in-one`; [architecture-v2.md](../design/architecture-v2.md), sections 1 and 11).
`kbf-deploy linux check-units --mode MODE` reads them back, each with its drop-ins, and
prints one `FAIL <rule>: <reason>` line per broken rule, then any `SKIP` or `WARN` line
(neither is a failure), then the summary:

```text
kbf-deploy linux render-units --config node.conf --out units --kbf-uid 1500 --mode daemon
kbf-deploy linux check-units --config node.conf --dir units --installable --mode daemon
```

Exit codes are those of the converged verbs ([storage-node.md](storage-node.md#rendering-the-converged-units)):
0 rendered or every rule holds, 1 a rule is broken, 2 refused.

## What check-units reads

- `kbf-<mode>.service` and `kbf.slice` in `--dir`, and their drop-ins: every `*.conf`
  file in `<name>.d/` beside them, applied after the unit in file-name order, as
  systemd applies them. Each key is read as systemd reads it: a single-valued key's
  last assignment wins, and an empty assignment resets a list (`Environment=`,
  `EnvironmentFile=`, `PassEnvironment=`, the credential keys, `Delegate=`,
  `DisableControllers=`, `AllowedCPUs=`, `AllowedMemoryNodes=`).
- On the host it runs on: `/sys/devices/system/cpu/online` and
  `/sys/devices/system/node/online` (rule `cpuset`, only when a unit sets a list), the
  files the unit's `EnvironmentFile=` lines name (rule `credential`), the storage unit's
  `User=` (`getent passwd`, rule `credential`), and `kbf check-args` (rule `parse`).
  Run it on the node, as root, for all of them.

It does not read drop-ins elsewhere (`/run/systemd/system`, `/usr/lib/systemd/system`,
`service.d/` and prefix drop-ins) or changes made with `systemctl set-property` or
`systemctl edit --runtime`.

## The rules

| rule | what it refuses |
|---|---|
| `render` | an `@KEY@` placeholder left; with `--installable`, a `<...>` one |
| `continuation` | a comment line inside a continued directive (below) |
| `exec` | an `ExecStart=` that is not `<KBF_BIN_DIR>/kbf <mode>` |
| `slice` | a unit not in `kbf.slice` |
| `delegate` | a mode that runs builds without `cpu`, `cpuset`, `memory` and `pids` delegated, or with one of them in `DisableControllers=` (the unit's, or `kbf.slice`'s) |
| `cpuset` | `AllowedCPUs=` or `AllowedMemoryNodes=` on a mode that runs builds, or on `kbf.slice`, narrower than the node's CPUs or NUMA nodes |
| `credential` | the cold store's key reaching a mode without a storage half (`daemon`, `all-in-one`); the `storage` unit, which holds it, run as root or as `KBF_USER` (the user builds run as), and on the node its key file readable by another user; in the `storage-and-daemon` unit, any way of giving the key but the one `EnvironmentFile=<KBF_ETC>/s3.env` (below), and on the node that file not a plain file of root's with mode 0600 |
| `storage` | a `storage` or `storage-and-daemon` unit without `--cold-store`, `--meta-dir`, `--storage-listen` or one of the three certificate flags, or with an `s3://` cold store and no `EnvironmentFile=<KBF_ETC>/s3.env` (for `storage-and-daemon`, exactly that path: no `-`, quotes or wildcard) |
| `state-store` | a `--state-store` that is not `sqlite:///<absolute path>`: a Postgres URI (`postgres://` or `postgresql://`; postgres is not wired yet), any other scheme, an empty or relative SQLite path, or a value that names a variable (below) |
| `ports` | two of a unit's listeners (`--listen`, `--worker-listen`, `--api-listen`, `--storage-listen`) on one port |
| `stop` | a mode that drains on SIGTERM (every mode but `daemon`) without `TimeoutStopSec=infinity` and `KillSignal=SIGTERM` (below) |
| `parse` | flags that `kbf <mode>` refuses |

**`delegate`.** systemd reads `Delegate=` this way (systemd.resource-control(5), and
`config_parse_delegate` in systemd's `src/core/load-fragment.c`): a boolean delegates
every controller (`yes`) or none (`no`); a list adds its controllers to the ones
already delegated; an empty value turns delegation on with no controller. So
`Delegate=yes` followed by `Delegate=memory` still delegates every controller, cpuset
among them, and the rule passes it. A slice cannot delegate: systemd enables on
`kbf.slice` every controller a unit under it delegates, unless the slice's
`DisableControllers=` lists it. The slice template sets only `MemoryMin=`.

**`cpuset`.** The daemon places leases on every CPU and NUMA node of the node
(`--lease-placement=numa-node` pins each lease to one NUMA node's CPUs and memory). A
list narrower than the node keeps the unit's cgroup, and every lease in it, off the
rest. A list is read as systemd reads it (`config_parse_cpu_set` in
`src/shared/cpu-set-util.c`): words split at spaces, tabs, line breaks and commas by
`extract_first_word` with `EXTRACT_UNQUOTE` only, so quotes are taken off
(`AllowedCPUs="0-1"` is the CPUs 0 and 1) and a `\` keeps the next character, with no
C escapes; an unclosed quote drops the whole assignment; each word a number or range
as `parse_range` and `safe_atou` read it (`0x`, `0o`, `0b` and a leading `0` pick the
base), a malformed one ignored. A `%` specifier is not expanded: a `WARN cpuset:` line
says the value was read as written. When the node's own sets cannot be read
(check-units run elsewhere), a set list is not compared and a `WARN cpuset:` line says
so. `StartupAllowedCPUs=` and `StartupAllowedMemoryNodes=` (boot and shutdown only)
are not read.

**`continuation`.** systemd.syntax(7): "When a comment line or lines follow a line
ending with a backslash, the comment block is ignored, so the continued line is
concatenated with whatever follows the comment block." That holds from systemd 241.
systemd 240 skips such a comment only when the `#` starts the line, and 239 and
older join the comment into the value (`config_parse` in `src/shared/conf-parser.c`
before the change that fixed systemd issue 10598), so its words become arguments of an
`ExecStart=`. The templates put every comment above its directive, and the rule refuses
one inside.

**`credential`.** It refuses, in `[Service]`:

- `EnvironmentFile=` naming the key file: its path, with `-`, quotes, `.`, `..` and
  repeated `/` taken off, is `<KBF_ETC>/s3.env` or ends in `/s3.env`, or it is a
  wildcard that matches `<KBF_ETC>/s3.env`. systemd expands every path with glibc's
  `glob(3)` (`safe_glob` in `src/basic/glob-util.c`, no flags, in PID 1's C locale),
  and the rule reads it the same way: `*`, `?`, `[...]` with ranges, `!` or `^`,
  `[:alpha:]` and the other C-locale classes, `[=c=]` and `[.c.]`, a leading `.`
  matched only by a `.`, and `\` escapes, taken off a path with no wildcard too
  (`/etc/kbf/\s3.env` is the key file). Braces and `~` are plain characters (systemd
  passes neither `GLOB_BRACE` nor `GLOB_TILDE`), and a `..` after a wildcard is read
  by its words, as if the wildcard matched some directory;
- `Environment=` setting any `AWS_*` variable, quoted or not, alone on the line or not:
  the key pair, `AWS_SESSION_TOKEN`, and `AWS_PROFILE`, `AWS_SHARED_CREDENTIALS_FILE`
  and `AWS_CONFIG_FILE` (which point an SDK at a key anywhere). A build node talks to no
  AWS-style store, so no `AWS_*` variable has a use there;
- `PassEnvironment=` naming an `AWS_*` variable;
- any `LoadCredential=`, `LoadCredentialEncrypted=`, `SetCredential=`,
  `SetCredentialEncrypted=` or `ImportCredential=` (opaque to the rule; a build node's
  units need none);
- on the node, an `EnvironmentFile=` whose file sets an `AWS_*` variable (a copy or a
  hard link of the key under another name), or that is a symbolic link (not followed).

What it cannot catch: offline, a file under another name that holds the key or links
to it, and a wildcard elsewhere that matches a copy named `s3.env`; on the node, the
files a wildcard `EnvironmentFile=` matches and a file it cannot read (each a
`SKIP credential:` line); `%` specifiers (not expanded); `UnsetEnvironment=` (not read,
so a variable it unsets still fails the rule); and the key reaching the node any other
way: a file the daemon reads by a flag, the service manager's own environment without
`PassEnvironment=`, or a key file other users can read.

## What each mode's unit carries

- **`storage`**: `--cold-store=s3://<KBF_S3_BUCKET>?endpoint=<MINIO_S3_ENDPOINT>&conditional-put=true`,
  `--hot-store=file://<KBF_HOT_DIR>`, `--meta-dir=<KBF_META_DIR>`,
  `--storage-listen=<KBF_LISTEN_ADDRESS>:<KBF_STORAGE_PORT>` (8983 by default: kbf's
  own default, 8982, is `KBF_API_PORT`'s), and `--storage-tls-cert`, `--storage-tls-key`
  and `--storage-client-ca` (`storage.pem`, `storage.key`, `ca.pem` under `KBF_ETC`).
  It runs as `KBF_SERVER_USER` with `EnvironmentFile=<KBF_ETC>/s3.env`, the bucket's
  key pair: on the node rule `credential` wants that file root's or the server user's,
  mode 0600. No state store: a storage node reaches it only through a frontend.
- **`daemon`**: the flags `kbf-daemon`'s unit has (`--server`, `--cas`, its
  certificates, `--data-dir`, the memory flags, the images, and `--reserve-cpus` and
  `--lease-placement` when `KBF_RESERVE_CPUS` and `KBF_LEASE_PLACEMENT` are set). `--server`
  and `--cas` dial the frontend by `KBF_FRONTEND_NAME`, not the node's own
  `KBF_HOST_NAME` (below); no key.
- **`all-in-one`**: below; no key (its cold store is a directory); its `--api-token-file` is
  `KBF_API_TOKEN_FILE` too.
- **`storage-and-daemon`**: the storage unit's flags (`--cold-store`, with
  `KBF_S3_PREFIX` as its path, `--meta-dir`, `--hot-store`, `--storage-listen`
  `<KBF_LISTEN_ADDRESS>:<KBF_STORAGE_PORT>` and the three certificate flags; no `--cas`:
  the daemon half reads the node's storage in-process), its registration
  (`--frontend=https://<KBF_FRONTEND_NAME>:<KBF_WORKER_PORT>` and
  `--advertise-addr=<KBF_HOST_NAME>:<KBF_STORAGE_PORT>`), then the daemon unit's flags,
  with `--server` the same frontend URL. It runs as `KBF_USER` with
  `EnvironmentFile=<KBF_ETC>/s3.env` (below). `install-units --mode` makes `--meta-dir`
  and `--hot-store` the user's, mode 0700, and `--hot-store` should be on `--data-dir`'s
  filesystem (`KBF_HOT_DIR`): the one cache.
- **`frontend`**: `--listen=<KBF_LISTEN_ADDRESS>:<KBF_REAPI_PORT>` with
  `--reapi-tls-cert=<KBF_REAPI_CERT>` and `--reapi-tls-key=<KBF_REAPI_KEY>` (a
  certificate pair the farm's CA did not issue, such as a `tailscale cert` pair, so the
  file names are settings; default `reapi.pem` and `reapi.key` under `KBF_ETC`);
  `--worker-listen=<KBF_LISTEN_ADDRESS>:<KBF_WORKER_PORT>` with `worker.pem`,
  `worker.key` and `--worker-client-ca=ca.pem`; `--api-listen=127.0.0.1:<KBF_API_PORT>`
  `--api-token-file=<KBF_API_TOKEN_FILE>` (default `api-token` under `KBF_ETC`; the
  path only, never the token: without it every write to the operator API, `POST
  /v1/frontend/drain` included, answers 401). The operator puts that file there;
  `install-units` never makes it, and refuses a missing one, one the server user does
  not own, or one with a group or other bit (mode 0600 or 0400);
  `--storage=<KBF_STORAGE_URL>` (required, `https://`) with the frontend's worker
  certificate as its client identity (`--storage-client-cert`, `--storage-client-key`)
  and `--storage-ca=ca.pem`; and `--state-store=sqlite://<KBF_STATE_DIR>/state.db`
  (below). It runs as `KBF_SERVER_USER`, who must own the keys.

`render-units --mode storage` refuses settings without `MINIO_S3_ENDPOINT`: the
converged units default to the host's own MinIO, but a storage node's bucket need not be
on the node. Every mode's settings must give the four ports (`KBF_REAPI_PORT`,
`KBF_WORKER_PORT`, `KBF_API_PORT`, `KBF_STORAGE_PORT`) four values, or nothing renders.

`KBF_HOST_NAME` is the node's own name (the name in its certificates, which it advertises)
and `KBF_FRONTEND_NAME` the one name it dials for the frontend (the name in the frontend's
worker certificate); left out, the frontend is the node itself. A farm node sets both: with
only the first, its daemon dials its own address, where nothing listens, and never
registers. The `storage` and `daemon` units still carry a `TODO(registration):` comment
where their `--frontend` and `--advertise-addr` go; only `storage-and-daemon` registers
today.

`KBF_S3_PREFIX` (default empty: kbf's own `kbf/`) is the cold store's key prefix in the
bucket, `a/b` or `a/b/`; a fresh prefix is a fresh cold store beside the old.
`KBF_RESERVE_CPUS` (a whole number) and `KBF_LEASE_PLACEMENT` (`none` or `numa-node`) are
left to kbf's defaults when empty.

### The state store: SQLite only

The only state store kbf opens today is a SQLite file; Postgres is a later backend. Rule
`state-store` fails a `--state-store` that is not `sqlite:///<absolute path>`: a
`postgres://` or `postgresql://` URI (postgres is not wired yet), any other scheme, a
SQLite path that is empty, relative or not normal, and a `--state-store=${VAR}`
(systemd expands the variable into the command line, so the rule cannot read the store
it names, and what it holds is in argv, readable by every user). A unit with no
`--state-store` passes. No message repeats the value.

### Updating a frontend without failing a build: `kbf-frontend@.service`

The frontend's unit is a template with two instances, `kbf-frontend@a` and
`kbf-frontend@b`, that run the same command line; only one serves, and only `@a` is
enabled at boot. To update, put the new `kbf` in place by rename (`install-units --mode
frontend --bin-dir DIR --apply` does, and with a serving instance restarts nothing), then
run, on the frontend's host:

```
kbf frontend handover --wait
```

It starts the idle instance with `systemctl start`; the unit is `Type=notify`, so the start
returns when the new frontend has taken the listeners, the state store and the daemons over
and serves. The old instance then answers the builds it holds and exits by itself (`--wait`
looks at it until it has). Buck2's open streams are not broken, new calls go to the new
frontend, daemons reconnect and keep their runs. A new instance that fails before it is
ready leaves the old one serving, and the command exits 1 and says so. With both instances
active (the old one still draining) it refuses. What each party sees, and every failure:
`docs/design/frontend-handover.md`. Rule `handover` of `check-units --mode frontend`
refuses a unit that is not `Type=notify`, whose `--handover-socket` is outside a
`RuntimeDirectory=` kept with `RuntimeDirectoryPreserve=yes`, or that has `Conflicts=`
(starting an instance would stop the one draining).

### Stopping a frontend: TimeoutStopSec=infinity

On SIGTERM kbf's frontend (and `all-in-one`'s) drains: it refuses new Execute calls,
which Buck2 retries elsewhere, and keeps every open Execute and WaitExecution stream
until its action is done; `--shutdown-timeout-secs` is 0, no deadline, by default.
Buck2 never recovers a stream that breaks mid-action, so a stop that ends the drain
fails every build in flight. systemd kills a unit with SIGKILL when `TimeoutStopSec=`
runs out, 90 s (`DefaultTimeoutStopSec=`) when the unit gives none. So both units have
`KillSignal=SIGTERM` and `TimeoutStopSec=infinity` (so do `storage` and
`storage-and-daemon`: a storage node finishes its large Writes at stop, and systemd's
90 s default would cut them), and rule `stop` refuses either
missing or changed: it reads `TimeoutSec=` too (it sets the stop timeout as well; the
last of the two wins) and the drop-ins. It asks for the word `infinity`: `0` means no
timeout to systemd as well, but reads as "at once" to a person. A daemon is not held:
it does not drain on SIGTERM (the operator API's `POST /v1/nodes/{node}:drain` empties a
node before it is stopped).

### storage-and-daemon and the cold store's key

One unit and one process (the daemon half reads and writes the node's storage
in-process, one cache on the SSD; architecture-v2.md, sections 1 and 10). The unit holds
the cold store's key pair through one `EnvironmentFile=<KBF_ETC>/s3.env`, a plain file of
root's with mode 0600 (systemd reads it as root). Rule `credential` allows exactly that
line, spelled exactly that way, and refuses every other way a build node is refused
(`Environment=` or `PassEnvironment=` with an `AWS_*` variable, a systemd credential, a
wildcard or `-` form of the file, another file that sets `AWS_*`); on the node it refuses
a key file another user owns or others can read, and `check-units` lists a missing file
as `SKIP credential:`.

The design keeps the key from builds by uid (sections 9 and 15): the key file the unit's
user's alone and each action a subordinate id of that user (the container driver's
`--userns=nomap`), with a test that an action cannot read the key. That test does not
exist yet, and the unit runs as the build user, which can read the key from the process's
environment. So this unit is for a trusted network only, as all-in-one is.

## all-in-one is for a trusted box

`kbf all-in-one` runs builds in the process that holds the farm's state and its cold
store, as one user. The container driver runs each action as a subordinate id of that
user (`--userns=nomap`), but no test yet proves an action cannot read what the user
holds (architecture-v2.md, section 9). Until one does, the unit says so in a comment
and every `check-units --mode all-in-one` prints:

```text
WARN trusted: kbf all-in-one runs builds beside the farm's state and its cold store, as one user; the container driver runs each action as a subordinate id of that user (--userns=nomap), but no test yet proves an action cannot read what the user holds: use it on a trusted single box or for development only
```

Its cold store is `file://<KBF_DATA_DIR>/cold`, the single box's shape (section 11), so
the unit holds no cold-store key and rule `credential` applies to it. Its scheduler
state is the SQLite file `<KBF_DATA_DIR>/state.db`, the default of `kbf all-in-one` (a
`--state-store=sqlite:///...` names another; any other value is refused), so the unit
gives no state store flag.

## A node's certificate

kbf-deploy issues no certificate. The farm's one issuer makes every certificate of the
new modes' mutual TLS from the one CA, each with exactly one role, by this table:

| row | role (`urn:kbf:role:<role>`) | extended key usage | names |
|---|---|---|---|
| `frontend-worker` | `frontend` | serverAuth, clientAuth (it also calls storage) | the frontend's names |
| `storage` | `storage` | serverAuth, clientAuth (replication calls) | the node's names |
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
- `name` (`daemon`): its DNS and IP names are exactly `--node-id`.
- `chain`: it chains to `--ca-cert` now, for each use of the row: as a server under its
  first name, as a client.

It prints a `FAIL <check>:` line per failed check and `CERT BROKEN`, or one
`CERT OK:` line. Exit codes: 0 the certificate is the row's, 1 a check failed, 2 refused
(a file that cannot be read, no certificate, no usable CA, `--node-id` for a row that
is not `daemon`).

Copy the certificate and its key to the node's `KBF_ETC` (the storage unit reads
`storage.pem` and `storage.key`), the key owned by the unit's user; `install-units
--mode` refuses a key another user owns or others can read. The frontend unit reads
`worker.pem` and `worker.key` (the `frontend-worker` row, which is also its client identity
toward storage), `ca.pem`, and the REAPI pair `KBF_REAPI_CERT` and `KBF_REAPI_KEY`.

## Installing a mode's unit

`kbf-deploy linux install-units --config node.conf --mode MODE [--bin-dir DIR]
[--apply]` installs one mode's unit on the node, as root:

1. It checks the host, read only: the unit's
   `User=` exists and is not root; for a mode that runs builds, the kbf user has subuid
   and subgid ranges and lingers; `kbf` is an executable in `--bin-dir` (or
   `KBF_BIN_DIR`); the rendered unit passes `check-units --mode MODE --installable` on
   this host, its flags parsed by that `kbf`; every `EnvironmentFile=` is there; and
   each file a key or token flag names (`--key`, `--*-key`, `--*token-file`) is the
   unit's user's with no group or other bit, and each certificate, CA or deny list flag
   names a file.
2. It plans, each step only when the host differs: the new `kbf` (with `--bin-dir`; the
   old one kept as `kbf.previous`), `kbf.slice` and `kbf-<mode>.service` (the frontend's is
   the template `kbf-frontend@.service`; 0644, root's),
   the directories the unit's flags name (the unit's user's: `--data-dir` 0755,
   `--meta-dir`, a `file://` hot or cold store, and the directory of a `--state-store`
   SQLite file 0700), then `daemon-reload`, enable
   and restart; with nothing changed, enable and start only if the unit is not. The frontend
   is enabled as the template and `kbf-frontend@a` is started only if neither instance is
   active: a serving instance is never restarted, the plan says to run `kbf frontend
   handover` ("Updating a frontend without failing a build"). A unit
   with `--api-listen=127.0.0.1:<port>` (`frontend`, `all-in-one`) is waited for on `/readyz`.
3. Without `--apply` every step is printed as a `DRY-RUN:` line and nothing is written,
   reloaded or started.

There is no `--uninstall` for a mode's unit yet. A node moving from the converged units
(`kbf-server.service`) to `kbf-storage.service` stops the old ones first
(`install-units --uninstall`): both would hold the same directories.

## Upgrade kbf before kbf-deploy

Rule `parse` runs `<kbf> check-args <mode> <flags...>`. A `kbf` older than
`check-args` refuses the word, so `check-units --mode` fails any mode's unit with:

```text
FAIL parse: kbf daemon refuses the flags of kbf-daemon.service (/usr/local/bin/kbf check-args: exit 2): error: unrecognized subcommand 'check-args'
```

(`all-in-one` the same, with its name.) Install the new `kbf` before running the new
`kbf-deploy` on a node, or check offline: on a machine with no `kbf` at the unit's
path and no `--kbf-bin`, the rule is skipped with a `SKIP parse:` line.
