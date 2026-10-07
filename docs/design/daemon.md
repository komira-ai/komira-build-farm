# The daemon and execution

`kbf-daemon` runs on every worker machine. It reports what the machine is, holds one
session with the server, runs the leases the server starts through an execution
driver, and leaves nothing behind. This document covers the daemon (`kbf-daemon`), the
container driver (`kbf-driver-container`), how outputs are collected, and the limits
an action runs under. The messages are in [worker-protocol.md](worker-protocol.md).

## Parts of the daemon

| Part | Module | Does |
|---|---|---|
| Detection | `report` | builds the node report from the kernel's text (see [capabilities.md](capabilities.md)) |
| Session loop | `daemon` | one outbound mutual-TLS stream at a time; reconnects when it ends |
| Contact clock | `contact` | when the server last provably heard the daemon, and so when to fence |
| Start window | `window` | when this stream's heartbeats were sent, so a `Start` that arrives too late is not run |
| Lease manager | `lease` | starts work only on `Start`, turns each outcome into one `Result`, fences, kills a cancelled lease |
| Runtime | `runtime` | the `Runtime` trait every execution driver implements |
| CAS client | `cas` | reads inputs and writes outputs over the server's ByteStream service |
| Trees | `tree` | writes an input root to disk and reads outputs back |
| Usage | `usage` | measures a child process's CPU time and peak memory when it is reaped |

The daemon is configured by flags only: `--server` (an `https://` URL), `--ca-cert`,
`--cert`, `--key`, `--tls-server-name`, `--node-id`, `--runtime` and `--reconnect-ms`.

## The session and the fence

The daemon connects, sends `Hello`, waits up to 10 seconds for `Welcome`, resends any
unacknowledged results, then heartbeats at the interval `Welcome` named. When the
stream ends for any reason it waits `--reconnect-ms` and tries again. Leases keep
running across reconnects, and the fence clock keeps running whether a stream is up or
not.

The contact clock records the **send** time of the newest message the server
acknowledged (a heartbeat, or the `Hello` a `Welcome` answered):

- if nothing sent in the last two heartbeat intervals has been acknowledged, the daemon
  declares a heartbeat gap and logs it, once per gap;
- once T = 40 s have passed since that send time, contact is lost: every running lease
  is killed (all kills run together) and reported `ABORTED`, and any `Start` that
  arrives is refused with `UNAVAILABLE`.

A `Start` that arrives after the window it names (14 s from the send of the heartbeat
it names) is not run, reported or listed, and a `Cancel` from the server kills the
named running lease; see [worker-protocol.md](worker-protocol.md).

Today the daemon fences every lease this way. Letting hermetic work run on through a
lost connection (`RUN_ON`) is **planned** for when `Start` carries a fence policy. At
shutdown the daemon abandons running leases; the scheduler gives them up after G.

## The runtime trait

```rust
pub trait Runtime: Send + Sync + 'static {
    fn driver(&self) -> &'static str;          // listed under `drivers` in the node report
    fn serves(&self, kind: &str) -> bool;      // does it run this lease kind?
    fn run(&self, work: Work) -> impl Future<Output = Result<ActionResult, RuntimeError>> + Send;
    fn kill(&self, lease_id: LeaseId) -> impl Future<Output = ()> + Send;
}
```

The lease manager never names a driver. It asks the runtime whether it serves a lease
kind and runs the lease through it. `run` returns `Ok` for an action that ran, whatever
its exit code. Its errors map to the `Result` status the server sees:

| `RuntimeError` | `Result` status |
|---|---|
| `Killed` | `ABORTED` |
| `TimedOut` | `DEADLINE_EXCEEDED` |
| `Invalid(why)` | `INVALID_ARGUMENT`: the client's error, never retried |
| `MissingBlob(digest)` | `FAILED_PRECONDITION` with a `MISSING` violation |
| `Failed(why)` | `INTERNAL`: the farm's failure |

Three runtimes exist:

- **`PodmanRuntime`** (`kbf-driver-container`, driver `container`): the runtime for farm
  nodes, below.
