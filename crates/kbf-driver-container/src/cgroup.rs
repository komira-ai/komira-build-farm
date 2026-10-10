//! The lease cgroup, `<actions>/kbf-lease-<term>-<seq>` (RFC 10.6).
//!
//! The daemon owns an `actions/` cgroup in its delegated subtree (`crate::delegate`).
//! For each lease the driver makes a child with the lease's limits and puts the
//! container under it (`--cgroup-parent`). The lease cgroup outlives the container, so
//! after the action ends its `memory.events` still says whether the kernel OOM killer
//! ran inside it, and whose limit made it run: Podman's own `OOMKilled` flag is not
//! trusted (it reads false rootless; see the hosted-runner spike).
//!
//! Memory (the build-host policy): a hard cap per lease, `memory.max` = booking x 1.5 +
//! 512 MiB (the native driver's limit), with `memory.oom.group=1` on the lease, so an
//! OOM kill takes every process in the lease and none outside it. No `memory.high` and
//! no `memory.swap.max`: swap stays allowed, so reclaim under host pressure moves a
//! lease's pages to swap rather than killing it. At its own cap a lease is reclaimed
//! into swap too, and the kernel kills it only once swap is full; so the driver
//! watches each lease ([`SwapKill`]) and kills one that is pressing its own cap into
//! swap and has itself pushed more than a threshold there at its cap (swap that host
//! pressure put there does not count). The node's backstop is `memory.max` on
//! `actions/`, which the daemon writes from `--actions-memory-max-gib` (or the
//! operator sets). CPU is compressible: `cpu.weight` from the booked CPU, never
//! `cpu.max`.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_daemon::RuntimeError;
use kbf_types::Resources;

/// Controllers the lease hands to the container's cgroup.
const SUBTREE: &str = "+cpu +memory +pids";
/// 512 MiB, the cap's headroom above 1.5 x the booking.
const HEADROOM: u64 = 512 << 20;
/// How often, and how long apart, removal retries a lease cgroup that is still busy:
/// a process still exiting, or Podman's exit hook (`podman container cleanup`, which
/// conmon starts inside the lease) still running.
const REMOVE_TRIES: u32 = 100;
const REMOVE_PAUSE: Duration = Duration::from_millis(50);

/// The hard memory cap (`memory.max`) for a lease that booked `memory_bytes`: 150% of
/// the booking plus 512 MiB, the native driver's limit. `None` (no cap) when nothing
/// was booked, as there.
#[must_use]
pub fn memory_max(memory_bytes: u64) -> Option<u64> {
    (memory_bytes > 0).then(|| {
        memory_bytes
            .saturating_add(memory_bytes / 2)
            .saturating_add(HEADROOM)
    })
}

/// The `cpu.weight` for a lease that booked `cpu_millis`: one core is the kernel's
/// default weight, 100. `None` (the default) when nothing was booked.
#[must_use]
pub fn cpu_weight(cpu_millis: u64) -> Option<u64> {
    (cpu_millis > 0).then(|| (cpu_millis / 10).clamp(1, 10_000))
}

/// When the driver kills a lease that keeps pushing into swap at its own cap.
///
/// Every `every`, the driver reads the lease cgroup's `memory.events` `max` (times
/// usage reached the lease's `memory.max`) and `memory.swap.current`, and splits what
/// the lease holds in swap into what it pushed there itself at its cap and the rest
/// ([`SwapWatch`]): a rise in swap between two samples across which `max` grew is the
/// lease's own (reclaim at its cap moved it there); a rise across which `max` did not
/// grow is not (host pressure, or the `actions/` limit, moved it there while the lease
/// was below its cap); a fall comes out of the lease's own share first. It kills the
/// lease (`cgroup.kill`, every process in it) when, all three at once, `max` grew
/// since the previous sample, `memory.swap.current` rose since the previous sample,
/// and the lease's own share is above [`SwapKill::threshold`]: the lease is at its
/// own cap now, reclaim at that cap is still moving it into swap, and what it put
/// there itself is past the margin. The kill is reported as the lease's own
/// out-of-memory kill.
///
/// So a lease that host pressure swapped out is never killed for that swap: not while
/// below its cap (no `max` events), and not when it later reaches its cap and pushes
/// a few pages more (only those pages count). Nor is a lease that booked no memory
/// (it has no cap). The previous sample is the last one read: a sample that cannot be
/// read is skipped and changes nothing. The first sample is compared with zeros,
/// which is what a new lease cgroup counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapKill {
    /// The least swap a lease at its cap may hold before it is killed, in bytes.
    pub floor_bytes: u64,
    /// The swap a lease at its cap may hold, as a percentage of its booked memory, when
    /// that is more than `floor_bytes`.
    pub percent: u64,
    /// How often each lease is sampled.
    pub every: Duration,
}

