//! Reads an lcov tracefile into one record per source file.
//!
//! Only the per-file summary lines are read: `SF:<path>` opens a record, `LF`/`LH` give
//! the lines found and hit, `BRF`/`BRH` the branches found and hit, and
//! `end_of_record` closes it. A summary line a record lacks counts as zero (llvm-cov
//! leaves the branch lines out when branch coverage is off). Every other line (`TN`,
//! `FN`, `FNDA`, `DA`, `BRDA`, ...) carries detail the summaries already total, and
//! is skipped.
//!
//! Refused, so that a damaged or merged file cannot report a wrong total: a summary
//! line outside a record, a summary line twice in one record, a count that is not a
//! decimal number, more hits than items, a record left open at the end, the same
//! source in two records, and a file with no records at all (an empty report would
//! otherwise pass every check).

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::{Counts, CrateCoverage};

/// The summary of one source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    pub path: PathBuf,
    pub coverage: CrateCoverage,
}

/// Why a tracefile was refused. `line` is 1-based.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LcovError {
    #[error("line {line}: {field} outside a record (no SF before it)")]
    OutsideRecord { line: usize, field: String },
    #[error("line {line}: SF while the record for {open} is still open")]
    Nested { line: usize, open: String },
    #[error("line {line}: {field} given twice in one record")]
    Repeated { line: usize, field: String },
    #[error("line {line}: {field} is not a count: {value:?}")]
    BadCount {
        line: usize,
        field: String,
        value: String,
    },
    #[error("line {line}: record for {path} hits {hit} of {found} {what}")]
    HitAboveFound {
        line: usize,
        path: String,
        what: &'static str,
        found: u64,
        hit: u64,
    },
    #[error("{path} appears in two records")]
    Duplicate { path: String },
    #[error("the record for {path} has no end_of_record")]
    Unterminated { path: String },
    #[error("the tracefile holds no records")]
    Empty,
}

/// A record being read: the source path and each summary field seen so far.
struct Open {
    path: String,
    /// LF, LH, BRF, BRH in that order.
    fields: [Option<u64>; 4],
}

const FIELDS: [&str; 4] = ["LF", "LH", "BRF", "BRH"];

impl Open {
    fn close(self, line: usize) -> Result<FileRecord, LcovError> {
        let [lf, lh, brf, brh] = self.fields.map(|f| f.unwrap_or(0));
        let counts = |what, found, hit| {
            Counts::new(found, hit).ok_or_else(|| LcovError::HitAboveFound {
                line,
                path: self.path.clone(),
                what,
                found,
                hit,
            })
        };
        let coverage = CrateCoverage {
            lines: counts("lines", lf, lh)?,
            branches: counts("branches", brf, brh)?,
        };
        Ok(FileRecord {
            path: PathBuf::from(&self.path),
            coverage,
        })
    }
}

