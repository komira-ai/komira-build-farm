//! The node report: what the daemon detected about its machine, as the sorted
//! `Capability` list a Hello carries, and the hash heartbeats repeat
//! (`docs/design/capabilities.md#the-node-report`).
//!
//! Detection reads the text the kernel publishes and hands it to `kbf-caps`, which
//! owns the parsing. On Linux and on macOS (Apple silicon) it reports `arch`, `os`,
//! every `isa_level` the CPU reaches, every `cpu.features` flag, `cpus`, `mem_gib`,
//! `page_size`, `gpu`, and the `drivers` the daemon was started with; on macOS
//! `cpu.model` too. A Linux node reads `/proc`, and counts its GPUs among the PCI
//! functions in sysfs (0 when there are none, or no PCI bus). A Mac asks `sysctl` for
//! `hw.optional` (the text `kbf-caps` parses), `hw.ncpu`, `hw.memsize`, `hw.pagesize`
//! and `machdep.cpu.brand_string`, and reports `gpu` 0: its integrated GPU is not one
//! the scheduler books. [`NodeReport::with_entries`] adds what the command line and
//! the driver add: node labels (`label.<key>`) and driver capabilities.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use kbf_caps::{CpuCaps, PciFunction};
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
    /// A PCI function's sysfs entry could not be read.
    #[error("read {}: {source}", path.display())]
    ReadPci {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A PCI function's `class` or `vendor` could not be parsed.
    #[error("parse PCI functions: {0}")]
    Pci(#[source] kbf_caps::ParseError),
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

/// What a driver lets its leases use of the node, where that is less than the node
/// has. `None` is no limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capacity {
    /// The CPUs leases may run on.
    pub cpus: Option<u64>,
    /// The memory all leases together may use, in bytes.
    pub memory_bytes: Option<u64>,
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
        detect_here(drivers)
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

    /// This report with its `cpus` and `mem_gib` lowered to `capacity`: each becomes the
    /// smaller of what was detected and the limit, memory in whole GiB rounded down. A
    /// driver whose leases run under a limit (the container driver's `actions/` cgroup)
    /// reports what they may use, so the scheduler books no more than that.
    #[must_use]
    pub fn within(self, capacity: Capacity) -> Self {
        let lower = |value: String, limit: Option<u64>| match (value.parse::<u64>(), limit) {
            (Ok(detected), Some(limit)) => detected.min(limit).to_string(),
            _ => value,
        };
        let entries = self.capabilities.into_iter().map(|c| {
            let value = match c.key.as_str() {
                "cpus" => lower(c.value, capacity.cpus),
                "mem_gib" => lower(c.value, capacity.memory_bytes.map(|b| b >> 30)),
                _ => c.value,
            };
            (c.key, value)
        });
        Self::new(entries)
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

/// This Linux node's report.
#[cfg(target_os = "linux")]
fn detect_here(drivers: &[&str]) -> Result<NodeReport, DetectError> {
    let cpuinfo = read("/proc/cpuinfo")?;
    let meminfo = read("/proc/meminfo")?;
    let smaps = read("/proc/self/smaps")?;
    let gpus = linux_gpus(Path::new(PCI_DEVICES))?;
    linux_report(&cpuinfo, &meminfo, &smaps, gpus, drivers)
}

/// This Mac's report.
#[cfg(target_os = "macos")]
fn detect_here(drivers: &[&str]) -> Result<NodeReport, DetectError> {
    let optional = sysctl(&["hw.optional"])?;
    let numbers = sysctl(&[
        "-n",
        "hw.ncpu",
        "hw.memsize",
        "hw.pagesize",
        "machdep.cpu.brand_string",
    ])?;
    macos_report(&optional, &numbers, drivers)
}

/// No detection here yet.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn detect_here(_drivers: &[&str]) -> Result<NodeReport, DetectError> {
    Err(DetectError::UnsupportedOs(std::env::consts::OS))
}

#[cfg(target_os = "linux")]
fn read(path: &'static str) -> Result<String, DetectError> {
    std::fs::read_to_string(path).map_err(|source| DetectError::Read { path, source })
}

/// Where macOS keeps `sysctl`.
const SYSCTL: &str = "/usr/sbin/sysctl";

/// The standard output of `sysctl` with `args`.
#[cfg(any(target_os = "macos", test))]
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
    let mut entries = common_entries(&cpu, "macos", cpus, mem_bytes >> 30, page_size, 0, drivers);
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
    gpus: u64,
    drivers: &[&str],
) -> Vec<(String, String)> {
    let mut entries: Vec<(String, String)> = vec![
        ("arch".into(), cpu.arch().name().into()),
        ("os".into(), os.into()),
        ("cpus".into(), cpus.to_string()),
        ("mem_gib".into(), mem_gib.to_string()),
        ("page_size".into(), page_size.to_string()),
        ("gpu".into(), gpus.to_string()),
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

/// Where Linux lists the PCI functions, one directory each.
#[cfg(target_os = "linux")]
const PCI_DEVICES: &str = "/sys/bus/pci/devices";

/// The GPUs among the PCI functions listed under `devices` (laid out as
/// `/sys/bus/pci/devices`), as `kbf-caps` counts them. A missing `devices` directory
/// (a node without a PCI bus, or a sandbox without sysfs) has none.
pub fn linux_gpus(devices: &Path) -> Result<u64, DetectError> {
    let read_err = |path: &Path| {
        let path = path.to_owned();
        move |source| DetectError::ReadPci { path, source }
    };
    let entries = match std::fs::read_dir(devices) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        entries => entries.map_err(read_err(devices))?,
    };
    let mut functions = Vec::new();
    for entry in entries {
        let dir = entry.map_err(read_err(devices))?.path();
        let read = |file: &str| {
            let path = dir.join(file);
            std::fs::read_to_string(&path).map_err(read_err(&path))
        };
        functions.push((read("class")?, read("vendor")?));
    }
    kbf_caps::gpus_from_linux_pci(
        functions
            .iter()
            .map(|(class, vendor)| PciFunction { class, vendor }),
    )
    .map_err(DetectError::Pci)
}

/// The report of a Linux node from its `/proc/cpuinfo`, `/proc/meminfo` and
/// `/proc/self/smaps` text, and its GPU count (see [`linux_gpus`]).
pub fn linux_report(
    cpuinfo: &str,
    meminfo: &str,
    smaps: &str,
    gpus: u64,
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
        gpus,
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
        let r = linux_report(SKYLAKE, MEMINFO, SMAPS, 0, &["fake"]).expect("report");
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
        assert_eq!(values(&r, "gpu"), ["0"]);
    }

    fn pci_fixture(dir: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join(dir)
    }

    /// Catches: a GPU count read from the wrong files or not reported, so the
    /// scheduler never sends GPU work to the node; a node without a PCI bus failing
    /// detection instead of reporting none; and an unreadable or malformed function
    /// counted as no GPU instead of named.
    #[test]
    fn reports_the_gpus_among_the_pci_functions() {
        let four = linux_gpus(&pci_fixture("../kbf-caps/tests/fixtures/pci/gpu_server_4x"))
            .expect("fixture");
        assert_eq!(four, 4);
        let r = linux_report(SKYLAKE, MEMINFO, SMAPS, four, &["fake"]).expect("report");
        assert_eq!(values(&r, "gpu"), ["4"]);
        let none = linux_gpus(&pci_fixture("../kbf-caps/tests/fixtures/pci/cpu_server"));
        assert_eq!(none.expect("fixture"), 0);
        let no_bus = linux_gpus(&pci_fixture("tests/fixtures/pci/no-such-directory"));
        assert_eq!(no_bus.expect("no PCI bus"), 0);

        // Errors are compared as text: a guard in the test would be a branch the
        // coverage ratchet counts and no run takes.
        let error = |dir: &Path| linux_gpus(dir).expect_err("refused").to_string();
        let no_vendor = pci_fixture("tests/fixtures/pci/no_vendor");
        let text = error(&no_vendor);
        let want = format!("read {}: ", no_vendor.join("0000-00-00.0/vendor").display());
        assert!(text.starts_with(&want), "{text}");
        let malformed = pci_fixture("tests/fixtures/pci/malformed");
        assert_eq!(
            error(&malformed),
            format!(
                "parse PCI functions: {}",
                kbf_caps::ParseError::PciValue("0x3d\n".to_owned())
            )
        );
        let a_file = malformed.join("0000-00-00.0/class");
        let text = error(&a_file);
        assert!(
            text.starts_with(&format!("read {}: ", a_file.display())),
            "{text}"
        );
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
        assert_eq!(values(&r, "gpu"), ["0"]);
        assert!(!values(&r, "isa_level").is_empty());
        for (numbers, what) in [
            ("", "hw.ncpu"),
            ("x\n", "hw.ncpu"),
            ("8\n0\n", "hw.memsize"),
            ("8\n1024\n", "hw.pagesize"),
            ("8\n1024\n16384\n", "machdep.cpu.brand_string"),
            ("8\n1024\n16384\n\n", "machdep.cpu.brand_string"),
        ] {
            // Compared as text: a pattern guard would be a branch the coverage ratchet
            // counts and no run takes.
            let error = macos_report(M3_ULTRA, numbers, &[]).expect_err(numbers);
            assert_eq!(
                error.to_string(),
                format!("{SYSCTL} has no {what}"),
                "{numbers:?}"
            );
        }
        assert!(matches!(
            macos_report("hw.optional.arm64: 0\n", MAC_NUMBERS, &[]),
            Err(DetectError::Cpu(_))
        ));
    }

    /// Catches: a report the scheduler reads differently from what build tools ask
    /// for, so Mac actions (`OSFamily=Darwin`, `ISA=arm-a64`, as Bazel and Buck2 send
    /// them) never find the Mac, or Linux actions land on it. The server reads `Hello`
    /// with `NodeCaps::from_report` over these same entries; a report it refuses (an
    /// arch spelling it does not know, a repeated single-valued entry) fails here too.
    #[test]
    fn reports_satisfy_the_platforms_build_tools_send() {
        use kbf_caps::{NodeCaps, Request};
        let caps = |r: &NodeReport| {
            let entries = r.capabilities().iter();
            NodeCaps::from_report(entries.map(|c| (c.key.as_str(), c.value.as_str())))
                .expect("the server reads the report")
        };
        let mac = caps(&macos_report(M3_ULTRA, MAC_NUMBERS, &["native"]).expect("report"));
        let linux = caps(&linux_report(SKYLAKE, MEMINFO, SMAPS, 0, &["fake"]).expect("report"));
        let wants = |props: &[(&str, &str)]| {
            Request::from_platform(props.iter().copied()).expect("a platform kbf serves")
        };
        for props in [
            &[("OSFamily", "Darwin")][..],
            &[("OSFamily", "macos")],
            &[("osfamily", "darwin")],
            &[("OSFamily", "Darwin"), ("ISA", "arm-a64")],
            &[("Arch", "arm64"), ("cpu.model", "Apple M3 Ultra")],
        ] {
            let want = wants(props);
            assert!(want.matches(&mac), "the Mac does not satisfy {props:?}");
            assert!(!want.matches(&linux), "Linux satisfies {props:?}");
        }
        for props in [
            &[("OSFamily", "Linux")][..],
            &[("OSFamily", "linux"), ("ISA", "x86-64")],
        ] {
            let want = wants(props);
            assert!(want.matches(&linux), "Linux does not satisfy {props:?}");
            assert!(!want.matches(&mac), "the Mac satisfies {props:?}");
        }
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

    /// Catches a node that reports MemTotal and every CPU while its leases may use less
    /// (the scheduler would overbook a host that also runs storage), a limit that raises
    /// what was detected, and a cap read as GiB rounded up. The fixture has 2 CPUs and
    /// 376 GiB.
    #[test]
    fn a_capacity_lowers_cpus_and_memory_to_what_leases_may_use() {
        let r = linux_report(SKYLAKE, MEMINFO, SMAPS, 0, &["container"]).expect("report");
        assert_eq!(
            (values(&r, "cpus"), values(&r, "mem_gib")),
            (vec!["2"], vec!["376"])
        );
        let capped = r.clone().within(Capacity {
            cpus: Some(1),
            memory_bytes: Some((200 << 30) + (1 << 29)),
        });
        assert_eq!(values(&capped, "cpus"), ["1"]);
        assert_eq!(values(&capped, "mem_gib"), ["200"]);
        assert_ne!(capped.hash(), r.hash());
        // Every other entry is unchanged.
        let others = |r: &NodeReport| -> Vec<(String, String)> {
            r.capabilities()
                .iter()
                .filter(|c| c.key != "cpus" && c.key != "mem_gib")
                .map(|c| (c.key.clone(), c.value.clone()))
                .collect()
        };
        assert_eq!(others(&capped), others(&r));
        // A limit above the node is the node.
        let above = r.clone().within(Capacity {
            cpus: Some(1000),
            memory_bytes: Some(1 << 50),
        });
        assert_eq!(above, r);
        assert_eq!(r.clone().within(Capacity::default()), r);
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
