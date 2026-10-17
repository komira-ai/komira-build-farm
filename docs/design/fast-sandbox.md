# Fast per-action sandboxes: shared read-only trees and a direct OCI runtime

Status: DRAFT, revision 3 (after security, adversarial and Rust-implementation review of revision 1,
and the maintainer decisions recorded in section 14). Revision 3 drops the kbf launcher: `crun` creates
every namespace and is driven through the `runc` crate; sections 1, 3, 5.2, 6, 8, 9, 11, 12 and 14
changed and section 15 lists what. Nothing here is built. Applies to the Linux container driver
(`crates/kbf-driver-container`); the macOS native driver and the draft VM driver are unchanged.

Claim tags: **[M]** measured on a kbf node or read in kbf code; **[P]** published (Sources);
**[I]** inference, to be settled by an experiment in section 11. Every latency number in section 7 is
[I]: nothing here promises a latency before the section 11 gates have run.

## 1. Summary

Today one action costs about nine `podman` processes, about 15k `link()` calls and four ownership
passes. Measured on a 44-core node [M]: p50 about 21 s create/start/cleanup and about 16 s linking a
15k-file toolchain tree at 50-80 concurrent actions; about 2 s for a tiny action on an idle node.

What the data say, and what they do not:

- Each `podman` invocation takes 0.10-0.25 s idle and 4-7 s at 80+ open leases, measured with
  podman's own monotonic clock inside the process [M]. Nine of them are about 45 s of a 60 s lease.
  CPU pressure and the SSD's own pressure were near zero over the same period [M], but PSI does not
  see uninterruptible waits on the ext4 journal or on rw-semaphores (they raise load average without
  raising pressure) [M]. **The cause of the inflation is therefore not attributed.** Candidates are
  podman's state database and storage locks, and fsync waits on the shared ext4 journal (99.9% of
  913,901 journal transactions were forced by fsync callers [M]). If the journal is the cause, this
  design inherits part of it. Section 11.3 makes the attribution a gate before the runtime change.
- Linking costs 0.6-0.75 ms per file idle against 17 us for the `linkat` itself [M]: the pipeline
  around the syscall (two blocking-pool hops per blob, an index mutex, sequential `create_dir`), times
  15k files, times every action. A plain loop would plausibly cost 0.3-0.8 s for 15k links [I]; section
  4.2 and step 0 test that before trees are credited with the whole 16 s.

The design, in order:

0. **Gates.** Attribute the podman time (E1) and measure raw `fsync`/`linkat` at 1, 8 and 64
   concurrency on the node's filesystem with no podman (E0); write a bulk linker and re-measure the
   16 s (E8); compute offline how many real actions the tree classifier covers (E9); run the crun floor
   and the rootless-mapping probes on a build host (E2, E2b).
1. **Shared trees**, on the existing podman driver. A large Directory node that recurs across actions
   is written once to `trees/<digest>/` and mounted read-only into each action that has it.
2. **No podman.** A new backend of `kbf-driver-container` writes an OCI bundle (`config.json` built with
   the `oci-spec` crate) and drives **`crun` through the `runc` crate**. `crun` itself creates the user,
   mount, pid, ipc, uts, net and cgroup namespaces, so kbf has no launcher, no `unshare` of its own and
   no AppArmor profile to ship. No image store, no database, no `conmon`, no `podman unshare`. The
   runtime binary is a flag (`--oci-runtime=/usr/bin/crun`), so `runc` or `youki` can be swapped.

**Primary:** `crun run` on a kbf-written bundle, rootless, one fresh sandbox per action, trees as
read-only bind mounts, the per-action input root as an overlay mounted by `crun`.
**Fallback (stated rollback):** step 1 on the podman driver (extra `:ro` volumes) plus the per-node
`--container-runtime` flag. It keeps the nine podman processes. An in-process runtime is **not
planned** (section 9). If `crun` alone cannot mount a rootless overlay, section 3.2 gives the second
fallback inside the same backend (read-only rootfs directory, no overlay on `/`), still with no profile.

## 2. Requirements and the industry baseline

Fresh sandbox per action, no reuse, no pre-started containers. Rootless (user `kbf`, no setuid
helper of ours, no root service). Isolation of the current strength: user, mount, pid, ipc, uts,
net namespaces; `--network=none`; the container's ids are subordinate ids, never kbf's
(`--userns=nomap`); seccomp, capability drop, masked paths; cgroup v2 limits. Base image pinned by
digest. Input root = shared read-only trees + small per-action files. Outputs read only from a
writable layer. Mac stays native.

What others do [P] (detail in the survey): BuildBuddy's executor writes an OCI bundle and runs
`crun` directly, with image layers as overlay lowers; Bazel's `linux-sandbox` builds namespaces and
mounts itself, binding inputs read-only; Buildbarn keeps one long-lived runner per slot and fills a
directory from its file cache; AWS Lambda restores a Firecracker snapshot. Lambda's speed comes from
doing no per-start file work and holding no shared lock, not from reusing a container; the same two
properties are what this design buys (trees: no per-file work; one `crun` per action: no shared state).
BuildBuddy's executor is under the BuildBuddy Enterprise license (production use needs a subscription),
so kbf takes the approach (an OCI bundle, `crun`, overlay and bind mounts) and none of the code; this
document was written from public documentation and kbf's own measurements.

## 3. Architecture

```
kbf-daemon (user kbf, tokio)
  tree store      trees/<digest>/            read-only, hard links to the file cache, LRU; dir 0700
  image store     images/<digest>/rootfs/    unpacked once per image digest, read-only
  per action      lease/{root,upper,work,bundle/,crun/}
  keeper          kbf-daemon __keeper (unchanged): holds the pipe, writes cgroup.kill on close
  OCI backend     oci-spec -> lease/bundle/config.json; runc crate client, command = --oci-runtime,
                  global opts: root = lease/crun, cgroupfs manager, log file in the lease
        |
        | spawn (1 exec): crun --root lease/crun run --bundle lease/bundle <id>
        v
crun (user kbf, distro AppArmor profile for /usr/bin/crun)
  user ns via newuidmap/newgidmap from the mappings in config.json; mount, pid, ipc, uts, net, cgroup
  ns; mounts (rootfs, /kbf/root overlay, read-only tree binds), /proc /dev /sys; pinned seccomp, caps,
  masked paths; pivot_root; exec
        |
        v  (after the action exits and the lease cgroup is empty)
reown pass: one more crun run of a "tool" bundle (section 3.0) that gives `upper` back to kbf
```

Roughly six execs per action (`crun`, `newuidmap`, `newgidmap`, twice) against about seventeen.

**Fresh sandbox per action, no reuse.** Every action gets its own bundle and its own namespaces,
created by `crun` at the start of the action and gone with its last process. Nothing is pre-started,
pooled or kept alive between actions, which is also what a Lambda-style host (where a container cannot
be held warm) requires. What may be prepared ahead are inputs (trees, images), never sandboxes.

**Why no launcher.** Revision 2 had a small `kbf-sandbox` binary create the namespaces, so it needed its
own AppArmor `userns,` grant on Ubuntu 24.04. Ubuntu already ships profiles that grant `userns` to
`/usr/bin/crun` and `/usr/bin/podman` [M, on the build hosts]. If `crun` creates every namespace, kbf
needs no profile and no id-map handshake, and the daemon (which parses network input) never gains user
namespaces itself: only the exec'd `crun` has them.

