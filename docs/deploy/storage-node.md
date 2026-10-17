# Storage nodes: the server-only and converged profiles

How to run `kbf-server` on a storage host: the systemd units, the priorities between
the processes that share the host, and, on a host that also runs builds, how much
memory builds may use. Where servers run and why is in
[deployment-topology.md](../design/deployment-topology.md); a host that runs builds
only is [linux-build-host.md](linux-build-host.md).

## Profiles

| Profile | Runs | Units on this page |
|---|---|---|
| server-only | a node of the object store (MinIO here) and `kbf-server` | [server-only units](#server-only-units) |
| converged | the same, and a `kbf-daemon` with the container driver ([converged topology](../design/deployment-topology.md#converged-topology)) | the server-only units, with the slice and daemon of the [converged units](#converged-units) |
| build-only | a `kbf-daemon` only | [linux-build-host.md](linux-build-host.md) |

## Where the bytes live: storage glossary

Five places hold what a farm keeps, and each has one flag or one directory. The server
takes two store URIs and, sometimes, a directory; the daemon takes one directory.

| Term | What it is | Set by |
|---|---|---|
| **Object store** (the cold store) | The durable home of every blob: an S3 bucket (MinIO, RustFS) or a directory on a disk. Nothing leaves it but by being deleted, and nothing is deleted yet. | `kbf-server --cold-store` |
| **Write tier** (the hot store) | A fast local disk in front of the object store. Uploads land there first and are flushed to the object store in the background; reads are served from it while a blob is still there. Optional: without one, uploads go straight to the object store. | `kbf-server --hot-store` |
| **Metadata log** | The index of what the object store holds (digest to segment object and offset) and the action cache: a single-voter Raft log on a local disk. Lose it and the blobs are unreachable. | in the hot store's directory (`<hot>/meta`), else `kbf-server --meta-dir` |
| **Input cache** | The daemon's per-node cache of input files, from which each lease's inputs are linked. Evicts least recently used past its budget. | `kbf-daemon --data-dir` (`cache/`) |
| **Input trees** | The daemon's shared read-only trees: a large directory of inputs built once and mounted read-only into every action that has it. | `kbf-daemon --data-dir` (`trees/`) |

The object store and the write tier are not exclusive: the write tier is a fast cache
of recent writes in front of the object store, which is cold and durable. A farm that
keeps everything on one disk names it as the object store and has no write tier.

A store URI is `file:///absolute/path` or `s3://bucket/prefix?endpoint=http://host:port`:

- `file://` is a directory on a local disk. An SSD is the hot store's usual home, HDDs
  or an NFS export the cold store's. A directory on a tmpfs is a store in memory:
  there is no `memory:` store, and the server refuses one.
- `s3://` takes the endpoint and the rest in the query: `endpoint=` (required),
  `region=` (default `us-east-1`), `conditional-put=true` (the bucket refuses to
  overwrite a key, as MinIO and RustFS do) and `min-put-rate=` (the slowest rate in
  bytes a second a PUT body may reach the store at, default 1 MiB/s; a PUT is cut after
  60 s plus its body's length at that rate). The prefix is the path, `kbf/` when there
  is none. The key pair is `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` in the
  environment, never in the URI.
- Both stores take both forms, except that the hot store must be a `file://`
  directory today: the write tier is block files on a local disk.

Sizes are not flags. The hot store's budget is 80% of its filesystem's free space after
it keeps 5% of the filesystem free (at least 1 GiB, and never more than half of what is
free), lowered by `--hot-store-max-bytes`; at the budget,
uploads take the direct path to the object store. The daemon's budgets are in
[the data directory](#the-daemons-data-directory).

## Priorities

When the host runs short, the object store goes last, then `kbf-server`, then the
daemon; builds go first. The object store holds every blob and the server holds the
farm's state, so losing either stops every build, while a build that is killed runs
again. Each unit says so in four settings:

| Unit | `OOMScoreAdjust=` | `MemoryMin=` | `CPUWeight=` | `MemorySwapMax=` |
|---|---|---|---|---|
| MinIO | -950 | 32G | 400 | 0 |
| `kbf-server` | -925 | 16G | 300 | 0 |
| `kbf-daemon` | -900 | 256M | 200 | not set |
| build containers | 0 (the driver's `--oom-score-adj=0`) | none | each lease's `cpu.weight`, inside the daemon's | not set |

- **`OOMScoreAdjust=`**: under a host-wide OOM the kernel kills the process with the
  highest score, so the lowest adjustment goes to what must survive longest.
- **`MemoryMin=`**: memory the kernel does not reclaim from the unit while it uses no
  more. The kernel caps it at what the unit's slice protects, so all three units sit
  in `kbf.slice`, whose `MemoryMin=` is the sum of theirs (see
  [linux-build-host.md](linux-build-host.md#memory-on-the-host) for why it is equal,
  not more). The figures are this page's example; size MinIO's and the server's to
  what they use at their peak on your host.
- **`CPUWeight=`**: how CPU is shared between the units of `kbf.slice` when it is
  contended. Nothing is capped: CPU a unit leaves idle goes to the others, builds
  included.
- **`MemorySwapMax=0`**: the object store's and the server's pages stay in RAM. A
  converged host has a large swapfile for builds; a server answering from swap would
  slow every build at once.

## Memory on a converged host

On a converged host, builds get what is left after the object store, the server and
the OS:

```text
--actions-memory-max-gib = RAM - (MinIO's reservation + kbf-server's reservation + 4 to 8 GiB for the OS)
```

The 4 to 8 GiB is the same headroom as on a build host
([linux-build-host.md](linux-build-host.md#headroom---actions-memory-max-gib)): the
kernel, systemd, sshd, the daemon itself and Podman's processes. A reservation is what
the process uses at its peak, and the same figure as its `MemoryMin=` above. The daemon
writes the result to `actions/memory.max` and reports the lower of it and the host's
memory as `mem_gib`, so the scheduler books no more than builds may use.

### Worked example: a 256 GiB host

| Part | GiB |
|---|---|
| RAM | 256 |
| minus MinIO's reservation | 32 |
| minus `kbf-server`'s reservation | 16 |
| minus the OS, systemd, sshd, the daemon and Podman | 8 |
| `--actions-memory-max-gib` | 200 |

## Server-only units

`/etc/systemd/system/kbf.slice`:

```ini
[Unit]
Description=kbf build farm

[Slice]
# The sum of its units' MemoryMin=: MinIO 32G + kbf-server 16G.
MemoryMin=48G
```

MinIO's own unit stays as its installation wrote it; a drop-in puts it in the slice
with its priorities.

`/etc/systemd/system/minio.service.d/kbf.conf`:

```ini
[Service]
Slice=kbf.slice
MemoryMin=32G
MemorySwapMax=0
OOMScoreAdjust=-950
CPUWeight=400
```

`/etc/systemd/system/kbf-server.service`:

```ini
[Unit]
Description=kbf build farm server
Wants=network-online.target
After=network-online.target minio.service

[Service]
# Not the daemon's user: builds must not read the operator API token.
User=kbf-server
Slice=kbf.slice
MemoryMin=16G
MemorySwapMax=0
OOMScoreAdjust=-925
CPUWeight=300
# AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY for the bucket, mode 0600: a secret
# never goes on the command line.
EnvironmentFile=/etc/kbf/s3.env
ExecStart=/usr/local/bin/kbf-server \
    --cold-store=s3://kbf?endpoint=http://127.0.0.1:9000&conditional-put=true \
    --meta-dir=/var/lib/kbf-server/raft \
    --listen=<storage host address>:8980 \
    --reapi-tls-cert=/etc/kbf/reapi.pem \
    --reapi-tls-key=/etc/kbf/reapi.key \
    --worker-listen=<storage host address>:8981 \
    --worker-tls-cert=/etc/kbf/worker.pem \
    --worker-tls-key=/etc/kbf/worker.key \
    --worker-client-ca=/etc/kbf/ca.pem \
    --worker-deny-list=/etc/kbf/deny-list \
    --api-listen=127.0.0.1:8982 \
    --api-token-file=/etc/kbf/api-token
# SIGTERM drains: new Execute calls are refused, open streams run until their action is
# done (Buck2 does not recover a stream that breaks), then the server exits. systemd must
# not SIGKILL it meanwhile, so no stop timeout. A second SIGTERM ends the open streams
# (`systemctl kill --signal=TERM` on the unit), and `kbf cluster drain --frontend` begins the
# same drain with no signal.
TimeoutStopSec=infinity
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

What each group of flags does:

- **The stores.** `--cold-store` is the bucket on the host's own MinIO node; blob bytes
  go there under `kbf/<log id>/`. `--meta-dir` is the metadata log, which keeps the
  index across a restart: a build after the restart gets the cache hits it got before.
  Put it on a partition of its own, not shared with the daemon's data directory, swap or
  the object store's data, so a full build disk cannot stop the log
  ([converged topology](../design/deployment-topology.md#converged-topology)). A fast
  local disk for uploads is `--hot-store=file:///path`; the log then lives in its
  `meta/` directory unless `--meta-dir` says otherwise.
- **The REAPI listener** serves TLS with a certificate from the farm's internal CA, for
  the front's proxy on another host and for clients that trust that CA. A server whose
  proxy runs on the same host may bind `--listen` to loopback and leave out the two TLS
  flags. Without `--reapi-auth-policy` every call is accepted ([reapi-auth.md](../reapi-auth.md)).
  The proxy health-checks the server with `grpc.health.v1` on this listener
  ([api.md](../api.md#grpchealthv1-on-the-reapi-listener)).
- **The worker listener** serves daemons over mutual TLS: their certificates must
  chain to `--worker-client-ca`, and the deny list is read again at every check: a
  daemon's `Hello`, `Heartbeat` and other calls, and a storage node's `Register` and
  `Heartbeat`, so a deny list that cannot be read refuses the storage nodes too
  ([worker-protocol.md](../design/worker-protocol.md)).
- **The operator API** (`/v1`, `/healthz`, `/readyz`) answers reads to whoever reaches
  it, so it is bound to loopback; writes need the token in `--api-token-file`, owned
  by `kbf-server` with mode 0600 or 0400 ([api.md](../api.md)).

Then:

```sh
sudo systemctl daemon-reload
sudo systemctl restart minio.service
sudo systemctl enable --now kbf-server.service
```

## Converged units

A converged host runs the server-only units above with this slice in place of theirs,
and a daemon set up as on a build host
([linux-build-host.md](linux-build-host.md): rootless Podman, the `kbf` user, swap),
in the same slice, with this unit.

`/etc/systemd/system/kbf.slice`:

```ini
[Unit]
Description=kbf build farm

[Slice]
# The sum of its units' MemoryMin=: MinIO 32G + kbf-server 16G + kbf-daemon 256M.
MemoryMin=49408M
```

`/etc/systemd/system/kbf-daemon.service`:

```ini
[Unit]
Description=kbf build farm node daemon
Wants=network-online.target
After=network-online.target kbf-server.service

[Service]
User=kbf
Slice=kbf.slice
Delegate=yes
# Equal to --supervisor-memory-min-mib.
MemoryMin=256M
OOMScoreAdjust=-900
CPUWeight=200
Environment=XDG_RUNTIME_DIR=/run/user/<kbf uid>
ExecStart=/usr/local/bin/kbf-daemon \
    --driver=container \
    --server=https://<storage host name>:8981 \
    --cas=https://<storage host name>:8981 \
    --ca-cert=/etc/kbf/ca.pem \
    --cert=/etc/kbf/node.pem \
    --key=/etc/kbf/node.key \
    --node-id=<node id> \
    --data-dir=/var/lib/kbf \
    --actions-memory-max-gib=200 \
    --supervisor-memory-min-mib=256
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

`--actions-memory-max-gib` is the [worked example](#worked-example-a-256-gib-host)'s
result; on another host, compute it from that host's RAM. The daemon's other settings
and what it logs at start are as on a build host
([linux-build-host.md](linux-build-host.md#the-units)).

## One host without MinIO: a `file://` cold store

For a development farm on one host, `kbf-server` can keep blob bytes in a directory on
the local disk instead of a bucket. The metadata log is a directory too, so a restart
of the server keeps both: a build after the restart gets the cache hits it got before.

```sh
kbf-server \
    --cold-store=file:///var/lib/kbf-server/blobs \
    --meta-dir=/var/lib/kbf-server/raft \
    --listen=127.0.0.1:8980 \
    --worker-listen=127.0.0.1:8981
```

- The directory is created if missing and locked while the server runs: a second
  server on it exits 2. Objects are keyed under the fixed prefix `kbf/<log id>/`, one
  file each in `objects/` (layout in
  [storage.md](../design/storage.md#the-object-store-interface)).
- A write is synced, file and directory, before it is answered, so an answered upload
  survives a crash or a power cut. A crash part-way through a write leaves a file in
  `tmp/`, removed at the next start.
- The start-up pass (a `file://` cold store only; an `s3://` one is not walked, and
  a `file://` hot store is the write tier's block files, which the tier reads at its
  own start): once the server serves (it never waits for the pass to bind or
  to report ready), a background task checks the directory against the index. It
  walks the 256 shards of `objects/` reading each file's header (its key, length and
  modification time, never its bytes) at about 1000 files a second, or as fast as it
  can while the disk is under the free-space floor, and then:
  - keeps each file of an object the index names (staged on the write tier
    included) or of this process's own writer (its writer epoch, or for a node
    registered with `--frontend` its member and boot);
  - deletes each file of an earlier epoch under this log's prefix that the index does
    not name, while the process writes in an epoch: an orphan, such as an upload that
    failed before its commit;
  - keeps and counts as foreign every other file: under another prefix, or of another
    writer (another node's, an earlier boot of this node's own member included, whose
    metadata directory may have been wiped; or an epoch's, once the node writes as a
    member and boot). It leaves a file without a header;
  - then forgets each indexed object, not staged, that has no file: its blobs leave
    the index, with every action-cache entry that needs one of them, so
    `FindMissingBlobs` reports them and the client uploads them again. Forgetting
    needs the Raft log at machine version 3, which a server binary older than this
    one refuses. Before it forgets the first, a log whose cell has a single voter (a
    single-host farm) is raised to it. A cell of more voters is never raised by the
    pass: until the operator raises it, once every replica runs version 3, the pass
    marks each such object missing instead (its blobs are reported missing and its
    action-cache entries miss, but stay in the index) and logs
    `evict/forget needs log V3: N objects held` once.
  - The guard: it forgets nothing unless the walk found the file of at least one
    indexed object. A directory with none is not the one the index was written over
    (a disk that is not mounted: the server creates the directory empty), and the
    pass logs an error instead of emptying the index.

  It logs one line at the end: files read, kept, orphans deleted, foreign, without a
  header, objects, blobs and entries forgotten, objects marked missing for want of
  version 3, objects held back by the guard, the bytes held and how long it took. A
  pass that fails is logged and runs again at the next start; until then nothing is
  evicted.
- Until the pass reaches it, or for a file lost while the server runs, a blob whose
  file was lost, damaged or left empty is still reported present by
  `FindMissingBlobs` until a read of it fails: that read answers `UNAVAILABLE` and
  marks its object unreachable, so the blob is reported missing and the client
  uploads it again, which heals it. The pass reads headers only, so a damaged object
  is found by a read. Before an action-cache hit is served, the server checks that
  the file of every object the hit needs is there and starts with its header; if one
  is not, the hit is a miss (the action runs again) and its blobs are reported
  missing.
- Eviction under disk pressure: the floor is 5% of the file system and at least
  1 GiB (the rule the daemon's cache keeps; not a flag). A write that would leave less
  free is refused (its upload answered `UNAVAILABLE`; the server's PUT retries
  twice) and wakes the evictor, which also looks every 10 seconds once the start-up
  pass is done. While less than twice the floor is free, it takes the least recently
  used objects until their lengths cover the shortfall: an object counts as used
  when `FindMissingBlobs` reports one of its blobs present, a blob of it is read, an
  action-cache hit needs it, or it is written; objects not used since the start
  come first, oldest file first. It never takes an object used in the last 60
  seconds, nor one staged on the write tier. It forgets them first (their blobs
  leave the index, with every action-cache entry that needs one, as in the start-up
  pass, raising the Raft log to machine version 3 only in a single-voter cell), then
  deletes their files; a crash between the two leaves files the next start-up pass
  deletes. In a cell of more voters whose log is below version 3, eviction is off
  until the operator raises the log: the evictor evicts nothing, logs
  `evict/forget needs log V3: eviction off` once, and writes stay refused under the
  floor. The directory
  is the store of record (an `s3://` cold store is never evicted from): nothing is behind it, so an evicted blob is gone, and
  `FindMissingBlobs` reports it missing so the client uploads it again (or the
  action runs again). Under pressure eviction cuts the retention's `min_ttl` short
  (a blob reported present stays held 7 days by default): the 60 seconds after a use
  are all it keeps, so a download that comes later than that after a hit can fail with
  `NOT_FOUND`. Each round that evicts logs a line (free bytes, target, objects,
  bytes, blobs and entries removed, and the bytes it could not cover). Size the disk
  for the cache you want to keep.

With a fast disk in front, add `--hot-store=file:///fast/kbf` (uploads land there and
are flushed to the cold store; the metadata log then lives in `/fast/kbf/meta`, and
`--meta-dir` is left out unless the log should be on another disk).

## The server's log

`kbf-server` writes its log to stderr (journald under systemd), and, besides that, to a
size-capped rotating file, so scheduling decisions and failures are still there after the
journal has rolled over or the host has restarted.

| Flag | Default | Meaning |
|---|---|---|
| `--log-dir` | `<hot store>/log` when `--hot-store` is a directory, else none | The directory of the log file; created (mode 0750) if missing. With neither flag the server logs to stderr alone and says so in one INFO line after it starts. |
| `--log-max-bytes` | 64 MiB | The most bytes one file holds before it is rotated. |
| `--log-files` | 8 | How many files are kept, the current one included. |

The log never takes more than `--log-max-bytes` times `--log-files` plus one line on
disk. A line longer than `--log-max-bytes` is written alone to a fresh file, so that one
file can be longer.

- **Layout.** `kbf-server.log` is the current file; `kbf-server.log.1` is the one before
  it, up to `kbf-server.log.<N - 1>` for `--log-files=N`. Files are created with mode 0640
  and kept by the server's user.
- **Rotation** happens before a write that would cross the cap: `.2` becomes `.3`, `.1`
  becomes `.2`, the current file becomes `.1`, and the file that would become `.N` is
  dropped. The current file is opened for append and never truncated at start, so a
  restart keeps the entries before it. Files past the count (after `--log-files` is
  lowered) are removed at start.
- **Format.** The same line as stderr (time, level, target, fields), without colour. INFO
  and above.
- **Durability.** Each event is written to the file as it happens; the file is not
  `fsync`ed per line, so a crash of the host can lose the last seconds (a crash of the
  server cannot).
- **Errors.** A directory or file that cannot be written (disk full, permission) never
  stops the server and never costs the stderr copy. The failure is reported once on
  stderr; the file then drops events until 64 KiB more of them have
  passed, and tries again.
- **What is in it at INFO and WARN, one line each.** An operation placed on a worker
  (`lease granted`, with operation, lease and node); how a lease ended (`lease finished
  ok`, or `lease finished failed` with the failure kind); a requeue and why (`lease given
  up; requeued`, with the reason, which for a memory kill names the old and the new
  booking, and `farm_runs` of `farm_budget` farm-fault reruns spent); an operation that
  fails for good (`operation finished failed`, with the failure kind and the budget and
  ladder its runs used); an operation no worker can run; and a write-tier flush that
  failed, or found a corrupt or conflicting object. Blob requests are not logged.

To read it from the host:

```sh
kbf-deploy log --log-dir /var/lib/kbf-server/log            # the last 100 lines
kbf-deploy log --hot-store /fast/kbf --lines 500            # <hot store>/log
```

`kbf-deploy log` only reads. It prints the last lines across `kbf-server.log` and the
rotated files, oldest first, and exits 2 with `REFUSED:` if the directory does not exist
or holds no `kbf-server.log`. It cannot find a server's flags, so it is told the directory
(`--log-dir`) or the hot store's directory (`--hot-store`, which names `<dir>/log`).
`pilot server-install` does not yet pass `--log-dir`, so a pilot server logs to journald
only unless its unit is given the flag.

## Planned: the scheduler's state in the log

`kbf-server` keeps its metadata in the Raft log on local disk (`--meta-dir`, or
`<hot store>/meta`; [deployment-topology.md](../design/deployment-topology.md#built-and-planned)).
Until the scheduler's state is in the log too, a restart keeps the daemons' epoch but not
the server's leases, so the old runs go on as orphans whose results are refused. The
unit gains this flag once the scheduler's state is built:

```text planned-flags kbf-server
--runtime-dir=/run/kbf
```

- `--meta-dir` (or `<hot store>/meta`): the server's state applied from a single-voter
  Raft log kept in the directory. Mount that directory on a partition of its own, not
  shared with build scratch, swap or the object store's data, so a full build disk
  cannot stop the log
  ([converged topology](../design/deployment-topology.md#converged-topology)). The
  server holds an exclusive lock on `log.lock` in the directory while it runs, so a
  second server started on the same directory is refused rather than writing the log.
- `--runtime-dir`: where the log's store records that a write to disk failed, so it
  does not reopen the log until the host reboots. `/run` is a tmpfs, cleared at reboot
  as the page cache is.

## The daemon's data directory

`kbf-daemon` keeps everything it writes under one directory, `--data-dir` (default
`/var/lib/kbf`), made if missing:

| Path | Holds | Mode |
|---|---|---|
| `<data-dir>/leases` | lease directories (every real driver) | 0700 |
| `<data-dir>/cache` | the input cache (container driver) | 0700 |
| `<data-dir>/trees` | the shared input trees (container driver, Linux) | 0711 |

The data directory itself is 0755: a rootless container's root is a subordinate id, an
"other" to the daemon's directories, and must search down to a tree. The three share one
filesystem, which the cache and the trees need to link files instead of copying them.

The cache and the trees are on by default under the container driver and sized from the
filesystem, not by flags: with a floor of 5% of the filesystem (at least 1 GiB) kept free
for everything else, the cache may keep half and the trees a quarter of the rest, and
the cache evicts entries no running lease links to keep the floor free.
`--data-max-bytes` only lowers the size they are budgeted from. The native driver never
makes `cache/` or `trees/`: its actions run as the daemon's user, who owns them and
could write them. A cache another daemon owns, or one that cannot be used, is left alone:
the daemon warns (`cache: off:<reason>`) and runs uncached.

At start the daemon logs the data directory, the leases directory, and the cache and
tree budgets it resolved; the server logs its cold store, hot store (with its budget) and
metadata directory.

## Rendering the converged units

`kbf-deploy linux render-units` writes the converged host's four files from templates
compiled into the binary (`crates/kbf-deploy/templates/linux/`) and a settings file
(`crates/kbf-deploy/templates/linux/config.example` documents each key; an unknown or
repeated key is refused). `kbf-deploy linux check-units` then checks them against the
[priorities](#priorities) and the memory split, one `FAIL <rule>: <reason>` line per
broken rule:

```text
kbf-deploy linux render-units --config node.conf --out units --kbf-uid 1500
kbf-deploy linux check-units --config node.conf --dir units --installable
```

`--out` is laid out as `/etc/systemd/system`: `kbf.slice`, `<MINIO_UNIT>.d/kbf.conf`,
`kbf-server.service`, `kbf-daemon.service`. Rendering writes nothing else, so it needs
no `--apply`. A template placeholder (`@KEY@`) with no value is refused rather than
written, and `check-units` fails a file that still holds one; without `--kbf-uid` the
daemon's unit keeps `<kbf uid>`, which `--installable` fails. Exit codes: 0 rendered or
every rule holds, 1 a rule is broken, 2 refused (a bad setting or an unreadable file),
with a `REFUSED:` line on stderr.

## Installing on the host

Three verbs change the host. Each runs as root, reads the host first, prints its plan
as `DRY-RUN:` lines and changes nothing; `--apply` carries the plan out, stopping at
the first step that fails. A re-run after an apply plans nothing. Exit codes: 0 done
(or nothing to do), 1 a step or the check after an apply failed, 2 refused
(`REFUSED:` on stderr, with the fix). They take the same `--config` file as
`render-units`.

```text
sudo kbf-deploy linux swap --config node.conf [--kubelet-allows-swap] [--remove] [--apply]
sudo kbf-deploy linux image-pull --config node.conf [--image REF]... [--apply]
sudo kbf-deploy linux install-units --config node.conf [--bin-dir DIR] [--restart-minio] [--apply]
sudo kbf-deploy linux install-units --config node.conf --uninstall [--apply]
```

- `swap` makes `KBF_SWAP_SIZE` at `KBF_SWAP_FILE` (`fallocate`, `chmod 600`, `mkswap`,
  on ext4 or xfs with 16 GiB to spare), turns it on and adds its `/etc/fstab` line,
  each only if the host lacks it. A file of another size is refused, never resized. It
  refuses while a kubelet (`kubelet`, `k3s`, `k3s-agent`, `rke2-server`, `rke2-agent`)
  is active, since kubelet's default `failSwapOn` would take the node out of its
  cluster at its next restart; `--kubelet-allows-swap` says its config sets
  `failSwapOn: false`. `--remove` turns it off, drops the line and removes the file.
- `image-pull` pulls each of `KBF_IMAGES` (or `--image`) missing from the kbf user's
  rootless Podman store, as that user: its uid, its gid and all its groups (as
  `sudo -u` gives them), `HOME` and `XDG_RUNTIME_DIR=/run/user/<uid>`. Each image in
  the store must have its pinned digest and be `--platform` (`linux/amd64`); after an
  apply each pulled image is checked the same way, and one still missing or not the
  pin exits 1 with a `FAIL` line (the pulls ran, so it is not a refusal). Even a dry
  run needs root, since it reads the store as the kbf user.
- `install-units` checks the host (a cgroup v2 memory controller, `MINIO_UNIT` is a
  systemd unit, both users, the kbf user's subuid and subgid ranges, each a line
  keyed by its name or its uid, and linger, each key file in `KBF_ETC` its service's
  alone, the certificates, the deny list and `s3.env`'s key pair, and the three ports
  free unless kbf-server runs), renders the units with the kbf user's uid and refuses
  them unless `check-units --installable` passes, and refuses a binary whose `--help`
  lacks a flag its unit passes. It then installs, each only if it differs: the
  binaries from `--bin-dir` (keeping the replaced one as `<name>.previous`), the four
  files under `/etc/systemd/system`, and the metadata directory (0700, the server's user) and the data directory
  (0755, the daemon's user). The binaries, the `.previous` copies and the four files are root's
  (uid and gid 0): one with the right bytes and mode that another user owns is
  replaced. After a change it runs `daemon-reload` and enables and restarts
  kbf-server and kbf-daemon; otherwise it only enables or starts one that is not.
  After any start or restart it waits up to 60 s for kbf-server's `/readyz`; a run
  that starts nothing does not wait. MinIO restarts only with `--restart-minio`: its
  new priorities take effect at its next start. `--uninstall` stops and disables both
  services when either is enabled or running, and moves the four files to
  `/var/lib/kbf-linux/removed/<unix seconds>/`; the binaries, the metadata log, the
  data directory, the bucket and the swapfile stay.

## Metadata backup and restore

kbf-server has no snapshot or backup command yet. The metadata directory (`--meta-dir`,
or `<hot store>/meta`) holds the log's record (`log-record`), its lock, the Raft hard
state and the log's segment files; a byte copy of every file taken while the server is
**stopped** is a valid backup. A server started over the copy opens the same log (its
start line prints the same `log=` id) at the same entry, and a copy that lost
`log-record` is refused at start ("holds files but no log-record") rather than opened as
a new, empty log. `kbf-server`'s `tests/file_cold_store.rs`
(`a_copy_of_a_stopped_servers_meta_dir_is_a_backup`) checks both.

A backup is a brief stop, taken by `kbf-deploy` as root:

```text
sudo kbf-deploy linux meta-backup --meta-dir DIR [--hot-dir DIR] --out FILE
                                  [--metrics URL] [--unit NAME] [--ready-secs N] [--apply]
kbf-deploy linux meta-verify --archive FILE --into DIR
```

- `meta-backup` reads `/metrics` (`--metrics`, default
  `http://127.0.0.1:8982/metrics`, the operator API port) and refuses unless
  `kbf_write_tier_unflushed_bytes` and `kbf_write_tier_unflushed_blocks` are 0 and
  `kbf_write_tier_flush_conflicts_total` is 0; an answer it cannot read is a refusal.
  The write tier is not flushed at shutdown, so a block left on the hot store holds
  acknowledged bytes the object store does not have. A server with a write tier needs
  `--hot-dir` (its `--hot-store` directory); one without needs none.
- It then stops the unit (`--unit`, default `kbf-server`), refuses if the hot store
  holds a `*.blk` block (written between the read and the stop; a flushed copy is
  `*.seg`), and writes `--out`: an uncompressed tar of every file of the directory
  (`*.tmp` aside, which the server removes at start) as `meta/<name>`, and
  `MANIFEST.sha256` with one `sha256sum` line per file. The archive is written beside
  `--out`, synced, linked into place (an existing file is never replaced) and its
  directory synced. It starts the unit and waits up to `--ready-secs` (300) for
  `/readyz` on the same port, then prints the archive's size and sha256 and
  `downtime: <s> s`, the seconds from the stop to the ready answer. Any failure after
  the stop starts the unit again before the verb exits 1.
- Without `--apply` it reads `/metrics` and the paths, refuses as an apply would, and
  prints each step as `DRY-RUN:`. Exit codes: 0 done, 1 a failure after the stop (the
  unit restarted), 2 refused before anything changed (`REFUSED:` on stderr).
- Copy the archive off the node and check its sha256 there. `--out` must not be inside
  the metadata directory; a copy on the same disk is no backup of that disk.

`meta-verify` extracts an archive into an empty or new directory (files 0600, the
directory 0700, each synced), checks every file against the manifest and that
`log-record` is there, and prints the file count, bytes, log segment count and the log
id. A file that does not match, one missing or unlisted, or no record exits 1 with a
`FAIL` line and removes what it wrote. It never starts a server. A plain
`tar -xf FILE && sha256sum -c MANIFEST.sha256` checks the same hashes.

**A restored copy shares the live log's id and its writer epochs.** A server started
over it writes object keys under the same `kbf/<log id>/` prefix, and the epochs it
takes follow the backup's, which the live server has already used. So:

- Never start a restored copy with write access to the live server's object store
  while the live server runs: its uploads can land on keys the live server wrote.
- Never start one against a live `file://` store at all: the start-up pass deletes
  every file of an earlier epoch under the prefix that the index does not name, and the
  copy's index lacks everything written since the backup.

To test a restore, start the copy on a spare host, as yourself (no service), with its
listeners on loopback, against either a throwaway MinIO holding a copy of the bucket
under the same prefix root or the live bucket through a read-only key. If the live
server has a write tier, give the copy a fresh, empty `--hot-store`: with nothing staged
at the backup it registers a new tier, and its `write tier attached` line must show
`lost=0`. Check that the start line's `log=` equals the live server's, that `/readyz`
answers 200, and that blobs and action-cache entries written before the backup read
back. Time the restore as the seconds from the start of `meta-verify` to that `/readyz`
200, beside the backup's `downtime:` and the directory's size.

The log keeps every entry it has committed, so the directory, the backup's downtime and
the restore's time all grow with the log until snapshots and compaction land
([metadata-snapshots.md](../design/metadata-snapshots.md)). Measure again at every
backup.

## What the tests check

- `crates/kbf-it/tests/deploy_units.rs` runs `kbf-server`'s flag parser over the
  `ExecStart=` of every server unit in `docs/deploy/`, checks that the planned flag
  above is still refused, that the worked example adds up and matches the converged
  daemon's `--actions-memory-max-gib`, and that the units keep the
  [priority](#priorities) order.
- `kbf-node`'s tests (`src/deploy_doc_tests.rs`) run `kbf-daemon`'s flag parser over
  every daemon unit in `docs/deploy/`.
- `kbf-deploy`'s tests (`src/linux/tests.rs`) plant one defect per rule in the
  templates (a swap limit dropped, an OOM score at the containers' 0, a later
  assignment that breaks the order, a slice minimum that is not the sum, a key on the
  command line, a bare `--image`) and require that rule's `FAIL` line; they
  also refuse each bad setting and a placeholder with no value. The host verbs'
  tests (`src/linux/install_tests.rs`, `images_tests.rs`, `swap_tests.rs`) apply each
  plan into a scratch directory with the tools played by a fake, check what landed,
  and require a re-run to plan nothing; each missing prerequisite is a row that must
  be refused with its own refusal. `src/user_tests.rs` runs `id` as another user
  through `sudo` (CI has passwordless sudo) and requires that user's uid, gid and
  supplementary groups in the child. The rendered units'
  `ExecStart=` lines go through `kbf-server`'s parser (`kbf-it`) and `kbf-daemon`'s
  (`kbf-node`) too.
- `kbf-deploy`'s `src/linux/meta/tests.rs` and `archive_tests.rs` refuse a backup on
  each write-tier reading (unflushed bytes, blocks, conflicts, an unreadable answer)
  before any step, require the unit started again after every failure that follows the
  stop (a late `*.blk`, an archive that fails, a failed stop), a dry run that runs
  nothing, a manifest that lists every file and no `*.tmp`, and a verify that catches a
  flipped byte, a lost, unlisted or stray entry, a missing `log-record` and a non-empty
  target.
