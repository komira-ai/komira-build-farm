# A Linux build host

How to run `kbf-daemon` with the container driver (`--driver=container`) on a
dedicated Linux build host: the systemd units, swap, and how much memory to leave the
host. What the daemon does with its cgroup is in
[daemon.md](../design/daemon.md) (`PodmanRuntime`); this page is what the host gives it.

The host runs cgroup v2 alone (the default on current distributions; otherwise boot
with `systemd.unified_cgroup_hierarchy=1`), rootless Podman, and a `kbf` user with
subordinate uid and gid ranges in `/etc/subuid` and `/etc/subgid` (the daemon checks
them at start).

## Memory on the host

The daemon writes these, from its own delegated cgroup down (the first two at start,
the rest for each lease):

| cgroup | file | from | what it does |
|---|---|---|---|
| `supervisor/` (the daemon) | `memory.min` | `--supervisor-memory-min-mib`, default 256 | the kernel does not reclaim this much from the daemon, so builds short of memory take it from each other and not from the process that reports them |
| `actions/` (every lease) | `memory.max` | `--actions-memory-max-gib` | the most all leases together may use; past it the kernel reclaims and, failing that, OOM-kills inside `actions/` |
| each lease | `memory.max` | the lease's booking x 1.5 + 512 MiB | a hard cap on the lease's RAM: at it the kernel reclaims the lease's own pages, into swap while there is swap |
| each lease and its container | `memory.oom.group` | always 1 | an OOM kill ends the whole action, not one of its processes |

Each container also starts with `oom_score_adj` 0 (`--oom-score-adj=0`): a process
inherits its parent's score, and the unit below lowers the daemon's, which builds
must not keep. No lease gets `memory.swap.max`, so leases may swap.

With 128 GB of swap the kernel would kill a lease at its cap only once swap is full,
long after the build has slowed to a crawl. So the daemon watches each lease: every
`--lease-swap-poll-ms` (default 1000) it reads the lease's `memory.events` `max` and
`memory.swap.current`, and kills the whole lease when its `max` events grew since the
last sample (it is pressing its own cap now) and it holds more in swap than the larger
of `--lease-swap-kill-mib` (default 512) and `--lease-swap-kill-percent` (default 25)
of its booking. It reports that kill, like the kernel's own OOM kill at the lease's
cap, as the action's out-of-memory kill (`MEMORY_KILL_OWN_LIMIT`). A lease that host
pressure moves into swap while it is below its cap counts no `max` events and is
never killed by this rule; a kernel OOM kill by `actions/memory.max` or the host is
reported as the node's (`MEMORY_KILL_NODE_PRESSURE`).

The kernel caps a cgroup's `memory.min` at what its ancestors protect. The daemon
reads `memory.min` from its unit's cgroup up to the top-level slice and logs a
warning naming the first one below `--supervisor-memory-min-mib`; the units below set
`MemoryMin=` on both. Set it equal to `--supervisor-memory-min-mib`, not above:
systemd mounts cgroup v2 with `memory_recursiveprot` where the kernel offers it, and
then protection the unit holds beyond what `supervisor/` claims is shared out to
`actions/` too.

### Swap: 128 GB

A swapfile lets the host page out cold build memory instead of OOM-killing a build
when leases together outgrow RAM for a while. A lease that outgrows its own cap is
not left to fill it: the daemon's swap watch above kills it. On ext4 or xfs:

```sh
sudo fallocate -l 128G /swapfile
sudo chmod 600 /swapfile
sudo mkswap /swapfile
sudo swapon /swapfile
echo '/swapfile none swap sw 0 0' | sudo tee -a /etc/fstab
```

### Headroom: `--actions-memory-max-gib`

Set `--actions-memory-max-gib` to the host's RAM minus 4 to 8 GiB: on a 256 GiB host,
248 to 252. The rest stays for the kernel, systemd, sshd, the daemon (whose own
`memory.min` sits outside `actions/`) and Podman's processes. The node reports the
lower of this cap and its memory as `mem_gib`, so the scheduler books no more than
leases may use.

### CPUs: uncapped

Leave the unit and its slice without `CPUQuota=` and `AllowedCPUs=`. CPU is
compressible: each lease gets a `cpu.weight` from its booked CPU, and an idle CPU goes
to whichever lease can use it. A cpuset, when one is set, lowers the `cpus` the node
reports to what it allows.

## The units

`/etc/systemd/system/kbf.slice`:

```ini
[Unit]
Description=kbf build farm

[Slice]
# The daemon's protection is capped by its slice's: equal to --supervisor-memory-min-mib.
MemoryMin=256M
```

`/etc/systemd/system/kbf-daemon.service`:

```ini
[Unit]
Description=kbf build farm node daemon
Wants=network-online.target
After=network-online.target

[Service]
User=kbf
Slice=kbf.slice
# The daemon manages its own cgroup: it moves itself into supervisor/ and makes
# actions/ beside it, with cpu, memory and pids enabled.
Delegate=yes
# Equal to --supervisor-memory-min-mib (see "Memory on the host").
MemoryMin=256M
# Under a host-wide OOM the kernel kills a build before the daemon; containers get 0
# back from the driver.
OOMScoreAdjust=-900
# Rootless Podman's run directory: the kbf user's systemd manager, kept by lingering
# (loginctl enable-linger kbf).
Environment=XDG_RUNTIME_DIR=/run/user/<kbf uid>
ExecStart=/usr/local/bin/kbf-daemon \
    --driver=container \
    --server=https://<server>:<worker port> \
    --cas=https://<server>:<worker port> \
    --ca-cert=/etc/kbf/ca.pem \
    --cert=/etc/kbf/node.pem \
    --key=/etc/kbf/node.key \
    --node-id=<node id> \
    --scratch=/var/lib/kbf/leases \
    --actions-memory-max-gib=<RAM in GiB minus 4 to 8> \
    --supervisor-memory-min-mib=256
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

The unit sets no `MemoryMax=`, `MemorySwapMax=`, `CPUQuota=` or `AllowedCPUs=`; the
cap on builds is `actions/memory.max`. A `MemoryMax=` on the unit or slice, if one is
set anyway, also bounds the `mem_gib` the node reports.

Then:

```sh
sudo loginctl enable-linger kbf
sudo systemctl daemon-reload
sudo systemctl enable --now kbf-daemon.service
```

At start the daemon logs the cgroup it set up, the `memory.min` it gave itself, and
what leases may use; a warning that names `MemoryMin=` means the unit or slice
protects less than `--supervisor-memory-min-mib`.
