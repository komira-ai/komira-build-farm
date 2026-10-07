//! The M1 exit check: from the summary line a build tool prints at the end of a build,
//! what ran where, and whether a pair of builds proves the cache.
//!
//! - Bazel (`--color=no --curses=no`): `INFO: 7 processes: 5 internal, 2 remote.` and,
//!   on a rebuild, `... 2 remote cache hit.` Internal processes are Bazel's own (file
//!   writes, symlinks), not actions, and are left out.
//! - buck2: `Commands: 2 (cached: 2, remote: 0, local: 0)`.
//!
//! The rule for a pair ([`check`]): the first build ran every action on the farm, at
//! least one, and nothing anywhere else; the second build, after a clean, answered
//! every one of those actions from the remote action cache and ran nothing. A summary
//! whose counts do not add up to its total is refused rather than half read.

use std::collections::BTreeMap;
use std::fmt;

/// The build tools the check reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Tool {
    Bazel,
    Buck2,
}

/// What one build ran, by where it ran.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Spawns {
    /// Actions the farm executed.
    pub remote: u64,
    /// Actions answered from the remote action cache.
    pub cached: u64,
    /// Anything else, by the tool's own name for it (local, a sandbox, a disk cache).
    pub other: BTreeMap<String, u64>,
}

impl fmt::Display for Spawns {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "remote {}, remote cache hit {}",
            self.remote, self.cached
        )?;
        for (kind, n) in &self.other {
            write!(f, ", {kind} {n}")?;
        }
        Ok(())
    }
}

/// Why a log yields no counts.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SummaryError {
    #[error("no summary line ({0}) in the log")]
    Missing(&'static str),
    #[error("summary line {0:?} does not parse")]
    Malformed(String),
}

/// The counts from the last summary line `tool` printed in `log`.
///
/// # Errors
/// The log has no summary line, or the last one does not parse or add up.
pub fn parse(tool: Tool, log: &str) -> Result<Spawns, SummaryError> {
    match tool {
        Tool::Bazel => bazel(log),
        Tool::Buck2 => buck2(log),
    }
}

fn bazel(log: &str) -> Result<Spawns, SummaryError> {
    let (line, (total, list)) = log
        .lines()
        .rev()
        .filter_map(|l| l.trim().strip_prefix("INFO: "))
        .find_map(|l| {
            let split = l.split_once(" processes: ");
            Some(l).zip(split.or_else(|| l.split_once(" process: ")))
        })
        .ok_or(SummaryError::Missing("Bazel's `INFO: N processes: ...`"))?;
    let malformed = || SummaryError::Malformed(line.to_owned());
    let total = number(total).ok_or_else(malformed)?;
    let mut spawns = Spawns::default();
    let mut sum = 0;
    for item in list.trim_end_matches('.').split(", ") {
        let (n, kind) = item.split_once(' ').ok_or_else(malformed)?;
        let n = number(n).ok_or_else(malformed)?;
        sum += n;
        match kind {
            "internal" => {}
            "remote" => spawns.remote += n,
            "remote cache hit" => spawns.cached += n,
            other => *spawns.other.entry(other.to_owned()).or_default() += n,
        }
    }
    (sum == total).then_some(spawns).ok_or_else(malformed)
}

fn buck2(log: &str) -> Result<Spawns, SummaryError> {
    let line = log
        .lines()
        .rev()
        .find_map(|l| l.split_once("Commands: ").map(|(_, rest)| rest.trim()))
        .ok_or(SummaryError::Missing("buck2's `Commands: N (...)`"))?;
    let malformed = || SummaryError::Malformed(line.to_owned());
    // buck2 appends `. Fallback: <n>/<executed>` when commands ran a second time.
    let (counts, fallback) = line.split_once(". Fallback: ").unwrap_or((line, ""));
    let (total, list) = counts.split_once(" (").ok_or_else(malformed)?;
    let total = number(total).ok_or_else(malformed)?;
    let list = list.strip_suffix(')').ok_or_else(malformed)?;
    let mut spawns = Spawns::default();
    if let Some((n, _)) = fallback.split_once('/') {
        let n = number(n).ok_or_else(malformed)?;
        spawns.other.insert("fallback".to_owned(), n);
    }
    let mut sum = 0;
    for item in list.split(", ") {
        let (kind, n) = item.split_once(": ").ok_or_else(malformed)?;
        let n = number(n).ok_or_else(malformed)?;
        sum += n;
        match kind {
            "cached" => spawns.cached += n,
            "remote" => spawns.remote += n,
            _ if n == 0 => {}
            other => *spawns.other.entry(other.to_owned()).or_default() += n,
        }
    }
    (sum == total).then_some(spawns).ok_or_else(malformed)
}

/// A count: ASCII digits only, so `+1` or `1.5` are not read as counts.
fn number(text: &str) -> Option<u64> {
    let digits = !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    digits.then(|| text.parse().ok()).flatten()
}

/// Checks a first build and its rebuild after a clean. Returns every rule broken.
///
/// # Errors
/// One message per broken rule.
pub fn check(first: &Spawns, second: &Spawns) -> Result<(), Vec<String>> {
    let mut broken = Vec::new();
    if first.remote == 0 {
        broken.push("the first build ran no action on the farm".to_owned());
    }
    if first.cached != 0 {
        broken.push(format!(
            "the first build had {} remote cache hit(s): the cell was not fresh",
            first.cached
        ));
    }
    for (build, spawns) in [("first", first), ("second", second)] {
        if !spawns.other.is_empty() {
            broken.push(format!(
                "the {build} build ran actions outside the farm: {spawns}"
            ));
        }
    }
    if second.remote != 0 || second.cached != first.remote {
        broken.push(format!(
            "the second build answered {} of the first build's {} action(s) from the cache \
             and ran {} again",
            second.cached, first.remote, second.remote
        ));
    }
    if broken.is_empty() {
        Ok(())
    } else {
        Err(broken)
    }
}

/// Reads both builds' logs and checks them: the report to print, and whether it passed.
#[must_use]
pub fn check_logs(tool: Tool, first: &str, second: &str) -> (String, bool) {
    let parsed = parse(tool, first).and_then(|f| Ok((f, parse(tool, second)?)));
    let (first, second) = match parsed {
        Ok(pair) => pair,
        Err(e) => return (format!("FAIL {tool:?}: {e}\n"), false),
    };
    let mut report = format!("{tool:?} first build: {first}\n{tool:?} second build: {second}\n");
    let passed = match check(&first, &second) {
        Ok(()) => {
            report.push_str(&format!(
                "OK {tool:?}: {n} of {n} action(s) ran on the farm, then all were remote cache hits\n",
                n = first.remote
            ));
            true
        }
        Err(broken) => {
            for b in broken {
                report.push_str(&format!("FAIL {tool:?}: {b}\n"));
            }
            false
        }
    };
    (report, passed)
}
