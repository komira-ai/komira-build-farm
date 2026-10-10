# Platform properties

A build tool describes where an action may run with REAPI platform properties
(`exec_properties` in Bazel and Buck2). kbf reads them from the `Action`'s platform,
or from the `Command`'s when the `Action` carries none. A property may appear once;
a repeated property is refused with `INVALID_ARGUMENT`.

These properties change how kbf schedules an action today:

| Property | Values | Default | Effect |
|---|---|---|---|
| `kbf-lease` | `action`, `whole_machine` | `action` | The kind of lease the action runs under (see [`kbf-lease`](#kbf-lease)). |
| `gpu` | a whole number | `0` | Whole GPUs the action needs. |
| `kbf-book-cpus` | a whole number, at least 1, plain digits | `1` | Whole cores the lease books. |
| `kbf-book-mem-gib` | a whole number, at least 1, plain digits | `1` | GiB of memory the lease books. |
| `OSFamily` | `linux`; `darwin`, `macos`, `macosx`, `osx` (any case) | any | The operating system of the worker. |
| `ISA`, `Arch` | `x86-64`, `x86_64`, `amd64`; `arm-a64`, `arm64`, `aarch64`; an ISA level such as `x86-64-v3` (any case) | any | The CPU architecture, and level, of the worker. |

Any other value of `kbf-lease`, `gpu`, `kbf-book-cpus` or `kbf-book-mem-gib` is refused
with `INVALID_ARGUMENT`. kbf's own
capability keys (`os`, `arch`, `isa_level`, `cpu.feature`, `label.<k>`, ...; see
[capabilities.md](design/capabilities.md#matching)) are matched too. Other properties
(`container-image`, `Pool`, ...) are accepted and not acted on by the scheduler.

### Property names are read in any case

kbf reads every property name it knows without regard to ASCII case, so a property
meant for kbf is never ignored because of how it is spelled: `osfamily=darwin`,
`OSFAMILY=Darwin` and `OSFamily=darwin` all ask for a Mac. This holds for the REAPI
names (`OSFamily`, `ISA`, `Arch`) and for every kbf key: the capability keys above
(`OS` is `os`, `ISA_Level` is `isa_level`), `gpu` (`GPU=1` books a GPU), and the
reserved `kbf-lease`, `kbf-cpu`, `kbf-mac-admin`, `kbf-book-cpus` and
`kbf-book-mem-gib`. In `label.<k>` only `label.` is
read in any case; the label's own name `<k>` is compared exactly, as its value is.
`arch` spelled exactly so is kbf's own `arch` key (values `x86_64`, `arm64`); in any
other spelling (`Arch`, `ARCH`) it is REAPI's `Arch` and takes the values in the
table. Values are compared as described for each key: `OSFamily`, `ISA` and `Arch`
values in any case, kbf's own keys' values exactly.

One property sent under two spellings of its name (`gpu` and `GPU`, `OSFamily` and
`osfamily`) is refused with `INVALID_ARGUMENT`, as is one requirement named twice
through two names (`OSFamily` and `os`, `ISA` and `arch`). A name kbf does not know,
in any case, is not acted on.

`container-image` is the one exception: it is read by the Linux container driver, not
by the scheduler, and only in exactly that spelling. `Container-Image` is a name kbf
does not know, so the action is scheduled as if it named no image, and the container
driver then refuses it ("the action names no container-image platform property").

## Where an action runs

An action runs only on a worker whose node report satisfies its platform: a Linux
action is never handed to a Mac, nor an arm64 action to an x86-64 machine. Buck2 and
Bazel name the platform with `OSFamily` (and `ISA` where the architecture matters):

```starlark
# Buck2: an execution platform's executor config
remote_execution_properties = {"OSFamily": "linux", "ISA": "x86-64"}
```

```starlark
# Bazel: a platform for actions that must run on the Macs
platform(
    name = "macos_arm64",
    exec_properties = {"OSFamily": "Darwin", "ISA": "arm-a64"},
)
```

An action no kbf daemon can ever run (an `OSFamily` other than Linux or macOS, an `ISA`
of another architecture) is refused at once with `FAILED_PRECONDITION`. An action that
no connected worker satisfies now waits: its operation stays `QUEUED`, and its metadata
carries a `google.rpc.ErrorInfo` (reason `NO_WORKER_CAN_RUN`, domain `kbf`) whose `why`
names the closest worker and what it lacks. The server logs it too. If no live worker
satisfies it for `--unservable-wait-secs` (300 by default; the wait restarts whenever
one does), it fails with `FAILED_PRECONDITION` and that reason. Work larger than every
worker that satisfies its platform (`gpu=2` where nodes have one) is treated the same.

## `gpu`

`gpu=N` places the action only on a worker with `N` free GPUs, and books those GPUs
for the action's lease alone: no other lease is given them until that lease ends
(its result is accepted, or the lease is given up and the action requeued). A worker
with one GPU runs one `gpu=1` action at a time; work that asks for no GPU still
shares the worker's CPUs and memory beside it.

A worker reports how many GPUs it has in the `gpu` entry of its node report; a report
without one has none. The daemon counts them on Linux from the PCI functions in
`/sys/bus/pci/devices`: each NVIDIA or AMD VGA-compatible or 3D controller is one
GPU. A server's management-controller VGA function and integrated graphics do not
count. An Apple silicon Mac has one GPU, for the whole host; the daemon does not
detect macOS nodes yet.

In Bazel:

```starlark
cc_test(
    name = "kernel_test",
    exec_properties = {"gpu": "1"},
    ...
)
```

## `kbf-lease`

An `action` lease (the default) books a share of a worker: one core and 1 GiB, or what
`kbf-book-cpus` and `kbf-book-mem-gib` name, plus its `gpu` GPUs. A `whole_machine`
lease books every core, byte of memory and GPU of its worker, and runs there alone.

A lease goes only to a worker whose daemon runs a driver for its kind: `container`,
`native` or `fake` (or the test-only `local`) for `action`, `native-whole-machine` for
`whole_machine` (see
[capabilities.md](design/capabilities.md#driver-entries)). No daemon runs
`native-whole-machine` yet, so today a `whole_machine` action waits with the reason
and then fails, as described under [Where an action runs](#where-an-action-runs).

A `whole_machine` lease waits for a worker that holds no lease. While it waits it holds
one worker that could run it, and work queued after it at the same or a lower QoS is
not placed there, so that worker empties as its leases end. More urgent work is still
placed there. Running leases are never stopped for it.

## `kbf-book-cpus` and `kbf-book-mem-gib`

Every action books one core and 1 GiB of memory on the worker it runs on, unless it
names a size: `kbf-book-cpus=N` books `N` whole cores, and `kbf-book-mem-gib=N` books
`N` GiB. `N` is written in plain digits with no leading zero: `+4` and `04` are
refused, since each would book the same as `4` under a different action digest, and so
miss the cache entries `4` made. Either may be given alone; the other stays at its
default. The scheduler places the action only where that much is free, and holds it
for the lease until the lease ends, as it does for the default booking.

The booking also sets the memory the action may use. The native driver (Macs) kills an
action whose processes together hold more than 150% of its booked memory plus 512 MiB:
2 GiB for the default booking, 12.5 GiB for `kbf-book-mem-gib=8`. A large `swiftc` or
`ld` step that dies at 2 GiB needs a larger booking. The container driver sets the
lease's soft memory limit (`memory.high`) and CPU weight from the booking.

```starlark
# Bazel: a link step that needs 4 cores and 12 GiB
cc_binary(
    name = "server",
    exec_properties = {"kbf-book-cpus": "4", "kbf-book-mem-gib": "12"},
    ...
)
```

A value that is not a whole number of at least 1 is refused with `INVALID_ARGUMENT`,
as is either key on a `kbf-lease=whole_machine` lease, which books the whole
worker. A booking larger than any worker that satisfies the platform waits and
then fails, as described under [Where an action runs](#where-an-action-runs).

Like every platform property, the two keys are part of the action digest: the same
action with another booking is a different action cache entry, so changing a booking
reruns the action once. They are not the capability keys `cpus` and `mem_gib`, which
ask for a worker whose whole machine has at least that much and book nothing.

## `xcode`

On Macs, `xcode=<build>` places the action on a worker that has that Xcode build
installed and runs it with that Xcode selected: `DEVELOPER_DIR` is set to the Xcode's
`Contents/Developer`, so `xcrun`, `cc`, `swiftc` and `xcodebuild` are that Xcode's. The
build is what `xcodebuild -version` prints after `Build version` (`16C5032a`), not the
marketing version (`16.2`), so two Xcodes that share a version but differ in build
never share a cache entry.

A Mac may have several Xcodes installed; it serves an action that names any of them
that is **ready**. The daemon asks each `Xcode*.app` in `/Applications` (the
`--xcode-apps` flag), under its own `DEVELOPER_DIR` and with its own `xcodebuild`
(the one inside the app), these questions, all at once, and reads the answers in
this order: `xcodebuild -version` (which must print a build),
`xcodebuild -license check`, `xcodebuild -checkFirstLaunchStatus`, `xcrun --find
clang`, and, on a node started with `--require-metal-toolchain` (one meant for GPU
work), whether `xcodebuild -showComponent MetalToolchain` says `Status: installed` (an
Xcode before 26, which has no `-showComponent` and bundles Metal, passes when `xcrun
--find metal` does). Every question runs as an action does: under the actions'
sandbox with the network off, in a lease directory of the daemon's own
(`lease-survey` under `--scratch`, removed when the survey ends), with the same
writes allowed in the user folders ([below](#what-an-action-on-a-mac-may-write)). One
for which every question exits 0, each within a minute, is ready and is reported as an
`xcode` entry of its node report. An Xcode whose licence is not accepted still answers
`-version` with exit 0, but `-license check` and every tool it runs (`xcrun`, `cc`,
`swiftc`) exit 69, so it would fail every action placed on it.

The daemon does not wait for its first survey before it says `Hello` (the survey
can take seconds: each `xcrun` lookup the cache does not hold does, and after a reboot
it holds none). Until that survey ends, the node's status lists each `Xcode*.app` it
found as `not_surveyed`, and its node report advertises no Xcode, so nothing that
names one is placed on it; when the survey ends the daemon sends its `Hello` and
status again, with the Xcodes' states, without a restart. An Xcode not surveyed yet
keeps whatever item its node's previous status gave it under `needs_attention` (a
restarted daemon's broken Xcode stays listed, and is neither logged as resolved nor
raised again by the survey that finds it unchanged); one with no previous item has
none.

An Xcode that is not ready is **not hidden**: the node's status lists every installed
Xcode with its state (`license_not_accepted`, `first_launch_not_run`,
`metal_toolchain_missing`, `failed`), the question it failed with its answer, and the
command that fixes it (for example `sudo
/Applications/Xcode_16.2.app/Contents/Developer/usr/bin/xcodebuild -license accept`),
and `GET /v1/nodes` lists it under the node's `needs_attention`
([api.md](api.md#get-v1nodes)). The daemon (once per start) and the server each log
it at `WARN` once when it appears, and again only when its build, state or fix
changes: a reason that
changes alone (`xcodebuild` starts its NSLog lines with the time and its pid) is not
logged again, and is not by itself a change the daemon sends.
The daemon asks again every three minutes (`--xcode-recheck-secs`), so an Xcode fixed
while the daemon runs is advertised within minutes, without a restart, and one that
stops being ready (an update whose new licence is not accepted) stops being advertised.

A hung question is killed, so it cannot keep an Xcode not surveyed for long. An answer counts
only once the program has exited and closed its output: one that exits but leaves a
child holding its output open is not ready when the minute is up. Each Xcode is asked
once (an app that links to another, such as `Xcode.app`, is listed with that one's
answers), on a thread of its own, and its questions at once, so a hung Xcode delays
the end of the survey (and the other Xcodes' readiness) by
up to a minute (two with `--require-metal-toolchain`, whose `xcrun --find metal` is
asked only after `-showComponent`), and the other Xcodes add nothing to that but
for their `xcrun` lookups, which run one at a time (each rewrites `xcrun`'s whole
cache, so two at once lose each other's entries and the next lookups take seconds;
one that hangs delays the others').
An action that names no
`xcode` runs with the Mac's default Xcode (`xcode-select`), or with the
`DEVELOPER_DIR` its own environment sets; one that names an `xcode` gets that Xcode
whatever its environment says.

```starlark
# Bazel: a platform for actions built with one Xcode
platform(
    name = "macos_arm64_xcode_16_2",
    exec_properties = {"OSFamily": "Darwin", "ISA": "arm-a64", "xcode": "16C5032a"},
)
```

## `ios.device` (planned)

**Planned, not on `main`:** today `ios.device` is a name kbf does not know, so it is not
acted on and an action that sends it may run on any worker, Linux included. The design
is in [ios-devices.md](design/ios-devices.md):

| Property | Values | Effect (planned) |
|---|---|---|
| `ios.device` | `1` | Books one USB-attached iPhone or iPad on a Mac for the lease alone; the action finds its UDID in `KBF_IOS_DEVICE_ID`. |
| `ios.device.class` | `iPhone`, `iPad` | Exact. |
| `ios.device.product_type` | a model identifier (`iPhone17,3`) | Exact. |
| `ios.device.os_version` | an iOS version | Exact. |
| `ios.device.os_build` | an iOS build | Exact. |

One device must satisfy every `ios.device.*` key. An attribute key without
`ios.device=1` will be refused, as will `ios.device` with `kbf-lease=vm` or
`whole_machine`.

## What an action on a Mac may write

The native driver runs every action on a Mac under `sandbox-exec` with a profile that
denies every file write outside the action's lease directory and `/dev`. The lease
directory holds the input root (the working directory and outputs), and the action's
own home, temporary and cache directories, named by `HOME`, `TMPDIR`,
`XDG_CACHE_HOME` and `CLANG_MODULE_CACHE_PATH`; all of it is removed when the lease
ends. Two kinds of write in the daemon user's temporary folder
(`getconf DARWIN_USER_TEMP_DIR`, under `/var/folders`) are allowed too, because macOS
tools make them there whatever `TMPDIR` says: Foundation's atomic saves (inside
`TemporaryItems`, which `swift build` and `xcodebuild` need; the daemon makes that
folder, and an action cannot remove, rename or replace it) and `xcrun`'s cache
(`xcrun_db`, behind `cc`, `clang` and `swiftc`). The daemon sweeps what leases leave
there once its last change is an hour from now, before or after, never through a
symlink. Until each lease runs as its own user, leases on one Mac share those names:
one lease can see another's temporary files there and rewrite the `xcrun` cache the
next lease reads. The daemon runs no developer tool outside the sandbox while it
serves: it asks the Xcodes ([above](#xcode)) under the actions' sandbox, where
`xcrun` reads and fills that cache as an action's would, and runs each Xcode's own
`xcodebuild` rather than the `/usr/bin` one. It leaves the cache in place at start:
without it every lookup of the first survey takes seconds, and no Xcode is ready
until that survey ends. It warms the cache under the actions' sandbox too, once its first survey ends for
the node's own Xcode and the ready ones, and later for each Xcode that becomes ready.
A tool that writes anywhere else (`/tmp`, a path
under the daemon user's real home, the rest of `/var/folders`) fails, so a build rule
points such a tool into the lease: `swiftc -module-cache-path`,
`xcodebuild -derivedDataPath`. `xcodebuild` and SwiftPM find `~` through the user
database, not `HOME`: `swift build` only warns and goes on without its user-level
caches, but `xcodebuild` resolving a Swift package fails, because it must write
`~/Library/Caches/org.swift.swiftpm` (issue #179); `xcodebuild` on a project without
packages works. Setting `CFFIXED_USER_HOME` to the lease's `HOME` is no way round it:
`xcodebuild` then has a system service mount its Metal toolchain under that home,
and the mount, owned by root, keeps the lease directory from being removed, which
fails the lease (issue #178). Apple's
tools that nest a sandbox of their own need it turned off, since macOS refuses a
sandbox inside a sandbox (`swiftc -disable-sandbox`, `swift build --disable-sandbox`);
the outer profile still keeps their writes inside the lease.
