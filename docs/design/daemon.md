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
The certificate must name `--node-id` as its one DNS subjectAltName, or the server
refuses the session (see
[worker-protocol.md](worker-protocol.md#node-identity-and-the-deny-list)).

## The session and the fence

The daemon connects, sends `Hello`, waits up to 10 seconds for `Welcome`, resends any
unacknowledged results, then heartbeats at the interval `Welcome` named. Every `Hello`
carries the instance id the daemon drew at random when it started, the same on every
stream and never kept across a restart: the scheduler gives up at once only the leases
of this process that a new stream's first heartbeat leaves out, and keeps those of
another process sharing the node's certificate until that one has fenced
([scheduler.md](scheduler.md#reconciling-with-what-workers-say-they-run), issue #140). When the
stream ends for any reason it waits `--reconnect-ms` and tries again. Leases keep
running across reconnects, and the fence clock keeps running whether a stream is up or
not.

Each lease is remembered with the lease epoch the newest `Welcome` named when its
`Start` arrived, and the action the `Start` named; every `Result` echoes that action.
A `Welcome` that names another epoch comes from a server that never granted those
leases (a restarted single-node server): before it resends anything, the daemon kills
their runs and forgets their results, unsent, and lists them only until the runs have
stopped. A `Welcome` that names no epoch drops nothing, and a lease granted while
none was named is kept. See
[worker-protocol.md](worker-protocol.md#server-restarts-and-the-lease-epoch).

The contact clock records the **send** time of the newest message the server
acknowledged (a heartbeat, or the `Hello` a `Welcome` answered):

- if nothing sent in the last two heartbeat intervals has been acknowledged, the daemon
  declares a heartbeat gap and logs it, once per gap;
- once T = 40 s have passed since that send time, contact is lost: every running lease
  is killed (all kills run together) and reported `ABORTED`, and any `Start` that
  arrives is refused with `UNAVAILABLE`.

The fence clock, and the Start window below, count the time the machine spends
suspended: they read `CLOCK_BOOTTIME` on Linux and `CLOCK_MONOTONIC_RAW`
(`mach_continuous_time`) on macOS, and the daemon does not build for an OS without such
a clock. Timers stop during suspend, so while a deadline is pending the daemon never
sleeps longer than `recheck_every` (1 s): a resume past T is noticed within that tick.
Whatever wakes the daemon (a server message, a heartbeat tick, a finished run, a stream
that connects or answers, or that tick), the fence is checked first, so after such a
resume no running lease reports its own result and no `Welcome` or acknowledgement
renews contact before its leases are fenced.

A `Start` that arrives after the window it names (14 s from the send of the heartbeat
it names) is not run, reported or listed, and a `Cancel` from the server kills the
named running lease; see [worker-protocol.md](worker-protocol.md).

Today the daemon fences every lease this way. Letting hermetic work run on through a
lost connection (`RUN_ON`) is **planned** for when `Start` carries a fence policy. At
shutdown the daemon abandons running leases; the scheduler gives them up after G.

## When the daemon is killed

A daemon that stops on SIGTERM or SIGINT kills its leases' processes on the way out. One
that is killed (SIGKILL, the kernel's OOM killer, a panic that aborts) cannot, and
nothing the kernel does to the daemon reaches its actions: each leads a process group
of its own (native driver) or runs in a container (container driver). The daemon
re-adopts nothing (re-adopting is **planned**), so the next daemon on the node ends
them before it says `Hello` (issue #155), as part of building its driver:

- **Native driver.** Before an action's program runs, the daemon writes its run record,
  `<scratch>/runs/lease-<term>-<seq>` (a new file, mode 0600, in a 0700 directory, never
  through a symlink): the leader's pid (the process group id), its start time and the
  boot. The child waits between fork and exec until the record is written; a child
  whose daemon dies first exits without running the program. At start the driver kills
  every recorded group (SIGKILL to the group and to every process found in it or below
  it, again until none is alive), then removes the record, then the lease directories.
  A record of an earlier boot, or whose pid now names a process with another start
  time, names nothing that runs, so nothing is signalled. The daemon never signals its
  own group, its parent or its parent's group, nor a group id of 1 or less. Processes
  of its own user that survive SIGKILL for the kill wait stop the daemon from starting;
  processes of another user named by a record are not its actions, and the record is
  dropped. An entry of `runs/` that is not a regular file of the daemon's user (a FIFO,
  a directory, a symlink), or a `runs` that is not its directory, is moved aside into
  `quarantine/` unread, and the start goes on.
- **Container driver.** Every container is created with the label
  `kbf.owner=<node id>`. At start the driver removes each lease it finds, among the
  containers so labelled and the lease scratch directories, as a lease's clean does
  (`cgroup.kill`, `podman rm --force`, the lease cgroup, the scratch directory). A
  leftover that cannot be listed or removed stops the daemon from starting.

So when a restarted daemon says `Hello`, nothing a previous daemon on the node started
under the same scratch directory (and, for containers, the same node id and Podman
store) still runs, and the leases its first heartbeat leaves out, which the scheduler
requeues once the handover grace has passed, do not run twice. The grace alone would
not do: it lets the older process fence, and a killed process fences nothing. Not
guaranteed:

- **A daemon that is not started again** (its service manager gave up, or the node id
  or scratch directory changed): its runs go on, unfenced, until the machine stops,
  while the scheduler requeues their leases after G. The service manager is the only
  other line. Under systemd, a unit's default `KillMode=control-group` kills everything
  left in the unit's cgroup when its main process dies, the native driver's actions
  included, and the containers' processes too when their cgroups are in the unit's
  delegated subtree; the repository ships no unit yet. launchd, when a job exits,
  kills only the job's own process group (`AbandonProcessGroup` false, the default;
  true stops even that), and has no setting that reaches an action leading a group of
  its own, so on a Mac the shipped plist's `KeepAlive` restart is what ends them.
- **Native: a process that left the group** (`setsid`) and whose parent exited before
  the sweep: nothing links it to the record. A per-lease user or cgroup would close
  this, as it would the same gap in a lease's own kill.
- **Native: a group id the kernel reused** while no daemon ran, in the same boot, whose
  new leader has exited while its group lives on: the record cannot tell that group
  from the action's.
- **Native: the run records.** They are trusted because only the daemon can write
  `runs/`. On macOS the sandbox ensures it. On Linux there is no sandbox until each
  lease runs as its own user, so an action, which runs as the daemon's user, can write
  a well-formed record there naming a group of that user, with its pid and start time
  (both readable in `/proc`), and the next start then SIGKILLs that group.
- **Results.** A result the killed daemon had not had acknowledged is lost (results are
  kept in memory only), and its lease is requeued.

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
   - Give the overlay's directories to the container's root (see User namespaces).
   - Make the lease cgroup (below).
2. **Start.** `podman create`, then `podman start --attach`, with stdout and stderr
   written to files in the scratch directory.
3. **Watch.** Wait for the container to exit, the action's timeout, or `kill`.
4. **Collect.** Read the exit code from Podman's record of the container. If it is 137
   (SIGKILL) and the lease cgroup's `memory.events` counts an `oom_kill`, the lease
   fails as an infrastructure failure. Otherwise give the overlay's directories back
   to the daemon's user, read the outputs (below) and store stdout and stderr in the
   CAS.
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
| Users | `--userns=nomap`: no container uid or gid is the daemon's user (below) |
| Hostname | `localhost` |
| Environment | the `Command`'s variables, passed with `--env` on top of what the image defines |
| Working directory | the `Command`'s, under `/kbf/root` |
| Files | the input root as a read-only overlay lower layer at `/kbf/root`; every write lands in a per-lease upper directory |
| Cgroup | `--cgroup-parent` set to the lease cgroup, `--cgroup-manager=cgroupfs`, `memory.oom.group=1` |
| Timeout | the `Action`'s timeout; one hour when it names none; a negative one is `INVALID_ARGUMENT` |

**Stopping a container** (timeout or kill): SIGTERM, then 5 seconds, then `cgroup.kill`
on the lease cgroup, then a forced removal in the clean step.

### User namespaces

Rootless Podman's default maps a container's root to the daemon's own uid on the host,
so an action would run as the daemon's user, kept from what that user can reach (the
root helpers' sockets, [fleet-updates-security.md](fleet-updates-security.md) S4.3) only
by the mount and pid namespaces. The driver therefore runs every container with
`--userns=nomap`: container ids 0, 1, 2 and up map to the daemon user's subordinate ids
in order, and the daemon's own uid and gid map to nothing.

- **Not `--userns=auto`.** Rootless, it took 65,535 ids of a standard 65,536-id range for
  the first container and refused a second while the first existed ("not enough unused
  IDs in user namespace", Podman 4.9 on the hosted runners), so a node could run one
  action at a time. With `nomap` every container shares one mapping. Containers stay
  apart by their mount, pid, ipc and network namespaces, not by uid. A real-Podman test
  runs a second lease to completion while a first container runs, so a switch back to
  `auto` fails it.
- **Who owns the lease's files.** A container cannot write under a directory whose owner
  it does not map: the overlay refuses with `EROFS`. So after writing the input root and
  making the output directories, the driver gives the overlay's lower, upper and work
  directories to id 1 of Podman's user namespace (the container's root) with
  `podman unshare chown -hR 1:1`. Once the container has exited, and before reading any
  output, it gives them back to id 0 there, the daemon's user, so an output the action
  made `0600` (or a `0700` directory) is read as its owner. `-h` changes a symlink
  itself and `-R` traverses none, so a link the action left hands nothing over.
- **The hand-back is not under the action's timeout.** It runs after the container has
  exited, outside the timeout and outside `kill`, and walks the whole upper tree, which
  the action controls. An action that leaves millions of files makes it as slow as the
  clean step's walk of the same tree; nothing bounds either yet.
- **Cleaning.** A lease that ends before collect (timeout, kill, failure) still has files
  owned by the container's ids. The daemon's user cannot unlink them, so the clean step
  falls back to `podman unshare rm -rf`, as it does for any file the daemon cannot
  remove.

**Node requirement.** The daemon's user needs at least 65,536 subordinate uids in
`/etc/subuid` and as many gids in `/etc/subgid` (`<user>:<first id>:<count>`), what
`useradd` gives a new user; fewer leave an image's high ids (65534, `nobody`) unmapped.
`kbf-daemon --driver container` checks both files at startup and refuses to start,
naming the file, the user and the fix, when either has no range or too small a one.
Ids are counted once: a range listed under both the user's name and uid, or two ranges
that overlap, count their distinct ids. The check reads the files directly, so a user
whose ranges only a directory service holds is refused.
After adding a range (`usermod --add-subuids 100000-165535 --add-subgids
100000-165535 <user>`), run `podman system migrate` as that user so Podman's user
namespace is made again with it.

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
