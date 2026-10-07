//! Per-crate line and branch coverage for the workspace, and the ratchet that keeps it
//! from falling.
//!
//! CI runs the workspace tests under `cargo llvm-cov --branch`, which writes an lcov
//! tracefile. This crate reads it ([`lcov`]), sums its per-file counts into one
//! [`CrateCoverage`] per directory under `crates/` ([`ratchet::by_crate`]), and compares
//! each crate with its line in the `coverage-baseline` file ([`baseline`]). CI fails
//! when a crate's line or branch coverage is below its recorded value; the table it
//! prints also shows each crate's gap to 100%, the project's target.
//!
//! Percentages are floored to hundredths ([`Percent`]), so one uncovered line in a
//! large crate shows as 99.99% and a gap of 0.01%, never as a rounded-up 100.00%.

pub mod baseline;
pub mod lcov;
pub mod ratchet;

use std::fmt;
use std::str::FromStr;

/// How many items (lines or branches) a source holds and how many of them ran.
/// `hit <= found` always holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    found: u64,
    hit: u64,
}

impl Counts {
    /// `None` when `hit > found`, which no coverage tool can report.
    pub fn new(found: u64, hit: u64) -> Option<Self> {
        (hit <= found).then_some(Self { found, hit })
    }

    pub fn found(self) -> u64 {
        self.found
    }

    pub fn hit(self) -> u64 {
        self.hit
    }

    /// The sum of two counts; saturates rather than wrapping.
    #[must_use]
    pub fn plus(self, other: Self) -> Self {
        Self {
            found: self.found.saturating_add(other.found),
            hit: self.hit.saturating_add(other.hit),
        }
    }

    /// The share that ran, or `None` when there is nothing to measure.
    pub fn percent(self) -> Option<Percent> {
        if self.found == 0 {
            return None;
        }
        let hundredths = u128::from(self.hit) * 10_000 / u128::from(self.found);
        // `hit <= found`, so `hundredths <= 10_000` and the cast cannot truncate.
        Some(Percent(hundredths as u16))
    }
}

/// Line and branch counts for one crate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CrateCoverage {
    pub lines: Counts,
    pub branches: Counts,
}

impl CrateCoverage {
    #[must_use]
    pub fn plus(self, other: Self) -> Self {
        Self {
            lines: self.lines.plus(other.lines),
            branches: self.branches.plus(other.branches),
        }
    }
}

/// A percentage in hundredths, from 0.00 to 100.00, floored (never rounded up).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Percent(u16);

impl Percent {
    pub const FULL: Percent = Percent(10_000);

    /// What is left to 100%.
    #[must_use]
    pub fn gap(self) -> Percent {
        Percent(Self::FULL.0 - self.0)
    }
}

impl fmt::Display for Percent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:02}", self.0 / 100, self.0 % 100)
    }
}

/// Why a string is not a [`Percent`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a percentage with two decimals from 0.00 to 100.00: {0:?}")]
pub struct BadPercent(String);

impl FromStr for Percent {
    type Err = BadPercent;

    /// Accepts exactly the form [`Percent`]'s `Display` writes: one to three digits, a
    /// dot and two digits, at most `100.00`.
    fn from_str(s: &str) -> Result<Self, BadPercent> {
        let bad = || BadPercent(s.to_owned());
        let (whole, frac) = s.split_once('.').ok_or_else(bad)?;
        let digits = |p: &str, len: std::ops::RangeInclusive<usize>| {
            len.contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit())
        };
        if !digits(whole, 1..=3) || !digits(frac, 2..=2) {
            return Err(bad());
        }
        // Both parts are checked digit strings of at most three digits, so this cannot
        // overflow, and `value <= 10_000` after the range check fits a `u16`.
        let number = |p: &str| p.bytes().fold(0u32, |n, b| n * 10 + u32::from(b - b'0'));
        let value = number(whole) * 100 + number(frac);
        if value > u32::from(Self::FULL.0) {
            return Err(bad());
        }
        Ok(Percent(value as u16))
    }
}

/// Formats an optional percentage as the baseline file and the table write it: `-`
/// when there is nothing to measure.
pub fn show(p: Option<Percent>) -> String {
    p.map_or_else(|| "-".to_owned(), |p| p.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pct(s: &str) -> Percent {
        s.parse().expect(s)
    }

    /// Catches: a percentage rounded up, which would show one uncovered line in a large
    /// crate as 100.00% with no gap.
    #[test]
    fn percent_floors_so_a_miss_is_never_hidden() {
        let c = Counts::new(100_000, 99_999).expect("valid");
        assert_eq!(c.percent(), Some(pct("99.99")));
        assert_eq!(c.percent().map(Percent::gap), Some(pct("0.01")));
        assert_eq!(
            Counts::new(3, 2).expect("valid").percent(),
            Some(pct("66.66"))
        );
        assert_eq!(
            Counts::new(4, 4).expect("valid").percent(),
            Some(Percent::FULL)
        );
        assert_eq!(
            Counts::new(4, 0).expect("valid").percent(),
            Some(pct("0.00"))
        );
    }

    /// Catches: a crate with nothing to measure reported as 0% (a false drop) or 100%
    /// (a value the baseline could not tell from a real one).
    #[test]
    fn nothing_found_has_no_percentage() {
        assert_eq!(Counts::default().percent(), None);
        assert_eq!(show(None), "-");
        assert_eq!(show(Some(pct("7.50"))), "7.50");
    }

    /// Catches: counts that claim more hits than items, which would yield over 100%.
    #[test]
    fn hits_above_found_are_refused() {
        assert_eq!(Counts::new(1, 2), None);
        let c = Counts::new(2, 1).expect("valid");
        assert_eq!((c.found(), c.hit()), (2, 1));
    }

    /// Catches: a sum that adds the wrong fields or wraps on overflow.
    #[test]
    fn counts_add_field_by_field_and_saturate() {
        let a = CrateCoverage {
            lines: Counts::new(10, 4).expect("valid"),
            branches: Counts::new(2, 1).expect("valid"),
        };
        let b = CrateCoverage {
            lines: Counts::new(5, 5).expect("valid"),
            branches: Counts::new(3, 0).expect("valid"),
        };
        let sum = a.plus(b);
        assert_eq!(sum.lines, Counts::new(15, 9).expect("valid"));
        assert_eq!(sum.branches, Counts::new(5, 1).expect("valid"));
        let max = Counts::new(u64::MAX, u64::MAX).expect("valid");
        assert_eq!(max.plus(max), max);
        assert_eq!(max.percent(), Some(Percent::FULL));
    }

    /// Catches: a baseline value read in a form the writer never produces, or out of
    /// range, so two spellings of one value could disagree.
    #[test]
    fn percent_parses_only_its_own_format() {
        for ok in ["0.00", "5.07", "99.99", "100.00"] {
            assert_eq!(pct(ok).to_string(), ok);
        }
        for bad in [
            "", "100", "1.5", "1.500", ".50", "1000.00", "100.01", "999.99", "-1.00", "1,00",
            "a.00", "1.0a", "+1.00",
        ] {
            assert_eq!(
                bad.parse::<Percent>(),
                Err(BadPercent(bad.to_owned())),
                "{bad}"
            );
        }
        assert_eq!(
            BadPercent("x".into()).to_string(),
            "not a percentage with two decimals from 0.00 to 100.00: \"x\""
        );
    }
}
