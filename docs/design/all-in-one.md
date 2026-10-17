# One server, one daemon, one process: libraries, three binaries, a shared cache

Status: **accepted by the project owner; phase 0 is in progress, phases 1 and 2 follow
it.** The owner's rulings (2026-10-15), each recorded where it applies:

- The secrets table of section 7.1 is accepted: the performance saving and the operational
  overhead it removes justify it (section 13, question 2).
- **There is no measurement gate.** The owner has ruled that performance and operational
  overhead justify the work; section 11 is kept as the list of numbers to record while
  building, not as a bar a phase must clear.
- In all-in-one the walk **reads and hashes each output file once**; for a blob
  `FindMissing` reports missing, the same buffer is handed to the server as `Bytes` (no
  spool copy), and the server's flush writes it to the tier later. There is no second
  hash on the upload path (sections 2 and 4).
- The server **trusts the daemon's digest at Commit and verifies in the background flush**
  (merged in #656). All-in-one uses that path unchanged.
- **In all-in-one there is no server coordinating any daemon other than its own.** The owner's
  ruling: `kbf-all-in-one` is a single-node farm, full stop. The server half serves only the
  in-process daemon half. It opens **no worker listener** and accepts no remote daemon; the
  command line has no node certificate or key, no deny list and no peer admission. The
  client-facing REAPI listener and the API listener stay as `kbf-server` has them. Both the
  data plane and the control plane between the halves are in-process calls (sections 1
  and 4). Scaling past one node means the split binaries, never all-in-one plus extra daemons.
- `--reserve-cpus` exists on the daemon and all-in-one only. The server takes every CPU
  tokio gives it (section 6).
- **A durable server log is a precondition (P1) and is being built separately**: a local
  filesystem blob store with the raft metadata log, no S3, for single-host farms. This
  document leaves the seam for it (section 7.4) and does not design it.

Phase 0 changes no behaviour and is in progress. No wire message changes in any phase: a
remote daemon sends what it sends today, and a server accepts what it accepts today.

**What this does and does not buy, in one paragraph.** All-in-one changes none of: Mojo
compile time (about 97% of a cold build's time), the hard-link and ownership passes that
bound a typical action, or the node-cache hit rate. It removes a loopback hop and a copy
for bytes that never leave the machine, gives one binary to deploy and one CPU flag on a
single node, and makes the farm's CPU and memory claim explicit. It costs the user
boundary around the farm's secrets, widens a crash from one half to both, and, until the
server's state is durable (section 10, P1), turns a daemon crash into the loss of the whole
index. If the typical action is dominated by links and ownership passes, the saving is
mainly operational, and this document says so here, not later; the owner has ruled that
this, with the upload saving, is enough.

This replaces an earlier Unix-socket design for the same host (section 12). Its reviews'
findings (transport not the measured cost; typical action not helped; the user boundary;
OOM and crash blast radius) are answered in sections 2, 7 and 11.

## 1. Decision

1. **Server and daemon become libraries.** `kbf-server` and the daemon each expose
   "start me with these arguments and a shutdown future" and a handle to what they
   serve. Their `main` functions shrink to argument parsing, logging and signals.
2. **Three thin binaries**, one per deployment shape:

   | Binary | Runs | For |
   |---|---|---|
   | `kbf-server` | the server only | storage nodes of the target farm |
   | `kbf-daemon` | the daemon only; data plane over gRPC | build hosts, Mac nodes, and any host whose server is elsewhere |
   | `kbf-all-in-one` | both, in one process; data plane in-process | the one-node farm and small converged Linux hosts. Linux only, container driver only (section 5). Not Macs, and not a typical dev box: most are Macs, which run `kbf-daemon` |

3. **One data-plane interface, two implementations**: remote (today's transfer engine
   over mutual TLS) and in-process (direct calls into the server's cache and segment
   code). The daemon is generic over the interface and cannot tell which it has.
4. **One control-plane interface, two implementations**, mirroring the data plane: remote
   (today's `Session` stream, leases and heartbeats over mutual TLS to the worker listener
   of `kbf-server`) and in-process (the daemon half's lease stream, welcome, heartbeats and
   results as calls and channels into the server half's scheduler, section 4). One
   conformance suite runs against both (section 8). The scheduler and its commit path are
   the same code behind both, so the sim scenarios still apply.
5. **All-in-one is a single-node farm.** The server half serves only the in-process
   daemon half: no worker listener, no remote daemon, and no node certificate, node key,
   deny list or peer admission on its command line. The REAPI and API listeners are as in
   `kbf-server`. A farm that needs a second node runs the split binaries
   (`kbf-server` + `kbf-daemon`); it never adds daemons to an all-in-one.
6. **One CPU flag**, `--reserve-cpus`, on `kbf-daemon` and `kbf-all-in-one` only, Linux
   only. `kbf-server` has no such flag and uses as many CPUs as tokio gives it. No pinning.
   What the reserve does not cover goes to builds (section 6). Unset, every binary behaves
   exactly as it does today.
7. **One conformance suite** for the data plane and one for the control plane, each run
   against both implementations (section 8).
8. **The split binaries stay first class.** They are the only way past one node, the
   target farm's shape, and the only shape that serves a remote daemon or a Mac. A split
   canary farm exercises the remote paths in production, and CI renders and checks the
   split and converged shapes as well as all-in-one (section 9).
9. **No measurement gate.** The owner has ruled that performance and operational overhead
   justify the work. The in-flight do-less PRs and the worker-cache link work (section 11)
   still change what the numbers show, so section 11's measurements are recorded as the
   work lands, to report what it bought. Phase 0 changes no behaviour.

## 2. What is known, and what is not

One action outputting a toolchain tree (15k files, 307 MB) spent 18.3 s in its outputs
phase: walk 7.8 s, spool 4.3 s, upload 6.0 s. That is **one measurement on `main`, before
the PRs of section 11**; section 11 asks for at least five runs with variance. The 6.0 s
is about ten 32 MiB uploads sent one after another (Begin, ranges, Commit, which hashes,
syncs the tier and commits metadata): as consistent with latency in series as with
bandwidth, so **not shown to be transport**.

The typical action differs: a 160 to 480 MB input root (a 15k-file toolchain tree that 63%
of actions share), hard links, per-directory fetches and ownership passes, 96% of blobs
already in the node cache (`worker-cache.md`), tiny outputs. None of that crosses a
network; an in-process plane does not change it. The directory cache and link plans do.

**All-in-one does nothing for compile time.** A build is compile CPU and memory inside a
container; the process boundary between the server and the daemon is not on that path.
The only effects on compiles are second-order: who gets CPU and memory under contention
(sections 6 and 7), and the blast radius of a crash. Whether all-in-one gives compiles more
CPU or fewer is a number to measure (M6, section 11), not an assumption: the farm's own
processes take CPU from compiles in both shapes, and a reserve books it explicitly.

**The in-process plane hashes once on the upload path.** In all-in-one the walk reads each
output file once and hashes it; that digest is what `FindMissing` is asked and what Commit
carries. The digest has to exist before `FindMissing` can run, so the tier write is a
later step, not the same pass: for a blob reported missing, the walk's buffer is passed to
the server as `Bytes` (no spool copy) and the server's flush writes it to the tier. A file
larger than the walk's per-file memory bound is not held; it is read a second time from
the file when it is reported missing (the bound is set in the phase that builds this and
is not fixed here). The server trusts the digest at Commit and verifies it in the
background flush, which hashes each record before the PUT and drops one that fails (#656,
on `main`). So a 307 MB output is hashed once on the upload path and once more by the
flush's verification off the critical path; the remote plane is unchanged. What in-process saves (TLS, protobuf encode, one kernel copy, the second read
and hash) is not offset by a second hash.

So the case for all-in-one is mainly operational: **one binary to deploy and supervise on
a single node, one place that decides how many CPUs builds get, one cache budget instead
of two, and no loopback hop and no second copy of bytes that never leave the machine.**

## 3. The crate split

Today (read from the tree):

- `kbf-server` is already a library plus a 200-line `main.rs`. The `main` still holds
  the assembly: `start` (build the store and metadata log from flags), `run` (open the
  `Cache`, attach the write tier, start the flusher, bind the listeners), the start line
  and the signal handlers.
- The daemon is two crates. `kbf-daemon` is the library (session, leases, cache,
  report). The binary is in `kbf-node/src/main.rs`, 1193 lines, because it names the
  drivers and the drivers depend on `kbf-daemon`. It builds the concrete
  `Arc<Transfers>` and hands it to the container and native runtimes, which are
  generic over `kbf_transfer::Cas`.
- `kbf-node` names `kbf-front` and `kbf-server` today only as **dev-dependencies** (its
  start-up and binary tests). `kbf-transfer` names `kbf-front` as a dev-dependency too;
  `kbf-front` does not depend on `kbf-transfer`.

Target:

| Crate | Becomes | Holds |
|---|---|---|
| `kbf-server` (lib) | `kbf_server::start(args, shutdown) -> ServerHandle` | store and metadata-log selection, `Cache::open`, tier attach, flusher, listeners. `ServerHandle` has the bound addresses, the `serving` future, and the `Arc<Cache>` plus `SegmentsService` that `kbf-all-in-one` wraps (section 4). The start line and `Routes` move with it. |
| `kbf-node` (lib) | `kbf_node::start(args, data_plane, shutdown) -> NodeHandle` | everything in today's `main.rs` below argument parsing: driver and runtime setup, cgroups, cache open, the `Daemon`. Generic over `C: Cas + DataPlane`. The `--cas` flag, TLS flags and `Transfers` construction move into `RemoteDataPlane::connect(args)`, which lives in `kbf-node`. Also `kbf_node::dispatch_helpers()`, the three re-exec dispatches (below). |
| `kbf-transfer` | gains the `DataPlane: Cas` trait | no new dependency |
| `kbf-server` (bin) | `kbf-server` | `Args::parse`, tracing, signals, `kbf_server::start`, print the start line. |
| `kbf-node` (bin) | `kbf-daemon` | `dispatch_helpers()`, `Cli::parse`, tracing, signals, `RemoteDataPlane::connect`, `kbf_node::start`. |
| `kbf-all-in-one` (new crate, bin) | `kbf-all-in-one` | `dispatch_helpers()`, one `Cli` flattening both argument sets, tracing, signals; `kbf_server::start`, then `kbf_node::start` with a `LocalDataPlane` built from the server handle. **Holds `LocalDataPlane`** and the conformance suite's instantiation. The only crate that depends on both libraries. Its body is `#[cfg(target_os = "linux")]`; on any other target `main` exits with an error naming the supported shape, so the macOS CI builds it. |

**Where `LocalDataPlane` lives, and why there is no cycle.** It needs `kbf-front`'s
`Cache` and `SegmentsService` and `kbf-transfer`'s trait. Only `kbf-all-in-one` may see
both, so it lives there. `kbf-transfer` keeps `kbf-front` as a dev-dependency only, and
`kbf-front` never names `kbf-transfer`. The shared conformance functions are generic over
`P: DataPlane` in `kbf-all-in-one`'s tests (a leaf, so no crate depends on it): they take a
`RemoteDataPlane` built against an in-process server over a real connection, and a
`LocalDataPlane` over the same server. `kbf-transfer`'s existing tests are untouched.

**Rules the split keeps, each with a check:**

- **Neither library depends on the other, in normal dependencies.** `kbf-server` must not
  name drivers; `kbf-node` must not name `kbf-front` or `kbf-server` outside
  `[dev-dependencies]`. `layering.rs` gains the rule (normal-dependency edges only, so the
  existing dev-dependencies stay) and a planted-edge mutant.
- **The helper re-execs.** The per-run keeper, the native `slots` helper and the userns
  `chown_dirs` helper are this same binary, re-executed
  (`kbf_daemon::keeper::dispatch`, `kbf_driver_native::slots::dispatch`,
  `kbf_driver_container::chown_dirs::dispatch` in `kbf-node/src/main.rs`). Each runs before
  argument parsing and before any runtime or listener exists. `dispatch_helpers()` is that
  code, called first by both `kbf-daemon` and `kbf-all-in-one` mains, so a re-executed
  `kbf-all-in-one` never reaches clap, never opens the server's directories and never
  starts a runtime.
- **Packaging.** `kbf-server` and `kbf-daemon` keep their names. `kbf-all-in-one` is a
  third artifact; section 9 names how it is installed and updated.
- **A refactor, not a rewrite.** `kbf-node`'s `main.rs` also drops under the 1000-line
  budget. Merging `kbf-daemon` and `kbf-node` is the crate-layout question (#634).

## 4. The data-plane interface and its two implementations

The interface is what drivers already use: `kbf_transfer::Cas` (`get`, `put`,
`put_chunks`, `read_to`, `missing`, `put_spool`, `lease`) plus the segment-range read the
cache fill uses (`read_located`, today an inherent method of `Transfers`). Phase 0 puts
that method on a small `DataPlane: Cas` trait so nothing in the daemon names `Transfers`.

| | `RemoteDataPlane` | `LocalDataPlane` |
|---|---|---|
| Bytes move | `Transfers` over one mutual-TLS HTTP/2 connection to the worker listener of `kbf-server` | function calls into the server's `Cache` and `SegmentsService` in the same process |
| Authentication | the daemon's certificate, admitted per call by `identity.rs` (the deny list is a check of this listener) | none: the daemon half is constructed with the server half's handle and there is no listener to authenticate on. Nothing is admitted because nothing is remote (see "Peer admission" below) |
| Integrity | the server trusts the daemon's digest at Commit and verifies it in the background flush, which hashes each record before the tier PUT and drops one that fails (#656) | **the same path.** The walk reads and hashes each output file once and hands the buffer over as `Bytes`; the flush writes it to the tier and verifies as for a remote daemon. There is no extra in-process hash and no flag that skips the flush's check |
| Upload ownership | `Connections` keyed by the peer `SocketAddr` | keyed by `Owner::Local(PlaneId)`, one fresh id per `LocalDataPlane` (idle timeout, drop-on-close and admission release hang on it as for TCP) |
| Copies | file, spool read, protobuf encode, TLS encrypt, kernel, decrypt, decode, into the server's buffer | the walk's buffer is passed as `Bytes` (a second file read only above the walk's memory bound); no spool copy, no encode, no TLS, no kernel hop |

**The control plane in all-in-one is in-process too.** No worker listener is opened, so no
node certificate, node key or deny list exists on the all-in-one command line, and the
unit holds none. Phase 0 puts the daemon's side of the control plane behind a trait,
`ControlPlane` (names are the implementer's): open the session and receive the welcome,
the lease stream, send heartbeats, report results, and the commit and release calls that
go with a lease. Two implementations, as for the data plane:

| | `RemoteControlPlane` | `LocalControlPlane` |
|---|---|---|
| Transport | the `Session` gRPC stream over mutual TLS to the worker listener of `kbf-server` | calls into the server half's scheduler, and bounded `tokio` channels for the streams (lease offers server to daemon; heartbeats and results daemon to server), in the same process |
| Identity | the node certificate, admitted by `identity.rs` | none on a wire; the daemon half is built with the server half's handle, and the server half knows exactly one session owner |
| Session end | the stream closes: a dropped connection, a restart | the channels close: the daemon half is dropped or the process ends. The scheduler sees the same "session closed" event as for a lost stream, so lease handover and requeue are the existing code paths |
| Messages | protobuf, serialized | the same Rust message structs, moved, not serialized |

The scheduler, the commit path and the lease state machine are single code behind both.
`LocalControlPlane` is an adapter at the scheduler's door and holds no scheduling logic;
a second code path would fork the commit path from the split deployment's, which is the
risk this design must avoid. **One conformance suite** (section 8) runs against both:
welcome and lease assignment, a heartbeat that extends a lease, a missed heartbeat that
expires one, results and commit, a session closed mid-lease and a new session after it, a
second session for the same node (refused or superseding, as the remote plane decides, and
the same on both), and cancellation. Backpressure is part of the suite: a daemon that
does not read offers does not grow the server's memory without bound.

**Peer admission.** The deny list, cordon and node-id admission apply only to the worker
listener of `kbf-server`: they decide which remote nodes may speak to it. All-in-one has
no such listener and exactly one node, its own, so there is nothing to admit. A separate
in-process deny-list check was considered and rejected for that reason: it would guard no
boundary, add a state the operator could edit with no effect on a real peer, and keep
alive the certificate and node-id machinery this shape removes. Operator cordon and drain
of the single node go through the API listener's existing calls and are unchanged.

**`Owner`.** Phase 0's one change inside `kbf-front`:
`Connections::of(Option<SocketAddr>)` becomes `of(Owner)`, with
`enum Owner { Peer(SocketAddr), Local(PlaneId) }` and **no empty variant**. Today a request
with no peer address already maps to "no connection", so a handler that reads a missing
`remote_addr` gets `None`; the new type makes that case unrepresentable in the handlers:
the remote service constructs `Owner::Peer` from the request or answers `UNAVAILABLE`
before any admission or ownership lookup, rather than continuing with no owner. Two local
planes get distinct `PlaneId`s from a process counter and never alias (section 8 has the
mutant).

Why this is direct calls, not an in-memory channel: every `Segments` handler in
`SegmentsService` is a unary `fn(&self, Request<T>) -> Result<Response<U>, Status>` over
prost structs, so the in-process adapter builds the request struct and calls the handler.
The struct is moved, not serialized. A tonic `Channel` over a `duplex` pipe would keep
HTTP/2 framing, protobuf and flow control, and save only TLS: that is the transport the
project owner rejected.

What `LocalDataPlane` does not do: skip the server's checks, hold the cache's state
outside the server, or let a daemon-supplied path or descriptor reach the server. It
passes `Bytes` and the daemon's digest; the server stores that same buffer and the flush
verifies it before the PUT. A digest mismatch is dropped by the flush and never reaches
the tier, as for a remote daemon.

## 5. The shared cache

Two stores exist today, and they are different shapes, which limits what "one cache"
can mean:

- **The server's tier** (`kbf-tier`): block files `<epoch>-<seq>.blk` holding records
  **packed at byte offsets** inside an object that will be flushed to the object store as
  a segment. Unflushed blocks are the only copy and are never evicted; flushed `.seg`
  copies are evicted least-recently-read first when a reservation needs room.
- **The daemon's node cache** (`worker-cache.md` section 6): **one file per blob** under
  `blobs/`, 0444/0555, filled by the daemon alone (download into `tmp/`, hash, rename),
  hard-linked into leases, evicted by least recent use past `--cache-max-bytes`, with
  entries a running lease links pinned.

A packed record cannot be hard-linked: a link names a whole file. So "inputs are linked
from the tier" cannot mean linking out of a block. What can be shared:

1. **Fill from the tier without the network (phase 2a).** On a node-cache miss the
   daemon's fill reads the blob's record from the server's block or `.seg` copy through
   the in-process plane (`read_located` as a local call), hashes it while writing into
   `tmp/`, and renames it into `blobs/`. The rule "only the daemon writes `blobs/`" holds
   unchanged. Saves the loopback `ReadRange`, TLS and one copy on a miss; nothing on a
   hit. Given 96% hits, expect this to matter only for cold toolchain bumps.
2. **One budget (phase 2b).** `--cache-max-bytes` and `--write-tier-bytes` stay two
   limits, but all-in-one adds `--storage-max-bytes` over the filesystem they share, and
   evicts in this order when a reservation does not fit: cache entries no lease pins,
   then flushed `.seg` copies. Unflushed tier blocks are never evicted (the existing
   rule), and a blob present in both the cache and a `.seg` copy is held twice on disk; a
   `.seg` copy is dropped first because the cache entry is the one a lease can link. This
   is an accounting change, not a new store. **What the split binaries do with a cache
   all-in-one populated:** the files are valid under the same lazy verify after restart
   (`worker-cache.md` 6.8), so the split daemon reads them unchanged; the split pair does
   not know `--storage-max-bytes`, so the two budgets can be jointly over-committed until
   each side's own eviction runs. Section 9's rollback step therefore checks free space
   against the sum of both limits and refuses if it does not fit.

**Outputs into the cache is not a phase of this design.** An earlier draft had a phase 2b
that moved an uploaded output into `blobs/`. `worker-cache.md` 6.7 already does that in
every binary: when the action is dead, the output is already uploaded (its digest known),
and the file in `upper` is regular with `st_nlink == 1`, the daemon strips its xattrs and
ACL and renames it into `tmp/` and on into `blobs/`. The rename source is the container's
`upper` file after the `chown 0:0` handback, not the spool copy, so it cannot race the
upload, which reads the spool; nothing in all-in-one changes that. All-in-one forbids the
copy mode and native driver that 6.7 declines, so there is nothing for a second mechanism
to add. It is dropped; 6.7 serves split and all-in-one alike.

**Not done:** one content-addressed file store replacing both; the tier packs records so
one segment is one PUT and one commit. A per-blob view over packed records would be its
own document, if a measurement asks for it.

**Ownership with one user.** Actions still cannot write the cache: in a container's user
namespace the file's owner is unmapped (`worker-cache.md` 6.1), and the tier directory is
never mounted. What goes is the user boundary between the tier and an escaped action;
section 7 says what that costs.

**Eviction.** A hard link keeps an evicted file alive for running leases, so eviction
needs only today's pins. The shared budget in 2b is a counter both sides consult, not a
lock held across both.

**Native driver:** its actions run as the daemon's user and could write the cache, so it
never uses one (`cache.rs` already warns `native-same-uid`). `kbf-all-in-one --driver
native` is refused at start. Macs run `kbf-daemon`.

## 6. `--reserve-cpus`

**Meaning.** The number of CPUs the farm's own work may use on this host. Everything
else belongs to builds. It is a count, not a set: nothing is pinned (`AllowedCPUs=` and
`cpuset` stay unset, as `linux-build-host.md` says), because idle CPU should go to whoever
can use it.

**Unset is today, bit for bit.** With `--reserve-cpus` unset the reported `cpus` is what
`Capacity` reports now (the nearest cpuset, else every host CPU), no cgroup weight is
written, the async runtime and the blocking and hashing pools are the ones in use today
(the unbounded-by-CPU blocking pool does the hashing), and the start line prints
`reserve=unset`. This is Phase 0's contract, and a test pins it (section 8). Nothing in
this section applies until a deploy sets the flag.

**Scope.** The flag exists on `kbf-daemon` and `kbf-all-in-one` only, and on Linux only.
`kbf-server` has no `--reserve-cpus`: it uses as much CPU as tokio gives it, and its pools
are the ones it has today. On macOS `kbf-daemon` refuses the flag at start:
a Mac daemon has no server beside it and no cgroup to weight; honouring it would lower the
reported `cpus`, which `native-whole-machine` leases use to size the machine, and bound
hashing to a few threads, which would slow large output uploads that today use the
unbounded blocking pool.

**What it does when set, in three places:**

1. **What the scheduler sees.** The node report's `cpus` becomes `host CPUs - R` (through
   `Capacity::within`, which already lowers `cpus` and `mem_gib`). **This is a hard
   booking cap, not the soft weight below.** Today the converged units report every host
   CPU (no reserve is subtracted anywhere in `linux-build-host.md` or `storage-node.md`),
   and the farm's processes compete with builds through weights alone. Setting `R` on the
   one-node farm therefore **lowers bookable CPUs by `R`**: it trades builds that could
   have used idle farm CPU for a guarantee the farm is not starved. That is the point of a
   reserve and it can cost compile throughput; section 11's M6 sets `R` from the 95th
   percentile of the farm's measured CPU, not from the sum of today's thread counts, and
   decides whether to set it at all.
2. **How the process's CPU-heavy work is bounded.** Hashing, spool copy, CRC and record
   verification run on a bounded pool of `hash_threads` threads, not on the shared
   blocking pool (which also does `fdatasync` and file I/O). The async runtime's worker
   count is set from the share. The bound is a throttle: a large upload hashes on at most
   `hash_threads` cores. Where that is too tight, `--hash-threads` overrides it, and the
   expected cost is computed in M3/M4.
3. **What the kernel does under contention. Both siblings get a weight.** The daemon's
   delegated subtree has two children, `supervisor/` (the farm process) and `actions/`
   (parent of every lease cgroup). Today the daemon writes `cpu.weight` only on each
   lease, below `actions/`; nothing is written on `actions/` or `supervisor/`, so both sit
   at the kernel default of 100. A `supervisor/` weight alone would be compared with
   `actions/`'s default 100, not with the leases: at `R = 6` the farm would take about 86%
   of a contended host. The design therefore writes **both**:
   `supervisor/cpu.weight = cpu_weight(R x 1000)` and
   `actions/cpu.weight = cpu_weight((host CPUs - R) x 1000)`, so under full contention the
   farm gets `R / host` and builds the rest. Uncontended, nothing is capped. (This fix
   applies to the split daemon too, and ships in Phase 0 with the flag.)

**Pool sizing from the reserve `R`, when set (initial rules; section 11 may change the
constants):**

| Pool | `kbf-server` | `kbf-daemon` | `kbf-all-in-one` |
|---|---|---|---|
| Share of `R` | no flag; all CPUs, tokio's defaults | all of `R` | server `ceil(R/2)`, daemon `R - ceil(R/2)`, each at least 1; `R >= 2` required |
| Async worker threads | tokio default | `max(2, ceil(share/2))` | per side, same rule, one runtime per side |
| Hash/verify pool | today's | `share` threads | **one pool per side**, each of its `share`; not one shared pool, so neither half can use the other's bound |
| Blocking I/O pool | today's | fixed bound, not CPU-derived | same |
| `cpus` reported | not reported (no daemon) | `host - R` | `host - R` |

There is **no default `R`**: it is set by the deploy from M6. Initial values to try are
the converged units' current thread counts, but they are not defaults.

**Converged split (two units, one host).** The server unit has no reserve flag, so the
daemon cannot derive its share as `reserve - server's R`. On a converged host the daemon's
`--reserve-cpus` is the figure the daemon subtracts from the CPUs it reports and books:
the daemon reports `cpus = host - R`. **What `R` bounds, plainly:** it caps what builds
may book. It does not bound the server. Item 3's weights are written only on the daemon's
`supervisor/` and `actions/` children, and on a converged host the server is a separate
systemd unit, a sibling of the daemon's unit; the only kernel bound on it is that unit's
`CPUWeight`, which `kbf-deploy/src/linux/check.rs` requires to be above the daemon's. The
server uses every CPU under tokio's defaults and can take CPU from builds beyond `R`. In
all-in-one (one process) `R` does cover both halves, because the server's pools run inside
the same process's reserve. `--colocated-server-cpus` is dropped.
`check-units` asserts in the converged shape that the daemon's `--reserve-cpus` is set and
below the host's CPUs. How far the server exceeds `R` under load is what M6 records.
Open question for the project owner (section 13): on a converged two-process host, is that
acceptable, or does the server need a bound of its own? Not decided here; no flag is added.

**Overrides.** `--hash-threads`, `--async-threads` and `--blocking-threads` win over the
derived values; the start line prints what was derived and overridden (`reserve=4 async=2
hash=4`). A deploy sets `--reserve-cpus` and nothing else; a value over `R` is accepted
with a WARN. A reserve at or over the host's CPUs is refused at start, and `R < 2` in
all-in-one too. A cgroup that cannot be written (no delegation) is logged: the pools are
still bounded and only the kernel share is lost, which is today's behaviour.

## 7. Isolation and blast radius

The previous review of this idea found four facts to answer. They are real, and this
section does not claim to restore what merging removes.

### 7.1 The user boundary and the secrets

**Which uid.** The unit runs as `kbf`, the user that has the Podman subuid/subgid ranges
and runs the daemon today; rootless containers cannot be launched from `kbf-server`'s uid
without the same setup. Everything `kbf-server` owns today (the tier at 0700, the raft and
metadata directories, the API token, the S3 key, the server's TLS key) is therefore owned
by `kbf`, and **readable by anything that runs as `kbf`**: the daemon half, and an action
that escapes its container.

**What `ProtectProc`, `LoadCredential`, Yama and `PrivateTmp` do not do.** They do not
narrow the same-uid case, and this document makes no claim that they do. An escaped action
runs as the same uid. It can open `/proc/<pid>/environ` and `/proc/<pid>/mem` of the farm
process unless the process is non-dumpable (the daemon sets `PR_SET_DUMPABLE=0`
already, `fleet-updates-security.md` S4; the merged process keeps it, which closes
those two paths for a non-root same-uid reader and nothing more), and a `LoadCredential=`
file under `/run/credentials/<unit>` is readable by the unit's uid by design. Yama's
`ptrace_scope` covers `PTRACE_ATTACH`, not reading `environ`. These settings stay in the
unit for other reasons (hygiene against other users' processes), and `check-units` asserts
them for that; they are not part of the isolation argument.

**What an escape yields.** Every secret and every state directory the merged process
holds, and what each is worth:

| Held by the all-in-one process | An escape as `kbf` yields | Where it is bounded |
|---|---|---|
| S3 key for the bucket | read and write of every blob and segment in the bucket | scope the key to the bucket and prefix; the server still verifies digests on read-through, but an attacker with the key can also delete or rewrite objects directly |
| operator API token | the write API: cordon, drain, hold, cancel | audited; an operator can rotate |
| the server's TLS private key for its client-facing listeners (all-in-one has no daemon to trust it and no node key) | impersonate the server to its clients | the clients of one node; no fleet trusts this key |
| the rollout credential and the client identity toward `kbf-mdm-gate`, if configured | force-update Macs (one per pool at a time) and relay signed requests | S1.3, S1.4: "one escape loses the Mac fleet" is the rating the security design gives a gate beside actions |
| the raft and metadata directories, and the tier | rewrite the farm's state and any unflushed block (the only copy) | none; this is the integrity of the farm |

The security design (`fleet-updates-security.md` S1.3, S1.4) already treats "a large
network-facing process" beside actions as what must not happen on a fleet that includes
Macs. That fixes the rule:

- **`check-units` refuses an all-in-one unit that configures the MDM gate client identity
  or a rollout credential.** A host that orders Mac rollouts runs `kbf-server` split from
  any actions. The one-node Linux farm, which has none, may run all-in-one.
- **Not for hostile work.** All-in-one is for first-party, authenticated clients on a
  single node. Untrusted or multi-tenant work, any Mac fleet, and any farm whose bucket
  also holds other tenants' data use the split shapes (table below).

**What still holds.** Rootless containers and cgroups stay the isolation of the action
(user namespace, `pids.max`, `memory.max`, `cpu.weight` per lease; the tier is never
mounted; the cache's write path is not reachable from inside). And **the server still
verifies every digest**: a compromised daemon half writes only bytes whose hashes the
server's flush computes itself before the PUT, so it cannot poison the CAS under another digest. That property
survives merging; the secrets above do not stay behind a boundary.

### 7.2 Memory and the out-of-memory killer

Actions run in the `actions` cgroup with `memory.max` and per-lease caps, so an over-limit
build is killed inside its cgroup, not by a global kill that may pick the farm process. The
farm process sits in the `supervisor/` leaf with `memory.min`, and the unit has
`OOMScoreAdjust=` making it the last candidate after the object store.

**Sizing, with a formula.** The unit's `MemoryMin=` and `supervisor/memory.min` are

```text
index_peak + write_bytes_in_flight + hash_pool x 32 MiB + tier_cache + daemon_part
```

where `index_peak` is the in-memory index under the memory log (or the raft state's
working set under the durable log of section 10), `write_bytes_in_flight` is
`--write-bytes-in-flight` (the server refuses to start with a segment cap above it; phase
1 of the write path holds `Bytes` for up to four 32 MiB segments per upload, so the bound
is what admission allows, not four), `hash_pool x 32 MiB` is the verify buffers,
`tier_cache` is the tier's read cache, and `daemon_part` is the daemon's `256 MiB`.
M7 supplies `index_peak` and the high-water mark; no figure is proposed here.

**The ordering test changes.** `check-units` today asserts MinIO < `kbf-server` <
`kbf-daemon` in `OOMScoreAdjust=` (-950 < -925 < -900). One process collapses the last two.
The test is rewritten: MinIO < all-in-one < 0 (build containers), the unit's `MemoryMin=`
equals the sum above, `kbf.slice`'s `MemoryMin=` equals MinIO's plus the unit's,
`--actions-memory-max-gib` is `RAM - (MinIO + the unit + the OS headroom)`, and
`--actions-memory-max-gib` is at least the largest single lease cap the pool may book
(the compile working set from M7). The planted mutants: the ordering inverted; the slice
sum missing the unit; a memory sum that leaves builds more than the host has.

### 7.3 A crash takes both halves down, and what is lost

One process means a panic outside a task, a failed assertion, an `abort`, or fd or thread
exhaustion in one half ends both, and a restart is the only recovery. What the design does
about each:

- **Panics.** Tokio contains a panic in a spawned task (the workspace sets no `panic =
  "abort"`). The process ends on a panic outside a task, an `abort` or the OOM killer.
  Phase 1 adds a test that a panic in a driver task leaves the process serving. The
  **hash/verify pool** is process-wide: a job that panics there must not lose the thread
  or hang the callers. Each job runs under `catch_unwind` and the caller gets `INTERNAL`
  (the same answer `check_all` gives today for a check that panicked); a test panics a
  hash job and then hashes and uploads normally.
- **Exhaustion in the farm process itself, which is the real risk.** Not the containers:
  those are in the lease cgroups. The unit sets `LimitNOFILE=` from the formula
  `4 x max connections + 2 x leases the pool may book + fixed headroom` (printed in the
  start line), and the daemon half writes `pids.max` on **`supervisor/`** (the farm
  process's own leaf, not the unit's, because the unit's `pids.max` would also count the
  delegated `actions/` subtree) as `async + hash + blocking + helpers + margin` threads.
  The blocking pool is a fixed bound, not CPU-derived. `check-units` asserts both
  settings are present and nonzero; a test starts the process with a tiny `pids.max` and
  asserts the pools refuse work with an error rather than spawning past it.
- **What containers are bounded by.** Each lease cgroup's `pids.max`, `memory.max` and
  `cpu.weight`. `nofile` and `nproc` are **rlimits, not cgroup controls**, and `nproc` is
  counted per uid (a container's mapped subuids, not the farm's uid); this design does not
  rely on them to bound actions.
- **The helpers.** A keeper re-exec of `kbf-all-in-one` must not start a server: see the
  `dispatch_helpers()` rule in section 3, and the test in section 8.

### 7.4 What a restart loses

**The seam for the durable log.** The durable server log (a local filesystem blob store plus
the raft metadata log, no S3, for single-host farms) is built in a separate change. The
only contract this design relies on is that the server's tier and metadata log are behind
the handle `LocalDataPlane` already calls (`Cache`, `SegmentsService`), so swapping the
store does not touch the data plane or the daemon.

**The server's log is not durable in the current deploy**, so this document does not say
that a restart recovers. Under `--meta-log=memory` (the default, and the only mode the
farm unit passes today) a restart forgets the CAS index, the action cache, the node
registry and every lease, and with `--store=s3` writes under a fresh key prefix on each
start (`deployment-topology.md`). `--meta-log=raft` is accepted but is "planned and not
accepted yet" in `storage-node.md`'s unit, and over it a restart keeps the epoch but not
the server's leases, so the old runs go on as orphans whose results are refused; the
scheduler's state is not in the log.

Today, a daemon crash costs one daemon. In all-in-one it restarts the process that holds
the index. So **a durable meta-log, with the scheduler's state in it, is a precondition of
phase 1** (section 10, P1), not a mitigation. Until then all-in-one is a development
shape, not a production move of the live farm. With P1 met, a whole-process restart costs
the lease handover grace (`daemon.md`) and re-reading the raft directory, as a server
restart does.

In a farm with several servers a crash of the daemon half restarts a server that may be the
leader and costs an election. That is why the target farm runs split.

### 7.5 When to use which

| Use | When |
|---|---|
| `kbf-all-in-one` | one Linux node, first-party authenticated clients, no gate client identity or rollout credential in the unit, a durable meta-log (P1), and a restart of everything acceptable |
| `kbf-server` + `kbf-daemon` on one host (converged) | several hosts, any HA servers, anything where a build host crash must not cost a leader or the index, or where the user boundary around the credentials is wanted |
| `kbf-server` and `kbf-daemon` on separate hosts | the target farm; untrusted or multi-tenant work; Mac nodes |

The single binary is a deployment choice that costs isolation. A farm outgrows it by
changing the unit, not the code, **subject to the ownership step of section 9**.

## 8. Tests: one suite, both data planes; sim coverage

**A conformance suite**, written against `DataPlane` once, in `kbf-all-in-one`'s tests
(section 3), run for each implementation. `RemoteDataPlane` runs against an in-process
server over a real connection; `LocalDataPlane` against the same server's handle.

Cases both pass: put and get of an empty blob, 1 byte, 1 MiB and one over the segment
cap; `missing` before and after a put; a segment upload and a `read_located` of it; a
blob already present; an upload whose record bytes do not match their digest (refused,
nothing indexed, and no retry that repeats it); two concurrent uploads; an upload
abandoned on drop releases `--write-bytes-in-flight`; a deny-list edit mid-session
is a remote-plane case only: no local case, since no admission exists there; the same sequence yields the same metadata (the final index is
compared between the two planes).

Each test names the mutant that turns it red:

| Test | Mutant |
|---|---|
| a record whose bytes do not match the daemon's digest is dropped by the background flush on both planes and never PUT | the local adapter bypasses the flush's check; the output file is hashed a second time on the upload path |
| the deny-list edit mid-session refuses the next call on `RemoteDataPlane`; `kbf-all-in-one` has no deny-list flag and no worker-listener flag (the parser rejects them) | an in-process admission check added, or a worker-listener flag accepted |
| dropped local upload releases the write budget; a stalled local upload does not starve remote ones | release missing for `Owner::Local` |
| two `LocalDataPlane`s: dropping one's upload releases only its own, and a Commit on one cannot adopt the other's upload | both planes given the same `PlaneId` |
| a remote request with no peer address is refused before admission | `Owner` has a "none" case that skips the lookup |
| both planes produce the same index | one plane skips the footer or the staging commit |
| `kbf-all-in-one` starts and an action's outputs appear in the CAS with zero TCP `PutRange` bytes | the daemon is wired to `RemoteDataPlane` by mistake |
| `kbf-all-in-one` binds only the REAPI and API listeners (no worker port is open), and a remote node's connection to where the worker port would be is refused | a worker listener left on in all-in-one |
| the control-plane conformance cases (section 8, "A conformance suite for the control plane") pass for `RemoteControlPlane` and `LocalControlPlane` | the local adapter completes a lease without the scheduler's commit path, or never reports a closed session |
| `kbf-server` has no driver crate, and `kbf-node` neither `kbf-front` nor `kbf-server`, in its **normal** dependency tree | `layering.rs` rule removed |
| `kbf-all-in-one __keeper ...`, `... slots ...` and the `chown_dirs` mode run the helper and exit with its code; no listener, no server directory opened, no clap error | `dispatch_helpers()` called after argument parsing, or missing from the all-in-one `main` |
| a panic in a driver task leaves the process serving | driver run on the farm's main task |
| a panicking hash job leaves the pool at full size and the next upload hashes | no `catch_unwind` in the pool |
| with a tiny `supervisor/pids.max` the pools refuse work instead of spawning; `LimitNOFILE` and `pids.max` are in the unit | thread cap not written; nofile unset |
| `--reserve-cpus` over the host, or `R = 1` in all-in-one, is refused; refused on macOS | validation removed |
| **unset**: reported `cpus` equals today's for the same fake host, no `cpu.weight` is written, and pools equal today's | reserve applied when the flag is absent |
| set: derived pool sizes match section 6 for each binary at `R` in {2, 4, 6, 16}; reported `cpus` is host minus `R` | all-in-one sizes each side by the whole `R`; reserve ignored by the report |
| set, on a fake cgroup root: `supervisor/cpu.weight = cpu_weight(R x 1000)`, `actions/cpu.weight = cpu_weight((host - R) x 1000)`, **and the ratio of the two equals `R : host - R`** | weight written on `supervisor/` only (the ratio is then `R x 10 : 1`, not the reserved share) |
| `kbf-server` rejects `--reserve-cpus`; the converged rendering sets the daemon's `--reserve-cpus` as the host-wide figure and the daemon's `cpus` is `host - R` | a server flag is added, or a converged daemon reports every host CPU |
| native driver with all-in-one is refused | the check removed |
| cache fill from the tier writes through `tmp/` and the rename, and a corrupted tier record is refused at the daemon's hash | fill copies straight into `blobs/`, or trusts the tier's checksum |
| shared budget evicts unpinned cache entries, then `.seg` copies, never an unflushed block, never a pinned entry | eviction order swapped |
| the memory sum, the OOM ordering of section 7.2 and the slice sum | each planted as in 7.2 |
| `check-units` refuses an all-in-one unit with a gate client identity or rollout credential | the refusal removed |

**A conformance suite for the control plane**, written against `ControlPlane` once, run
for each implementation (cases in section 4). `RemoteControlPlane` runs against an
in-process server over a real connection; `LocalControlPlane` against the same
server's handle. The final scheduler state is compared between the two, as the index is
for the data plane.

**Real two-process path.** `kbf-it` runs the split pair and `kbf-all-in-one` through the
same action (multi-file output, a cache hit on a second run) and compares CAS contents.
`deploy_units.rs` renders the all-in-one, converged and split units and runs their flags
through the parser, in CI on every change.

**macOS CI.** `kbf-all-in-one` builds there as the stub of section 3 (its body is
`cfg(target_os = "linux")`); a test asserts the stub exits nonzero with the message.

**Simulation.** `kbf-sim-cell` has a `FakeDaemon` and a fake server on a modelled
network. Phase 1 adds a data-plane mode to `FakeDaemon` that bypasses the network model
for bytes, and a control-plane mode that replaces the session with the in-process channels
(the network model stays for the split mode), with injectable faults (crash of the whole
process between staging and answering, restart with a stale session). The existing
properties 1 to 9 hold unchanged in both modes. Added: **A1** after any whole-process
crash, every upload either completed whole or left nothing indexed; **A2** the final index
of a run is equal between the network mode and the in-process mode for the same seed;
**A3** (needs P1) after a whole-process crash and restart, the CAS index and the action
cache equal their state at the last commit and no lease is accepted that the restart did
not issue. The sim server hashes by construction, so a sim property does not test that
`kbf-front` hashes; the digest-mismatch test above, with its mutant against the real code,
does.

## 9. Deploy

- **Shapes.** `kbf-deploy` gains `all-in-one`, rendering one unit. The existing
  `converged` rendering (both units) stays. `check-units` asserts for all-in-one: the
  hygiene settings of section 7.1 (`ProtectProc`, `LoadCredential`, `PrivateTmp`), no gate
  client identity or rollout credential, the memory sum and ordering of 7.2, `LimitNOFILE`
  and the thread cap of 7.3, delegation, no native driver, `--reserve-cpus` below the
  host's CPUs, and that every state directory is owned by the unit's user (below).
- **The target farm** runs `kbf-server` on storage nodes and `kbf-daemon` on build hosts
  and Macs, the daemons with their own `--reserve-cpus` (the servers have none); the flag is the only change.
- **Changing shape is an explicit step, with ownership.** Today `kbf-server` owns the tier
  (0700), the raft and metadata directories and its keys; `kbf` owns the cache, the
  scratch directory and the daemon's keys. A unit swap alone leaves each binary unable to
  read the other's files. `install-units --shape` is the step: it refuses while either
  unit is active, moves ownership of every directory and key listed above to the target
  unit's user (to `kbf` for all-in-one, back to the pair for the split), checks free space
  against the sum of `--cache-max-bytes` and `--write-tier-bytes` (section 5), writes the
  units, and runs `check-units`, which verifies ownership and refuses a mismatch in either
  direction. **Reverting is this step run the other way, not restoring two unit files.**
  A host never changes shape as a side effect of an update.
- **Installing and updating the third artifact.** `kbf-updater` installs artifacts by
  name (`set.artifacts`, one file per name, per pool and platform), and CI builds a set
  from every green main. `kbf-all-in-one` is a new artifact name, built and signed by the
  same CI path (the automatic key covers unprivileged binaries only, `fleet-updates-
  security.md` S2.2) and attested like the other two; an all-in-one host is a pool whose
  set names `kbf-all-in-one` and not `kbf-daemon`. `set.rs` and the updater's artifact
  list change to allow it, and a host that has `kbf-all-in-one` installed refuses a set
  that names `kbf-daemon` for it (a shape change is the explicit step above).
- **A rollout restarts the server that orders it.** Every daemon update on an all-in-one
  host restarts the server. Two consequences are stated, not left to chance. (1) The
  rollout record is durable before any node is touched (`fleet-updates.md` section 4.1),
  but under the memory log a restart forgets it: so the rollout of an all-in-one host
  needs P1 (a durable log holding the rollout record), and until then such a host is
  updated by running `kbf-updater` on it by an operator, outside the rollout driver. (2)
  `apply.rs` does not restart what it replaced today ("not done yet"); the restart and the
  drain for all-in-one is the same unit step the daemon needs, and the all-in-one phase
  waits on it.
- **A canary that stays split.** All-in-one accepts no remote daemon, so the canary is a
  separate split farm (`kbf-server` + `kbf-daemon`, a Linux daemon among its nodes) that
  runs the same `kbf-it` action daily. The remote data plane, the remote control plane,
  admission, mutual TLS and `Owner::Peer` are exercised in production there, and not only
  by CI and the Macs. It does not connect to the all-in-one farm.

## 10. Migration phases and preconditions

**Phase 0: library seams and the shared interface. No behaviour change. In progress.** Reverts by
`git revert`.

- `kbf_server::start`/`ServerHandle` and `kbf_node::start`/`NodeHandle`; both mains thin;
  `dispatch_helpers()`; existing tests pass unchanged.
- `DataPlane: Cas` with the segment-range read; `RemoteDataPlane` wraps `Transfers`; the
  daemon is generic over it. `Owner` in `Connections` and the handlers (section 4).
- `--reserve-cpus` in `kbf-daemon` (not `kbf-server`) with the derived pools and the two
  cgroup weights; **unset, bit-identical to today** (a test
  proves it), so reported `cpus` does not change until a deploy sets it.
- `ControlPlane` trait, with `RemoteControlPlane` wrapping today's session code; the
  control-plane conformance suite, for `RemoteControlPlane` only. No wire change.
- The conformance suite, for `RemoteDataPlane` only. The `kbf-all-in-one` crate exists as
  the stub only, if at all.
- Rebases onto the in-flight PRs of section 11: the contract the local plane needs is "a
  put feeds the server's incremental per-record check", not a function name.

**Preconditions of phase 1 (all must hold):**

- **P1: a durable meta-log with the scheduler's state in it**, and the rollout record in
  it, running on the target host. **Being built separately**: a local filesystem blob
  store with the raft metadata log and no S3, for single-host farms. This design takes
  the store and log as given, behind the `Cache`/`SegmentsService` handle the local plane
  already calls, so a different tier does not change `LocalDataPlane`. Without it a whole-process crash loses the index
  (section 7.4).
- **P2: the worker-cache directory cache and link plans** (`worker-cache.md`) have landed
  and been measured, so M5 shows what is left after the real lever.
- **P3: #653 to #656** have landed (section 11).
- **P4: the security table of section 7.1: accepted by the project owner (2026-10-15).**
  Remaining: the host has no gate client identity or rollout credential.
- **P5: the restart and drain step of `apply.rs`**, and the `set.rs` change of section 9.
- **P6: removed.** The owner ruled the measurement gate out (section 11).

**Phase 1: the all-in-one binary, a single-node farm with in-process data and control
planes.** Three slices, each independently reviewable:

- **(a) the data plane**: `LocalDataPlane` for puts (outputs, stdout, stderr) and gets.
  Merged as #724.
- **(b)**: #730.
- **(c) the in-process control plane and no worker listener**: `LocalControlPlane` behind
  `ControlPlane`, the control-plane conformance suite for both, and `kbf-all-in-one` opening
  no worker listener and taking no node certificate, key or deny-list flag.

With the slices: `kbf-deploy` shape and the ownership step, unit, `check-units` rules
(including that an all-in-one unit configures no worker listener, node certificate or
deny list), the whole-process-crash sim mode, the driver-panic and helper-dispatch
tests. Reverts by the ownership step run backwards, which is safe only while P1 holds;
before P1 a revert is also a state loss.

**Phase 2: the shared cache for inputs.** 2a fill from the tier, 2b the shared budget, in
that order, each behind its own `--shared-cache` flag value (`fill`, `budget`) so each is
measured and reverted alone. The cache keeps its writer, its index and its restart rules.

Each phase is independently shippable; a later one does not rely on an earlier one's flag.
No phase changes a message a remote daemon sends.

## 11. Measurements to record (no gate)

The project owner has ruled (2026-10-15) that performance and operational overhead justify
this work, so **nothing below gates a phase.** The thresholds that were proposals here are
withdrawn. The list stays so the work reports what it bought, and so a number that comes
out badly is seen, not hidden.

Merged: #656 (tier uploads trust the daemon's digests; the flusher hashes each record
before the PUT, and the no-tier path hashes before its PUT). The PRs in flight that change
this exact path: #654 (spool copy on the blocking pool, no CRC-32C), #655 (up to four
segment uploads at once, hardware CRC), #653 (the `outputs phases` line), and the thread
flags proposed alongside. Once these **and the worker-cache
directory cache and link plans** land, measure on the one-node farm and on a split pair of
hosts, same toolchain tree and same action. **Each number is at least five runs on the
farm with variance reported, and names the box (CPU model, cores, RAM, storage).** Each
ratio is stated against its denominator: against the split pair on the same host, or
against the target farm; there is no DuckDB analogue, so no external number is quoted.

Record M2 first: if Commit turns out to be most of the upload time, that is a recorded
observation that transport is not the cost, and the PR says the case is operational plus
the saved read and hash.

| # | Measurement | Where it comes from |
|---|---|---|
| M1 | the `outputs phases` line for the 15k-file output (walk, hash, FindMissing, spool or buffer handoff, upload) | #653, new baseline |
| M2 | server stage timings for that upload: bytes-in, Commit split into hash, tier write, `fdatasync`, `PutStaged` | server upload-path metrics |
| M3 | `PutRange` throughput over loopback: 16 streams, 4 MiB ranges, TCP+TLS vs a null sink vs an in-process call, **with the hash CPU per 15k-file output (one pass in all-in-one, plus the flush's verification) counted for both arms** | `kbf-bench` (`connect.rs` exists); the in-process arm is throwaway bench code |
| M4 | CPU model, `sha_ni`, single-core SHA-256 and CRC-32C throughput on build hosts and storage nodes | one-off |
| M5 | typical action, **broken into**: link syscalls (count, time), `chown-dirs` time, Directory fetch count, run, outputs; node-cache hit rate; bytes filled per action. Taken with and without the directory cache and link plans | `worker-cache.md` metrics |
| M6 | **the farm's CPU as a percentage of host CPU** (server and daemon, mean and p95) and **compile wall time (p50, p95)** over a loaded hour, with and without `--reserve-cpus` at the proposed `R`. This is what a reserve costs the compiles | process metrics, cgroup `cpu.stat`, action timings |
| M7 | memory high-water of the server and daemon on a loaded hour, plus the compile working set of the largest lease: the figures for the formula of 7.2 | cgroup |
| M8 | cold-miss fills: count, bytes and time per hour on the one-node farm | cache fill metrics |

**Phases.** Phase 0 is a refactor and is in progress. Phase 1 proceeds when P1, P2, P3,
P4's remaining condition and P5 hold. Phase 2a and 2b stay behind their own flag values
(`fill`, `budget`) so each is measured and can be reverted alone; M8 and M7 are the
numbers to read for them, and whether a phase whose number shows no gain stays is the
owner's call (there is no gate). The M3 in-process arm is no longer conditional on M2, and its "second hash" charge is
gone with the second hash (section 2).

## 12. Rejected alternatives

**A Unix-socket `Segments` channel between a daemon and a server on one host** (the
first version of this document, #657). Rejected as the main design by the project owner
("the unix socket is a bit inefficient too"), and by its own reviews:

- It keeps a transport: protobuf framing, a kernel copy and flow control, even without
  TLS. The measured cost was not shown to be transport (section 2).
- It needed new authentication the existing admission code does not give: a session
  token, a peer-uid check on both sides, a per-call deny-list check, a socket path from the
  daemon's flag, pre-authentication limits. Each is a permanent surface.
- It was dead code in the target farm and left two binaries and two CPU settings on the
  one box.

**File descriptors over a socket:** a framed protocol, `SCM_RIGHTS`, filesystem
allowlists and a TOCTOU analysis for one saved read. **A shared content-addressed file
store replacing the tier and the node cache:** discards the packed segment (one PUT and one
metadata commit per upload). **Keeping the control plane on mutual TLS to a loopback worker listener in all-in-one**
(this document's earlier position, "an in-process control plane is not designed here"):
replaced by the in-process control plane of section 4, because the owner ruled that all-in-one
serves no daemon but its own, which leaves no reason to open a worker listener or to hold a
node key. The risk it raised, forking the scheduler's commit path, is met by one scheduler
behind one trait with one conformance suite. **A separate in-process deny-list check in
all-in-one:** peer admission guards only the worker listener of `kbf-server`; all-in-one has
none and one node, so the check would guard nothing. **All-in-one plus extra remote
daemons:** a multi-node farm is the split binaries (`kbf-server` + `kbf-daemon`); a hybrid
needs the listener, certificates and admission this shape removes, and its isolation
argument (section 7) assumes no remote peer. **Reflink
or `copy_file_range` into the tier:** block alignment and cross-filesystem behaviour make
it unreliable, and copy-then-hash is TOCTOU-unsafe. **A phase that renames outputs into
the cache:** already `worker-cache.md` 6.7 in every binary (section 5). **One hash pool
shared by the server and daemon halves:** removes the per-side bound the split gives;
two pools instead (section 6). **A probe of `ProtectProc` and `LoadCredential` as a gate:**
the claim it would test is withdrawn (section 7.1), so there is nothing to verify; the
decision rests on the secrets table.

## 13. Open questions

Decided by the project owner (2026-10-15):

1. **Thresholds of section 11:** withdrawn; no measurement gate.
2. **The secrets table of section 7.1 for the one-node farm: accepted.** An escape from a
   build container yields the S3 key (or, with the local store, the tier directory), the
   API token, the server's TLS key and the state directories. Still refused for a host with
   a gate client identity or rollout credential, and for untrusted or multi-tenant work.
3. **P1, the durable server log, is built separately** (a local filesystem blob store and
   the raft metadata log, no S3, for single-host farms) and is a precondition of phase 1.

Open:

4. **Set `--reserve-cpus` on the one-node farm, and to what?** Setting it lowers bookable
   CPUs by `R` (section 6). Recommendation: set it from M6 once recorded; if the farm uses
   under a core at p95, reserve that and no more.
5. **Does a converged two-process host need a server-side CPU bound?** The daemon's `R`
   caps what builds may book and does not bound the server, which is a sibling systemd
   unit bounded only by its `CPUWeight` (section 6). In all-in-one `R` covers both halves.
   Recommendation: accept it for the one-node farm, which runs all-in-one; record the
   server's CPU in M6 on any converged host, and decide on a bound only if it exceeds `R`.
