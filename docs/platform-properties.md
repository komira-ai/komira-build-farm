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

Any other value of these two is refused with `INVALID_ARGUMENT`. Other properties
are accepted and not yet acted on.

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
