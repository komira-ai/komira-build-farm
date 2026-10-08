//! Time: the gate's clock, and the RFC 3339 and local date-time forms its messages use.
//!
//! Every time the gate keeps is whole seconds since the Unix epoch, UTC.

use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, PrimitiveDateTime};

/// Seconds in an hour and a day.
pub const HOUR: i64 = 3600;
pub const DAY: i64 = 24 * HOUR;

/// Where the gate reads the time. Tests drive a fake.
pub trait Clock: Send + Sync + 'static {
    /// Seconds since the Unix epoch.
    fn now(&self) -> i64;
}

/// The system clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        OffsetDateTime::now_utc().unix_timestamp()
    }
}

/// Reads an RFC 3339 time (`2026-10-08T12:00:00Z`, any offset) as seconds since the
/// epoch; `None` if it is not one.
pub fn parse_rfc3339(text: &str) -> Option<i64> {
    OffsetDateTime::parse(text, &Rfc3339)
        .ok()
        .map(OffsetDateTime::unix_timestamp)
}

/// Writes seconds since the epoch as an RFC 3339 UTC time. Out-of-range values (never
/// produced by the clock) are clamped to the epoch.
pub fn format_rfc3339(secs: i64) -> String {
    let at = OffsetDateTime::from_unix_timestamp(secs).unwrap_or(OffsetDateTime::UNIX_EPOCH);
    at.format(&Rfc3339).unwrap_or_default()
}

/// Whether `text` is a local date and time with no offset, `YYYY-MM-DDTHH:MM:SS`: the
/// form of a DDM enforcement's `TargetLocalDateTime`.
pub fn is_local_date_time(text: &str) -> bool {
    let format = time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");
    PrimitiveDateTime::parse(text, format).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_round_trips_and_refuses_other_forms() {
        // Catches: a parser that accepts a date alone, or drops the offset.
        const T: i64 = 1_800_000_000;
        assert_eq!(parse_rfc3339("2027-01-15T08:00:00Z"), Some(T));
        assert_eq!(parse_rfc3339("2027-01-15T09:00:00+01:00"), Some(T));
        assert_eq!(parse_rfc3339("2027-01-15"), None);
        assert_eq!(format_rfc3339(T + HOUR), "2027-01-15T09:00:00Z");
        // Out of range: the epoch.
        assert_eq!(format_rfc3339(i64::MAX), format_rfc3339(0));
    }

    #[test]
    fn local_date_time_has_no_offset() {
        assert!(is_local_date_time("2026-10-09T02:00:00"));
        assert!(!is_local_date_time("2026-10-09T02:00:00Z"));
        assert!(!is_local_date_time("2026-10-09"));
    }

    #[test]
    fn the_system_clock_is_after_the_epoch() {
        assert!(SystemClock.now() > 0);
    }
}
