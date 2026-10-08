//! Lease ids as the helper accepts them, the lease user's name, and the uid range.

use std::fmt;

use kbf_types::LeaseId;

/// The prefix of every lease user's name. Nothing outside it is ever created or
/// deleted.
pub const USER_PREFIX: &str = "kbf-lease-";

/// Parses a lease id in its text form, `<term>.<seq>` (`LeaseId`'s `Display`).
///
/// Strict: two decimal `u64`s, no sign, no leading zero, nothing else. One lease id has
/// exactly one spelling, so the ledger of used ids cannot be dodged by writing an id
/// differently (`007.1` for `7.1`).
///
/// # Errors
/// The text is not exactly that form.
pub fn parse_lease(text: &str) -> Result<LeaseId, String> {
    let refuse = || format!("lease id {text:?} is not <term>.<seq>");
    let (term, seq) = text.split_once('.').ok_or_else(refuse)?;
    Ok(LeaseId::new(
        number(term).ok_or_else(refuse)?,
        number(seq).ok_or_else(refuse)?,
    ))
}

fn number(digits: &str) -> Option<u64> {
    let canonical = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'));
    if canonical { digits.parse().ok() } else { None }
}

/// The lease user's account name: `kbf-lease-<term>-<seq>`. A hyphen stands for the
/// lease id's dot, which some macOS tools treat specially in an account name.
#[must_use]
pub fn user_name(lease: LeaseId) -> String {
    format!("{USER_PREFIX}{}-{}", lease.term, lease.seq)
}

/// The uids lease users get, `first..=last`. The helper never touches a uid outside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UidRange {
    first: u32,
    last: u32,
}

/// The lowest uid a range may start at: macOS keeps the uids below 500 for system
/// accounts, and gives people uids from 501.
pub const MIN_UID: u32 = 500;
/// The highest uid a range may end at, below the `nobody`-style uids near `u32::MAX`
/// and Linux's 65534.
pub const MAX_UID: u32 = 60_000;
/// The most uids a range may hold. A uid is reused only once the range wraps.
pub const MAX_LEN: u32 = 10_000;

impl UidRange {
    /// A range of lease uids.
    ///
    /// # Errors
    /// `first > last`, or the range leaves `MIN_UID..=MAX_UID` or holds more than
    /// [`MAX_LEN`] uids.
    pub fn new(first: u32, last: u32) -> Result<Self, String> {
        if first > last {
            return Err(format!("uid range {first}-{last} is empty"));
        }
        if first < MIN_UID || last > MAX_UID {
            return Err(format!(
                "uid range {first}-{last} leaves {MIN_UID}-{MAX_UID}"
            ));
        }
        if last - first >= MAX_LEN {
            return Err(format!(
                "uid range {first}-{last} holds more than {MAX_LEN} uids"
            ));
        }
        Ok(Self { first, last })
    }

    /// Whether `uid` is a lease uid.
    #[must_use]
    pub fn contains(self, uid: u32) -> bool {
        (self.first..=self.last).contains(&uid)
    }

    /// Every uid of the range once, starting after `after` (the uid handed out last)
    /// and wrapping, so a uid is reused as late as possible. From the first uid when
    /// `after` is `None` or outside the range.
    pub fn after(self, after: Option<u32>) -> impl Iterator<Item = u32> {
        let len = self.last - self.first + 1;
        let start = match after {
            Some(uid) if self.contains(uid) => uid - self.first + 1,
            _ => 0,
        };
        (0..len).map(move |i| self.first + (start + i) % len)
    }
}

impl std::str::FromStr for UidRange {
    type Err = String;

    /// `<first>-<last>`, as in `600-699`.
    fn from_str(text: &str) -> Result<Self, String> {
        let refuse = || format!("uid range {text:?} is not <first>-<last>");
        let (first, last) = text.split_once('-').ok_or_else(refuse)?;
        let first = number(first).and_then(|n| u32::try_from(n).ok());
        let last = number(last).and_then(|n| u32::try_from(n).ok());
        Self::new(first.ok_or_else(refuse)?, last.ok_or_else(refuse)?)
    }
}

impl fmt::Display for UidRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.first, self.last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a parser that accepts a second spelling of an id (leading zero, sign,
    /// spaces), which would let one lease id get two users past the ledger.
    #[test]
    fn a_lease_id_has_one_spelling() {
        assert_eq!(parse_lease("7.1"), Ok(LeaseId::new(7, 1)));
        assert_eq!(parse_lease("0.0"), Ok(LeaseId::new(0, 0)));
        assert_eq!(
            parse_lease("18446744073709551615.10"),
            Ok(LeaseId::new(u64::MAX, 10))
        );
        for bad in [
            "",
            "7",
            "7.",
            ".1",
            "07.1",
            "7.01",
            "+7.1",
            "7.-1",
            " 7.1",
            "7.1 ",
            "7.1.2",
            "7_1",
            "a.1",
            "18446744073709551616.1",
        ] {
            assert!(parse_lease(bad).is_err(), "{bad:?} was accepted");
        }
    }

    /// Catches: a name that drops a field or keeps the dot.
    #[test]
    fn the_user_name_carries_both_fields() {
        assert_eq!(user_name(LeaseId::new(3, 41)), "kbf-lease-3-41");
    }

    /// Catches: a range check off by one at either end, or one that accepts system
    /// uids.
    #[test]
    fn a_range_holds_its_ends_and_refuses_system_uids() {
        let range: UidRange = "600-699".parse().unwrap();
        assert_eq!(range.to_string(), "600-699");
        assert!(range.contains(600));
        assert!(range.contains(699));
        assert!(!range.contains(599));
        assert!(!range.contains(700));
        assert!("600-600".parse::<UidRange>().is_ok());
        for bad in [
            "699-600",
            "499-600",
            "501-60001",
            "600",
            "600-",
            "-600",
            "a-b",
            "600-0699",
            "500-10500",
            "600-4294967296",
        ] {
            assert!(bad.parse::<UidRange>().is_err(), "{bad:?} was accepted");
        }
        assert!("500-10499".parse::<UidRange>().is_ok());
    }

    /// Catches: allocation that restarts at the first uid (reusing a uid at once) or
    /// skips or repeats one when it wraps.
    #[test]
    fn uids_come_round_after_the_last_one_handed_out() {
        let range = UidRange::new(600, 603).unwrap();
        let order = |after| range.after(after).collect::<Vec<_>>();
        assert_eq!(order(None), [600, 601, 602, 603]);
        assert_eq!(order(Some(601)), [602, 603, 600, 601]);
        assert_eq!(order(Some(603)), [600, 601, 602, 603]);
        assert_eq!(order(Some(42)), [600, 601, 602, 603]);
    }
}