/// Parses a tracefile; see the module docs for what it refuses.
pub fn parse(text: &str) -> Result<Vec<FileRecord>, LcovError> {
    let mut records = Vec::new();
    let mut seen = BTreeSet::new();
    let mut open: Option<Open> = None;
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let raw = raw.trim_end();
        if raw == "end_of_record" {
            let record = open.take().ok_or_else(|| LcovError::OutsideRecord {
                line,
                field: raw.to_owned(),
            })?;
            records.push(record.close(line)?);
            continue;
        }
        let Some((field, value)) = raw.split_once(':') else {
            continue;
        };
        if field == "SF" {
            if let Some(o) = &open {
                return Err(LcovError::Nested {
                    line,
                    open: o.path.clone(),
                });
            }
            if !seen.insert(value.to_owned()) {
                return Err(LcovError::Duplicate {
                    path: value.to_owned(),
                });
            }
            open = Some(Open {
                path: value.to_owned(),
                fields: [None; 4],
            });
            continue;
        }
        let Some(slot) = FIELDS.iter().position(|f| *f == field) else {
            continue;
        };
        let record = open.as_mut().ok_or_else(|| LcovError::OutsideRecord {
            line,
            field: field.to_owned(),
        })?;
        let count = value.parse::<u64>().map_err(|_| LcovError::BadCount {
            line,
            field: field.to_owned(),
            value: value.to_owned(),
        })?;
        if record.fields[slot].replace(count).is_some() {
            return Err(LcovError::Repeated {
                line,
                field: field.to_owned(),
            });
        }
    }
    if let Some(o) = open {
        return Err(LcovError::Unterminated { path: o.path });
    }
    if records.is_empty() {
        return Err(LcovError::Empty);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cov(lf: u64, lh: u64, brf: u64, brh: u64) -> CrateCoverage {
        CrateCoverage {
            lines: Counts::new(lf, lh).expect("valid"),
            branches: Counts::new(brf, brh).expect("valid"),
        }
    }

    /// Catches: summary fields read into the wrong slot, a detail line (`DA`, `BRDA`)
    /// counted as a summary, or a second record merged into the first.
    #[test]
    fn reads_each_record_summary() {
        let text = "TN:\nSF:/w/crates/a/src/lib.rs\nFN:1,f\nDA:1,3\nDA:2,0\nBRDA:1,0,0,1\n\
                    BRDA:1,0,1,-\nLF:2\nLH:1\nBRF:2\nBRH:1\nend_of_record\n\
                    SF:/w/crates/b/src/lib.rs\nLH:4\nLF:5\nend_of_record\n";
        let got = parse(text).expect("valid");
        assert_eq!(
            got,
            vec![
                FileRecord {
                    path: "/w/crates/a/src/lib.rs".into(),
                    coverage: cov(2, 1, 2, 1),
                },
                // No BRF/BRH: branch coverage was off for this file; zero, not an error.
                FileRecord {
                    path: "/w/crates/b/src/lib.rs".into(),
                    coverage: cov(5, 4, 0, 0),
                },
            ]
        );
    }

    /// Catches: trailing whitespace or CRLF line ends breaking `end_of_record`.
    #[test]
    fn tolerates_crlf() {
        let got = parse("SF:/w/x.rs\r\nLF:1\r\nLH:1\r\nend_of_record\r\n").expect("valid");
        assert_eq!(got[0].coverage, cov(1, 1, 0, 0));
    }

    fn err(text: &str) -> LcovError {
        parse(text).expect_err(text)
    }

    /// Catches: each malformed shape the module docs list being accepted, which would
    /// let a damaged report pass the ratchet with a wrong total.
    #[test]
    fn refuses_malformed_tracefiles() {
        let field = |f: &str| f.to_owned();
        assert_eq!(
            err("LF:3\n"),
            LcovError::OutsideRecord {
                line: 1,
                field: field("LF")
            }
        );
        assert_eq!(
            err("SF:a\nend_of_record\nend_of_record\n"),
            LcovError::OutsideRecord {
                line: 3,
                field: field("end_of_record")
            }
        );
        assert_eq!(
            err("SF:a\nSF:b\n"),
            LcovError::Nested {
                line: 2,
                open: field("a")
            }
        );
        assert_eq!(
            err("SF:a\nLH:1\nLH:1\n"),
            LcovError::Repeated {
                line: 3,
                field: field("LH")
            }
        );
        assert_eq!(
            err("SF:a\nBRF:-1\n"),
            LcovError::BadCount {
                line: 2,
                field: field("BRF"),
                value: field("-1")
            }
        );
        assert_eq!(
            err("SF:a\nLF:1\nLH:2\nend_of_record\n"),
            LcovError::HitAboveFound {
                line: 4,
                path: field("a"),
                what: "lines",
                found: 1,
                hit: 2
            }
        );
        assert_eq!(
            err("SF:a\nBRF:1\nBRH:2\nend_of_record\n"),
            LcovError::HitAboveFound {
                line: 4,
                path: field("a"),
                what: "branches",
                found: 1,
                hit: 2
            }
        );
        assert_eq!(
            err("SF:a\nend_of_record\nSF:a\nend_of_record\n"),
            LcovError::Duplicate { path: field("a") }
        );
        assert_eq!(
            err("SF:a\nLF:1\n"),
            LcovError::Unterminated { path: field("a") }
        );
        assert_eq!(err(""), LcovError::Empty);
        assert_eq!(err("TN:\nnoise\n"), LcovError::Empty);
    }

    /// Catches: an error message that drops the location or the values a reader needs.
    #[test]
    fn errors_name_the_place() {
        let messages = [
            err("LF:3\n").to_string(),
            err("SF:a\nSF:b\n").to_string(),
            err("SF:a\nLH:1\nLH:1\n").to_string(),
            err("SF:a\nBRF:x\n").to_string(),
            err("SF:a\nLF:1\nLH:2\nend_of_record\n").to_string(),
            err("SF:a\nend_of_record\nSF:a\nend_of_record\n").to_string(),
            err("SF:a\n").to_string(),
            err("").to_string(),
        ];
        assert_eq!(
            messages,
            [
                "line 1: LF outside a record (no SF before it)",
                "line 2: SF while the record for a is still open",
                "line 3: LH given twice in one record",
                "line 2: BRF is not a count: \"x\"",
                "line 4: record for a hits 2 of 1 lines",
                "a appears in two records",
                "the record for a has no end_of_record",
                "the tracefile holds no records",
            ]
        );
    }
}
