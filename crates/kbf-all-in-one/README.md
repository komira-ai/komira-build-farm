# kbf-all-in-one

The farm server and a worker daemon in one process, for a one-node Linux farm. The
daemon's blobs move through calls into the server's cache instead of over the network;
everything else is what `kbf-server` and `kbf-daemon` do on one host today. The design
is in komira-build-farm#657.

Linux only, the container driver only. Elsewhere the binary exits 2 and says to run
`kbf-server` and `kbf-daemon`.

## What the binary does today

1. **The helpers first.** Before any flag is read, a run of this binary as one of the
   daemon's helpers (each run's keeper, the container driver's `__chown-dirs`) does that
   and exits, as `kbf-daemon` does. So a helper never reads the command line, opens the
   server's directories or starts a runtime.
2. **One command line.** Every flag of `kbf-daemon` and every flag of `kbf-server`, in one
   command. The three thread overrides that both binaries have (`--async-threads`,
   `--blocking-threads`, `--cpu-threads`) size the daemon half, with `--reserve-cpus`; the
   server half sizes itself as `kbf-server` does when none is given. Refused: a
   `--driver` other than `container`, and `--cas`.
3. **The server half.** It starts as `kbf-server` starts (`kbf_server::start`), on an
   async runtime of its own, and prints the same start line on stdout.
4. **The daemon half.** It starts as `kbf-daemon` starts (`kbf_node::start_with`), on an
   async runtime of its own, with `LocalDataPlane` as its data plane in place of the
   transfer engine. Its control plane is a remote daemon's: `--server` names the server
   half's worker listener, over mutual TLS with the node's certificate, as any node's.
5. **Stopping.** SIGTERM or SIGINT stops the server half as it stops `kbf-server`; the
   daemon half stops once the server half has, abandoning the leases still running, and
   the process exits 0. A server half that stops serving on its own stops the daemon too,
   and the process exits 1. A start the server half refuses exits 2, as `kbf-server`
   does; one the daemon half refuses exits as `kbf-daemon` does (1, or 2 for an
   `--image` the node cannot run).

## The in-process data plane

`LocalDataPlane` implements `kbf_transfer::Cas` over the server's `LocalStore`
(`ServerHandle::local`). Every call runs on the server half's runtime, through the
server's own code:

- **Each output file, stdout and stderr, read once** (`intake`, `src/intake.rs`): the
  container driver reads each file once, one 1 MiB chunk at a time (its per-file memory
  bound), and hands every chunk to the hasher and to the plane in that same pass. A
  file over 16 MiB (`kbf_front::MAX_UNSTAGED_BLOB_BYTES`) is never held in memory and
  never refused, whatever its size. With a hot store that has room for it, it is
  **streamed onto the write tier** (`LocalStore::stream`): each chunk is written to a
  block of its own as it arrives, then checked (length, CRC-32C), asked FindMissing,
  and committed as one `PutStaged`; the flusher checks its digest before the PUT. With
  no hot store, or a full one, it is **streamed to the cold store**
  (`LocalStore::put_streamed`), as a ByteStream `Write` of a large blob is: its chunks
  pass through a queue of two (the walk waits when it is full), hashed as they go into
  the store's write (a multipart upload on S3), and the store completes the object only
  once they hash to the digest the walk names at the file's end. A file of at most
  16 MiB is kept in memory and put in a batch; a batch of 32 MiB (or 16,384 files) is
  stored as below while the walk goes on, at most four at once, and the rest when the
  walk ends, before the Directory messages and the Tree that name them. A file is
  never read again. The one ceiling is S3's: about 48.8 TiB per object (10,000 parts
  of 5 GiB).
