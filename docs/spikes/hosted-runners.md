# Spike: what GitHub-hosted runners can carry

kbf's infrastructure tests need rootless Podman, cgroup v2 limits and delegation,
I/O control, a network-less container, MinIO and several servers on one machine.
This spike measured which of those a GitHub-hosted runner provides, on x86 and arm.
It is a measurement, not a decision. Each farm test that needs real infrastructure
gets a verdict below:

- **fits**: runs as a hosted CI job;
- **fits with limits**: runs hosted, under the stated limits;
- **farm probe**: needs a machine we control.

The probes live in `tools/spike/`. The workflow is `.github/workflows/spike-hosted.yml`.
It runs by hand (`workflow_dispatch`) and on pull requests that change the probes.
Each probe prints `SPIKE <arch>.<key>=<value>` lines. To re-measure after an image
change:

```sh
gh workflow run spike-hosted.yml --ref main
gh run view <run id> --log | grep ' SPIKE '
```

## Where the numbers come from

All numbers below come from run `37585958715` (commit `7f8a6f4`) unless a row says
otherwise. Earlier runs `37583366334` (`a7a58ae`), `37584012663` (`01aaadb`) and
`37584743766` (`429a7b4`) ran the I/O probe on earlier versions of the scripts; the
"over four runs" bounds below take all four runs. Their p99 ranges, where they differ
from the measurement run: x86 baseline 0.85 to 1.98 ms and `io.max` set 3.02 to
3.4 ms; arm `io.max` set 17.7 to 18.3 ms in `37583366334` and `37584743766`, and 23.2
to 23.8 ms in `37584012663` (before the CPU and I/O halves of the hog were split into
separate arms in `429a7b4`); arm `CPUWeight=1000` arm 18.5 to 19.1 ms in
`37584743766`. None of them changes a verdict.

| | x86 job | arm job |
|---|---|---|
| Runner label | `ubuntu-24.04` | `ubuntu-24.04-arm` |
| Image | `ubuntu24 20260927.320.1` | `ubuntu24-arm64 20261004.142.1` |
| Kernel | `6.17.0-1022-azure` | `6.17.0-1022-azure` |
| CPU | 4 vCPU, AMD EPYC 9V74 (EPYC 7763 on earlier runs) | 4 vCPU, Neoverse-N2 |
| Memory, swap | 15.9 GiB, 3 GiB | 15.9 GiB, 3 GiB |
| Workspace disk | 145 GB ext4, 87 GB free, direct writes 434 MB/s | 145 GB ext4, 108 GB free, 328 MB/s |
| systemd, Podman | 255, 4.9.3 (crun 1.14.1, conmon 2.1.10, netavark) | same |

## Verdicts

| Test | Verdict | Measured (x86 / arm) |
|---|---|---|
| **Converged slices.** A CPU and disk hog runs in the actions slice. A Raft-style fsync probe in the server slice must keep p99 under 10 ms. The mutant drops the `io.max` and `io.latency` lines. | **x86: fits with limits.** It covers the `io.max` half only. **`io.latency` half: farm probe.** **arm: farm probe.** | x86, `io.max` only: p99 3.0 to 3.2 ms; `io.max` and `io.latency` lines: 2.8 to 3.0 ms (at most 3.4 ms over four runs). Lines dropped: 686 to 1331 ms (at least 59.9 ms over four runs). The kernel has no `io.latency` (see Limits). arm, `io.max` set: p99 16.8 to 18.9 ms at either cap, even with the probe on its own filesystem (up to 23.8 ms in an earlier run). Lines dropped: 107 to 1040 ms (at least 35.8 ms over four runs). |
| **Memory pressure.** A hog under `memory.high` beside small actions. The neighbours finish and the hog lives. | **fits** | The hog asked for 512 MiB under a 256 MiB high mark with no swap. It stayed at 271 MiB, alive, with 274 / 864 `high` events and 0 `oom_kill`. Neighbours took 203 to 208 ms with the hog, against 203 to 306 ms alone (arm: 131 to 137 against 131 to 201). Control: the same hog under `memory.max` was OOM-killed on both. |
| **No network for an action.** DNS and outbound connect fail, and the store's port is unreachable. The mutant runs the action on the host network. | **fits** | `--network=none`: DNS fails and the connect to MinIO fails, on both. The host-network mutant reaches MinIO. See "Networked actions" below for the default network. |
| **Node readiness check, PSI off.** | **fits with limits** | PSI cannot be switched off: it is a boot-time kernel setting, and `/proc/pressure/{cpu,memory,io}` and the per-cgroup `*.pressure` files exist on both. A test feeds the check a planted pressure source or a planted cgroupfs kubelet config. A real PSI-off kernel needs a farm probe. |
| **Integration cell.** Three servers, MinIO and RustFS, two daemons, real buck2 and Bazel on one runner. Also the upgrade and rollback test on a three-node cell, and the token and certificate tests. | **fits** (budget). Behaviour is measured once the server and daemon exist. | Seven endpoints all answered within 0.45 s / 0.34 s of starting, after a one-time pull of 0.6 to 4.1 s per image. Memory available fell by 96 / 71 MiB. Container memory: MinIO 89 / 56 MB, RustFS 72 / 65 MB, the daemon stand-ins under 1 MB each. The three stand-ins are Python HTTP servers; the `kbf-server` binary only prints its version today. |
| **Store conformance against real stores.** | **fits** for MinIO and RustFS. **Garage and a degraded store: not measured yet.** | The `objstore-s3` job already runs both stores, pinned by digest, as service containers on x86. Here the same digests pulled and ran under rootless Podman on both arches. MinIO answered its health check 920 / 698 ms after `podman run`. Garage needs a pinned image first. |
| **Build-tool conformance.** buck2 remote-test results and per-target no-cache marking. | **fits** | buck2 release `2026-10-01`, fetched and checked against its sha256: 388 ms / 1089 ms. `bazel` (bazelisk) is preinstalled on both images. |
| **Daemon cgroup delegation.** A prerequisite for the slice, memory and lease tests. | **fits** | See "Delegation" below. |

