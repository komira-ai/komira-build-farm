//! The node report: what the daemon detected about its machine, as the sorted
//! `Capability` list a Hello carries, and the hash heartbeats repeat (RFC 4.3).
//!
//! Detection reads the text the kernel publishes and hands it to `kbf-caps`, which
//! owns the parsing. v0 detects on Linux only: `arch`, `os`, every `isa_level` the CPU
//! reaches, every `cpu.features` flag, `cpus`, `mem_gib`, `page_size`, and the
//! `drivers` the daemon was started with. macOS detection arrives with the Mac drivers.

use std::collections::BTreeSet;

use kbf_caps::CpuCaps;
use kbf_proto::worker::{Capability, Hello};
use prost::Message;
use sha2::{Digest, Sha256};

/// Why the node report could not be built.
#[derive(Debug, thiserror::Error)]
pub enum DetectError {
    /// A kernel file could not be read.
    #[error("read {path}: {source}")]
    Read {
        path: &'static str,
        #[source]
        source: std::io::Error,
    },
    /// `/proc/cpuinfo` could not be parsed.
    #[error("parse /proc/cpuinfo: {0}")]
    Cpu(#[from] kbf_caps::ParseError),
    /// A kernel file lacks a line the report needs.
    #[error("{path} has no {what}")]
    Missing {
        path: &'static str,
        what: &'static str,
    },
    /// This operating system has no detection yet.
    #[error("node detection is not implemented for {0}")]
    UnsupportedOs(&'static str),
}

/// A node report: sorted, de-duplicated `(key, value)` entries and their hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeReport {
    capabilities: Vec<Capability>,
    hash: Vec<u8>,
}

impl NodeReport {
    /// A report from entries in any order. Equal sets of entries give equal reports.
    pub fn new<K, V>(entries: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        let sorted: BTreeSet<(String, String)> = entries
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        let capabilities: Vec<Capability> = sorted
            .into_iter()
            .map(|(key, value)| Capability { key, value })
            .collect();
        let hash = report_hash(&capabilities);
        Self { capabilities, hash }
    }

    /// Detects this machine's report. `drivers` are the execution drivers the daemon
    /// offers.
    pub fn detect(drivers: &[&str]) -> Result<Self, DetectError> {
        if cfg!(target_os = "linux") {
            let cpuinfo = read("/proc/cpuinfo")?;
            let meminfo = read("/proc/meminfo")?;
            let smaps = read("/proc/self/smaps")?;
            linux_report(&cpuinfo, &meminfo, &smaps, drivers)
        } else {
            Err(DetectError::UnsupportedOs(std::env::consts::OS))
        }
    }

