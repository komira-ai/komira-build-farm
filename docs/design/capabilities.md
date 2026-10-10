# Capabilities

A farm is only useful if each action lands on a machine that can run it. kbf's rule is
simple: **a daemon finds out what its machine can do by asking the kernel, and reports
it.** Nobody types hardware facts into a configuration file, so a wrong hand-typed fact
cannot strand a machine or send work where it cannot run. The scheduler then matches
each action's platform properties against these reports.

This document covers the node report, how it is detected (`kbf-daemon`'s `report`
module and `kbf-caps`), the node status that sits beside it, ISA levels, the matching
rules, and what is **planned**. Everything not marked planned is on `main`.

## The node report

The report is a list of `(key, value)` entries, carried in `Hello` (see
[worker-protocol.md](worker-protocol.md#hello-and-welcome)). Entries are sorted by key,
then value, and de-duplicated, so equal reports encode to equal bytes and hash equally.
A list-valued capability repeats its key once per value.

What a Linux daemon reports today:

| Key | Value | Source |
|---|---|---|
| `arch` | `x86_64` or `arm64` | `/proc/cpuinfo` |
| `os` | `linux` | |
| `cpus` | number of `processor` lines | `/proc/cpuinfo` |
| `mem_gib` | `MemTotal`, in GiB, rounded down | `/proc/meminfo` |
| `page_size` | the kernel page size, in bytes | `KernelPageSize` in `/proc/self/smaps` |
| `gpu` | NVIDIA and AMD display and 3D controllers | `/sys/bus/pci/devices` (`kbf_caps::gpu`) |
| `isa_level` (repeated) | **every** level the CPU reaches, lowest first | computed from the features |
| `cpu.features` (repeated) | every CPU feature flag, in the kernel's names | `/proc/cpuinfo` |
| `drivers` (repeated) | the execution driver this daemon runs: `container`, `native` or `fake` | `kbf-daemon --driver` |
| `label.<k>` | an operator's label, `--label k=v` | the daemon's flags |

A daemon on an Apple silicon Mac asks `sysctl` instead: `hw.optional` (which `kbf-caps`
parses into `cpu.features` and `isa_level`), `hw.ncpu` (`cpus`), `hw.memsize`
(`mem_gib`), `hw.pagesize` (`page_size`) and `machdep.cpu.brand_string` (`cpu.model`,
reported on macOS only), with `os` = `macos` and `arch` = `arm64`. It reports `gpu` = 0:
`kbf-caps` can count the integrated GPU (`gpus_from_macos_sysctl`), but the daemon does
not offer it for booking. A daemon on any other operating system refuses to start.

`cpus` and `mem_gib` are the whole machine. They are resources, not capabilities: the
scheduler books against them and there are no slots (see
[scheduler.md](scheduler.md#placement)). The server reads the rest of the report with
`kbf_caps::NodeCaps::from_report`: `arch` (required), `cpu.features`, the countable
entries and the exact keys below. The reported `isa_level` list is not read back: the
level is computed again from the features, so the two can never disagree.

### Reading CPU features

- **Linux.** Features come from the `flags` (x86-64) or `Features` (arm64) lines of
  `/proc/cpuinfo`. The kernel already omits features the operating system has not
  enabled (an AVX flag is cleared when the OS does not save the wide registers), so no
  separate check is needed. On a machine whose processors report different sets, the
  report keeps only the features **every** processor has, so a heterogeneous machine
  never claims a feature some of its cores lack. A block that mixes architectures is
  refused.
- **macOS.** Every `hw.optional.*` key whose value is 1 is a feature, under its own
  name with the prefixes removed (`FEAT_AES`), and, where Linux has a hwcap for the
  same thing, under the kernel's name too (`aes`). So a request for `cpu.feature=aes`
  matches an Apple silicon Mac and a Linux arm64 machine alike.

The parsers are tested against committed real outputs from several CPU generations of
both architectures.

### Driver entries

The driver adds its own entries. The native driver (Macs) reports `network_isolation`
(`sandbox-exec` or `none`; reported, not matched) and one `xcode` entry per ready Xcode
build it can select (see [Matching](#matching)); an installed Xcode that is not ready is
in the node status instead (`xcodes`, below).

A daemon runs one driver today, so `drivers` has one value. Placement reads it
(`kbf_types::LeaseKind::drivers`): a lease goes only to a worker that lists a driver
serving its kind, by the table below, and a worker that lists none serves nothing. The
daemon also refuses a `Start` for a lease kind its driver does not serve.

| Value | Serves | Status |
|---|---|---|
| `container` | `action` | exists (Linux, rootless Podman) |
| `native` | `action` | exists (Macs, plain processes) |
| `fake` | `action` | exists, for bring-up only: runs nothing |
| `local` | `action` | exists, in tests only: `kbf-daemon`'s child-process runtime |
| `vm` | `vm` | **planned**: listed only when a check that boots no VM passes, at daemon start and periodically ([macos-vm-guests.md](macos-vm-guests.md#8-the-launch-daemon-risk)) |
| `native-whole-machine` | `whole_machine` | **planned**: the bare-metal whole-machine runtime, listed only when `kbf-mac-session` is present ([fleet-updates.md](fleet-updates.md#102-isolation-layers), phase P4) |

On `main` no driver serves `whole_machine`, so a `whole_machine` lease waits, saying
that no live worker serves the kind, and is refused after the unservable wait. A
`whole_machine` lease books the whole worker and is placed only on one that holds no
lease; while it waits it holds one worker in queue (QoS) order, which takes no less
urgent work meanwhile (see
[platform-properties.md](../platform-properties.md#kbf-lease)).

### Planned VM entries

The macOS VM driver ([macos-vms.md](macos-vms.md#52-node-report-planned)) will add, all
**planned**:

| Entry | Meaning | Kind |
|---|---|---|
| `vm.slots` | how many VMs may run at once; fills a `vms` booking dimension | report-only |
| `vm.max_cpus`, `vm.max_mem_gib` | the framework's bounds, read at start | report-only |
| `vm.image` (repeated) | the golden images on the node's disk whose file manifest re-verifies, by recipe digest ([macos-vm-guests.md](macos-vm-guests.md#5-image-identity)) | capability: a request names one, matched by membership on the digest |

A report-only entry is never a request key: an action cannot ask for `vm.slots`, and
`vms` is booked only through `kbf-lease=vm`.

### Planned device entries

Physical iOS devices on a Mac node ([ios-devices.md](ios-devices.md#52-report-entries))
will add, all **planned**:

| Entry | Meaning | Kind |
|---|---|---|
| `ios.device` (repeated) | one per **ready** USB-attached iPhone or iPad: a sorted `k=v` list of `id` (the UDID), `class`, `product_type`, `os_version`, `os_build` | capability: one device is booked per lease, and one device must satisfy every `ios.device.*` key of a request |

A device that is not ready is not a report entry; it is listed, with its state and the
fix, in `NodeStatus` (planned `devices`). An older server skips the entry, as it skips
every report entry it does not know.

## The node status

Facts that route no work go in `NodeStatus`, not in the report, so they do not change
the report hash. A daemon sends it after each `Welcome` (`kbf-daemon`'s `status`
module), and the server shows it in `GET /v1/nodes` ([api.md](../api.md#get-v1nodes)):

| Field | Mac | Linux |
|---|---|---|
| `os_name` | `sw_vers -productName` | `os-release` `NAME` |
| `os_version` | `sw_vers -productVersion` | `os-release` `VERSION_ID` |
| `os_build` | `sw_vers -buildVersion` | `os-release` `BUILD_ID`, where set |
| `kernel` | empty | `/proc/sys/kernel/osrelease` |
| `daemon_version` | the `kbf-daemon` version | the same |
| `xcode_builds` | the report's `xcode` entries (the ready Xcodes), sorted | empty |
| `xcodes` | every installed Xcode: app, build, state (`ready` or why not), reason, fix command | empty |

A field that cannot be read is empty; status never stops a node from joining.

**Planned:** `os_version` (both platforms), `os_build` (Mac) and `kernel` (Linux)
also become report entries matched exactly, so an action can pin them
([fleet-updates.md](fleet-updates.md#31-observed)). Today an action that names one is
not matched on it (see [unknown keys](#unknown-keys)). Client-defined probes
(`probe.<k>`, [mac-node-provisioning.md](mac-node-provisioning.md#31-host-identity))
are **planned** as `NodeStatus` values: never report entries and never request keys.
Work routes on `xcode` and `os_build`, not on a probe.

## ISA levels

A level names a cumulative set of features. A machine at a level supports every level
below it, and the report lists all of them, so a request for an older level is served
by a newer machine.

**x86-64** uses the four psABI microarchitecture levels. Each adds, in kernel flag
names:

| Level | Adds |
|---|---|
| `x86-64-v1` | `cmov cx8 fpu fxsr mmx syscall sse sse2` |
| `x86-64-v2` | `cx16 lahf_lm popcnt pni sse4_1 sse4_2 ssse3` |
| `x86-64-v3` | `avx avx2 bmi1 bmi2 f16c fma abm movbe xsave` |
| `x86-64-v4` | `avx512f avx512bw avx512cd avx512dq avx512vl` |

**arm64** has no official level ladder for user space. kbf uses Armv8 architecture
versions, where a version means "every feature Arm makes mandatory at that version, and
the kernel reports as a hwcap, is present":

| Level | Adds (kernel hwcap names) |
|---|---|
| `armv8.0-a` | `fp asimd` |
| `armv8.1-a` | `atomics asimdrdm crc32` |
| `armv8.2-a` | `dcpop` |
| `armv8.3-a` | `paca pacg jscvt fcma lrcpc` |
| `armv8.4-a` | `dit uscat ilrcpc flagm` |

The ladder stops at `armv8.4-a`; later features are still reported by name. A machine's
level is the highest rung whose features, and every lower rung's, are all present. The
two families are never compared: an x86-64 level never satisfies an Arm request.

## Matching

`kbf_caps::Request::parse` turns an action's platform properties into a request, and
`Request::unmet` lists every requirement a node does not meet (empty means it matches).
Each key has one typed comparison:

| Request key | Comparison |
|---|---|
| `arch` | exact |
| `isa_level` | at least, within the family: `x86-64-v3` is served by v3 and v4 |
| `cpu.feature` (may repeat) | every requested feature is present |
| `cpus`, `mem_gib`, `nvme_gib`, `gpu` | the node has at least this amount (`gpu` is also booked, below) |
| `os`, `os_image`, `cpu.model`, `page_size`, `label.<k>` | exact |
| `xcode` | membership: the node reports one `xcode` entry per ready Xcode build, and the request names one of them |
| `os_build`, `os_version`, `kernel` | **planned**: exact, once they are report entries (see [The node status](#the-node-status)) |
| `vm.image` | **planned**: membership on the digest (see [Planned VM entries](#planned-vm-entries)) |
| `ios.device` | **planned**: `1` books one specific device; `ios.device.class`, `ios.device.product_type`, `ios.device.os_version`, `ios.device.os_build` are exact, and one device must satisfy all of them (see [Planned device entries](#planned-device-entries)) |

Every other key may appear once. A value that does not parse or a repeated key is
refused. A Mac with two Xcodes installed serves an action that names either build, and
the native driver runs it with that Xcode selected (`DEVELOPER_DIR`; see
[platform-properties.md](../platform-properties.md#xcode)). No daemon reports
`os_image` or `nvme_gib` yet, so a request for `os_image`, or for `nvme_gib` above 0,
matches no node (a missing amount counts as zero, so `nvme_gib=0` matches every node).

A report entry the matcher does not know (`drivers`, `network_isolation`, `isa_level`,
...) is skipped, so a newer daemon's entries never stop an older server from reading
its report.

### Unknown keys

`Request::parse` refuses a key it does not know. But a client's platform reaches it
through `Request::from_platform` (below), which passes on only the names kbf reads, so
**an unknown platform property is ignored today**: `OSFamilly=darwin`, `os_build=24B83`
or `vm.slots=2` matches every node. Refusing an unknown property at `Execute` is
**planned** (see [Planned](#planned)).

**Reserved keys** ask for a kind or a size of capacity, not a hardware fact, and are
skipped by the matcher (`kbf_caps::RESERVED_KEYS`; names read in any case):

| Key | Meaning |
|---|---|
| `kbf-lease` | the lease kind: `action` (the default, a share of a machine) or `whole_machine`; **planned**: `vm` |
| `kbf-cpu` | the name is reserved; its meaning is **planned**: `dedicated` for whole physical cores, for quiet performance runs |
| `kbf-mac-admin` | the name is reserved; its meaning is **planned**: a privileged whole-machine lease on macOS |
| `kbf-book-cpus`, `kbf-book-mem-gib` | the whole cores and GiB an `action` lease books in place of one core and 1 GiB (see [platform-properties.md](../platform-properties.md#kbf-book-cpus-and-kbf-book-mem-gib)); the front refuses either on a `whole_machine` lease. **Planned**: they also size a `vm` lease's guest |
| `kbf-node` | **planned**, not yet reserved: pins a lease to one node for qualification. The front will refuse it from every client; only the rollout driver's internal submitter sets it ([fleet-updates.md](fleet-updates.md#31-observed)) |

`kbf-book-cpus` and `kbf-book-mem-gib` are not `cpus` and `mem_gib`: those ask for a node
whose whole machine has at least that much, and book nothing.

**Every platform property is part of the action digest**, which is REAPI's rule. Two
consequences shape the design:

- A property that does not change the output still splits the cache. That is why QoS
  is a request header and never a property (see [scheduler.md](scheduler.md#qos)).
- An "at least" match can put host-tuned output in a cache entry for a lower level: a
  tool that compiles for the host CPU (`-march=native`), run for a `x86-64-v3` request
  on a v4 machine, produces v4 code under a v3 key. Toolchains should pass an explicit
  target CPU; a tool that cannot should ask for `cpu.model` exactly.

### The names build tools send

Buck2 and Bazel send whatever their remote platform sets (`remote_execution_properties`,
`exec_properties`); by convention that is REAPI's standard names, not kbf's.
`kbf_caps::Request::from_platform` reads them:

| Property | Value (any case) | Request |
|---|---|---|
| `OSFamily` | `linux` | `os=linux` |
| `OSFamily` | `darwin`, `macos`, `macosx`, `osx` | `os=macos` |
| `ISA`, `Arch` | `x86-64`, `x86_64`, `amd64` | `arch=x86_64` |
| `ISA`, `Arch` | `arm-a64`, `arm64`, `aarch64` | `arch=arm64` |
| `ISA`, `Arch` | an ISA level (`x86-64-v3`, `armv8.2-a`) | its `arch` and `isa_level` |

Every key in the matching table above passes through unchanged, except `gpu`, which is
booked (see [platform-properties.md](../platform-properties.md#gpu)). Every other
property is not a capability and is left to whoever reads it (`kbf-lease`,
`container-image`, ...). One requirement named twice (`OSFamily` and `os`) is refused.

Property names are read without regard to ASCII case (`kbf_caps::property_name`): the
REAPI names above and every kbf key (`osfamily`, `OS`, `GPU`, `Kbf-Lease`), so a
property meant for kbf is never dropped for its spelling and the action run anywhere.
A label's own name keeps its case. `arch` spelled exactly so is kbf's key; any other
spelling is REAPI's `Arch`. One name in two spellings is refused. The native driver
reads `xcode` in any case too.

**`container-image` is the exception: it is matched exactly.** The front does not read
it; the container driver looks for that exact name. `Container-Image=...` is a name kbf
does not know, so the front ignores it and the container driver refuses the action as
naming no `container-image`.

## What the code enforces today

- The front reads `kbf-lease` from the action's platform (on the `Action`, or on the
  `Command` for clients older than REAPI 2.2). Absent means `action`; any value other
  than `action` or `whole_machine` is `INVALID_ARGUMENT`. A property name that is empty
  or appears twice is `INVALID_ARGUMENT` as well.
- The front turns the platform into a request (above). A malformed one is
  `INVALID_ARGUMENT`. One no kbf daemon can ever run, an `os` other than `linux` or
  `macos` or an `ISA` naming another architecture, is `FAILED_PRECONDITION` at once.
- **Matching in placement.** The scheduler offers an action only to a live worker whose
  report lists a driver serving its lease kind and satisfies its request; matches are
  memoised per request and kind within a placement round. See
  [scheduler.md](scheduler.md#placement).
- **Whole-machine booking.** A `whole_machine` lease is placed only on a worker that
  holds no lease, and books all of its cores, memory and GPUs, so nothing is placed
  beside it. One that fits nowhere holds one worker that could run it; work after it
  in queue order is not placed there until it has emptied.
- **Answers when nothing matches.** A request no live worker satisfies (or none that
  does is large enough for) waits in the queue, and its callers see why in the
  operation's metadata. After `--unservable-wait-secs` (300 by default) of that it
  fails with `FAILED_PRECONDITION`, which neither Bazel nor Buck2 retries.
- The daemon refuses a `Start` for a lease kind no runtime of its serves. Every driver
  on `main` serves `action` only.
- The container driver reads `container-image` itself, by that exact name (see
  [daemon.md](daemon.md#the-container-driver)).
- Unknown platform properties are ignored, not refused (see [Unknown keys](#unknown-keys)).
- Each daemon sends its `NodeStatus` after `Welcome`, and again when its Xcodes change;
  it routes no work.
- A Mac's native driver reports one `xcode` entry per ready `Xcode*.app` in
  `/Applications` (`--xcode-apps`): one that answers `xcodebuild -version` and whose
  `xcodebuild -license check`, `xcodebuild -checkFirstLaunchStatus`, `xcrun --find
  clang` (and, with `--require-metal-toolchain`, the Metal toolchain check) exit 0, each
  asked under the actions' sandbox with the network off, as an action runs. It runs an
  action that names an `xcode` build with that Xcode's `DEVELOPER_DIR`. It asks
  again every `--xcode-recheck-secs`, and a change resends the `Hello` (so placement
  sees it) and the `NodeStatus` (which lists every installed Xcode, ready or not, with
  the fix).

## Planned

- **Unknown keys refused.** An unknown property is refused at `Execute` with
  `INVALID_ARGUMENT` naming the closest known key; today it is ignored, so a misspelt
  `OSFamilly` matches every worker.
- **`kbf-node`** reserved, and refused from every client (above).
- **More report entries:** `os_version` (both platforms), `os_build` (Mac) and
  `kernel` (Linux), matched exactly; `cpu.model` on Linux (a human name for the
  microarchitecture), `nvme_gib`, `os_image` (on bootc Linux), the SDKs of each Xcode
  on macOS (see [mac-node-provisioning.md](mac-node-provisioning.md#31-host-identity)),
  and the VM driver's `drivers` value and `vm.*` entries (above).
- **iOS devices:** `ios.device` report entries and request keys, a booking of one
  device id per lease carried in `Start`, and `NodeStatus.devices` with an attention
  item for each device that is not ready ([ios-devices.md](ios-devices.md)). Until the
  server knows `ios.device`, a request for it is an unknown property and matches
  every node (see [Unknown keys](#unknown-keys)).
- **Client-defined probes** (`probe.<k>`) as `NodeStatus` values, never report entries
  or request keys (see [mac-node-provisioning.md](mac-node-provisioning.md#31-host-identity)).
- **Re-detection:** the daemon re-detects its software keys after an update step and
  every 10 minutes, and sends a changed report mid-session
  ([fleet-updates.md](fleet-updates.md#31-observed)); today it detects once, at start.
- **Platform aliases:** named, immutable sets of properties (for example an
  architecture plus a default image), so a client can name a platform briefly and an
  alias's bytes, and so its action digests, never change once used.
- **Protected floors:** capacity a machine keeps for other services, subtracted from
  `cpus` and `mem_gib` before placement sees them.
- **Report changes noticed by hash:** heartbeats already carry the report hash; the
  server will compare it with the newest `Hello`'s to notice a report that changed.
