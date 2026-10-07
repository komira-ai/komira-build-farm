//! Per-crate totals, the comparison with the baseline, and the table CI prints.
//!
//! A crate is a directory under `<root>/crates` holding a `Cargo.toml` (the workspace's
//! `members = ["crates/*"]`). A crate whose code no test binary instruments (an empty
//! skeleton) has no records and nothing to measure; it is still listed, as `-`.
//!
//! A crate fails the ratchet when, for lines or branches, it measures below its
//! baseline value, it has no data where the baseline has a value (its code moved out
//! of every test binary, or the report lost it), or the baseline says `-` and it now
//! measures below 100% (new code that tests do not fully cover). A workspace crate
//! missing from the baseline, and a baseline crate missing from the workspace, fail
//! too, so the file always names exactly the workspace's crates. A crate that measures
//! above its baseline passes and is marked so the baseline can be raised.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::io;
use std::path::{Path, PathBuf};

use crate::baseline::{Baseline, Entry};
use crate::lcov::FileRecord;
use crate::{Counts, CrateCoverage, Percent, show};

/// The workspace's crates: the directories under `<root>/crates` with a `Cargo.toml`.
pub fn workspace_crates(root: &Path) -> io::Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for entry in std::fs::read_dir(root.join("crates"))? {
        let entry = entry?;
        if entry.path().join("Cargo.toml").is_file() {
            out.insert(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(out)
}

/// A tracefile source that is not inside a workspace crate.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not under <root>/crates/<crate>/ for any workspace crate")]
pub struct Unattributed(pub PathBuf);

/// Sums the records per crate. Every crate in `crates` gets an entry, zero when no
/// record names it; a record outside every crate is an error rather than dropped.
pub fn by_crate(
    root: &Path,
    crates: &BTreeSet<String>,
    files: &[FileRecord],
) -> Result<BTreeMap<String, CrateCoverage>, Unattributed> {
    let base = root.join("crates");
    let mut out: BTreeMap<String, CrateCoverage> = crates
        .iter()
        .map(|c| (c.clone(), CrateCoverage::default()))
        .collect();
    for f in files {
        let total = f
            .path
            .strip_prefix(&base)
            .ok()
            .filter(|rest| rest.components().count() > 1)
            .and_then(|rest| rest.iter().next())
            .and_then(|name| out.get_mut(name.to_str()?));
        let Some(total) = total else {
            return Err(Unattributed(f.path.clone()));
        };
        *total = total.plus(f.coverage);
    }
    Ok(out)
}

/// Lines or branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Lines,
    Branches,
}

impl fmt::Display for Metric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Metric::Lines => "lines",
            Metric::Branches => "branches",
        })
    }
}

/// How one crate stands against the baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Measured exactly as recorded.
    Ok,
    /// At or above the baseline everywhere and above it somewhere: passes; the
    /// baseline can be raised.
    Above,
    /// Below the baseline for these metrics: fails.
    Below(Vec<Metric>),
    /// A workspace crate the baseline does not list: fails.
    Unrecorded,
    /// A baseline crate the workspace does not have: fails.
    Stale,
}

impl Status {
    pub fn fails(&self) -> bool {
        matches!(self, Status::Below(_) | Status::Unrecorded | Status::Stale)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Status::Ok => f.write_str("ok"),
            Status::Above => f.write_str("above baseline: raise it"),
            Status::Below(m) => {
                let m: Vec<String> = m.iter().map(Metric::to_string).collect();
                write!(f, "BELOW BASELINE ({})", m.join(", "))
            }
            Status::Unrecorded => f.write_str("NOT IN BASELINE"),
            Status::Stale => f.write_str("NOT IN WORKSPACE"),
        }
    }
}

/// True when `measured` fails a recorded value; see the module docs.
fn below(recorded: Option<Percent>, measured: Option<Percent>) -> bool {
    match (recorded, measured) {
        (Some(r), Some(m)) => m < r,
        (Some(_), None) => true,
        (None, Some(m)) => m < Percent::FULL,
        (None, None) => false,
    }
}

/// One crate: what was measured (`None` if it is not in the workspace) and what the
/// baseline records (`None` if it is not listed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub krate: String,
    pub measured: Option<CrateCoverage>,
    pub recorded: Option<Entry>,
}

impl Row {
    pub fn status(&self) -> Status {
        let (Some(m), Some(r)) = (self.measured, self.recorded) else {
            return match self.measured {
                Some(_) => Status::Unrecorded,
                None => Status::Stale,
            };
        };
        let now = Entry::of(m);
        let mut failed = Vec::new();
        if below(r.lines, now.lines) {
            failed.push(Metric::Lines);
        }
        if below(r.branches, now.branches) {
            failed.push(Metric::Branches);
        }
        if !failed.is_empty() {
            Status::Below(failed)
        } else if now != r {
            Status::Above
        } else {
            Status::Ok
        }
    }
}