- **`FakeRuntime`** (driver `fake`): runs nothing and returns an empty result. The only
  runtime the `kbf-daemon` binary offers today, for bring-up; the binary that runs the
  container driver is **planned**.
- **`LocalRuntime`** (driver `local`): **tests only**. Runs each action as a plain child
  process of the daemon so the whole path (fetch, write inputs, run, measure, upload,
  report) can be tested where no container runtime exists. It isolates nothing and
  applies no timeout. The end-to-end harness uses it.

## The container driver

`PodmanRuntime` runs each `action` lease in a fresh rootless Podman container that is
never reused. Each lease goes through six steps.

1. **Prepare.**
   - Fetch the `Action` and `Command` from the CAS and verify them.
   - Read the image from the `container-image` platform property (on the `Action`, or
     on the `Command` for clients older than REAPI 2.2). It must be
     `docker://<repo>@sha256:<digest>`: a tag is refused, and so is a digest that names
     an image index (a multi-architecture list) rather than one architecture's manifest.
     The driver tells the two apart by reading the manifest the node's image store
     keeps under that digest, after checking that its bytes hash to the digest. The image
     must already be in the node's store; nodes never pull at action time.
   - Check the `Command`: it must have arguments; the working directory and every output
     path must be relative, without `.`, `..` or empty components.
   - Write the input root into the lease's scratch directory. Every name in a client's
     `Directory` messages is checked (no slash, `.`, `..` or NUL; no name twice) and
     files are created exclusively in directories the driver made, so no write follows
     a symlink the input tree planted.
   - Refuse an action whose output path already exists in the input root, and one whose
     working directory is hidden by an input file or symlink of the same name, as
     `INVALID_ARGUMENT`.
   - Make the lease cgroup (below).
2. **Start.** `podman create`, then `podman start --attach`, with stdout and stderr
   written to files in the scratch directory.
3. **Watch.** Wait for the container to exit, the action's timeout, or `kill`.
4. **Collect.** Read the exit code from Podman's record of the container. If it is 137
   (SIGKILL) and the lease cgroup's `memory.events` counts an `oom_kill`, the lease
   fails as an infrastructure failure. Otherwise read the outputs (below) and store
   stdout and stderr in the CAS.
5. **Clean.** Remove the container, the lease cgroup and the scratch directory.
6. **Verify clean.** Neither directory may remain.

Cleaning runs on every path out of a lease: success, failure, timeout and kill. If the
daemon drops a lease's future instead, the lease's destructor cleans up before the
future is gone. A clean that fails turns the lease into a failure: a dirty node must be
loud.

### What the container gets

| Aspect | Setting |
|---|---|
| Network | `--network=none`: loopback only. No action has network today. |
| Image | by per-architecture manifest digest; `--pull=never` |
| Entrypoint | the action's argv as a JSON array: the image's `ENTRYPOINT` and `CMD` are ignored and no argument is re-split |
| Hostname | `localhost` |
| Environment | the `Command`'s variables, passed with `--env` on top of what the image defines |
| Working directory | the `Command`'s, under `/kbf/root` |
| Files | the input root as a read-only overlay lower layer at `/kbf/root`; every write lands in a per-lease upper directory |
| Cgroup | `--cgroup-parent` set to the lease cgroup, `--cgroup-manager=cgroupfs`, `memory.oom.group=1` |
| Timeout | the `Action`'s timeout; one hour when it names none; a negative one is `INVALID_ARGUMENT` |

**Stopping a container** (timeout or kill): SIGTERM, then 5 seconds, then `cgroup.kill`
on the lease cgroup, then a forced removal in the clean step.

### Cgroups and limits

The daemon owns a delegated cgroup v2 subtree, `actions/`, whose
`cgroup.subtree_control` enables `cpu`, `memory` and `pids`. For each lease the driver
makes `actions/kbf-lease-<term>-<seq>` and puts the container under it.

