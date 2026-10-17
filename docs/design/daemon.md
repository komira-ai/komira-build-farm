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
| CAS | `cas` | the `Cas` trait (from `kbf-transfer`) runtimes read inputs and write outputs through; the binary's runtimes share one transfer engine (`kbf-transfer`), which speaks ByteStream and FindMissing to the worker listener (`--cas`, mutual TLS) over one connection |
| Trees | `tree` | writes an input root to disk and reads outputs back |
| Usage | `usage` | measures a child process's CPU time and peak memory when it is reaped |

The daemon is configured by flags only: `--server` (an `https://` URL; repeatable, or
several separated by commas), `--ca-cert`, `--cert`, `--key`, `--tls-server-name`,
`--node-id`, `--runtime`, `--reconnect-ms` and `--reconnect-max-ms`. How long a silent
storage node is waited on is set by `--storage-dial-timeout-ms`,
`--storage-ping-interval-ms` and `--storage-ping-timeout-ms` (10000 each; none is zero or
over 3600000, one hour, and the ping timeout is at most the interval); a daemon with one of them wrong does not
start.
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
stream ends it waits `--reconnect-ms` and tries again. Leases keep running across
reconnects, and the fence clock keeps running whether a stream is up or not.

### Reaching a server

The daemon never stops trying to reach a server (module `connect`). Each round of
attempts resolves every `--server` host again (a DNS name may carry a record per
server; an address literal resolves to itself) and tries each address in turn,
starting after the address it tried last. An attempt that ends before `Welcome` for
any reason (a refused connection, a TLS failure, a server answering `UNAVAILABLE`, no
connection or no `Welcome` within 10 seconds) moves on to the next address at once.
When every address of a round failed, the daemon waits and starts another round: the
wait after the `n`th failed round in a row is `--reconnect-ms` (default 1000) times
2^(n-1), at most `--reconnect-max-ms` (default 30 000), less a random part of up to
half. After a welcomed session ends, the wait is `--reconnect-ms`, jittered the same
way, and the count starts over.

Each attempt verifies the server certificate against `--tls-server-name`, or the
URL's host when that flag is not given, whatever address the host resolved to.
Failed attempts are logged at WARN at most once a minute while they repeat (the first
after a session always is), the rest at DEBUG; each WARN counts the attempts since the
last session. An observer of the daemon's events sees `ConnectFailed` for each failed
attempt and `Retrying` (failed rounds, the wait) for each wait. The blob client
(`--cas`) is a single URL; it is not covered by this.

