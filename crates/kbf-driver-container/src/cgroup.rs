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
//! lease's pages to swap rather than killing it. The node's backstop is `memory.max`
//! on `actions/`, which the daemon writes from `--actions-memory-max-gib` (or the
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
            self.write("memory.swap.max", "0")?;
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
        assert_eq!(classify(events(5, 1, 0)), None);
        assert_eq!(classify(events(0, 0, 0)), None);
    }

    /// Catches a counter read from the wrong line (`oom` from `oom_kill` or
    /// `oom_group_kill`, which share its prefix) and a missing count read as zero.
    #[test]
    fn memory_events_are_parsed_by_whole_key() {
        let text = "low 0\nhigh 9\nmax 4\noom 2\noom_kill 7\noom_group_kill 1\n";
        assert_eq!(MemoryEvents::parse(text).expect("parsed"), events(4, 2, 7));
        let text = "oom_kill 7\noom_group_kill 1\nmax 4\n";
        let err = MemoryEvents::parse(text).expect_err("no oom line");
        assert!(err.to_string().contains("no oom count"), "{err}");
        let err = MemoryEvents::parse("max 1\noom 1\noom_kill many\n").expect_err("garbled");
        assert!(err.to_string().contains("no oom_kill count"), "{err}");
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