The `__keeper` stays a separate tiny process. The lease cgroup contains the container's processes
(`crun` places them there; `crun` itself stays outside). If the daemon dies the keeper still writes
`cgroup.kill`. A conformance test SIGKILLs `crun` alone and expects the container to die (crun's
monitor is the container's parent; the cgroup kill is the backstop).

### 3.0 Ownership without a setup namespace: the tool bundle

Rootless means the action's ids are subordinate ids (`/etc/subuid`, size checked at start by
`subids.rs`), never kbf's: the action bundle maps container ids `0..65534` to host ids
`subuid_start..+65535` (`uidMappings`/`gidMappings` with host ids from the subuid range; `crun` runs
`newuidmap`/`newgidmap` for that when it is not root [I, E2b]). That is today's `--userns=nomap`. The
price is that kbf cannot create, chown or delete files owned by those ids from outside a user namespace.
Podman paid it with `podman unshare`; revision 2 with a launcher. Revision 3 uses `crun` for it too:

A **tool bundle** is a second, tiny `crun run` with the mapping `0 -> kbf`, `1..65535 -> subuid_start..`.
Inside it kbf is root, and the action's id X is tool id X+1 (the same offset the old setup namespace
had). Its rootfs is the same read-only base image; the only things it mounts are the node binary
(`kbf-node __reown`, bind-mounted read-only) and the lease's `upper` and `work` (read-write). It has no
network, the same seccomp and capability set, and it runs only after the action's cgroup is empty, so
no live process races its by-descriptor, `O_NOFOLLOW` walk. It does the three jobs that need a namespace:
give `upper` back to kbf (5.2), remove the lease's subordinate-owned files (8), and, in step 3, unpack
an image with layer uid X written as tool id X+1 (6.2). Cost: one more `crun` floor (about 50 ms on
`crun`'s own benchmark [P]), measured in E2/E3 and weighed against the old four ownership passes. The
pre-run chown of `upper`/`work` is avoided by creating them as kbf-owned `0777` directories inside a
`0700` lease directory (E4 measures it); the shared trees need no ownership work (5.2).

### 3.1 What the container sees

| Path | Source | Writable |
|---|---|---|
| `/` | `images/<image digest>/rootfs`, `root.readonly`; see 3.2 for the overlay variant | no (`/tmp`, `/var/tmp`, `/run`, `$HOME` are tmpfs mounts from the config) |
| `/kbf/root` | overlay mounted by `crun` from `mounts`: lower `lease/root` (per-action files), upper `lease/upper` | yes; **outputs are read from `upper` only** |
| `/kbf/root/<path>` per shared tree | bind of `trees/<digest>`, `ro,nosuid,nodev`, `crun` remounts it read-only | no |
| `/proc`, `/dev`, `/sys`, `/dev/shm` | crun, from the pinned config | per OCI defaults; `/dev/shm` sized by `ContainerLimits` |

Overlay options are pinned, not defaulted (section 5.3). `/kbf/root` keeps today's meaning (working
directory inside it; an output path already in the input root is refused `INVALID_ARGUMENT` before
anything runs). A tree's mount point is an empty directory in `lease/root`, created by `materialize` as
a directory node, so no symlink can sit at a mount point; a conformance row plants a symlink there
anyway (mutant) to confirm `crun` resolves mount destinations beneath the rootfs and not through it.
The read-only property is that of a bind mount in the action's own user namespace: `crun` applies
`MS_RDONLY` and the flags are locked, so the action cannot remount it writable (conformance row).

### 3.2 The rootfs question and the stated fallback

Revision 2 mounted an overlay as `/` so that writes anywhere were discarded. `crun` takes a rootfs
**directory** and does not mount one for you, and it runs no step between creating the namespaces and
setting the rootfs. Two cases, settled by E2b:

- **Case A (preferred if E2b passes):** the image rootfs is `root.readonly`, with `tmpfs` mounts for the
  writable places a toolchain needs (`/tmp`, `/var/tmp`, `/run`, `$HOME`), sized from `ContainerLimits`.
  A write elsewhere under `/` fails `EROFS`. This is a behaviour change for actions that write to, say,
  `/var/cache`; E6 runs the real toolchain corpus and names every such path before the flip.
- **Case B (if `crun` cannot mount an overlay in its own user namespace for `/kbf/root`):** bind-mounted
  read-only trees and a plain read-only rootfs directory as above, and the writable input root is a
  plain per-action directory (`lease/root`, per-action files hard-linked as today, trees bound read-only
  inside it). Outputs are read from that directory. Still no launcher and no profile; the cost is
  that an action's writes to its own input files are not separated from outputs by an upper layer, so
  the collector keeps a manifest of the declared input files and treats only new or changed ones as outputs
  (the check `outputs.rs` already makes for the podman driver's `upper` is the model; stated here as a
  work item, not designed).

In neither case does kbf run a program of its own before `crun`, which is the point of the decision.

## 4. Shared trees

### 4.1 Which Directory nodes qualify

The daemon walks the action's input root from the top, as `kbf_daemon::tree` does. A Directory node
becomes a shared tree when **all** hold:

- it holds at least `--tree-min-files` regular files and symlinks below it (default 256, the C4b
  threshold in `worker-cache.md` 6.6);
- **it has recurred**: its digest has been seen in at least `--tree-min-sightings` distinct actions
  (default 2) according to the summary map, or a scheduler hint names it. A parent's digest changes when
  any descendant does, so in an exec root where toolchain and sources share a directory the highest
  node is action-unique; a tree built for it is built per action, evicted, and is worse than today.
  A node that has not recurred is **descended into** and its children are considered instead;
- no output path of the `Command`, and not the working directory, lies at or under it (the `private`
  set `materialize_cached` takes today): such a subtree is walked into, and only its qualifying
  children are considered. (An output under `external/` with working directory `.` disqualifies a
  whole toolchain; correct, and counted by E9 so the cost is known);
- it is not the root, and it is the **highest** node on its path that meets the rules above;
- the action has fewer than `--tree-max-mounts` trees (default 8, largest first; the rest are linked
  per action as today), and the node has built fewer than `--tree-build-bytes-per-min` bytes of trees
  in the last minute (the cap stops a corpus of unique digests from churning the store).

Everything else is per-action: the directory skeleton, symlinks and files outside trees, written
with the bulk linker (4.2). The classifier also enforces an **aggregate cap**: if more than
`--tree-max-uncovered-files` (default 2,000) files stay per-action, the action is logged as
"uncovered" with its count, so a classifier that covers too little shows up in the node report rather
than as a silent 16 s.

A node's file count comes from a **summary** (digest -> files, bytes, sightings) filled bottom-up on
the first walk that fetches the node's Directory protos, kept in memory and in `trees/<digest>/.summary`.
The Directory cache (C4a: decoded `Directory` messages by digest, shared by all leases) is a
prerequisite: without it the walk above the trees is a CAS round trip per level per action [M, code:
no Directory cache exists today].

### 4.2 Preparation, and the bulk linker

1. Single-flight by digest in the daemon (a `OnceCell` per digest in a map; no file lock).
2. Build in `trees/.tmp-<random>/` with the **bulk linker**: one blocking thread, plain `mkdirat` and
   `linkat` loops over a pre-fetched Directory tree, no per-file task hops and no per-file index lock
   (pins are taken in one batch). Directories 0755, symlinks as in the tree, each file a hard link to
   its cache entry (the cache pins it while the tree exists; files stay owned by kbf). The same linker
   serves the per-action tree and the fallback path, replacing `materialize_cached` for them; trees must
   not be built with the slow pipeline, or a cold node pays 11 s idle and about 40 s loaded and every
   waiter queues behind it.
3. `renameat2(RENAME_NOREPLACE)` to `trees/<digest>`. Presence of the final name means complete. A
   crash leaves only `.tmp-*`, swept at start. Losing the rename race: delete mine, use theirs.
4. Cost: one bulk link, paid once per digest per node. Directory inodes only; file data is the cache's.

**Cold node.** Eighty first-wave actions would wait on the same single flights. Pre-warming from the
first sight of a digest in the scheduler's placement hint (`worker-cache.md` 8) is part of step 1, not
a later option, and E10 measures the first minutes after a deploy against today.

