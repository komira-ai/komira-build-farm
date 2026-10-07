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
`mac-node-provisioning.md` (the Mac provisioning profile) and
[macos-vms.md](macos-vms.md) (VMs and the GPU on Mac nodes).

Claims about other projects and Apple's software are marked **[V]** (read in the
linked source) or **[A]** (an assumption, or a number nobody has measured on our
nodes). Section 14 collects them.

## 1. Summary

| Topic | Design |
|---|---|
| Principle | Once a daemon completes its handshake, `kbf-server` owns that node's software lifecycle. Updates are rolling: the farm never goes offline as a whole, and a platform never drops below its floor. |
| State | Each node reports what it runs (observed). Each pool has a signed *software set* (desired). "Update available" is computed by the server, never by the node. |
| Rollout | cordon, drain, apply, reboot, re-handshake, qualify, uncordon; one node at a time per pool by default; a canary and a soak first; any failed gate halts the rollout and alerts. |
| Who applies | A small root helper, `kbf-updater`, with five fixed verbs, that installs only manifests signed by the operator's CI. `kbf-daemon` stays unprivileged. macOS itself is updated through MDM, because Apple leaves no other supported unattended path on macOS 27. |
| Mac | Apple Business Manager plus a self-hosted open-source MDM (NanoHUB). Xcode and simulator runtimes come from the operator's private store, side by side, selected per action. |
| Linux | No MDM exists or is needed. v1: the distribution's packages pinned to a dated archive snapshot. Target: an image-based OS (bootc) with automatic rollback. |
| GPU | No GPU in VMs. GPU tests, including desktop-app plus local-LLM tests, lease the whole bare-metal Mac, one at a time. |
| App isolation | A throwaway non-admin user per lease, a leak scan against a baseline, a reboot on doubt, and a remote erase and automatic re-enrollment when a leak persists. |
| UI | A Fleet page: per worker OS, build, kernel or Xcode, update available, state. **Update** per worker; **Update all Linux workers** and **Update all Mac workers** start rolling updates. |
| Hands | Still needed for: enrolling Macs bought before the organisation existed (once each), yearly certificate renewals, one Xcode download per release, a Mac that will not boot, and hardware. |

## 2. The principle

A node joins by opening the worker stream and completing the handshake
([worker-protocol.md](worker-protocol.md)). From then on:

- **The server decides what the node runs and when it changes.** The node never
  updates itself. Every automatic update mechanism of the OS is turned off, so a node
  only changes when a rollout tells it to.
- **The server is the right place.** It already knows what runs where, so it can drain
  a node without losing work, keep enough nodes of each platform serving, and check
  the node afterwards with real work. A per-node cron job can do none of that.
- **Rolling, never everything.** A rollout takes nodes out one at a time (or a small,
  bounded number), and stops at the first failure. A bad update costs one node.
- **Signed, not clicked into existence.** The UI chooses *when* and *where*. *What* is
  installed is a software set that the operator's CI built and signed from a reviewed
  change. A compromised server can choose among signed sets, nothing more.

## 3. Node software state

### 3.1 Observed

What a node runs, reported by `kbf-daemon`. Two kinds:

**Matchable keys** go into the node report ([capabilities.md](capabilities.md)), so
actions can route on them. Today the report has `arch`, `os`, `cpus`, `mem_gib`,
`page_size`, `gpu`, `isa_level`, `cpu.features`, `drivers` and `cpu.model`; the
matcher already compares `os_image` and `xcode` exactly, but no daemon reports them.
New keys:

| Key | Platform | Source |
|---|---|---|
| `os_version`, `os_build` | both | `sw_vers`; `/etc/os-release` |
| `kernel` | Linux | `uname -r` |
| `os_image` | Linux on bootc | the booted image digest |
| `xcode` (repeated) | Mac | every installed Xcode build |
| `sim_runtime` (repeated) | Mac | installed simulator runtimes |
| `vm_image` (repeated) | Mac | golden VM images present on the node |
| `probe.<name>` | both | an operator-configured command (section 6.1) |

The daemon re-detects these periodically and resends `Hello` when one changes, as it
does for capabilities today.

