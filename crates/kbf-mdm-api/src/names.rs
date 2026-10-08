//! Values the server and the gate check the same way.
//!
//! **Only kbf's own declarations** (`mdm-backend.md` section M2.2): every declaration
//! the gate creates is named `kbf.<...>`, and the gate changes or removes nothing
//! else. [`is_kbf_declaration`] is that test: a prefix, never a substring, so
//! `com.example.kbf.osupdate` is not kbf's. A serial is ASCII letters and digits only,
//! so `kbf.osupdate.<serial>` names exactly one Mac and cannot be stretched into
//! another identifier.

use std::cmp::Ordering;
use std::fmt;

/// The prefix of every declaration the gate may create, change or withdraw.
pub const DECLARATION_PREFIX: &str = "kbf.";

/// The prefix of a Mac's enforcement declaration, before its serial.
pub const OSUPDATE_PREFIX: &str = "kbf.osupdate.";

/// A value that is not in its documented form.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    /// Not 1 to 32 ASCII letters and digits.
    #[error("serial {0:?} is not 1 to 32 ASCII letters and digits")]
    Serial(String),
    /// Not `YYYY-MM-DDTHH:MM:SS`, or not a real time.
    #[error("{0:?} is not a local date-time YYYY-MM-DDTHH:MM:SS")]
    LocalDateTime(String),
    /// Not `YYYY-MM-DD`, or not a real date.
    #[error("{0:?} is not a date YYYY-MM-DD")]
    Date(String),
    /// Not 64 lower-case hex digits.
    #[error("{0:?} is not a SHA-256 in lower-case hex")]
    Sha256(String),
    /// Not 1 to 4 dot-separated decimal numbers.
    #[error("{0:?} is not a macOS version")]
    Version(String),
}

/// A Mac's hardware serial: 1 to 32 ASCII letters and digits.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Serial(String);

impl Serial {
    /// `s`, if it is 1 to 32 ASCII letters and digits.
    ///
    /// # Errors
    /// [`NameError::Serial`] otherwise.
    pub fn new(s: impl Into<String>) -> Result<Self, NameError> {
        let s = s.into();
        if (1..=32).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric()) {
            Ok(Self(s))
        } else {
            Err(NameError::Serial(s))
        }
    }

    /// The serial as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Serial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The identifier of `serial`'s enforcement declaration: `kbf.osupdate.<serial>`.
#[must_use]
pub fn osupdate_declaration(serial: &Serial) -> String {
    format!("{OSUPDATE_PREFIX}{serial}")
}

/// Whether `identifier` is one of kbf's declarations: it starts with `kbf.` and names
/// something after it.
#[must_use]
pub fn is_kbf_declaration(identifier: &str) -> bool {
    identifier.len() > DECLARATION_PREFIX.len() && identifier.starts_with(DECLARATION_PREFIX)
}

/// Whether `build` is a macOS build such as `26A434` or `25G241a`: 1 to 32 ASCII
/// letters and digits.
#[must_use]
pub fn is_build(build: &str) -> bool {
    (1..=32).contains(&build.len()) && build.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// A calendar date, as Apple's catalogue writes it: `YYYY-MM-DD`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    year: i64,
    month: u32,
    day: u32,
}

impl Date {
    /// Parses `YYYY-MM-DD`.
    ///
    /// # Errors
    /// [`NameError::Date`] if `s` is not in that form or names no real day.
    pub fn parse(s: &str) -> Result<Self, NameError> {
        parse_date(s.as_bytes()).ok_or_else(|| NameError::Date(s.to_owned()))
    }

    /// The day `days` after 1970-01-01 (before it, if negative).
    #[must_use]
    pub fn from_days_since_epoch(days: i64) -> Self {
        let (year, month, day) = civil_from_days(days);
        Self { year, month, day }
    }

