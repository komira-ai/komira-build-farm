//! What a lease cgroup counted, read when the action ended (`cpu.stat`, `memory.peak`,
//! `memory.numa_stat`) into the [`CgroupUsage`] the lease's result carries.
//!
//! Measurement only: nothing here decides anything. A file that is not there (cgroup
//! v1, a kernel without `memory.peak`, a build without NUMA) leaves its fields absent,
//! and a file that does not parse does too; neither is an error, since the lease's
//! result does not depend on them.
//!
//! The NUMA split is the one a sampler saw while the action ran, if it held more than
//! at its end: the kernel uncharges anonymous memory when the processes exit, so
//! `memory.numa_stat` after the end shows little more than page cache ([`NumaSample`]).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kbf_proto::worker::{CgroupUsage, NumaNodeBytes, ResourceUsage};

/// How often [`NumaSample::watch`] reads `memory.numa_stat` while the action runs, unless
/// `ContainerConfig::numa_every` says otherwise.
pub(crate) const NUMA_EVERY: Duration = Duration::from_secs(1);

/// The counters of `cpu.stat` the usage takes.
#[derive(Debug, Default, PartialEq, Eq)]
struct CpuStat {
    usage: Option<u64>,
    user: Option<u64>,
    system: Option<u64>,
    periods: Option<u64>,
    throttled: Option<u64>,
    throttled_usec: Option<u64>,
}

/// The `key value` lines of `cpu.stat`: each wanted key's value that parses.
fn cpu_stat(text: &str) -> CpuStat {
    let mut stat = CpuStat::default();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let (Some(key), Some(value)) = (words.next(), words.next()) else {
            continue;
        };
        let Ok(value) = value.parse::<u64>() else {
            continue;
        };
        match key {
            "usage_usec" => stat.usage = Some(value),
            "user_usec" => stat.user = Some(value),
            "system_usec" => stat.system = Some(value),
            "nr_periods" => stat.periods = Some(value),
            "nr_throttled" => stat.throttled = Some(value),
            "throttled_usec" => stat.throttled_usec = Some(value),
            _ => {}
        }
    }
    stat
}

/// `anon` plus `file` bytes per node of a `memory.numa_stat` (lines such as
/// `anon N0=4096 N1=8192`; cgroup v2 counts bytes). Empty if neither line is there.
fn numa_stat(text: &str) -> BTreeMap<u32, u64> {
    let mut nodes = BTreeMap::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        if !matches!(words.next(), Some("anon" | "file")) {
            continue;
        }
        for word in words {
            let Some((node, bytes)) = word.strip_prefix('N').and_then(|w| w.split_once('=')) else {
                continue;
            };
            if let (Ok(node), Ok(bytes)) = (node.parse::<u32>(), bytes.parse::<u64>()) {
                let held: &mut u64 = nodes.entry(node).or_default();
                *held = held.saturating_add(bytes);
            }
        }
    }
    nodes
}

fn total(nodes: &BTreeMap<u32, u64>) -> u64 {
    nodes.values().fold(0, |a, b| a.saturating_add(*b))
}

/// The number in the file `name` of the cgroup `dir`, if it is there and parses.
fn read_u64(dir: &Path, name: &str) -> Option<u64> {
    std::fs::read_to_string(dir.join(name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The NUMA split of the sample that held the most memory.
#[derive(Debug, Default)]
pub(crate) struct NumaSample {
    best: BTreeMap<u32, u64>,
    /// Counts each sample that read some memory, if the runtime counts them.
    counted: Option<Arc<AtomicU64>>,
}

impl NumaSample {
    /// A sample that adds one to `counted` for each reading of `memory.numa_stat` that
    /// held memory ([`crate::ContainerRuntime::numa_samples`]).
    pub(crate) fn counting(counted: Arc<AtomicU64>) -> Self {
        Self {
            best: BTreeMap::new(),
            counted: Some(counted),
        }
    }

    /// Reads `memory.numa_stat` of the cgroup `dir` and keeps it if it holds more than
    /// every earlier sample (a sample that cannot be read changes nothing).
    pub(crate) fn observe(&mut self, dir: &Path) {
        let Ok(text) = std::fs::read_to_string(dir.join("memory.numa_stat")) else {
            return;
        };
        let now = numa_stat(&text);
        if let Some(counted) = &self.counted {
            counted.fetch_add(u64::from(total(&now) > 0), Ordering::Release);
        }
        if total(&now) > total(&self.best) {
            self.best = now;
        }
    }

    /// Samples every `every` (the driver passes [`NUMA_EVERY`]), for as long as it is
    /// polled.
    pub(crate) async fn watch(&mut self, dir: &Path, every: Duration) {
        loop {
            tokio::time::sleep(every).await;
            self.observe(dir);
        }
    }

    /// The usage of the cgroup `dir`, read after the action's processes have exited
    /// (the cgroup is not removed yet), with the best NUMA sample: the end counts as
    /// one more sample.
    pub(crate) fn finish(mut self, dir: &Path) -> CgroupUsage {
        self.observe(dir);
        let stat = std::fs::read_to_string(dir.join("cpu.stat"))
            .map(|text| cpu_stat(&text))
            .unwrap_or_default();
        CgroupUsage {
            cpu_usage_micros: stat.usage,
            cpu_user_micros: stat.user,
            cpu_system_micros: stat.system,
            cpu_periods: stat.periods,
            cpu_throttled_periods: stat.throttled,
            cpu_throttled_micros: stat.throttled_usec,
            memory_peak_bytes: read_u64(dir, "memory.peak"),
            numa: self
                .best
                .into_iter()
                .map(|(node, bytes)| NumaNodeBytes { node, bytes })
                .collect(),
        }
    }
}

/// The usage a result carries for a lease that ran `wall`: the cgroup's counters beside
/// the four numbers every runtime reports (user and system CPU, peak memory, wall time),
/// which come from the same files.
pub(crate) fn resource_usage(cgroup: CgroupUsage, wall: Duration) -> ResourceUsage {
    ResourceUsage {
        cpu_user_micros: cgroup.cpu_user_micros.unwrap_or(0),
        cpu_system_micros: cgroup.cpu_system_micros.unwrap_or(0),
        peak_memory_bytes: cgroup.memory_peak_bytes.unwrap_or(0),
        wall_micros: u64::try_from(wall.as_micros()).unwrap_or(u64::MAX),
        cgroup: Some(cgroup),
    }
}

#[cfg(test)]
#[path = "cgroup_usage_tests.rs"]
mod tests;
