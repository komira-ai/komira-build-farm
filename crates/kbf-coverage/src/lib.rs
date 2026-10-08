//! Per-crate line and branch coverage for the workspace, and the ratchet that keeps it
//! from falling.
//!
//! CI runs the workspace tests under `cargo llvm-cov --branch`, which writes an lcov
//! tracefile. This crate reads it ([`lcov`]), sums its per-file counts into one
//! [`CrateCoverage`] per directory under `crates/` ([`ratchet::by_crate`]), and compares
//! each crate with its line in the `coverage-baseline` file ([`baseline`]). The baseline
//! records how many lines and branches of each crate no test runs, and CI fails when
//! either count rises: new code must be covered, or the change must cover as much
//! existing code as it leaves uncovered. A percentage cannot enforce that, since
//! partly covered new code can still raise it. CI also fails when either count falls
//! below the file's, until the file is lowered to match, so the baseline never keeps
//! slack for a later change to spend. The table CI prints also shows each
//! crate's percentages and gap to 100%, the project's target.
//!
//! Percentages are floored to hundredths ([`Percent`]), so one uncovered line in a
//! large crate shows as 99.99% and a gap of 0.01%, never as a rounded-up 100.00%.

pub mod baseline;
pub mod lcov;
pub mod ratchet;

use std::fmt;

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

    /// How many items did not run, or `None` when there is nothing to measure.
    pub fn missed(self) -> Option<u64> {
        (self.found > 0).then(|| self.found - self.hit)
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

/// Formats an optional value as the baseline file and the table write it: `-` when
/// there is nothing to measure.
pub fn show<T: fmt::Display>(v: Option<T>) -> String {
    v.map_or_else(|| "-".to_owned(), |v| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(found: u64, hit: u64) -> Counts {
        Counts::new(found, hit).expect("valid")
    }

    fn pct(c: Counts) -> String {
        show(c.percent())
    }

    /// Catches: a percentage rounded up, which would show one uncovered line in a large
    /// crate as 100.00% with no gap.
    #[test]
    fn percent_floors_so_a_miss_is_never_hidden() {
        let c = counts(100_000, 99_999);
        assert_eq!(pct(c), "99.99");
        assert_eq!(show(c.percent().map(Percent::gap)), "0.01");
        assert_eq!(pct(counts(3, 2)), "66.66");
        assert_eq!(pct(counts(20, 1)), "5.00");
        assert_eq!(counts(4, 4).percent(), Some(Percent::FULL));
        assert_eq!(pct(counts(4, 0)), "0.00");
    }

    /// Catches: a crate with nothing to measure reported as 0% or 0 missed (values the
    /// baseline could not tell from a real measurement).
    #[test]
    fn nothing_found_has_no_percentage_and_no_miss_count() {
        assert_eq!(Counts::default().percent(), None);
        assert_eq!(Counts::default().missed(), None);
        assert_eq!(show::<Percent>(None), "-");
        assert_eq!(show(Some(7)), "7");
    }

    /// Catches: the missed count computed from the wrong field, which would let a drop
    /// in hits pass the ratchet.
    #[test]
    fn missed_is_found_minus_hit() {
        assert_eq!(counts(10, 7).missed(), Some(3));
        assert_eq!(counts(4, 4).missed(), Some(0));
        assert_eq!(counts(4, 0).missed(), Some(4));
    }

    /// Catches: counts that claim more hits than items, which would yield over 100%.
    #[test]
    fn hits_above_found_are_refused() {
        assert_eq!(Counts::new(1, 2), None);
        let c = counts(2, 1);
        assert_eq!((c.found(), c.hit()), (2, 1));
    }

    /// Catches: a sum that adds the wrong fields or wraps on overflow.
    #[test]
    fn counts_add_field_by_field_and_saturate() {
        let a = CrateCoverage {
            lines: counts(10, 4),
            branches: counts(2, 1),
        };
        let b = CrateCoverage {
            lines: counts(5, 5),
            branches: counts(3, 0),
        };
        let sum = a.plus(b);
        assert_eq!(sum.lines, counts(15, 9));
        assert_eq!(sum.branches, counts(5, 1));
        let max = counts(u64::MAX, u64::MAX);
        assert_eq!(max.plus(max), max);
        assert_eq!(max.percent(), Some(Percent::FULL));
    }
}
