//! The lease cgroup, `<actions>/kbf-lease-<term>-<seq>` (RFC 10.6).
//!
//! The daemon owns an `actions/` cgroup in its delegated subtree (`crate::delegate`).
//! For each lease the driver makes a child with the lease's soft limits and puts the
//! container under it (`--cgroup-parent`). The lease cgroup outlives the container, so
//! after the action ends its `memory.events` still says whether the kernel OOM killer
//! ran inside it: Podman's own `OOMKilled` flag is not trusted (it reads false
//! rootless; see the hosted-runner spike).
//!
//! Memory, per the simple policy (RFC 5.7): `memory.high` = reservation x 1.5 + 512 MiB,
//! no `memory.max` per lease and no `memory.swap.max`, so swap stays allowed. The one hard
//! limit is `memory.max` on `actions/`, which the daemon writes from
//! `--actions-memory-max-gib` (or the operator sets). CPU is compressible: `cpu.weight`
//! from the booked CPU, never `cpu.max`.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_types::Resources;

/// Controllers the lease hands to the container's cgroup.
const SUBTREE: &str = "+cpu +memory +pids";
/// 512 MiB, the soft limit's headroom above 1.5 x the reservation.
const HEADROOM: u64 = 512 << 20;
/// How often, and how long apart, removal retries a lease cgroup that is still busy:
/// a process still exiting, or Podman's exit hook (`podman container cleanup`, which
/// conmon starts inside the lease) still running.
const REMOVE_TRIES: u32 = 100;
const REMOVE_PAUSE: Duration = Duration::from_millis(50);

/// The soft memory limit for a lease that booked `memory_bytes`; `None` (no limit) when
/// nothing was booked.
#[must_use]
pub fn memory_high(memory_bytes: u64) -> Option<u64> {
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
        if let Some(high) = memory_high(resources.memory_bytes) {
            self.write("memory.high", &high.to_string())?;
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

    /// How many processes the kernel OOM killer has killed in this cgroup and below.
    pub(crate) fn oom_kills(&self) -> io::Result<u64> {
        let events = std::fs::read_to_string(self.dir.join("memory.events"))
            .map_err(|e| io::Error::new(e.kind(), format!("read memory.events: {e}")))?;
        events
            .lines()
            .find_map(|line| line.strip_prefix("oom_kill "))
            .and_then(|n| n.trim().parse().ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("memory.events has no oom_kill count: {events:?}"),
                )
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

    /// Catches a soft limit that drifts from the RFC formula, and a zero booking read
    /// as "512 MiB for everything".
    #[test]
    fn memory_high_is_one_and_a_half_times_plus_headroom() {
        assert_eq!(memory_high(1 << 30), Some((3 << 29) + (512 << 20)));
        assert_eq!(memory_high(0), None);
        assert_eq!(memory_high(u64::MAX), Some(u64::MAX));
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
