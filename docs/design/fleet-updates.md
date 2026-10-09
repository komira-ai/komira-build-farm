# Fleet updates: keeping every node's software current

This document says how kbf keeps the software on its worker machines up to date: the
operating system, Apple's developer tools, VM images and kbf's own daemon. It also
says how a test that installs a desktop app and uses the GPU runs on a bare-metal Mac
without leaking into the next test, what device management a Mac needs and what plays
that role on Linux, and what an operator sees and clicks in the farm's UI.

Everything here is **planned**. Nothing in this document exists in the code today
except where a section says "today". It builds on
[capabilities.md](capabilities.md), [worker-protocol.md](worker-protocol.md) and
[scheduler.md](scheduler.md) and [mac-node-provisioning.md](mac-node-provisioning.md)
(#76: the Mac provisioning profile), and on a design in an open PR:
`macos-vms.md` (open PR #85: VMs, bare-metal builds and the GPU on Mac nodes). Key
names shared with #85 (`vm.image`, `vm.slots`, `kbf-book-cpus`, ...) are defined there
and only used here.
**The security model** (threat model, root helpers and who may call them, signing
keys, credentials, the MDM gate, enrollment) is in
[fleet-updates-security.md](fleet-updates-security.md); sections there are "S1"...
**The MDM as a backend** (the three operations the server uses, erase as an
operator-signed action, network, moving the MDM, configuration management) is in
[mdm-backend.md](mdm-backend.md); sections there are "M1"...

**#76 reflects these points (its sections 6.1-6.3):** the server computes "update
available" (3.2), MDM is decided (7), and `softwareupdate` with a stored password
is a fallback (7.3). #76's profile and script stay; `kbf-updater` applies the profile.

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
| Mac | Apple Business Manager plus a self-hosted open-source MDM (NanoHUB), behind a narrow gate (`kbf-mdm-gate`); `kbf-server` cannot erase a Mac at all (an erase needs an operator's hardware-key signature, M4). Xcodes are installed side by side on the host and selected per action; simulator runtimes live only in VM images (#85). |
| Linux | No Apple-style MDM protocol exists for Linux; root plus `kbf-updater` plays that role. v1: the distribution's packages pinned to a dated archive snapshot. Target: an image-based OS (bootc) with automatic rollback. |
| GPU | No GPU in VMs. A GPU test, including a desktop-app plus local-LLM test, is a `kbf-lease=whole_machine` lease with `gpu=1` that empties the bare-metal Mac, one at a time. |
| App isolation | A throwaway non-admin user per lease, a leak scan against a baseline, a reboot on doubt, and, when a leak persists, quarantine, then an operator-signed remote erase and automatic re-enrollment. |
| UI | A Fleet page: per worker OS, build, kernel or Xcode, update available, state. **Update** per worker; **Update all Linux workers** and **Update all Mac workers** start rolling updates. |
| Hands | Still needed for: enrolling Macs bought outside Apple Business Manager (once each), yearly certificate renewals, one Xcode download per release, Screen Recording grants, an operator's touch per erase (M4.4), a Mac that will not boot, and hardware. |

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

A compromised `kbf-server` can install only signed sets for a node's own pool,
force-update one Mac per pool at a time within the gate's caps, install a profile the
gate already allowlists, and deny service; it cannot erase a Mac (M4). A compromised
MDM host, CI or `main` branch is more serious; S1 states each plainly.

## 3. Node software state

### 3.1 Observed

What a node runs, reported by `kbf-daemon`. Two kinds:

**Matchable keys** go into the node report ([capabilities.md](capabilities.md)), so
actions can route on them. Today every daemon reports `arch`, `os`, `cpus`, `mem_gib`,
`page_size`, `gpu`, `isa_level`, `cpu.features` and `drivers`, the macOS detector adds
`cpu.model` (`crates/kbf-daemon/src/report.rs`), and the native driver adds
`network_isolation`. The matcher already compares `os_image` and `xcode` exactly, but
no daemon reports them. Every new key below is ignored today (an unknown request
property is dropped, `crates/kbf-caps/src/platform.rs`, and an unknown report entry
skipped), so an action that pins `os_build` today matches any Mac; a second `xcode` is
refused as repeated (`crates/kbf-caps/src/report.rs`). Each needs a `kbf-caps` change.

The new comparison is **membership**: the node reports a set (the key repeated, one
value per entry), the request names one value, and the node matches when its set
contains it. It is the comparison #85 defines for `vm.image`.

| Key | Platform | Source | Comparison | Defined by |
|---|---|---|---|---|
| `os_version` | both | `sw_vers`; `/etc/os-release` | exact | this document |
| `os_build` | Mac | `sw_vers -buildVersion` | exact | #76 section 3.1 (`capabilities.md`) |
| `kernel` | Linux | `uname -r` | exact | this document |
| `os_image` | Linux on bootc | the booted image digest | exact (exists) | capabilities.md |
| `xcode` (set) | Mac | every installed Xcode build | membership (changed from exact) | #85 phase 1 |
| `vm.image` (set) | Mac | golden VM images on disk | membership on the digest only | #85 section 5.1 |
| `vm.slots`, `vm.max_cpus`, `vm.max_mem_gib` | Mac | the VM driver | as #85 section 5.2 | #85 |
| `drivers` gains `vm` | Mac | the VM driver's boot check | not a request key: placement maps the lease kind to it (exists, repeated) | #85 section 5.2 |
| `drivers` gains `native-whole-machine` | Mac | listed only when `kbf-mac-session` is present (10.1) | as `vm` | #85 section 5.2 |

**Probes are reported, never requested.** A `probe.<name>` (6.1) is a status value,
not a capability key: a request naming it is refused once unknown keys are refused
(planned in `capabilities.md`; ignored today), and work routes on `xcode` (and
`os_build`).

**Reserved keys** (skipped by the matcher, names read in any case):

| Key | Meaning | Defined by |
|---|---|---|
| `kbf-book-cpus`, `kbf-book-mem-gib` | what a lease books | #85 section 5.1 |
| `kbf-node` | pin a lease to one node, for qualification (section 4.2). The front refuses it from every client with `INVALID_ARGUMENT`: no REAPI caller can send it. The rollout driver submits qualification work to the scheduler directly, as a new internal submitter, so no client role is needed for it. | this document |

**Re-detection is planned.** Today detection runs once, at daemon start
(`crates/kbf-node/src/main.rs`), except the native driver's Xcodes: it asks them again
every few minutes, and a change resends `Hello` and `NodeStatus` mid-stream
(`crates/kbf-driver-native/src/xcode_watch.rs`, issue #164). Noticing a changed report by its hash is listed as
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
the MDM's record), each probe's value per Xcode (6.1), and, where readable, firmware
versions.

### 3.2 Desired: the software set

A *software set* is a manifest for one pool. It names, each with a SHA-256 or digest:

- Mac: the macOS version and build; the Xcode builds and their `.xip` digests; the
  Metal toolchain components; the provisioning profile version; the VM images (which
  carry the simulator runtimes, #85); the probe commands and their expected values per
  Xcode, if any.
- Linux: the archive snapshot and expected kernel package, or the host image digest.
- Both: the `kbf-daemon`, `kbf-updater` and (Mac) `kbf-mac-session` artifacts.

It also carries:

- `pool` and `platform` (`os`, `arch`). A node's `kbf-updater` is provisioned with its
  pool and platform and refuses any other set, so a Mac set can never reach a Linux
  node, nor an x86-64 set an arm64 node.
- A `serial`, monotonic per pool. There is no rollback flag: **a rollback is a new set
  with a higher serial that names the old artifacts**, signed like any other. A
  replayed old set is always refused.
- `expires` and `min_serial` (a per-pool floor that retires bad sets), and an ed25519
  signature by one of two CI keys under an offline root key (S2, S3).

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

**Who starts a rollout.** For the unprivileged `kbf-daemon`, the farm's CI on GitHub
Actions builds every green, attested `main`, signs a set naming it and calls
`POST /v1/rollouts` itself, with a short-lived `rollout` credential that sends only a
target and a pool; the strategy is the server's per-pool policy (S9). Anything that
runs as root (`kbf-updater`, `kbf-mac-session`, OS, Xcode, firmware) is signed with
the platform key only after an operator approves (S2.2); a deployment chooses whether
the approved set's rollout starts automatically or waits for a click.

### 3.3 Node states

```
serving -> cordoned -> draining -> updating -> rebooting -> qualifying -> serving
                                       \             \             \
                                        +-------------+-------------+--> held | quarantined
```

- `cordoned`: placement skips the node; running leases continue.
- `updating`, `rebooting`: silent by design; absence alerts wait for a return deadline
  (Linux 15 minutes, macOS 60 **[A]**); leases are still requeued at the grace period.
- `held`: a gate failed. `quarantined`: the report or leak scan does not match; only a
  repair or an operator returns it.
- `preparing`, `restoring` (Mac whole-machine leases, 10.2): the GUI session switches.

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
2. **Take the slot.** One counting semaphore per pool, shared by every rollout, held
   for this node with no time-based expiry. `min_serving` is re-checked until Apply;
   if another node of the class dies meanwhile, this node is uncordoned instead.
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
   serve whole-machine leases record a new leak-scan baseline after the reboot and
   before the first qualification lease (S4.2).
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
  snapshots, Xcode, VM images, the profile and `kbf-daemon` (all
  side by side). **A macOS update cannot be rolled back**; the repair is an
  operator-signed erase and re-provision at the pool's set (7.5, M4). The UI says this before a macOS
  rollout starts.

Prior art: Kured ([kured](https://kured.dev/docs/configuration/)), FleetLock
([protocol](https://coreos.github.io/zincati/development/fleetlock/protocol/)) and Borg
([Verma et al.](https://research.google.com/pubs/archive/43438.pdf)) **[V]** hold a
counting lock per group, gate before and after, and keep the lock on failure. kbf is
the scheduler, so drain is a scheduler state and leftover work is requeued.

## 5. Who applies a change on the node

### 5.1 The choice

`kbf-updater` (5.2) is the single entry point on every node. On Macs it re-runs #76's
idempotent profile (`kbf-mac-provision apply`) for settings, Xcode and the Metal
toolchain (simulator runtimes live in VM images only); on Linux it applies pinned
packages or a bootc image (section 8); everywhere it upgrades kbf's own components.
macOS itself goes through MDM, ordered by the server through the gate (7.2, 7.6): the
only supported unattended path on 27 that keeps no volume owner's password on a node.

### 5.2 The root helpers, in brief

The full rules are S3 and S4 of
[fleet-updates-security.md](fleet-updates-security.md).

- **`kbf-updater`** (both OSes, root): verbs `status`, `stage <set>`, `apply <set>`. It
  installs a set only if it is signed under the root-signed key statement, for its own
  pool and platform, newer than the installed serial, unexpired and above the pool's
  floor. It keeps a state file, so a crash mid-apply resumes or reports. Applying the
  installed set is a no-op.
- **On bootc it never calls `bootc rollback`**, which only queues the previous image
  unless given `--apply` and drops `/etc` changes made since
  ([bootc-rollback](https://bootc.dev/bootc/man/bootc-rollback.8.html)) **[V]**; a
  rollback switches to the old digest a signed set names
  ([bootc-switch](https://bootc.dev/bootc/man/bootc-switch.8.html)) **[V]**, and node
  configuration is never kept as local changes to `/etc`.
- **`kbf-mac-session`** (Mac, root): per-lease users (`user-create`, `run`,
  `user-delete`, `kill-uid`), the GUI session switch (`session-login`, `session-idle`),
  the leak scan (`scan`, `baseline`) and `reboot-dirty`, each restricted to the lease
  uid range.
- **Nothing a lease runs can call either helper**: no action, probe or VM runs under a
  uid in the helpers' group (Linux containers run with `--userns=nomap`; Macs run
  every action as a lease user), and every caller is checked as the genuine
  `kbf-daemon` (S4.3).
- **Auto-login is off at rest.** `kbf-daemon` is a LaunchDaemon and needs no login
  session, so there is no idle user. On every boot, before the daemon may send
  `Hello`, `kbf-mac-session` clears auto-login and removes any leftover lease user, so
  a Mac that lost power mid-lease never comes back logged in as a dead lease user.

## 6. Version skew and routing

During a rollout a pool is mixed. Three rules keep that safe:

- **Pinned work routes exactly.** An action that pins `os_build`, `xcode`,
  `os_image` or `vm.image` through its platform properties only matches nodes that
  report it (3.1). Unpinned work may land on either side.
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

- **A probe, per Xcode, report-only.** A client-defined command (a client's identity
  script is one) that the daemon runs once **for each pinned Xcode**, with that
  Xcode's `DEVELOPER_DIR`, and reports in `NodeStatus` as `probe.<name>` per Xcode
  build. The command and its expected value per Xcode come only from the set
  `kbf-updater` verified and installed, never from the server, and the probe runs as a lease-range user, never as the daemon's
  (S3.2). A probe is never a request key: clients
  route on `xcode` (plus `os_build` when they need it). When a probe's value for one
  Xcode differs from the expected value, the daemon stops reporting **that Xcode** in
  `xcode`, so work asking for it goes elsewhere; the node's other Xcodes keep
  serving, and the server alerts. kbf never names a client.
- **A client gate.** A rollout can stop at `awaiting_client` after the canary, until
  the operator confirms that the client accepts the canary's new probe value (and a
  set with the new expected value is signed). It alerts and times out into `held`
  (4.3).

The better fix belongs to the client: key actions on the `xcode` build they pin, not
on a list of host identities, so an update is one cold miss and a one-value change.

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

### 7.2 macOS updates with MDM (decided)

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

### 7.4 Xcode and the Metal toolchain

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
    into a read-only share served by macOS file sharing under a dedicated non-admin
    account: no kbf root listener, though macOS's own `smbd` runs as root, and lease
    users on the rack network can reach the share (read-only, the `.xip` is not
    secret from them); each Mac's `kbf-updater` pulls it
    and checks the set's SHA-256. No server or object store holds it (also for counsel
    **[A]**). With no licence question at all: a person downloads on each Mac.
  - The `.xip` never goes in a public bucket or in this repository.
- **Components:** the Metal toolchain is downloaded once with `-downloadComponent
  metalToolchain -exportPath` and imported on each host offline with
  `-importComponent`
  ([Apple](https://developer.apple.com/documentation/xcode/downloading-and-installing-additional-xcode-components))
  **[V]**. **Simulator runtimes are never installed on a host:** simulators need a GUI
  session, so they run in VM leases, and the runtimes are part of the golden VM image
  build (`macos-vms.md` section 7, with `-downloadPlatform ... -exportPath` and
  `-importPlatform` in the image build). Both are Apple Software under the same
  licence, so the same sign-off applies.
- **Install** (by `kbf-updater` running the profile): expand to
  `/Applications/Xcode-<build>.app` (`xip` checks Apple's signature), `xcodebuild
  -license accept`, `xcodebuild -runFirstLaunch`, import the Metal toolchain. That all
  of this works with no Apple Account signed in is **[A]**.
- **Side by side, selected per action.** Adding an Xcode is additive: no drain, no
  reboot. The daemon sets `DEVELOPER_DIR` for each action from its `xcode` platform
  property (#85 phase 1); it never switches the global `xcode-select`. The node
  reports every installed build in `xcode` (membership, 3.1). Old Xcodes are removed
  when no pool's set names them. First-launch packages may be shared by every
  installed Xcode, so a new Xcode can change what an older one uses
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
  **[V]**), the profiles of section 10.2, and the node's join credential, issued per
  device and only for serials in Apple Business Manager (S6).
- **Remote erase.** `EraseDevice` runs Erase All Content and Settings on Apple
  silicon, silently authorised by the bootstrap token
  ([erase](https://support.apple.com/guide/deployment/erase-devices-dep0a819891e/web))
  **[V]**. Send `ObliterationBehavior: DoNotObliterate`, so a failed preflight reports
  an error instead of falling back to a reinstall that needs a person
  ([device.erase.yaml](https://github.com/apple/device-management/blob/release/mdm/commands/device.erase.yaml))
  **[V]**. "Return to Service" is not available on macOS (same file) **[V]**, so the
  erased Mac comes back through ADE and Auto Advance. That this chain runs end to end
  with no person on 27 is **[A]**: one probe settles it. Every erase goes through the
  gate and carries an operator's hardware-key signature (M4).
- **Cost of a full re-provision** (erase, enrollment, profile, Xcode, VM images,
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
  **[V]**. ABM and Apple's push service (APNs) are closed Apple services every Mac MDM
  depends on; kbf cannot avoid them.
- **A self-hosted, open-source MDM.** NanoHUB (MIT) "unifies NanoMDM, NanoCMD, and
  KMFDDM" ([nanohub](https://github.com/micromdm/nanohub)), a DDM server included;
  NanoDEP ([nanodep](https://github.com/micromdm/nanodep)) and scep
  ([scep](https://github.com/micromdm/scep)) are MIT too; MicroMDM v1's support ended
  ([micromdm](https://github.com/micromdm/micromdm)) **[V]**. Fleet is open-core: its
  pricing table ticks "Enforce operating system (OS) updates" and "Send lock and wipe
  commands" for Premium only, the Free column empty
  ([pricing](https://fleetdm.com/pricing)) **[V]**.
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
  anchor certificate: the ADE profile's `anchor_certs` are used "as trusted anchor
  certificates when evaluating the trust of the connection to the MDM server URL"
  ([Profile](https://developer.apple.com/documentation/devicemanagement/profile))
  **[V]**.
- **Where it runs.** Under its own uid, never beside `kbf-server`; it may start on a
  host outside the build pool and move later (M7); where it settles is an open
  decision (S7). Its enrollment and check-in endpoints must be reachable on the rack
  network, not only on an overlay network, because a freshly erased Mac enrolls
  before any overlay client is installed (M6). Its API is not: only `kbf-mdm-gate`
  holds the API key.
- **`kbf-mdm-gate`** (decision: build it, S5). A small process beside the MDM that
  holds the API key and offers `kbf-server`, over mTLS pinned to the server's
  identity, only the verbs of M2.2: `status`, `enforce` (and `withdraw`) of a build
  a signed set names for that Mac's pool, `profile` (an allowlisted one),
  `grant-admin` (S5.2), and a relay for signed erase requests (with `bring-forward`
  of an erase already scheduled under one) that the server cannot create. **The
  server cannot make an erase:** the gate erases only on a request signed by an
  operator's hardware-backed key (M4), one Mac at a time within a daily cap
  (default 2). Every verb is limited to the gate's own inventory and keeps a Mac floor
  counted from MDM check-ins; every erase, enforcement, grant and profile install
  alerts natively, without the server.
- **kbf stays generic.** kbf ships no MDM. The gate drives the MDM through the
  `MdmBackend` trait (crate `kbf-mdm`, which also holds the gate): NanoHUB first, a
  hosted MDM with a separate wipe privilege optional (M5). Nodes map to enrollments by
  the serial and platform UUID the daemon reports. The MDM decides nothing.

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
  means "a newer approved snapshot exists". Package integrity rests on apt's archive
  signature; the set pins which snapshot and kernel (S3.1).
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

fwupd's UEFI capsule plugin installs on the next reboot
([uefi-capsule](https://fwupd.github.io/libfwupdplugin/uefi-capsule-README.html))
**[V]**. Firmware is its own set kind: manual approval, canary, soak, concurrency 1,
and only on nodes whose BMC is known to work, since a failed flash needs the BMC or a
person. Microcode rides the normal rollout as packages **[A]**.

## 9. VM images as rolled-out software

A golden VM image (`macos-vms.md` section 7), built in CI on a Mac from a pinned
restore image, Xcode and simulator runtimes and named by digest, is part of a pool's
set. `kbf-updater stage` places it ahead of time and the node reports it in
`vm.image`; switching the set makes VM leases ask for the new `vm.image`, with no
reboot. A guest cannot run a newer macOS than its host **[A]**, so hosts move first.

## 10. GPU work and desktop-app tests on a bare-metal Mac

### 10.1 No GPU in VMs

On Apple silicon there is no GPU passthrough; a macOS guest gets a paravirtual Metal
device. One published measurement on an M1 Ultra found a stock guest running LLM
inference at about 4-14% **of** bare-metal speed (7 to 23 times slower): generation
4.4% (TinyLlama 1.1B) and 6.5% (Gemma 4 12B), prompt processing 8.9% and 13.8%
([measurements](https://github.com/trycua/cua/blob/main/blog/gpu-passthrough-macos-vms.md))
**[V]** for their machine, **[A]** for others. The same source's faster guest depends
on injecting a library to change private Metal behaviour; not fit for a farm.

So a GPU test, including a test that installs the desktop app and drives it with a
local LLM, is a `kbf-lease=whole_machine` lease with `gpu=1`, as `macos-vms.md`
section 8.1 defines: it empties the Mac, no other lease (bare-metal or VM) runs there
at the same time, and GPU tests on one Mac run one at a time (the front refuses `gpu`
on macOS for any other lease kind, #85 section 8.1). **Planned:** the
whole-machine booking (all of the node's `cpus`, memory, `gpus` and both `vms` slots)
and the reservation that keeps one-core work from starving it (`macos-vms.md` section
5.3), and the runtime that serves it, below. VMs remain for simulator and GUI tests
that do not need the GPU.

**The bare-metal whole-machine runtime** (planned, in `kbf-driver-native`) is the
runtime #85 points to for `whole_machine` leases on a Mac. It reports its own
`drivers` value, `native-whole-machine` (#85 section 5.2), listed only when
`kbf-mac-session` is present, so a Mac without it never receives a `whole_machine`
lease. Per lease it: asks `kbf-mac-session` for a fresh lease user; if the lease needs
a GUI session, moves the node through `preparing`, where the session switches to that
user; starts the action as that user (`run`) inside the lease's sandbox profile; and
moves the node through `restoring`: the user is killed and deleted and the leak scan
runs. Its layers are next.

### 10.2 Isolation layers

macOS has no namespaces or cgroups. The isolation available is separate users, what a
non-admin user cannot change, a scan for what changed anyway, and an erase. The system
volume is a sealed, signed snapshot
([signed system volume](https://support.apple.com/guide/security/signed-system-volume-security-secd698747c9/web))
**[V]**, so whatever a lease leaves behind is on the Data volume, which is what the
scan covers.

| Layer | When | What | Time **[A]** |
|---|---|---|---|
| L0 | every Mac lease | a fresh non-admin user `kbf-lease-<lease id>` with no secure token, made by `kbf-mac-session`; the app installed into that user's own Applications folder or run from the action directory; model weights read from a root-owned, read-only model store filled from the CAS | ~10 s |
| L1 | once per Mac, at provisioning | `automationmodetool` run as root by the provisioning profile; Setup Assistant panes skipped for new users (MDM profile); Full Disk Access for `kbf-mac-session` (MDM profile); managed login items for kbf's own services (MDM) | 0 per lease |
| L2 | every whole-machine lease that needs a GUI session, in `preparing` and `restoring` | auto-login into the lease user, then a userspace restart (or a full reboot); at the end auto-login is cleared and the same restart returns the Mac to the login window | 1-4 min each way |
| L3 | every Mac lease: cleanup; whole-machine leases: also the scan | cleanup (kill the uid, delete the user, sweep its state, S4.2); for whole-machine leases a leak scan against the node's baseline | 30-60 s |
| L4 | a leak; always after a privileged lease | after a leak: reboot and scan again, and if still dirty quarantine and alert; an operator signs the erase (M4); re-provision, re-qualify. After a privileged lease: straight to the erase the operator signed before the lease (M4.4) | 45-90 min |
| L5 | about monthly per Mac, one at a time | an operator-signed erase anyway: proves the repair path works and resets state no scan sees | 45-90 min |

Overhead per desktop-app lease without a leak: about 3 to 9 minutes **[A]** (two
session switches plus cleanup and scan), small next to an LLM test.

Notes on each layer:

- **L0.** A non-admin user cannot write `/Applications`, `/Library/LaunchDaemons`,
  system extensions or system settings **[A]** (standard macOS behaviour). A test that
  must exercise the real `.pkg` installer into `/Applications` is a **privileged
  lease** (`kbf-mac-admin=true`, a planned reserved key in
  [capabilities.md](capabilities.md)), the only lease whose user may be an
  administrator. An administrator can reach root, and no later scan can prove a clean
  state against root, so **a privileged lease always ends in an erase** (L4's erase,
  45-90 minutes), with the node's identity revoked and quarantined first, and it needs
  an explicit client permission, spare erase budget and an operator-signed erase for
  that Mac held by the gate before the lease is placed (S8, M4.4). Every other lease user is
  non-admin. App teams should ship a per-user install path for most tests. The MDM's
  managed administrator (7.5) is never a lease user.
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
  preparation timeout is quarantined and the lease requeued. In `restoring` auto-login
  is cleared and the Mac returns to the login window; at rest no user is logged in
  (S4.2). So each whole-machine lease with a GUI session pays two switches, 1-4 minutes each **[A]**. Auto-login
  needs FileVault off (#76); its stored login is obfuscated, not secret, which is
  acceptable only for a random, non-admin user deleted after the lease. A userspace
  restart into auto-login on 27 is **[A]**; the fallback is a full reboot.
- **L3, the scan** (`kbf-mac-session scan`, with Full Disk Access). Each item is
  diffed against a baseline recorded at qualification: users and uids in the lease
  range; launchd jobs and the launch agent and daemon folders (names and hashes);
  background tasks (`sfltool dumpbtm`
  ([Apple](https://support.apple.com/guide/deployment/manage-login-items-background-tasks-mac-depdca572563/web))
  **[V]**); system extensions and loaded kexts; profiles; package receipts;
  `/Applications` with code-signature hashes; system privacy database rows; firewall
  application list, DNS, proxies and the hosts file; System keychain certificates;
  mounts; files owned by the lease uid in shared places; logins of the managed
  administrator; free disk; the Xcode and Metal toolchain set; that auto-login is off.
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
| Developer tools | Xcode builds and probe values per Xcode; VM images (with their simulator runtimes) | |
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
- **Erase** on a Mac row: shows the `kbf-admin erase` command the operator runs and
  signs with their security key; the server only relays the signed request (M4.2).

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
Roles (`admin`, and CI's short-lived `rollout`, which sends only `target` and a pool
`selector`) are in S9; strategy overrides such as `accept_outage` are admin-only.

- `GET /v1/nodes`: per node, the observed state, the pool's set, available updates
  and update state. The UI's JSON twin; clients and scripts read it.
- `GET /v1/software`: signed sets per pool, and upstream releases with their source.
- `POST /v1/rollouts`: `selector` (nodes, or one or more pools), `target` (a set
  digest or `latest`), `strategy` (`canary`, `max_unavailable`, `min_serving`,
  `soak`, `window`, `accept_outage`).
- `GET /v1/rollouts/{id}`; `POST /v1/rollouts/{id}:pause`, `:resume`, `:skip`,
  `:cancel`, `:rollback`. Progress goes to the event stream.
- `POST /v1/macs/{serial}:erase`: body is an operator-signed erase request (M4.2),
  forwarded to the gate unchanged.

## 12. Remote recovery without a BMC

Mac Studios have no BMC. What replaces it:

| Problem | Remote fix | Needs |
|---|---|---|
| Hung Mac | power-cycle through a switched PDU outlet; `pmset autorestart 1` boots it on power return | a switched PDU **[A]** |
| Hung Mac, no PDU | Lights Out Management: an enrolled controller Mac starts, stops or restarts it; Apple lists Mac Studio models ([LOM](https://support.apple.com/guide/deployment/lights-out-management-payload-settings-dep580cf25bc/web)) **[V]** | MDM and one controller Mac on the same subnet over Ethernet (IPv6) **[V]**; a second controller is a redundancy choice, not Apple's requirement; that it resets a panicked Mac is **[A]** |
| Hung Mac, cabled neighbour | `macvdmtool reboot` restarts the target Mac over a USB-C cable from a neighbour ([macvdmtool](https://github.com/AsahiLinux/macvdmtool)) **[V]**, unofficial | a cable to the right port; which port on a Mac Studio is **[A]** (the README lists ports for MacBook Air, MacBook Pro and Mac mini only) |
| Bad software, leak, drift | operator-signed erase through the gate and automatic re-provision (7.5, M4) | MDM, ABM, an operator's touch |
| Will not boot, firmware failure | DFU restore from a cabled neighbour Mac (`macvdmtool dfu`, then `cfgutil restore`) **[A]** for Mac Studio, unofficial | a cable in the DFU port; moving it needs a person |
| Hardware | none | a person |

Entering DFU by hand needs the power button held while plugging in power
([Apple](https://support.apple.com/en-us/108900)) **[V]**. There is no supported remote
way into recoveryOS **[A]**.

Linux: the BMC (Redfish or IPMI) where the board has one; bootc with greenboot-rs and
a watchdog for software that does not boot; a switched PDU otherwise.

**What still needs a person:** the "Hands" row of section 1, plus firmware recovery
without a BMC; an Xcode download per Mac if counsel rejects both mirror and copies.

## 13. Phased plan

| Phase | Delivers |
|---|---|
| P0 probes | on one Mac: DDM enforcement with a near deadline on 27; erase then ADE and Auto Advance; `automationmodetool` as root and across user deletion; Screen Recording need of XCUITest and whether a grant persists across users; userspace restart into auto-login; `DEVELOPER_DIR` per action. On one Linux node: snapshot upgrade; cgroup delegation after reboot; BMC and watchdog presence. Legal: the Xcode licence questions of 7.4 |
| P1 read-only | software keys in the report with membership matching, re-detection and `Report`; `NodeStatus`; `/v1/nodes`; Fleet page with update available; cordon and drain in protocol and scheduler; `kbf-updater` for `kbf-daemon` and the profile only, on Linux once containers run with `--userns` (kernel 6.5 or later), and on Macs once actions run as lease users; `kbf-mac-session`'s `user-create`, `run`, `kill-uid` and `user-delete` on Macs (S4.2, S4.3); signing keys in a KMS under an offline root key (S2) |
| P2 Linux rollouts | durable rollout record; rollout object, canary, soak, qualification, `min_serving`, `accept_outage`, halt, startup reconciliation; Linux updates by snapshot; CI-started rollouts of kbf's own components; Update buttons for Linux |
| P3 Mac rollouts | the MDM host decided (S7); ABM, MDM and `kbf-mdm-gate` live while the Macs are wiped (M9); operators' security keys in the gate's allowed signers (M4); per-device join credentials and identity binding (S6); macOS and Xcode rollouts; probes and the client gate; Update buttons for Macs |
| P4 bare-metal GPU and desktop-app leases | `kbf-mac-session`'s session switch, scan and baseline; the bare-metal whole-machine runtime with per-lease users, `preparing`/`restoring` and the leak scan with planted-leak tests; L4/L5 erase; then golden VM images as software sets |
| P5 image-based Linux | bootc with greenboot-rs and a watchdog on new servers, then the rest one at a time |

Changes by crate (rough sizes, tests included):

| Crate | Change | Lines |
|---|---|---|
| `kbf-proto` | `NodeStatus`, `Report`; `Drain`, `Update` and `Prepare` server messages | ~200 |
| `kbf-daemon`, `kbf-node` | on Macs a TLS signer through Security.framework with the keychain identity; re-detect and send `Report`; new keys; probes from config or set; update marker and `Update`/`Start` refusals; forward to the updater; `DEVELOPER_DIR` per action | ~800 |
| `kbf-updater` (new) | socket and caller check, key statements, set checks of S3.1, profile / package / bootc backends, state file | ~2,100 |
| `kbf-mac-session` (new) | socket and caller check, the verbs of S4.2, sweep, leak scan | ~1,700 |
| `kbf-driver-container` | `--userns=nomap`, so no container uid is the daemon's; the overlay handed to the container's root and back; the subordinate id check at startup | ~300 |
| `kbf-mdm` (new) | `MdmBackend`; NanoHUB backend; DDM status; `kbf-mdm-gate` with mTLS, inventory, caps, the profile allowlist, signed-erase verification and its own alerts; `kbf-admin erase` | ~1,500 |
| `kbf-types`, `kbf-sched` | node states; cordon in placement; rollout state machine; slots; `min_serving` by hardware class; `accept_outage`; internal submitter for `kbf-node`; whole-Mac reservation | ~1,900 |
| `kbf-server` | rollout driver, durable record and startup reconciliation, roles and OIDC tokens, identity binding and revocation, audit, `/v1/nodes`, `/v1/software`, `/v1/rollouts`, Fleet page, absence suppression | ~1,500 |
| `kbf-caps`, `kbf-front` | membership comparison; new exact and set keys, `probe.*`; `kbf-node` refused from clients | ~300 |
| `kbf-alert` | rollout held, client gate waiting, node not back, certificate, token and catalogue expiry, Mac awaiting a signed erase | ~200 |
| `kbf-driver-native` | the bare-metal whole-machine runtime: lease users through `kbf-mac-session`, GUI session, cleanup | ~1,500 |
| `kbf-sim`, `kbf-it` | tests below | ~1,000 |

Tests, each with the planted mutant that must turn it red (the security tests are in
S10):

| Test | Mutant |
|---|---|
| A rollout never drops a class below `min_serving` (simulation over seeds) | ignore `min_serving` in the pre-gate: the last arm64 node is drained |
| A single-node class updates only with `accept_outage`; a software key never forms a class | include `xcode` in the class: the rollout never starts |
| A failed canary holds the rollout; no second node is touched | release the slot on failure |
| Two concurrent rollouts in one pool never exceed `max_unavailable` | one semaphore per rollout |
| A class member dying during a drain uncordons the draining node | check `min_serving` only before cordon |
| `Update` refused while a lease is live or in the 40 s fence window; `Start` refused while the update marker is set | drop the lease check |
| A server restart with an outstanding MDM enforcement never places work on that Mac, and withdraws it | clear the update marker on reconnect: a lease starts before the forced install |
| A leader change mid-rollout neither repeats nor skips a step | keep rollout state outside the durable record |
| A Mac reporting two Xcode builds handshakes and matches an action asking for either | keep `xcode` exact: the report is refused as repeated |
| The front refuses `kbf-node` from a client | accept it at the front |
| A node that returns with the wrong build is quarantined | accept any `Hello` |
| A planted leak per scan item turns the leak scan red | drop that scan item |
| A Mac powered off mid-lease boots with auto-login off and no lease user before `Hello` | skip the boot reset: it comes up logged in as the dead lease user |
| A probe mismatch for one Xcode drops only that Xcode from `xcode` | drop the whole node |

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
| An own MDM vendor certificate needs Enterprise Program membership; NanoHUB's API covers what the gate needs (and the MDM assumptions of M10) | reading the references, before P3 |
| No supported unattended Data-volume snapshot revert; Gatekeeper caches per code hash; lease uid range free | P4 |
| Per-lease overhead 3-9 min; re-provision 45-90 min; return deadlines 15 and 60 min | P2-P4 measurements |
| LOM resets a panicked Mac; `macvdmtool` ports on a Mac Studio; a switched PDU exists | P3 |
| VM LLM speed on other Macs; a guest cannot run a newer macOS than its host | #85 phases 2-3 |
| The reference deployment runs Ubuntu 24.04; which boards have a BMC and a watchdog; arm64 bootc images exist; microcode rides the rollout | P0, before P5 |
