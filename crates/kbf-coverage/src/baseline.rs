//! The `coverage-baseline` file: one line per workspace crate with how many of its lines
//! and branches no test runs. CI fails when either count rises.
//!
//! Each line is `<crate> <lines missed> <branches missed>`, separated by whitespace; a
//! value is a count in decimal digits, or `-` for a crate with nothing to measure.
//! Blank lines and lines starting with `#` are comments. [`render`] writes the file and
//! [`parse`] reads what it writes, refusing a crate listed twice.

use std::collections::BTreeMap;

use crate::{CrateCoverage, show};

/// One crate's recorded missed counts; `None` means nothing to measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub lines: Option<u64>,
    pub branches: Option<u64>,
}

impl Entry {
    /// The entry that records `c` as measured.
    pub fn of(c: CrateCoverage) -> Self {
        Self {
            lines: c.lines.missed(),
            branches: c.branches.missed(),
        }
    }
}

/// Crate name to its recorded missed counts.
pub type Baseline = BTreeMap<String, Entry>;

/// Why a baseline file was refused. `line` is 1-based.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BaselineError {
    #[error(
        "line {line}: expected `<crate> <lines missed> <branches missed>`, found {found} fields"
    )]
    Fields { line: usize, found: usize },
    #[error("line {line}: not a count of missed items or `-`: {value:?}")]
    Count { line: usize, value: String },
    #[error("line {line}: {krate} is listed twice")]
    Duplicate { line: usize, krate: String },
}

/// A count is decimal digits only, so `+1`, `-1` and `1.0` (an old percentage) are
/// refused rather than read as something the writer never wrote.
fn value(line: usize, s: &str) -> Result<Option<u64>, BaselineError> {
    if s == "-" {
        return Ok(None);
    }
    let bad = || BaselineError::Count {
        line,
        value: s.to_owned(),
    };
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    s.parse().map(Some).map_err(|_| bad())
}

/// Reads a baseline file.
pub fn parse(text: &str) -> Result<Baseline, BaselineError> {
    let mut out = Baseline::new();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = raw.split_whitespace().collect();
        let [krate, lines, branches] = fields[..] else {
            return Err(BaselineError::Fields {
                line,
                found: fields.len(),
            });
        };
        let entry = Entry {
            lines: value(line, lines)?,
            branches: value(line, branches)?,
        };
        if out.insert(krate.to_owned(), entry).is_some() {
            return Err(BaselineError::Duplicate {
                line,
                krate: krate.to_owned(),
            });
        }
    }
    Ok(out)
}

const HEADER: &str = "\
# Coverage ratchet, checked by the CI coverage job (crates/kbf-coverage). One line per
# workspace crate: how many of its lines and branches no test runs; `-` means the crate
# has nothing to measure. CI fails when a crate misses more than its line here, so a
# change covers the code it adds (or covers as much existing code as it leaves
# uncovered), and when it misses fewer, so the file never keeps slack. The target is 0
# everywhere. The coverage job uploads the measured file as
# `coverage-baseline.measured`; copy it here when a count changes or a crate is added.
# crate lines-missed branches-missed
";

/// Writes a baseline file, crates in name order, values aligned.
pub fn render(baseline: &Baseline) -> String {
    let width = baseline.keys().map(String::len).max().unwrap_or(0);
    let mut out = String::from(HEADER);
    for (krate, e) in baseline {
        out.push_str(&format!(
            "{krate:<width$} {:>6} {:>6}\n",
            show(e.lines),
            show(e.branches)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Counts;

    /// Catches: a writer and reader that disagree, so a baseline CI wrote would not
    /// read back as the same values (or would lose a crate).
    #[test]
    fn render_then_parse_round_trips() {
        let mut b = Baseline::new();
        b.insert(
            "kbf-types".into(),
            Entry::of(CrateCoverage {
                lines: Counts::new(8, 7).expect("valid"),
                branches: Counts::default(),
            }),
        );
        b.insert(
            "a".into(),
            Entry {
                lines: Some(0),
                branches: Some(12_345_678),
            },
        );
        let text = render(&b);
        assert!(text.starts_with("# Coverage ratchet"), "{text}");
        assert!(text.contains("\nkbf-types      1      -\n"), "{text}");
        assert!(text.contains("\na              0 12345678\n"), "{text}");
        assert_eq!(parse(&text), Ok(b));
        assert_eq!(parse(&render(&Baseline::new())), Ok(Baseline::new()));
    }

    /// Catches: comments or blank lines read as crates, or surrounding whitespace
    /// rejected.
    #[test]
    fn skips_comments_and_blank_lines() {
        let got = parse("# c\n\n   \n  # indented\n  x\t17  -  \n").expect("valid");
        assert_eq!(got.len(), 1);
        assert_eq!(
            got["x"],
            Entry {
                lines: Some(17),
                branches: None
            }
        );
    }

    /// Catches: a malformed line skipped, which would drop a crate from the ratchet; a
    /// value in another form (a signed number, or a percentage from an older file) read
    /// as a count; or a crate listed twice with the second value silently winning.
    #[test]
    fn refuses_malformed_lines() {
        assert_eq!(
            parse("x 1\n"),
            Err(BaselineError::Fields { line: 1, found: 2 })
        );
        assert_eq!(
            parse("# h\nx 1 2 3\n"),
            Err(BaselineError::Fields { line: 2, found: 4 })
        );
        // The last value is all digits but overflows a u64.
        for bad in ["97.50", "+1", "-1", "--", "1e3", "18446744073709551616"] {
            assert_eq!(
                parse(&format!("x - {bad}\n")),
                Err(BaselineError::Count {
                    line: 1,
                    value: bad.into()
                }),
                "{bad}"
            );
        }
        assert_eq!(
            parse("x 97.50 -\n"),
            Err(BaselineError::Count {
                line: 1,
                value: "97.50".into()
            })
        );
        assert_eq!(
            parse("x - -\nx - -\n"),
            Err(BaselineError::Duplicate {
                line: 2,
                krate: "x".into()
            })
        );
        let messages: Vec<String> = ["x\n", "x 1.00 -\n", "x - -\nx - -\n"]
            .iter()
            .map(|t| parse(t).expect_err(t).to_string())
            .collect();
        assert_eq!(
            messages,
            [
                "line 1: expected `<crate> <lines missed> <branches missed>`, found 1 fields",
                "line 1: not a count of missed items or `-`: \"1.00\"",
                "line 2: x is listed twice",
            ]
        );
    }
}