- **Outputs** (`put_records`): the driver's walk has hashed each file once. The plane asks the cache which digests it lacks, packs those records into the
  segments and uploads the transfer engine plans (`kbf_transfer::plan`), and gives the
  `Segments` handlers each range of their bytes, from the intake's buffers (or read
  from where the walk left them, for what the intake did not take):
  `Begin`, `PutRange`, `Commit`, as for a remote daemon, with no TLS, encoding or socket.
  Nothing is spooled and nothing is hashed on the way. With a write tier, `Commit` trusts
  the digests and the flusher hashes every record before its PUT, dropping one that does
  not match; without one, `PutRange` hashes each record as its bytes arrive. An upload
  holding a record over 16 MiB that the server has no tier room for (`Begin` answers
  FAILED_PRECONDITION) is stored record by record instead, as the transfer engine
  falls back to ByteStream.
- **A whole blob** (`put`, and `put_chunks` through it): hashed once, by the server,
  under the admission a ByteStream `Write` takes; a `put_chunks` blob over 16 MiB is
  streamed to the cold store as its chunks are read, never gathered.
- **Reads** (`get`) and **FindMissing** (`missing`): the cache's own, which checks every
  blob it reads against its digest.
- **Streamed reads** (`read_to`, what the input cache fills a file with): the cache's
  streamed read (`LocalStore::read_stream`), pumped on the server half's runtime a chunk
  at a time into the file as it arrives (at most two chunks between the halves, never
  the blob), and hashed: a stream cut short is not taken for the blob.

No message a remote daemon sends changes: `kbf-daemon` still uses the transfer engine.

## One cache (`src/one_cache.rs`)

