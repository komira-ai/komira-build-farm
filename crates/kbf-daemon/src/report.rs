//! The node report: what the daemon detected about its machine, as the sorted
//! `Capability` list a Hello carries, and the hash heartbeats repeat (RFC 4.3).
//!
//! Detection reads the text the kernel publishes and hands it to `kbf-caps`, which
//! owns the parsing. On Linux and on macOS (Apple silicon) it reports `arch`, `os`,
//! every `isa_level` the CPU reaches, every `cpu.features` flag, `cpus`, `mem_gib`,
//! `page_size`, and the `drivers` the daemon was started with; on macOS `cpu.model`
//! too. A Linux node reads `/proc`; a Mac asks `sysctl` for `hw.optional` (the text
//! `kbf-caps` parses), `hw.ncpu`, `hw.memsize`, `hw.pagesize` and
//! `machdep.cpu.brand_string`. [`NodeReport::with_entries`] adds what the command line
//! and the driver add: node labels (`label.<key>`) and driver capabilities.

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
    /// `/proc/cpuinfo` or `sysctl hw.optional` could not be parsed.
    #[error("parse the CPU description: {0}")]
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
        } else if cfg!(target_os = "macos") {
            let optional = sysctl(&["hw.optional"])?;
            let numbers = sysctl(&[
                "-n",
                "hw.ncpu",
                "hw.memsize",
                "hw.pagesize",
                "machdep.cpu.brand_string",
            ])?;
            macos_report(&optional, &numbers, drivers)
        } else {
            Err(DetectError::UnsupportedOs(std::env::consts::OS))
        }
    }

    /// This report with `entries` added (node labels, driver capabilities).
    #[must_use]
    pub fn with_entries<K, V>(self, entries: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        let mine = self.capabilities.into_iter().map(|c| (c.key, c.value));
        let more = entries.into_iter().map(|(k, v)| (k.into(), v.into()));
        Self::new(mine.chain(more))
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

/// Where macOS keeps `sysctl`.
const SYSCTL: &str = "/usr/sbin/sysctl";

/// The standard output of `sysctl` with `args`.
fn sysctl(args: &[&str]) -> Result<String, DetectError> {
    let failed = |source: std::io::Error| DetectError::Read {
        path: SYSCTL,
        source,
    };
    let out = std::process::Command::new(SYSCTL)
        .args(args)
        .output()
        .map_err(failed)?;
    if !out.status.success() {
        return Err(failed(std::io::Error::other(format!(
            "sysctl {} exited with {}: {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))));
    }
    String::from_utf8(out.stdout).map_err(|e| failed(std::io::Error::other(e)))
}

/// The report of an Apple silicon Mac from `sysctl hw.optional` (`optional`) and
/// `sysctl -n hw.ncpu hw.memsize hw.pagesize machdep.cpu.brand_string` (`numbers`,
/// one value per line in that order).
pub fn macos_report(
    optional: &str,
    numbers: &str,
    drivers: &[&str],
) -> Result<NodeReport, DetectError> {
    let cpu = CpuCaps::from_macos_sysctl(optional)?;
    let mut lines = numbers.lines().map(str::trim);
    let mut number = |what: &'static str| {
        lines
            .next()
            .and_then(|line| line.parse::<u64>().ok())
            .filter(|&n| n > 0)
            .ok_or(DetectError::Missing { path: SYSCTL, what })
    };
    let cpus = number("hw.ncpu")?;
    let mem_bytes = number("hw.memsize")?;
    let page_size = number("hw.pagesize")?;
    let model = lines
        .next()
        .filter(|line| !line.is_empty())
        .ok_or(DetectError::Missing {
            path: SYSCTL,
            what: "machdep.cpu.brand_string",
        })?;
    let mut entries = common_entries(&cpu, "macos", cpus, mem_bytes >> 30, page_size, drivers);
    entries.push(("cpu.model".into(), model.into()));
    Ok(NodeReport::new(entries))
}

/// The entries every node reports.
fn common_entries(
    cpu: &CpuCaps,
    os: &str,
    cpus: u64,
    mem_gib: u64,
    page_size: u64,
    drivers: &[&str],
) -> Vec<(String, String)> {
    let mut entries: Vec<(String, String)> = vec![
        ("arch".into(), cpu.arch().name().into()),
        ("os".into(), os.into()),
        ("cpus".into(), cpus.to_string()),
        ("mem_gib".into(), mem_gib.to_string()),
        ("page_size".into(), page_size.to_string()),
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
    entries
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

    let entries = common_entries(
        &cpu,
        "linux",
        cpus as u64,
        mem_kib >> 20,
        page_kib * 1024,
        drivers,
    );
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

    const M3_ULTRA: &str = include_str!("../../kbf-caps/tests/fixtures/apple_m3_ultra.sysctl");
    const MAC_NUMBERS: &str = "32\n274877906944\n16384\nApple M3 Ultra\n";

    /// Catches: a Mac report that drops a feature, names the OS or architecture other
    /// than the matcher's keys spell them, reads memory in the wrong unit, or loses the
    /// CPU model; and one that accepts `sysctl` output missing a value instead of
    /// refusing to start.
    #[test]
    fn a_mac_reports_its_features_counts_and_model() {
        let r = macos_report(M3_ULTRA, MAC_NUMBERS, &["native"]).expect("report");
        let cpu = CpuCaps::from_macos_sysctl(M3_ULTRA).expect("fixture parses");
        let want: Vec<&str> = cpu.features().iter().map(String::as_str).collect();
        assert!(want.contains(&"aes"), "fixture sanity");
        assert_eq!(values(&r, "cpu.features"), want);
        assert_eq!(values(&r, "arch"), ["arm64"]);
        assert_eq!(values(&r, "os"), ["macos"]);
        assert_eq!(values(&r, "cpus"), ["32"]);
        assert_eq!(values(&r, "mem_gib"), ["256"]);
        assert_eq!(values(&r, "page_size"), ["16384"]);
        assert_eq!(values(&r, "cpu.model"), ["Apple M3 Ultra"]);
        assert_eq!(values(&r, "drivers"), ["native"]);
        assert!(!values(&r, "isa_level").is_empty());
        for (numbers, what) in [
            ("", "hw.ncpu"),
            ("x\n", "hw.ncpu"),
            ("8\n0\n", "hw.memsize"),
            ("8\n1024\n", "hw.pagesize"),
            ("8\n1024\n16384\n", "machdep.cpu.brand_string"),
            ("8\n1024\n16384\n\n", "machdep.cpu.brand_string"),
        ] {
            let error = macos_report(M3_ULTRA, numbers, &[]).expect_err(numbers);
            assert!(
                matches!(error, DetectError::Missing { what: w, .. } if w == what),
                "{numbers:?}: {error}"
            );
        }
        assert!(matches!(
            macos_report("hw.optional.arm64: 0\n", MAC_NUMBERS, &[]),
            Err(DetectError::Cpu(_))
        ));
    }

    /// Catches: a `sysctl` failure (a missing key, a missing program) taken for
    /// output, and output lost. Linux has `/usr/sbin/sysctl` too, with other keys.
    #[cfg(target_os = "linux")]
    #[test]
    fn sysctl_output_and_failure() {
        assert_eq!(sysctl(&["-n", "kernel.ostype"]).expect("ostype"), "Linux\n");
        let error = sysctl(&["hw.optional"]).expect_err("no such key on Linux");
        assert!(
            error.to_string().contains("sysctl hw.optional exited"),
            "{error}"
        );
    }

    /// Catches: labels and driver capabilities dropped, or replacing the detected
    /// entries instead of joining them, or a report whose hash ignores them.
    #[test]
    fn added_entries_join_the_report() {
        let r = NodeReport::new([("os", "macos"), ("drivers", "native")]);
        let more = r
            .clone()
            .with_entries([("label.pool", "darwin-sized"), ("os", "macos")]);
        let keys: Vec<(&str, &str)> = more
            .capabilities()
            .iter()
            .map(|c| (c.key.as_str(), c.value.as_str()))
            .collect();
        assert_eq!(
            keys,
            [
                ("drivers", "native"),
                ("label.pool", "darwin-sized"),
                ("os", "macos")
            ]
        );
        assert_ne!(more.hash(), r.hash());
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
