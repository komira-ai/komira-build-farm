# kbf-all-in-one

The farm server and a worker daemon in one process, for a one-node Linux farm. The
process's own daemon moves its sessions and blobs through calls into the server half
instead of over the network; the worker listener still serves remote daemons (Macs,
daemon-only nodes) as `kbf-server`'s does. Everything else is what `kbf-server` and
`kbf-daemon` do on one host today. The design is in komira-build-farm#657.

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
   async runtime of its own, and prints the same start line on stdout. Its worker
   listener serves remote daemons as `kbf-server`'s does.
4. **The daemon half.** It starts as `kbf-daemon` starts (`kbf_node::start_with`), on an
   async runtime of its own, with `LocalDataPlane` as its data plane in place of the
   transfer engine and `LocalControlPlane` as its control plane in place of the dial to
   `--server`: each of its sessions (`Hello`, leases, heartbeats, results) is opened
   through calls into the server half's worker service, with no TLS, encoding or
   socket, and it never connects to the worker listener.
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
  bound), and hands every chunk to the hasher and to the plane in that same pass. The
  plane keeps the file's bytes and, once the file is hashed, puts it in a batch; a batch
  of 32 MiB (or 16,384 files) is stored as below while the walk goes on, at most four at
  once, and the rest when the walk ends, before the Directory messages and the Tree that
  name them. A file is never read again. A file over the server's largest blob (1 GiB)
  is not taken, and goes as a remote daemon's would.
- **Outputs** (`put_records`): the driver's walk has hashed each file once. The plane asks the cache which digests it lacks, packs those records into the
  segments and uploads the transfer engine plans (`kbf_transfer::plan`), and gives the
  `Segments` handlers each range of their bytes, from the intake's buffers (or read
  from where the walk left them, for what the intake did not take):
  `Begin`, `PutRange`, `Commit`, as for a remote daemon, with no TLS, encoding or socket.
  Nothing is spooled and nothing is hashed on the way. With a write tier, `Commit` trusts
  the digests and the flusher hashes every record before its PUT, dropping one that does
  not match; without one, `PutRange` hashes each record as its bytes arrive.
- **A whole blob** (`put`, and `put_chunks` through it): hashed once, by the server,
  under the admission a ByteStream `Write` takes.
- **Reads** (`get`, and `read_to` through it) and **FindMissing** (`missing`): the cache's
  own, which checks every blob it reads against its digest.

No message a remote daemon sends changes: `kbf-daemon` still uses the transfer engine.

## The in-process control plane

`LocalControlPlane` implements `kbf_node::ControlPlane` (the trait `kbf-daemon` opens its
sessions through; a dialled daemon's is the `Session` call over mutual TLS) over the
server's `LocalWorker` (`ServerHandle::local_worker`). Each session is opened on the
server half's runtime, by the server's own session code: the same `Hello` checks,
registration, heartbeats, `Start`s, `Cancel`s and results as a remote daemon's session on
the worker listener. It carries no certificate, so it is admitted as on a plain-text
worker listener, with no deny list. A session the server refuses, or one on a server
half whose runtime is gone (UNAVAILABLE), is followed by the daemon's reconnect wait, as
a dialled daemon's failed attempt is; a session the server ends is opened again.

No message a remote daemon sends changes: `kbf-daemon`, and any remote daemon of this
process, still dials `--server`.

## How it is tested

- `tests/conformance.rs`: every check of `kbf-cas-conformance` against `LocalDataPlane`,
  over a server started as `kbf-server` starts, with a write tier and without. The
  transfer engine runs the same checks in `kbf-cas-conformance`'s own tests.
- `tests/binary.rs`: the binary end to end. One action runs on the container driver over
  the driver's own stand-in `podman` (`kbf-fake-podman`, built from
  `kbf-driver-container`'s source, which runs the action as a plain process) and a cgroup
  tree of plain directories; its output file, output directory, stdout and stderr are
  read back over REAPI; SIGTERM ends the process with 0. The binary is given no `--cas`,
  so a daemon half wired to the transfer engine would not start. After an action runs
  on the own daemon, the process holds no TCP connection with the worker listener's
  port at either end, and the listener is still bound (its sockets in `/proc/<pid>/fd`
  against the rows of `/proc/<pid>/net/tcp` and `tcp6`). A remote daemon in the test
  (`kbf_daemon::Daemon`, the fake driver, its own certificate) registers through the
  worker listener over mutual TLS and runs an action only it can take. Also the starts
  each half refuses, with their exit statuses, and the binary run with keeper
  arguments, which answers as the keeper before it reads a flag and starts no server.
- `tests/one_read.rs`: the container driver's output collection over a lease of the
  plane, with a write tier and without: an inotify watch counts the opens of each output
  file, which must be one each, and every blob reads back as the file.
- `src/plane_tests.rs`: a digest that does not parse, a server half whose runtime is
  gone, a file changed after the walk hashed it.
- `src/intake_tests.rs`: batches sent while the walk goes on (by bytes and by file
  count), the wait for room among the batches in flight, one digest stored once, a
  file left unfinished storing nothing, a file over the largest blob not taken, and a
  server half that is gone.
- `src/control_tests.rs`: a session through the plane is the server's own (a first
  message that is not `Hello` refused, a `Hello` welcomed and a heartbeat acknowledged),
  and a server half whose runtime is gone refuses each session UNAVAILABLE.
- `src/cli_tests.rs`: each flag reaches its half, once; the refusals.
- `kbf-daemon`'s `tests/in_process.rs`: a daemon given an in-process plane and no
  usable server URL or TLS file opens every session through it, retries one refused,
  runs a lease on one welcomed, and opens another when the server ends it.

## Not done yet

- **Production use.** The server half's scheduler state is in memory, so a restart of
  the process loses every lease and operation (the cache's index is kept: the server's
  metadata log is on disk, under `--meta-dir` or the hot store). Until the
  scheduler's state is durable, all-in-one is a development shape.
- **The in-process planes' admission.** A remote daemon's calls are admitted by its
  certificate and the deny list; the own daemon's calls through the in-process planes
  are not admitted.
- **The own daemon's dial flags.** `--server`, `--ca-cert`, `--cert` and `--key` are
  still required, as `kbf-daemon` requires them, though the own daemon reads none of
  them: it neither dials nor opens a TLS file.
- **`--reserve-cpus` across both halves.** The daemon half takes the flag as
  `kbf-daemon` does; the server half uses every CPU, as `kbf-server` does.
- **Crash and panic behaviour** (the next change): the whole-process crash mode of the
  simulation, a test that a panic in a driver task leaves the process serving, and the
  rest of the helper-dispatch tests (each helper mode runs and exits with its code, with
  no listener bound and no server directory opened).
- **Deploying it** (the change after): the `all-in-one` shape in `kbf-deploy`, its unit
  and `check-units` rules, the ownership step that moves the state directories and keys
  between the split pair and this binary, and its place among the release artifacts.