**Status fields** do not route work, so they go in a new `DaemonMessage::NodeStatus`
kept out of the report hash: the `kbf-daemon` commit, the updater's version and state,
the software set it last applied, `reboot_required`, last boot time, the hardware
serial and platform UUID (to join with the MDM's record), and, where readable,
firmware versions.

### 3.2 Desired: the software set

A *software set* is a manifest per pool. It names, each with a SHA-256 or digest:

- Mac: the macOS version and build; the Xcode builds and their `.xip` digests; the
  simulator runtimes and Metal toolchain components; the provisioning profile version;
  the VM images.
- Linux: the archive snapshot and expected kernel package, or the host image digest.
- Both: the `kbf-daemon` and `kbf-updater` artifacts.

It carries a monotonic serial and an ed25519 signature made by the operator's CI from
a reviewed change. The server stores the set's digest in the scheduler's control log,
so it survives a leader change once Raft is wired.

**Update available** is computed by the server:

- the node's observed state differs from its pool's current set; or
- a newer upstream release exists than the pool's set. Upstream means Apple's public
  software catalogue for macOS ([gdmf.apple.com/v2/pmv](https://gdmf.apple.com/v2/pmv),
  the source Apple names for device-management services
  ([schema](https://github.com/apple/device-management/blob/release/declarative/declarations/configurations/softwareupdate.enforcement.specific.yaml)))
  **[V]**, Apple's developer releases feed for Xcode
  ([releases.rss](https://developer.apple.com/news/releases/rss/releases.rss)) **[V]**,
  and the operator's image pipeline or archive snapshots for Linux.

An upstream release does not become a button. It becomes a prompt to build and sign a
new set. Only signed sets are offered for install.

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

## 4. The rollout protocol

### 4.1 The rollout record

| Field | Meaning | Default |
|---|---|---|
| `target` | the software set digest | the pool's newest signed set |
| `selector` | which nodes: a list, or a platform (`os=macos`; `os=linux,arch=x86_64`) | |
| `max_unavailable` | nodes of one pool out at once | 1 |
| `min_serving` | live, uncordoned nodes kept per capability class | max(1, 25%) |
| `canary` | nodes updated first | 1 |
| `soak` | wait after the canary before the rest | 2 h |
| `qualify` | the qualification job (4.3) | per platform |
| `drain_deadline` | how long running leases may finish | 30 min |
| `window` | optional maintenance window | none |
| `state` | `pending`, `running`, `held`, `done`, `cancelled` | |
| `actor` | who started it, and when | |

A rollout is a server job. Its state and the slots it holds live in the scheduler's
control log, and every step is idempotent, so a server restart or leader change
continues or halts it, never repeats a half-done step blindly.

### 4.2 Per node

1. **Pre-gate.** No firing alerts. `min_serving` would still hold for every capability
   class the node belongs to: the last arm64 node, the last GPU Mac, or a member of a
   storage or consensus quorum while another member is down are never taken. Inside
   the window. For a node that runs a server role: object-store healing complete, all
   consensus voters healthy, and leadership transferred away first.
2. **Take the slot.** A counting semaphore per pool, owned by this rollout and node,
   with no time-based expiry.
3. **Cordon.** Placement skips the node.
4. **Drain.** A new `ServerMessage::Drain{deadline}` (the planned drain message in
   [worker-protocol.md](worker-protocol.md)). `batch` work is preempted at once and
   requeued. Other leases, including whole-Mac leases, run to completion. At the
   deadline the rollout **holds** rather than killing work.
5. **Apply.** A new `ServerMessage::Update{rollout, step, set_digest}` on the existing
   stream, so no node needs an inbound port. The daemon forwards it to `kbf-updater`
   (section 5), or, for macOS itself, the server asks the MDM (section 7.2).
6. **Reboot,** when the update needs one.
7. **Re-handshake.** The daemon reconnects within the return deadline, and its `Hello`
   and `NodeStatus` must show the target set. The node's own report is the done
   signal, not the MDM's or the updater's. Anything else quarantines the node.
8. **Qualify.** Startup checks pass (on Linux: cgroup v2, delegated controllers, PSI).
   The qualification job runs as real leases pinned to the node with a reserved key
   `kbf-node=<id>`, accepted only from operator-defined jobs. Mac: a Swift and Xcode
   build, one GUI lease, and the GPU golden test of
   [macos-vms.md](macos-vms.md) section 8.2. Linux: a container build and the
   per-node-class probe suite. Mac whole-machine nodes also record a new leak-scan
   baseline (section 10.3).
9. **Uncordon and release the slot.** Next node. After the canary, soak first.

### 4.3 Failure, halt and rollback

- Any failed gate moves the node to `held` and the rollout to `held`, and fires a
  native alert. **Nothing proceeds by itself.** The slot is not released: the broken
  node holds the only slot, so the rollout cannot reach a second node.
- Operator actions: **resume**, **skip node** (mark it failed and release its slot),
  **cancel**, **roll back**.
- Roll back where the platform supports it: Linux on bootc (`bootc rollback`), Linux
  kernels (a one-time boot into the previous kernel), Xcode, simulator runtimes, VM
  images, the profile and `kbf-daemon` (all side by side). **A macOS update cannot be
  rolled back**; the repair is an erase and re-provision at the pool's set (section
  7.5). The UI says this before a macOS rollout starts.

### 4.4 Prior art

The shape follows Kured ([kured](https://kured.dev/docs/configuration/)), FleetLock
([protocol](https://coreos.github.io/zincati/development/fleetlock/protocol/)) and
Borg ([Verma et al.](https://research.google.com/pubs/archive/43438.pdf)) **[V]**: a
coordinator-held counting lock per group, concurrency 1, gates before and after, and a
failure keeps the lock. kbf is simpler: it is the scheduler, so drain is a scheduler
state and leftover work is requeued under the fence rules kbf already keeps.

## 5. Who applies a change on the node

### 5.1 Options

| Option | What it is | Fit |
|---|---|---|
| A. Root helper | `kbf-updater`, a small root service with fixed verbs, verifying signed manifests | both OSes; one code path; the daemon stays unprivileged |
| B. Provisioning profile | re-run the idempotent profile (`kbf-mac-provision apply`) as root | Mac settings, Xcode, runtimes; not the OS itself |
| C. MDM | Apple's device-management protocol | the only supported unattended macOS update path on macOS 27 (7.2); Apple only |
| D. Image-based OS | the host OS is a container image (bootc), staged A/B | Linux; atomic, with rollback |

**Recommendation:** A as the single entry point on every node. It runs B for Mac
settings and developer tools, D (or pinned packages) for Linux, and kbf-daemon
upgrades. macOS itself goes through C, ordered by the server, because nothing on the
node can authorise it without a stored password.

### 5.2 `kbf-updater`

- A LaunchDaemon on Macs, a systemd unit on Linux. It listens on a Unix socket owned
  `root:<kbf role group>`, mode 0660. Only `kbf-daemon`'s role account can reach it.
- Five verbs: `status`, `stage <set>`, `apply <set>`, `reboot`, `rollback`.
- It fetches the manifest and artifacts by digest from the farm's CAS, verifies the
  ed25519 signature against a public key pinned at provisioning, checks every
  artifact's SHA-256, and refuses a serial lower than the installed one unless the
  manifest is marked as a rollback.
- It keeps a state file (last applied set, in-progress step), so a crash or reboot
  mid-apply resumes or reports, never guesses.
- A compromised `kbf-daemon` or server can at worst install a set CI signed. A
  compromised signing key is the threat that matters; the key is held by the
  attested build, not by any node or server.

## 6. Version skew and routing

During a rollout a pool is mixed. Three rules keep that safe:

- **Pinned work routes exactly.** An action that pins `os_build`, `xcode`,
  `os_image` or a probe value through its platform properties only matches nodes
  that report it ([capabilities.md](capabilities.md)). Unpinned work may land on
  either side.
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

- **A probe.** The operator configures a named command in the cell config; the daemon
  runs it and reports `probe.<name>`. A client's identity script is one probe. kbf
  never names a client.
- **A client gate.** A rollout can stop at `awaiting_client` after the canary, until
  the operator confirms that the client accepts the canary's new probe value.

The better fix belongs to the client: route on one exact property per action
(`probe.host_identity=<value>`) instead of keying on a list. An update is then one cold
miss and a one-value change. The rollout order follows: update half the pool, flip
the value, then the old side drains of work and updates quickly.

## 7. The Mac path

### 7.1 What Apple changed

On macOS 27 (our pinned major version) the old update controls are gone:

- The `com.apple.SoftwareUpdate` profile payload: deprecated 26.0, removed 27.0
  ([schema](https://github.com/apple/device-management/blob/release/mdm/profiles/com.apple.SoftwareUpdate.yaml))
  **[V]**.
- The MDM commands `ScheduleOSUpdate`, `AvailableOSUpdates` and `OSUpdateStatus`:
  deprecated 26.0, removed 27.0 for macOS
  ([schedule](https://github.com/apple/device-management/blob/release/mdm/commands/system.update.schedule.yaml),
  [available](https://github.com/apple/device-management/blob/release/mdm/commands/system.update.available.yaml),
  [status](https://github.com/apple/device-management/blob/release/mdm/commands/system.update.status.yaml))
  **[V]**.
- What remains is declarative device management (DDM). Whether the local preference
  keys a script writes are still honoured on 27 is **[A]**, to test on a node.

### 7.2 macOS updates with MDM (recommended)

- **Block automatic updates.** DDM software-update settings set automatic download and
  install `AlwaysOff` and defer major, minor and system updates 1 to 90 days
  ([settings](https://github.com/apple/device-management/blob/release/declarative/declarations/configurations/softwareupdate.settings.yaml))
  **[V]**. Background Security Improvements "can still be installed ... independent of
  the Enable key"
  ([Apple](https://support.apple.com/guide/deployment/software-update-settings-declarative-dep0578d8b8a/web))
  **[V]**, and each changes the OS build. So a node's build can still move without a
  rollout; the daemon reports it, and the server treats it as drift (section 3.2).
- **Install a chosen update.** The server drains the node, then asks the MDM to post
  `softwareupdate.enforcement.specific` for that one device: `TargetOSVersion`,
  `TargetBuildVersion` and a `TargetLocalDateTime` a few minutes ahead. At the
  deadline "the device force installs it". It needs macOS 14 or later, supervision and
  system scope
  ([schema](https://github.com/apple/device-management/blob/release/declarative/declarations/configurations/softwareupdate.enforcement.specific.yaml))
  **[V]**. Whether a deadline a few minutes ahead installs at once on a drained node,
  and whether major upgrades enforce the same way, is **[A]**.
- **No password.** On Apple silicon the bootstrap token authorises the update; on
  macOS 26 and later it is created at enrollment, so no user login is needed
  ([bootstrap token](https://support.apple.com/guide/deployment/use-secure-and-bootstrap-tokens-dep24dbdcf9e/web),
  [enforcement](https://support.apple.com/guide/deployment/install-and-enforce-software-updates-depd30715cbb/web))
  **[V]**.
- **Watch it.** DDM status items report install state, pending version and failure
  reason ([status](https://github.com/apple/device-management/tree/release/declarative/status))
  **[V]**. The server shows them in the UI while the node is silent.
- **A pin means "this build, while Apple signs it" [A].** Apple's catalogue lists each
  version's posting and expiry dates; the UI shows them.

### 7.3 macOS updates without MDM (fallback only)

`softwareupdate --install <label> --restart --user <admin> --stdinpass` exists; Apple's
manual marks `--user` and `--stdinpass` "Apple silicon only"
([softwareupdate(8)](https://keith.github.io/xcode-man-pages/softwareupdate.8.html))
**[V]**. A root job on the node then holds an admin password as a secret. Whether it
works from a background service with nobody logged in on 27 is **[A]**. There is also
no unattended re-setup after an erase, no remote power control and no supported way
to block updates. This path is for the time before MDM is live, not a design.

### 7.4 Xcode, simulator runtimes and the Metal toolchain

- **Download: a person, once per Xcode release.** The `.xip` needs an Apple Developer
  sign-in, and automated two-factor sign-in is unreliable
  ([xcodes#326](https://github.com/XcodesOrg/xcodes/issues/326)) **[V]**. The operator
  downloads it once, and CI stores it in a **private** object store under its
  SHA-256. The Xcode licence allows "a reasonable number of copies" on Apple computers
  "owned or controlled by You" for internal use
  ([Xcode SLA](https://www.apple.com/legal/sla/docs/xcode.pdf)) **[V]**; that a private
  internal mirror fits this is the operator's reading, not legal advice **[A]**. The
  `.xip` never goes in a public bucket or in this repository.
- **Runtimes and components:** downloaded once with `xcodebuild -downloadPlatform ...
  -exportPath` and `-downloadComponent metalToolchain -exportPath`, installed on each
  node offline with `-importPlatform` and `-importComponent`
  ([Apple](https://developer.apple.com/documentation/xcode/downloading-and-installing-additional-xcode-components))
  **[V]**.
- **Install** (by `kbf-updater` running the profile): expand to
  `/Applications/Xcode-<build>.app` (`xip` checks Apple's signature), `xcodebuild
  -license accept`, `xcodebuild -runFirstLaunch`, import runtimes. That all of this
  works with no Apple Account signed in is **[A]**.
- **Side by side, selected per action.** Adding an Xcode is additive: no drain, no
  reboot. The daemon sets `DEVELOPER_DIR` for each action from its `xcode` platform
  property; it never switches the global `xcode-select`. Old Xcodes are removed when
  no pool's set names them. First-launch packages and simulator runtimes may be shared
  by every installed Xcode, so a new Xcode can change what an older one uses **[A]**;
  the canary's qualification checks this.
- **Order.** Each Xcode sets a minimum macOS. A set whose Xcode needs a newer macOS
  updates macOS first, in the same rollout step; the UI shows the dependency.
- **Never from the App Store,** which offers only the newest Xcode and so cannot pin
  a version **[A]**.

### 7.5 Enrollment, erase and re-provision

- **Zero-touch setup.** Automated Device Enrollment with Auto Advance skips every Setup
  Assistant pane on a Mac with Ethernet, macOS 11 or later
  ([ADE](https://support.apple.com/guide/deployment/automated-device-enrollment-management-dep73069dd57/web))
  **[V]**. After enrollment the MDM installs the provisioning package (DDM
  `com.apple.configuration.package` on macOS 26 and later
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
  with no person on 27 is **[A]**: one probe settles it.
- **Cost of a full re-provision** (erase, enrollment, profile, Xcode and runtimes from
  the store, cache refill): about 45 to 90 minutes **[A]**. A repair step, not a
  per-lease step.
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
- **A self-hosted, open-source MDM.** NanoHUB (MIT) "unifies NanoMDM, NanoCMD, and
  KMFDDM" ([nanohub](https://github.com/micromdm/nanohub)) **[V]**, so it includes a
  DDM server. NanoDEP (MIT) speaks the enrollment API
  ([nanodep](https://github.com/micromdm/nanodep)) **[V]**; micromdm/scep (MIT) issues
  enrollment certificates ([scep](https://github.com/micromdm/scep)) **[V]**.
  MicroMDM v1 is in maintenance mode with support ended
  ([micromdm](https://github.com/micromdm/micromdm)) **[V]**. Fleet keeps OS-update
  enforcement and wipe in its paid tier
  ([pricing](https://fleetdm.com/pricing)) **[V]**, which does not fit an
  open-source-only farm.
- **An APNs push certificate,** renewed yearly by a person under the same Apple
  account, from a CSR signed by an MDM vendor
  ([micromdm certificates](https://micromdm.io/blog/certificates/)) **[V]**. The
  ABM server token also expires. Both expiries are native alerts at 30 and 7 days.
- **Where it runs.** As a service on the farm's Linux nodes. It must be reachable on
  the rack network, not only on an overlay network, because a freshly erased Mac
  enrolls before any overlay client is installed. Its bootstrap tokens can erase every
  Mac, so its API is reachable only by `kbf-server` and its store is treated as a
  secret.
- **kbf stays generic.** kbf ships no MDM. `kbf-server` talks to one through an
  `OsUpdateBackend` trait (crate `kbf-mdm`); the first implementation targets
  NanoHUB's API **[A]** (to read before coding). kbf maps node to enrollment by the
  serial number and platform UUID the daemon reports. The MDM decides nothing; the
  rollout engine does.

## 8. The Linux path

There is no Apple-style MDM on Linux, and none is needed: root on the node can already
do everything. The questions are which agent applies the update and who orders it.
Here `kbf-server` orders it and `kbf-updater` applies it. Products that call
themselves Linux device management do not fit: Canonical Landscape's free self-hosted
tier is for up to 10 machines for personal or evaluation use
([Landscape](https://ubuntu.com/landscape)) **[V]**, and Fleet is open-core (above).

### 8.1 v1: pinned packages

- Keep the current distribution (Ubuntu 24.04 LTS on our nodes **[A]**, inferred from
  the kernel). Turn unattended upgrades off.
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
- Rollback is weak: a one-time boot into the previous kernel covers a kernel
  regression; a userland rollback is a downgrade to the previous snapshot, not atomic.
  A node that does not boot needs its BMC or a person.

### 8.2 Target: an image-based host OS (bootc)

- `bootc upgrade` stages a new image A/B style, `--apply` reboots into it, and
  `--check` only reads metadata
  ([bootc-upgrade](https://bootc.dev/bootc/man/bootc-upgrade.8.html)) **[V]**;
  `bootc rollback` boots the previous image
  ([bootc-rollback](https://bootc.dev/bootc/man/bootc-rollback.8.html)) **[V]**.
- greenboot adds boot-time health checks and an automatic rollback after a set number
  of failed boots ([greenboot](https://github.com/fedora-iot/greenboot/blob/main/README.md))
  **[V]**. That is what makes a Linux node that fails to boot recover without a person.
- The host OS becomes an image pinned by digest, built by the operator's CI from a
  pinned upstream base, the same way kbf treats action images. "Update available"
  means "a newer attested image exists for this pool".
- Cost: a reinstall per node (cheapest when new servers arrive), a distribution change
  to one with bootc images, and a check that arm64 images exist for the arm64 nodes
  **[A]**.
- Also considered: Fedora CoreOS with Zincati and Flatcar
  ([Zincati](https://coreos.github.io/zincati/usage/updates-strategy/),
  [locksmith](https://github.com/flatcar/locksmith)) **[V]** bring their own lock
  servers **[A]**; kernel live patching does not replace reboots
  ([Livepatch](https://ubuntu.com/security/livepatch)) **[V]**.

### 8.3 Firmware

fwupd covers UEFI capsules, NVMe and BMCs, and capsule updates need a reboot
([fwupd](https://fwupd.github.io/libfwupdplugin/)) **[V]**; vendor support decides
whether it works. Firmware is a set kind of its own: always manual approval, canary,
soak, concurrency 1. A failed flash is recoverable only through the BMC or by a
person, so firmware updates wait until a node's BMC is known to work. Microcode ships
as distribution packages and rides the normal rollout **[A]**.

## 9. VM images as rolled-out software

- A golden VM image ([macos-vms.md](macos-vms.md)) is built in CI on a Mac from a
  pinned restore image, a pinned Xcode and runtimes, and named by digest.
- It is part of a pool's software set. `kbf-updater stage` places it on nodes ahead
  of time; switching the set makes VM leases ask for the new `vm_image`. The host is
  not touched and nothing reboots.
- A guest cannot run a newer macOS than its host **[A]**, so a set that moves both
  updates hosts first, then images.

## 10. GPU work and desktop-app tests on a bare-metal Mac

### 10.1 No GPU in VMs

On Apple silicon there is no GPU passthrough; a macOS guest gets a paravirtual Metal
device. One published measurement found a stock guest running LLM inference at about
4-14% **of** bare-metal speed (7 to 23 times slower): TinyLlama generation 12.63
tokens/s against 286.71
([measurements](https://github.com/trycua/cua/blob/main/blog/gpu-passthrough-macos-vms.md))
**[V]** for their machine, **[A]** for ours. The same source's faster guest depends on
injecting a library to change private Metal behaviour; not fit for a farm.

So GPU tests, including tests that install the desktop app and drive it with a local
LLM, take a `whole_machine` lease on bare metal: no other lease, bare-metal or VM, on
that Mac at the same time, one GPU test per Mac at a time. The lease books the node's
whole capacity and both VM slots, and the scheduler reserves the Mac so one-core work
cannot starve it ([macos-vms.md](macos-vms.md) sections 5.3 and 8.1). VMs remain for
simulator and GUI tests that do not need the GPU.

### 10.2 Isolation layers

macOS has no namespaces or cgroups. The isolation available is separate users, what a
non-admin user cannot change, a scan for what changed anyway, and an erase.

| Layer | When | What | Time **[A]** |
|---|---|---|---|
| L0 | every lease | a fresh non-admin user `kbf-lease-<n>` with no secure token; the app installed into that user's own Applications folder or run from the action directory; model weights read from a root-owned, read-only model store filled from the CAS | ~10 s |
| L1 | once per Mac, by MDM | UI automation mode enabled without authentication; Setup Assistant panes skipped for new users; Full Disk Access for the leak scanner; managed login items for kbf's own services | 0 per lease |
| L2 | every GUI lease | auto-login into the lease user, then a userspace restart (or a full reboot); the same at the end to return to the idle user | 1-4 min |
| L3 | every lease | cleanup (delete the user and home, kill its uid, sweep shared folders) and a leak scan against the node's baseline | 30-60 s |
| L4 | leak, or after a privileged lease | reboot and scan again; still dirty: quarantine, MDM erase, automatic re-provision, re-qualify | 45-90 min |
| L5 | about monthly per Mac, one at a time | a scheduled erase anyway: proves the repair path works and resets state no scan sees | 45-90 min |

Overhead per desktop-app lease without a leak: about 2 to 4 minutes **[A]**, small next
to an LLM test.

Notes on each layer:

- **L0.** A non-admin user cannot write `/Applications`, `/Library/LaunchDaemons`,
  system extensions or system settings **[A]** (standard macOS behaviour). A test that
  must exercise the real `.pkg` installer into `/Applications` is a **privileged lease**
  (`kbf-mac-admin=true`): an admin lease user, and always L4's reboot and scan.
  Recommendation to app teams: ship a per-user install path for most tests and run the
  system installer as its own, rarer test.
- **L1, UI automation.** `automationmodetool enable-automationmode-without-authentication`
  lets UI-test automation start without a person, meant for CI
  ([automationmodetool(1)](https://keith.github.io/xcode-man-pages/automationmodetool.1.html))
  **[V]**; that it is per machine and survives user deletion is **[A]**. XCUITest
  then drives the app by bundle id. A per-user Accessibility grant does not work here:
  a configuration profile cannot be installed silently without MDM, and on macOS 27
  the privacy-profile `Accessibility` key is deprecated, shows a notification and lets
  the user change it
  ([TCC schema](https://github.com/apple/device-management/blob/release/mdm/profiles/com.apple.TCC.configuration-profile-policy.yaml))
  **[V]**; its replacement asks the user to consent
  ([app.settings](https://github.com/apple/device-management/blob/release/declarative/declarations/configurations/app.settings.yaml))
  **[V]**.
- **Screen Recording cannot be granted by a profile**, only denied (TCC schema above)
  **[V]**. A test that needs real screen capture needs a person's click per grant.
  Whether XCUITest screenshots need it is **[A]**; prove it before relying on it.
- **L1, Setup Assistant.** The `com.apple.SetupAssistant.managed` payload skips the
  first-login panes for each new user
  ([schema](https://github.com/apple/device-management/blob/release/mdm/profiles/com.apple.SetupAssistant.managed.yaml))
  **[V]**.
- **L2.** Auto-login needs FileVault off (it is off on rack nodes). The stored login
  is obfuscated, not secret; acceptable only because the user is random, non-admin and
  deleted after the lease. That a userspace restart brings up the auto-login session
  on 27 is **[A]**; the fallback is a full reboot.
- **L3, the scan.** Each item is diffed against a baseline recorded at qualification:
  users and uids in the lease range; launchd jobs and the launch agent and daemon
  folders (names and hashes); background tasks (`sfltool dumpbtm`
  ([Apple](https://support.apple.com/guide/deployment/manage-login-items-background-tasks-mac-depdca572563/web))
  **[V]**); system extensions and loaded kexts; profiles; package receipts;
  `/Applications` with code-signature hashes; system privacy database rows; firewall
  application list, DNS, proxies and the hosts file; System keychain certificates;
  mounts; files owned by the lease uid in shared places; free disk; the Xcode and
  runtime set.
- **Every scan item has a planted-leak test** (a privileged lease that drops a launch
  daemon must turn the scan red). A scan item that never fails proves nothing.
- **Known limit.** Gatekeeper's assessment state is system-wide and is cached per code
  hash, so a second run of the same app build may skip the first-launch check **[A]**.
  A test of first-launch behaviour asks for a freshly erased Mac (L5 state).
- **Rejected:** reverting an APFS snapshot. A whole-system restore goes through macOS
  Recovery, entered with the power button on Apple silicon
  ([Apple](https://support.apple.com/guide/mac-help/recover-all-your-files-mh15638/mac))
  **[V]**: physical, and it would roll back the daemon's own state.

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
| Update available | newer signed set; newer Apple release (minor / security / major) with posting date | newer signed set; newer snapshot or image |
| State | serving, cordoned, draining, updating, rebooting, qualifying, held, quarantined | same |
| Rollout step | if any | if any |
| MDM | enrolled, supervised, bootstrap token escrowed, last check-in | n/a |

The page also shows the push certificate and enrollment token expiry dates, and, for
Macs, the queued actions per pinned host value (section 6).

### 11.2 What the operator clicks

- **Update** on a row: a one-node rollout to the pool's newest signed set. It still
  cordons and drains first; there is no shortcut.
- **Update all Linux workers** (per pool: x86_64, arm64) and **Update all Mac
  workers**: one rollout with that selector.
- **Pause, Resume, Skip node, Cancel, Roll back** on a running rollout.

A button is disabled, with the reason shown, when it cannot work: a Mac not
supervised or without an escrowed bootstrap token; an Xcode whose `.xip` is not in the
store; an Xcode that needs a newer macOS than the set provides; no signed set newer
than the node's.

### 11.3 "Update all Mac workers", step by step

1. The UI shows the plan before it starts: the target set, the canary, the order,
   `min_serving` (with 2 Macs, one always serves), the expected time, that macOS cannot
   be rolled back, and any client cache cost (section 6.1).
2. The server takes the canary: cordon, drain (whole-Mac leases finish), asks the MDM
   to enforce the target build now, watches DDM status, waits for the reboot.
3. The daemon reconnects; its report must show the target build and Xcode set.
4. Qualification runs on the canary. Optional client gate (`awaiting_client`).
5. Soak (2 h by default). Then the next Mac, one at a time, the same steps.
6. Any failure: the rollout holds, an alert fires, the failed Mac stays out, every
   other Mac keeps serving.

"Update all Linux workers" is the same with `kbf-updater` applying the snapshot or
image. Action images are pinned by digest, so a host update changes no action output
and costs no cache; only drain time.

### 11.4 API

Served by `kbf-server` under `/v1`, admin role only. Every write is an audit record
naming the actor.

- `GET /v1/nodes`: per node, the observed state, the pool's set, available updates
  and update state. The UI's JSON twin; clients and scripts read it.
- `GET /v1/software`: signed sets per pool, and upstream releases with their source.
- `POST /v1/rollouts`: `selector` (nodes, or a platform and pool), `target` (a set
  digest or `latest`), `strategy` (`canary`, `max_unavailable`, `min_serving`,
  `soak`, `window`).
- `GET /v1/rollouts/{id}`; `POST /v1/rollouts/{id}:pause`, `:resume`, `:skip`,
  `:cancel`, `:rollback`. Progress goes to the event stream.

## 12. Remote recovery without a BMC

Mac Studios have no BMC. What replaces it:

| Problem | Remote fix | Needs |
|---|---|---|
| Hung Mac | power-cycle through a switched PDU outlet; `pmset autorestart 1` boots it on power return | a switched PDU **[A]** |
| Hung Mac, no PDU | Lights Out Management: an enrolled controller Mac starts, stops or restarts it; Apple lists Mac Studio models ([LOM](https://support.apple.com/guide/deployment/lights-out-management-payload-settings-dep580cf25bc/web)) **[V]** | MDM, two controller Macs on the same subnet over Ethernet; that it resets a panicked Mac is **[A]** |
| Bad software, leak, drift | MDM erase and automatic re-provision (7.5) | MDM, ABM |
| Will not boot, firmware failure | DFU restore from a cabled neighbour Mac (`macvdmtool` ([AsahiLinux](https://github.com/AsahiLinux/macvdmtool)), then `cfgutil restore`) **[A]**, unofficial | a cable in each Mac's DFU port; moving it needs a person |
| Hardware | none | a person |

Entering DFU by hand needs the power button held while plugging in power
([Apple](https://support.apple.com/en-us/108900)) **[V]**. There is no supported remote
way into recoveryOS **[A]**.

Linux: the BMC (Redfish or IPMI remote console and power) where the board has one;
bootc with greenboot for software that does not boot; a switched PDU otherwise. Which
of our boards have a working BMC is unknown and recorded before firmware updates
start.

**What still needs a person:** adding each Mac bought before the organisation existed
to ABM (once); the yearly push certificate and token renewals; one Xcode download per
release; Screen Recording grants; a Mac that will not boot; firmware recovery without
a BMC; hardware.

## 13. Phased plan

| Phase | Delivers |
|---|---|
| P0 probes | on one Mac: DDM enforcement with a near deadline on 27; erase then ADE and Auto Advance; `automationmodetool` across user deletion; Screen Recording need of XCUITest; `DEVELOPER_DIR` per action. On one Linux node: snapshot upgrade; cgroup delegation after reboot; BMC presence |
| P1 read-only | software state in the report and `NodeStatus`; `/v1/nodes`; Fleet page with update available; cordon and drain in protocol and scheduler; `kbf-updater` for `kbf-daemon` and the profile only |
| P2 Linux rollouts | rollout object, canary, soak, qualification, `min_serving`, halt; Linux updates by snapshot; Update buttons for Linux |
| P3 Mac rollouts | ABM and MDM live while the Macs are wiped; macOS and Xcode rollouts; probes and the client gate; Update buttons for Macs |
| P4 bare-metal GPU and desktop-app leases | per-lease-user driver, leak scan with planted-leak tests, L4/L5 erase; then golden VM images as software sets |
| P5 image-based Linux | bootc with greenboot on new servers, then the rest one at a time |

Changes by crate (rough sizes, tests included):

| Crate | Change | Lines |
|---|---|---|
| `kbf-proto` | `NodeStatus`; `Drain` and `Update` server messages | ~150 |
| `kbf-daemon`, `kbf-node` | re-detect and resend; new keys; probes; forward to the updater; `DEVELOPER_DIR` per action | ~600 |
| `kbf-updater` (new) | socket, manifest verify, profile / package / bootc backends, state file | ~1,800 |
| `kbf-mdm` (new) | `OsUpdateBackend`; NanoHUB client; DDM status | ~700 |
| `kbf-types`, `kbf-sched` | node states; cordon in placement; rollout state machine; slots; `min_serving`; `kbf-node` pin; whole-Mac reservation | ~1,800 |
| `kbf-server` | rollout driver, audit, `/v1/nodes`, `/v1/software`, `/v1/rollouts`, Fleet page, absence suppression | ~1,200 |
| `kbf-caps`, `kbf-front` | new exact and repeated keys, `probe.*`, `kbf-node` validation | ~250 |
| `kbf-alert` | rollout held, node not back, certificate and token expiry | ~200 |
| `kbf-driver-native` | per-lease-user GUI driver, cleanup, leak scan | ~2,500 |
| `kbf-sim`, `kbf-it` | tests below | ~800 |

Tests, each with the defect it catches:

- A rollout never drops a class below `min_serving`. Mutant: ignore `min_serving` in
  the pre-gate; the simulation finds a seed where the last arm64 node is drained.
- A failed canary holds the rollout and no second node is touched. Mutant: release the
  slot on failure.
- A replayed older manifest is refused by `kbf-updater`. Mutant: skip the serial check.
- An unsigned or wrongly signed manifest is refused. Mutant: skip verification.
- A leader change mid-rollout neither repeats nor skips a step. Mutant: keep rollout
  state outside the control log.
- A node that returns with the wrong build is quarantined. Mutant: accept any `Hello`.
- A planted launch daemon after a lease turns the leak scan red. Mutant: drop that scan
  item.

## 14. Verified and assumed

Claims marked **[V]** in the text are read in the linked primary source. The ones
that carry the design, and every assumption to test:

| Claim | Status |
|---|---|
| `com.apple.SoftwareUpdate` payload and `ScheduleOSUpdate`, `AvailableOSUpdates`, `OSUpdateStatus` removed on macOS 27 | **[V]** Apple's device-management schema |
| DDM enforcement takes target version, build and deadline; needs supervision, macOS 14+ | **[V]** schema |
| A deadline minutes ahead installs at once on a drained node; majors enforce the same way | **[A]** |
| `softwareupdate --stdinpass` works unattended on 27 with nobody logged in | **[A]** |
| Local software-update preferences still honoured on 27 | **[A]** |
| Background Security Improvements can install regardless of the setting | **[V]** Apple |
| ADE with Auto Advance needs no person on Ethernet | **[V]** Apple; end to end after an erase on 27 **[A]** |
| NanoHUB, NanoDEP, scep are MIT and maintained; MicroMDM v1 support ended | **[V]** GitHub |
| Xcode download needs a sign-in; automation unreliable | **[V]** xcodes issue |
| Private internal Xcode mirror fits the licence | **[A]** (reading, not legal advice) |
| `DEVELOPER_DIR` per action selects Xcode for all tools | **[A]** |
| Installing a new Xcode does not change an older one's behaviour | **[A]** |
| Stock VM LLM inference at 4-14% of bare metal | **[V]** for the source's machine; **[A]** for ours |
| Privacy-profile Accessibility deprecated and notifying on 27; Screen Recording only deniable | **[V]** schema |
| `automationmodetool` lets UI automation start without authentication | **[V]** man page; persistence across users **[A]** |
| Per-lease overhead 2-4 min; re-provision 45-90 min | **[A]** |
| LOM supports Mac Studio | **[V]** Apple; panic recovery **[A]** |
| bootc upgrade, rollback; greenboot automatic rollback | **[V]** bootc, greenboot |
| arm64 bootc images for our arm64 nodes | **[A]** |
| Our Linux nodes run Ubuntu 24.04; boards have a BMC | **[A]** |