Pure tests need no infrastructure and fit as ordinary `cargo test` jobs: the
deterministic simulation, parsers and matching, the workflow lint, the catalog lint,
snapshot coverage, and the Podman driver behind a fake runtime.

## Probes and their controls

Each probe has an arm that must fail, or must succeed, for the result to mean
anything. The step fails if a control comes out wrong.

- **I/O** (`io_slices.sh`, `fsync_probe.py`, `io_hog.sh`). The probe appends 4 KiB
  records with `fdatasync` every 2 ms for 15 s. The hog runs one `sha256sum` per core,
  two direct-I/O `dd` writers and one buffered `dd` that syncs. There are nine arms,
  interleaved over three rounds:
  - baseline;
  - open (the mutant);
  - CPU half of the hog only;
  - `io.max` only;
  - `io.latency` only;
  - both;
  - both with `CPUWeight=1000` and a cap at 1/8 of the disk rate;
  - open and both with the probe on its own ext4 loop filesystem.

  The `io.max` cap is a quarter of the disk's direct-write rate as measured at the
  start of the step: 108 / 82 MB/s. The probe reads the cap back from cgroupfs and
  measures the hog's write rate (104 / 76 MB/s) from `io.stat`.
- **Memory** (`memory_high.sh`, `mem_hog.py`). Runs baseline neighbours, the hog under
  `MemoryHigh` with `MemorySwapMax=0`, then the control: the same hog under `MemoryMax`
  must end `Result=oom-kill`.
- **Rootless Podman** (`podman_rootless.sh`). `--memory`, `--cpus` and `--pids-limit`
  are read back from the container's own cgroup, under both cgroup managers. The control
  is a 200 MiB buffer under `--memory 64m`, which must be killed (exit 137 and one
  memory-cgroup OOM in the kernel log), while 16 MiB under the same limit succeeds.
- **No network** (`no_network.sh`). Runs the same busybox checks under `none`,
  slirp4netns and pasta (each named explicitly), podman's default with no `--network`
  flag (the helper it starts is read from the process table), and `host`. The step
  fails if the `none` arm reaches DNS or MinIO, and if the host-network mutant cannot
  reach MinIO, because then the check could never go red.
- **Delegation** (`delegation.sh`, `delegated_inner.sh`). Records each controller
  write and the container's cgroup as seen from the host.
- **Footprint** (`several_servers.sh`). The step fails unless all seven endpoints
  answer.
- **Build budget** (`build_budget.sh`). The buck2 download must match its pinned
  sha256.

The controls were seen failing during the spike:
- the OOM control failed in run `37583366334`, when it trusted Podman's `OOMKilled`
  flag (see Limits);
- the no-network control failed in the same run, because MinIO had not started.

## Limits