- **Memory** follows a soft-limit policy. `memory.high` = booked memory x 1.5 + 512 MiB,
  so an action that needs a little more than booked slows down rather than dies. There
  is no per-lease `memory.max` and no `memory.swap.max`, so swap stays allowed. The one
  hard limit is `memory.max` on `actions/`, set by whoever runs the daemon's unit, which
  keeps the node itself safe. `memory.oom.group=1` makes a kernel OOM kill take the whole
  action, not part of it. Nothing is set when no memory was booked.
- **CPU** is compressible: `cpu.weight` = booked millicores / 10, clamped to 1..10000
  (one core is the kernel's default weight of 100). The driver never sets `cpu.max`,
  because throttling distorts wall time.
- **OOM detection** reads the lease cgroup's `memory.events`, which outlives the
  container. Podman's own OOM flag is not trusted (it reads false when rootless).
- **Removal** of a lease cgroup that is still busy (a process still exiting) writes
  `cgroup.kill` and retries.

## Output collection

Outputs are read from the overlay's upper directory, which holds only what the action
created or changed. That is why no output may already exist in the input root: an
output read from the upper directory must be whole there.

The upper directory is the action's to shape. It could replace the working directory,
or any directory on an output's path, with a symlink to a host path
(`rm -rf d; ln -s /host/dir d`, output `d/key.pem`). So **no output is read by path**:

- The upper directory is opened once. Every component below it, the working
  directory's included, is opened relative to its parent's descriptor with
  `O_NOFOLLOW`.
- A component on the way that is absent, a file or a symlink means the action did not
  create the output there, and the output is left out.
- The last component is examined without following it. A symlink is recorded as a
  symlink and never dereferenced. A file is opened with `O_NOFOLLOW` and its type is
  checked again on the open descriptor, so an entry that changed after it was examined
  is an error, never read. A FIFO never blocks the open.
- The same rules hold inside an output directory. Entries are sorted by name, as REAPI
  wants.

An output directory is walked iteratively, with its stack of open directories on the
heap, so no depth of tree can overflow a thread's stack. Only the directory being read
holds a descriptor; the walk returns to a parent through `..` and refuses a `..` that
is not the directory it came from (by device and inode).

**Output limits** (`OutputLimits`; the walk stops and the action fails past any one of
them, recording none of its outputs):

| Limit | Default |
|---|---|
| directory depth below an output directory | 512 levels |
| entries across all outputs (files, directories, symlinks, others) | 1,000,000 |
| bytes across all output files | 16 GiB |

Every output file, stdout and stderr is stored in the CAS before the `Result` is sent;
the server refuses an `OK` result whose outputs are not all stored.

## Measuring usage

`usage::Child` spawns an action as the leader of its own process group and reaps it
with `wait4`, which returns the CPU time and peak resident memory of the process and
every descendant it waited for. These are packed into a `ResourceUsage` inside the
result (see [worker-protocol.md](worker-protocol.md#resourceusage)). Today the local
runtime reports usage; the container driver does not yet.

## Planned

- A shipped daemon binary that runs the container driver, with the driver's
  configuration as flags.
- **Re-adopting leases across a daemon restart:** each lease runs as its own systemd
  unit, so work continues while the daemon restarts, and the daemon lists every
  re-adopted lease in its first heartbeat.
- **Raising `memory.high`** while the node has room, so an action that outgrew its
  booking keeps full speed; pausing new leases under memory pressure.
- **Image import:** images copied from farm storage into the node's store once, by
  digest, with layers verified; a node never pulls from a public registry at action
  time.
- **Prefetch and a local cache:** inputs fetched as soon as a lease is offered, and a hot
  set of toolchains and images kept on the node and reported to placement.
- **Networked actions,** which get egress, are never cached and always self-fence.
- **Service containers** for tests that need several containers, declared up front so
  the result stays cacheable.
- **Other drivers** behind the same trait: native execution and whole-machine leases on
  macOS, and virtual machines later. Every driver must pass one freshness test: a
  marker left by one lease is never seen by the next.
- **Usage from the container driver,** read from the lease cgroup.