impl SwapKill {
    /// 512 MiB or 25 % of the booking, whichever is more, sampled every second. The
    /// floor keeps a small action that brushes its cap (its cap already carries
    /// 512 MiB of headroom above 1.5 x the booking) from being killed for a few cold
    /// pages; the percentage scales it for large bookings, where 512 MiB of swap is a
    /// small share of the working set. A second is short against a build step and long
    /// against the cost of two file reads per lease.
    pub const DEFAULT: Self = Self {
        floor_bytes: 512 << 20,
        percent: 25,
        every: Duration::from_secs(1),
    };

    /// The swap above which a lease that booked `memory_bytes` and is at its cap is
    /// killed: the larger of the floor and the percentage of the booking.
    #[must_use]
    pub fn threshold(&self, memory_bytes: u64) -> u64 {
        let share = u64::try_from(u128::from(memory_bytes) * u128::from(self.percent) / 100)
            .unwrap_or(u64::MAX);
        share.max(self.floor_bytes)
    }
}

/// One sample of a lease cgroup for [`SwapKill`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pressure {
    /// `memory.events` `max`.
    pub max: u64,
    /// `memory.current`: what the lease holds in RAM.
    pub current: u64,
    /// `memory.swap.current`: what the lease holds in swap.
    pub swap: u64,
}

/// What [`SwapKill`] has seen of one lease: its last sample, and how much of the swap
/// it held then it did not push there itself at its cap. A pure state machine over
/// samples; [`presses_into_swap`] feeds it.
///
/// Both counters are the lease cgroup's, which take in the container's cgroup below
/// it: `memory.swap.current` is the swap of every process in the lease, and
/// `memory.events` `max` counts reclaim at the lease's own `memory.max` (and at any
/// limit below it, which the driver does not set). A limit above the lease, the
/// `actions/` backstop or the host, counts no `max` event in the lease, so swap it
/// moves is never the lease's own.
///
/// The split is cumulative over the lease's life, not reset when its `max` events
/// pause: a lease that presses its cap into swap in bursts, with quiet samples between
/// them, is judged on all it pushed. A fall in swap (pages read back, or a process
/// exiting) is taken from the lease's own share first, so pages read back at the cap
/// and pushed out again (churn) never add up past what the lease holds; only once
/// its own share is 0 does a fall reduce the rest. Swap that rose across a sample in
/// which `max` also grew is all the lease's own, even if host pressure moved some of
/// it in the same interval: the lease was at its cap then.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SwapWatch {
    /// The swap the lease may push itself at its cap: [`SwapKill::threshold`].
    threshold: u64,
    /// The last sample read: zeros, a new lease cgroup's counts, before the first.
    last: Pressure,
    /// Of `last.swap`, what the lease did not push there at its cap.
    not_own: u64,
}

impl SwapWatch {
    /// The watch of a new lease, killed past `threshold` bytes of its own swap.
    pub(crate) fn new(threshold: u64) -> Self {
        Self {
            threshold,
            last: Pressure::default(),
            not_own: 0,
        }
    }

    /// Takes the sample `now` and says whether [`SwapKill`] kills the lease: what it
    /// pushed into swap itself at its cap ([`SwapWatch::own`]) is now above the
    /// threshold. That share grows only across a sample in which `max` grew and swap
    /// rose, so the kill comes only at such a sample: the lease is pressing its cap
    /// into swap now.
    pub(crate) fn kills(&mut self, now: Pressure) -> bool {
        let pressed = now.max > self.last.max;
        if now.swap > self.last.swap && !pressed {
            self.not_own = self.not_own.saturating_add(now.swap - self.last.swap);
        }
        // A fall comes out of the lease's own share first: the rest is what is left.
        self.not_own = self.not_own.min(now.swap);
        self.last = now;
        self.own() > self.threshold
    }

    /// What the lease holds in swap now that it pushed there itself, at its cap.
    pub(crate) fn own(&self) -> u64 {
        self.last.swap - self.not_own
    }
}