- **No `io.latency` on the hosted kernel.** The kernel is built with
  `CONFIG_BLK_CGROUP_IOLATENCY=n`. `CONFIG_BLK_CGROUP_IOCOST` and
  `CONFIG_BLK_CGROUP_IOPRIO` are `y`, and `io.cost.qos` exists at the root. systemd
  accepts `IODeviceLatencyTargetSec=` without complaint and writes nothing, so a slice
  test must read `io.latency` back and fail if it is missing. Otherwise its
  `io.latency` half passes without testing anything. The `io.latency`-only arm
  protected nothing: x86 p99 551 to 1304 ms, arm 148 to 183 ms.
- **The arm disk misses the 10 ms target under any write load we tried.** The CPU half
  of the hog alone costs about 3 ms of p99 on both arches. With `io.max` the x86 probe
  holds 3 ms. The arm probe sat at 16.8 to 18.9 ms in the measurement run (17.7 to 19.1 ms and
  23.2 to 23.8 ms in earlier runs), and these did not bring it under 10 ms:
  - a 41 MB/s cap instead of 82 MB/s;
  - `CPUWeight=1000` on the server slice;
  - a separate filesystem for the probe (18.2 to 18.9 ms).

  So the cause is the device, not a shared journal. On x86 the separate filesystem
  gives 5.2 to 6.0 ms. The mutant still separates on arm (dropped lines: at least
  35.8 ms over four runs), but at the 10 ms target both arms are red.
- **Slice names: a dash means nesting.** systemd places `a-b.slice` inside `a.slice`.
  `kbf-daemon.slice` therefore lives at `kbf.slice/kbf-daemon.slice`. A
  `kbf-server-guarded.slice` would sit inside `kbf-server.slice`, where `io.latency`
  (which throttles siblings) has no sibling to act on. The spike's first run fell into
  this.
- **cgroup v2 and delegation.** The hierarchy is `cgroup2fs`. Root controllers are
  `cpuset cpu io memory hugetlb pids rdma misc dmem`, with `cpuset cpu io memory pids`
  enabled for children. The runner user's systemd manager (`user@<uid>.service`) gets
  only `cpu memory pids`: **no `io`**. A rootless daemon that must set `io.max`
  therefore needs a system unit with `Delegate=yes`.
- **Delegation works.** A system unit with `Delegate=yes`, running as the unprivileged
  runner user, receives `cpuset cpu io memory pids` and owns its cgroup. It moves itself
  into a leaf, because a cgroup that hands controllers to children may hold no process.
  It then enables `cpu io memory pids` for an `actions` subtree and writes `io.max`
  there. Rootless Podman with `--cgroup-manager=cgroupfs --cgroup-parent=<unit>/actions`
  puts the container at `actions/libpod-<id>` with `memory.max`, `cpu.max` and
  `pids.max` applied. The parent must be computed before the move: run from inside the
  leaf, the container lands under the leaf, which lacks the memory controller, and
  crun fails.
- **Podman's `OOMKilled` flag is unreliable rootless.** It reads `false` on a container
  the memory cgroup killed (exit 137, one kernel memory-cgroup OOM). The daemon should
  read `memory.events` itself.
- **User namespaces.** `kernel.apparmor_restrict_unprivileged_userns=1`, so a plain
  `unshare --user` fails for the runner user. Rootless Podman works: one subuid range of
  65536. The runner user already has lingering enabled and a user bus. Both cgroup
  managers applied the limits.
- **Networked actions.** Podman 4.9's default rootless network here is slirp4netns:
  run `37588003106` (`8ba9601`) started a container with no `--network` flag and found
  a `slirp4netns` helper and no pasta, on both arches. From it, and from an explicit
  slirp4netns container, a container reaches a port published on the host's own
  address (MinIO answered).
  With pasta it did not. So a `networked` action is kept off the store's port only by
  the network mode or a firewall rule, and the integration test must check it.
- **Ports.** Unprivileged ports start at 1024. The ephemeral range is 32768 to 60999.
  Four TCP sockets listen when the job starts, and the probes' high fixed ports
  (19000 to 19303) never collided.
- **Timing.** The I/O step with nine arms takes 571 s on x86 and 659 s on arm. One
  configuration plus its mutant, three rounds of 15 s each, fits in about 2 minutes.
  The x86 CPU model changed between runs (EPYC 7763 and 9V74), so time-based thresholds
  need margin. A cold `cargo build --workspace --locked` takes 22 s / 28 s today (754
  MiB of `target/`) after a 7.5 s / 5.5 s toolchain install. It will grow with the
  code. Pull-request runs never restore a saved cache, because `rust-cache` saves only
  on `main`.
- **Disk.** There is no separate `/mnt` disk on these images. The workspace disk
  reports `rotational=1` and scheduler `none`, so do not read the device type from
  sysfs.