    /// Days from 1970-01-01 to this date.
    #[must_use]
    pub fn days_since_epoch(self) -> i64 {
        days_from_civil(self.year, self.month, self.day)
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// A `TargetLocalDateTime`: `YYYY-MM-DDTHH:MM:SS` in the Mac's local time, no zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalDateTime {
    date: Date,
    hour: u32,
    minute: u32,
    second: u32,
}

impl LocalDateTime {
    /// Parses `YYYY-MM-DDTHH:MM:SS`.
    ///
    /// # Errors
    /// [`NameError::LocalDateTime`] if `s` is not in that form or names no real time.
    pub fn parse(s: &str) -> Result<Self, NameError> {
        let err = || NameError::LocalDateTime(s.to_owned());
        let b = s.as_bytes();
        if b.len() != 19 || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
            return Err(err());
        }
        let date = parse_date(&b[..10]).ok_or_else(err)?;
        let hour = number(&b[11..13]).filter(|h| *h < 24).ok_or_else(err)?;
        let minute = number(&b[14..16]).filter(|m| *m < 60).ok_or_else(err)?;
        let second = number(&b[17..19]).filter(|s| *s < 60).ok_or_else(err)?;
        Ok(Self {
            date,
            hour,
            minute,
            second,
        })
    }

    /// The local time at `unix_seconds` in a zone `utc_offset_seconds` east of UTC
    /// (negative: west).
    #[must_use]
    pub fn from_unix_seconds(unix_seconds: i64, utc_offset_seconds: i32) -> Self {
        let local = unix_seconds.saturating_add(i64::from(utc_offset_seconds));
        let secs_of_day = local.rem_euclid(86_400);
        // Both narrowings are in range: secs_of_day < 86 400.
        let hms = |div: i64, modulo: i64| u32::try_from(secs_of_day / div % modulo).unwrap_or(0);
        Self {
            date: Date::from_days_since_epoch(local.div_euclid(86_400)),
            hour: hms(3600, 24),
            minute: hms(60, 60),
            second: hms(1, 60),
        }
    }

    /// Its date.
    #[must_use]
    pub fn date(self) -> Date {
        self.date
    }
}

impl fmt::Display for LocalDateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}T{:02}:{:02}:{:02}",
            self.date, self.hour, self.minute, self.second
        )
    }
}

/// A profile's SHA-256, 64 lower-case hex digits: how the server names a profile.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sha256Hex(String);

impl Sha256Hex {
    /// `s`, if it is 64 lower-case hex digits.
    ///
    /// # Errors
    /// [`NameError::Sha256`] otherwise.
    pub fn new(s: impl Into<String>) -> Result<Self, NameError> {
        let s = s.into();
        if s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            Ok(Self(s))
        } else {
            Err(NameError::Sha256(s))
        }
    }

    /// The digest as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A macOS version such as `27.0.1`. Two versions compare by their numbers, with
/// trailing zeros ignored: `27.0` equals `27` and is older than `27.0.1`.
#[derive(Clone, Debug)]
pub struct OsVersion {
    text: String,
    parts: Vec<u32>,
}

impl OsVersion {
    /// Parses 1 to 4 dot-separated decimal numbers.
    ///
    /// # Errors
    /// [`NameError::Version`] otherwise.
    pub fn parse(s: &str) -> Result<Self, NameError> {
        let mut parts = Vec::new();
        for part in s.split('.') {
            let n = number(part.as_bytes()).ok_or_else(|| NameError::Version(s.to_owned()))?;
            parts.push(n);
        }
        if parts.len() > 4 {
            return Err(NameError::Version(s.to_owned()));
        }
        while parts.last() == Some(&0) {
            parts.pop();
        }
        Ok(Self {
            text: s.to_owned(),
            parts,
        })
    }

    /// The version as it was written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl PartialEq for OsVersion {
    fn eq(&self, other: &Self) -> bool {
        self.parts == other.parts
    }
}

impl Eq for OsVersion {}

impl PartialOrd for OsVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OsVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.parts.cmp(&other.parts)
    }
}

impl fmt::Display for OsVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// 1 to 9 ASCII digits as a number.
fn number(b: &[u8]) -> Option<u32> {
    if b.is_empty() || b.len() > 9 || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(b.iter().fold(0, |n, d| n * 10 + u32::from(d - b'0')))
}