/// Returns the sample at which the lease in `cgroup`, capped at `cap`, pressed its cap
/// into swap past `threshold` ([`SwapKill`]), with what of its swap it pushed there
/// itself ([`SwapWatch::own`]); sampled every `every`. Never returns for a lease with
/// no cap. A sample that cannot be read is skipped: the kernel's own OOM kill at the
/// cap still ends such a lease once swap is full.
pub(crate) async fn presses_into_swap(
    cgroup: &LeaseCgroup,
    cap: Option<u64>,
    threshold: u64,
    every: Duration,
) -> (Pressure, u64) {
    if cap.is_none() {
        return std::future::pending().await;
    }
    // The lease cgroup is new, so its `max` events and its swap count from 0.
    let mut watch = SwapWatch::new(threshold);
    loop {
        tokio::time::sleep(every).await;
        let Ok(now) = cgroup.pressure() else {
            continue;
        };
        if watch.kills(now) {
            return (now, watch.own());
        }
    }
}

/// One lease's cgroup.
#[derive(Debug)]
pub(crate) struct LeaseCgroup {
    /// The directory, under the cgroup filesystem's mount.
    dir: PathBuf,
    /// The same cgroup as Podman's `--cgroup-parent` names it: relative to the mount,
    /// starting with `/`.
    name: String,
}

impl LeaseCgroup {
    /// The cgroup `<parent>/<leaf>` under the cgroup mount `root`; nothing is created
    /// yet, so cleaning up after a lease that never got this far finds nothing.
    pub(crate) fn at(root: &Path, parent: &str, leaf: &str) -> Self {
        let name = format!("{}/{leaf}", parent.trim_end_matches('/'));
        let dir = root.join(name.trim_start_matches('/'));
        Self { dir, name }
    }

    /// Creates the cgroup and sets its limits. A cgroup left half-made is removed by
    /// [`LeaseCgroup::remove`] like a whole one.
    pub(crate) fn create(&self, resources: Resources) -> io::Result<()> {
        std::fs::create_dir(&self.dir)?;
        self.write("cgroup.subtree_control", SUBTREE)?;
        // Before the container exists, so nothing in the lease ever runs without it.
        self.write("memory.oom.group", "1")?;
        if let Some(max) = memory_max(resources.memory_bytes) {
            self.write("memory.max", &max.to_string())?;
        }
        if let Some(weight) = cpu_weight(resources.cpu_millis) {
            self.write("cpu.weight", &weight.to_string())?;
        }
        Ok(())
    }

    /// The name Podman's `--cgroup-parent` takes.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// The directory under the cgroup mount.
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    fn write(&self, file: &str, value: &str) -> io::Result<()> {
        std::fs::write(self.dir.join(file), value)
            .map_err(|e| io::Error::new(e.kind(), format!("write {file}: {e}")))
    }

    /// The counters of this cgroup's `memory.events` (this cgroup and below).
    pub(crate) fn events(&self) -> io::Result<MemoryEvents> {
        let events = std::fs::read_to_string(self.dir.join("memory.events"))
            .map_err(|e| io::Error::new(e.kind(), format!("read memory.events: {e}")))?;
        MemoryEvents::parse(&events)
    }

    /// The lease's `max` events, memory and swap now, for [`SwapKill`].
    pub(crate) fn pressure(&self) -> io::Result<Pressure> {
        Ok(Pressure {
            max: self.events()?.max,
            current: self.read_u64("memory.current")?,
            swap: self.read_u64("memory.swap.current")?,
        })
    }

