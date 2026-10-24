//! What a running guest looks like from the host, without asking the guest anything.
//!
//! A guest that executes spends CPU in the process that hosts it and writes blocks to
//! its disk image; one that has halted or never started does neither. The sampler reads
//! the process table (the helper and Virtualization.framework's own VM service
//! processes) and the clone's `disk.img` allocation, so a silent guest can be told apart
//! from a stopped one.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use serde_json::{Value, json};

/// The command name of the process that runs a guest's CPUs. It is a service of the
/// framework, not a child of the helper, so it is found by name.
pub const VM_SERVICE: &str = "com.apple.Virtualization.VirtualMachine";

/// One process table row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub pid: u32,
    /// CPU time used so far, in milliseconds.
    pub cpu_ms: u64,
    pub command: String,
}

/// Reads a `ps` CPU time: `M:SS.hh`, `H:MM:SS` or `D-HH:MM:SS`, in milliseconds.
#[must_use]
pub fn parse_cpu_time(text: &str) -> Option<u64> {
    let (days, rest) = match text.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, text),
    };
    let (whole, frac) = match rest.split_once('.') {
        Some((w, f)) => (w, f),
        None => (rest, ""),
    };
    let mut seconds = 0u64;
    let parts: Vec<&str> = whole.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    for p in &parts {
        seconds = seconds * 60 + p.parse::<u64>().ok()?;
    }
    // Fraction digits are a decimal: ".5" is 500 ms, ".05" is 50 ms, ".123" is 123 ms.
    let millis = if frac.is_empty() {
        0
    } else {
        let digits: String = frac.chars().chain("00".chars()).take(3).collect();
        digits.parse::<u64>().ok()?
    };
    Some((days * 86_400 + seconds) * 1000 + millis)
}

/// Reads the output of `ps -axo pid=,cputime=,comm=`. A line that does not parse is
/// left out.
#[must_use]
pub fn parse_table(text: &str) -> Vec<Row> {
    text.lines()
        .filter_map(|line| {
            let mut it = line.trim_start().splitn(3, char::is_whitespace);
            let pid = it.next()?.parse().ok()?;
            let cpu_ms = parse_cpu_time(it.next()?)?;
            // The command may hold spaces: it is the rest of the line.
            let command = it.next()?.trim().to_owned();
            Some(Row {
                pid,
                cpu_ms,
                command,
            })
        })
        .collect()
}

/// The disk image's allocation: 512-byte blocks actually allocated, and the last change.
#[must_use]
pub fn disk_state(disk: &Path) -> Option<(u64, i64)> {
    std::fs::symlink_metadata(disk)
        .ok()
        .map(|m| (m.blocks(), m.mtime()))
}

/// One look at one guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// Milliseconds since the helper was spawned.
    pub at_ms: u64,
    /// CPU time of the helper process; `None` when it is not in the table (gone).
    pub helper_cpu_ms: Option<u64>,
    /// Every VM service process: `(pid, cpu_ms)`. With two guests running, the service
    /// whose CPU rises belongs to the guest that executes.
    pub services: Vec<(u32, u64)>,
    pub disk_blocks: Option<u64>,
    pub disk_mtime: Option<i64>,
}

impl Sample {
    /// Takes a sample from a process table text and a disk image.
    #[must_use]
    pub fn take(table: &str, helper_pid: u32, disk: &Path, at_ms: u64) -> Self {
        let rows = parse_table(table);
        let disk = disk_state(disk);
        Self {
            at_ms,
            helper_cpu_ms: rows.iter().find(|r| r.pid == helper_pid).map(|r| r.cpu_ms),
            services: rows
                .iter()
                .filter(|r| r.command.contains(VM_SERVICE))
                .map(|r| (r.pid, r.cpu_ms))
                .collect(),
            disk_blocks: disk.map(|d| d.0),
            disk_mtime: disk.map(|d| d.1),
        }
    }

    #[must_use]
    pub fn json(&self) -> Value {
        json!({
            "at_ms": self.at_ms,
            "helper_cpu_ms": self.helper_cpu_ms,
            "vm_services": self.services.iter().map(|(pid, cpu)| json!({"pid": pid, "cpu_ms": cpu})).collect::<Vec<_>>(),
            "disk_blocks_512": self.disk_blocks,
            "disk_mtime_unix": self.disk_mtime,
        })
    }
}

