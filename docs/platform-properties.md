# Platform properties

A build tool describes where an action may run with REAPI platform properties
(`exec_properties` in Bazel and Buck2). kbf reads them from the `Action`'s platform,
or from the `Command`'s when the `Action` carries none. A property may appear once;
a repeated property is refused with `INVALID_ARGUMENT`.

These properties change how kbf schedules an action today:

| Property | Values | Default | Effect |
|---|---|---|---|
| `kbf-lease` | `action`, `whole_machine` | `action` | The kind of lease the action runs under. |
| `gpu` | a whole number | `0` | Whole GPUs the action needs. |
| `kbf-book-cpus` | a whole number, at least 1 | `1` | Whole cores the lease books. |
| `kbf-book-mem-gib` | a whole number, at least 1 | `1` | GiB of memory the lease books. |
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

## `kbf-book-cpus` and `kbf-book-mem-gib`

Every action books one core and 1 GiB of memory on the worker it runs on, unless it
names a size: `kbf-book-cpus=N` books `N` whole cores, and `kbf-book-mem-gib=N` books
`N` GiB. Either may be given alone; the other stays at its default. The scheduler places
the action only where that much is free, and holds it for the lease until the lease
ends, as it does for the default booking.

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
as is either key on a `kbf-lease=whole_machine` lease, which is planned to book the
whole worker. A booking larger than any worker that satisfies the platform waits and
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

A Mac may have several Xcodes installed; it serves an action that names any of them.
The daemon finds them at start: each `Xcode*.app` in `/Applications` (the
`--xcode-apps` flag) that answers `xcodebuild -version` is reported as an `xcode`
entry of its node report. An Xcode that does not answer (its licence not accepted, its
first launch not run) is left out and logged. An action that names no `xcode` runs
with the Mac's default Xcode (`xcode-select`), or with the `DEVELOPER_DIR` its own
environment sets; one that names an `xcode` gets that Xcode whatever its environment
says.

```starlark
# Bazel: a platform for actions built with one Xcode
platform(
    name = "macos_arm64_xcode_16_2",
    exec_properties = {"OSFamily": "Darwin", "ISA": "arm-a64", "xcode": "16C5032a"},
)
```

## What an action on a Mac may write

The native driver runs every action on a Mac under `sandbox-exec` with a profile that
denies every file write outside the action's lease directory and `/dev`. The lease
directory holds the input root (the working directory and outputs), and the action's
own home, temporary and cache directories, named by `HOME`, `TMPDIR`,
`XDG_CACHE_HOME` and `CLANG_MODULE_CACHE_PATH`; all of it is removed when the lease
ends. A tool that writes elsewhere (`/tmp`, a path under the daemon user's real home,
the per-user folders under `/var/folders`) fails, so a build rule points such a tool
into the lease: `swiftc -module-cache-path`, `xcodebuild -derivedDataPath`. Apple's
tools that nest a sandbox of their own need it turned off, since macOS refuses a
sandbox inside a sandbox (`swiftc -disable-sandbox`, `swift build --disable-sandbox`);
the outer profile still keeps their writes inside the lease.
