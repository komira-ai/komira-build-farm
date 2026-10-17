# A Linux build host

How to run `kbf-daemon` with the container driver (`--driver=container`) on a
dedicated Linux build host: the systemd units, swap, and how much memory to leave the
host. What the daemon does with its cgroup is in
[daemon.md](../design/daemon.md) (the OCI backend); this page is what the host gives it.

The host runs cgroup v2 alone (the default on current distributions; otherwise boot
with `systemd.unified_cgroup_hierarchy=1`), and a `kbf` user with a subordinate uid and
gid range of at least 65,536 in `/etc/subuid` and `/etc/subgid` (the daemon checks
them at start). kbf runs containers with `crun` only.

## What the host installs

The daemon makes its containers with `crun`, rootless, from the `kbf` user's account:

| Needs | Where it comes from |
|---|---|
| `crun` at `/usr/bin/crun` (the units pass `--oci-runtime=/usr/bin/crun`); 1.14.1 is the version the tests run | the distribution's `crun` package |
| `newuidmap` and `newgidmap`, setuid root (or holding file capabilities), on the daemon's `PATH` | the `uidmap` package |
| a subordinate uid and gid range of at least 65,536 for `kbf` | `usermod --add-subuids 100000-165535 --add-subgids 100000-165535 kbf` |
| kernel 5.11 or later (overlayfs inside a user namespace) | the distribution |
| cgroup v2 delegated to the daemon's unit | the units' `Delegate=yes` |
| every directory from `/` to `--data-dir` searchable by others (`chmod o+x`): the container's ids are not the daemon user's | the host |
| unprivileged user namespaces allowed for `crun` | on Ubuntu 24.04, the AppArmor profile `crun` ships |
| headroom under `kernel.keys.maxkeys` (200 per user by default; each container's session keyring counts) | `sysctl kernel.keys.maxkeys` |
| egress to the registries the pinned images come from (`registry-1.docker.io` for `docker.io`, `gcr.io`) | the network |

What an action sees of the host is only its image and its inputs: its root `/` is the
unpacked image, **read-only**, and only `/tmp`, `/var/tmp`, `/run`, `/root` and the
input root `/kbf/root` are writable; an action that writes anywhere else gets `EROFS`.
That is the driver's behaviour and stays so until a writable overlay of the image is
chosen.

The daemon checks the id range, the directories' search permission, `crun` and the two id
tools at start and refuses to start naming the one that is wrong. Lingering and
`XDG_RUNTIME_DIR` are not needed by `crun`; `kbf-deploy linux install-units` still checks
that `kbf` lingers because its units still set `XDG_RUNTIME_DIR`, until they are changed.

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
`memory.swap.current`, and counts as the lease's own only the swap that rose across
a sample in which its `max` events grew (it was pressing its own cap); a fall comes
out of that share first. It kills the whole lease when its own share is more than
the larger of `--lease-swap-kill-mib` (default 512) and `--lease-swap-kill-percent`
(default 25) of its booking, which can happen only at a sample where it is pushing
into swap at its cap. It reports that kill, like the kernel's own OOM kill at the
lease's cap, as the action's out-of-memory kill (`MEMORY_KILL_OWN_LIMIT`). Swap
that host pressure moves out of a lease while it is below its cap counts no `max`
events, so it is never the lease's own, even when the lease later reaches its cap and
pushes a few pages more. One case is not told apart: swap that host pressure moves in
the same sample in which the lease's `max` events grow counts as the lease's own (the
two counters the watch reads cannot separate them). A lease that sits at its cap, for
example one reading many files into page cache, can be killed for such swap once it
adds up past the threshold. A kernel OOM kill by
`actions/memory.max` or the host is reported as the node's
(`MEMORY_KILL_NODE_PRESSURE`).

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

On a converged storage node `kbf-deploy linux swap` makes these steps, each only if the host
lacks it ([storage-node.md](storage-node.md#installing-on-the-host)).

### Headroom: `--actions-memory-max-gib`

Set `--actions-memory-max-gib` to the host's RAM minus 4 to 8 GiB: on a 256 GiB host,
248 to 252. The rest stays for the kernel, systemd, sshd, the daemon (whose own
`memory.min` sits outside `actions/`) and the containers' runtime processes. The node reports the
lower of this cap and its memory as `mem_gib`, so the scheduler books no more than
leases may use. On a host that also runs a `kbf-server` and a node of the object
store (the [converged topology](../design/deployment-topology.md#converged-topology)),
also subtract what those two use at their peak
([storage-node.md](storage-node.md#memory-on-a-converged-host)).

### CPUs: `--reserve-cpus`

Leave the unit and its slice without `CPUQuota=` and `AllowedCPUs=`; the daemon takes
its share itself, and pins nothing. `--reserve-cpus=N` (logical CPUs, a hyperthread
counts one; default 8, which is 4 cores of 2 threads, or half the CPUs on a machine of
fewer than 16) is what the farm's own processes keep:

- the leases get the CPUs the daemon may use (`available_parallelism`: the affinity
  mask and the cgroup quota) minus `N`, as `actions/cpu.max` (that many CPUs of time,
  on any CPU: a bandwidth limit, not a cpuset). The node reports it as `cpus`. Within
  the limit, CPU is compressible as before: each lease has a `cpu.weight` from its
  booked CPU, and an idle CPU goes to whichever lease can use it.
- the daemon's own pools are sized from `N`: async workers `max(2, N/4)`, the CPU pool
  (`--cpu-threads`, hashing) `max(2, N/2)`, each at most the physical cores.
  `--async-threads`, `--blocking-threads` and `--cpu-threads` override them (a value
  given is used as given). On the 88-CPU, 44-core node: reserve 8 is 2 workers and 4
  CPU threads, 80 CPUs for leases.
- `N` of 0 is allowed; `N` at the CPUs the daemon may use or more is refused at start
  (the leases would get nothing).

`--reserve-cpus` and `--reserve-share` are the daemon's flags; `kbf-server` has neither
and refuses them at start (an old unit that passes one fails at once, status 2, instead
of the flag being ignored). The server runs no actions, so it uses every CPU it may: one
async worker and one hashing thread (`--cpu-threads`) per logical CPU (hyperthreads
included: the server has no other tenant, and a hashing thread stalled on a read leaves
its sibling the core), and tokio's cap of 512 on the blocking pool; the start line
names the sizes. `--async-threads`, `--blocking-threads` and `--cpu-threads` override
them on the server too. On a host that runs both (the all-in-one), `N` is the daemon's
`--reserve-cpus` and covers the server's share as well: give it enough for what the
server and the object store node use at their peak. A unit's own `CPUQuota=` lowers the
`cpus` the node reports to it, and the CPUs the server counts, as well.

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
# Not used by crun (it runs with an explicit --root); the installer still checks that
# the kbf user lingers (loginctl enable-linger kbf) until this line goes.
Environment=XDG_RUNTIME_DIR=/run/user/<kbf uid>
ExecStart=/usr/local/bin/kbf-daemon \
    --driver=container \
    --oci-runtime=/usr/bin/crun \
    --server=https://<server>:<worker port> \
    --cas=https://<server>:<worker port> \
    --ca-cert=/etc/kbf/ca.pem \
    --cert=/etc/kbf/node.pem \
    --key=/etc/kbf/node.key \
    --node-id=<node id> \
    --data-dir=/var/lib/kbf \
    --actions-memory-max-gib=<RAM in GiB minus 4 to 8> \
    --supervisor-memory-min-mib=256
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

The daemon dials the server's worker listener over mutual TLS, so that server runs
with `--worker-tls-cert`, `--worker-tls-key` and `--worker-client-ca`. TLS is
optional in `kbf-server`; a deployment turns it on in its config.

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