/// Every crate in the workspace or the baseline, in name order.
pub fn rows(measured: &BTreeMap<String, CrateCoverage>, baseline: &Baseline) -> Vec<Row> {
    let names: BTreeSet<&String> = measured.keys().chain(baseline.keys()).collect();
    names
        .into_iter()
        .map(|k| Row {
            krate: k.clone(),
            measured: measured.get(k).copied(),
            recorded: baseline.get(k).copied(),
        })
        .collect()
}

/// `hit/found`, the percentage and the gap to 100% for one metric.
fn cells(c: Option<Counts>) -> [String; 3] {
    let Some(c) = c else {
        return ["-".into(), "-".into(), "-".into()];
    };
    let p = c.percent();
    [
        format!("{}/{}", c.hit(), c.found()),
        show(p),
        show(p.map(Percent::gap)),
    ]
}

/// The report as a Markdown table (readable in a log, rendered in a job summary),
/// with a workspace total and a closing verdict line.
pub fn render(rows: &[Row]) -> String {
    let mut out = String::new();
    out.push_str(
        "| crate | lines | line % | line gap to 100% | branches | branch % | branch gap to 100% | baseline lines / branches | status |\n\
         |---|---:|---:|---:|---:|---:|---:|---:|---|\n",
    );
    let mut line = |name: &str, m: Option<CrateCoverage>, recorded: String, status: String| {
        let [l, lp, lg] = cells(m.map(|m| m.lines));
        let [b, bp, bg] = cells(m.map(|m| m.branches));
        // Writing to a String cannot fail.
        let _ = writeln!(
            out,
            "| {name} | {l} | {lp} | {lg} | {b} | {bp} | {bg} | {recorded} | {status} |"
        );
    };
    let mut total = CrateCoverage::default();
    let mut failing = 0;
    for r in rows {
        let recorded = r.recorded.map_or_else(
            || "-".to_owned(),
            |e| format!("{} / {}", show(e.lines), show(e.branches)),
        );
        let status = r.status();
        failing += usize::from(status.fails());
        total = total.plus(r.measured.unwrap_or_default());
        line(&r.krate, r.measured, recorded, status.to_string());
    }
    line("**workspace**", Some(total), String::new(), String::new());
    let _ = write!(
        out,
        "\n{}\n",
        if failing == 0 {
            "coverage ratchet: PASS".to_owned()
        } else {
            format!("coverage ratchet: FAIL ({failing} crate(s) need attention; see status)")
        }
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(found: u64, hit: u64) -> Counts {
        Counts::new(found, hit).expect("valid")
    }

    fn cov(lf: u64, lh: u64, brf: u64, brh: u64) -> CrateCoverage {
        CrateCoverage {
            lines: counts(lf, lh),
            branches: counts(brf, brh),
        }
    }

    fn pct(s: &str) -> Option<Percent> {
        Some(s.parse().expect(s))
    }

    fn rec(lines: Option<Percent>, branches: Option<Percent>) -> Option<Entry> {
        Some(Entry { lines, branches })
    }

    fn row(measured: Option<CrateCoverage>, recorded: Option<Entry>) -> Row {
        Row {
            krate: "k".into(),
            measured,
            recorded,
        }
    }

    fn file(path: &str, c: CrateCoverage) -> FileRecord {
        FileRecord {
            path: path.into(),
            coverage: c,
        }
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    /// Catches: records summed into the wrong crate, a crate without records left out
    /// of the table, or a source outside every crate silently dropped.
    #[test]
    fn sums_records_per_crate() {
        let crates = set(&["a", "b", "empty"]);
        let files = [
            file("/w/crates/a/src/lib.rs", cov(10, 9, 4, 3)),
            file("/w/crates/a/src/x/y.rs", cov(5, 5, 0, 0)),
            file("/w/crates/b/src/main.rs", cov(2, 0, 2, 0)),
        ];
        let got = by_crate(Path::new("/w"), &crates, &files).expect("attributed");
        assert_eq!(got["a"], cov(15, 14, 4, 3));
        assert_eq!(got["b"], cov(2, 0, 2, 0));
        assert_eq!(got["empty"], CrateCoverage::default());
        assert_eq!(got.len(), 3);
        for outside in [
            "/w/src/lib.rs",           // not under crates/
            "/v/crates/a/src/lib.rs",  // another root
            "/w/crates/zz/src/lib.rs", // not a workspace crate
            "/w/crates/a",             // the crate directory itself, not a file in it
        ] {
            assert_eq!(
                by_crate(Path::new("/w"), &crates, &[file(outside, cov(1, 1, 0, 0))]),
                Err(Unattributed(outside.into())),
            );
        }
        assert_eq!(
            Unattributed("/w/x.rs".into()).to_string(),
            "/w/x.rs is not under <root>/crates/<crate>/ for any workspace crate"
        );
    }

    /// Catches: each rule of the ratchet inverted or dropped (module docs): a drop
    /// passing, data lost passing, uncovered new code passing under `-`, a rise
    /// failing, and an unlisted or stale crate passing.
    #[test]
    fn status_follows_the_ratchet_rules() {
        let half = cov(2, 1, 4, 2); // 50.00 / 50.00
        let at = |l: &str, b: &str| rec(pct(l), pct(b));
        assert_eq!(row(Some(half), at("50.00", "50.00")).status(), Status::Ok);
        assert_eq!(
            row(Some(half), at("49.99", "50.00")).status(),
            Status::Above
        );
        assert_eq!(row(Some(half), at("50.00", "0.00")).status(), Status::Above);
        assert_eq!(
            row(Some(half), at("50.01", "50.00")).status(),
            Status::Below(vec![Metric::Lines])
        );
        assert_eq!(
            row(Some(half), at("50.00", "50.01")).status(),
            Status::Below(vec![Metric::Branches])
        );
        assert_eq!(
            row(Some(half), at("100.00", "100.00")).status(),
            Status::Below(vec![Metric::Lines, Metric::Branches])
        );
        // Data lost: the baseline has values, the crate now has nothing measured.
        let none = CrateCoverage::default();
        assert_eq!(
            row(Some(none), at("10.00", "10.00")).status(),
            Status::Below(vec![Metric::Lines, Metric::Branches])
        );
        // `-` recorded: new code must be fully covered to pass.
        assert_eq!(row(Some(none), rec(None, None)).status(), Status::Ok);
        assert_eq!(
            row(Some(cov(1, 1, 0, 0)), rec(None, None)).status(),
            Status::Above
        );
        assert_eq!(
            row(Some(cov(3, 2, 0, 0)), rec(None, None)).status(),
            Status::Below(vec![Metric::Lines])
        );
        assert_eq!(row(Some(half), None).status(), Status::Unrecorded);
        assert_eq!(row(None, at("1.00", "1.00")).status(), Status::Stale);
    }

    /// Catches: a failing status reported as passing (or the reverse), which decides
    /// the job's exit code.
    #[test]
    fn only_problems_fail() {
        assert!(!Status::Ok.fails());
        assert!(!Status::Above.fails());
        assert!(Status::Below(vec![Metric::Lines]).fails());
        assert!(Status::Unrecorded.fails());
        assert!(Status::Stale.fails());
    }

    /// Catches: a crate on one side only (workspace or baseline) left out of the rows.
    #[test]
    fn rows_cover_workspace_and_baseline() {
        let measured: BTreeMap<String, CrateCoverage> = [
            ("a".to_owned(), cov(1, 1, 0, 0)),
            ("b".to_owned(), cov(0, 0, 0, 0)),
        ]
        .into();
        let baseline: Baseline = [
            (
                "b".to_owned(),
                Entry {
                    lines: None,
                    branches: None,
                },
            ),
            (
                "c".to_owned(),
                Entry {
                    lines: None,
                    branches: None,
                },
            ),
        ]
        .into();
        let got = rows(&measured, &baseline);
        let names: Vec<&str> = got.iter().map(|r| r.krate.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        assert_eq!(got[0].recorded, None);
        assert_eq!(got[2].measured, None);
        assert_eq!(got[1].status(), Status::Ok);
    }

    /// Catches: a table that misplaces a column, hides the gap to 100%, totals the
    /// wrong rows or prints PASS while a crate fails.
    #[test]
    fn renders_the_table() {
        let rows = [
            row(Some(cov(4, 3, 2, 1)), rec(pct("75.00"), pct("50.00"))),
            Row {
                krate: "gone".into(),
                measured: None,
                recorded: rec(None, None),
            },
        ];
        let text = render(&rows);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[2],
            "| k | 3/4 | 75.00 | 25.00 | 1/2 | 50.00 | 50.00 | 75.00 / 50.00 | ok |"
        );
        assert_eq!(
            lines[3],
            "| gone | - | - | - | - | - | - | - / - | NOT IN WORKSPACE |"
        );
        assert_eq!(
            lines[4],
            "| **workspace** | 3/4 | 75.00 | 25.00 | 1/2 | 50.00 | 50.00 |  |  |"
        );
        assert_eq!(
            lines[6],
            "coverage ratchet: FAIL (1 crate(s) need attention; see status)"
        );
        let pass = render(&rows[..1]);
        assert!(pass.ends_with("\ncoverage ratchet: PASS\n"), "{pass}");
        let empty = render(&[row(Some(CrateCoverage::default()), None)]);
        assert!(
            empty.contains("| k | 0/0 | - | - | 0/0 | - | - | - | NOT IN BASELINE |"),
            "{empty}"
        );
    }

    /// Catches: a status label that does not say which metric fell.
    #[test]
    fn status_labels() {
        let labels: Vec<String> = [
            Status::Ok,
            Status::Above,
            Status::Below(vec![Metric::Lines, Metric::Branches]),
            Status::Unrecorded,
            Status::Stale,
        ]
        .iter()
        .map(Status::to_string)
        .collect();
        assert_eq!(
            labels,
            [
                "ok",
                "above baseline: raise it",
                "BELOW BASELINE (lines, branches)",
                "NOT IN BASELINE",
                "NOT IN WORKSPACE",
            ]
        );
    }
}
