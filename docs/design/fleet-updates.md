# Fleet updates: keeping every node's software current

This document says how kbf keeps the software on its worker machines up to date: the
operating system, Apple's developer tools, VM images and kbf's own daemon. It also
says how a test that installs a desktop app and uses the GPU runs on a bare-metal Mac
without leaking into the next test, what device management a Mac needs and what plays
that role on Linux, and what an operator sees and clicks in the farm's UI.

Everything here is **planned**. Nothing in this document exists in the code today
except where a section says "today". It builds on
[capabilities.md](capabilities.md), [worker-protocol.md](worker-protocol.md) and
[scheduler.md](scheduler.md), and on two designs in review:
`mac-node-provisioning.md` (open PR #76: the Mac provisioning profile) and
`macos-vms.md` (open PR #85: VMs, bare-metal builds and the GPU on Mac nodes). Key
names shared with #85 (`vm.image`, `vm.slots`, `kbf-book-cpus`, ...) are defined there
and only used here. Merge order: #76, then #85, then this document, so the links to
`macos-vms.md` resolve.

**This supersedes three points of #76:** its scheduled release check outside the farm
(#76 section 6.1; here the server computes "update available", 3.2), its lean against
MDM (#76 section 6.3 and decision 2; here MDM is recommended, 7), and `softwareupdate`
with a stored password (#76 section 6.2; here a fallback, 7.3). #76's profile and
script stay; the profile is what `kbf-updater` applies (5).

Claims about other projects and Apple's software are marked **[V]** (read in the
linked source) or **[A]** (an assumption, or a number nobody has measured on the
reference deployment's hardware). Section 14 collects them.

## 1. Summary

| Topic | Design |
|---|---|
| Principle | Once a daemon completes its handshake, `kbf-server` orders every change to that node's software. Updates are rolling: the farm never goes offline as a whole, and a platform never drops below its floor. |
| State | Each node reports what it runs (observed). Each pool has a signed *software set* (desired). "Update available" is computed by the server, never by the node. |
| Rollout | cordon, drain, apply, reboot, re-handshake, qualify, uncordon; one node at a time per pool by default; a canary and a soak first; any failed gate halts the rollout and alerts. The rollout record is durable before any node is touched. |
| Who applies | `kbf-updater`, a small root helper with fixed verbs that installs only manifests signed by the operator's CI for its own pool and platform. On Macs a second root helper, `kbf-mac-session`, owns per-lease users. `kbf-daemon` stays unprivileged, and no action can reach either helper. macOS itself is updated through MDM, because Apple leaves no supported unattended path on macOS 27 that does not keep a volume owner's password on the node. |
| Mac | Apple Business Manager plus a self-hosted open-source MDM (NanoHUB), behind a narrow gate (`kbf-mdm-gate`) so that `kbf-server` cannot erase every Mac. Xcode and simulator runtimes are installed side by side and selected per action. |
| Linux | No Apple-style MDM protocol exists for Linux; root plus `kbf-updater` plays that role. v1: the distribution's packages pinned to a dated archive snapshot. Target: an image-based OS (bootc) with automatic rollback. |
| GPU | No GPU in VMs. A GPU test, including a desktop-app plus local-LLM test, is a `kbf-lease=whole_machine` lease with `gpu=1` that empties the bare-metal Mac, one at a time. |
| App isolation | A throwaway non-admin user per lease, a leak scan against a baseline, a reboot on doubt, and a remote erase and automatic re-enrollment when a leak persists. |
| UI | A Fleet page: per worker OS, build, kernel or Xcode, update available, state. **Update** per worker; **Update all Linux workers** and **Update all Mac workers** start rolling updates. |
| Hands | Still needed for: enrolling Macs bought outside Apple Business Manager (once each), yearly certificate renewals, one Xcode download per release, Screen Recording grants, a Mac that will not boot, and hardware. |

## 2. The principle

A node joins by opening the worker stream and completing the handshake
([worker-protocol.md](worker-protocol.md)). From then on:

- **The server decides when the node changes.** The node never updates itself. Every
  automatic update mechanism of the OS is turned off, so a node only changes when a
  rollout tells it to (Apple's Background Security Improvements are the exception,
  section 7.2).
- **The server is the right place.** It already knows what runs where, so it can drain
  a node without losing work, keep enough nodes of each platform serving, and check
  the node afterwards with real work. A per-node cron job can do none of that.
- **Rolling, never everything.** A rollout takes nodes out one at a time (or a small,
  bounded number), and stops at the first failure. A bad update costs one node.
- **Signed, not clicked into existence.** The UI chooses *when* and *where*. *What* is
  installed is a software set that the operator's CI built and signed from a reviewed
  change.

**What a compromised `kbf-server` can do,** stated plainly:

- On a node, through `kbf-updater`: install a set CI signed **for that node's pool and
  platform** with a serial higher than the installed one (section 5.2). It cannot
  downgrade a node, cross pools, or run a command of its choosing.
- Through `kbf-mdm-gate` (section 7.6): enforce a macOS build named in a signed set
  for that Mac's pool, and erase **one Mac at a time**, within a daily cap and never
  below the Mac floor. Without the gate, the server would hold the MDM's API, which
  can `EraseDevice` every Mac at once and force any macOS build Apple still signs; that
  is why the gate exists.
- Deny service: cordon, drain or hold every node. That costs availability, not
  integrity, and every write is audited.

What it cannot be defended against here: whoever holds root on the MDM's host, or the
MDM's API key, can erase every Mac and push any profile. That host is the Mac fleet's
root of trust, like the Apple Business Manager administrator account, and is kept off
`kbf-server`'s machines and credentials.

## 3. Node software state

### 3.1 Observed

What a node runs, reported by `kbf-daemon`. Two kinds:

**Matchable keys** go into the node report ([capabilities.md](capabilities.md)), so
actions can route on them. Today every daemon reports `arch`, `os`, `cpus`, `mem_gib`,
`page_size`, `gpu`, `isa_level`, `cpu.features` and `drivers`, and the macOS detector
adds `cpu.model` (`crates/kbf-daemon/src/report.rs`). The matcher already compares
`os_image` and `xcode` exactly, but no daemon reports them. Every new key below is
refused today as unknown, and a second `xcode` as repeated
(`crates/kbf-caps/src/matching.rs`, `crates/kbf-caps/src/report.rs`), so each needs
a `kbf-caps` change.

The new comparison is **membership**: the node reports a set (the key repeated, one
value per entry), the request names one value, and the node matches when its set
contains it. It is the comparison #85 defines for `vm.image`.

| Key | Platform | Source | Comparison | Defined by |
|---|---|---|---|---|
| `os_version` | both | `sw_vers`; `/etc/os-release` | exact | this document |
| `os_build` | Mac | `sw_vers -buildVersion` | exact | this document |
| `kernel` | Linux | `uname -r` | exact | this document |
| `os_image` | Linux on bootc | the booted image digest | exact (exists) | capabilities.md |
| `xcode` (set) | Mac | every installed Xcode build | membership (changed from exact) | #85 phase 1 |
| `sim_runtime` (set) | Mac | installed simulator runtime builds | membership | this document |
| `vm.image` (set) | Mac | golden VM images on disk | membership on the digest only | #85 section 5.1 |
| `vm.slots`, `vm.max_cpus`, `vm.max_mem_gib` | Mac | the VM driver | as #85 section 5.2 | #85 |
| `drivers` gains `vm` | Mac | the VM driver's boot check | not a request key: placement maps the lease kind to it (exists, repeated) | #85 section 5.2 |
| `probe.<name>` | both | a command named in the node's provisioned config or a signed set (section 6.1) | exact | this document |

`probe.<name>` follows the case rules of `label.<k>`
([platform-properties.md](../platform-properties.md)): `probe.` is read in any case;
the probe's own name and its value are compared exactly.

**Reserved keys** (skipped by the matcher, names read in any case):

| Key | Meaning | Defined by |
|---|---|---|
| `kbf-book-cpus`, `kbf-book-mem-gib` | what a lease books | #85 section 5.1 |
| `kbf-node` | pin a lease to one node, for qualification (section 4.2). The front refuses it from every client with `INVALID_ARGUMENT`: no REAPI caller can send it. The rollout driver submits qualification work to the scheduler directly, as a new internal submitter, so no client role is needed for it. | this document |

**Re-detection is planned.** Today detection runs once, at daemon start
(`crates/kbf-node/src/main.rs`), and `Hello` is sent only when a session opens
(`crates/kbf-daemon/src/daemon.rs`). Noticing a changed report by its hash is listed as
planned in [capabilities.md](capabilities.md#planned). This design adds:

- The daemon re-detects the software keys when `kbf-updater` finishes a step and every
  10 minutes (to catch Background Security Improvements and hand changes).
- A changed report goes mid-session in a new `DaemonMessage::Report`; the session and
  its leases continue, and only new placements see the change. The heartbeat's report
  hash lets the server notice a report it has not seen.
- A step that reboots needs none of this: the `Hello` at session open is its done
  signal (4.2).

**Status fields** do not route work, so they go in a new `DaemonMessage::NodeStatus`
kept out of the report hash: the `kbf-daemon` commit, the updater's version and state
(including an in-progress step), the software set it last applied,
`reboot_required`, last boot time, the hardware serial and platform UUID (to join with
the MDM's record), and, where readable, firmware versions.

### 3.2 Desired: the software set

A *software set* is a manifest for one pool. It names, each with a SHA-256 or digest:

- Mac: the macOS version and build; the Xcode builds and their `.xip` digests; the
  simulator runtimes and Metal toolchain components; the provisioning profile version;
  the VM images; the probe commands, if any.
- Linux: the archive snapshot and expected kernel package, or the host image digest;
  the probe commands, if any.
- Both: the `kbf-daemon`, `kbf-updater` and (Mac) `kbf-mac-session` artifacts.

It also carries:

- `pool` and `platform` (`os`, `arch`). A node's `kbf-updater` is provisioned with its
  pool and platform and refuses any other set, so a Mac set can never reach a Linux
  node, nor an x86-64 set an arm64 node.
- A `serial`, monotonic per pool. There is no rollback flag: **a rollback is a new set
  with a higher serial that names the old artifacts**, signed like any other. A
  replayed old set is always refused.
- An ed25519 signature made by the operator's CI from a reviewed change.

The server keeps the set's digest in the rollout record (section 4.1).

**Update available** is computed by the server:

- the node's observed state differs from its pool's current set; or
- a newer upstream release exists than the pool's set. Upstream means Apple's public
  software catalogue for macOS ([gdmf.apple.com/v2/pmv](https://gdmf.apple.com/v2/pmv),
  the source Apple names for device-management services
  ([schema](https://github.com/apple/device-management/blob/release/declarative/declarations/configurations/softwareupdate.enforcement.specific.yaml)))
  **[V]**, polled at most once a day as that schema asks, Apple's developer releases
  feed for Xcode
  ([releases.rss](https://developer.apple.com/news/releases/rss/releases.rss)) **[V]**,
  and the operator's image pipeline or archive snapshots for Linux.

An upstream release does not become a button. It becomes a prompt to build and sign a
new set. Only signed sets are offered for install.

**Who starts a rollout.** For kbf's own components (`kbf-daemon`, `kbf-updater`,
`kbf-mac-session`), the farm's CI on GitHub Actions builds every green, attested
`main`, signs a set naming those artifacts and calls `POST /v1/rollouts` itself, with a
`rollout` credential that may start rollouts of signed sets and nothing else. The
server runs the rolling mechanics. OS, Xcode and firmware sets are signed by the same
CI from a reviewed change; a deployment chooses whether merging one starts its rollout
automatically or waits for an operator's click.

### 3.3 Node states

```
serving -> cordoned -> draining -> updating -> rebooting -> qualifying -> serving
                                       \             \             \
                                        +-------------+-------------+--> held | quarantined
```

- `cordoned`: placement skips the node; running leases continue.
- `updating` and `rebooting`: the node is silent by design. Absence alerts are
  suppressed until a per-platform return deadline (start values: Linux 15 minutes,
  macOS 60 minutes **[A]**). Leases are still requeued at the grace period as today.
- `held`: a gate failed; the node stays out and the rollout halts.
- `quarantined`: the node's report or leak scan does not match; only a repair or an
  operator returns it.
- `preparing` and `restoring` (Mac whole-machine leases only, section 10.2): the
  node's GUI session is being switched before a lease starts or after it ends.

## 4. The rollout protocol

### 4.1 The rollout record

| Field | Meaning | Default |
|---|---|---|
| `target` | the software set digest | the pool's newest signed set |
| `selector` | which nodes: a list, or one or more pools (`os=macos`; `os=linux`, which covers the x86-64 and arm64 pools) | |
| `max_unavailable` | nodes of one pool out at once; each pool in the selector has its own | 1 |
| `min_serving` | live, uncordoned nodes kept per capability class | max(1, 25%) |
| `canary` | nodes updated first, per pool | 1 |
| `soak` | wait after the canary before the rest | 2 h |
| `qualify` | the qualification job (4.2) | per platform |
| `drain_deadline` | how long running leases may finish | 30 min |
| `client_gate_timeout` | how long `awaiting_client` may wait (6.1) | 24 h |
| `accept_outage` | classes the operator allows to drop to zero, with a reason | none |
| `window` | optional maintenance window | none |
| `state` | `pending`, `running`, `awaiting_client`, `held`, `done`, `cancelled` | |
| `actor` | who started it, and when | |

A **capability class** is a hardware and platform class only: the node's `os`, `arch`,
`drivers` and `gpu`. Software keys (`xcode`, `os_build`, `kernel`, ...) never define a
class, or a rollout would protect the very value it replaces.

**The record is durable before anything moves.** Today the scheduler's control log is
the process itself ([scheduler.md](scheduler.md)), so a restart forgets everything.
Rollouts that touch nodes (phases P2 and P3) therefore wait for a durable rollout
record: the Raft control log once it is wired, or until then a record the server
writes and syncs to disk before each step. Every step is idempotent and names its
rollout and step, so a restart or leader change continues or halts the rollout, never
repeats a half-done step blindly.

**On startup the server reconciles** before it places work on any node in a rollout:

- with `kbf-mdm-gate`: every outstanding macOS enforcement that does not match a
  durable rollout step in `updating` is withdrawn;
- with each node: its `NodeStatus` carries the updater's state file and the daemon's
  update marker (4.2 step 5). A node that reports an update in progress with no
  matching durable step goes to `held` and alerts. It is never assumed serving.

### 4.2 Per node

1. **Pre-gate.** No firing alerts. Inside the window. `min_serving` would still hold
   for every capability class the node belongs to. A node that is the last of its
   class (the only arm64 node, the only GPU Mac) is **never** taken unless the
   rollout's `accept_outage` names that class: an explicit, audited choice to run
   without that platform for the update's duration, shown in the plan (11.3). For a
   node that runs a server role: object-store healing complete, all consensus voters
   healthy, and leadership transferred away first.
2. **Take the slot.** A counting semaphore per pool, owned by this rollout and node,
   with no time-based expiry.
3. **Cordon.** Placement skips the node.
4. **Drain.** A new `ServerMessage::Drain{deadline}` (the planned drain message in
   [worker-protocol.md](worker-protocol.md)). Running leases, including whole-Mac
   leases, run to completion. Preempting and requeueing `batch` work at once depends
   on preemption, itself **planned** ([scheduler.md](scheduler.md)); until it lands,
   `batch` work drains like any other. At the deadline the rollout **holds** rather
   than killing work.
5. **Apply.** The server checks its lease records: no live lease on the node. It then
   sends a new `ServerMessage::Update{rollout, step, set_digest}` on the existing
   stream, so no node needs an inbound port. The daemon **refuses `Update`** while any
   lease is live or a lease's 40 s self-fence window
   ([worker-protocol.md](worker-protocol.md)) is still open, and otherwise writes a
   local update marker and refuses every `Start` until the step finishes or is
   cancelled. The marker survives a daemon or server restart, so a Mac about to be
   force-updated by the MDM never accepts work, whatever the server remembers. The
   daemon forwards the step to `kbf-updater` (section 5); for macOS itself the server
   asks the MDM through the gate (section 7.2).
6. **Reboot,** when the update needs one.
7. **Re-handshake.** The daemon reconnects within the return deadline, and its `Hello`
   and `NodeStatus` must show the target set. The node's own report is the done
   signal, not the MDM's or the updater's. Anything else quarantines the node.
8. **Qualify.** Startup checks pass (on Linux: cgroup v2, delegated controllers, PSI).
   The qualification job runs as real leases pinned to the node with the reserved key
   `kbf-node=<id>`, which only the rollout driver can submit (3.1). Mac: a Swift and
   Xcode build, one GUI lease, and the local-LLM GPU test of `macos-vms.md` section
   8.2. Linux: a container build and the per-node-class probe suite. Mac nodes that
   serve whole-machine leases also record a new leak-scan baseline (section 10.2).
9. **Uncordon and release the slot.** Next node. After the canary, soak first.

### 4.3 Failure, halt and rollback

- Any failed gate moves the node to `held` and the rollout to `held`, and fires a
  native alert. **Nothing proceeds by itself.** The slot is not released: the broken
  node holds the only slot, so the rollout cannot reach a second node.
- `awaiting_client` (6.1) is a hold too: it alerts when entered, and at
  `client_gate_timeout` it becomes `held` and alerts again.
- Operator actions: **resume**, **skip node** (mark it failed and release its slot),
  **cancel**, **roll back**.
- **Roll back** is a new rollout to a newly signed set (a higher serial naming the
  old artifacts, 3.2), with the same gates. It works where the old artifacts can be
  installed again: Linux on bootc (switch to the old image digest), Linux kernels and
  snapshots, Xcode, simulator runtimes, VM images, the profile and `kbf-daemon` (all
  side by side). **A macOS update cannot be rolled back**; the repair is an erase and
  re-provision at the pool's set (section 7.5). The UI says this before a macOS
  rollout starts.

Prior art: Kured ([kured](https://kured.dev/docs/configuration/)), FleetLock
([protocol](https://coreos.github.io/zincati/development/fleetlock/protocol/)) and Borg
([Verma et al.](https://research.google.com/pubs/archive/43438.pdf)) **[V]** hold a
counting lock per group, gate before and after, and keep the lock on failure. kbf is
the scheduler, so drain is a scheduler state and leftover work is requeued.

## 5. Who applies a change on the node

### 5.1 Options

| Option | What it is | Fit |
|---|---|---|
| A. Root helper | `kbf-updater`, a small root service with fixed verbs, verifying signed manifests | both OSes; one code path; the daemon stays unprivileged |
| B. Provisioning profile | re-run the idempotent profile (`kbf-mac-provision apply`, #76) as root | Mac settings, Xcode, runtimes; not the OS itself |
| C. MDM | Apple's device-management protocol | the only supported unattended macOS update path on macOS 27 that keeps no volume owner's password on the node (7.2, 7.3); Apple only |
| D. Image-based OS | the host OS is a container image (bootc), staged A/B | Linux; atomic, with rollback |

**Recommendation:** A as the single entry point on every node. It runs B for Mac
settings and developer tools, D (or pinned packages) for Linux, and kbf-daemon
upgrades. macOS itself goes through C, ordered by the server through the gate.

### 5.2 `kbf-updater`

- A LaunchDaemon on Macs, a systemd unit on Linux, running as root.
- Three verbs: `status`; `stage <set>` (fetch and verify, change nothing installed);
  `apply <set>` (install, and reboot if the set says the step needs it). There is no
  `rollback` verb (a rollback is a newer set, 3.2) and no bare `reboot` verb, so no
  caller can reboot a node without a signed set that requires it. Applying the set
  already installed is a no-op that reports success and never reboots.
- It fetches the manifest and artifacts by digest from the farm's CAS, verifies the
  ed25519 signature against a public key pinned at provisioning, checks every
  artifact's SHA-256, and refuses a set whose `pool` or `platform` differ from those
  pinned at provisioning, or whose serial is not higher than the installed one.
- It keeps a state file (last applied set, in-progress step), so a crash or reboot
  mid-apply resumes or reports, never guesses. `NodeStatus` carries it.
- On bootc it never calls `bootc rollback`: that only queues the previous image for
  the next boot unless given `--apply`, and changes made to `/etc` since then do not
  carry over ([bootc-rollback](https://bootc.dev/bootc/man/bootc-rollback.8.html))
  **[V]**. A rollback is a switch to the old digest named by a signed set
  ([bootc-switch](https://bootc.dev/bootc/man/bootc-switch.8.html)) **[V]**, and node
  configuration is never kept as local changes to `/etc`.

### 5.3 Who can reach a root helper

Both root helpers (`kbf-updater`, and `kbf-mac-session` of 5.4) listen on a Unix
socket in a root-owned directory, mode 0750, group the daemon's role group, socket
mode 0660. That alone is not enough: on `main` the native driver runs every action as
the daemon's own user (`crates/kbf-driver-native/src/lib.rs`), so any action could
connect. Two controls, the first being the one that matters:

1. **No action runs under a uid in the helper's group.** On Macs every action runs as
   a per-lease user outside the group (L0 in 10.2; also #85's phase 1). On Linux
   actions run in rootless containers whose mount namespace does not contain the
   helper's socket directory. A node does not enable either helper until this holds:
   on Macs, `kbf-updater` ships with the per-lease user, not before.
2. **The helper checks the caller's identity on every connection.** On Linux:
   `SO_PEERCRED` gives the peer's uid and pid; the uid must be the daemon's, and the
   peer's executable (`/proc/<pid>/exe`) must hash to the `kbf-daemon` digest of the
   installed set. On macOS: the peer's audit token (`LOCAL_PEERTOKEN`) gives the uid
   and the process's code identity; the uid must be the daemon's, and the code
   signature must satisfy the `kbf-daemon` requirement (its cdhash) pinned by the
   installed set. This is defence in depth: code running as the daemon's uid could
   still drive the daemon itself, which is why control 1 comes first.

Planned test (`kbf-it`, on a hosted macOS runner and a Linux runner): a lease's action
connects to each helper's socket and sends `status`; the connection must be refused
(at `connect`, or by the identity check) and the helper's audit log must record the
refusal. Mutants that must turn it red: socket mode 0666; the action run as the
daemon's uid; the identity check removed (with a test binary of the daemon's uid as
the caller).

### 5.4 `kbf-mac-session`

Per-lease users (section 10.2) need root for every step: creating and deleting users,
setting auto-login, killing a user's processes, and reading the whole disk for the
leak scan. `kbf-daemon` stays unprivileged and `kbf-updater` touches software only, so
these belong to a second Mac-only root helper, `kbf-mac-session` (LaunchDaemon, socket
rules of 5.3, Full Disk Access granted by MDM at provisioning). Its fixed verbs, every
argument restricted to the lease uid range (default 600-699 **[A]**):

| Verb | Does |
|---|---|
| `user-create <lease>` | create `kbf-lease-<n>`: random password, no secure token, non-admin; admin only for a `kbf-mac-admin` lease (10.2 L0) |
| `session-login <lease>` | set auto-login to that user, then a userspace restart (or a full reboot) |
| `session-idle` | set auto-login back to the node's idle user, then the same restart |
| `kill-uid <uid>` | kill every process of a uid in the lease range |
| `user-delete <lease>` | delete the user and home, sweep the uid's files in shared places |
| `scan` | the leak scan of 10.2 against the stored baseline; returns the diff |
| `baseline` | record the scan baseline; refused unless no lease user has existed since the last boot that followed an apply or an erase |
| `reboot-dirty` | reboot after a leak, and refuse every verb but `scan` until the next scan |

It can never touch a uid outside the lease range, the idle user or the MDM's managed
administrator, and it cannot change installed software. `automationmodetool` is not
one of its verbs: it runs once, as root, from the provisioning profile (10.2 L1).

## 6. Version skew and routing

During a rollout a pool is mixed. Three rules keep that safe:

- **Pinned work routes exactly.** An action that pins `os_build`, `xcode`,
  `os_image`, `vm.image` or a probe value through its platform properties only matches
  nodes that report it (3.1). Unpinned work may land on either side.
- **The UI shows the split.** Queued actions per value of each pinned key, so an
  operator sees when the old side is idle and can be updated fast.
- **Finish a Mac roll in one window.** Apple toolchains match by exact build, so a
  split pool has less capacity for each side.

### 6.1 Client host-identity lists

A client project may compute its own host identity on a Mac (for example a digest of
the developer directory, SDK version and build, compiler, linker and OS build) and
keep a list of accepted identities in its build configuration. If that list is part
of every action key, any change to it cold-misses that client's whole Mac cache, and
a rolling update changes it twice: once to add the new identity after the canary, once
to remove the old one at the end.

kbf stays generic and helps with two things:

- **A probe.** A named command the daemon runs and reports as `probe.<name>`. The
  command comes only from the node's provisioned configuration or from the pool's
  signed set, never from the server, so the server cannot make a node run a command.
  A client's identity script is one probe. kbf never names a client.
- **A client gate.** A rollout can stop at `awaiting_client` after the canary, until
  the operator confirms that the client accepts the canary's new probe value. It
  alerts and times out into `held` (4.3).

The better fix belongs to the client: route on one exact property per action
(`probe.host_identity=<value>`) instead of keying on a list, so an update is one cold
miss and a one-value change.

## 7. The Mac path

### 7.1 What Apple changed

On macOS 27 (the pinned major version of #76) the old update controls are gone:

- The `com.apple.SoftwareUpdate` profile payload: deprecated 26.0, removed 27.0
  ([schema](https://github.com/apple/device-management/blob/release/mdm/profiles/com.apple.SoftwareUpdate.yaml))
  **[V]**.
- The MDM commands `ScheduleOSUpdate`, `AvailableOSUpdates` and `OSUpdateStatus`:
  deprecated 26.0, removed 27.0 for macOS
  ([schedule](https://github.com/apple/device-management/blob/release/mdm/commands/system.update.schedule.yaml),
  [available](https://github.com/apple/device-management/blob/release/mdm/commands/system.update.available.yaml),
  [status](https://github.com/apple/device-management/blob/release/mdm/commands/system.update.status.yaml))
  **[V]**.
- What remains is declarative device management (DDM). Whether local preference keys
  are still honoured on 27 is **[A]**.

### 7.2 macOS updates with MDM (recommended)

- **Block automatic updates.** DDM software-update settings set automatic download and
  install `AlwaysOff` and defer major, minor and system updates 1 to 90 days
  ([settings](https://github.com/apple/device-management/blob/release/declarative/declarations/configurations/softwareupdate.settings.yaml))
  **[V]**. Background Security Improvements "can still be installed ... independent of
  the Enable key"
  ([Apple](https://support.apple.com/guide/deployment/software-update-settings-declarative-dep0578d8b8a/web))
  **[V]**, and each changes the OS build. So a node's build can still move without a
  rollout; re-detection (3.1) reports it, and the server treats it as drift.
- **Install a chosen update.** The server drains the node, then asks the gate to
  post `softwareupdate.enforcement.specific` for that one device: `TargetOSVersion`,
  `TargetBuildVersion` and a `TargetLocalDateTime` a few minutes ahead. At the
  deadline "the device force installs it". It needs macOS 14 or later, supervision and
  system scope
  ([schema](https://github.com/apple/device-management/blob/release/declarative/declarations/configurations/softwareupdate.enforcement.specific.yaml))
  **[V]**. Whether a deadline a few minutes ahead installs at once on a drained node,
  and whether major upgrades enforce the same way, is **[A]**.
- **A supplemental release** (a build with an "(a)" suffix) installs only on its base
  version, so moving to it from an older version takes two enforcement steps: the
  base version first, then the supplement (same schema) **[V]**. The rollout plans
  both as one step for the node.
- **No password.** On Apple silicon the bootstrap token authorises the update; on
  macOS 26 and later it is created at enrollment, so no user login is needed
  ([bootstrap token](https://support.apple.com/guide/deployment/use-secure-and-bootstrap-tokens-dep24dbdcf9e/web),
  [enforcement](https://support.apple.com/guide/deployment/install-and-enforce-software-updates-depd30715cbb/web))
  **[V]**.
- **Watch it.** DDM status items report install state, pending version and failure
  reason ([status](https://github.com/apple/device-management/tree/release/declarative/status))
  **[V]**. The server shows them in the UI while the node is silent.
- **A pin holds while Apple's catalogue lists the build.** Enforcement only works
  while GDMF lists that version or build, and a service must remove declarations that
  name versions no longer available (same schema) **[V]**. Each entry in the catalogue
  has an expiry date; the UI shows it per pool, and the server alerts 14 days before a
  pool's pinned build expires. After that a node erased and re-provisioned can no
  longer be brought to the old build, so a pool must move before expiry.

### 7.3 macOS updates without MDM (fallback only)

`softwareupdate --install <label> --restart --user <owner> --stdinpass`: `--user` is
"An owner user to authorize installation" (a volume owner with a secure token, not any
administrator), both flags are "Apple silicon only", and with nobody logged in
`--restart` makes macOS "trigger a forced reboot if necessary"
([softwareupdate(8)](https://keith.github.io/xcode-man-pages/softwareupdate.8.html))
**[V]**; from a background service on 27 it is **[A]**. A root job then holds a volume
owner's password as a secret. Lease users (10.2) have no secure token and can never
authorise an update. There is no unattended re-setup after an erase and no supported
way to block updates. This path is for the time before MDM is live, not a design.

### 7.4 Xcode, simulator runtimes and the Metal toolchain

- **Download: a person, once per Xcode release.** The `.xip` needs an Apple Developer
  sign-in, and automated two-factor sign-in is unreliable
  ([xcodes#326](https://github.com/XcodesOrg/xcodes/issues/326)) **[V]**.
- **The licence limits where the `.xip` may be kept**
  ([Xcode SLA](https://www.apple.com/legal/sla/docs/xcode.pdf)) **[V]**. Section 2.2
  grants a licence to "Install a reasonable number of copies of the Apple Software on
  Apple-branded computers that are owned or controlled by You to be used internally".
  Section 2.7 restricts it: "You agree not to rent, lease, lend, upload to or host on
  any website or server, sell, redistribute, or sublicense the Apple Software and Apple
  Services, in whole or in part", and "This Agreement does not allow the Apple Software
  or Services to be made available over a network where they could be run or used by
  multiple computers at the same time". A private object-store mirror of the `.xip`
  sits directly against "host on any ... server". So:
  - **The mirror needs legal sign-off** before it is built. Counsel should also read
    the "made available over a network" sentence for a farm that runs Xcode's tools
    for remote builds **[A]**; this document does not settle it.
  - **Fallback without sign-off:** the operator downloads the `.xip` on one farm Mac
    and `kbf-updater` copies it Mac to Mac, checked against the set's SHA-256; no
    server or object store holds it (also for counsel **[A]**). The fallback with no
    licence question is a person downloading on each Mac, once per release.
  - The `.xip` never goes in a public bucket or in this repository.
- **Runtimes and components:** downloaded once with `xcodebuild -downloadPlatform ...
  -exportPath` and `-downloadComponent metalToolchain -exportPath`, installed on each
  node offline with `-importPlatform` and `-importComponent`
  ([Apple](https://developer.apple.com/documentation/xcode/downloading-and-installing-additional-xcode-components))
  **[V]**. They are Apple Software under the same licence, so the same sign-off
  applies.
- **Install** (by `kbf-updater` running the profile): expand to
  `/Applications/Xcode-<build>.app` (`xip` checks Apple's signature), `xcodebuild
  -license accept`, `xcodebuild -runFirstLaunch`, import runtimes. That all of this
  works with no Apple Account signed in is **[A]**.
- **Side by side, selected per action.** Adding an Xcode is additive: no drain, no
  reboot. The daemon sets `DEVELOPER_DIR` for each action from its `xcode` platform
  property (#85 phase 1); it never switches the global `xcode-select`. The node
  reports every installed build in `xcode` (membership, 3.1). Old Xcodes are removed
  when no pool's set names them. First-launch packages and simulator runtimes may be
  shared by every installed Xcode, so a new Xcode can change what an older one uses
  **[A]**; the canary's qualification checks this.
- **Order.** Each Xcode sets a minimum macOS. A set whose Xcode needs a newer macOS
  updates macOS first, in the same rollout step; the UI shows the dependency.
- **Never from the App Store,** which offers only the newest Xcode and so cannot pin
  a version **[A]**.

### 7.5 Enrollment, erase and re-provision

- **Zero-touch setup.** Automated Device Enrollment with Auto Advance skips every Setup
  Assistant pane on a Mac with Ethernet, macOS 11 or later
  ([ADE](https://support.apple.com/guide/deployment/automated-device-enrollment-management-dep73069dd57/web))
  **[V]**. Apple also requires the MDM to create a **managed administrator account**
  and skip the account-creation pane (same page) **[V]**, so every node has one. Its
  password stays in the MDM; it is never handed to a lease, never used by
  `kbf-mac-session`, and the leak scan alerts if it logs in. After enrollment the MDM
  installs the provisioning package (DDM `com.apple.configuration.package` on macOS 26
  and later
  ([schema](https://github.com/apple/device-management/blob/release/declarative/declarations/configurations/package.yaml))
  **[V]**), the profiles of section 10.2, and the node's join credential.
- **Remote erase.** `EraseDevice` runs Erase All Content and Settings on Apple
  silicon, silently authorised by the bootstrap token
  ([erase](https://support.apple.com/guide/deployment/erase-devices-dep0a819891e/web))
  **[V]**. Send `ObliterationBehavior: DoNotObliterate`, so a failed preflight reports
  an error instead of falling back to a reinstall that needs a person
  ([device.erase.yaml](https://github.com/apple/device-management/blob/release/mdm/commands/device.erase.yaml))
  **[V]**. "Return to Service" is not available on macOS (same file) **[V]**, so the
  erased Mac comes back through ADE and Auto Advance. That this chain runs end to end
  with no person on 27 is **[A]**: one probe settles it. Every erase goes through the
  gate (7.6).
- **Cost of a full re-provision** (erase, enrollment, profile, Xcode and runtimes,
  cache refill): about 45 to 90 minutes **[A]**. A repair step, not a per-lease step.
- **Activation Lock.** Never sign an Apple Account into a node. Apple Business Manager
  can turn Activation Lock off for organisation-owned Macs
  ([Apple](https://support.apple.com/guide/deployment/activation-lock-depf4ab94ef1/web))
  **[V]**.

### 7.6 What MDM needs

- **Apple Business Manager** (it needs a D-U-N-S number). Macs bought through Apple or
  an authorised reseller linked to the organisation appear automatically. A Mac set up
  before that is added once, in person, with Apple Configurator for iPhone at Setup
  Assistant, with a 30-day provisional period
  ([Apple](https://support.apple.com/guide/business/add-devices-using-apple-configurator-axm200a54d59/web))
  **[V]**.
- **Proprietary services kbf cannot avoid.** Apple Business Manager and Apple's push
  service (APNs) are Apple's closed services; every Mac MDM depends on them. kbf's
  own parts stay open source; these two are a fixed external dependency of running
  Macs at all.
- **A self-hosted, open-source MDM.** NanoHUB (MIT) "unifies NanoMDM, NanoCMD, and
  KMFDDM" ([nanohub](https://github.com/micromdm/nanohub)) **[V]**, so it includes a
  DDM server. NanoDEP (MIT) speaks the enrollment API
  ([nanodep](https://github.com/micromdm/nanodep)) **[V]**; micromdm/scep (MIT) issues
  enrollment certificates ([scep](https://github.com/micromdm/scep)) **[V]**.
  MicroMDM v1 is in maintenance mode with support ended
  ([micromdm](https://github.com/micromdm/micromdm)) **[V]**. Fleet is open-core: on
  its pricing table both "Enforce operating system (OS) updates" and "Send lock and
  wipe commands" are ticked for Premium only, with the Free column empty
  ([pricing](https://fleetdm.com/pricing)) **[V]**, so it does not fit an
  open-source-only farm.
- **An APNs push certificate,** renewed yearly by a person under the same Apple
  account. Its CSR must be signed by an MDM vendor certificate, and there are two
  ways to get one: a community vendor-signing service such as
  [mdmcert.download](https://mdmcert.download) (free; an outside dependency that sees
  the CSR) ([micromdm certificates](https://micromdm.io/blog/certificates/)) **[V]**,
  or the organisation's own vendor certificate, which needs Apple Developer Enterprise
  Program membership **[A]**. The ABM server token also expires. Both expiries are
  native alerts at 30 and 7 days.
- **TLS that a freshly erased Mac trusts.** The enrollment URL is contacted during
  Setup Assistant, before any profile is installed, so its certificate must chain to a
  publicly trusted CA, or the ADE enrollment profile must carry the private CA as an
  anchor certificate **[A]** (to read in Apple's ADE profile reference before P3).
- **Where it runs.** As a service on the farm's Linux nodes, under its own uid. Its
  enrollment and check-in endpoints must be reachable on the rack network, not only
  on an overlay network, because a freshly erased Mac enrolls before any overlay
  client is installed. Its API is not: only `kbf-mdm-gate` holds the API key.
- **`kbf-mdm-gate`** (decision: build it). NanoHUB's API with the bootstrap tokens can
  erase every Mac and enforce any build Apple signs. Handing that API to `kbf-server`,
  a large network-facing process, would make a server compromise a fleet wipe. The
  gate is a small separate process (its own uid, on the MDM's host) that holds the
  API key and offers `kbf-server` only:
  - `enforce <serial> <set>`: the gate fetches the set, verifies its signature against
    the same pinned key, checks that the set's pool is the pool the gate has on record
    for that serial (from its own provisioned inventory, not from the server), and
    posts the enforcement for exactly the set's build;
  - `withdraw <serial>`: remove an outstanding enforcement;
  - `erase <serial> <reason>`: refused while another erase is outstanding (until that
    Mac re-enrolls and checks in, or 24 h pass); refused beyond a daily cap (default
    2); refused if the Macs checked in and not being erased would drop below the gate's
    own Mac floor. The gate counts Macs from MDM check-ins, not from what the server
    says;
  - status reads (enrollment, DDM status, last check-in).

  Every call is in the gate's own audit log. A compromised server can then erase at
  most one Mac at a time and two a day, and enforce only builds a reviewed set names.
  The alternative (the server holds the API, and the doc says so) costs nothing to
  build but makes a server compromise a Mac-fleet wipe.
- **kbf stays generic.** kbf ships no MDM. `kbf-server` talks to the gate through an
  `OsUpdateBackend` trait (crate `kbf-mdm`, which also holds the gate); the first
  backend targets NanoHUB's API **[A]**. Nodes map to enrollments by the serial and
  platform UUID the daemon reports. The MDM decides nothing.

## 8. The Linux path

No Apple-style MDM protocol exists for Linux; root plus `kbf-updater` plays that role.
The questions are which agent applies the update and who orders it: `kbf-server` orders
it and `kbf-updater` applies it. Products that call themselves Linux device management
do not fit: Canonical Landscape's free self-hosted tier is for up to 10 machines for
personal or evaluation use
([Landscape editions](https://ubuntu.com/landscape/pricing)) **[V]**, and Fleet is
open-core (7.6).

### 8.1 v1: pinned packages

- Keep the current distribution (Ubuntu 24.04 LTS on the reference deployment **[A]**,
  inferred from the kernel). Turn unattended upgrades off.
- A pool's target is a dated archive snapshot plus the expected kernel package. On
  24.04, apt accepts `--snapshot <timestamp>`
  ([Ubuntu snapshot service](https://ubuntu.com/server/docs/how-to/software/snapshot-service/))
  **[V]**, so every node of a pool reaches the same package set and "update available"
  means "a newer approved snapshot exists".
- `kbf-updater apply` runs a full upgrade against the snapshot, then reboots only if
  `/run/reboot-required` exists.
- Podman fit: cgroup v2 only; the profile's drop-in on `user@.service` delegates
  `cpu cpuset io memory pids`, and the post-update gate rechecks it
  ([rootless cgroup v2](https://rootlesscontaine.rs/getting-started/common/cgroup2/))
  **[V]**.
- Rollback is weak: a set naming the previous kernel covers a kernel regression; a
  userland rollback is a downgrade to the previous snapshot, not atomic. A node that
  does not boot needs its BMC or a person.

### 8.2 Target: an image-based host OS (bootc)

- `bootc upgrade` stages a new image A/B style, `--apply` reboots into it, and
  `--check` only reads metadata
  ([bootc-upgrade](https://bootc.dev/bootc/man/bootc-upgrade.8.html)) **[V]**; for
  rollback see 5.2.
- greenboot-rs (BSD-3, written for bootc) adds boot-time health checks and an
  automatic rollback after a set number of failed boots
  ([greenboot-rs](https://github.com/fedora-iot/greenboot-rs)) **[V]**; the shell
  greenboot it replaces is deprecated. Boot counting only catches a boot that ends in
  a reboot: a hang needs a hardware watchdog to turn it into one. So a Linux node that
  fails to boot recovers without a person **provided a watchdog is enabled** on its
  board **[A]**, which is checked per board before P5.
- The host OS becomes an image pinned by digest, built by the operator's CI from a
  pinned upstream base, the same way kbf treats action images. "Update available"
  means "a newer attested image exists for this pool".
- Cost: a reinstall per node (cheapest when new servers arrive), a distribution change
  to one with bootc images, and a check that arm64 images exist for the arm64 nodes
  **[A]**.
- Also considered: Fedora CoreOS with Zincati (a FleetLock client,
  [protocol](https://coreos.github.io/zincati/development/fleetlock/protocol/)) and
  Flatcar with locksmith (its lock in etcd,
  [locksmith](https://github.com/flatcar/locksmith)) **[V]**: both need an external
  lock service, which kbf-server could be by serving FleetLock's `/v1/pre-reboot` and
  `/v1/steady-state`. Live patching does not replace reboots
  ([Livepatch](https://ubuntu.com/security/livepatch)) **[V]**.

### 8.3 Firmware

fwupd's UEFI capsule plugin installs capsules on the next reboot
([uefi-capsule](https://fwupd.github.io/libfwupdplugin/uefi-capsule-README.html))
**[V]**; NVMe and BMC support depend on the vendor. Firmware is a set kind of its own:
always manual approval, canary, soak, concurrency 1. A failed flash is recoverable
only through the BMC or by a person, so firmware updates wait until a node's BMC is
known to work. Microcode ships as distribution packages and rides the normal rollout
**[A]**.

## 9. VM images as rolled-out software

- A golden VM image (`macos-vms.md` section 7) is built in CI on a Mac from a pinned
  restore image, a pinned Xcode and runtimes, and named by digest.
- It is part of a pool's software set. `kbf-updater stage` places it on nodes ahead
  of time, and the node reports it in `vm.image`; switching the set makes VM leases
  ask for the new `vm.image`. The host is not touched and nothing reboots.
- A guest cannot run a newer macOS than its host **[A]**, so a set that moves both
  updates hosts first, then images.

## 10. GPU work and desktop-app tests on a bare-metal Mac

### 10.1 No GPU in VMs

On Apple silicon there is no GPU passthrough; a macOS guest gets a paravirtual Metal
device. One published measurement on an M1 Ultra found a stock guest running LLM
inference at about 4-14% **of** bare-metal speed (7 to 23 times slower): TinyLlama
generation 12.63 tokens/s against 286.71 (4.4%), Gemma 12B prompt processing 13.8%
([measurements](https://github.com/trycua/cua/blob/main/blog/gpu-passthrough-macos-vms.md))
**[V]** for their machine, **[A]** for others. The same source's faster guest depends
on injecting a library to change private Metal behaviour; not fit for a farm.

So a GPU test, including a test that installs the desktop app and drives it with a
local LLM, is a `kbf-lease=whole_machine` lease with `gpu=1`, as `macos-vms.md`
section 8.1 defines: it empties the Mac, no other lease (bare-metal or VM) runs there
at the same time, and GPU tests on one Mac run one at a time. **Planned:** the
whole-machine booking (all of the node's `cpus`, memory, `gpus` and both `vms` slots)
and the reservation that keeps one-core work from starving it (`macos-vms.md` section
5.3), and the runtime that serves it, below. VMs remain for simulator and GUI tests
that do not need the GPU.

**The bare-metal whole-machine runtime** (planned, in `kbf-driver-native`) is the
runtime #85 points to for `whole_machine` leases on a Mac. Per lease it: asks
`kbf-mac-session` for a fresh lease user; moves the node through `preparing`, where
the GUI session switches to that user; runs the action as that user inside the
lease's sandbox profile; and moves the node through `restoring`, where the user is
killed, deleted and the leak scan runs. Its layers are next.

### 10.2 Isolation layers

macOS has no namespaces or cgroups. The isolation available is separate users, what a
non-admin user cannot change, a scan for what changed anyway, and an erase. The system
volume is a sealed, signed snapshot
([signed system volume](https://support.apple.com/guide/security/signed-system-volume-security-secd698747c9/web))
**[V]**, so whatever a lease leaves behind is on the Data volume, which is what the
scan covers.

| Layer | When | What | Time **[A]** |
|---|---|---|---|
| L0 | every lease | a fresh non-admin user `kbf-lease-<n>` with no secure token, made by `kbf-mac-session`; the app installed into that user's own Applications folder or run from the action directory; model weights read from a root-owned, read-only model store filled from the CAS | ~10 s |
| L1 | once per Mac, at provisioning | `automationmodetool` run as root by the provisioning profile; Setup Assistant panes skipped for new users (MDM profile); Full Disk Access for `kbf-mac-session` (MDM profile); managed login items for kbf's own services (MDM) | 0 per lease |
| L2 | every GUI lease, in `preparing` and `restoring` | auto-login into the lease user, then a userspace restart (or a full reboot); the same at the end to return to the idle user | 1-4 min |
| L3 | every lease | cleanup (kill the uid, delete the user and home, sweep shared folders) and a leak scan against the node's baseline | 30-60 s |
| L4 | leak, or after a privileged lease | reboot and scan again; still dirty: quarantine, erase through the gate, automatic re-provision, re-qualify | 45-90 min |
| L5 | about monthly per Mac, one at a time | a scheduled erase anyway: proves the repair path works and resets state no scan sees | 45-90 min |

Overhead per desktop-app lease without a leak: about 2 to 4 minutes **[A]**, small next
to an LLM test.

Notes on each layer:

- **L0.** A non-admin user cannot write `/Applications`, `/Library/LaunchDaemons`,
  system extensions or system settings **[A]** (standard macOS behaviour). A test that
  must exercise the real `.pkg` installer into `/Applications` is a **privileged
  lease** (`kbf-mac-admin=true`, a planned reserved key in
  [capabilities.md](capabilities.md)) with an administrator lease user. An
  administrator can reach root, and no later scan can prove a clean state against
  root, so a privileged lease ends in an erase by default (a deployment may relax this
  to reboot and scan, audited). App teams should ship a per-user install path for most
  tests. The MDM's managed administrator (7.5) is never a lease user.
- **L1, UI automation.** `automationmodetool enable-automationmode-without-authentication`
  lets UI-test automation start without a person; its manual says running it "requires
  an administrator to authenticate in the shell" and describes configuring "a device"
  for "labs where the active users do not have administrator privileges"
  ([automationmodetool(1)](https://keith.github.io/xcode-man-pages/automationmodetool.1.html))
  **[V]**. MDM has no payload for it, so the provisioning profile runs it as root.
  That it runs as root without a password prompt, and that the setting survives user
  deletion, are **[A]**. XCUITest then drives the app by bundle id.
- **L1, Accessibility.** A per-user Accessibility grant by profile does not fit: on
  macOS 27 the privacy-profile `Accessibility` key is deprecated, shows a notification
  and lets the user change it
  ([TCC schema](https://github.com/apple/device-management/blob/release/mdm/profiles/com.apple.TCC.configuration-profile-policy.yaml))
  **[V]**, and its replacement is user-scoped and asks the user to consent
  ([app.settings](https://github.com/apple/device-management/blob/release/declarative/declarations/configurations/app.settings.yaml))
  **[V]**.
- **Screen Recording cannot be granted by a profile.** The privacy profile's
  `Authorization` value `AllowStandardUserToSetSystemService`, valid only for
  `ScreenCapture` and `ListenEvent`, lets a standard user approve it without an
  administrator's password (TCC schema above) **[V]**; the click remains. Since every
  lease has a new user, a test that needs real screen capture needs a person's click,
  unless a grant made once persists across lease users **[A]**. Whether XCUITest
  screenshots need it at all is **[A]**; prove it in P0 before relying on it.
- **L1, Setup Assistant.** The `com.apple.SetupAssistant.managed` payload skips the
  first-login panes for each new user
  ([schema](https://github.com/apple/device-management/blob/release/mdm/profiles/com.apple.SetupAssistant.managed.yaml))
  **[V]**.
- **L2 restarts `kbf-daemon` too.** A userspace restart tears down LaunchDaemons as
  well as the GUI session, so the daemon's stream drops. It happens only in
  `preparing` and `restoring`, with no lease running: the whole Mac is booked, the
  server sends a new `ServerMessage::Prepare{lease}`, the daemon has `kbf-mac-session`
  log the lease user in, reconnects, and its `Hello` names that user; only then does
  the server send `Start`. Nothing runs, so nothing is fenced; a node that misses the
  preparation timeout is quarantined and the lease requeued. Auto-login needs
  FileVault off (#76); its stored login is obfuscated, not secret, which is acceptable
  only for a random, non-admin, deleted user. A userspace restart into auto-login on 27
  is **[A]**; the fallback is a full reboot.
- **L3, the scan** (`kbf-mac-session scan`, with Full Disk Access). Each item is
  diffed against a baseline recorded at qualification: users and uids in the lease
  range; launchd jobs and the launch agent and daemon folders (names and hashes);
  background tasks (`sfltool dumpbtm`
  ([Apple](https://support.apple.com/guide/deployment/manage-login-items-background-tasks-mac-depdca572563/web))
  **[V]**); system extensions and loaded kexts; profiles; package receipts;
  `/Applications` with code-signature hashes; system privacy database rows; firewall
  application list, DNS, proxies and the hosts file; System keychain certificates;
  mounts; files owned by the lease uid in shared places; logins of the managed
  administrator; free disk; the Xcode and runtime set.
- **Every scan item has a planted-leak test** (a privileged lease that drops a launch
  daemon must turn the scan red). A scan item that never fails proves nothing.
- **Known limit.** Gatekeeper's assessment state is system-wide and is cached per code
  hash, so a second run of the same app build may skip the first-launch check **[A]**.
  A test of first-launch behaviour asks for a freshly erased Mac (L5 state).
- **Rejected:** reverting an APFS snapshot of the Data volume. Apple's documented
  whole-system restore is a reinstall of macOS followed by Time Machine and Migration
  Assistant
  ([Apple](https://support.apple.com/guide/mac-help/recover-all-your-files-mh15638/mac))
  **[V]**; that there is no supported unattended revert of the Data volume to a
  snapshot is **[A]**. It would also roll back the daemon's own state.

## 11. The Fleet UI and API

### 11.1 What the operator sees

A Fleet page in kbf's web UI (planned), one row per worker:

| Column | Mac | Linux |
|---|---|---|
| Node, pool, platform | yes | yes |
| OS version and build | `sw_vers` | `os-release` |
| Kernel or image | | kernel, or bootc image digest |
| Developer tools | Xcode builds, simulator runtimes | |
| `kbf-daemon` commit, profile version | yes | yes |
| Update available | newer signed set; newer Apple release (minor / security / major) with posting and expiry date | newer signed set; newer snapshot or image |
| State | serving, cordoned, draining, updating, rebooting, qualifying, preparing, restoring, held, quarantined | same, without preparing and restoring |
| Rollout step | if any | if any |
| MDM | enrolled, supervised, bootstrap token escrowed, last check-in | n/a |

The page also shows the push certificate and enrollment token expiry dates, each
pool's pinned macOS build and its catalogue expiry, the gate's erase budget, and, for
Macs, the queued actions per pinned host value (section 6).

### 11.2 What the operator clicks

- **Update** on a row: a one-node rollout to the pool's newest signed set. It still
  cordons and drains first; there is no shortcut.
- **Update all Linux workers**: one rollout across every Linux pool (x86-64 and
  arm64), each pool with its own canary, `max_unavailable` and `min_serving`.
  **Update all Mac workers**: the same for the Mac pools.
- **Pause, Resume, Skip node, Cancel, Roll back** on a running rollout.

A button is disabled, with the reason shown, when it cannot work: a Mac not
supervised or without an escrowed bootstrap token; an Xcode that is not available to
install; an Xcode that needs a newer macOS than the set provides; no signed set newer
than the node's; a node that is the last of its class (unless the operator ticks
"accept outage" and gives a reason).

### 11.3 "Update all Mac workers", step by step

1. The UI shows the plan before it starts: the target set, the canary, the order,
   `min_serving` (with 2 Macs, one always serves), any class that would need "accept
   outage", the expected time, that macOS cannot be rolled back, and any client cache
   cost (section 6.1).
2. The server writes the durable rollout record, then takes the canary: cordon,
   drain (whole-Mac leases finish), `Update` to the daemon (which then refuses work),
   asks the gate to enforce the target build now, watches DDM status, waits for the
   reboot.
3. The daemon reconnects; its report must show the target build and Xcode set.
4. Qualification runs on the canary. Optional client gate (`awaiting_client`).
5. Soak (2 h by default). Then the next Mac, one at a time, the same steps.
6. Any failure: the rollout holds, an alert fires, the failed Mac stays out, every
   other Mac keeps serving.

"Update all Linux workers" is the same with `kbf-updater` applying the snapshot or
image; action images are pinned by digest, so it costs drain time, not cache.

### 11.4 API

Served by `kbf-server` under `/v1`. Every write is an audit record naming the actor.
Two roles (planned; today the only roles are `Daemon` and `Client`,
`crates/kbf-meta/src/model.rs`): `admin` for everything below, and `rollout`, CI's
credential, which may only `POST /v1/rollouts` with a signed set (3.2).

- `GET /v1/nodes`: per node, the observed state, the pool's set, available updates
  and update state. The UI's JSON twin; clients and scripts read it.
- `GET /v1/software`: signed sets per pool, and upstream releases with their source.
- `POST /v1/rollouts`: `selector` (nodes, or one or more pools), `target` (a set
  digest or `latest`), `strategy` (`canary`, `max_unavailable`, `min_serving`,
  `soak`, `window`, `accept_outage`).
- `GET /v1/rollouts/{id}`; `POST /v1/rollouts/{id}:pause`, `:resume`, `:skip`,
  `:cancel`, `:rollback`. Progress goes to the event stream.

## 12. Remote recovery without a BMC

Mac Studios have no BMC. What replaces it:

| Problem | Remote fix | Needs |
|---|---|---|
| Hung Mac | power-cycle through a switched PDU outlet; `pmset autorestart 1` boots it on power return | a switched PDU **[A]** |
| Hung Mac, no PDU | Lights Out Management: an enrolled controller Mac starts, stops or restarts it; Apple lists Mac Studio models ([LOM](https://support.apple.com/guide/deployment/lights-out-management-payload-settings-dep580cf25bc/web)) **[V]** | MDM and one controller Mac on the same subnet over Ethernet (IPv6) **[V]**; a second controller is a redundancy choice, not Apple's requirement; that it resets a panicked Mac is **[A]** |
| Hung Mac, cabled neighbour | `macvdmtool reboot` restarts the target Mac over a USB-C cable from a neighbour ([macvdmtool](https://github.com/AsahiLinux/macvdmtool)) **[V]**, unofficial | a cable to the right port; which port on a Mac Studio is **[A]** (the README lists ports for MacBook Air, MacBook Pro and Mac mini only) |
| Bad software, leak, drift | erase through the gate and automatic re-provision (7.5) | MDM, ABM |
| Will not boot, firmware failure | DFU restore from a cabled neighbour Mac (`macvdmtool dfu`, then `cfgutil restore`) **[A]** for Mac Studio, unofficial | a cable in the DFU port; moving it needs a person |
| Hardware | none | a person |

Entering DFU by hand needs the power button held while plugging in power
([Apple](https://support.apple.com/en-us/108900)) **[V]**. There is no supported remote
way into recoveryOS **[A]**.

Linux: the BMC (Redfish or IPMI) where the board has one; bootc with greenboot-rs and
a watchdog for software that does not boot; a switched PDU otherwise.

**What still needs a person:** adding each Mac bought outside Apple Business Manager
(once); the yearly push certificate and token renewals; one Xcode download per release
(per Mac, if counsel rejects both the mirror and Mac-to-Mac copies); Screen Recording
grants; a Mac that will not boot; firmware recovery without a BMC; hardware.

## 13. Phased plan

| Phase | Delivers |
|---|---|
| P0 probes | on one Mac: DDM enforcement with a near deadline on 27; erase then ADE and Auto Advance; `automationmodetool` as root and across user deletion; Screen Recording need of XCUITest and whether a grant persists across users; userspace restart into auto-login; `DEVELOPER_DIR` per action. On one Linux node: snapshot upgrade; cgroup delegation after reboot; BMC and watchdog presence. Legal: the Xcode licence questions of 7.4 |
| P1 read-only | software keys in the report with membership matching, re-detection and `Report`; `NodeStatus`; `/v1/nodes`; Fleet page with update available; cordon and drain in protocol and scheduler; `kbf-updater` for `kbf-daemon` and the profile only, on Linux, and on Macs once actions run as lease users (5.3) |
| P2 Linux rollouts | durable rollout record; rollout object, canary, soak, qualification, `min_serving`, `accept_outage`, halt, startup reconciliation; Linux updates by snapshot; CI-started rollouts of kbf's own components; Update buttons for Linux |
| P3 Mac rollouts | ABM, MDM and `kbf-mdm-gate` live while the Macs are wiped; macOS and Xcode rollouts; probes and the client gate; Update buttons for Macs |
| P4 bare-metal GPU and desktop-app leases | `kbf-mac-session`; the bare-metal whole-machine runtime with per-lease users, `preparing`/`restoring` and the leak scan with planted-leak tests; L4/L5 erase; then golden VM images as software sets |
| P5 image-based Linux | bootc with greenboot-rs and a watchdog on new servers, then the rest one at a time |

Changes by crate (rough sizes, tests included):

| Crate | Change | Lines |
|---|---|---|
| `kbf-proto` | `NodeStatus`, `Report`; `Drain`, `Update` and `Prepare` server messages | ~200 |
| `kbf-daemon`, `kbf-node` | re-detect and send `Report`; new keys; probes from config or set; update marker and `Update`/`Start` refusals; forward to the updater; `DEVELOPER_DIR` per action | ~800 |
| `kbf-updater` (new) | socket and peer check, manifest verify with pool/platform/serial, profile / package / bootc backends, state file | ~1,900 |
| `kbf-mac-session` (new) | socket and peer check, the verbs of 5.4, leak scan | ~1,500 |
| `kbf-mdm` (new) | `OsUpdateBackend`; NanoHUB client; DDM status; `kbf-mdm-gate` with its erase limits | ~1,100 |
| `kbf-types`, `kbf-sched` | node states; cordon in placement; rollout state machine; slots; `min_serving` by hardware class; `accept_outage`; internal submitter for `kbf-node`; whole-Mac reservation | ~1,900 |
| `kbf-server` | rollout driver, durable record and startup reconciliation, roles, audit, `/v1/nodes`, `/v1/software`, `/v1/rollouts`, Fleet page, absence suppression | ~1,500 |
| `kbf-caps`, `kbf-front` | membership comparison; new exact and set keys, `probe.*`; `kbf-node` refused from clients | ~300 |
| `kbf-alert` | rollout held, client gate waiting, node not back, certificate, token and catalogue expiry | ~200 |
| `kbf-driver-native` | the bare-metal whole-machine runtime: lease users through `kbf-mac-session`, GUI session, cleanup | ~1,500 |
| `kbf-sim`, `kbf-it` | tests below | ~1,000 |

Tests, each with the planted mutant that must turn it red:

| Test | Mutant |
|---|---|
| A rollout never drops a class below `min_serving` (simulation over seeds) | ignore `min_serving` in the pre-gate: the last arm64 node is drained |
| A single-node class updates only with `accept_outage`; a software key never forms a class | include `xcode` in the class: the rollout never starts |
| A failed canary holds the rollout; no second node is touched | release the slot on failure |
| `kbf-updater` refuses a replayed older manifest, and an older one marked "rollback" | skip the serial check |
| `kbf-updater` refuses a manifest for another pool or platform | skip the pool check: a Mac set applies on a Linux test node |
| `kbf-updater` refuses an unsigned or wrongly signed manifest | skip verification |
| An action is refused at each root helper's socket (5.3) | socket 0666; action as the daemon's uid; peer check removed |
| `Update` refused while a lease is live or in the 40 s fence window; `Start` refused while the update marker is set | drop the lease check |
| A server restart with an outstanding MDM enforcement never places work on that Mac, and withdraws it | clear the update marker on reconnect: a lease starts before the forced install |
| A leader change mid-rollout neither repeats nor skips a step | keep rollout state outside the durable record |
| The gate refuses a second outstanding erase, an erase past the cap or below its floor, and a build no signed set names | drop the outstanding-erase check |
| A Mac reporting two Xcode builds handshakes and matches an action asking for either | keep `xcode` exact: the report is refused as repeated |
| The front refuses `kbf-node` from a client | accept it at the front |
| A node that returns with the wrong build is quarantined | accept any `Hello` |
| A planted leak per scan item turns the leak scan red | drop that scan item |

## 14. Assumptions to test

Claims marked **[V]** are read in the source linked where they appear. Every **[A]**,
with what settles it:

| Assumption | Settled by |
|---|---|
| A DDM deadline minutes ahead installs at once on a drained node; majors enforce the same way; local preferences honoured on 27; `--stdinpass` from a background service | P0 |
| Erase, then ADE and Auto Advance, re-provisions with no person on 27 | P0 |
| `automationmodetool` as root without a prompt, and across user deletion; userspace restart into auto-login | P0 |
| XCUITest screenshots need Screen Recording; a grant persists across lease users | P0 |
| `DEVELOPER_DIR` per action selects Xcode for all tools; a new Xcode leaves an older one unchanged | P0, then every canary |
| A private Xcode mirror, or Mac-to-Mac copies, fit the licence; a farm running Xcode tools for remote builds fits section 2.7 | counsel, before P3 |
| An ADE profile can carry a private CA; an own MDM vendor certificate needs Enterprise Program membership; NanoHUB's API covers what the gate needs | reading the references, before P3 |
| No supported unattended Data-volume snapshot revert; Gatekeeper caches per code hash; lease uid range free | P4 |
| Per-lease overhead 2-4 min; re-provision 45-90 min; return deadlines 15 and 60 min | P2-P4 measurements |
| LOM resets a panicked Mac; `macvdmtool` ports on a Mac Studio; a switched PDU exists | P3 |
| VM LLM speed on other Macs; a guest cannot run a newer macOS than its host | #85 phases 2-3 |
| The reference deployment runs Ubuntu 24.04; which boards have a BMC and a watchdog; arm64 bootc images exist; microcode rides the rollout | P0, before P5 |