### 4.3 Eviction and accounting

- A tree holds one cache pin per distinct file. Trees for different toolchain versions share inodes,
  so weight is **not** charged per tree; the store tracks the set of distinct pinned inodes (a
  refcount per inode in the cache) and charges each once. `--tree-store-max-bytes` bounds pinned
  distinct bytes. The cache's LRU cannot free a file a tree links, so the two budgets could starve each
  other: a test fills the store to its cap and checks the cache can still admit (risk 8).
- An in-memory refcount counts attached actions. Only refcount 0 is evicted: `rename` to
  `trees/.dead-<n>` first (a new request re-prepares under the digest name), then the `remove.rs` walk
  in the background, then unpin. An in-use mount source is EBUSY anyway; the refcount keeps the policy
  simple and the failure a logged skip.
- At daemon start there are no running actions (the keeper kills them), so refcounts begin at 0.
- Hard links cut link-count pressure (the 65,000-link ext4 ceiling per inode, `worker-cache.md` 6.5):
  one link per tree per file instead of one per action per file.
- **Page cache.** The tree, the per-action lower and the cache entry are hard links to the same inode,
  and overlayfs reads through the real inode, so 80 actions share one page-cache copy of a toolchain.
  That is a reason to keep hard links over copies. Shared pages are charged to the first cgroup that
  touches them, and copy-up never writes a cache inode (the lower is never written).

### 4.4 Correctness: an action cannot modify a shared tree

There are **two** barriers, plus detection; they are stated as what they are.

1. The attach is a **read-only bind mount** that `crun` makes in the action's own mount namespace
   from `mounts` (`ro,nosuid,nodev`). Inside a user namespace a read-only flag cannot be cleared by that
   namespace's root [P, `mount_namespaces(7)`: locked mount flags]. Conformance row: as container root
   with `CAP_SYS_ADMIN` in its own ns, `mount -o remount,rw` fails (E2b d); mutant: drop `ro` from the
   mount entry.
2. Files are owned by kbf, which is unmapped in the container, and have no group/other write bit.
   The cache already creates entries 0444 (0555 executable) and `verify.rs` checks mode and owner when it
   places an entry [M, code: `layout.rs` `Key::mode`, `verify.rs:39`]; REAPI carries only `is_executable`,
   so kbf sets modes itself. Container root has no `CAP_DAC_OVERRIDE`/`CAP_FOWNER` over an inode whose
   owner it does not map. A conformance test fails if any cached file is group- or other-writable.

Not barriers, and said so:

- The host-side path to a tree is closed by mode: `trees/` and its parent are 0700 kbf-owned
  (conformance test). The container has no path to the cache or `trees/`; only the tree is mounted.
- `verify.rs` runs on `place`; a write through a shared inode after placement is not detected there. The
  tree store therefore re-hashes a **sample** of tree files periodically (bytes read per hour capped)
  and on any `EROFS`-bypass suspicion, and treats a mismatch as a cache poisoning event: evict, alert.
  This is detection after the fact, not a barrier.

Edge cases:

- **Symlinks in a tree** stay symlinks and resolve inside the container, never on the host. Names are
  checked at preparation by `check_names` (no `.`, `..`, `/`, NUL, duplicates).
- **Hard links.** REAPI has no hard-link node; equal files may share one cache inode, so `st_nlink > 1`;
  `link(2)` from a tree path into `/kbf/root` fails `EXDEV`. Tools that hard-link without a copy fallback
  fail (risk 3).
- **Ownership.** Per-action files and tree files are kbf-owned and unmapped, so an action cannot edit its
  own inputs in place: a toolchain that does gets `EACCES` (not only `EROFS`). That matches today.
- **`rename(2)` across a tree boundary** fails `EXDEV`; a write into a tree fails `EROFS`. Today a new
  file in a toolchain directory succeeds. This is a behaviour change (D1). The escape hatch is **per-run
  fallback to the bulk-linked per-action tree for that digest** (today's semantics, at today's cost
  minus the pipeline overhead), not a writable shared overlay (rejected, section 15).
- **Output under a tree** disqualifies that tree (4.1), so outputs are always in `upper`.
- **Unexpected writers.** The tree directory is written only by the daemon (preparation, eviction),
  never through a mount; rename-then-delete means an in-use tree is never edited.

## 5. Ownership and mounts

### 5.1 Why idmapped mounts are not the answer

Creating an idmapped mount needs `CAP_SYS_ADMIN` in the filesystem's user namespace; a rootless user
does not have it, and podman's documentation says `idmap` is rootful-only [P]. EROFS/composefs needs a
root mount on 6.8 [P]. Mapping container root to kbf would remove the chown passes but undo the
isolation `worker-cache.md` 5.1 depends on. None is used.

### 5.2 What replaces the chown passes

| Today (`give_overlay`, 4 execs of podman or helper) | New |
|---|---|
| chown the input root's directories to `1:1` (15k `statat`, one `fchown` per directory) | Shared trees need none (bind mounts of kbf-owned, read-only directories; ids that the action's namespace does not map read as `nobody`, and mode bits give `o+rx`). The per-action tree is hundreds of entries; its directories are `0755` and readable by "other", so the overlay lower needs no chown. Files and symlinks are never touched: they are hard links to cache inodes, and `chown` on one re-owns the shared inode for the cache, every tree and every other action, which would end barrier 2. |
| chown `upper`, `work` to `1:1` | Created as kbf-owned `0777` directories in a `0700` lease directory (E4 measures; if the overlay refuses them, the tool bundle chowns them once before the run, one more floor). |
| after the run, chown `root`, `upper`, `work` back to `0:0` | The tool bundle's reown of `upper` described below. `root` is never chowned. |

Conformance test: stat a cached inode's owner and mode before and after a lease and expect them
unchanged (mutant: a chown that follows files).

**Reown order (after the run).** (1) `crun` has exited; (2) the daemon writes `cgroup.kill` and **waits
until the cgroup is empty**, so no live process can race the walk by swapping directories or symlinks;
(3) the tool bundle (3.0) walks `upper` by descriptor with `O_NOFOLLOW`, as `chown_dirs.rs` and
`outputs.rs` do, giving each entry to kbf (tool id 0). Rules: `chown` clears setuid/setgid and file
capabilities on non-directories (kernel `chown_common` `ATTR_KILL_SUID/SGID/PRIV`; confirmed on 6.8 by a
test, not assumed): an action that creates a mode-4755 file and a `security.capability` xattr must
produce an output with neither. Outputs are attacker-shaped, so the collector stays `O_NOFOLLOW`,
size-capped and special-file refusing as today, and also handles mode-000 directories, symlinks to host
paths and sparse files; the checklist is a test row.

The order cannot assume the daemon survived: section 8 covers leases left by a dead daemon.

**Candidate to measure (E4):** the per-action lower's directories at `0777` so nothing is chowned at
all. It changes `ls -l` inside the container and is measured only; it is never applied to a shared tree.

### 5.3 Overlay and mount options (pinned)

Both overlays are mounted with `userxattr,metacopy=off,redirect_dir=off,index=off,xino=off`, and `/`,
`/kbf/root` and every tree attach carry `nosuid,nodev`. With `userxattr` the overlay metadata lives in
`user.overlay.*` xattrs, which the container can write on its own files; `metacopy`/`redirect_dir` off
means a stub or redirect planted in `upper` cannot make collection read empty data or follow a redirect.
The collector handles the opaque-directory xattr and the 0:0 whiteout character device explicitly
(a whiteout is a deleted file, not an output). Today these options are podman's defaults; they become a
tested constant (a unit test pins the option string, a conformance row reads `/proc/self/mountinfo`).
`volatile` is a candidate on both overlays (E2/E3), since neither upper needs crash safety.

## 6. Runtime: what kbf writes