    fn read_u64(&self, file: &str) -> io::Result<u64> {
        let text = std::fs::read_to_string(self.dir.join(file))
            .map_err(|e| io::Error::new(e.kind(), format!("read {file}: {e}")))?;
        text.trim()
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("{file} {text:?}")))
    }

    /// The most memory the cgroup has held at once (`memory.peak`).
    pub(crate) fn peak(&self) -> io::Result<u64> {
        let peak = std::fs::read_to_string(self.dir.join("memory.peak"))?;
        peak.trim().parse().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, format!("memory.peak {peak:?}"))
        })
    }

    /// How many times the OOM killer has run for the parent's (`actions/`) own limit:
    /// `oom` in its `memory.events.local`, which counts no child's limit.
    pub(crate) fn parent_ooms(&self) -> io::Result<u64> {
        let parent = self.dir.parent().unwrap_or(&self.dir);
        let events = std::fs::read_to_string(parent.join("memory.events.local"))?;
        count(&events, "oom").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("no oom count: {events:?}"),
            )
        })
    }

    /// What the daemon reports when the kernel's OOM killer ended this lease; `None`
    /// when it killed nothing here. `cap` is the lease's `memory.max`, and
    /// `backstop_before` the parent's [`LeaseCgroup::parent_ooms`] when the lease
    /// started, for the busy-node message.
    pub(crate) fn oom_outcome(
        &self,
        cap: Option<u64>,
        backstop_before: Option<u64>,
    ) -> io::Result<Option<RuntimeError>> {
        let events = self.events()?;
        Ok(match classify(events) {
            None => None,
            Some(OomKill::OwnCap) => {
                let limit = cap.unwrap_or(0);
                // The usage reached the cap, so the cap is the least it used.
                let used = self.peak().unwrap_or(limit);
                Some(RuntimeError::OutOfMemory { used, limit })
            }
            Some(OomKill::BusyNode) => {
                let backstop = match (backstop_before, self.parent_ooms().ok()) {
                    (Some(before), Some(now)) if now > before => format!(
                        "the OOM killer ran {} time(s) for the actions/ cgroup's own limit \
                         during the lease",
                        now - before
                    ),
                    (Some(_), Some(_)) => "the OOM killer did not run for the actions/ \
                                           cgroup's own limit during the lease: a limit \
                                           above it, or the host, ran short"
                        .to_owned(),
                    _ => "the actions/ cgroup's memory.events.local could not be read".to_owned(),
                };
                Some(RuntimeError::BusyNode(format!(
                    "the kernel OOM killer killed {} process(es) in the lease cgroup, \
                     whose own limit did not run it (memory.events: oom {}, max {}); \
                     {backstop}",
                    events.oom_kill, events.oom, events.max
                )))
            }
        })
    }

    /// Kills every process in the cgroup (`cgroup.kill`).
    pub(crate) fn kill(&self) -> io::Result<()> {
        self.write("cgroup.kill", "1")
    }

    /// Removes the cgroup and any cgroups left below it, deepest first. A cgroup still
    /// emptying is retried for up to five seconds. Blocking: run it off the async threads.
    pub(crate) fn remove(&self) -> io::Result<()> {
        remove_tree(&self.dir)
    }
}

/// The `memory.events` counters that say whether, and why, the kernel's OOM killer
/// ended a lease. On cgroup v2 they count this cgroup and every cgroup below it; an
/// event is counted at the cgroup whose limit caused it, and in that cgroup's
/// ancestors, never in its children.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MemoryEvents {
    /// Times usage reached a `memory.max`: the lease's own cap (nothing below it has
    /// one). Reclaim (swap) may have resolved them, so on its own this is no kill.
    pub max: u64,
    /// Times the OOM killer was invoked because a limit at or below this cgroup could
    /// not be met: here, the lease's own cap.
    pub oom: u64,
    /// Processes in this cgroup or below that any OOM killer killed: the lease's own,
    /// `actions/`'s, or the host's.
    pub oom_kill: u64,
}

impl MemoryEvents {
    fn parse(events: &str) -> io::Result<Self> {
        let get = |key| {
            count(events, key).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("memory.events has no {key} count: {events:?}"),
                )
            })
        };
        Ok(Self {
            max: get("max")?,
            oom: get("oom")?,
            oom_kill: get("oom_kill")?,
        })
    }
}

/// The value of `key` in a flat-keyed cgroup file (`<key> <n>` per line).
fn count(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix(' '))
        .and_then(|n| n.trim().parse().ok())
}

/// How the kernel's OOM killer ended a lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OomKill {
    /// The lease reached its own cap and the OOM killer ran for that cap (`oom` and
    /// `max` counted in the lease): the action needs more memory than it booked.
    OwnCap,
    /// Processes of the lease were OOM-killed while the lease's own cap never made the
    /// OOM killer run (no `oom` in the lease): `actions/`'s backstop or the host ran
    /// short. The action's booking was not the cause.
    BusyNode,
}

/// How the OOM killer ended the lease whose `memory.events` read `events`; `None` when
/// it killed nothing there. The exit code plays no part: a busy node's kill and a kill
/// at the lease's cap both end the action with SIGKILL (137).
pub(crate) fn classify(events: MemoryEvents) -> Option<OomKill> {
    if events.oom_kill == 0 {
        None
    } else if events.oom > 0 && events.max > 0 {
        Some(OomKill::OwnCap)
    } else {
        Some(OomKill::BusyNode)
    }
}

/// Removes `dir` and the cgroups below it. A busy cgroup (a process still in it, or a
/// child cgroup made after the listing) gets the whole tree killed (`cgroup.kill`: the
/// lease is over) and the removal starts again from a fresh listing.
fn remove_tree(dir: &Path) -> io::Result<()> {
    retry_busy(dir, REMOVE_TRIES, REMOVE_PAUSE, &mut remove_once)
}