When the hot store (`--hot-store`) is a directory on the filesystem of `--data-dir`,
the daemon half's input cache and the server half's hot store are one cache on that disk
([architecture-v2.md](../../docs/design/architecture-v2.md#10-locality),
`kbf_storage::local_cache`):

- **One copy of a blob.** The input cache's directory (`<data-dir>/cache`) and its index
  are the node's only local copies: the hot store deletes a block once it is flushed
  rather than keeping a flushed copy, and a start deletes the flushed copies an earlier
  run kept. A lease's input is a hard link to the entry (the same inode), and a shared
  read-only tree (`<data-dir>/trees`, at its own path) is built from links to entries.
  What the hot store still holds is its unflushed blocks: acknowledged uploads waiting
  for the cold store, which no cache may drop. While one of them holds a blob a lease
  also needs, the blob is on the disk twice, until that block is flushed.
- **One budget.** The input cache evicts its entries (least recently used, never one a
  running lease pins) until they and the hot store's blocks fit its budget: after each
  input it fills, and within about a second of the hot store growing (uploads with no lease
  running). A hot store budgeted at or above the input cache leaves room for pinned
  entries alone; the daemon warns when its blocks fill the whole budget.
- **One fill.** `/metrics` reports `kbf_local_cache_input_bytes`,
  `kbf_local_cache_storage_bytes` and their sum, `kbf_local_cache_fill_bytes`.

With no hot store, or one on another filesystem, each half keeps what it keeps without
the other, and the start says why (`one cache: off (...)`). The cold store is the
durable store, not a cache: a `file://` cold store on the same disk holds its own copy.

## How it is tested

- `tests/conformance.rs`: every check of `kbf-cas-conformance` against `LocalDataPlane`,
  over a server started as `kbf-server` starts, with a write tier and without. The
  transfer engine runs the same checks in `kbf-cas-conformance`'s own tests.
- `tests/binary.rs`: the binary end to end. One action runs on the container driver over
  the driver's own stand-in `podman` (`kbf-fake-podman`, built from
  `kbf-driver-container`'s source, which runs the action as a plain process) and a cgroup
  tree of plain directories; its output file, output directory, stdout and stderr are
  read back over REAPI; SIGTERM ends the process with 0. The binary is given no `--cas`,
  so a daemon half wired to the transfer engine would not start. Also the starts each
  half refuses, with their exit statuses, and the binary run with keeper arguments,
  which answers as the keeper before it reads a flag and starts no server.
- `tests/one_read.rs`: the container driver's output collection over a lease of the
  plane, with a write tier and without: an inotify watch counts the opens of each output
  file, which must be one each (a file over 1 GiB too, held after with a tier and
  without one), and every blob reads back as the file; a blob over 5 GiB is not
  refused for its size.
- `tests/binary.rs` also runs one action whose 3 MiB input a client uploaded: once the
  hot store is flushed, one inode under the hot store and the data directory holds the
  input (the input cache's entry) and `/metrics` counts it once; after a SIGKILL and a
  restart, the same action leaves the same.
- `tests/one_cache.rs`: the input cache filled over the plane: a 12 MiB input reaches
  its file through one streamed read and no whole-blob read, the file never more than
  five chunks behind what the stream handed out; two leases needing one input at once
  read it once, into one entry both link; what a streamed read answers when the stream
  is damaged or cut, the blob is absent, the file cannot be written or the server half
  is gone; the hot store's staged blocks reported to the one cache as its storage
  bytes; and a server half that keeps flushed copies without the one cache and none
  with it.
- `tests/memory.rs`: a counting allocator over the whole in-process path (the walk,
  the plane, the server's upload until its commit): a 2 GiB output streams onto the
  write tier with under 16 MiB live, and so does a 2 GiB upload through the server's
  `Segments` handlers (a remote daemon's path); without a tier, a 256 MiB output
  streams to the cold store with under 16 MiB live.
- `tests/helpers.rs`: the daemon's helpers run through this binary. The keeper,
  waiting on its stdin as it does for a run's whole life, has one thread and no
  socket, epoll, eventfd or directory open (read from `/proc`), then writes its kill
  file and exits 0; `__chown-dirs` exits 0, 1 and 2 as the helper does, in its own
  words.
- Crash and panic behaviour, in the crates that hold it: `kbf-sim-cell`'s
  `Kind::WholeProcess` kills the whole process, both halves, mid-build, and starts it
  again (property 14: no action is answered as succeeded with its outputs missing,
  and the index and action cache survive the kill; the actions the kill cut are run
  again on the client's retry); `kbf-daemon`'s `lease_panic_tests.rs`: a run whose
  driver panics reports INTERNAL and the next lease runs; the hashing pools'
  own tests (`kbf-front`'s `HashPool`, `kbf-daemon`'s `CpuPool`): a job that panics
  is an error to its caller and the pool keeps its size.
- `src/plane_tests.rs`: a digest that does not parse, a server half whose runtime is
  gone, a file changed after the walk hashed it; without a tier, a `put_chunks` blob
  over 16 MiB streamed (held, not read again once held, corrupt, unreadable) and an
  upload with one stored record by record.
- `src/one_cache_tests.rs`: one cache only when the hot store shares the data
  directory's filesystem, given to both halves.
- `src/intake_tests.rs`: batches sent while the walk goes on (by bytes and by file
  count), the wait for room among the batches in flight, one digest stored once, a
  file left unfinished storing nothing, a file over 5 GiB taken, a large file
  without a tier streamed to the store once (withdrawn when seen or held, nothing
  stored when unfinished or when its bytes fail its digest), and a server half that
  is gone.
- `src/cli_tests.rs`: each flag reaches its half, once; the refusals.

## Not done yet

- **Production use.** The server half's scheduler state is in memory, so a restart of
  the process loses every lease and operation (the cache's index is kept: the server's
  metadata log is on disk, under `--meta-dir` or the hot store). Until the
  scheduler's state is durable, all-in-one is a development shape.
- **The in-process plane's admission.** A remote daemon's blob calls are admitted by its
  certificate and the deny list; this plane's calls are not admitted.
- **`--reserve-cpus` across both halves.** The daemon half takes the flag as
  `kbf-daemon` does; the server half uses every CPU, as `kbf-server` does.
- **The simulation's in-process data plane.** In `kbf-sim-cell` the daemon half's
  outputs still cross the modelled network; the design's in-process mode (bytes off
  the network model, the control plane on it) and its property A2 (the same final
  index in both modes) are not built, nor A3 (needs a durable scheduler).
- **Deploying it** (the change after): the `all-in-one` shape in `kbf-deploy`, its unit
  and `check-units` rules, the ownership step that moves the state directories and keys
  between the split pair and this binary, and its place among the release artifacts.
