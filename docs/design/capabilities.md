# Capabilities

A farm is only useful if each action lands on a machine that can run it. kbf's rule is
simple: **a daemon finds out what its machine can do by asking the kernel, and reports
it.** Nobody types hardware facts into a configuration file, so a wrong hand-typed fact
cannot strand a machine or send work where it cannot run. The scheduler then matches
each action's platform properties against these reports.

This document covers the node report, how it is detected (`kbf-daemon`'s `report`
module and `kbf-caps`), ISA levels, the matching rules, and what is **planned**.

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
| `isa_level` (repeated) | **every** level the CPU reaches, lowest first | computed from the features |
| `cpu.features` (repeated) | every CPU feature flag, in the kernel's names | `/proc/cpuinfo` |
| `drivers` (repeated) | the execution drivers this daemon offers | the daemon's runtime |

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
- **macOS** (parsed, not yet reported). Every `hw.optional.*` key whose value is 1 is a
  feature, under its own name with the prefixes removed (`FEAT_AES`), and, where Linux
  has a hwcap for the same thing, under the kernel's name too (`aes`). So a request for
  `cpu.feature=aes` matches an Apple silicon Mac and a Linux arm64 machine alike.

The parsers are tested against committed real outputs from several CPU generations of
both architectures.

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
| `cpus`, `mem_gib`, `nvme_gib` | the node has at least this amount |
| `os`, `os_image`, `cpu.model`, `page_size`, `gpu`, `label.<k>` | exact |
| `xcode` | membership: the node reports one `xcode` entry per installed Xcode build, and the request names one of them |
| `os_build` | exact; **planned**: today the front drops it as an unknown name, so it matches every node (see [mac-node-provisioning.md](mac-node-provisioning.md#31-host-identity)) |

Every other key may appear once. An unknown key, a value that does not parse, or a
repeated key is refused, so a typo fails loudly instead of matching nothing forever. A
Mac with two Xcodes installed serves an action that names either build, and the native
driver runs it with that Xcode selected (`DEVELOPER_DIR`; see
[platform-properties.md](../platform-properties.md#xcode)).

**Reserved keys** ask for a kind of capacity, not a hardware fact, and are skipped by
the matcher:

| Key | Meaning |
|---|---|
| `kbf-lease` | the lease kind: `action` (the default, a share of a machine) or `whole_machine` |
| `kbf-cpu` | **planned**: `dedicated` for whole physical cores, for quiet performance runs |
| `kbf-mac-admin` | **planned**: a privileged whole-machine lease on macOS |
| `kbf-book-cpus`, `kbf-book-mem-gib` | the whole cores and GiB an `action` lease books in place of one core and 1 GiB (see [platform-properties.md](../platform-properties.md#kbf-book-cpus-and-kbf-book-mem-gib)) |

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
spelling is REAPI's `Arch`. One name in two spellings is refused.

## What the code enforces today

- The front reads `kbf-lease` from the action's platform (on the `Action`, or on the
  `Command` for clients older than REAPI 2.2). Absent means `action`; any value other
  than `action` or `whole_machine` is `INVALID_ARGUMENT`. A property name that is empty
  or appears twice is `INVALID_ARGUMENT` as well.
- The front turns the platform into a request (above). A malformed one is
  `INVALID_ARGUMENT`. One no kbf daemon can ever run, an `os` other than `linux` or
  `macos` or an `ISA` naming another architecture, is `FAILED_PRECONDITION` at once.
- **Matching in placement.** The scheduler offers an action only to a live worker whose
  report satisfies its request; matches are memoised per request within a placement
  round. See [scheduler.md](scheduler.md#placement).
- **Answers when nothing matches.** A request no live worker satisfies (or none that
  does is large enough for) waits in the queue, and its callers see why in the
  operation's metadata. After `--unservable-wait-secs` (300 by default) of that it
  fails with `FAILED_PRECONDITION`, which neither Bazel nor Buck2 retries.
- The daemon refuses a `Start` for a lease kind no runtime of its serves. The container
  driver serves `action` only.
- The container driver reads `container-image` itself (see
  [daemon.md](daemon.md#the-container-driver)).
- A Mac's native driver reports one `xcode` entry per `Xcode*.app` in `/Applications`
  (`--xcode-apps`) that answers `xcodebuild -version`, and runs an action that names
  an `xcode` build with that Xcode's `DEVELOPER_DIR`.

## Planned

- **Drivers in placement.** A worker is feasible only if it also offers a driver for
  the lease kind (today the daemon refuses the `Start`).
- **Unknown keys refused.** An unknown property is refused at `Execute` with
  `INVALID_ARGUMENT` naming the closest known key; today properties that are not
  capability keys are ignored, so a misspelt `OSFamilly` matches every worker.
- **More report entries:** `cpu.model` (a human name for the microarchitecture),
  `nvme_gib`, `gpu`, `os_image` (on Linux), `os_build` and the SDKs of each Xcode on
  macOS (see [mac-node-provisioning.md](mac-node-provisioning.md#31-host-identity)), the
  images already on the machine, and
  virtualization support (reported only, for a later VM driver).
- **Labels** added by operators on top of detected facts, matched as `label.<k>`.
- **Client-defined probes** (`probe.<k>`) are status values, never report entries or
  request keys (see [mac-node-provisioning.md](mac-node-provisioning.md#31-host-identity)).
- **Platform aliases:** named, immutable sets of properties (for example an
  architecture plus a default image), so a client can name a platform briefly and an
  alias's bytes, and so its action digests, never change once used.
- **Protected floors:** capacity a machine keeps for other services, subtracted from
  `cpus` and `mem_gib` before placement sees them.
- **Report changes noticed by hash:** heartbeats already carry the report hash; the
  server will compare it with the newest `Hello`'s to notice a report that changed.
