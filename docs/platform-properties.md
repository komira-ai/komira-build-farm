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
count. An Apple silicon Mac has one GPU, for the whole host, and reports none: on
macOS (`OSFamily=darwin`, or any spelling that names macOS) a GPU comes only with the
whole Mac, so `gpu` of 1 or more is refused unless the action also asks for
`kbf-lease=whole_machine`, which then books no GPU on top: the lease has the whole Mac.

In Bazel:

```starlark
cc_test(
    name = "kernel_test",
    exec_properties = {"gpu": "1"},
    ...
)
```

## `kbf-lease`

An `action` lease (the default) books a share of a worker: one core and 4 GiB (the first rung of
the memory ladder), or what
`kbf-book-cpus` and `kbf-book-mem-gib` name, plus its `gpu` GPUs. A `whole_machine`
lease books every core, byte of memory and GPU of its worker, and runs there alone.

A lease goes only to a worker whose daemon runs a driver for its kind: `container`,
`native` or `fake` (or the test-only `local`) for `action`, `native-whole-machine` for
`whole_machine` (see
[capabilities.md](design/capabilities.md#driver-entries)). Only a Mac whose daemon runs
`kbf-mac-session` runs `native-whole-machine`: there each lease runs as a throwaway
user in that user's GUI session, and the Mac is cleaned before the lease reports
([fleet-updates.md](design/fleet-updates.md#101-no-gpu-in-vms)). On a farm without such
a Mac a `whole_machine` action waits with the reason and then fails, as described under
[Where an action runs](#where-an-action-runs). A GPU test on a Mac is a
`whole_machine` lease:

```starlark
py_test(
    name = "app_with_local_model_test",
    exec_properties = {"OSFamily": "darwin", "kbf-lease": "whole_machine", "gpu": "1"},
    ...
)
```

A `whole_machine` lease waits for a worker that holds no lease. While it waits it holds
one worker that could run it, and work queued after it at the same or a lower QoS is
not placed there, so that worker empties as its leases end. More urgent work is still
placed there. Running leases are never stopped for it.

## `kbf-book-cpus` and `kbf-book-mem-gib`

Every action books one core and 4 GiB of memory on the worker it runs on (the first
rung of the server's memory ladder, `--memory-ladder-gib`), unless it
names a size: `kbf-book-cpus=N` books `N` whole cores, and `kbf-book-mem-gib=N` books
`N` GiB. `N` is written in plain digits with no leading zero: `+4` and `04` are
refused, since each would book the same as `4` under a different action digest, and so
miss the cache entries `4` made. Either may be given alone; the other stays at its
default. The scheduler places the action only where that much is free, and holds it
for the lease until the lease ends, as it does for the default booking.

The booking also sets the memory the action may use. The native driver (Macs) kills an
action whose processes together hold more than 150% of its booked memory plus 512 MiB:
6.5 GiB for the default booking, 12.5 GiB for `kbf-book-mem-gib=8`. A large `swiftc` or
`ld` step that dies at 2 GiB needs a larger booking. The container driver gives the
lease the same limit as a hard cap on its RAM (`memory.max`, swap allowed), and its
CPU weight, from the booking; a lease that keeps pressing that cap while its swap
keeps rising, and that holds more than 512 MiB or 25% of its booking in swap,
whichever is more (the node's defaults), is killed as out of memory.

The booking is where an action starts, not always where it runs. When a daemon reports
that the action passed its own memory limit, the server runs it again one rung up a
fixed ladder, 4, then 16, then 32 GiB, then a whole host (the largest node that could
run it, so that no other lease shares it; the rungs are the server's
`--memory-ladder-gib`). A size named here climbs from where it is: a killed 6 GiB ask
runs again with 16 GiB. The server remembers the raised booking for the action (its instance name and action digest), so
the next run of the same action starts there; only a kill at the largest node reaches
the client, as `FAILED_PRECONDITION` saying the action needs more memory than any node
offers. A node that kills the action under its own memory pressure reruns it with the
same booking. See [scheduler.md](design/scheduler.md#memory-kills). Both drivers say
which memory ran out: the native driver's memory watch and the container driver's
lease cap are the action's own limit.

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
sandbox with the network off (as `network=off` runs one), in a lease directory of the daemon's own
(`lease-survey` under `--data-dir`, removed when the survey ends), with the same
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

## `simulator`

On Macs, `simulator=<device type>,<runtime>` gives the action a simulator of its own:
the native driver makes a new device for the lease before the action runs and deletes
it when the lease ends. Without it, every simulator an action uses is one the daemon
user's leases share, so two leases' UI tests can use the same device at once and one
can leave it dirty for the next. The name is read in any case; the value is exact.

| Part | Values |
|---|---|
| `<device type>` | a `simctl` device type, by identifier (`com.apple.CoreSimulator.SimDeviceType.iPhone-16`) or by name (`iPhone 16`) |
| `<runtime>` | a `simctl` runtime identifier (`com.apple.CoreSimulator.SimRuntime.iOS-18-4`) |

Both are required, separated by one comma; any other value is refused with
`INVALID_ARGUMENT`. The value is passed to `xcrun simctl create` unchanged, so what
`xcrun simctl list devicetypes` and `xcrun simctl list runtimes` print are the values
that work.

- **At start**, after the inputs are written, the driver runs `xcrun simctl create
  kbf-lease-<term>-<seq> <device type> <runtime>`, under the actions' sandbox with the
  network off and with the Xcode the action's `xcode` property names. The device is
  in the daemon user's default device set (`~/Library/Developer/CoreSimulator/Devices`):
  `xcodebuild -destination` finds only the devices of that set, not one made in a set
  elsewhere with `simctl --set` (the native-macos job checks both). If the command
  fails (a runtime the Mac does not have, a device type the runtime does not
  support) or does not answer within two minutes, the lease fails with `simctl`'s
  reason and the action does not run.
- **The action** finds the device's UDID in `KBF_SIMULATOR_UDID` (it wins over the
  Command's environment). The driver boots the device and waits until Xcode lists it
  before the action runs (Xcode's list of devices can change with a boot, and a
  device that is not listed makes `xcodebuild test -destination` fail after 30 s), so
  the action finds it booted: `bootstatus -b` returns at once, and `simctl boot` of a
  booted device fails. A device that does not boot or is not listed in time fails the
  lease with the reason, and the action does not run:

  ```sh
  xcrun simctl bootstatus "$KBF_SIMULATOR_UDID" -b
  xcodebuild test -project App.xcodeproj -scheme App -destination "id=$KBF_SIMULATOR_UDID"
  ```

- **At the end**, whatever the outcome (exit, failure, timeout, kill, a run the daemon
  drops), before the lease directory is removed: the device the create printed, and
  any other device named for the lease (one a create cut short made late), is shut
  down and deleted, each by its UDID, with the Mac's default Xcode. The daemon keeps
  the UDID itself, so nothing the action writes in its lease directory changes what is
  deleted. A delete that fails fails the lease, as a lease directory that cannot be
  removed does. A daemon that was killed during such a lease deletes, at its next
  start, every device whose name starts `kbf-lease-`: give no device of your own such
  a name on a farm Mac.

**Not isolated.** A booted simulator's processes run under `CoreSimulatorService`,
outside the actions' sandbox and its `network=off` rule, and the action can reach every
device of the default set with `simctl`, not only its own. The property gives a lease
a clean device of its own; it does not confine what the action does with simulators.

The scheduler does not read `simulator`: name `OSFamily=darwin` too, or the action
may be placed on a Linux worker, whose driver does not read it either. A Mac that does
not have the runtime fails the lease. Hosts set up by `kbf-mac-provision` have no
simulator runtimes ([macos-vms.md](design/macos-vms.md)), so there the property works
only once runtimes are installed.

```starlark
# Bazel: a test that drives an iOS app on its own simulator
sh_test(
    name = "app_ui_test",
    srcs = ["app_ui_test.sh"],
    exec_properties = {
        "OSFamily": "Darwin",
        "simulator": "com.apple.CoreSimulator.SimDeviceType.iPhone-16,com.apple.CoreSimulator.SimRuntime.iOS-18-4",
    },
    ...
)
```

## `vm.image` and the VM report keys

macOS VM leases (`kbf-lease=vm`, [macos-vms.md](design/macos-vms.md)) are **planned**:
no driver runs a VM and no daemon reports a VM entry. What the server already reads:

| Property | Values | Effect |
|---|---|---|
| `vm.image` | `<name>@sha256:<64 lowercase hex digits>` | Matched by membership on the digest: the action runs only on a worker whose report lists an image with that digest, under any name. A value without a digest is refused with `INVALID_ARGUMENT`. |
| `vm.slots`, `vm.max_cpus`, `vm.max_mem_gib` | none | Report-only: an action that names one, in any case, is refused with `INVALID_ARGUMENT`. |

The digest is the image's recipe digest, which names the image's pinned inputs, not the
bytes of one node's build ([macos-vm-guests.md](design/macos-vm-guests.md#5-image-identity)).
Because no daemon reports `vm.image` yet, an action that names one matches no worker:
it waits and then fails as described under [Where an action runs](#where-an-action-runs).
`kbf-lease=vm` is still refused as an unknown lease kind.

## `ios.device`

The server matches and books devices; **no daemon reports a device yet**, so an action
that names `ios.device=1` waits and then fails as described under
[Where an action runs](#where-an-action-runs). The design is in
[ios-devices.md](design/ios-devices.md):

| Property | Values | Effect |
|---|---|---|
| `ios.device` | `1` | Books one USB-attached iPhone or iPad on a Mac for the lease alone, named in the lease's `Start`. **Planned:** the action finds its UDID in `KBF_IOS_DEVICE_ID`. |
| `ios.device.class` | `iPhone`, `iPad` | Exact. |
| `ios.device.product_type` | a model identifier (`iPhone17,3`) | Exact. |
| `ios.device.os_version` | an iOS version | Exact. |
| `ios.device.os_build` | an iOS build | Exact. |

One device must satisfy every `ios.device.*` key: an iPhone on iOS 18.6 and an iPad on
iOS 26.0 do not together serve `ios.device.class=iPhone` with
`ios.device.os_version=26.0`. Refused with `INVALID_ARGUMENT`: a value other than `1`,
an attribute key without `ios.device=1`, an `ios.device.<attribute>` not in the table,
`ios.device` with an OS other than macOS, and `ios.device` with `kbf-lease=vm` or
`whole_machine`. Names are read in any case.

## `network`

On a Mac (the native driver), an action has the network, the internet and the LAN,
unless it turns it off:

| `network` | The action gets |
|---|---|
| absent, `on`, `standard` | the network (the default): the sandbox's only network rule denies the `usbmuxd` socket, which reaches an iOS device |
| `off`, `none` | loopback, Unix sockets and the node's own non-loopback address: every other connect is refused with `EPERM` by the sandbox (`crates/kbf-driver-native/src/network.rs`) |

Any other value is refused with `INVALID_ARGUMENT`. Unix sockets stay open with
`network=off`, so DNS lookups still go out through the system's resolver; a connect to
the node's own LAN address is allowed too, so a service listening on it is reachable. A node
without `sandbox-exec` cannot turn the network off and says so in its
`network_isolation` capability (`none`). Linux container actions have no network
([ARCHITECTURE.md](../ARCHITECTURE.md)), whatever the property says.

## What an action on a Mac may write

The native driver runs every action on a Mac under `sandbox-exec` with a profile that
denies every file write outside the action's lease directory and `/dev`. The lease
directory holds the input root (the working directory and outputs), and the action's
own home, temporary and cache directories, named by `HOME`, `TMPDIR`,
`XDG_CACHE_HOME` and `CLANG_MODULE_CACHE_PATH`; all of it is removed when the lease
ends. Three kinds of write in the daemon user's temporary folder
(`getconf DARWIN_USER_TEMP_DIR`, under `/var/folders`) are allowed too, because macOS
tools make them there whatever `TMPDIR` says: Foundation's atomic saves (inside
`TemporaryItems`, which `swift build` and `xcodebuild` need; the daemon makes that
folder, and an action cannot remove, rename or replace it), `xcrun`'s cache
(`xcrun_db`, behind `cc`, `clang` and `swiftc`), and Swift Build's scratch directory
(`swbuild.tmp.<random>` and what is in it, which `xcodebuild` needs to build an app).
The daemon sweeps the first two's leftovers once their last change is an hour from now,
before or after, never through a symlink; it leaves `swbuild.tmp.*` alone, since a
long build's can be older than that. Until each lease runs as its own user, leases on one Mac share those names:
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

The same profile keeps every action away from the services that reach an iOS device
plugged into the Mac: it refuses the lookup of any CoreDevice or `remoted` mach
service and a connect to `usbmuxd`'s socket, so `xcrun devicectl` fails (its
connection to `CoreDeviceService` is invalidated) and no tool talks to a device
through `usbmuxd`. No lease books a device yet
([design/ios-devices.md](design/ios-devices.md#61-what-the-sandbox-must-allow)).

It also refuses opening an application through Launch Services (`open`, the sandbox's
`lsopen`): Launch Services starts the application outside the sandbox and outside the
lease's process tree, so it could write anywhere the daemon's user can, use the
network and outlive the lease, and an action can open an application it linked in its
own lease. `xcodebuild` starts a macOS UI-test runner that way, so macOS UI tests fail
under the native driver (error -54, "The sandbox profile of this process is missing
"(allow lsopen)""). iOS UI tests on a simulator do not need it
([spikes/macos-p0.md](spikes/macos-p0.md#xcuitest-under-the-native-drivers-sandbox)).
