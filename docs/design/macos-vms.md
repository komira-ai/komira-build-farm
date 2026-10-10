# macOS virtual machines, bare-metal builds and the GPU on Mac nodes

This document says what runs where on a Mac node: which work runs on bare metal under
the native driver's sandbox, which work needs a macOS virtual machine (VM), how big a
VM is, how the scheduler knows an action needs one, how the planned VM driver creates
and destroys a VM for every lease, and how tests that use the GPU (including tests
that run a local language model) are run and judged. Everything in it is **planned**
unless it says otherwise; what exists on `main` today is named as such.

**What this changes.** The farm's earlier plan had no virtualization in its first
version. That stays true for Linux: Linux actions run in containers on bare metal.
For macOS only, it changes: simulator, GUI and UI-test work needs a logged-in GUI
session, and the way to give each such lease a fresh one, beside other work on the
same Mac, is a macOS VM. Builds and unit tests stay on bare metal. **GPU work never
runs in a VM** ([section 8.1](#81-on-bare-metal-never-in-a-vm)).

Isolating what installing a desktop app does to a bare-metal Mac, and fleet-wide
updates, device management and UI, are a separate design:
[fleet-updates.md](fleet-updates.md), with its security model in
[fleet-updates-security.md](fleet-updates-security.md).

How a guest gets past Setup Assistant, how screenshots and videos leave it, its
network, how an image is named, which uid runs the VM, how the VM helper is signed,
and the probes that must answer the open questions are in a companion design,
[macos-vm-guests.md](macos-vm-guests.md).

Claims about Apple's software and other projects are marked **[V]** (read in the
source linked) or **[A]** (an assumption or a number nobody has measured on our
hardware). The table in [section 11](#11-verified-and-assumed) lists them together.

## 1. Summary

| Question | Answer |
|---|---|
| Why VMs on a Mac at all | Simulator, GUI and UI tests need a logged-in GUI session. A launch daemon has none. A VM gives each lease a fresh one and is destroyed afterwards. |
| How many | At most **2 running macOS guests per Mac**: the macOS licence allows two, and macOS refuses a third. |
| What stays on bare metal | Compiling and linking (Swift, C, Objective-C), `swift build`, `swift test`, `xcodebuild build`/`build-for-testing` of Swift packages, frameworks and macOS targets, logic unit tests, and all GPU work. iOS app builds with storyboards or asset catalogs wait for a probe ([section 3.2](#32-do-swift-and-xcode-builds-need-a-vm)). |
| Default VM size | A 6 vCPU, 16 GiB guest; with 1 GiB for its helper it books 6 cores and 17 GiB in full while it runs. An action may ask for 4 to 12 vCPU and 8 to 32 GiB, which books 4-12 cores and 9-33 GiB. |
| How the scheduler knows | The action says `kbf-lease=vm` and names a pinned image (`vm.image`). Nodes report `vm.slots`, `vm` in `drivers`, and the images they hold. A VM books `vms=1` plus its cores and memory. |
| VM lifetime | Clone the golden image, boot, run one action, collect outputs, destroy. A VM is never reused. |
| GPU | Never in a VM: a stock guest runs LLM inference at about 4-14% of bare-metal speed (7 to 23 times slower): generation 4.4% (TinyLlama 1.1B) and 6.5% (Gemma 4 12B), prompt processing 8.9% and 13.8%. GPU tests, including tests that install the desktop app and drive it with a local LLM, run on bare metal with the whole Mac, one at a time. |
| Order | 1: builds and unit tests on bare metal. 2: VMs. 3: GPU. |

## 2. Why VMs on a Mac

### 2.1 GUI work needs a GUI session

`kbf-daemon` runs as a launchd daemon under a role account, with nobody logged in.
Apple's rule for daemons: a launch daemon "is not allowed to connect to the window
server"; only agents running in a logged-in (Aqua) session get GUI services
([TN2083](https://developer.apple.com/library/archive/technotes/tn2083/_index.html))
**[V]**. Booting a simulator, running XCUITest, and UI-testing a macOS app all need
that session **[A]**: there is no direct source, and the `simctl boot` probe of
[section 3.2](#32-do-swift-and-xcode-builds-need-a-vm) settles it. BuildBuddy runs its
Mac executor as a LaunchAgent and recommends auto-login so it survives reboots
([BuildBuddy](https://www.buildbuddy.io/docs/enterprise-mac-rbe/)) **[V]**.

There are two ways to give a lease a GUI session:

| | Whole-Mac lease with an auto-logged-in user | macOS VM per lease |
|---|---|---|
| Fresh state per lease | no: the logged-in user's state carries over unless wiped | yes: a clone of a golden image, destroyed after |
| Other work on the Mac at the same time | no, or shares one user's session | yes: bare-metal leases keep running beside up to 2 VMs |
| Host setup | auto-login off at rest; a root helper (`kbf-mac-session`, [fleet-updates.md](fleet-updates.md)) sets it for one lease and clears it after | host auto-login stays off; the guest image logs in |
| Toolchain per lease | one of the host's pinned Xcodes, chosen with `DEVELOPER_DIR` | the image's Xcode; two images can differ |

This document chooses the VM.

### 2.2 The licence: two guests per Mac

The macOS 27 software licence, section 2B(iii), grants the right "to install, use and
run up to two (2) additional copies or instances of the Apple Software ... within
virtual operating system environments on each Apple-branded computer you own or
control that is already running the Apple Software, for purposes of: (a) software
development; (b) testing during software development; (c) using macOS Server; or (d)
personal, non-commercial use"
([macOS 27 SLA](https://www.apple.com/legal/sla/docs/macOS27.pdf)) **[V]**. The same
section excludes "service bureau, time-sharing, terminal sharing, relay service or
other similar types of services" except as section 3 permits; section 3 lets a Mac be
leased to one customer for continuous integration for at least 24 consecutive hours
**[V]**.

macOS enforces the count: starting a VM beyond it fails with
[`VZErrorVirtualMachineLimitExceeded`](https://developer.apple.com/documentation/virtualization/vzerror/code/virtualmachinelimitexceeded)
**[V]**, and an Apple engineer states that "the limit of 2 virtual Macs is part of the
macOS End-User License Agreement"
([Apple forums](https://developer.apple.com/forums/thread/729580)) **[V]**.

What follows for kbf:

- **2 is a ceiling, enforced by the OS.** It counts *running* guests, so a slot is free
  only after the previous VM has stopped.
- A single-tenant farm building and testing its own software is purposes (a) and (b)
  **[A]** (not a legal opinion).
- A farm offered as a service to third parties falls under section 3 instead, which
  requires notice to Apple, gives the lessee "sole and exclusive use and control of the
  Apple Software and the Apple-branded hardware" for at least 24 hours, and says "a
  Lessor may only virtualize a single instance or copy of the Apple Software as a
  provisioning tool" (section 3A(ii)-(iii), 3A's notice paragraph, and 3D) **[V]**.
  So a third-party service must lease whole Macs, at least
  24 hours each, to one lessee, with notice to Apple. Per-action VM leases from
  different customers on one Mac are not permitted.
- Simulators run inside macOS; they are not iOS VMs, so the licence's ban on running
  iOS "in virtual operating system environments" is not what a simulator is **[A]**.

## 3. What stays on bare metal

### 3.1 The native driver today

On `main`, `kbf-driver-native` runs an action without the network under
`/usr/bin/sandbox-exec` with the profile `(allow default) (deny job-creation)
(deny lsopen) (deny network*)` plus loopback and Unix-socket allows; an action that
asks for the network runs **without** `sandbox-exec`
(`crates/kbf-driver-native/src/network.rs`) **[V]**. Open
[#77](https://github.com/komira-ai/komira-build-farm/pull/77) sandboxes every action.
Each lease gets its own scratch directory, its process tree is killed at the end, and
removal gives back permissions the action took away and clears the immutable and
append-only flags (`crates/kbf-driver-native/src/lib.rs`) **[V]**. Clearing ACLs on
removal is part of open #77.

macOS refuses to apply a sandbox inside a sandbox: a tool that runs `sandbox-exec`
itself fails with `sandbox-exec: sandbox_apply: Operation not permitted`
([swift-package-manager#7098](https://github.com/swiftlang/swift-package-manager/issues/7098),
[rules_swift#1202](https://github.com/bazelbuild/rules_swift/issues/1202)) **[V]**.
Bazel's macOS sandbox has the same limit
([Bazel sandboxing](https://bazel.build/docs/sandboxing)) **[V]**.

### 3.2 Do Swift and Xcode builds need a VM?

**Compiling does not. Running a simulator or a UI does.** Every Apple tool that nests
a sandbox has a switch to turn its own off, and kbf's outer sandbox stays on.

| Work | Where it runs | What the action's command line must carry |
|---|---|---|
| `swiftc`, `clang`, `ld` called directly, no macros (Buck2 and Bazel Apple rules) | bare metal | nothing **[A]** (Bazel runs them under its macOS sandbox) |
| `swiftc` with macros (including `@Observable`, `#Preview`) | bare metal | `-disable-sandbox`, in the Swift driver since Swift 5.10 ([swift-driver#1493](https://github.com/swiftlang/swift-driver/pull/1493)) **[V]**. rules_swift sends it by default on macOS ([feature_names.bzl](https://github.com/bazelbuild/rules_swift/blob/master/swift/internal/feature_names.bzl)) **[V]** |
| `swift build`, `swift test` (logic tests) | bare metal | `--disable-sandbox` ("Disable the sandbox when executing subprocesses", [Options.swift](https://github.com/swiftlang/swift-package-manager/blob/main/Sources/CoreCommands/Options.swift)) **[V]**, plus `-Xswiftc -disable-sandbox` when macros are used: the compiler sandboxed macro plugins regardless of SwiftPM's `--disable-sandbox`, and the driver's `-disable-sandbox` was the fix ([SwiftPM#7098](https://github.com/swiftlang/swift-package-manager/issues/7098)) **[V]**. Packages resolved beforehand into the inputs |
| `xcodebuild build` / `build-for-testing` of Swift packages, frameworks and macOS targets (unsigned) | bare metal | `-derivedDataPath` inside the lease ([xcodebuild(1)](https://keith.github.io/xcode-man-pages/xcodebuild.1.html)) **[V]**; `OTHER_SWIFT_FLAGS='$(inherited) -disable-sandbox' -IDEPackageSupportDisableManifestSandbox=1 -IDEPackageSupportDisablePluginExecutionSandbox=1`, reported working by a user in Homebrew discussions ([Homebrew](https://github.com/orgs/Homebrew/discussions/59)) **[V]**; `-skipMacroValidation -skipPackagePluginValidation` **[A]** until checked with `xcodebuild -help` on our Xcode; `CODE_SIGNING_ALLOWED=NO` **[A]**; `ENABLE_USER_SCRIPT_SANDBOXING` to be tested, `NO` if it nests **[A]** |
| Unsigned iOS Simulator builds of apps with storyboards or asset catalogs (`xcodebuild` runs `ibtool`, `actool`) | VM until the `ibtool`/`actool` probe shows bare metal works | the same flags as the row above. `ibtool` may use CoreSimulator device sets **[A]** (the [Apple forums](https://developer.apple.com/forums/thread/76989) thread is about Interface Builder in Xcode 8.3.2, not the `ibtool` command line) |
| XCTest or XCUITest on a simulator, macOS UI tests | **VM** | the image has the simulator runtimes (they live in VM images only) and an auto-logged-in user |
| Signing with a real identity, archive, notarize | not on the shared pool | a keychain in a user session; from a non-GUI session (SSH) `codesign` fails with `errSecInternalComponent` ([Apple forums](https://developer.apple.com/forums/thread/685967)) **[V]**, and from a launch daemon **[A]**. A separate release lane, out of scope here |

Rules that follow:

- **kbf does not rewrite commands.** The flags are part of the action and so of its
  digest; the build rules or the client send them. A shim in place of `sandbox-exec`
  would not work, since the tools call it by absolute path **[A]**.
- **Turning the inner sandboxes off must not lose anything.** SwiftPM's sandbox keeps a
  manifest or plugin from writing outside its cache; kbf's profile today allows every
  write the daemon's user can make. Phase 1 therefore denies writes outside the lease
  directory, its `TMPDIR` and a per-lease `HOME`, and adds the per-lease user
  ([section 9](#9-phased-plan)) **[A]** design. `swiftc`, `clang` and `xcodebuild` write
  module caches and logs under the per-user cache folder (`/var/folders/.../C`) and
  `~/Library`, so the action also sets `-module-cache-path` /
  `CLANG_MODULE_CACHE_PATH` and the per-user cache folder inside the lease **[A]**.
- **Several Xcodes on one host** are selected per action with `DEVELOPER_DIR`
  ([xcode-select(1)](https://keith.github.io/xcode-man-pages/xcode-select.1.html))
  **[V]**. On `main`, `xcode` is already an exact capability key with one value, and a
  report with two `xcode` entries is refused as repeated (`crates/kbf-caps/src/matching.rs`,
  `crates/kbf-caps/src/report.rs`) **[V]**. Phase 1 therefore changes `xcode` in
  `kbf-caps` to a set the node reports, matched by membership: the action asks for one
  build and any node that has it serves it. A VM image holds exactly one Xcode, and its
  digest names it.
- **The Mac node provisioning design ([mac-node-provisioning.md](mac-node-provisioning.md)) reflects these points:** several
  Xcodes per host instead of "exactly one Xcode (or one Command Line Tools version) per
  pool"; simulator runtimes live in VM images only, never in the host profile; and GUI
  work runs in the VM lease, whose guest logs itself in. Host auto-login is off at rest.
  One exception: bare-metal GPU tests that install the desktop app need a GUI session
  on the host ([section 8.1](#81-on-bare-metal-never-in-a-vm)); for that one
  whole-machine lease a root helper, `kbf-mac-session`, sets auto-login and clears it
  afterwards ([fleet-updates.md](fleet-updates.md)). "There is no disk
  image" stays
  true for the host, but VM golden images now live on Mac nodes.

Four probes on one Mac settle the **[A]** rows above before phase 1 closes:
`xcodebuild` with user-script sandboxing on, inside kbf's profile; `ibtool`/`actool`
for iOS from the daemon's role user; `simctl boot` from that user (expected to fail);
and writes of `swiftc`, `clang` and `xcodebuild` under the write-deny profile. Whether
`swift build --disable-sandbox -Xswiftc -disable-sandbox` builds a package with macros is
a confirmation, not an open question.

## 4. VM sizing

### 4.1 What Virtualization.framework does with CPU and memory

- **vCPUs** are set by `cpuCount`, which must lie between `minimumAllowedCPUCount` and
  `maximumAllowedCPUCount`
  ([cpuCount](https://developer.apple.com/documentation/virtualization/vzvirtualmachineconfiguration/cpucount))
  **[V]**. Apple publishes no numbers for the bounds; the daemon reads them at start.
  A vCPU is a host thread scheduled by macOS, not a pinned core, and it runs mostly on
  performance cores **[A]** (third-party measurements on an M1 Max; to be measured on
  ours). So vCPUs compete with bare-metal compiles for the same cores.
- **Memory** is `memorySize`, "a contiguous block of virtual memory that the host
  system reserves, but doesn't allocate immediately"
  ([memorySize](https://developer.apple.com/documentation/virtualization/vzvirtualmachineconfiguration/memorysize))
  **[V]**. A balloon device can only *ask* the guest to give memory back; "if it
  doesn't return any memory pages, the virtual machine leaves the guest's memory size
  unchanged"
  ([balloon](https://developer.apple.com/documentation/virtualization/vzvirtiotraditionalmemoryballoondevice))
  **[V]**. For Linux guests the host's cost has been measured to ratchet up to the
  most the guest ever touched, with no return after inflating the balloon
  ([arcbox](https://arcbox.dev/blog/macos-vm-memory-ratchet)) **[V]**; a macOS guest
  fills free memory with file cache during a build, so it will reach its full size
  **[A]**. **kbf books a VM's full `memorySize`.**
- **Image minimums**: each restore image states a minimum CPU count and memory size
  ([requirements](https://developer.apple.com/documentation/virtualization/vzmacosconfigurationrequirements/minimumsupportedmemorysize))
  **[V]**. Tart never gives a macOS VM fewer than 4 vCPUs, "because otherwise VMs are
  frequently freezing" ([Tart VM.swift](https://github.com/openai/tart/blob/main/Sources/tart/VM.swift))
  **[V]**.
- **No nested virtualization** in a macOS guest: the only switch,
  `isNestedVirtualizationSupported`, is on the generic (Linux) platform and needs M3
  or later
  ([docs](https://developer.apple.com/documentation/virtualization/vzgenericplatformconfiguration/isnestedvirtualizationsupported))
  **[V]**. A VM lease cannot start a VM.

### 4.2 What a VM's work needs

| Part | Memory | Source |
|---|---|---|
| macOS guest, idle | 3-4 GiB | **[A]**, to be measured |
| `xcodebuild` on a mid-size app | 5-8 GiB | [third-party](https://www.theodorehq.com/shiny/blog/xcode-memory-mac) **[A]** |
| One booted simulator | 1-4 GiB | third-party reports differ **[A]** |
| Test runner | about 1 GiB | **[A]** |
| **Total, one or two simulators** | **13-21 GiB** | **[A]** |

For comparison, GitHub's hosted arm64 macOS runners, which are VMs, have 3 CPUs and
7 GB; the xlarge arm64 runner has 5 CPUs and 14 GB
([standard](https://docs.github.com/en/actions/reference/runners/github-hosted-runners),
[larger](https://docs.github.com/en/actions/reference/runners/larger-runners)) **[V]**.
Cirrus Labs' Xcode image templates use 4 CPUs and 8 GB
([templates](https://github.com/cirruslabs/macos-image-templates)) **[V]**. 8 GB is
too tight for XCUITest of a real app with a simulator **[A]**.

### 4.3 Options for a 96 GB, 28 or 32 core Mac Studio

The example node is a Mac Studio with an M3 Ultra: 28 cores (20 performance, 8
efficiency) or 32 cores (24 + 8), 96 GB
([Apple](https://support.apple.com/en-us/122211)) **[V]** for the 28-core split, **[A]**
for 32. The daemon today reports `cpus` = `hw.ncpu`, efficiency cores included, and
subtracts nothing (`crates/kbf-daemon/src/report.rs`) **[V]**.

Rules common to every option, all **[A]** design:

- **Host floor:** 2 cores and 8 GiB are not offered (macOS, `kbf-daemon`, the VM
  helpers, page cache). The node offers 26 or 30 cores and 88 GiB. The report keeps
  `cpus` and `mem_gib` as the whole machine, as
  [capabilities.md](capabilities.md) defines them; the server subtracts the floor from
  the node's capacity before placement (the planned "Protected floors" of
  [capabilities.md](capabilities.md#planned) and [scheduler.md](scheduler.md)).
- **A running VM books** its vCPUs as cores and its memory plus 1 GiB for the helper
  process: 6 cores and 17 GiB by default, 4-12 cores and 9-33 GiB within the bounds.
- **An idle slot books nothing:** its CPU and memory serve bare-metal leases.
- **No overcommit** of memory. CPU is booked in full too: a starved simulator makes UI
  tests time out, and quality comes before throughput. Booking is admission control;
  the native driver sets no CPU limit on bare-metal actions.

| | **A: 2 × medium (default)** | B: 2 × large | C: sized per action |
|---|---|---|---|
| Each VM | 6 vCPU, 16 GiB | 8 vCPU, 24 GiB | action asks 4-12 vCPU, 8-32 GiB |
| Booked by both VMs | 12 cores, 34 GiB | 16 cores, 50 GiB | varies |
| Left for bare metal, 28-core, both VMs running | 14 cores, 54 GiB | 10 cores, 38 GiB | varies |
| Left for bare metal, 32-core, both VMs running | 18 cores, 54 GiB | 14 cores, 38 GiB | varies |
| Left for bare metal, no VM running | 26 / 30 cores, 88 GiB | same | same |
| Fits | Xcode + 1-2 simulators + XCUITest | parallel simulator clones, large Swift apps | anything within the bounds |
| Costs | big apps may need more | a third of bare-metal capacity while both run | fragmentation; capacity varies |

**Recommendation:** C as the mechanism, with A as the default. An action that says
nothing gets a 6 vCPU, 16 GiB guest (6 cores and 17 GiB booked); one that needs more
asks for it, up to 12 vCPU and 32 GiB (both bounds are server configuration). The VM
is configured at boot from what was
booked, so one golden image serves every size.

**Disk:** a VM lease also needs local disk for its clone's growth (DerivedData,
simulator data), about 40 GiB **[A]**, checked against the node's free scratch space
before the VM starts. Golden images live on the same APFS volume as the clones,
because `clonefile` fails with `EXDEV` across file systems, and each file of the image
is cloned on its own, not the directory: "Cloning directories with these functions is
strongly discouraged"
([clonefile(2)](https://keith.github.io/xcode-man-pages/clonefile.2.html)) **[V]**.

## 5. How the scheduler knows a VM is needed

The action says so, the node says what it can do, and the scheduler books it.

### 5.1 Platform properties (partly planned)

| Key | Kind | Values | Meaning | On `main` |
|---|---|---|---|---|
| `kbf-lease` | reserved | `vm` (new; beside `action` and `whole_machine`) | run in a fresh macOS VM | **planned**: `vm` is refused as an unknown lease kind |
| `vm.image` | capability, membership | `<name>@sha256:<recipe digest>` | the golden image, named by the digest of its pinned inputs ([macos-vm-guests.md](macos-vm-guests.md#5-image-identity)); a value without a digest is refused, as for container images | matched on the digest; no daemon reports one yet, so a request naming one matches no worker |
| `kbf-book-cpus` | reserved | whole cores (named like `cpus` and `vm.max_cpus`) | what the lease books. For `kbf-lease=vm` it is also the guest's vCPU count: 4-12, default 6 | booked on an `action` lease; the VM bounds are planned |
| `kbf-book-mem-gib` | reserved | GiB | what the lease books. For `kbf-lease=vm`: 9-33, default 17, and the guest's `memorySize` is this minus 1 GiB for the helper (8-32 GiB, default 16) | booked on an `action` lease; the VM bounds are planned |

**One size convention for every lease kind.** `kbf-book-cpus` and `kbf-book-mem-gib`
are what a lease books, on bare metal and in a VM alike: on an `action` lease they
replace the 1 core and 1 GiB every action books today (and so raise the native
driver's memory kill, which is 150% + 512 MiB of the booking); on a `vm` lease they
size the guest. A `whole_machine` lease books the whole node, so the front refuses
either key on it; without the keys an `action` lease books 1 core and 1 GiB
(`DEFAULT_RESOURCES`, `crates/kbf-front/src/execution.rs`) **[V]**. They are not `cpus`
and `mem_gib`: those two are capability keys that
ask for a node whose *whole machine* has at least that much, and they book nothing.
They are also not `kbf-cpu`, which is planned as the value `dedicated` (whole physical
cores for quiet performance runs), not a size. When learned sizes (`kbf-estimator`,
[scheduler.md](scheduler.md)) land, an action without the keys gets a learned size
instead of the default, and an explicit key wins over the estimate. A VM's size is
fixed at boot, so a VM lease always uses the key or the VM default, never an estimate.

**The cost: they split the action cache.** Like every platform property, the two keys
are part of the action digest, so the same action with a larger memory booking is a
different cache entry: raising memory changes no output, but the first run after the
change misses. That is the reason QoS is a request header and never a property
([capabilities.md](capabilities.md), [scheduler.md](scheduler.md#qos)). The keys stay
anyway, because Bazel and Buck2 have no other per-action channel to the farm. Learned
sizes should make them rare: an action needs one only when the estimate is wrong for it.

**`vm.image` is a capability key**, the same name in the request and the report, like
`xcode` and `os_image`. Its comparison is membership on the digest only: the request
carries `<name>@sha256:<digest>`, the node reports the digests it holds, and the
matcher compares the digest part. It is not a reserved key, and the scheduler has no
image rule of its own.

All of these are part of the action digest, which is wanted: a result from a VM and
one from bare metal never share a cache entry. `kbf-book-cpus` and `kbf-book-mem-gib`
are among the reserved keys the matcher skips (`RESERVED_KEYS`,
`crates/kbf-caps/src/matching.rs` **[V]**). The front checks them and turns them into a
booking.

### 5.2 Node report (planned)

| Entry | Meaning |
|---|---|
| `drivers` gains `vm` | the existing repeated `drivers` key ([capabilities.md](capabilities.md)); `vm` is listed only when Virtualization.framework is usable: a check that boots nothing passes at daemon start and periodically ([macos-vm-guests.md](macos-vm-guests.md#8-the-launch-daemon-risk)) |
| `vm.slots=2` | how many VMs may run at once; fills the `vms` dimension |
| `vm.image=<recipe digest>`, one per image | golden images already on the node's disk whose file manifest re-verifies |
| `vm.max_cpus`, `vm.max_mem_gib` | the framework's bounds, read at start |

`vm.slots`, `vm.max_cpus` and `vm.max_mem_gib` are report-only: an action cannot ask
for them, and `vms` is booked only through `kbf-lease=vm`.

No new key names the lease kinds a node serves: the scheduler maps each lease kind to
the drivers that serve it and reads `drivers`. `action` maps to `native` or
`container` only; `vm` maps to `vm`; `whole_machine` maps to `native-whole-machine`,
the `drivers` value of the planned bare-metal whole-machine runtime (the per-lease-user
driver of [fleet-updates.md](fleet-updates.md), phase P4). A daemon lists
`native-whole-machine` only when `kbf-mac-session` is present, so a Mac before that
phase, or any other native daemon, never looks able to serve `whole_machine`. On
`main` no driver serves
`whole_machine`: the native and container drivers serve `action` only
(`crates/kbf-driver-native/src/runtime.rs`, `crates/kbf-driver-native/tests/lease.rs`)
**[V]**. That is the planned "Drivers in placement" of
[capabilities.md](capabilities.md#planned) and [scheduler.md](scheduler.md).

### 5.3 Booking and placement (planned)

On `main`, `Resources` has four dimensions (`cpu_millis`, `memory_bytes`, `gpus`,
`vms`, `crates/kbf-types/src/work.rs`), with `vms` checked by `fits` but booked and
filled by nothing yet (`kbf-lease=vm` is refused). An `action` lease books 1 core and
1 GiB, or its `kbf-book-cpus` and `kbf-book-mem-gib` (`crates/kbf-front/src/execution.rs`).
The lease kind is in the scheduler's request: a worker is feasible only if its `drivers`
serve the kind, and a `whole_machine` lease books the whole worker and, when it fits
nowhere, holds one worker in queue order ([scheduler.md](scheduler.md#placement)).
Otherwise placement is first fit in worker-name order **[V]**. The changes still
planned:

1. **A `vms` dimension** in `Resources`, filled from `vm.slots`, checked by `fits` like
   `gpus`. A VM lease books `vms=1`, `kbf-book-cpus` and `kbf-book-mem-gib`. The
   scheduler therefore never asks for a third VM, and the driver still maps
   `VZErrorVirtualMachineLimitExceeded` to an infrastructure failure that is retried
   elsewhere.
2. **The `vm` kind in the request.** For `action` and `whole_machine` this is on
   `main` (above); what remains is the `vm` kind, feasible only on a node whose
   `drivers` include `vm`. A `whole_machine` lease books the node's full capacity, so
   once `vm.slots` fills `vms` it books the node's `vms` too, and no VM is placed
   beside it.
3. **Image locality is a hard requirement.** `vm.image` matches only nodes that report
   the digest. An image is tens of gigabytes; it is never fetched at action time.
   Images are placed on nodes ahead of time ([section 7](#7-images)).
4. **Reservation for VM leases.** A VM lease needs 4-12 cores and 9-33 GiB in one piece
   (6 cores and 17 GiB by default); a stream of 1-core actions would starve it. When a
   VM lease at the head of its queue fits no node, the scheduler reserves the best
   candidate (a node with a free slot and the image) and stops placing new bare-metal
   work there until the VM fits, bounded by a timeout. Running leases finish; nothing
   is killed. A reservation follows the queue's QoS order and is not a quota: it keeps
   off only work *less urgent* than the lease holding it, so a `batch` VM reservation
   never keeps `ci` or `interactive` work off the node. A GPU test's whole-machine lease
   ([section 8.1](#81-on-bare-metal-never-in-a-vm)) drains a Mac the same way, under
   the same rule.

This is all pure scheduler code, testable in the simulator crate.

## 6. The VM driver: one VM per lease, never reused

`kbf-driver-vm` (new, macOS only) implements the daemon's `Runtime` trait. It runs one
helper process, `kbf-vmm`, per VM, so a VM crash cannot take the daemon down and
killing the helper stops the VM. The helper is a Rust binary on
[`objc2-virtualization`](https://crates.io/crates/objc2-virtualization) (licence "Zlib
OR Apache-2.0 OR MIT") **[V]**, signed with the `com.apple.security.virtualization`
entitlement Apple requires
([sample](https://developer.apple.com/documentation/virtualization/running-macos-in-a-virtual-machine-on-apple-silicon))
**[V]**.

`kbf-vmm` runs as a dedicated non-admin uid outside the root helpers' group: a
throwaway lease user of its own for each VM, from `kbf-mac-session user-create` and
started with `run` ([macos-vm-guests.md](macos-vm-guests.md#6-the-run-as-uid-and-the-control-channel)),
never as the daemon's role account; otherwise a guest escape would reach the root helpers (fleet-updates security
design, [fleet-updates-security.md](fleet-updates-security.md)). So `kbf-mac-session`'s
`run` verb must exist by phase 2 of this design; [fleet-updates.md](fleet-updates.md) ships it in its phase P1
on Macs.

Per lease:

1. **Prepare.** Fetch the action's inputs into the lease directory, as the native
   driver does. Clone the golden image's files with `clonefile`: copy-on-write, so the
   clone costs no space until written ([clonefile(2)](https://keith.github.io/xcode-man-pages/clonefile.2.html))
   **[V]**. Give the clone a new machine identifier and auxiliary storage: "each should
   have a unique auxiliaryStorage and machineIdentifier"
   ([VZMacPlatformConfiguration](https://developer.apple.com/documentation/virtualization/vzmacplatformconfiguration))
   **[V]**.
2. **Configure.** From the booking, so `Start` needs no new field: `cpuCount` is the
   booked cores, and `memorySize` is the booked memory minus the 1 GiB booked for the
   helper (16 GiB for the default 17 GiB booking), so the guest is not 1 GiB too large.
   Devices: one virtio socket; two virtiofs shares, one read-only for control files
   and one read-write holding the execution root
   ([macos-vm-guests.md](macos-vm-guests.md#33-how-outputs-leave-the-guest)); no
   network device unless network-on VM leases are decided
   ([macos-vm-guests.md](macos-vm-guests.md#4-networking)); a graphics device
   (`VZMacGraphicsDeviceConfiguration`) with one display and no view on the host.
3. **Boot.** Wait for the guest agent to report ready over the socket. Boot time is not
   charged to the action's timeout; the boot timeout is 120 s **[A]** until boot is
   measured (expected 20-60 s **[A]**).
4. **Run.** Send argv, environment and working directory over the socket to
   `kbf-guest`, a small self-contained Rust binary (dynamically linked only to
   libSystem, since macOS does not support fully static executables) baked into the
   image, running in the guest's
   auto-logged-in user session. It runs the command in the execution root on the
   read-write share, sends stdout and stderr as framed chunks over the socket, and
   returns the exit code and usage; `kbf-vmm` writes stdout and stderr to two files in
   the lease directory, outside the execution root
   ([macos-vm-guests.md](macos-vm-guests.md#33-how-outputs-leave-the-guest)). Not SSH:
   no keys, no `sshd`, no network needed.
5. **Collect.** Read the declared outputs from the host side of the read-write share
   with `kbf-outputs`, which follows no symlinks.
6. **Destroy.** `stop` the VM (immediate, no clean shutdown), end the helper, delete the
   clone and the lease directory, and check nothing is left. Kill, timeout, cancel and
   fence all end here.

Rules:

- **Never reuse.** No warm pool, no VM kept between leases. The freshness test every
  driver must pass applies: a marker left by one lease is never seen by the next.
- **Fencing:** every lease self-fences today; a VM with the network or a GUI always
  will.
- **A risk to test first:** whether Virtualization.framework starts a macOS VM from a
  launchd daemon with no user logged in on the host **[A]** (probe P5,
  [macos-vm-guests.md](macos-vm-guests.md#8-the-launch-daemon-risk)). If the probe fails, the
  fallback is an open decision: a session of a dedicated non-admin VM user, held while
  the node serves VM leases. That is an exception to "auto-login off at rest" and needs
  a ruling; [mac-node-provisioning.md](mac-node-provisioning.md)'s `autologin` check and `kbf-mac-session`'s user-id range rule
  ([fleet-updates.md](fleet-updates.md))
  would then have to allow that user. A per-lease switch is impossible, because it
  restarts userspace under running leases.
- **Not used:** Tart and Orchard are under FSL-1.1-ALv2, which forbids a competing use
  and becomes Apache-2.0 only two years after each release
  ([LICENSE](https://github.com/openai/tart/blob/main/LICENSE)) **[V]**. kbf is
  Apache-2.0 and depends only on open-source licences. Tart's code is read as a
  reference, not copied.

## 7. Images

A golden image is a directory: the disk image, auxiliary storage, the hardware model
and a configuration file. kbf builds images with its own tool, `kbf-vmm` driving
`VZMacOSInstaller`, never with Packer or Tart: Packer is under BUSL-1.1 and its Tart
plugin depends on Tart (FSL), so neither is open source. Cirrus's Packer templates are
read as a reference for the steps. An image is built in a fixed order:

1. **macOS**: a pinned restore image (`.ipsw`) by URL and SHA-256, installed with
   [`VZMacOSInstaller`](https://developer.apple.com/documentation/virtualization/vzmacosinstaller)
   **[V]**. Cirrus pins the full IPSW URL in its templates
   ([vanilla-tahoe.pkr.hcl](https://github.com/cirruslabs/macos-image-templates/blob/main/templates/vanilla-tahoe.pkr.hcl))
   **[V]**.
2. **Settings**: auto-login of a non-admin test user, sleep and screen lock off,
   updates off, `kbf-guest` as a LaunchAgent of that user, set up by a first-boot
   launch daemon inside the guest
   ([macos-vm-guests.md](macos-vm-guests.md#2-guest-first-boot-setup)).
3. **Xcode**: one pinned `.xip` by SHA-256, then `xcodebuild -runFirstLaunch`, then the
   pinned simulator runtimes (`xcodebuild -downloadPlatform iOS -buildVersion <v>`),
   checked against an expected list with `xcrun simctl list runtimes`
   ([xcode.pkr.hcl](https://github.com/cirruslabs/macos-image-templates/blob/main/templates/xcode.pkr.hcl))
   **[V]**.
4. **Digests**: the SHA-256 of the recipe (the pinned inputs) names the image, and
   clients route on it in `vm.image`; the SHA-256 of a manifest of the image's files is
   the per-node content digest, which attests the disk but is not routed on, because
   the installer's output is not byte-reproducible **[A]**
   ([macos-vm-guests.md](macos-vm-guests.md#5-image-identity)). Host-identity probes
   ([mac-node-provisioning.md](mac-node-provisioning.md), §3.1) are for bare-metal Xcodes.

Size: Cirrus's Xcode template uses a 140 GB disk **[V]**; plan 100-140 GB per golden
image **[A]**. A node keeps at most two (Xcode N and N-1) **[A]**, 200-280 GB, plus up
to 40 GiB per running clone (up to 80 GiB with both VMs running).

Distribution, two options (a decision, [section 10](#10-open-decisions); the
licence position is set out in
[macos-vm-guests.md](macos-vm-guests.md#54-distribution-and-the-licences)):

- **Build once, ship by digest** through the CAS as a chunked blob, imported on each
  Mac by an operator command, then reported as `vm.image`. One build, identical bytes
  everywhere. Needs chunked upload, which `kbf-segments` has and the cache does not use
  yet. This copies an image holding macOS and Xcode from one Mac to another; whether
  the macOS and Xcode licences allow that has not been checked by a lawyer **[A]**.
- **Build on each Mac** from the same pinned inputs. No large transfers; the bytes
  differ per Mac, so the content digest is per node and clients route on the recipe
  digest.

macOS 27 adds DiskImageKit with read-only base layers shared by several VMs plus
copy-on-write overlays ([DiskImageKit](https://developer.apple.com/documentation/diskimagekit))
**[V]**. Apple documents it for Swift only **[V]**; `clonefile` is enough to start.

## 8. GPU

### 8.1 On bare metal, never in a VM

**GPU work never runs in a VM.** A stock macOS guest runs LLM inference at about 4-14%
of bare-metal speed (7 to 23 times slower), which is not acceptable. GPU tests, including
tests that install the desktop app and drive it with a local LLM, run on bare metal with
the whole Mac, one at a time. There is no GPU-in-VM option.

The evidence: on Apple silicon the GPU shares the system's memory: "The CPU and GPU
have direct access to the same memory pool"
([MLX](https://ml-explore.github.io/mlx/build/html/usage/unified_memory.html)) **[V]**.
A macOS guest gets a paravirtualized Metal device. One published test on an M1 Ultra
(llama.cpp) found the stock guest's Metal device reports an older GPU family (Apple GPU
family 5) without SIMD-group matrix or bfloat16 support, and measured it at about 4-14%
of bare-metal speed (7 to 23 times slower): generation 4.4% (TinyLlama 1.1B, 12.63
tokens/s against 286.71) and 6.5% (Gemma 4 12B), prompt processing 8.9% and 13.8%
([measurements](https://github.com/trycua/cua/blob/main/blog/gpu-passthrough-macos-vms.md))
**[V]** for their numbers, **[A]** for ours. MLX may not run in a stock guest at all
**[A]**.

So a GPU test is a whole-machine lease:

- It asks for `kbf-lease=whole_machine` and `gpu=1`. No other lease, bare-metal or VM,
  runs on the Mac while it does: the GPU and the unified memory are the test's alone,
  macOS has no way to partition the GPU between processes **[A]**, and performance
  numbers from a shared machine are noise. The scheduler drains the Mac first
  ([section 5.3](#53-booking-and-placement-planned)); GPU tests on one Mac run one at a
  time.
- On macOS the front refuses `gpu`≥1 unless `kbf-lease=whole_machine` (and always with
  `kbf-lease=vm`), with `INVALID_ARGUMENT`, so GPU work cannot land beside other work.
- The daemon reports `gpu=1` on Apple silicon. Today it reports 0
  (`crates/kbf-daemon/src/report.rs`) **[V]**.
- The model must fit: weights + KV cache + activations + process. Example,
  Qwen2.5-7B-Instruct at 4 bits: 4.3 GB of weights; its KV cache is 2 × 28 layers ×
  4 KV heads × 128 × 2 bytes = 56 KiB per token, 0.47 GB at 8k tokens; about 8 GiB.
- macOS caps GPU-wired memory (`iogpu.wired_limit_mb`,
  [mlx-lm](https://github.com/ml-explore/mlx-lm/blob/main/README.md)) **[V]**; the
  default is about three quarters of RAM on large Macs **[A]**. The daemon reports the
  cap; a test checks that its model fits under it and fails loudly if it does not. An
  action never changes the sysctl.
- A test that installs the desktop app needs a GUI session on the host, and what the
  install leaves behind must not reach the next lease. That isolation, and fleet-wide
  updates, device management and UI, are designed in
  [fleet-updates.md](fleet-updates.md).

### 8.2 Testing a local LLM on the GPU

A language model's output cannot be checked against one fixed string unless the
runtime, batch size and hardware are fixed: low-precision arithmetic makes near-ties
common, and a change in batch size or kernel changes the
order of floating-point sums, which can flip one token and everything after it
([Thinking Machines](https://thinkingmachines.ai/blog/defeating-nondeterminism-in-llm-inference/),
[llama.cpp#3014](https://github.com/ggml-org/llama.cpp/issues/3014)) **[V]**. The forward
pass itself is run-to-run deterministic at a fixed batch size (same sources), which
layer 2 below relies on. Metal's
`relaxed` and `fast` math modes allow "aggressive, potentially lossy assumptions"
([MTLMathMode](https://developer.apple.com/documentation/metal/mtlmathmode)) **[V]**, so
the runtime build is part of the result's identity. One study found mlx-lm
bit-identical across 20 runs of 50 prompts at temperature 0, one request at a time, on
one machine ([mlx-lm discussion](https://github.com/ml-explore/mlx-lm/discussions/1017))
**[V]**; whether that holds on our Macs is **[A]** until tested.

The fix is to split the testing into four layers, cheapest and most exact at the
bottom:

| Layer | What it checks | Pass rule | Example | Planted defect that must turn it red |
|---|---|---|---|---|
| **1. Kernels** | each GPU operation against a float32 CPU reference on seeded random inputs | exact for integer work; per-type tolerance otherwise (starting at bf16 rtol 1e-2, fp32 rtol 1e-5, then set at twice the worst error seen over 10,000 inputs) | dequantize a 4-bit block of 64 weights: exactly equal; a 1×4096 × 4096×4096 bf16 matmul: every element within `\|got − ref\| ≤ atol + rtol·\|ref\|`, rtol 1e-2 and atol scaled by √K·max\|input\| (an element-wise relative error is unstable for outputs near zero; the error relative to each row's norm is the alternative) | swap two scale factors in dequantize |
| **2. Golden outputs, tiny model** | a pinned 0.3 GB model (Qwen2.5-0.5B-Instruct, 4-bit) gives the recorded tokens | first a determinism check: 20 prompts in 5 fresh processes, identical tokens. If it passes, exact tokens; if not, top-5 log-probabilities within 0.05, failing only where the golden's margin was clear | "List the first five primes as JSON:", greedy, 32 tokens; golden keyed by model digest, runtime digest, chip/GPU family and macOS build (the hardware key below), since Metal kernels are compiled per GPU family and macOS build; across keys only the log-prob comparison applies; updated only in a reviewed commit | change `rope_theta`, or skip RoPE on one layer: tokens diverge at once (the model's config has no `rope_scaling` to drop) |
| **3. Behaviour** | properties, not strings: parses as JSON, matches a schema, is a valid tool call, answers yes/no correctly against labels | run N different inputs; pass when the 95% Wilson lower bound of the pass rate is at or above the committed bar; choose N so a model at its expected rate passes at least 95% of the time; never retry a failed case | 200 pinned texts through a 7B model; each output must parse and every relation's ends must be in its entity list; bar 0.90, so at least 189 of 200 must pass | break the prompt's JSON instruction: the rate collapses |
| **4. Performance** | load time, time to first token on a 512-token prompt, decode tokens/s over 256 tokens, peak memory | median of 5 runs; fail if more than a set margin (start at 7%) below the trailing median on the same hardware key | 7B 4-bit decode tokens/s on one Mac model and macOS build | add a sleep in the decode loop |

The Wilson bounds that set N **[V]** (arithmetic):

| Passes / N | 95% interval |
|---|---|
| 50 / 50 | 0.929 - 1.000 |
| 48 / 50 | 0.865 - 0.989 |
| 95 / 100 | 0.888 - 0.978 |
| 96 / 100 | 0.902 - 0.984 |
| 188 / 200 | 0.898 - 0.965 |
| 189 / 200 | 0.904 - 0.969 |

Pass when the 95% Wilson lower bound is at or above the bar. Choose N so a model at its
expected rate passes at least 95% of the time. For bar 0.90 and an expected rate of
about 0.97, that is N ≈ 200 (189/200 needed; a 0.97 model passes 98% of runs); at
N = 100 the bar needs 96/100, which a 0.97 model passes only 82% of the time and a 0.95
model 44%, so the gate would be flaky. With 50 cases, one failure already fails it. The
alternative is a bar about 5 points below the measured baseline. The lower end of a
two-sided 95% interval is a 97.5% one-sided bound.

Rules for all four layers:

- **Hermetic.** Weights and the runtime (the MLX or llama.cpp build) are action inputs
  by digest, never downloaded in the action, which runs with the network off. Loopback
  stays open, so a test can start a local model server; the process-tree kill reaps
  it.
- **A fresh process per case,** or the runtime's prompt cache off: Ollama's MLX runner
  returned different outputs for the same prompt after restoring its prompt cache
  (fixed by disabling that restore)
  ([ollama#16860](https://github.com/ollama/ollama/issues/16860)) **[V]**.
- **Caching.** Layers 1-3 cache normally; a cache hit for layer 3 replays a measured
  rate on identical inputs. The macOS build and chip must be in the platform so a
  change re-runs them. Layer 4 sets `Action.do_not_cache`
  ([remote_execution.proto](https://github.com/bazelbuild/remote-apis/blob/main/build/bazel/remote/execution/v2/remote_execution.proto))
  **[V]**.
- **Numbers leave the action** in `ExecutedActionMetadata.auxiliary_metadata`
  (same proto) **[V]**: pass count, N, tokens/s, peak memory, stamped with the hardware
  key (chip, GPU cores, RAM, macOS build, runtime digest, weights digest). kbf stores a
  series per test and key; the CI layer decides about quarantine (a test whose interval
  straddles its bar keeps running and reporting but stops blocking, and an issue goes to
  its owner).
- **Never skip silently.** A missing model is a missing input and a failed action, not
  an exit 0.

## 9. Phased plan

### Phase 1: builds and unit tests on bare metal (now)

Done on `main`: the native driver with `sandbox-exec`, per-lease scratch, process-tree
kill and removal that clears the immutable and append-only flags
([#64](https://github.com/komira-ai/komira-build-farm/pull/64)), and platform routing
([#71](https://github.com/komira-ai/komira-build-farm/pull/71)), sandbox every action and ACL-proof removal
([#77](https://github.com/komira-ai/komira-build-farm/pull/77)), and the Mac node provisioning design
([mac-node-provisioning.md](mac-node-provisioning.md)).
Open: the fence clock during suspend ([#78](https://github.com/komira-ai/komira-build-farm/issues/78)),
the node id checked against the certificate ([#79](https://github.com/komira-ai/komira-build-farm/issues/79)).

| Crate | Change |
|---|---|
| `kbf-front`, `kbf-caps` | done on `main` ([#90](https://github.com/komira-ai/komira-build-farm/pull/90)): the size keys `kbf-book-cpus` and `kbf-book-mem-gib` ([section 5.1](#51-platform-properties-partly-planned)) set what an `action` lease books in place of the default 1 core and 1 GiB (`DEFAULT_RESOURCES`, `crates/kbf-front/src/execution.rs`), and the native driver's memory kill, 150% + 512 MiB of the booking (`crates/kbf-driver-native/src/config.rs`), follows the booking **[V]**; an action without `kbf-book-mem-gib` is still killed at 2 GiB, which large `swiftc` and `ld` steps exceed |
| `kbf-caps` | done on `main` ([#90](https://github.com/komira-ai/komira-build-farm/pull/90)): `xcode` is a set the node reports, one entry per Xcode build, and a request names one build, matched by membership; a request naming two `xcode` entries, or an empty one, is refused (`crates/kbf-caps/src/matching.rs`) **[V]** |
| `kbf-driver-native` | deny file writes outside the lease, `TMPDIR` and a per-lease `HOME`; set `HOME`, `TMPDIR`, the module cache paths and the per-user cache folder per lease; per-lease user; fill usage |
| `kbf-daemon` | report each Xcode build (several `xcode` entries) and SDKs; set `DEVELOPER_DIR` from the action |
| `kbf-server`, `kbf-sched` | subtract the host floor from capacity, not from the report (the planned "Protected floors"); lease kind in placement from `drivers` (the planned "Drivers in placement") |
| docs | the Swift and Xcode flags in [section 3.2](#32-do-swift-and-xcode-builds-need-a-vm), as guidance for build rules |

Exit: a real Swift package and an `xcodebuild build-for-testing` of a framework or
macOS target build on a Mac node, the four probes of section 3.2 answered, and their
logic tests pass. iOS app targets with storyboards or asset catalogs are in the exit
only if the `ibtool`/`actool` probe passes.

### Phase 2: macOS VMs

| Crate | Change | Rough size |
|---|---|---|
| `kbf-types` | `Resources.vms` | ~60 lines |
| `kbf-caps` | `vm.image`, matched by membership on the digest; `vm.slots`, `vm.max_cpus`, `vm.max_mem_gib` | ~250 |
| `kbf-sched` | kind in `Request`, reservation in QoS order; simulator tests | ~900 |
| `kbf-front` | lease kind `vm`, VM bounds on `kbf-book-cpus` and `kbf-book-mem-gib` | ~150 |
| `kbf-server` | read `vm.slots` | ~80 |
| `kbf-daemon`, `kbf-node` | a runtime that dispatches by kind across several drivers; VM flags (image directory, boot timeout) | ~370 |
| `kbf-driver-vm` (new) | section 6 | ~3,000 with tests |
| `kbf-vmm`, `kbf-guest` (new) | the VM helper, run as a dedicated non-admin uid (needs `kbf-mac-session run`, [fleet-updates.md](fleet-updates.md) phase P1), and the guest agent | ~1,000 |
| `kbf-segments`, `kbf-front` | chunked upload, if images ship through the CAS | ~400 |
| `kbf-it` | the freshness test on VMs | ~150 |

Exit: two XCUITest actions run at once on one Mac beside bare-metal builds; a third
waits; each VM is gone afterwards; measured boot time, idle guest memory and a real
app's peak are recorded and replace the **[A]** numbers in section 4.

### Phase 3: GPU

| Crate | Change |
|---|---|
| `kbf-daemon` | report `gpu=1`, GPU core count and the wired-memory cap on macOS |
| `kbf-proto`, `kbf-server` | GPUs in `Start` |
| `kbf-sched` | GPU tests as whole-machine leases with `gpu=1`: drain the Mac, one at a time |
| `kbf-driver-native`, `kbf-daemon` | the bare-metal whole-machine runtime that serves `whole_machine` (none does on `main`): the per-lease-user driver of [fleet-updates.md](fleet-updates.md), phase P4; reports `native-whole-machine` |
| `kbf-front`, `kbf-sched` | a `whole_machine` lease books the node's full cores, memory, `gpus` and `vms` |
| `kbf-daemon`, `kbf-server` | carry `auxiliary_metadata`; store series per test and hardware key |
| front / CAS | model weights as chunked CAS inputs, kept from eviction on nodes that run GPU tests |

Order inside phase 3: layers 1 and 2 with the 0.3 GB model, then layer 3 with a 7B
model, then layer 4.

## 10. Open decisions

| Decision | Options | Lean |
|---|---|---|
| Default VM size | 4 vCPU / 12 GiB; **6 / 16**; 8 / 24 | a 6 vCPU / 16 GiB guest (6 cores / 17 GiB booked), per-action up to 12 vCPU / 32 GiB |
| VM CPU booking | full vCPU count; half | full |
| Image distribution | ship by digest through the CAS; build on each Mac | build on each Mac and route on the recipe digest; shipping images waits for a legal review of the Xcode licence ([macos-vm-guests.md](macos-vm-guests.md#54-distribution-and-the-licences)) |
| Images per node | one Xcode; two (N and N-1) | two (200-280 GB) |
| `ibtool`/`actool` | VM; bare metal | bare metal if the probe passes |
| If a VM cannot start from a launch daemon | a session of a dedicated non-admin VM user held while the node serves VM leases (an exception to auto-login off at rest); no VMs on that macOS | needs a ruling; a per-lease switch is impossible ([section 6](#6-the-vm-driver-one-vm-per-lease-never-reused)) |

The open decisions about guest setup, network, signing and the control channel are in
[macos-vm-guests.md](macos-vm-guests.md#10-open-decisions).

Decided, not open: GPU work never runs in a VM; a GPU test takes the whole Mac on bare
metal, one at a time ([section 8.1](#81-on-bare-metal-never-in-a-vm)).

## 11. Verified and assumed

| Claim | Status | Source |
|---|---|---|
| The licence allows 2 macOS guests per Mac for development and testing | V | [macOS 27 SLA §2B(iii)](https://www.apple.com/legal/sla/docs/macOS27.pdf) |
| macOS refuses a VM beyond the limit | V | [VZError](https://developer.apple.com/documentation/virtualization/vzerror/code/virtualmachinelimitexceeded), [forums](https://developer.apple.com/forums/thread/729580) |
| Our single-tenant use is "development / testing" | A | not a legal opinion |
| A launch daemon cannot use the window server | V | [TN2083](https://developer.apple.com/library/archive/technotes/tn2083/_index.html) |
| Simulators need a logged-in GUI session | A | no direct source; settled by the `simctl boot` probe |
| Nested `sandbox-exec` fails | V | [SwiftPM#7098](https://github.com/swiftlang/swift-package-manager/issues/7098) |
| `swiftc -disable-sandbox`, SwiftPM `--disable-sandbox` exist | V | [swift-driver#1493](https://github.com/swiftlang/swift-driver/pull/1493), [Options.swift](https://github.com/swiftlang/swift-package-manager/blob/main/Sources/CoreCommands/Options.swift) |
| xcodebuild's IDEPackageSupport flags avoid nested sandboxes | V (reported working by a user in Homebrew discussions) | [Homebrew](https://github.com/orgs/Homebrew/discussions/59) |
| `-skipMacroValidation`, `-skipPackagePluginValidation` exist | A | to be checked with `xcodebuild -help` |
| SwiftPM `--disable-sandbox` alone does not cover macros; `-Xswiftc -disable-sandbox` does | V | [SwiftPM#7098](https://github.com/swiftlang/swift-package-manager/issues/7098) |
| `codesign` fails with `errSecInternalComponent` from a non-GUI SSH session | V; A for a launch daemon | [Apple forums](https://developer.apple.com/forums/thread/685967) |
| A third-party service must lease whole Macs, 24 hours or more, to one lessee | V | [macOS 27 SLA §3](https://www.apple.com/legal/sla/docs/macOS27.pdf) |
| User-script sandboxing nests-fails | A | probe |
| Network-on actions run without `sandbox-exec` on `main` | V | `crates/kbf-driver-native/src/network.rs` |
| VM memory is reserved, not shrinkable without guest help | V | [memorySize](https://developer.apple.com/documentation/virtualization/vzvirtualmachineconfiguration/memorysize), [balloon](https://developer.apple.com/documentation/virtualization/vzvirtiotraditionalmemoryballoondevice) |
| A macOS guest reaches its full memory | A | [arcbox](https://arcbox.dev/blog/macos-vm-memory-ratchet) is Linux guests |
| vCPUs are host threads on performance cores | A | third-party, M1 Max |
| No nested virtualization in macOS guests | V | [isNestedVirtualizationSupported](https://developer.apple.com/documentation/virtualization/vzgenericplatformconfiguration/isnestedvirtualizationsupported) |
| Each clone needs its own machine identifier | V | [VZMacPlatformConfiguration](https://developer.apple.com/documentation/virtualization/vzmacplatformconfiguration) |
| `clonefile` is copy-on-write, same file system only | V | [clonefile(2)](https://keith.github.io/xcode-man-pages/clonefile.2.html) |
| Tart's licence is FSL-1.1-ALv2 | V | [LICENSE](https://github.com/openai/tart/blob/main/LICENSE) |
| `objc2-virtualization` is Zlib/Apache-2.0/MIT | V | [crates.io](https://crates.io/crates/objc2-virtualization) |
| VZ runs from a launchd daemon with nobody logged in | A | first probe of phase 2 |
| Guest idle memory 3-4 GiB; boot 20-60 s; clone growth 40 GiB | A | to be measured |
| GitHub arm64 runners: 3 CPU / 7 GB; xlarge 5 / 14 GB | V | [GitHub](https://docs.github.com/en/actions/reference/runners/larger-runners) |
| M3 Ultra 28-core is 20P + 8E | V | [Apple](https://support.apple.com/en-us/122211) |
| Guest Metal is a reduced device, about 4-14% of bare metal for LLM work (7 to 23 times slower) | V for the cited M1 Ultra numbers; A for ours | [measurements](https://github.com/trycua/cua/blob/main/blog/gpu-passthrough-macos-vms.md) |
| MLX may not run in a stock guest | A | no bfloat16 or SIMD-group matrix in the guest's GPU family |
| DiskImageKit is documented for Swift only | V | [DiskImageKit](https://developer.apple.com/documentation/diskimagekit) |
| CPU and GPU share memory | V | [MLX](https://ml-explore.github.io/mlx/build/html/usage/unified_memory.html) |
| GPU-wired memory is capped by `iogpu.wired_limit_mb` | V that it exists; A for the default | [mlx-lm](https://github.com/ml-explore/mlx-lm/blob/main/README.md) |
| No per-process GPU partitioning on macOS | A | |
| Batch size changes LLM outputs | V | [Thinking Machines](https://thinkingmachines.ai/blog/defeating-nondeterminism-in-llm-inference/), [llama.cpp#3014](https://github.com/ggml-org/llama.cpp/issues/3014) |
| mlx-lm deterministic at temperature 0, batch 1 | V on one M4 Max; A for ours | [mlx-lm#1017](https://github.com/ml-explore/mlx-lm/discussions/1017) |
| `do_not_cache` and `auxiliary_metadata` exist in REAPI | V | [remote_execution.proto](https://github.com/bazelbuild/remote-apis/blob/main/build/bazel/remote/execution/v2/remote_execution.proto) |
| Every action books 1 core and 1 GiB; native kill at 2 GiB | V | `crates/kbf-front/src/execution.rs`, `crates/kbf-driver-native/src/config.rs` |
| A Mac reports `gpu=0` | V | `crates/kbf-daemon/src/report.rs` |