### 6.1 The bundle, and the hardening podman used to supply

**Podman, not `crun`, supplies today's hardening** [M, code: `podman.rs` passes only `--userns=nomap`,
`--network=none`, limits; seccomp, capabilities, masked paths, `/dev` and sysctls are podman's
defaults]. `crun` applies only what `config.json` contains; its `crun spec` output is minimal (to be
confirmed by running `crun spec` on the CI runner, step 0). Removing podman without replacing that
policy would give the action the full capability set in its user namespace and no seccomp filter, and the
main attack on a rootless sandbox is a kernel bug reached through capabilities in a user namespace (for
example `unshare(CLONE_NEWUSER|CLONE_NEWNET)` then nftables).

Therefore kbf **vendors and pins by digest**, in the OCI backend: the seccomp profile (containers-common
`seccomp.json` at a pinned version), the capability set (checked against what the podman driver gets
today), the masked and read-only paths, `noNewPrivileges: true`, the minimal `/dev` device list, the
default sysctls (`net.ipv4.ping_group_range`), a private cgroup namespace, `/sys/fs/cgroup` read-only,
and `--no-new-keyring` behaviour. A golden `config.json` generated from the vendored inputs is checked in.

`lease/bundle/config.json` is otherwise generated from the inputs of `create_args` in `podman.rs`:

| podman (today) | OCI `config.json` |
|---|---|
| `--network=none` | `network` namespace with no path; loopback up (E2 checks that `crun` does it) |
| `--userns=nomap`, subids | `linux.uidMappings/gidMappings`: container `0..65534` -> host subuid range (3.0) |
| `--user=0:0` | `process.user` 0:0 |
| `--unsetenv-all`, env | `process.env` = the Command's variables only |
| `--cgroup-parent`, `memory.oom.group=1` | `linux.cgroupsPath` = the lease cgroup the daemon creates; daemon writes `memory.oom.group` as now |
| pids, `nofile`, `nproc` | `linux.resources.pids`, `process.rlimits` |
| `/dev/shm` size | `tmpfs` mount with `size=` from `ContainerLimits` |
| `--oom-score-adj=0` | `process.oomScoreAdj` |
| hostname | `hostname` (`/etc/hosts`, `/etc/hostname` from small files in the lease dir) |
| seccomp, caps, masked paths, noNewPrivileges | the vendored, pinned set above, written explicitly |