fn parse_date(b: &[u8]) -> Option<Date> {
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let year = i64::from(number(&b[..4])?);
    let month = number(&b[5..7]).filter(|m| (1..=12).contains(m))?;
    let day = number(&b[8..10]).filter(|d| (1..=days_in_month(year, month)).contains(d))?;
    Some(Date { year, month, day })
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to a proleptic Gregorian date (H. Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_from_march = i64::from((month + 9) % 12);
    let day_of_year = (153 * month_from_march + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = (month_from_march + 2) % 12 + 1;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    // In range by construction: 1 <= day <= 31, 1 <= month <= 12.
    let narrow = |v: i64| u32::try_from(v).unwrap_or(0);
    (year, narrow(month), narrow(day))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a serial check that lets a dot or slash through, so
    /// `kbf.osupdate.<serial>` could name another identifier; one that accepts an
    /// empty or overlong serial.
    #[test]
    fn a_serial_is_letters_and_digits_only() {
        assert_eq!(
            Serial::new("C02XK1ABJG5H").unwrap().as_str(),
            "C02XK1ABJG5H"
        );
        assert_eq!(
            Serial::new("a".repeat(32)).unwrap().to_string(),
            "a".repeat(32)
        );
        for bad in ["", "C02.X", "C02/X", "C02 X", "Ç02X", &"a".repeat(33)] {
            assert_eq!(
                Serial::new(bad),
                Err(NameError::Serial(bad.to_owned())),
                "{bad}"
            );
        }
    }

    /// Catches: matching kbf's declarations by substring, or accepting the bare prefix.
    #[test]
    fn kbf_declarations_are_matched_by_prefix() {
        let serial = Serial::new("C02X").unwrap();
        assert_eq!(osupdate_declaration(&serial), "kbf.osupdate.C02X");
        assert!(is_kbf_declaration("kbf.osupdate.C02X"));
        assert!(is_kbf_declaration("kbf.x"));
        assert!(!is_kbf_declaration("kbf."));
        assert!(!is_kbf_declaration("com.example.kbf.osupdate.C02X"));
        assert!(!is_kbf_declaration("kbfx.osupdate"));
        assert!(!is_kbf_declaration(""));
    }

    /// Catches: an empty, overlong or punctuated build taken for one.
    #[test]
    fn a_build_is_letters_and_digits() {
        assert!(is_build("26A434"));
        assert!(is_build("25G241a"));
        assert!(is_build(&"1".repeat(32)));
        for bad in ["", "26A 434", "26A434/", &"1".repeat(33)] {
            assert!(!is_build(bad), "{bad}");
        }
    }

    /// Catches: dates accepted out of range (month 13, 31 April, 29 February in a
    /// common year, including the century rule) or in another layout.
    #[test]
    fn dates_parse_only_real_days() {
        let d = Date::parse("2027-01-06").unwrap();
        assert_eq!(d.to_string(), "2027-01-06");
        for good in [
            "2028-02-29",
            "2400-02-29",
            "2027-04-30",
            "2026-12-31",
            "2027-02-28",
        ] {
            assert_eq!(Date::parse(good).unwrap().to_string(), good);
        }
        for bad in [
            "2027-02-29",
            "2100-02-29",
            "2027-04-31",
            "2027-13-01",
            "2027-00-01",
            "2027-01-00",
            "2027-01-32",
            "2027/01/01",
            "2027-01-1",
            "2027-1-01x",
            "202a-01-01",
            "2027-0a-01",
            "",
        ] {
            assert_eq!(
                Date::parse(bad),
                Err(NameError::Date(bad.to_owned())),
                "{bad}"
            );
        }
    }

    /// Catches: an off-by-one in the day count, either way round, across the leap
    /// day and a year end.
    #[test]
    fn days_since_epoch_round_trips() {
        assert_eq!(Date::parse("1970-01-01").unwrap().days_since_epoch(), 0);
        assert_eq!(
            Date::parse("2026-10-08").unwrap().days_since_epoch(),
            20_734
        );
        for days in [0, 20_734, 20_818, 20_819, 21_243, 21_244, 157_113] {
            let date = Date::from_days_since_epoch(days);
            assert_eq!(date.days_since_epoch(), days, "{date}");
            assert_eq!(Date::parse(&date.to_string()).unwrap(), date);
        }
        assert_eq!(
            Date::from_days_since_epoch(20_819).to_string(),
            "2027-01-01"
        );
        assert_eq!(
            Date::from_days_since_epoch(21_243).to_string(),
            "2028-02-29"
        );
        assert_eq!(
            Date::from_days_since_epoch(21_244).to_string(),
            "2028-03-01"
        );
        assert_eq!(
            Date::from_days_since_epoch(157_113).to_string(),
            "2400-02-29"
        );
    }

    /// Catches: a `TargetLocalDateTime` in another layout, or with an hour, minute or
    /// second out of range, sent to the gate.
    #[test]
    fn local_date_times_parse_only_real_times() {
        let t = LocalDateTime::parse("2026-10-08T23:59:59").unwrap();
        assert_eq!(t.to_string(), "2026-10-08T23:59:59");
        assert_eq!(t.date(), Date::parse("2026-10-08").unwrap());
        for bad in [
            "2026-10-08 23:59:59",
            "2026-10-08T23-59:59",
            "2026-10-08T23:59-59",
            "2026-10-08T24:00:00",
            "2026-10-08T23:60:00",
            "2026-10-08T23:59:60",
            "2027-02-30T00:00:00",
            "2026-10-08T23:59:59Z",
            "2026-10-08T2a:00:00",
        ] {
            assert_eq!(
                LocalDateTime::parse(bad),
                Err(NameError::LocalDateTime(bad.to_owned())),
                "{bad}"
            );
        }
    }

    /// Catches: the zone offset ignored or applied the wrong way, and a day boundary
    /// crossed in either direction.
    #[test]
    fn local_time_from_unix_seconds_applies_the_offset() {
        // 2026-10-08T12:34:56Z.
        let at = 1_791_462_896;
        assert_eq!(
            LocalDateTime::from_unix_seconds(at, 0).to_string(),
            "2026-10-08T12:34:56"
        );
        assert_eq!(
            LocalDateTime::from_unix_seconds(at, 2 * 3600).to_string(),
            "2026-10-08T14:34:56"
        );
        assert_eq!(
            LocalDateTime::from_unix_seconds(at, -13 * 3600).to_string(),
            "2026-10-07T23:34:56"
        );
        assert_eq!(
            LocalDateTime::from_unix_seconds(at, 12 * 3600).to_string(),
            "2026-10-09T00:34:56"
        );
    }

    /// Catches: a digest check that accepts upper case, a short digest or non-hex.
    #[test]
    fn a_profile_digest_is_64_lower_case_hex_digits() {
        let good = "0123456789abcdef".repeat(4);
        assert_eq!(Sha256Hex::new(good.clone()).unwrap().as_str(), good);
        for bad in [
            "0123456789ABCDEF".repeat(4),
            "0".repeat(63),
            "0".repeat(65),
            "g".repeat(64),
        ] {
            assert_eq!(Sha256Hex::new(bad.clone()), Err(NameError::Sha256(bad)));
        }
    }

    /// Catches: versions compared as text (`27.10` before `27.9`), trailing zeros
    /// making equal versions differ, and malformed versions accepted.
    #[test]
    fn versions_compare_by_number() {
        let v = |s| OsVersion::parse(s).unwrap();
        assert!(v("27.10") > v("27.9"));
        assert!(v("27.0.1") > v("27"));
        assert_eq!(v("27.0"), v("27"));
        assert_eq!(v("27.0").partial_cmp(&v("27.0.0")), Some(Ordering::Equal));
        assert!(v("26.7.1") < v("27.0.1"));
        assert_eq!(v("27.0.1").as_str(), "27.0.1");
        assert_eq!(v("27.0").to_string(), "27.0");
        assert_eq!(v("0").as_str(), "0");
        for bad in ["", "27.", ".1", "27.a", "27.0.1.2.3", "1234567890"] {
            assert_eq!(
                OsVersion::parse(bad),
                Err(NameError::Version(bad.to_owned()))
            );
        }
        assert!(OsVersion::parse("27.0.1.300").is_ok());
    }
}