/// Runs `remove` on `dir` until it succeeds, fails with anything but EBUSY, or has
/// failed busy `tries` times. Before each retry it writes `cgroup.kill` in `dir`.
fn retry_busy(
    dir: &Path,
    tries: u32,
    pause: Duration,
    remove: &mut dyn FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let mut busy = 0;
    loop {
        match remove(dir) {
            Ok(()) => return Ok(()),
            Err(e) if busy + 1 < tries && e.kind() == io::ErrorKind::ResourceBusy => {
                busy += 1;
                let _ = std::fs::write(dir.join("cgroup.kill"), "1");
                std::thread::sleep(pause);
            }
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("remove cgroup {}: {e}", dir.display()),
                ));
            }
        }
    }
}

/// One deepest-first pass of `rmdir` over `dir`; a cgroup already gone is fine.
fn remove_once(dir: &Path) -> io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            remove_once(&entry.path())?;
        }
    }
    match std::fs::remove_dir(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory beside the test binary (inside the target directory).
    fn scratch(name: &str) -> PathBuf {
        let exe = std::env::current_exe().expect("test binary");
        let dir = exe
            .parent()
            .expect("deps directory")
            .join("kbf-driver-container-unit")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch");
        dir
    }

    fn busy() -> io::Error {
        io::Error::from(io::ErrorKind::ResourceBusy)
    }

    /// Catches a busy cgroup (a process still exiting) failing the clean at once, and a
    /// retry that does not kill the tree first: two busy passes, then success.
    #[test]
    fn a_busy_cgroup_is_killed_and_retried() {
        let dir = scratch("busy-then-gone");
        let mut calls = 0;
        let mut remove = |d: &Path| {
            calls += 1;
            // The kill is written before every retry.
            assert_eq!(d.join("cgroup.kill").exists(), calls > 1);
            if calls < 3 { Err(busy()) } else { Ok(()) }
        };
        retry_busy(&dir, 5, Duration::ZERO, &mut remove).expect("removed");
        assert_eq!(calls, 3);
    }

    /// Catches a retry loop that never gives up on a cgroup that stays busy, and an
    /// error that is not EBUSY retried instead of returned with the cgroup named.
    #[test]
    fn retries_end_and_other_errors_are_returned_at_once() {
        let dir = scratch("always-busy");
        let mut calls = 0;
        let mut remove = |_: &Path| {
            calls += 1;
            Err(busy())
        };
        let err = retry_busy(&dir, 4, Duration::ZERO, &mut remove).expect_err("busy");
        assert_eq!((calls, err.kind()), (4, io::ErrorKind::ResourceBusy));
        assert!(err.to_string().contains("always-busy"), "{err}");

        let mut calls = 0;
        let mut remove = |_: &Path| {
            calls += 1;
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        };
        let err = retry_busy(&dir, 4, Duration::ZERO, &mut remove).expect_err("denied");
        assert_eq!((calls, err.kind()), (1, io::ErrorKind::PermissionDenied));
    }

    /// Catches child cgroups left behind (removal must go deepest first), a cgroup
    /// already gone failing the clean, and a cgroup that will not go read as removed.
    /// On cgroupfs the interface files vanish with their cgroup; on a plain directory a
    /// file stays, so it stands in for a cgroup `rmdir` refuses.
    #[test]
    fn removal_goes_deepest_first_and_reports_what_stays() {
        let dir = scratch("deepest-first");
        std::fs::create_dir_all(dir.join("a/b/c")).expect("mkdir");
        std::fs::create_dir_all(dir.join("d")).expect("mkdir");
        remove_once(&dir).expect("removed");
        assert!(!dir.exists());
        remove_once(&dir).expect("already gone is fine");

        let dir = scratch("stays");
        std::fs::write(dir.join("memory.events"), "").expect("plant a file");
        let err = remove_once(&dir).expect_err("not empty");
        assert_eq!(err.kind(), io::ErrorKind::DirectoryNotEmpty);
    }

    /// Catches a listing error other than "already gone" read as success, which would
    /// report a lease cgroup removed while it is still there.
    #[test]
    fn a_listing_error_is_returned() {
        let dir = scratch("not-a-dir");
        let file = dir.join("cgroup");
        std::fs::write(&file, "").expect("plant a file");
        let err = remove_once(&file).expect_err("ENOTDIR");
        assert_eq!(err.kind(), io::ErrorKind::NotADirectory);
    }

    /// Catches a cgroup that vanishes between its listing and its `rmdir` (Podman
    /// removing its own cgroup meanwhile) failing the clean. `<dir>/sub/..` lists
    /// `<dir>`, whose pass removes `sub`; the `rmdir` of `<dir>/sub/..` then finds no
    /// `sub` to walk through (ENOENT), as if the cgroup had gone.
    #[test]
    fn a_cgroup_gone_before_its_rmdir_is_fine() {
        let dir = scratch("gone-before-rmdir");
        std::fs::create_dir_all(dir.join("sub/leaf")).expect("mkdir");
        remove_once(&dir.join("sub/..")).expect("gone is fine");
        assert!(!dir.join("sub").exists());
    }

    /// Catches a cap that drifts from the native driver's limit (the "cap = booking"
    /// mutant: no buffer above the booking), and a zero booking read as "512 MiB for
    /// everything".
    #[test]
    fn memory_max_is_one_and_a_half_times_plus_headroom() {
        assert_eq!(memory_max(1 << 30), Some((3 << 29) + (512 << 20)));
        assert_eq!(memory_max(0), None);
        assert_eq!(memory_max(u64::MAX), Some(u64::MAX));
    }

    /// Catches a cgroup made without its hard cap or its OOM group, and a swap limit
    /// or soft limit written (the policy leaves swap allowed and sets no
    /// `memory.high`). A plain directory stands in for cgroupfs: `create` writes the
    /// files it sets and no others.
    #[test]
    fn create_writes_the_cap_and_the_oom_group_and_no_swap_limit() {
        let root = scratch("create");
        let lease = LeaseCgroup::at(&root, "/actions", "kbf-lease-1-1");
        std::fs::create_dir(root.join("actions")).expect("mkdir");
        lease
            .create(Resources::new(2000, 1 << 30))
            .expect("created");
        let read = |f: &str| std::fs::read_to_string(lease.dir().join(f)).ok();
        assert_eq!(
            read("memory.max"),
            Some(((3u64 << 29) + (512 << 20)).to_string())
        );
        assert_eq!(read("memory.oom.group").as_deref(), Some("1"));
        assert_eq!(read("cpu.weight").as_deref(), Some("200"));
        assert_eq!(read("memory.swap.max"), None);
        assert_eq!(read("memory.high"), None);

        let unbooked = LeaseCgroup::at(&root, "/actions", "kbf-lease-1-2");
        unbooked.create(Resources::default()).expect("created");
        let read = |f: &str| std::fs::read_to_string(unbooked.dir().join(f)).ok();
        assert_eq!(read("memory.max"), None);
        assert_eq!(read("memory.oom.group").as_deref(), Some("1"));
    }

    fn events(max: u64, oom: u64, oom_kill: u64) -> MemoryEvents {
        MemoryEvents { max, oom, oom_kill }
    }

    /// Catches a kill classified by anything but the lease's own counters: a lease
    /// that hit its own cap and was killed for it is `OwnCap`; one killed while its
    /// own cap never made the OOM killer run is `BusyNode`, even after it touched the
    /// cap once and reclaim resolved it (`max` without `oom`); no kill is `None`.
    #[test]
    fn a_kill_is_the_leases_only_when_its_own_cap_ran_the_oom_killer() {
        assert_eq!(classify(events(3, 1, 2)), Some(OomKill::OwnCap));
        assert_eq!(classify(events(0, 0, 1)), Some(OomKill::BusyNode));
        assert_eq!(classify(events(5, 0, 1)), Some(OomKill::BusyNode));
        // `oom` without `max` is a count the kernel does not give (the OOM killer runs
        // for a limit only once usage reached it); it is not read as the lease's own.
        assert_eq!(classify(events(0, 1, 1)), Some(OomKill::BusyNode));
        assert_eq!(classify(events(5, 1, 0)), None);
        assert_eq!(classify(events(0, 0, 0)), None);
    }

    /// Catches a counter read from the wrong line (`oom` from `oom_kill` or
    /// `oom_group_kill`, which share its prefix) and a missing count read as zero. The
    /// second text puts `oom_kill` and `oom_group_kill` before `oom`, so a key matched as
    /// a bare prefix finds `oom_kill`'s line first and fails to parse `oom`; the
    /// kernel's own order (the first text) would hide that.
    #[test]
    fn memory_events_are_parsed_by_whole_key() {
        let text = "low 0\nhigh 9\nmax 4\noom 2\noom_kill 7\noom_group_kill 1\n";
        assert_eq!(MemoryEvents::parse(text).expect("parsed"), events(4, 2, 7));
        let text = "oom_group_kill 1\noom_kill 7\nmax 4\noom 2\n";
        assert_eq!(MemoryEvents::parse(text).expect("parsed"), events(4, 2, 7));
        let text = "oom_kill 7\noom_group_kill 1\nmax 4\n";
        let err = MemoryEvents::parse(text).expect_err("no oom line");
        assert!(err.to_string().contains("no oom count"), "{err}");
        let err = MemoryEvents::parse("max 1\noom 1\noom_kill many\n").expect_err("garbled");
        assert!(err.to_string().contains("no oom_kill count"), "{err}");
    }

    /// Catches a threshold that drops the floor (a small booking killed for a few cold
    /// pages) or the share of the booking (a large one killed at 512 MiB of swap), and
    /// a share that overflows for a huge booking.
    #[test]
    fn the_swap_threshold_is_the_floor_or_a_share_of_the_booking() {
        let policy = SwapKill::DEFAULT;
        assert_eq!(policy.threshold(0), 512 << 20);
        assert_eq!(policy.threshold(1 << 30), 512 << 20);
        assert_eq!(policy.threshold(2 << 30), 512 << 20);
        assert_eq!(policy.threshold(8 << 30), 2 << 30);
        assert_eq!(policy.threshold(u64::MAX), u64::MAX / 4);
        let flat = SwapKill {
            floor_bytes: 7,
            percent: 0,
            every: Duration::ZERO,
        };
        assert_eq!(flat.threshold(1 << 40), 7);
    }

    const MIB: u64 = 1 << 20;

    /// The index of the sample, fed in order to the watch of a new lease, at which it
    /// kills the lease: each sample is (`memory.events` `max`, `memory.swap.current`).
    fn killed_at(samples: &[(u64, u64)], threshold: u64) -> Option<usize> {
        let mut watch = SwapWatch::new(threshold);
        samples.iter().position(|&(max, swap)| {
            watch.kills(Pressure {
                max,
                current: 0,
                swap,
            })
        })
    }

    /// Catches the false positive seen on podman-arm (a lease killed as out of memory
    /// for swap the host put there): the absolute-swap rule. Host reclaim moved
    /// 143265792 bytes of the lease to swap below its cap (`max` 0), a few pages were
    /// read back (143233024), then the lease pressed its cap (`max` 156 to 7205) and
    /// pushed 32 KiB back out. The lease pushed 32 KiB into swap itself, far below the
    /// 8 MiB threshold; the old rule counted all 143 MB and killed it.
    #[test]
    fn a_lease_the_host_swapped_is_not_killed_for_a_few_pages_at_its_cap() {
        let mut samples = vec![(0, 143_265_792); 17];
        samples.push((0, 143_233_024));
        for max in [
            156, 565, 1184, 1986, 2838, 3650, 4268, 4829, 5411, 6043, 6628, 7205,
        ] {
            samples.push((max, 143_233_024));
        }
        samples.push((7800, 143_265_792));
        assert_eq!(killed_at(&samples, 8 * MIB), None);
        let mut watch = SwapWatch::new(8 * MIB);
        for &(max, swap) in &samples {
            watch.kills(Pressure {
                max,
                current: 0,
                swap,
            });
        }
        assert_eq!(watch.own(), 32 << 10, "what the lease pushed itself");
    }

    /// Catches a rule that never kills (or kills too late, or at the threshold rather
    /// than past it): a lease at 0 swap pressing its cap pushes 3 MiB more into swap at
    /// each sample. At exactly the 8 MiB threshold it lives; one byte past, it dies.
    #[test]
    fn a_lease_pressing_its_cap_into_swap_is_killed_past_the_threshold() {
        let samples = [(10, 3 * MIB), (20, 6 * MIB), (30, 9 * MIB)];
        assert_eq!(killed_at(&samples, 8 * MIB), Some(2));
        let samples = [(10, 4 * MIB), (20, 8 * MIB), (30, 8 * MIB + 1)];
        assert_eq!(killed_at(&samples, 8 * MIB), Some(2));
        assert_eq!(killed_at(&samples[..2], 8 * MIB), None);
    }

    /// Catches host swap ignored for good (a lease the host swapped never killed, even
    /// when it then pushes past the threshold itself at its cap) and the host's share
    /// taken wrong: from the first sample (143265792) rather than after the pages read
    /// back (143233024), the lease's own share at the last sample would be 32 KiB
    /// short of the threshold and it would live.
    #[test]
    fn a_lease_the_host_swapped_is_killed_for_what_it_pushes_itself_past_the_threshold() {
        let host = 143_233_024;
        let samples = [
            (0, 143_265_792),
            (0, host),
            (100, host + 4 * MIB),
            (200, host + 8 * MIB),
            (300, host + 8 * MIB + 1),
        ];
        assert_eq!(killed_at(&samples, 8 * MIB), Some(4));
        assert_eq!(killed_at(&samples[..4], 8 * MIB), None);
    }

    /// Catches a share reset when the lease's `max` events pause (a lease pressing its
    /// cap into swap in bursts never killed), and swap the host moves during the pause
    /// counted as the lease's. The lease pushes 5 MiB at its cap, pauses (`max` flat)
    /// while the host moves 100 MiB more of it to swap, then pushes 4 MiB more at its
    /// cap: 9 MiB is its own, past 8 MiB. With only 1 MiB more, 6 MiB is its own.
    #[test]
    fn pushes_at_the_cap_add_up_across_a_pause_and_host_swap_in_the_pause_does_not() {
        let samples = [
            (10, 5 * MIB),
            (10, 5 * MIB),
            (10, 105 * MIB),
            (20, 109 * MIB),
        ];
        assert_eq!(killed_at(&samples, 8 * MIB), Some(3));
        let samples = [(10, 5 * MIB), (10, 105 * MIB), (20, 106 * MIB)];
        assert_eq!(killed_at(&samples, 8 * MIB), None);
    }

    /// Catches a fall in swap taken from the host's share first: pages read back at the
    /// cap and pushed out again would then add up as the lease's own until it was
    /// killed. The host moved 100 MiB of the lease to swap; at its cap the lease pushes
    /// 6 MiB out, reads 4 MiB back, pushes it out again, and so on. Its own share never
    /// passes 6 MiB.
    #[test]
    fn pages_read_back_and_pushed_out_again_at_the_cap_do_not_add_up() {
        let mut samples = vec![(0, 100 * MIB)];
        for i in 1..=20 {
            let swap = if i % 2 == 1 { 106 * MIB } else { 102 * MIB };
            samples.push((i * 10, swap));
        }
        assert_eq!(killed_at(&samples, 8 * MIB), None);
    }

    /// Catches a kill on swap alone (the "ignores max events" mutant: host pressure
    /// moves a lease below its cap, or one that hit its cap once before, into swap) and
    /// on `max` events alone (a lease at its cap that reclaim keeps in RAM, or whose
    /// swap is flat at the threshold).
    #[test]
    fn swap_without_max_events_or_max_events_without_swap_kill_nothing() {
        assert_eq!(killed_at(&[(0, 1 << 40)], 8 * MIB), None);
        assert_eq!(
            killed_at(&[(3, 0), (3, 1 << 40), (3, 1 << 41)], 8 * MIB),
            None
        );
        assert_eq!(killed_at(&[(9, 0), (99, 0), (999, 0)], 8 * MIB), None);
        assert_eq!(
            killed_at(&[(10, 8 * MIB), (20, 8 * MIB), (30, 8 * MIB)], 8 * MIB),
            None
        );
    }

    /// Catches a sample that cannot be read taken as zero (a missing or garbled
    /// `memory.current` or `memory.swap.current` must fail the sample, which the watch
    /// then skips), and a whole sample read from the wrong files.
    #[test]
    fn a_pressure_sample_reads_its_three_files_or_fails() {
        let root = scratch("pressure");
        let lease = LeaseCgroup::at(&root, "/", "kbf-lease-1-1");
        std::fs::create_dir(lease.dir()).expect("mkdir");
        let put = |f: &str, v: &str| std::fs::write(lease.dir().join(f), v).expect("write");
        put("memory.events", "max 3\noom 0\noom_kill 0\n");
        let err = lease.pressure().expect_err("no memory.current");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("read memory.current"), "{err}");
        put("memory.current", "4096\n");
        put("memory.swap.current", "lots\n");
        let err = lease.pressure().expect_err("garbled");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("memory.swap.current"), "{err}");
        put("memory.swap.current", "8192\n");
        assert_eq!(
            lease.pressure().expect("read"),
            Pressure {
                max: 3,
                current: 4096,
                swap: 8192
            }
        );
    }

    /// Catches CPU booked as a hard cap's units, or a weight outside the kernel's range.
    #[test]
    fn cpu_weight_is_a_hundred_per_core_within_range() {
        assert_eq!(cpu_weight(1000), Some(100));
        assert_eq!(cpu_weight(2500), Some(250));
        assert_eq!(cpu_weight(1), Some(1));
        assert_eq!(cpu_weight(u64::MAX), Some(10_000));
        assert_eq!(cpu_weight(0), None);
    }
}
