//! The `coverage-baseline` file: one line per workspace crate with the line and branch
//! coverage CI must not fall below.
//!
//! Each line is `<crate> <lines> <branches>`, separated by whitespace; a value is a
//! [`Percent`] in its own two-decimal form, or `-` for a crate with nothing to measure.
//! Blank lines and lines starting with `#` are comments. [`render`] writes the file and
//! [`parse`] reads exactly what it writes, refusing a crate listed twice.

use std::collections::BTreeMap;

use crate::{BadPercent, CrateCoverage, Percent, show};

/// One crate's recorded coverage; `None` means nothing to measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub lines: Option<Percent>,
    pub branches: Option<Percent>,
}

impl Entry {
    /// The entry that records `c` as measured.
    pub fn of(c: CrateCoverage) -> Self {
        Self {
            lines: c.lines.percent(),
            branches: c.branches.percent(),
        }
    }
}

/// Crate name to its recorded coverage.
pub type Baseline = BTreeMap<String, Entry>;

/// Why a baseline file was refused. `line` is 1-based.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BaselineError {
    #[error("line {line}: expected `<crate> <lines> <branches>`, found {found} fields")]
    Fields { line: usize, found: usize },
    #[error("line {line}: {source}")]
    Percent { line: usize, source: BadPercent },
    #[error("line {line}: {krate} is listed twice")]
    Duplicate { line: usize, krate: String },
}

fn value(line: usize, s: &str) -> Result<Option<Percent>, BaselineError> {
    if s == "-" {
        return Ok(None);
    }
    s.parse()
        .map(Some)
        .map_err(|source| BaselineError::Percent { line, source })
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
# Coverage ratchet for kbf-coverage (see crates/kbf-coverage). One line per workspace
# crate: line and branch coverage in percent, floored to hundredths; `-` means the crate
# has nothing to measure. CI fails when a crate measures below its line here. The
# target is 100.00 everywhere. The coverage job uploads the measured file as
# `coverage-baseline.measured`; copy it here when coverage rises or a crate is added.
# crate lines branches
";

/// Writes a baseline file, crates in name order, values aligned.
pub fn render(baseline: &Baseline) -> String {
    let width = baseline.keys().map(String::len).max().unwrap_or(0);
    let mut out = String::from(HEADER);
    for (krate, e) in baseline {
        out.push_str(&format!(
            "{krate:<width$} {:>7} {:>8}\n",
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

    fn pct(s: &str) -> Option<Percent> {
        Some(s.parse().expect(s))
    }

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
                lines: pct("100.00"),
                branches: pct("0.00"),
            },
        );
        let text = render(&b);
        assert!(text.starts_with("# Coverage ratchet"), "{text}");
        assert!(text.contains("\nkbf-types   87.50        -\n"), "{text}");
        assert!(text.contains("\na          100.00     0.00\n"), "{text}");
        assert_eq!(parse(&text), Ok(b));
        assert_eq!(parse(&render(&Baseline::new())), Ok(Baseline::new()));
    }

    /// Catches: comments or blank lines read as crates, or surrounding whitespace
    /// rejected.
    #[test]
    fn skips_comments_and_blank_lines() {
        let got = parse("# c\n\n   \n  # indented\n  x\t1.00  -  \n").expect("valid");
        assert_eq!(got.len(), 1);
        assert_eq!(
            got["x"],
            Entry {
                lines: pct("1.00"),
                branches: None
            }
        );
    }

    /// Catches: a malformed line skipped, which would drop a crate from the ratchet,
    /// or a crate listed twice with the second value silently winning.
    #[test]
    fn refuses_malformed_lines() {
        assert_eq!(
            parse("x 1.00\n"),
            Err(BaselineError::Fields { line: 1, found: 2 })
        );
        assert_eq!(
            parse("# h\nx 1.00 2.00 3.00\n"),
            Err(BaselineError::Fields { line: 2, found: 4 })
        );
        assert_eq!(
            parse("x 1.0 -\n"),
            Err(BaselineError::Percent {
                line: 1,
                source: BadPercent("1.0".into())
            })
        );
        assert_eq!(
            parse("x - 101.00\n"),
            Err(BaselineError::Percent {
                line: 1,
                source: BadPercent("101.00".into())
            })
        );
        assert_eq!(
            parse("x - -\nx - -\n"),
            Err(BaselineError::Duplicate {
                line: 2,
                krate: "x".into()
            })
        );
        let messages: Vec<String> = ["x\n", "x 1 -\n", "x - -\nx - -\n"]
            .iter()
            .map(|t| parse(t).expect_err(t).to_string())
            .collect();
        assert_eq!(
            messages,
            [
                "line 1: expected `<crate> <lines> <branches>`, found 1 fields",
                "line 1: not a percentage with two decimals from 0.00 to 100.00: \"1\"",
                "line 2: x is listed twice",
            ]
        );
    }
}
