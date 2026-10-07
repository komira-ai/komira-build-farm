//! Farm time: the only notion of time a pure core sees.

use std::time::Duration;

/// A point in time, in milliseconds, supplied by the caller.
///
/// Pure cores never read a clock. The caller (the server, or the simulator with a
/// seeded virtual clock) passes the current farm time in with each input, so replaying
/// the same inputs replays the same decisions. The epoch is the caller's; values are
/// only compared and subtracted, never turned into a calendar date here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FarmTime(u64);

impl FarmTime {
    /// The farm time `millis` milliseconds after the epoch.
    #[must_use]
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis)
    }

    /// Milliseconds since the epoch.
    #[must_use]
    pub const fn as_millis(self) -> u64 {
        self.0
    }

    /// This time plus `d`, saturating at the largest representable time. Sub-millisecond
    /// parts of `d` are dropped.
    #[must_use]
    pub fn saturating_add(self, d: Duration) -> Self {
        let millis = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        Self(self.0.saturating_add(millis))
    }

    /// The time elapsed from `earlier` to `self`, or zero if `earlier` is later.
    #[must_use]
    pub fn saturating_duration_since(self, earlier: Self) -> Duration {
        Duration::from_millis(self.0.saturating_sub(earlier.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: arithmetic that wraps instead of saturating, which would turn a far
    /// deadline into one in the past, or a negative elapsed time into a huge one.
    #[test]
    fn arithmetic_saturates() {
        let t = FarmTime::from_millis(1_000);
        assert_eq!(
            t.saturating_add(Duration::from_millis(500)).as_millis(),
            1_500
        );
        assert_eq!(
            FarmTime::from_millis(u64::MAX - 1).saturating_add(Duration::from_secs(1)),
            FarmTime::from_millis(u64::MAX)
        );
        assert_eq!(
            FarmTime::from_millis(0).saturating_add(Duration::MAX),
            FarmTime::from_millis(u64::MAX)
        );
        let later = FarmTime::from_millis(1_250);
        assert_eq!(
            later.saturating_duration_since(t),
            Duration::from_millis(250)
        );
        assert_eq!(t.saturating_duration_since(later), Duration::ZERO);
        assert!(t < later);
    }
}