Both channels, the worker stream's and the blob client's, send an HTTP/2 PING every
20 seconds, idle or not, and close the connection when one is not acknowledged within
20 seconds (`KeepAlive`). A connection that died without a FIN or RST then fails its
calls within 40 seconds, rather than leaving a call waiting for an answer that cannot
come.

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
  is killed (all kills run together) and reported lost (`UNAVAILABLE` with
  `Result.lost` = `LOST_FENCED`, which the server runs again as a farm fault), and any
  `Start` that arrives is refused with `UNAVAILABLE`.

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
re-adopts nothing (re-adopting is **planned**), so its runs are ended twice over (issue
#155): as it dies, by each run's keeper, and when a daemon is started again on the
node, before it says `Hello`, by that daemon's start-up sweep.

**The keeper.** Each run gets one: the daemon's own binary started again in its hidden
keeper mode (`kbf-keeper __keeper group` or `__keeper cgroup <cgroup.kill> <tries>`;
`kbf_daemon::keeper`), which `main` dispatches before it parses any flag. Its stdin is a
pipe only the daemon holds the other end of (close-on-exec, so no action inherits it).
The daemon never writes the pipe. When the daemon dies, however it dies, the kernel
closes its end; the keeper reads end-of-file and ends the run. It ignores SIGHUP, SIGINT
and SIGTERM, so a service manager stopping the daemon's unit with SIGTERM does not stop
it first, and only then writes a ready line on stdout: the daemon waits for that line,
so a run starts only once its keeper ignores those signals, and a program that does not
answer as a keeper refuses the run. The program is `/proc/self/exe` on Linux, the file
the daemon runs even after an update renamed a new binary over it; on macOS it is the
path the daemon was started by, which names the new binary after such a swap, so the
keeper's arguments are kept the same from one version to the next.
This needs no restart, so a killed daemon's runs no longer go on while the scheduler
requeues their leases elsewhere after the handover grace.

- **Native driver.** The keeper joins the action's process group before the program
  runs: the daemon starts it between writing the run record and sending the go byte
  (below), and a keeper that cannot be started, or cannot join the group, refuses the
  run as an unwritten record does. At end-of-file it SIGKILLs its own group
  (`kill 0`), the action's. Being a member is what makes that safe: while it lives the
  group id names this action's group, since the kernel reuses no pid that names a live
  group; and the daemon's kill of the group at the end of a lease, or the sweep's,
  ends the keeper with the rest, so it needs no dismissal. It counts toward the
  lease's memory.
- **Container driver.** The keeper starts once the lease cgroup is made, before
  `podman create`, in a process group of its own. At end-of-file it writes the lease
  cgroup's `cgroup.kill`, which kills every process in the lease, the container's
  among them, and again every second for 30 seconds, while the file exists: a
  `podman start` the dead daemon left in flight can still put the container's first
  process in the lease after the first write. The container, the lease cgroup and the
  scratch directory stay, with nothing running in them, until the sweep. The daemon
  removes the keeper (SIGKILL, then it reaps it) once the lease is cleaned.

**The start-up sweep** runs as part of building the driver:

- **Native driver.** Before an action's program runs, the daemon writes its run record,
  `<scratch>/runs/lease-<term>-<seq>` (a new file, mode 0600, in a 0700 directory, never
  through a symlink): the leader's pid (the process group id), its start time and the
  boot. The child waits between fork and exec until the record is written; a child
  whose daemon dies first exits without running the program. At start the driver kills
  every recorded group (SIGKILL to the group and to every process found in it or below
  it, again until none is alive), then removes the record, then the lease directories.
  A record of an earlier boot, or whose pid now names a process with another start
  time, names nothing that runs, so nothing is signalled. The daemon never signals its
  own group, its parent or its parent's group, nor a group id of 1 or less: a record
  naming either group is read and dropped, and that group is not walked. Processes of
  its own user that survive SIGKILL for the kill wait stop the daemon from starting;
  processes of another user, and those it never signals, found in or below a recorded
  group are not its actions: they are left alone (the walk goes on below them), and
  the record is dropped. An entry of `runs/` that is not a regular file of the daemon's
  user that only that user may write (a FIFO, a directory, a symlink, a group- or
  other-writable file), or a `runs` that is not such a directory, is moved aside into
  `quarantine/` unread, and the start goes on. A `quarantine` that is not the daemon's
  directory (a symlink, or a directory others may write) is removed, never emptied
  through; the daemon's own is emptied after the kill, through the descriptor it
  checked, and what the sweep moves aside goes in through that descriptor too.
- **Container driver.** Every container is created with the label
  `kbf.owner=<node id>`. At start the driver removes each lease it finds, among the
  containers so labelled and the lease scratch directories, as a lease's clean does
  (`cgroup.kill`, `podman rm --force`, the lease cgroup, the scratch directory). A
  leftover that cannot be listed or removed stops the daemon from starting.

So when a restarted daemon says `Hello`, nothing a previous daemon on the node started
under the same scratch directory (and, for containers, the same node id and Podman
store) still runs, and the leases its first heartbeat leaves out, which the scheduler
requeues once the handover grace has passed, do not run twice. That is also why the
restarted daemon may say so: once every runtime is built, the `kbf-daemon` binary
names the previous daemon process, whose id it finds in `<--data-dir>/leases/daemon-instance`,
as `Hello.predecessor_instance_id`, and writes its own id there for the next one. The
server then requeues those leases at once, not after the grace, if the previous
stream has closed (issue #22; [scheduler.md](scheduler.md#reconciling-with-what-workers-say-they-run)). The grace alone would
not do: it lets the older process fence, and a killed process fences nothing. Not
guaranteed:

- **A keeper that dies with the daemon, and a daemon that is not started again**
  (its service manager gave up, or the node id or scratch directory changed): a run
  whose keeper was killed too (by the OOM killer, by hand) goes on, unfenced, until
  the machine stops, while the scheduler requeues its lease after G. The service
  manager is the only other line. Under systemd, a unit's default
  `KillMode=control-group` kills everything left in the unit's cgroup when its main
  process dies, the native driver's actions included, and the containers' processes
  too when their cgroups are in the unit's delegated subtree, as under the units the
  deploy guides give ([linux-build-host.md](../deploy/linux-build-host.md#the-units),
  [storage-node.md](../deploy/storage-node.md)), which keep that default. launchd, when a job exits, kills only the job's own process group
  (`AbandonProcessGroup` false, the default; true stops even that), and has no
  setting that reaches an action leading a group of its own, so on a Mac the shipped
  plist's `KeepAlive` restart is the line after the keeper.
- **Container: a `podman start` that outlasts the keeper's 30 seconds** of kills, and
  puts the container's process in the lease after the last one.
- **Native: a process that left the group** (`setsid`): the keeper does not reach it,
  and once its parent has exited (the keeper's kill of the group may be what ends the
  parent) nothing links it to the record either, so the sweep misses it too. A
  per-lease user or cgroup would close this, as it would the same gap in a lease's own
  kill.
- **Native: a group id the kernel reused** while no daemon ran, in the same boot, whose
  new leader has exited while its group lives on: the record cannot tell that group
  from the action's.
- **Native: the run records.** They are trusted because only the daemon can write
  `runs/`. On macOS the sandbox ensures it. On Linux there is no sandbox until each
  lease runs as its own user, so an action, which runs as the daemon's user, can write
  a well-formed record there naming a group of that user, with its pid and start time
  (both readable in `/proc`), and the next start then SIGKILLs that group and every
  process below it. A record may name a group the daemon may not signal, another
  user's: that group is left alone, but the walk goes on below it, and the daemon's
  user's processes there are SIGKILLed. Only the daemon's own group and its parent's
  are not walked.
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

The lease manager never names a driver. A daemon holds a set of runtimes
(`Runtimes`), and each Start runs on the one that serves its kind; a kind none serves
is refused at once with `FAILED_PRECONDITION` (`no driver here serves lease kind`). A
lease is killed (on `Cancel`, or by the fence) through the runtime it runs on. The set
refuses a runtime that serves a kind of `LeaseKind::ALL` another in it already serves,
so no Start can run on either of two; the binary then refuses to start, naming both
drivers and the kind. The node report lists every runtime's driver under `drivers`:
`Daemon::new` adds them to the report it is given, so the Hello of every stream, and
the Hello resent when a driver's report changes, carries all of them.

The trait returns `impl Future`, so the set holds each runtime behind a private
object-safe twin of the trait that boxes `run`'s and `kill`'s futures: one allocation
per lease and per kill. `run` returns `Ok` for an action that ran, whatever
its exit code. Its errors map to the `Result` status the server sees:

| `RuntimeError` | `Result` status |
|---|---|
| `Killed` | `ABORTED` |
| `TimedOut { stdout, stderr }` | `DEADLINE_EXCEEDED`, with an `action_result` holding the two log digests the driver stored (the partial result), unless it stored neither |
| `Invalid(why)` | `INVALID_ARGUMENT`: the client's error, never retried |
| `MissingBlob(digest)` | `FAILED_PRECONDITION` with a `MISSING` violation |
| `Failed(why)` | `INTERNAL`: the farm's failure |
| `Lost(why, message)` | `UNAVAILABLE` with `Result.lost` (daemon restarted, or fenced): the farm lost the run, which the server runs again |

Five runtimes exist. The `kbf-daemon` binary (crate `kbf-node`) offers all but
`LocalRuntime`, picked by `--driver`, which may be given more than once: one runtime per
driver, each driver once, the node's own (native or container) set up first, then the
others in the order given. Two that serve the same kind refuse to start (`container`,
`native` and `fake` all serve `action`); a Mac runs `--driver native --driver
native-whole-machine`.

- **`PodmanRuntime`** (`kbf-driver-container`, driver `container`, Linux only): the
  runtime for Linux farm nodes, below. The binary requires `--data-dir` and `--cas`, and
  checks the daemon user's subordinate ids before it starts. It then sets up the cgroup
  its systemd unit delegates (`Delegate=yes`): it moves its processes into a
  `supervisor/` leaf, writes its `memory.min` from `--supervisor-memory-min-mib`
  (default 256; it warns when its unit or slice protects less), makes `actions/` with
  cpu, memory and pids enabled, and writes `actions/memory.max` from
  `--actions-memory-max-gib`. It refuses to start, naming the
  fix, on cgroup v1, from the root cgroup, or when the unit does not delegate those
  controllers or the cgroup is not writable. `--cgroup-parent` names an `actions/`
  cgroup set up by someone else instead (checked, not changed). The node reports at most
  what `actions/` may use: `mem_gib` is the lower of MemTotal and the lowest
  `memory.max` above the leases, `cpus` the lower of the nearest
  `cpuset.cpus.effective` and the lowest `cpu.max` quota on the path. The daemon writes
  `actions/cpu.max` itself: the CPUs it may use minus `--reserve-cpus` (default 8).
  `--lease-placement=numa-node` (default `none`, which pins nothing) puts each lease on
  one NUMA node: the lease cgroup gets that node's CPUs (`cpuset.cpus`) and memory node
  (`cpuset.mems`), and the container's cgroup inherits both. The node is the one with
  the least CPU booked by running leases, the lowest id on a tie; the lease's
  `inputs phases:` line ends with `numa_node=N`. A lease that asks for all the memory the
  node reports, or for more than any one node holds, is unpinned: it gets every CPU and
  memory node the daemon may use, books no node, and its line ends `numa_node=all`. It
  enables `cpuset` in `actions/` and
  its parent (the start fails, naming the fix, when the unit is not offered it) and
  runs as `none`, logged once, on a host with fewer than two NUMA nodes that hold CPUs.
  It changes neither admission nor `--reserve-cpus`, a quota that removes no CPU from a
  node.
  [docs/deploy/linux-build-host.md](../deploy/linux-build-host.md) has the systemd
  units and the host's swap and memory headroom.
- **`NativeRuntime`** (`kbf-driver-native`, driver `native`): each action as plain
  processes on the node, for Macs. The macOS end state (a root daemon, a reused pool of
  unprivileged lease users cleaned between leases, one long-lived GUI user for
  whole-machine leases) is in [macos-end-state.md](macos-end-state.md) (**planned**).
- **`WholeMachineRuntime`** (`kbf-driver-native`, driver `native-whole-machine`, macOS
  only): each `whole_machine` lease as a throwaway user in that user's GUI session,
  through the root helper `kbf-mac-session`
  ([fleet-updates.md](fleet-updates.md#101-no-gpu-in-vms)). The binary requires
  `--data-dir` (its files go in `<scratch>/whole-machine`) and `--cas`, and refuses to
  start when nothing serves `--mac-session-socket`. It hands the daemon its phase
  (`Daemon::with_phase`), which every `Hello` carries
  ([worker-protocol.md](worker-protocol.md#node-phases)): the session switch restarts
  the daemon, and the runtime takes the lease up again from its on-disk record when the
  server sends the `Start` again. `--daemon-user` names the daemon's own user, which the
  lease directory's access control entries keep able to read and remove what the lease
  user writes.
- **`FakeRuntime`** (driver `fake`): runs nothing and returns an empty result, for
  bring-up. It serves `action`; `FakeRuntime::serving` makes one serve another kind
  under another driver name, so a daemon with several runtimes can be tested with fakes.
- **`LocalRuntime`** (driver `local`): **tests only**. Runs each action as a plain child
  process of the daemon so the whole path (fetch, write inputs, run, measure, upload,
  report) can be tested where no container runtime exists. It isolates nothing and
  applies no timeout. The end-to-end harness uses it.

## The container driver

**kbf runs containers with `crun` only.** Podman is not a runtime a node can be given:
measured on a node, Podman took about twenty seconds to launch a container and `crun`
milliseconds, so the node's runtime is the OCI backend (below), the default and the
only value of `--container-runtime`. The steps and tables in this section that name
Podman describe the path the code still holds for its own tests until it is deleted;
what a node runs is the OCI backend section, and the six steps are the same code for it.

`PodmanRuntime` (the name stays until the Podman code is deleted) runs each `action`
lease in a fresh rootless container that is never reused. Each lease goes through six
steps.

1. **Prepare.**
   - Fetch the `Action` and `Command` from the CAS and verify them.
   - Read the image from the `container-image` platform property (on the `Action`, or
     on the `Command` for clients older than REAPI 2.2). It must be
     `docker://<repo>@sha256:<digest>`: a tag is refused, and so is a digest that names
     an image index (a multi-architecture list) rather than one architecture's manifest.
     The driver tells the two apart by fetching the manifest from the image's registry
     by that digest: its bytes must hash to the digest, and an index is refused. The
     image is unpacked in the node's image store once per digest, on the first lease
     that names it or at start for a pinned image (below), and every byte of it was
     checked against a digest on the way in.
   - Check the `Command`: it must have arguments; the working directory and every output
     path must be relative, without `.`, `..` or empty components.
   - Write the input root into the lease's scratch directory. Every name in a client's
     `Directory` messages is checked (no slash, `.`, `..` or NUL; no name twice) and
     files are created exclusively in directories the driver made, so no write follows
     a symlink the input tree planted.
   - Refuse an action whose working directory is hidden by an input file or symlink of
     the same name, as `INVALID_ARGUMENT`.
   - Move each entry of the input root that an output path names, whole, into the
     overlay's upper directory, then make the working directory and each output's
     parent there (see Output collection). A parent made through a symlink is refused
     as `INVALID_ARGUMENT`.
   - Give the overlay's directories to the container's root (see User namespaces).
   - Make the lease cgroup (below).
2. **Start.** `podman create`, then `podman start --attach`, with stdout and stderr
   written to files in the scratch directory.
3. **Watch.** Wait for the container to exit, the action's timeout, or `kill`. On a
   timeout, once the container is stopped, store what it wrote to stdout and stderr in
   the CAS, each within the stdio limit; a log past the limit, or one that cannot be
   stored, is left out, and the lease still ends `TimedOut`.
4. **Collect.** Read the exit code from Podman's record of the container. If it is 137
   (SIGKILL), or there is none, and the lease cgroup's `memory.events` counts an
   `oom_kill`, the lease fails as out of memory or as killed on a busy node (see
   Cgroups and limits). Otherwise give the overlay's directories back to the daemon's
   user, read the outputs (below) and store stdout and stderr in the CAS.
5. **Clean.** Remove the container, the lease cgroup and the scratch directory.
6. **Verify clean.** Neither directory may remain.

**Pinned images.** `kbf-daemon --driver container --image docker://<repo>@sha256:<d>`
(repeatable) names images the node is set up to run. At start, after its cgroup is
set up and before it connects, the daemon checks each as the prepare step checks a
lease's image (`PodmanRuntime::check_pinned`): named by digest, fetched into the node's
image store if it is not there (from its registry, every byte checked against a digest),
and one architecture's manifest. The first that cannot be stops the daemon with exit
status 2 and the reason: a registry that cannot be reached or refuses, a blob that is not
its digest, or an index digest (name the per-architecture manifest digest). The images are listed, sorted and each once, in the node's `NodeStatus`
(`container_images`), which `GET /v1/nodes` shows as `software.container_images`. They
route no work: placement does not match an action's image against them, and an action
may still name an image the node was not pinned to (it is checked at prepare as
always). Another driver given `--image` refuses to start (exit status 1).

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
| Users | `--userns=nomap`: no container uid or gid is the daemon's user (below); `--user=0:0`, the container's root, whatever the image's `USER` |
| Hostname | `localhost` |
| Environment | `--unsetenv-all`, then the `Command`'s variables with `--env`: nothing from the image's `ENV`, Podman's defaults (`PATH`, `container`; `TERM` only with a tty, which an action never has) or the node's `containers.conf`. Podman 4.9 still adds `HOSTNAME=localhost` and `HOME` (uid 0's home in the image's `/etc/passwd`) when the `Command` sets neither; both follow from the image digest |
| Limits | `--pids-limit`, `--shm-size` and `--ulimit` for `nofile` and `nproc` (soft = hard), from `kbf-daemon`'s `--container-pids-limit` (default 8192), `--container-shm-mib` (64), `--container-nofile` (65,536) and `--container-nproc` (32,768). `nproc` counts per user, and every container's root is the same subordinate id, so it bounds the node's actions together |
| Working directory | the `Command`'s, under `/kbf/root` |
| Files | the input root as a read-only overlay lower layer at `/kbf/root`; every write lands in a per-lease upper directory |
| Cgroup | `--cgroup-parent` set to the lease cgroup, `--cgroup-manager=cgroupfs`, `memory.oom.group=1` |
| Timeout | the `Action`'s timeout; one hour when it names none; a negative one is `INVALID_ARGUMENT` |

**Stopping a container** (timeout or kill): SIGTERM, then 5 seconds, then `cgroup.kill`
on the lease cgroup, then a forced removal in the clean step.

### The OCI backend (`--container-runtime=oci`)

`kbf-daemon --container-runtime=oci [--oci-runtime=/usr/bin/crun]` (the default, and the
only value) runs each action in a fresh rootless sandbox made by `crun` directly, with no
Podman anywhere: not on the action path, not for images, not in the deploy kit, not in
host setup. The six driver steps, the lease cgroup, the watch, the OOM classification,
output collection and the clean are the same code (`backend.rs` is the seam, and holds
the Podman path's last code until it is deleted). `crun` is driven through the `runc`
crate (containerd/rust-extensions, Apache-2.0) and `config.json` is built with
`oci-spec`'s typed builders.

Per action: the image is already unpacked (below); a helper container gives the lease's
directories to the action's root; `config.json`, the `hosts` and `hostname` files are
written; `crun run` runs the bundle in the foreground; a helper gives the directories
back; `crun delete --force` closes the lease. That is four `crun` processes where the
Podman path runs nine (`podman image inspect`, `create`, `start`, `inspect`, the
`unshare` chowns, `rm`, with a cache more). No `podman` process, `conmon` or Podman
state database is touched.

- **The exit code is `crun run`'s own status** (`128 + n` for a signal), so there is no
  moment at which the container is stopped but its exit code is not recorded (#658). A
  non-zero status is the farm's failure rather than the action's only if `crun` wrote an
  error record to its `--log` (JSON) file; the lookup failures of `program.rs` (exit 127
  and 126) and `ETXTBSY` are read from that log in the same words as on the Podman path.
- **Namespaces:** pid, network (new, nothing configured: loopback only; `crun` brings
  `lo` up), ipc, uts, mount, cgroup and user. Container ids `0..65536` map to the first
  range of at least 65,536 in the daemon user's `/etc/subuid` and `/etc/subgid`; the
  daemon user itself is unmapped, as with `--userns=nomap`. `crun` runs `newuidmap` and
  `newgidmap` for the mapping, so `uidmap` is a node requirement.
- **Capabilities, masked and read-only paths, seccomp** are what Podman 4.9 supplies,
  written out (`oci/hardening.rs`): the eleven `default_capabilities` of
  containers/common v0.57.4 (`CHOWN DAC_OVERRIDE FOWNER FSETID KILL NET_BIND_SERVICE
  SETFCAP SETGID SETPCAP SETUID SYS_CHROOT`; inheritable and ambient empty), its twelve
  `DefaultMaskedPaths` and six `DefaultReadOnlyPaths`, and that release's
  `seccomp.json`, vendored beside the code with its sha256 pinned in a test and resolved
  for the capability set and the architecture (`includes`/`excludes` of `caps` and
  `arches`) exactly as containers/common resolves it. `noNewPrivileges` is false, as
  Podman's default. Left out: Podman's `net.ipv4.ping_group_range` sysctl and the `gid=5`
  option of `/dev/pts`.
- **Cgroups:** the container's cgroup is `<lease cgroup>/ctr`, made by `crun`
  (`cgroupfs` manager, its default) in the daemon's delegated subtree with
  `pids.max` and `memory.oom.group=1`; memory and CPU stay on the lease cgroup, so
  `memory.events` is read as before.
- **Files.** `/` is the unpacked image, **read-only**; `/kbf/root` is an overlay
  (`lowerdir` the input root, `upperdir` the lease's upper, options pinned to
  `userxattr,metacopy=off,redirect_dir=nofollow,index=off,xino=off`); `/tmp`, `/var/tmp`,
  `/run` and `/root` are directories of the lease on disk (not tmpfs, which would count
  against the lease's memory); `/etc/hosts` and `/etc/hostname` are small read-only
  files. **This differs from Podman, whose container root is writable everywhere**: an
  action that writes to another place in the image fails with `EROFS`. Shared read-only
  trees (`SharedMount`: a host directory and a destination below `/kbf/root`) are bound
  read-only after the overlay; the tree store that fills them is not part of this
  backend.
- **Helper containers** (`oci/helper.rs`) do the work that needs a user namespace with
  the daemon user as id 0 and the range as ids `1..` (so the action's root is `1:1`), or,
  for an image's unpack, an action's own mapping. A helper runs one program, never a
  shell: this daemon's binary in its `__chown-dirs` mode (with `--whole` trees, so the
  whole hand-over is one process and walks by descriptor, never following a symlink) or
  its `__unpack-layers` mode, or the node's `find` (to empty a directory the container's
  ids own), from a read-only bind of the host's `/usr`. `crun` creates the namespaces, so the only
  AppArmor grant for user namespaces is `crun`'s shipped profile, and the daemon needs
  none of its own. A helper's cgroup is `<actions parent>/<id>`, named in its config:
  left to itself `crun` makes one beside the daemon's own, and kills it on removal.
- **The image store** (`oci/image.rs`, and the `kbf-image` crate): `<scratch>/oci-images/<hex>/rootfs`,
  keyed by the manifest digest alone and unpacked once per digest, off the action path.
  The daemon fetches the image itself, anonymously, over https (hyper over rustls, the
  host's roots; a `Bearer` challenge is answered with a pull token, and a blob's
  redirect target gets no credentials): the manifest by the pinned digest, which its
  bytes must hash to, and which must be one image, not an index (a Docker schema 1
  manifest, a foreign layer and a zstd layer are refused too); then the config and every
  layer blob, each hashed as it is written and compared with the manifest's digest and
  size before use (a failed one is deleted), and the image's own platform, when its
  config states one, must be the node's. Limits bound a wrong pin: 4 MiB of manifest, 128
  layers, 16 GiB of blobs. One helper container then applies the layers in order with
  `kbf-daemon __unpack-layers`, in an action's user namespace, so every file is owned by
  the host id the action sees as the image's own id: `.wh.` whiteouts and `.wh..wh..opq`
  opaque markers apply to lower layers only, a path is resolved inside the root and never
  outside it (`..` in an entry is refused, a symlink is followed as a chroot would), the
  owner is set before the mode (`chown` clears setuid), an id above 65,535 is an error,
  and device nodes and fifos are skipped. One flight per digest; the blobs and the stage
  are removed whatever happens. After that a lease costs one `stat`. Tar's extended
  attributes are not extracted (file capabilities are lost). A node needs egress to the
  registries of its images (`registry-1.docker.io` for `docker.io`, `gcr.io`, ...).
- **Node requirements:** `crun`; `newuidmap` and `newgidmap` (package `uidmap`) on the
  daemon's `PATH`, setuid root or holding file capabilities; one subordinate range of
  65,536 for the daemon's user; every directory from `/` to `--data-dir` searchable by
  others (the container's ids are not the daemon user's: `chmod o+x`); kernel overlayfs
  in a user namespace (5.11 or later); cgroup v2 delegated to the daemon's unit
  (`Delegate=yes`); and whatever permits unprivileged user namespaces for `crun` (its
  shipped AppArmor profile on Ubuntu 24.04). The daemon checks the range, the paths, the
  files and the two id tools at start and refuses to start naming what is wrong.
  Lingering and `XDG_RUNTIME_DIR` are not needed: `crun` always runs with an explicit
  `--root` inside the lease.
- **Per-action overhead.** The stand-in `crun` test (`fake_oci.rs`) times the lifecycle
  and counts the processes (four, against the Podman path's more); what a real node adds
  is to be measured on a build host: the latency of a helper container (user namespace,
  `newuidmap`, bind mounts) idle and at 50 to 80 concurrent leases; `crun run` of a
  trivial action; the overlay mount and the network namespace teardown at concurrency;
  the one-time unpack; and the kernel's per-user key quota (`kernel.keys.maxkeys`, 200 by
  default), which each container's session keyring counts against.

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
- **With the input cache** (`--cache-dir`, [worker-cache.md](worker-cache.md) section
  5.1), the input root's files are hard links to read-only cache entries the daemon's
  user owns, which the container's root must never own. The driver then gives the
  input root's directories alone to id 1 (and back), with the daemon binary's own
  `__chown-dirs` mode run under `podman unshare`, and `chown -hR` covers only the upper
  and work directories. An input an output names is a private copy, never a link. An
  action that opens a cached input for writing in place gets `EACCES`, and one that
  renames or hard-links one gets `EOVERFLOW` (overlayfs will not copy up a file whose
  owner the container does not map).
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

- **Memory** follows the build-host policy. Each lease has a hard cap, `memory.max` =
  booked memory x 1.5 + 512 MiB (the native driver's limit), with
  `memory.oom.group=1` on the lease cgroup (and on the container), so an OOM kill takes
  every process in the lease and none outside it. The lease cgroup sets no
  `memory.high` and no `memory.swap.max`: swap stays allowed, so reclaim under host
  pressure moves a lease's pages to swap instead of killing it. The same holds at the
  cap: while swap is free, a lease at its cap is reclaimed into swap, and the kernel
  kills it there only when reclaim cannot free enough (swap full, or none). So the
  driver watches each lease (`SwapKill`): every `--lease-swap-poll-ms` (default 1000)
  it reads the lease cgroup's `memory.events` `max` and `memory.swap.current`, and
  splits the lease's swap into what the lease pushed there itself at its cap and the
  rest. A rise in swap between two samples across which `max` grew is the lease's own
  (reclaim at its cap moved it); a rise across which `max` did not grow is not (host
  pressure, or the `actions/` limit, moved it while the lease was below its cap: a
  limit above the lease counts no `max` event in it); a fall (pages read back, a
  process exiting) comes out of the lease's own share first. The split runs over the
  lease's whole life: a pause in its `max` events does not reset it. The driver kills
  the lease (`cgroup.kill`, every process in it) when its own share is above the
  larger of `--lease-swap-kill-mib` (default 512) and `--lease-swap-kill-percent`
  (default 25) of its booking. The share grows only at a sample where `max` grew and
  swap rose, so the kill comes only while the lease is pressing its cap into swap.
  The floor spares a small action that brushes its cap for a few cold pages; the
  share scales the margin for large bookings. The previous sample is the last one the
  driver could read: a sample it cannot read is skipped and changes nothing, and the
  first is compared with zeros (a new lease cgroup's counts). So swap that host
  pressure moves while a lease is below its cap is never counted against it, even
  when it later reaches its cap and pushes a few pages more (only those count). Swap
  that host pressure moves in a sample in which the lease's `max` events also grew
  counts as the lease's own: the two counters cannot tell them apart, so a lease that
  sits at its cap (page cache counts toward `memory.max`) can be killed for it.
  Nor is one that booked no memory (no cap).
  The node's backstop is `memory.max` on `actions/`, set from
  `--actions-memory-max-gib` (or by whoever runs the daemon's unit). Nothing is capped
  when no memory was booked.
- **The memory backstop** (every driver, Linux): while the node's memory stall time is
  high and it runs a lease, the daemon stops taking new ones. It samples the `some`
  line's `avg10` of `/proc/pressure/memory` (the percentage of the last 10 seconds in
  which at least one task waited for memory) every second. When that is above
  `--memory-pressure-pause-percent` (default 10) and a lease runs, the daemon's
  `Hello` says `PHASE_MEMORY_PRESSURE` ([worker-protocol.md](worker-protocol.md#node-phases))
  and the server offers the node nothing; when the stall is at or below the threshold,
  or the last lease ends, the next `Hello` says serving. Leases that run are not
  touched. It cannot deadlock: it pauses only while a lease runs, and every lease
  ends, so a node whose stall comes from outside (another process on the box) still
  takes one lease at a time. Where the file cannot be read (macOS, a kernel without
  pressure stall information) the daemon logs that the backstop is off and never
  pauses. A runtime's own phase (a session switch, a quarantine) comes first. It does
  not replace the `actions/` limit: that kills, this only stops adding load.
- **CPU** is compressible: `cpu.weight` = booked millicores / 10, clamped to 1..10000
  (one core is the kernel's default weight of 100). The driver never sets `cpu.max`,
  because throttling distorts wall time.
- **Memory kills** are reported in `Result.memory_kill`:
  - The swap watch's kill: `RESOURCE_EXHAUSTED` with `MEMORY_KILL_OWN_LIMIT`
    (`RuntimeError::OutOfMemory`, with what the lease held in RAM and swap and its
    cap).
  - A kernel OOM kill is read from the lease cgroup's `memory.events`, which outlives
    the container, when the action ends with SIGKILL (137), when Podman recorded no
    exit, or when Podman could not be asked: the counters do not depend on Podman's
    record. Podman's own OOM flag is not trusted (it reads false when rootless). The
    kernel counts an event at the cgroup whose limit caused it and at that cgroup's
    ancestors, so the lease's counters say whose limit it was:
    - `oom_kill` with `oom` and `max` in the lease: the OOM killer ran for the lease's
      own cap (swap was full). `RESOURCE_EXHAUSTED` with `MEMORY_KILL_OWN_LIMIT`
      (`RuntimeError::OutOfMemory`, with `memory.peak` and the cap).
    - `oom_kill` without `oom` in the lease: the OOM killer ran for a limit above it
      (`actions/`'s backstop, or the host). `UNAVAILABLE` with
      `MEMORY_KILL_NODE_PRESSURE` (`RuntimeError::BusyNode`); its message says whether
      `oom` in `actions/memory.events.local` grew during the lease. The booking was not
      the cause.
    - No `oom_kill`: an exit 137 is the action's own result.
- **Removal** of a lease cgroup that is still busy (a process still exiting) writes
  `cgroup.kill` and retries.

### End to end (the M2 test)

The `m2-container` CI job (`kbf-cell m2`, `crates/kbf-it/src/m2.rs`) runs the driver
behind the real server and a real client, on a hosted x86_64 runner.
It pulls distroless `base-debian12` by its linux/amd64 manifest digest (the sample's
`crates/kbf-it/m2/buck2/.buckconfig` names the same image as `container_image`, which
its execution platform passes on as `container-image`), starts one `kbf-server` and one
`kbf-daemon --driver container --image <the sample's image>` as a `Delegate=yes` system
service with `--actions-memory-max-gib 8` (the daemon fetches and unpacks the image before
it says `Hello`; no tool pulls it ahead), checks the cgroup layout the daemon made in its unit
(`supervisor/` holds the daemon; the unit's cgroup and `actions/` enable `cpu memory
pids`), and builds the sample project with pinned buck2:

- every target remote-only, then again after `buck2 clean`, every action answered from
  the action cache (the rule `kbf-cell check` applies, as in M1). The image has no
  shell, so each action runs `kbf-m2-act`, a statically linked helper that is an input
  file of the action;
- `//:hog`, which touches 3 GiB slowly: its first run books 1 GiB (the cell's server runs `--memory-ladder-gib 1,2,4`; the
  default first rung, 4 GiB, would pass the hog's cap), presses
  its 2 GiB cap into swap until the swap watch kills it, and the server's log must
  hold exactly one requeue from 1 GiB to 2 GiB booked, after which it passes;
- `//:pause` under a fresh salt: while it sleeps, exactly one `crun` container exists
  (one state directory under `<lease>/crun/` of the daemon's leases) and a `kbf-lease-*`
  cgroup is under `actions/`; afterwards no container and no lease directory is left;
- `//:pause` again, with the daemon killed mid-lease and a new one started: the server
  reruns the lease within 10 s of the new daemon's welcome, and the build succeeds on
  the farm.

## Output collection

Outputs are read from the overlay's upper directory, which holds only what the action
created or changed. An output read from there must be whole there, so an output path
that names an entry of the input root (REAPI allows it; Bazel and Buck2 never send it)
gets that entry moved from the lower to the upper directory before the overlay is
mounted. The action sees the same tree; what it changes is changed in place, what it
deletes is unlinked (no lower entry is left to hide, so no whiteout), and the output
is read back as the action left it: an unchanged input whole, an edited one with its
new bytes, a deleted one absent. That is what the native driver collects from its
input root. Outputs are taken ancestors first, so one below another moves with it. The
walk to an output follows no symlink and stops at the first component that is absent
or not a directory: the parent made for the output in the upper directory hides a lower
file or symlink of that name instead of merging with it.

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

## Where a lease's time went

When a lease's run ends the daemon logs one INFO line, `lease phases:` and then
`key=value` fields, and sends the same fields to an observer as `Event::LeaseEnded`
(`kbf_daemon::phases`):

| Field | What it is |
|---|---|
| `lease`, `driver`, `ok` | the lease, the driver it ran on, and whether the run produced a result (an action that exits non-zero does) |
| `lease_ms` | from the run's start in the daemon to its end |
| `inputs_ms`, `run_ms`, `outputs_ms` | the driver's own stamps in the result's `ExecutedActionMetadata`: input fetch, execution, output upload; `-` when the run produced no result. What `lease_ms` holds beyond their sum is the driver's work outside them (making and removing the lease's directories, the container's create and chown) |
| `output_files`, `output_dirs` | the files and directories the result names at its top level (the files inside an output directory are in the `transfers:` line's `written_blobs`) |

The fields after `output_dirs` are what the lease's own cgroup (v2) counted, read
when the action ended; each is `-` where it was not measured (a driver without a lease
cgroup, a kernel without the file: absent is not 0). They are measurement only; nothing
in scheduling reads them.

| Field | What it is |
|---|---|
| `cpu_usage_us`, `cpu_user_us`, `cpu_system_us` | `cpu.stat` `usage_usec`, `user_usec`, `system_usec`: CPU time of every process in the cgroup |
| `cpu_periods`, `cpu_throttled`, `throttled_us` | `cpu.stat` `nr_periods`, `nr_throttled`, `throttled_usec`: all 0 while the lease has no `cpu.max` quota, which kbf does not set |
| `mem_peak` | `memory.peak`, bytes |
| `parallelism` | `cpu_usage_us` over the run's wall time (the container's create to its exit), two decimals: how many CPUs the action kept busy on average |
| `numa_bytes` | `node:bytes` pairs, comma-separated: `memory.numa_stat` `anon` plus `file` per NUMA node, of the sample that held the most memory (sampled every second while the action runs, `PodmanConfig::numa_every`, and once at its end: the kernel uncharges anonymous memory when the processes exit, so the end alone shows little) |
| `numa_spread` | the share of that memory not on the node holding most of it, two decimals (0.00 is all on one node) |

The same counters ride in the result, in `ExecutedActionMetadata.auxiliary_metadata`
as a `kbf.worker.v1.ResourceUsage` whose optional `cgroup` field (`CgroupUsage`) is new;
a daemon or server that predates it ignores the field.

A lease that ends without a result (timed out, killed, out of memory) has the same
fields, read from its cgroup before the cgroup is removed (`Runtime::failed_usage`;
the container driver does it), and carries the `ResourceUsage` in the metadata of the
`action_result` of its `worker.Result`: the peak memory of an OOM-killed action and the
CPU of a timed-out one are what the memory ladder needs. A lease killed for memory, its
own limit or the node's, ends the line with ` oom=1` (no such key otherwise).

At start the daemon logs the host's NUMA layout once (Linux), from
`/sys/devices/system/node`: `numa layout: nodes=2 cpus=64 node0=0-15,32-47
node1=16-31,48-63`. `numa_bytes` node numbers are those nodes.

`outputs_ms` includes waiting for the transfer engine's request limit, which every
lease of the process shares; there is no per-lease busy time.

What the lease moved is the transfer engine's `transfers:` INFO line
([worker-cache.md](worker-cache.md#4-the-shared-transfer-engine)), which carries the
same `lease=` field: join the two lines on it. The engine logs it when the lease's
last handle on the engine goes, which is after the lease ends if a read it started is
still finishing for another lease. Its counts, per lease:

| Field | What it is |
|---|---|
| `read_calls`, `read_bytes` | ByteStream reads the lease started (a retry is a read) and the bytes they received, including bytes that arrive after the lease left while another lease still waits on the read |
| `merged_reads` | reads served by a fetch another caller had started; they add no bytes |
| `find_missing_digests`, `outputs_present` | digests the lease asked FindMissing about, and uploads skipped because the server already held the blob (no bytes sent) |
| `write_calls` | ByteStream writes, every attempt (a retried or failed one too) |
| `written_blobs`, `written_bytes` | blobs the lease's successful writes stored (output files, Directories, Trees, stdout, stderr), and the bytes they sent. Uploads are not shared: two leases writing the same new blob at once both send it, and both count it |
| `retries` | requests retried after UNAVAILABLE or a broken connection |

So the sum of `read_bytes` over the leases is what the server served, with nothing
counted twice. The server's side of the same uploads is in `GET /metrics`
([api.md](../api.md#get-metrics)).

**Upload deadline.** The whole upload (output files, stdout and stderr) must finish
within `--output-upload-deadline-secs` (default 1800, 30 minutes; container, native and
whole-machine drivers). Past it the upload is dropped, which cancels its calls to the
CAS, and the lease fails with INTERNAL and the reason `the output upload did not finish
within its deadline of ...: stopped after ...`. Without it, a store that stops
answering held the lease, and its slot, with no error.

**Hashing the outputs.** The container driver reads each output file once, to hash it:
SHA-256 for the digest, CRC-32C for the footer of the segment it is sent in. The hashing
runs on `--cpu-threads` threads of their own (default `max(2, reserve/2)`, 4 with the
default `--reserve-cpus` of 8; shared by all leases and apart
from the threads that run actions), one read buffer per thread, and the listing of the
output tree is one task of the blocking pool, not one per file. The upload then reads the
files again, in ranges, only to send them (`--segment-uploads-in-flight` uploads at once,
default 4, within `--transfer-in-flight`'s upload share): nothing is copied to scratch.
A file that is no longer the one that was hashed (replaced, removed, or its size or times
changed) fails the lease with a read error naming it. The lease's log line `outputs
phases` says where the time went: `list_ms`, `hash_ms`, `find_missing_ms`, `upload_ms`.

### The server's `action usage:` line

The server logs one INFO line, `action usage:`, per lease report it takes whose
`action_result` carries a `ResourceUsage`: an OK run, and a failed one the daemon
measured, whatever the outcome (for example an out-of-memory kill, each rung of the
memory ladder included, a timeout, an infrastructure failure, a lost run). It reads the usage before it judges the report, since judging
drops a killed run's result and cuts a timeout's to its logs
(`kbf_server::action_usage`). The fields, in this order:

| Field | What it is |
|---|---|
| `exit_code` | the exit code the daemon reported for a run it says ended (an OK status); `-` for one it reports failed (killed, timed out, lost: none). It is the run's, not the server's verdict: an OK run whose outputs the server cannot store has its exit code and `outcome=infra` |
| `mnemonic`, `target_hint` | the `RequestMetadata` the client sent on the Execute (`action_mnemonic`, `target_id`), quoted; `-` when it sent none (buck2 sends neither). A hint only, never a key |
| `wall_us` and the cgroup fields | the run's wall time and the same cgroup fields as the `lease phases:` line above |
| `key` | the action's shape key, 16 hex digits (below) |
| `cfg` | the configuration hash an output path named (`<16 hex>` or `<16 hex>-<16 hex>`); `-` for none (buck2's content-hash paths) |
| `target`, `target_kind` | the target the action's own `buck-out` paths name (issue #796): `"cell//pkg:name"` from its first output pattern that names one, else from its first program word that does (a test binary buck2 built), and the owner's kind (`target`, `anon` or `bxl`); `-` when they name none (a remotely run test under `buck-out/v2/test/execution/`, run by a program outside `buck-out`). Read from the Command, never from the client |
| `outcome` | how the scheduler judged the report: `ok`, `oom` (the action's own memory limit), `node-pressure`, `timeout`, `infra`, `invalid`, `missing-input`, `lost-restart`, `lost-fenced` |
| `lease`, `node` | the lease and the node that ran it: the join to the daemon's `lease phases:` and `transfers:` lines |

The fields keep their order; `target_hint` was called `target` before the derived
target took that name. A report the server refuses (its lease is no longer
current) has no line: that is how a **cancelled** lease ends. Its usage is in the
daemon's `lease phases:` line, which joins on `lease=` to the server's `lease granted`
line, and that line names the `key` too. A report without a `ResourceUsage` has no line.

**The shape key.** buck2 sends no per-action identity (`action_mnemonic` and
`target_id` are empty in its `RequestMetadata`), and the action digest changes with
every input, so the key is read from the `Command`: the program it runs and the paths it
writes, each made independent of the configuration and of the content hashes buck2 puts
in paths. One target's action keeps its key across builds and configurations (`cfg`
tells configurations apart); two actions of one target, or two targets, differ.

- Output paths: under `buck-out/<v>/<root>/` (`art`, `art-anon`, `art-bxl`, and the
  `gen` kinds of older buck2) the prefix up to the root goes, and with it the hashes
  buck2 puts where its owner's layout says. A target: the configuration hash after the
  cell, and the content hash or `output_artifacts` placeholder in the one segment right
  after its `__<name>__` (a hex directory further down is kept).
  `buck-out/v2/art/cell/<cfg>/pkg/__t__/t.o`,
  `buck-out/v2/art/cell/pkg/__t__/output_artifacts/t.o` and
  `buck-out/v2/gen/cell/pkg/__t__/<content>/t.o` are all `cell/pkg/__t__/t.o`; a
  dynamic action's `__action__<k>__` stays. A target name may hold a `/` (buck2 writes
  `//pkg:foo/bar` as `__foo/bar__`): a name runs from a segment that starts with `__` to
  the first later one that ends with `__`. An anonymous target: the configuration hash
  glued to its package, and the attributes or content hash right before its
  `__<name>__` (any segment of hex digits there, whatever its length), also where buck2 glues it to the cell (`art-anon/anon<16 hex>/__t__/o`
  is `anon/__t__/o`); in content mode buck2 glues the cell to the package, which stays
  glued (a stable key, but not the configuration-hash form's). A BXL function: the
  execution-platform hash after the cell (as `cfg`) and the arguments or content hash
  right after its `__<name>__`
  (`art-bxl/komira/<16 hex>/tools/build/lint/test_weld.bxl/__check__/<16 hex>/test_weld`
  is `komira/tools/build/lint/test_weld.bxl/__check__/test_weld`). buck2 writes the
  BXL hashes unpadded; 13 to 16 hex digits are read as a hash, so the 1 in 65 536 with
  fewer is kept. Scratch paths (`buck-out/<v>/tmp/`) are left out; any other path is
  kept as sent. The patterns are sorted and deduplicated (buck2 may send one path as
  both an output file and an output directory), and `cfg` is the first pattern's that
  names one. An action with no outputs has only its program in the key, so every such
  action of one program shares a key.
- **Known limit: remotely run tests.** buck2 writes a test's outputs under
  `buck-out/v2/test/execution/<cell>/<16 hex>/<16 hex>/…`; the first hash is of the
  whole target identifier, configuration included, the second of the test cases run.
  No segment holds the configuration on its own, so the path is kept as sent: such an
  action keys once per target, configuration and set of test cases (stable across
  builds), and its line says `cfg=-`.
- The program: argv\[0\] (normalized the same way under `buck-out`, its file name when
  it is another absolute path; an argument file's path, after its `@`, the same way),
  then, while the last word is a bare program name (no `/` and no `@`: a program looked up on `PATH`, such as
  `env`, a shell or an interpreter, which may only run the next argument), the next
  argument that is not a flag or a `NAME=value` assignment, up to three words. A `-c`,
  or a cluster of single-letter flags ending in `c` (`-euc`), ends it, kept as the last
  word, so a `-c` script never enters the key (a script after `-e`, as Perl or Node
  take one, does: `-e` is a shell's flag too, so it is skipped). Any such cluster ends
  it, a linker's `-lc` too (`cc -lc a.o` is `cc -lc`). A buck2 genrule is
  `env <shell> <cell>/…/genrule.sh`, a prelude Python tool
  `<interpreter> <cell>/…/<script>.py`, a tool built in the repository its `buck-out`
  pattern alone. A bare-name compiler is read the same way (`cc -c a.c` is `cc -c`,
  `cc -o a.o a.c` is `cc a.o a.c`, `clang @<argsfile> a.c` is `clang @<its pattern>`,
  `clang @<file name>` for an absolute one). The
  rule lists no program names.
- The key is the first 8 bytes of SHA-256 over the tag `kbf-shape-v1`, the program's
  words and the patterns, each length-prefixed. A change to any rule above changes the
  tag, so keys of two normalizers never mix. `RequestMetadata` never enters the key.

The first time a process sees a key it logs what the key stands for, once:
`action shape: key=… program="env bash …" outputs=<n> patterns="<first 8>…"`.

## Measuring usage

`usage::Child` spawns an action as the leader of its own process group and reaps it
with `wait4`, which returns the CPU time and peak resident memory of the process and
every descendant it waited for. These are packed into a `ResourceUsage` inside the
result (see [worker-protocol.md](worker-protocol.md#resourceusage)). Today the local
runtime reports usage; the container driver does not yet.

## Planned

- **Re-adopting leases across a daemon restart:** each lease runs as its own systemd
  unit, so work continues while the daemon restarts, and the daemon lists every
  re-adopted lease in its first heartbeat.
- **Pausing new leases** under memory pressure.
- **Image import:** images copied from farm storage into the node's store once, by
  digest, with layers verified; a node never pulls from a public registry at action
  time.
- **Prefetch and a local cache:** inputs fetched as soon as a lease is offered, and a hot
  set of toolchains and images kept on the node and reported to placement. The cache
  (a per-node NVMe file cache linked or cloned into each lease) and the shared blob
  transfer engine it fills through are designed in [worker-cache.md](worker-cache.md);
  its first stage, the container driver's cache (`kbf_daemon::cache`), is built.
- **Networked actions,** which get egress, are never cached and always self-fence.
- **Service containers** for tests that need several containers, declared up front so
  the result stays cacheable.
- **Other drivers** behind the same trait: virtual machines (`vm`) later. Every driver must pass one freshness test: a
  marker left by one lease is never seen by the next.
- **Usage from the container driver,** read from the lease cgroup.