/// What a series of samples says, in numbers an operator can compare between guests.
#[must_use]
pub fn summarize(samples: &[Sample]) -> Value {
    let first = samples.first();
    let last = samples.last();
    let blocks = |s: Option<&Sample>| s.and_then(|s| s.disk_blocks);
    let grown = blocks(last)
        .zip(blocks(first))
        .map(|(l, f)| l.saturating_sub(f));
    let helper = last
        .and_then(|s| s.helper_cpu_ms)
        .zip(first.and_then(|s| s.helper_cpu_ms))
        .map(|(l, f)| l.saturating_sub(f));
    // The most CPU any VM service process gained between two samples.
    let mut service_gain = 0u64;
    for pair in samples.windows(2) {
        for (pid, now) in &pair[1].services {
            if let Some((_, before)) = pair[0].services.iter().find(|(p, _)| p == pid) {
                service_gain = service_gain.max(now.saturating_sub(*before));
            }
        }
    }
    // The last sample at which the disk was still growing.
    let last_growth_ms = samples
        .windows(2)
        .filter(|w| w[1].disk_blocks > w[0].disk_blocks)
        .map(|w| w[1].at_ms)
        .next_back();
    json!({
        "samples": samples.len(),
        "disk_blocks_512_grown": grown,
        "disk_last_growth_ms": last_growth_ms,
        "helper_cpu_ms_gained": helper,
        "vm_service_max_cpu_ms_gained_between_samples": service_gain,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::tmp;

    /// Catches a CPU-time reader that mistakes the fraction (".5" is half a second, not
    /// five milliseconds), drops hours or days, or accepts text that is not a time.
    #[test]
    fn cpu_times_are_read_in_milliseconds() {
        for (text, ms) in [
            ("0:00.00", 0),
            ("0:01.50", 1500),
            ("0:01.5", 1500),
            ("0:01.05", 1050),
            ("0:01.123", 1123),
            ("2:03", 123_000),
            ("1:02:03", 3_723_000),
            ("1:02:03.25", 3_723_250),
            ("2-01:00:00", 2 * 86_400_000 + 3_600_000),
        ] {
            assert_eq!(parse_cpu_time(text), Some(ms), "{text}");
        }
        for bad in [
            "",
            "12",
            "a:b",
            "1:2:3:4",
            "x-1:00:00",
            "0:01.zz",
            "1:00.5x",
        ] {
            assert_eq!(parse_cpu_time(bad), None, "{bad}");
        }
    }

    const TABLE: &str = "    1 0:05.00 /sbin/launchd\n  \
        4242 1:00.25 /usr/local/kbf-vmm/kbf-vmm\n \
        900 0:10.10 /System/Library/Frameworks/Virtualization.framework/Versions/A/XPCServices/com.apple.Virtualization.VirtualMachine.xpc/Contents/MacOS/com.apple.Virtualization.VirtualMachine\n\
        901 0:00.50 /Applications/Some App/helper with spaces\n\
        junk line\n\
        12 notatime /x\n\
        13 0:01.00\n";

    /// Catches a table reader that loses a command with spaces, keeps a line that is not a
    /// row, or reads the wrong column.
    #[test]
    fn the_process_table_is_read_row_by_row() {
        let rows = parse_table(TABLE);
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows[1],
            Row {
                pid: 4242,
                cpu_ms: 60_250,
                command: "/usr/local/kbf-vmm/kbf-vmm".into()
            }
        );
        assert_eq!(rows[3].command, "/Applications/Some App/helper with spaces");
    }

    /// Catches a sample that mixes up the helper with another process, misses the VM
    /// services, or reports a disk that is not there as zero blocks.
    #[test]
    fn a_sample_has_the_helper_the_vm_services_and_the_disk() {
        let dir = tmp("sampler-take");
        let disk = dir.join("disk.img");
        std::fs::write(&disk, vec![1u8; 8192]).expect("disk");
        let s = Sample::take(TABLE, 4242, &disk, 10_000);
        assert_eq!(s.at_ms, 10_000);
        assert_eq!(s.helper_cpu_ms, Some(60_250));
        assert_eq!(s.services, vec![(900, 10_100)]);
        assert!(s.disk_blocks.expect("blocks") >= 16);
        assert!(s.disk_mtime.expect("mtime") > 0);
        let gone = Sample::take(TABLE, 5, &dir.join("missing"), 0);
        assert_eq!(gone.helper_cpu_ms, None);
        assert_eq!((gone.disk_blocks, gone.disk_mtime), (None, None));
        let j = s.json();
        assert_eq!(j["vm_services"][0]["pid"], 900);
        assert_eq!(j["helper_cpu_ms"], 60_250);
        assert!(j["disk_blocks_512"].as_u64().is_some());
        assert!(gone.json()["disk_blocks_512"].is_null());
    }

    fn sample(
        at_ms: u64,
        helper: Option<u64>,
        services: &[(u32, u64)],
        blocks: Option<u64>,
    ) -> Sample {
        Sample {
            at_ms,
            helper_cpu_ms: helper,
            services: services.to_vec(),
            disk_blocks: blocks,
            disk_mtime: None,
        }
    }

    /// Catches a summary that cannot tell a guest that executes (CPU and writes rising)
    /// from one that is flat, and one that reports growth for a disk that shrank or was
    /// never read.
    #[test]
    fn the_summary_tells_an_executing_guest_from_a_flat_one() {
        let busy = [
            sample(0, Some(100), &[(9, 1000)], Some(100)),
            sample(10_000, Some(400), &[(9, 6000)], Some(900)),
            sample(20_000, Some(500), &[(9, 9000), (10, 5)], Some(900)),
        ];
        let s = summarize(&busy);
        assert_eq!(s["samples"], 3);
        assert_eq!(s["disk_blocks_512_grown"], 800);
        assert_eq!(s["disk_last_growth_ms"], 10_000);
        assert_eq!(s["helper_cpu_ms_gained"], 400);
        assert_eq!(s["vm_service_max_cpu_ms_gained_between_samples"], 5000);
        let flat = [
            sample(0, Some(100), &[(9, 1000)], Some(100)),
            sample(10_000, Some(100), &[(9, 1000)], Some(100)),
        ];
        let f = summarize(&flat);
        assert_eq!(f["disk_blocks_512_grown"], 0);
        assert!(f["disk_last_growth_ms"].is_null());
        assert_eq!(f["vm_service_max_cpu_ms_gained_between_samples"], 0);
        let shrunk = [
            sample(0, None, &[], Some(500)),
            sample(1, None, &[], Some(100)),
        ];
        let s = summarize(&shrunk);
        assert_eq!(s["disk_blocks_512_grown"], 0);
        assert!(s["helper_cpu_ms_gained"].is_null());
        let none = summarize(&[]);
        assert_eq!(none["samples"], 0);
        assert!(none["disk_blocks_512_grown"].is_null());
        let unread = [sample(0, None, &[], None), sample(1, None, &[], None)];
        assert!(summarize(&unread)["disk_blocks_512_grown"].is_null());
    }
}