`crun run` stays foreground with `process.terminal=false`; stdout and stderr are the files the daemon
opens now. The OOM classification stays on the lease cgroup's `memory.events`. `crun --root
<lease>/crun` keeps runtime state per action: no shared directory.

**Exit contract.** The backend reports errors in-process (`thiserror`; `anyhow` only in `main`) and maps
them to the driver's Failed/Infra split: bundle write failed, `crun` failed to start the container (its
`--log` file in the lease dir, read on a nonzero status before the process ran), mount or id-map failed
(the same log), reown failed, and "the action exited N". An action that exited 1 is told from a `crun`
failure by `crun`'s log and by `state`: the container reached `running` and its init exited, or it did
not. [I: confirm `crun run` returns the container process's own status and uses a distinct code for its
own failures; E2.] A table test covers the mapping.

**File descriptors.** The daemon opens everything `O_CLOEXEC` and the `runc` crate spawns `crun` with
only the stdio files; `crun` closes its own descriptors before `exec` (it has had the "Leaky Vessels"
class of bug fixed; the version floor in 6.4 covers it). A conformance row asserts `/proc/self/fd`
inside lists only 0, 1, 2 (mutant: a daemon descriptor without CLOEXEC passed to the spawner).

### 6.2 Base image

`images/<digest>/rootfs` is produced once per image digest and never changes. Acquisition is off the
start path. Step 2a: extract from the podman store that already holds the image (`check_pinned` reads it
today). Step 2b: a pure-Rust pull and unpack run in the tool bundle (3.0), so layer files
with uid X land owned by tool id X+1, which is the action's id X. A digest pin proves provenance, not that the tar is benign, so
the unpacker is treated as a parser of untrusted input: path traversal and symlink-then-write ordering
are refused (every path is opened relative to the rootfs with `openat2 RESOLVE_BENEATH`), hardlinks may
only point inside the rootfs, setuid bits and `security.capability` xattrs in layers are kept only as the
image declares and reviewed (the image is pinned), device nodes are refused, decompressed size is
capped, and the tar crate is chosen after checking recent parser-desync advisories (a Rust async tar
fork had one in late 2025: confirm the number and the fixed version before choosing; [U]). It runs in the
narrowest namespace that works and is fuzzed like the spec parser. Image-by-digest pinning, the "tag or
index digest is `INVALID_ARGUMENT`" rule and the `kbf.owner` sweep stay as today.

### 6.3 Why `crun` through the `runc` crate, and what the crate gives

The runtime is the **`crun` CLI, driven by the `runc` crate** (containerd/rust-extensions, Apache-2.0,
0.3.x [P]). The crate is a client for the runc command line; `GlobalOpts::command(path)` points it at
`/usr/bin/crun` (crun implements the same `--root`, `--log`, `--log-format`, `--systemd-cgroup`
interface and the `create`/`start`/`run`/`state`/`kill`/`delete` verbs). It has a sync and an async client
and a `custom_spawner` hook. Its `exec` is missing from the async client (not needed: one process per
sandbox). Typed `config.json` comes from the `oci-spec` crate's builders (Apache-2.0), so the table in
6.1 is code, not a string template, and the golden file is generated from the same builders.

Used as follows: `run` (foreground, `detach(false)`) for the action, because only a foreground `crun run`
returns the container process's exit status; `kill`/`state`/`delete` for the failure paths. Items to
confirm at step 0 rather than assume [U]: that the crate's `run` returns the container's exit status
(if not, the one `run` call uses `tokio::process::Command` and the crate keeps the other verbs); that
`GlobalOpts::rootless` stays unset or `auto` (crun has no `--rootless` flag, it detects it); the crate's
dependency tree and license set against kbf's `deny.toml`; that the `custom_spawner` lets the daemon set
`PR_SET_PDEATHSIG` and a process group so a daemon crash does not leave a stray `crun`.

Rejected: `libcrun` (`./configure --enable-shared`, no API-stability promise, bindgen plus libyajl,
libseccomp and libcap in the build closure, C that calls `fork` and `unshare` inside our binary, saves
one `execve`, about 1 ms against a 47 ms floor). youki's `libcontainer` (0.x, moving API, needs
libseccomp and libsystemd at build time, few reverse dependencies, and the one independent run recorded
36 failures in 1,000 starts for 0.3.3, a 2024 blog, "not scientific"; `runtimes.md`). Speed is not the
argument (the 111 vs 47 ms bench compares CLI to CLI).

### 6.4 `crun` provisioning

`crun` comes from the distribution package on nodes (1.14 on Ubuntu 24.04 [M]) and from the same package
version in CI; the backend ships a minimum version and `sandbox-check` asserts it. The runtime path is
the flag `--oci-runtime`, default `/usr/bin/crun`; the AppArmor profile that allows `userns` is attached
to that path, so a copy elsewhere loses it, and `sandbox-check` says so. Swapping to `runc` or `youki` is
the flag plus the same conformance suite (9). The results of E1, E2, E2b and the loopback behaviour are
recorded **with the crun version** beside each, since all are version-dependent. Whether the hosted
runner image ships `crun` is checked in step 0; if not, installing it is an explicit `runner_setup` step.

## 7. Latency, and what it does to 21 s + 16 s

Anchors:

- `crun` full create/start/delete of a trivial container: 47 ms (youki README bench, kernel 6.5, 16
  cores) [P]; about 17 ms each over 100 sequential runs (`crun` README, machine unstated) [P]. The first
  bench drops caches on each run and does not appear to be rootless; neither covers overlay mounts, the
  `newuidmap` double exec, netns teardown or cgroup removal. They are anchors, not predictions.
- One `unlinkat` of a hard link: 17 us; a 15k-link `remove_tree` 0.26 s idle [M].
- One `podman` invocation: 0.10-0.25 s idle, 4-7 s at 80+ open leases [M].

Expected per action, idle, excluding the payload [I] (a hope until E2 runs):

| Phase | Expected |
|---|---|
| spawn `crun`, `newuidmap`, `newgidmap` (3 execs) | 3-10 ms |
| `/kbf/root` overlay + up to 8 tree binds | 2-10 ms |
| bundle write, per-action tree (hundreds of links), chown of its directories | 5-15 ms |
| `crun` create/start (netns, cgroup, `/proc`, seccomp, pivot_root) | 20-60 ms |
| teardown: cgroup kill+rmdir, tool bundle (reown `upper`, remove), remove per-action tree | 60-120 ms |
| **Total** | **90-210 ms**, versus about 2 s now; the real number may land in the low hundreds |

**Under 60-80 concurrent actions nothing is published and I promise no number.** The data do **not**
show the inflation is podman's and not the kernel's (section 1). What this design removes with certainty
is podman's database, storage locks and process starts. The remaining shared kernel state, which the
draft's first version understated, and which E3 times separately at 8 and 64 concurrent:

- `namespace_unlock()` calls `synchronize_rcu_expedited()` after dropping `namespace_sem` whenever
  anything was unmounted; every sandbox teardown unmounts and mount setup takes `namespace_sem` for write,
  so at 64 concurrent it is a global serialization point [P, 6.8 `fs/namespace.c`, from review reading;
  confirm by timing];
- network namespace teardown is asynchronous on a single `netns` workqueue (`cleanup_net` takes
  `pernet_ops_rwsem` and calls `synchronize_rcu()`) [P, 6.8 `net/core/net_namespace.c`; same caveat],
  so netns creation/teardown rates are a possible ceiling;
- `cgroup_mutex` at mkdir/rmdir, and the count of dying cgroups (they slow `memory.stat` reads);
- the ext4 journal: the link, unlink and delete work, the action's own fsyncs (compilers fsync), the
  `discard` mount option (202M discard ops, about 0.9 ms each [M]) and `rootfs-upper` all share one SATA
  SSD and one journal.

Mitigations considered and measured, not assumed: `volatile` overlays (E2/E3); fixed-interval `fstrim`
against `discard` (E5); `rootfs-upper` on a size-capped tmpfs or separate volatile filesystem (small
`/tmp`-style writes; section 12); fewer mounts; batched deletes.

Pass criterion for the stage: p50 setup+teardown at 64 concurrent under 1 s and flat from 8 to 64
within a factor of 3; a miss names the kernel resource in E3.

Against the measured costs: the **16 s of linking** becomes about 0 for tree-covered files plus the
first-sight preparation (4.2), and the bulk linker (E8) may remove most of it before trees are used; the
**21 s** becomes the table's figure only if the inflation was podman's (E0/E1 decide). Unchanged: server
round trips, Directory fetches the C4a cache does not cover, output hashing and upload (about 0.4 s per
lease in the A8 run [M]), and the payload.

## 8. Concurrency and recovery

- Tree hit: one `stat` of `trees/<digest>`, an atomic increment. Miss: the per-digest single flight. No
  global lock; a map shard lock is held for the lookup only.
- Image hit: a `stat` of `images/<digest>/rootfs`. Miss: single flight, off the lease's timeout clock.
- Per-action directories are private to the lease. `crun --root` is per lease. No database.
- No kbf process other than the daemon is on the path: `crun` is a child, so a blocking mount or
  `newuidmap` wait stalls no tokio worker as long as the call runs in `spawn_blocking` (or the async
  client). The daemon's synchronous `cgroup.create` and keeper read on async workers is a separate
  finding and is fixed in the new backend the same way.
- `--max-open-leases` stays as an admission bound; the knee-capping stopgap is not part of this design.

**Orphan cleanup needs a namespace.** Per-lease directories hold files owned by real subordinate ids
(the files an action wrote into `upper` and `/tmp`-like directories), which kbf cannot delete from
outside a user namespace. After a SIGKILL of the daemon the container may be gone too. The start-up
sweep runs the tool bundle (3.0) in remove mode for every leftover lease (the same `crun` call as the
post-run reown); a conformance test runs it on a lease left by a killed daemon. Image-store eviction
(files owned by subordinate ids after unpack) uses the same bundle.

## 9. Fallbacks and what is not chosen

- **F1, podman with shared trees** (no new runtime): `--volume trees/<digest>:/kbf/root/<path>:ro` per
  tree. Same store, classifier and tests. Ships first and proves the layout; keeps the nine podman
  processes. Together with the per-node `--container-runtime` flag it is the stated rollback.
- **F2, an in-process Rust runtime** (own `clone3`/`pivot_root`, `seccompiler`, caps, masked paths):
  **not planned.** It would be a second runtime to audit against every kernel fix and every runc/crun
  escape CVE, and kbf would own the whole hardening list (risk 6). It is a last resort only if E2/E3
  show `crun`'s own floor exceeds the budget, and it would need a named maintainer and a cost estimate
  before anyone starts.
- **F3, a kbf launcher that unshares itself** (revision 2's primary): dropped by the maintainers'
  decision. It needs its own AppArmor `userns,` profile and a setup-namespace id-map handshake that
  `crun` already implements.
- **Not chosen:** the Podman REST API (saves process starts, not podman's locks, database, `conmon` or
  storage layer); bubblewrap/nsjail (no OCI image or cgroup story; one more binary); gVisor (syscall-
  heavy builds pay); composefs/EROFS and idmapped mounts (need root on 6.8, section 5.1); microVMs as the
  default (section 10); `libcrun`/`libcontainer` (6.3).
- **Conformance cross-check:** one CI job runs the same generated bundles under `runc`, to separate "our
  `config.json` is wrong" from "crun behaves differently" and avoid a crun-only bundle.

## 10. Coexistence

- **macOS native driver:** untouched. The classifier and tree store live in `kbf-daemon`, so the Mac
  driver can use the same decision with its own mechanism (`clonefile` of `dirs/<digest>`, C4c in
  `worker-cache.md`).
- **VM driver (draft, PR #392):** a stronger-isolation tier, not a rival. Its `VmHost` trait takes a
  read-only inputs share; for a Linux microVM host the same classifier would give one read-only block
  image per tree digest. Firecracker has no virtio-fs, so a block image is the form [P]. It is not on this
  plan's path.
- **Scheduler and node report:** the driver keeps `KIND`/`DRIVER`; the node report adds `runtime =
  podman | sandbox`, the tree store's size and hit rate, and the uncovered-files count (4.1).

## 11. Migration, tests, measurement

### 11.1 Steps (each reversible by a flag, each mergeable alone)

| # | Change | Flag | Rollback |
|---|---|---|---|
| 0 | Gates E0, E1, E2, E2b, E8, E9, plus `crun spec`, the `runc` crate checks of 6.3 and runner-image checks; per-verb timings in `Podman::output`; `make_directories` vs links in the `inputs:` line | none | n/a |
| 1 | Directory cache (C4a) + bulk linker + tree store + classifier + pre-warm, behind podman `:ro` volumes (F1) | `--shared-trees=off\|on` | off |
| 2 | OCI backend in `kbf-driver-container` (`oci-spec` bundle writer, vendored hardening, `runc` crate client over `crun`, tool bundle for reown and reap, image store 2a), **together with the backend trait** (create, run, wait, kill, collect, reap), named from the conformance suite below; both implementations land in the same PR. Consumes the tree store through the narrow "shared read-only mounts" interface (a list of host directory, container path, read-only) that step 1 defines. | `--container-runtime=podman\|oci`, `--oci-runtime=<path>` | per node |
| 3 | Pure-Rust image pull and unpack (2b) in the tool bundle; remove the last podman dependency | same | per node |

The trait is not landed as a standalone refactor (a trait with one implementation is a concrete type),
and step 2 does not move the 7,000-line crate: both backends live in `kbf-driver-container`'s lease
code first, reusing its `cgroup`, `delegate`, `subids`, `outputs`, `remove`, `cas` modules; a separate
`kbf-driver-oci` crate is extracted only if the module crosses the file-size limit or after step 3.
**Step 2 starts only when E0/E1 have named the cause of the inflation and E2 and E2b have passed.**

### 11.2 Conformance

The same lease-level tests run against both runtimes through one test body parameterised by the runtime
constructor (today's real-podman tests in `runtime_tests.rs` become the shared suite). Each row names the
defect it catches and the mutant to plant:

| Test | Defect / planted mutant |
|---|---|
| exit code, signal, timeout (stdout/stderr kept), OOM at lease cap vs backstop; runtime failure vs action exit 1 | wrong status mapping from `crun` |
| writes outside `/kbf/root` do not appear in outputs; outputs only from `upper` | output leak from the rootfs layer |
| write, chmod, rename, unlink, `link`, `mount -o remount,rw` (as container root with `CAP_SYS_ADMIN` in own ns) into a shared tree each fail; tree digest unchanged | mutants: drop `MOUNT_ATTR_RDONLY`; chown the tree |
| cached inode owner and mode unchanged after a lease; no cached file is group/other-writable; `trees/` and parent are 0700 | mutant: chown following files; mode 0644 entry |
| **hardening, read from inside:** `Seccomp:2`, `NoNewPrivs:1`, `CapEff` equals the pinned mask; `/proc/kcore`, `/proc/sys`, `/sys/firmware` masked or read-only; `unshare`, `mount`, `bpf`, `keyctl` return EPERM/ENOSYS; `perf_event_open`, `userfaultfd` blocked; the `/dev` list matches | mutants: seccomp section removed; add `CAP_SYS_ADMIN` |
| `/proc/self/fd` lists only 0, 1, 2 | mutant: fd without CLOEXEC |
| container ids: `id` and `uid_map` show no id of kbf; a file kbf owns is `nobody` | mapping regressed to include kbf |
| inside, write `memory.max`/`pids.max` and expect EACCES/EROFS; `/sys/fs/cgroup` read-only | container raises its own limits |
| a mode-4755 file and a `security.capability` xattr created by the action reach the output with neither; mode-000 dir, symlink to a host path, sparse file handled | reown/collector gaps |
| overlay: `mountinfo` shows the pinned options and `nosuid,nodev`; planted metacopy/redirect xattr in `upper` is not followed | option defaults |
| tree mount survives `crun`'s rootfs bind and private remount and shows `ro` with locked flags in `mountinfo`; lowers that are mounts stack to the depth we use (the 6.8 limit is run, not listed) | nested-mount differences |
| an output path under a would-be tree disqualifies it; one differing file per action builds no tree | tree churn; outputs in a tree |
| two actions share a tree concurrently; eviction of a tree in use is refused | refcount / rename-delete order |
| daemon killed mid-action: keeper kills the cgroup; next start's tool-bundle sweep clears the lease; **`crun` SIGKILLed alone: the container still dies** | orphaned sandbox |
| env contains only the Command's variables; fresh netns has loopback only and no abstract-socket path to the host | environment or network leak |
| node-start preflight (`sandbox-check`) fails clearly on each missing prerequisite | opaque failure on a new host |
| `id`/`ls -ln /` of the base image in the unpacked rootfs (2b); unpacker refuses traversal, device nodes, outside hardlinks | silent permission bug; tar attack |

Unit tests assert `config.json` (built through `oci-spec`) against the same table as the argv tests in
`podman.rs` and against the golden file generated from the vendored hardening inputs, so a dropped limit,
flag or seccomp section is a red test. Backend mutants: drop the read-only flag from a tree mount; chown
`upper` without `O_NOFOLLOW` (a symlink planted in `upper` must not escape); skip the reown (output
hashing must fail); map a container id to kbf's uid in the action bundle; run the tool bundle before the
cgroup is empty.

**CI.** The existing hosted job (`podman-x86`/`podman-arm`, `cargo xtask podman-tests` in a
`Delegate=yes` unit) works because Ubuntu ships profiles allowing `userns` for `podman` and `crun`. The
OCI backend execs `/usr/bin/crun`, so it runs under that same shipped profile and `runner_setup` needs no
profile and no sysctl change; this is checked on `ubuntu-24.04` with
`kernel.apparmor_restrict_unprivileged_userns=1` in step 0 (the symptom of a missing grant is an `EPERM`
on the `uid_map` write, actions/runner-images#10443). The arm job is verified separately (`newuidmap`
and `/etc/subuid` setup may differ). `sandbox-check` is a library of functions each returning a typed
`Missing(...)`, runnable without root and without podman, so it is also the first CI step and fails there
with the message a node would give; it names the AppArmor sysctl and the `crun` path when the maps fail.

### 11.3 Measurement plan (a probe binary in `kbf-bench`, run on a test node only with the maintainers' explicit go)

The probe runs as user kbf. It reports per-phase histograms (spawn, ns setup, mounts, `crun`, payload,
teardown) at 1, 8, 32 and 64 concurrent, for payloads: `true`; read 1,000 files of a 15k-file tree;
write 200 MB; fork 500. **Gates (before step 2): E0, E1, E2, E2b.** The `crun` floor on our hardware is
run by the held `bench_runtimes.sh` already staged for the maintainers on a build host; nothing here is
executed on a node by an agent.

- **E0** raw `fsync` and `linkat`/`unlinkat` loops, 1/8/64 concurrent, on the node's filesystem, no
  podman. Says whether the journal alone reproduces the inflation.
- **E1** `strace -f -tt -c` / `perf trace -s` of one idle lease and one at 60 concurrent on today's
  driver, **including the podman lock trace** (which lock, held by whom): time per phase and the lock
  waited on. Settles the section 1 attribution.
- **E2** a hand-written bundle run by `crun` as user kbf, no tree: the runtime floor (compare the 47 ms
  anchor [P]), whether it brings loopback up, `--cgroup-manager=cgroupfs` against the no-internal-process
  rule for the lease cgroup, and that it runs under the distribution's `crun` AppArmor profile with
  `kernel.apparmor_restrict_unprivileged_userns=1` and no profile of ours (the `/proc/self/attr/current`
  of the action is recorded: what confines it besides seccomp and capabilities). Record crun's version.
- **E2b** the rootless mapping and mount probes, each a yes/no: (a) `crun` as kbf accepts a bundle
  mapping container `0..65534` to subuid host ids and starts (newuidmap path); (b) the tool bundle with
  the mapping `0 -> kbf, 1.. -> subuid` can chown and remove files the action created; (c) `crun` mounts
  the `/kbf/root` overlay from `mounts` with `userxattr` in its own user namespace, with a kbf-owned
  `0777` upper and work; (d) a read-only bind of a tree stays read-only against `remount,rw` from inside;
  (e) the same with the image rootfs as `/` read-only (case A, 3.2). The answers pick case A or B (3.2).
  If (a) or (b) fails the primary path stops.
- **E3** E2 + the tree binds, + the nine-tree case, at 1..64 concurrent; per-phase timing of
  `unshare(NEWNET)`, mount and umount at 8 and 64, the dying-cgroup count, `volatile` overlays on/off,
  and the tool bundle's added floor.
- **E4** ownership of upper/work: `0777` kbf-owned vs a pre-run chown; owner and mode of the upper's
  files; whether kbf can read them without the tool bundle.
- **E5** delete/unmount cost with and without `discard` (a test-node mount change) and with fixed-
  interval `fstrim`; `volatile` on the overlay.
- **E6** a corpus run of the real toolchain targets with trees mounted and the read-only rootfs (case A):
  every `EROFS`/`EXDEV` is a finding for D1 and D5.
- **E8** the bulk linker against the current pipeline, idle and at 64 concurrent; then trees against the
  bulk linker at 64, which is where shared-inode contention (the case for trees) shows or does not.
- **E9** offline, no node access: from the A8 journal and the CAS input roots, the distribution of input
  files per action, the share inside qualifying nodes, the share of actions with fewer than 1,000
  uncovered files, how often an output under a tree disqualifies one, and how many digests recur.
  **Gate for step 1.**
- **E10** the first minutes after a deploy (cold trees, 80 first-wave actions) against today.

Removed as moot: the AppArmor-profile experiment for a kbf launcher (old E7), and the launcher-handshake
timings. Kill criteria: E2 slower than 300 ms idle, E3 flat-lining above 1 s at 64 concurrent, or E2b (a)
or (b) failing, stops the primary path and leaves F1 shipped.

## 12. Rootless requirements and security posture

Ubuntu 24.04 nodes (kernel 6.8, `crun` 1.14, `uidmap`, cgroup v2) [M]:

- `kernel.apparmor_restrict_unprivileged_userns=1` is on [M]. kbf ships **no** AppArmor profile: user
  namespaces are created only by `/usr/bin/crun`, which the distribution's profile allows; the daemon
  has none. The restriction is not a hard boundary (published bypasses exist [P, Qualys 2025]); keeping
  the grant on `crun` alone is as narrow as it can be. A host without that profile (a `crun` at another
  path, another distribution) is a `sandbox-check` failure, not a reason to turn the sysctl off.
- `newuidmap`/`newgidmap` setuid, `/etc/subuid`/`/etc/subgid` with at least 65,536 ids for kbf, **and no
  overlap with any other user's range** (`subids.rs` checks size, not overlap; `sandbox-check` adds the
  overlap check); `user.max_user_namespaces` above zero.
- cgroup v2 with `Delegate=yes` for the unit and `cpu memory pids io` delegated (`delegate.rs`). The
  lease cgroup files stay kbf-owned, which is what keeps the container from raising its own limits
  (`DAC_OVERRIDE` does not apply to an unmapped owner); cgroupns is private.
- Unprivileged overlayfs (kernel 5.11 or newer [P], 6.8 here) with `userxattr`. **The cache, `trees/`,
  `lease/` and the image store must be on one filesystem**, because hard links require it (`images/`
  too, so that unpack and eviction stay on the same device as the lower mounts); ext4 or XFS with
  `ftype=1`, **not** `noexec`, `nosuid` or `nodev`. The node's single SATA SSD carries journal, discard
  and rootfs-upper traffic together; `sandbox-check` records the device layout of each host and flags
  this, and a separate small volatile filesystem for the writable tmpfs-like places is considered (E5).
- **Kernel floor.** Overlayfs and user namespaces have a long CVE history (copy-up setuid issues, the
  2023 Ubuntu overlayfs pair; confirm numbers against NVD before citing, [U]). Mounting overlay in a
  an overlay mount in a user namespace is not new exposure (podman does it too), but `sandbox-check` refuses a kernel below a pinned
  patched floor, and `nosuid` is the default (5.3).
- Per new host: `kbf-node sandbox-check` (user, sysctl, subid ranges and overlap, `newuidmap` bits,
  cgroup delegation, a trial overlay mount in a trial namespace, filesystem flags and device, `crun`
  version, kernel floor) before it takes sandbox leases. What those hosts have is not known to me; this
  list is what to verify.

**Unsafe code.** The backend adds no `unsafe` of its own: namespaces and mounts are `crun`'s, bundle and
config are safe Rust (`oci-spec`), and the only descriptor-level code is the reown walker that already
exists (`chown_dirs.rs`, `outputs.rs`), which runs inside the tool bundle. `rustix` stays the choice
if a syscall is needed (for example `close_range`, `pidfd_open` on `crun`).

## 13. Risks

1. **The inflation is not podman's.** If E0 reproduces it without podman, or E3 stays slow at 64, the
   kernel (mount-namespace RCU, netns teardown, `cgroup_mutex`, the journal) is implicated. Mitigation:
   the gates in 11.3 come first; the fix is fewer mounts, batching, `volatile`, a separate filesystem.
2. **Write-in-tree behaviour (D1).** Toolchains that write next to themselves fail `EROFS`. Mitigation:
   E6 before the flip; per-digest fallback to the per-action tree.
3. **Cross-mount tools.** `rename`/`link` across a tree boundary give `EXDEV`; most tools fall back to
   copy, some do not.
4. **Ownership model.** The security argument rests on kbf being unmapped in the container. Every change
   to the mappings restates it; the tests plant the mutant.
5. **The tool bundle is new privileged-ish code** (root-of-kbf inside a namespace, chown by descriptor).
   Small, runs only after the cgroup is empty, mounts only the lease's `upper`/`work`; reviewed like
   `chown_dirs.rs`; spec parsing and the unpacker fuzzed. There is no longer a kbf program that
   creates namespaces.
6. **Hardening is kbf's to own once podman is gone.** The vendored seccomp/caps/paths are a supply-chain
   input: pinned by digest, updated deliberately, the update is a reviewed diff of the golden file.
   Bazel's `linux-sandbox` source is a checklist [P].
7. **Fresh sandbox per action excludes warm tricks:** no namespace or netns pools, no prestarted
   containers. What may be prepared ahead are inputs (trees, images), not sandboxes.
8. **Tree store and cache coupling:** trees pin cache entries; the budgets in 4.3 are tested against the
   cache's LRU so neither starves the other.
9. **Kernel floor:** designed for 6.8; no 6.13 fd-layers or 6.15 features. Test on 6.8, not a newer dev box.
10. **Image extract ownership and parsing (2b):** a wrong id shift is a silent permission bug; a hostile
    tar is a setup-ns-root exploit. Covered by the rows in 11.2 and 6.2.
11. **Classifier coverage unknown until E9.** If few actions qualify, the benefit shrinks to the bulk
    linker's.

## 14. Decisions

Decided by the maintainers (recorded, not open):

- **D1. Yes.** Shared trees are read-only (`EROFS` on write), mounted into actions instead of linking
  about 15k files per action; per-digest fallback to the per-action tree where E6 finds a toolchain that
  writes next to itself.
- **D2. Moot.** No kbf binary creates namespaces, so no kbf AppArmor profile exists to decide on.
  If E2b shows `crun` cannot mount the rootless overlay, the fallback is 3.2 case B (read-only trees,
  plain read-only rootfs directory), still with no profile.
- **D3. Rootless only, no root component.** Software must never require root to build: no setuid helper
  of ours, no root service, no idmapped mounts, EROFS or jailer.
- **D4. Fresh sandbox per action**, no reuse and no pre-started containers (a Lambda-style host cannot
  hold one).
- **D5. Runtime.** `crun` as the default, driven by the `runc` crate with configs from `oci-spec`; the
  runtime binary is a flag so `runc` or `youki` can be swapped. BuildBuddy's code is not used (license).

Still open:

- **Q1.** Authorise the gate experiments (E0, E1, E2, E2b) on a test node, via the held bench script.
  Nothing past step 1 starts without their results.
- **Q2.** Case A of 3.2 makes `/` read-only outside tmpfs mounts. Acceptable if E6 finds no real
  toolchain needing otherwise?

## 15. Review ledger

Three reviews of revision 1 (security, adversarial, Rust implementation). Every must-fix was accepted;
the items declined or changed in kind are listed with reasons.

| Review item | Disposition |
|---|---|
| Sec M1 / Rust M1: podman, not crun, supplies seccomp, caps, masked paths | Accepted. Section 6.1 vendors and pins them; 11.2 reads them from inside; mutants named. |
| Sec M2: chown directories only | Accepted (5.2), reuse `chown_dirs`, owner-unchanged test. |
| Sec M3: barrier 4 is not a barrier | Accepted: two barriers, 0444/0555 already enforced by the cache [M], sampled re-hash for detection (4.4). |
| Sec M4: leaked fds | Accepted (6.1). |
| Sec M5: reown order, suid, attacker-shaped outputs | Accepted (5.2). |
| Sec M6: pin overlay options | Accepted (5.3). |
| Sec S1-S7 | Accepted: kernel floor and `nosuid` (12), AppArmor on the launcher and which profile the action runs under (3, E7), keeper kept external (3), tar-parser treatment (6.2), podman's lost defaults enumerated (6.1), netns/abstract sockets (11.2), 0777 trick demoted (5.2). The CVE numbers are not cited because they were not verified. |
| Sec questions | Q1 is a test row (4.4); Q2 added to `sandbox-check` (12). |
| Adv M1: inflation not attributed | Accepted: section 1 and 7 reworded; E0/E1 are gates. |
| Adv M2: bulk linker ablation | Accepted: E8, and tree builds use the bulk linker (4.2). |
| Adv M3: "highest node" makes unique trees | Accepted: recurrence rule, byte-rate cap, differing-file test (4.1). |
| Adv M4: coverage unknown | Accepted: E9 is the gate for step 1; aggregate uncovered-files cap. |
| Adv M5: journal contention, `volatile`, `fstrim` | Accepted: candidates in E2/E3/E5, not last. |
| Adv M6: shared-kernel-state list | Accepted (7, E3, `rprivate` in 3.0). Kernel-source claims are tagged as review reading to confirm by timing. |
| Adv M7: AppArmor is a gate | Accepted: E7 moved first. |
| Adv M8: `--tree-mode=overlay` with 0777 lowers | **Changed.** Rejected as written: a world-writable directory in a shared lower contradicts the immutability argument. The escape hatch is per-digest fallback to a per-action tree. |
| Adv S1-S8 | Accepted: anchors reworded, filesystem layout and one-device rule, page-cache paragraph, inode-level weight accounting, pre-warm in step 1, E9 counts disqualification, ownership/EACCES note, dying cgroups in E3. OVL_MAX_STACK stays a CI-run fact, not cited. |
| Rust runtime choice | Agreed (CLI exec from a single-threaded launcher). The speed argument against `libcontainer` is dropped; reasons are in 6.3. |
| Rust M2: orphan cleanup needs a namespace | Accepted: `kbf-sandbox reap` (8). |
| Rust M3: id-map handshake | Accepted: helper-fork design chosen over daemon-side (3.0). |
| Rust M4: CI AppArmor | Accepted (11.2 CI paragraph). Not yet verified on the arm runner. |
| Rust M5: crun provisioning | Accepted (6.4). |
| Rust M6: trait with one implementation | Accepted: trait lands with the second implementation (11.1). |
| Rust S1-S7 | Accepted: exit contract and error enum (6.1), `rustix`/unsafe module (12), nested-mount tests, launcher mutants (11.2), F2 "not planned" (9), `runc` cross-check job (9), `sandbox-check` as typed library (11.2). |

**Revision 3** (maintainer decisions, section 14). The launcher, its id-map handshake, the AppArmor
`userns,` profile, `kbf-sandbox reap`, the `MS_PRIVATE`/`open_tree` attach and the launcher fd-leak surface
are removed; `crun` creates every namespace (3, 3.0) and the tool bundle replaces the launcher's ownership
and reap jobs. Rust M3 (handshake), Rust M4 (CI AppArmor profile) and Sec S2 (profile on the launcher) are
therefore moot, not answered; Sec M4 (leaked fds) narrows to `crun`'s own behaviour and the `/proc/self/fd`
row stays. The review dispositions above that name the launcher are kept as history. New unverified items,
each an experiment or a step 0 check: that `crun` as an unprivileged user accepts subuid host ids in
`uidMappings` (E2b a); that the tool bundle can chown and remove (E2b b); that `crun` mounts the overlay
in its own namespace (E2b c); that the `runc` crate's `run` returns the exit status and works with
`crun` (6.3); the CVE and advisory numbers named as [U].

Also not verified, each an experiment above: what `crun spec` emits; whether it brings loopback up; the
6.8 overlay stack-depth limit; which AppArmor profile the action runs under; whether the hosted runner
image ships `crun`.

## Sources

Fetched 2026-10-14 unless noted.

- crun README (benchmark, `libcrun`): https://github.com/containers/crun
- youki README (47/111/224 ms bench): https://github.com/youki-dev/youki; `libcontainer`: https://docs.rs/libcontainer/latest/libcontainer/
- Red Hat, introducing crun: https://www.redhat.com/en/blog/introduction-crun
- Podman `run` (`--rootfs :O`, `idmap` is rootful-only): https://docs.podman.io/en/latest/markdown/podman-run.1.html; seccomp policy: https://docs.podman.io/en/v4.6.0/markdown/options/seccomp-policy.html
- Default seccomp profile is containers-common `seccomp.json` (copied by a downstream project): https://git.notmyidea.org/almet/dangerzone/commit/3e434d08d1443c118ead809a4150ff1f3c73173e
- Kernel overlayfs (unprivileged mounts, `userxattr`, `metacopy`, `lowerdir+`): https://docs.kernel.org/filesystems/overlayfs.html; 6.8 `fs/overlayfs/super.c`, `fs/erofs/super.c`, `fs/namespace.c`, `net/core/net_namespace.c`: https://github.com/torvalds/linux/tree/v6.8
- `mount_namespaces(7)`, `user_namespaces(7)`, `open_tree(2)`, `mount_setattr(2)`, `openat2(2)`: https://man7.org/linux/man-pages/
- Ubuntu AppArmor userns restriction: https://discourse.ubuntu.com/t/understanding-apparmor-user-namespace-restriction/58007; bypasses (2025): https://www.qualys.com/2025/three-bypasses-of-Ubuntu-unprivileged-user-namespace-restrictions.txt
- Hosted-runner failure for unprivileged user namespaces: https://github.com/actions/runner-images/issues/10443
- Bazel sandboxing and `linux-sandbox`: https://bazel.build/docs/sandboxing
- BuildBuddy `ociruntime` (crun directly, layers as overlay lowers): https://github.com/buildbuddy-io/buildbuddy/blob/master/enterprise/server/remote_execution/containers/ociruntime/ociruntime.go
- Buildbarn: https://github.com/buildbarn/bb-remote-execution
- Firecracker spec and Kata design (no virtio-fs): https://github.com/firecracker-microvm/firecracker/blob/main/SPECIFICATION.md; https://github.com/kata-containers/kata-containers/blob/main/docs/design/virtualization.md
- composefs: https://github.com/containers/composefs
- kbf code read: `kbf-driver-container/src/{lib,podman,runtime,tree,chown_dirs}.rs`, `kbf-daemon/src/{runtime,tree}.rs`, `kbf-daemon/src/cache/{layout,lease,verify}.rs`, `docs/design/worker-cache.md` 5.1 and 6.6; the per-step attribution of a lease from the overhead investigation.
- `runc` crate (containerd/rust-extensions; `GlobalOpts::command`, `custom_spawner`, `CreateOpts`): https://docs.rs/runc/latest/runc/ ; https://github.com/containerd/rust-extensions
- `oci-spec` crate: https://docs.rs/oci-spec/latest/oci_spec/
- BuildBuddy Enterprise license (the executor under `enterprise/` is not open source): https://github.com/buildbuddy-io/buildbuddy/blob/master/enterprise/LICENSE
