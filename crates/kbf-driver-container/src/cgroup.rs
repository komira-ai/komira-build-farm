//! The lease cgroup, `<actions>/kbf-lease-<term>-<seq>` (RFC 10.6).
//!
//! The daemon owns a delegated `actions/` cgroup. For each lease the driver makes a child
//! with the lease's soft limits and puts the container under it (`--cgroup-parent`). The
//! lease cgroup outlives the container, so after the action ends its `memory.events`
//! still says whether the kernel OOM killer ran inside it: Podman's own `OOMKilled` flag
//! is not trusted (it reads false rootless; see the hosted-runner spike).
//!
//! Memory, per the simple policy (RFC 5.7): `memory.high` = reservation x 1.5 + 512 MiB,
//! no `memory.max` per lease and no `memory.swap.max`, so swap stays allowed. The one hard
//! limit is `memory.max` on `actions/`, which the daemon's unit sets. CPU is compressible:
//! `cpu.weight` from the booked CPU, never `cpu.max`.

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
    let mut tries = 0;
    loop {
        match remove_once(dir) {
            Ok(()) => return Ok(()),
            Err(e) if tries + 1 < REMOVE_TRIES && e.kind() == io::ErrorKind::ResourceBusy => {
                tries += 1;
                let _ = std::fs::write(dir.join("cgroup.kill"), "1");
                std::thread::sleep(REMOVE_PAUSE);
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
