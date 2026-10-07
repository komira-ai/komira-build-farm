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
| `OSFamily` | `linux`; `darwin`, `macos`, `macosx`, `osx` (any case) | any | The operating system of the worker. |
| `ISA`, `Arch` | `x86-64`, `x86_64`, `amd64`; `arm-a64`, `arm64`, `aarch64`; an ISA level such as `x86-64-v3` (any case) | any | The CPU architecture, and level, of the worker. |

Any other value of `kbf-lease` or `gpu` is refused with `INVALID_ARGUMENT`. kbf's own
capability keys (`os`, `arch`, `isa_level`, `cpu.feature`, `label.<k>`, ...; see
[capabilities.md](design/capabilities.md#matching)) are matched too. Other properties
are accepted and not acted on by the scheduler.

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