    /// The entries, sorted by key, then value.
    #[must_use]
    pub fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    /// SHA-256 of the entries as `Hello.capabilities` encodes them, in order.
    #[must_use]
    pub fn hash(&self) -> &[u8] {
        &self.hash
    }
}

/// SHA-256 over the wire bytes of `capabilities` as Hello's field 4: each entry's tag,
/// length and encoding, in order.
fn report_hash(capabilities: &[Capability]) -> Vec<u8> {
    let only_caps = Hello {
        capabilities: capabilities.to_vec(),
        ..Hello::default()
    };
    Sha256::digest(only_caps.encode_to_vec()).to_vec()
}

fn read(path: &'static str) -> Result<String, DetectError> {
    std::fs::read_to_string(path).map_err(|source| DetectError::Read { path, source })
}

/// The report of a Linux node from its `/proc/cpuinfo`, `/proc/meminfo` and
/// `/proc/self/smaps` text.
pub fn linux_report(
    cpuinfo: &str,
    meminfo: &str,
    smaps: &str,
    drivers: &[&str],
) -> Result<NodeReport, DetectError> {
    let cpu = CpuCaps::from_linux_cpuinfo(cpuinfo)?;
    let cpus = cpuinfo
        .lines()
        .filter(|l| {
            l.split_once(':')
                .is_some_and(|(k, _)| k.trim() == "processor")
        })
        .count();
    if cpus == 0 {
        return Err(DetectError::Missing {
            path: "/proc/cpuinfo",
            what: "processor line",
        });
    }
    let mem_kib = kib_field(meminfo, "MemTotal").ok_or(DetectError::Missing {
        path: "/proc/meminfo",
        what: "MemTotal line",
    })?;
    let page_kib = kib_field(smaps, "KernelPageSize").ok_or(DetectError::Missing {
        path: "/proc/self/smaps",
        what: "KernelPageSize line",
    })?;

    let mut entries: Vec<(String, String)> = vec![
        ("arch".into(), cpu.arch().name().into()),
        ("os".into(), "linux".into()),
        ("cpus".into(), cpus.to_string()),
        ("mem_gib".into(), (mem_kib >> 20).to_string()),
        ("page_size".into(), (page_kib * 1024).to_string()),
    ];
    entries.extend(
        cpu.levels()
            .into_iter()
            .map(|l| ("isa_level".into(), l.name().into())),
    );
    entries.extend(
        cpu.features()
            .iter()
            .map(|f| ("cpu.features".into(), f.clone())),
    );
    entries.extend(drivers.iter().map(|d| ("drivers".into(), (*d).into())));
    Ok(NodeReport::new(entries))
}

/// The first `<key>: <n> kB` line's value.
fn kib_field(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (k, rest) = line.split_once(':')?;
        if k.trim() != key {
            return None;
        }
        rest.trim().strip_suffix("kB")?.trim().parse().ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SKYLAKE: &str =
        include_str!("../../kbf-caps/tests/fixtures/skylake_sp_platinum_8180.cpuinfo");
    const MEMINFO: &str = "MemTotal:       394796064 kB\nMemFree:  1 kB\n";
    const SMAPS: &str =
        "55d0-55d1 r--p 0 08:01 1 /bin/x\nSize: 4 kB\nKernelPageSize:        4 kB\n";

    fn values<'a>(r: &'a NodeReport, key: &str) -> Vec<&'a str> {
        r.capabilities()
            .iter()
            .filter(|c| c.key == key)
            .map(|c| c.value.as_str())
            .collect()
    }

    /// Catches: a report that drops features (keeping only the ones an ISA level names,
    /// or a fixed list), or lists only the highest ISA level. The scheduler matches
    /// `cpu.feature` requests against this list, so a dropped flag hides capacity.
    #[test]
    fn reports_every_feature_and_every_level() {
        let r = linux_report(SKYLAKE, MEMINFO, SMAPS, &["fake"]).expect("report");
        let cpu = CpuCaps::from_linux_cpuinfo(SKYLAKE).expect("fixture parses");
        let want: Vec<&str> = cpu.features().iter().map(String::as_str).collect();
        assert!(want.contains(&"avx512vl"), "fixture sanity");
        assert_eq!(values(&r, "cpu.features"), want);
        assert_eq!(
            values(&r, "isa_level"),
            ["x86-64-v1", "x86-64-v2", "x86-64-v3", "x86-64-v4"]
        );
        assert_eq!(values(&r, "arch"), ["x86_64"]);
        assert_eq!(values(&r, "mem_gib"), ["376"]);
        assert_eq!(values(&r, "page_size"), ["4096"]);
        assert_eq!(values(&r, "drivers"), ["fake"]);
    }

    /// Catches: a report whose order or hash depends on the order entries were added,
    /// so the server would see a changed report where nothing changed.
    #[test]
    fn equal_entries_give_equal_bytes_and_hash() {
        let a = NodeReport::new([("os", "linux"), ("arch", "x86_64"), ("drivers", "fake")]);
        let b = NodeReport::new([("drivers", "fake"), ("os", "linux"), ("arch", "x86_64")]);
        assert_eq!(a, b);
        let keys: Vec<&str> = a.capabilities().iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, ["arch", "drivers", "os"]);
        let c = NodeReport::new([("os", "linux"), ("arch", "arm64"), ("drivers", "fake")]);
        assert_ne!(a.hash(), c.hash());
    }
}
