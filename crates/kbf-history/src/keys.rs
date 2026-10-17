//! What history objects and index rows are called: the UTC day, the writer, the
//! invocation key, the retention classes and families of objects, and the key of every
//! object and `ui/` row this crate writes.
//!
//! Object keys are `<root>history/<class>/<family>/<yyyymmdd written, UTC>/...`: the
//! retention class leads, so one lifecycle rule covers a class, and the day lets a
//! sweeper delete whole days.

use std::fmt;

use kbf_objstore::{KeyError, KeyPrefix, ObjectKey};
use sha2::{Digest as _, Sha256};

use crate::run_id::StreamId;

const MS_PER_DAY: u64 = 86_400_000;

/// The largest time a list key holds: twelve hex digits.
const MAX_LIST_MS: u64 = 0xffff_ffff_ffff;

/// A UTC calendar day, as the number `yyyymmdd`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Day(u32);

impl Day {
    /// The UTC day that holds `ms` milliseconds after the Unix epoch.
    #[must_use]
    pub fn from_ms(ms: u64) -> Day {
        let (y, m, d) = civil_from_days(ms / MS_PER_DAY);
        Day(y * 10_000 + m * 100 + d)
    }

    /// The day from its `yyyymmdd` number, as stored in a row or header.
    #[must_use]
    pub fn from_number(n: u32) -> Day {
        Day(n)
    }

    /// The `yyyymmdd` number.
    #[must_use]
    pub fn number(self) -> u32 {
        self.0
    }
}

impl fmt::Display for Day {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:08}", self.0)
    }
}

/// The civil date of a day count from the Unix epoch (the proleptic Gregorian calendar, for the
/// non-negative day counts a `u64` of milliseconds gives).
fn civil_from_days(days: u64) -> (u32, u32, u32) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    // A `u64` of milliseconds reaches about year 584 million, which `yyyymmdd` in a
    // `u32` cannot show; saturate at the largest year it can.
    let y = y.min(MAX_YEAR);
    // Each value is now below its bound (year, 12, 31), so the casts are exact.
    (y as u32, m as u32, d as u32)
}

/// The largest year whose `yyyymmdd` fits a `u32`.
const MAX_YEAR: u64 = 429_495;

/// Which history writer wrote an object: the server epoch on a v1 frontend. Every
/// writer writes only keys that carry its id, so no two writers share an object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WriterId(pub u64);

impl fmt::Display for WriterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// The key of an invocation's history: the first 16 hex digits of SHA-256 of
/// `id/<record id>` for an invocation the client named, or of
/// `idless/<writer>/<day>` for the id-less pseudo-invocation of one writer and day
/// (runs whose client sent no id). No client id can produce the second form.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InvKey(String);

impl InvKey {
    /// The key of the invocation the client named `record_id`.
    #[must_use]
    pub fn from_record_id(record_id: &str) -> InvKey {
        InvKey::hashed(&format!("id/{record_id}"))
    }

    /// The key of `writer`'s id-less pseudo-invocation for `day`.
    #[must_use]
    pub fn idless(writer: WriterId, day: Day) -> InvKey {
        InvKey::hashed(&format!("idless/{writer}/{day}"))
    }

    fn hashed(text: &str) -> InvKey {
        InvKey(hex::encode(&Sha256::digest(text.as_bytes())[..8]))
    }

    /// Checks a key read back from an index row or object.
    ///
    /// # Errors
    /// [`KeyTextError`] when `text` is not 16 lowercase hex digits.
    pub fn parse(text: &str) -> Result<InvKey, KeyTextError> {
        let ok = text.len() == 16
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if ok {
            Ok(InvKey(text.to_owned()))
        } else {
            Err(KeyTextError(text.to_owned()))
        }
    }

    /// The 16 hex digits.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InvKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Text that should have been an invocation key and is not.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not an invocation key (16 lowercase hex digits)")]
pub struct KeyTextError(String);

/// A retention class: how long the objects under its prefix are kept. The operator's
/// lifecycle rule for a class expires objects [`Class::expire_after_days`] after they
/// were written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    /// Seven days: windows and client reports.
    R7,
    /// Thirty days: invocation chunks, day indexes, test cases.
    R30,
    /// Forty-five days: daily outcomes, rollups and series.
    R45,
    /// Four hundred days: long-lived day objects and monthly merges.
    R400,
}

impl Class {
    /// Every class, shortest first.
    pub const ALL: [Class; 4] = [Class::R7, Class::R30, Class::R45, Class::R400];

    /// How many days the class keeps what it holds.
    #[must_use]
    pub fn days(self) -> u32 {
        match self {
            Class::R7 => 7,
            Class::R30 => 30,
            Class::R45 => 45,
            Class::R400 => 400,
        }
    }

    /// The lifecycle expiry: the class's days plus two, so a sweeper deleting by the day
    /// in the key always runs before the store expires the object under it.
    #[must_use]
    pub fn expire_after_days(self) -> u32 {
        self.days() + 2
    }

    /// The key segment that leads every object of the class.
    #[must_use]
    pub fn prefix(self) -> &'static str {
        match self {
            Class::R7 => "r7",
            Class::R30 => "r30",
            Class::R45 => "r45",
            Class::R400 => "r400",
        }
    }
}

/// A family of history objects: what its records are and which class keeps it. The
/// number is the family byte of the container header, so it never changes meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Family {
    /// An invocation chunk: run and hit records, and the failed-run section (tails and
    /// command renderings), of one invocation from one writer.
    InvocationChunk,
}

impl Family {
    /// The family byte of the container header.
    #[must_use]
    pub fn number(self) -> u8 {
        match self {
            Family::InvocationChunk => 1,
        }
    }

    /// The family with this header byte, if this build knows it.
    #[must_use]
    pub fn from_number(n: u8) -> Option<Family> {
        match n {
            1 => Some(Family::InvocationChunk),
            _ => None,
        }
    }

    /// The retention class of the family's objects.
    #[must_use]
    pub fn class(self) -> Class {
        match self {
            Family::InvocationChunk => Class::R30,
        }
    }

    /// The key segment after the class.
    #[must_use]
    pub fn dir(self) -> &'static str {
        match self {
            Family::InvocationChunk => "inv",
        }
    }
}

/// The root every history object lives under, `<root>history/`, and the keys below it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryRoot(KeyPrefix);

impl HistoryRoot {
    /// `<root>history/` under the store's own prefix `root` (empty, or ending in `/`).
    ///
    /// # Errors
    /// [`KeyError`] when `root` does not make a valid prefix.
    pub fn under(root: &KeyPrefix) -> Result<HistoryRoot, KeyError> {
        if !(root.as_str().is_empty() || root.as_str().ends_with('/')) {
            return Err(KeyError::Segment(root.as_str().to_owned()));
        }
        Ok(HistoryRoot(root.child("history/")?))
    }

    /// The prefix, ending in `history/`.
    #[must_use]
    pub fn prefix(&self) -> &KeyPrefix {
        &self.0
    }

    /// The key of chunk `seq` of invocation `inv` from `writer`, written on `day`:
    /// `<root>history/r30/inv/<day>/<inv-key>/<writer>-<seq>`.
    #[must_use]
    pub fn chunk(&self, day: Day, inv: &InvKey, writer: WriterId, seq: u32) -> ObjectKey {
        let family = Family::InvocationChunk;
        let name = format!(
            "{}/{}/{day}/{inv}/{writer}-{seq:08x}",
            family.class().prefix(),
            family.dir()
        );
        // Every part is hex digits, a fixed word or a `yyyymmdd` number under a prefix
        // that was checked, so the key is valid by construction.
        self.0
            .key(&name)
            .expect("hex digits and fixed words under a checked prefix make a valid key")
    }
}

/// Where history's index rows live on the state store: `ui/` by default, or under a
/// prefix of the deployment's choosing (`<prefix>ui/`), so a state store shared by
/// several farms or tenants keeps each one's rows apart, as [`HistoryRoot`] does for
/// objects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexRoot(String);

impl Default for IndexRoot {
    /// `ui/`.
    fn default() -> IndexRoot {
        IndexRoot("ui/".to_owned())
    }
}

/// A prefix that cannot lead index rows.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not an index prefix (empty, or ending in `/`)")]
pub struct IndexPrefixError(String);

impl IndexRoot {
    /// `<prefix>ui/`.
    ///
    /// # Errors
    /// [`IndexPrefixError`] when `prefix` is neither empty nor ends in `/`.
    pub fn under(prefix: &str) -> Result<IndexRoot, IndexPrefixError> {
        if !(prefix.is_empty() || prefix.ends_with('/')) {
            return Err(IndexPrefixError(prefix.to_owned()));
        }
        Ok(IndexRoot(format!("{prefix}ui/")))
    }

    /// The root, ending in `ui/`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The counter writers reserve blocks of stream ids from.
    #[must_use]
    pub fn streams_counter(&self) -> String {
        format!("{}streams", self.0)
    }

    /// The header row of an invocation.
    #[must_use]
    pub fn invid(&self, inv: &InvKey) -> String {
        format!("{}invid/{inv}", self.0)
    }

    /// The prefix of every chunk row of an invocation, in writer and sequence order.
    #[must_use]
    pub fn ichunk_prefix(&self, inv: &InvKey) -> String {
        format!("{}ichunk/{inv}/", self.0)
    }

    /// The row of chunk `seq` of `inv` from `writer`.
    #[must_use]
    pub fn ichunk(&self, inv: &InvKey, writer: WriterId, seq: u32) -> String {
        format!("{}ichunk/{inv}/{writer}/{seq:08x}", self.0)
    }

    /// The prefix of every row of `stream` on `day`, in open-time order.
    #[must_use]
    pub fn stream_prefix(&self, day: Day, stream: StreamId) -> String {
        format!("{}stream/{day}/{stream}/", self.0)
    }

    /// The row of `stream` on `day` that opened at `first_ms` (its first id's time, which
    /// no later id of the stream precedes). The time is part of the key, so a fallback
    /// stream id reused after a restart gets a row of its own and the newest row at or
    /// before a run's time is the run's stream.
    #[must_use]
    pub fn stream(&self, day: Day, stream: StreamId, first_ms: u64) -> String {
        format!("{}stream/{day}/{stream}/{first_ms:012x}", self.0)
    }

    /// `text` as one segment of a row key. Instance names and configuration hashes are
    /// the client's, and an instance such as `projects/p/instances/i` holds `/`, which
    /// would move a row into another instance's range. Letters, digits, `.`, `_` and
    /// `-` stand for themselves, every other byte is `%` and two uppercase hex digits,
    /// and the empty text is `~` (never produced otherwise: `~` itself is escaped), so
    /// a segment never holds `/` and is never empty.
    #[must_use]
    pub fn segment(text: &str) -> String {
        if text.is_empty() {
            return "~".to_owned();
        }
        let mut out = String::with_capacity(text.len());
        for b in text.bytes() {
            if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-') {
                out.push(char::from(b));
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    }

    /// The text a [`IndexRoot::segment`] stands for; `None` for text it cannot have made.
    #[must_use]
    pub fn unsegment(segment: &str) -> Option<String> {
        if segment == "~" {
            return Some(String::new());
        }
        let bytes = segment.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut at = 0;
        while at < bytes.len() {
            let b = bytes[at];
            if b == b'%' {
                let hex = segment.get(at + 1..at + 3)?;
                let byte = u8::from_str_radix(hex, 16).ok()?;
                // Only the form `segment` writes: two uppercase digits (not `+4` or `4f`).
                if format!("{byte:02X}") != hex {
                    return None;
                }
                out.push(byte);
                at += 3;
            } else if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-') {
                out.push(b);
                at += 1;
            } else {
                return None;
            }
        }
        String::from_utf8(out).ok().filter(|s| !s.is_empty())
    }

    /// The prefix of every list entry, `ui/inv/`.
    #[must_use]
    pub fn inv_list_root(&self) -> String {
        format!("{}inv/", self.0)
    }

    /// The prefix of `instance`'s list entries, in first-seen order.
    #[must_use]
    pub fn inv_list_prefix(&self, instance: &str) -> String {
        format!("{}inv/{}/", self.0, IndexRoot::segment(instance))
    }

    /// The list entry of `inv` under `instance`, keyed by the time the invocation was
    /// listed at (its header's `list_ms`). The time is fixed-width hex, so keys sort by
    /// it.
    #[must_use]
    pub fn inv_list(&self, instance: &str, listed_ms: u64, inv: &InvKey) -> String {
        format!(
            "{}{}",
            self.inv_list_prefix(instance),
            IndexRoot::inv_list_tail(listed_ms, inv)
        )
    }

    /// `<time>/<inv-key>`: what follows an instance in a list entry's key.
    #[must_use]
    pub fn inv_list_tail(listed_ms: u64, inv: &InvKey) -> String {
        format!("{:012x}/{inv}", listed_ms.min(MAX_LIST_MS))
    }

    /// The instance, time and invocation a list entry's key names, if it is one of
    /// this root's.
    #[must_use]
    pub fn parse_inv_list(&self, key: &str) -> Option<(String, u64, InvKey)> {
        let rest = key.strip_prefix(self.0.as_str())?.strip_prefix("inv/")?;
        let mut parts = rest.split('/');
        let (instance, time, inv) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() || time.len() != 12 {
            return None;
        }
        Some((
            IndexRoot::unsegment(instance)?,
            u64::from_str_radix(time, 16).ok()?,
            InvKey::parse(inv).ok()?,
        ))
    }

    /// The shape and configuration a `<shape>/<cfg>` tail names.
    #[must_use]
    pub fn parse_shape_tail(tail: &str) -> Option<(u64, String)> {
        let (shape, cfg) = tail.split_once('/')?;
        if shape.len() != 16 {
            return None;
        }
        Some((
            u64::from_str_radix(shape, 16).ok()?,
            IndexRoot::unsegment(cfg)?,
        ))
    }

    /// The prefix of every shape catalog row, `ui/shape/`.
    #[must_use]
    pub fn shape_root(&self) -> String {
        format!("{}shape/", self.0)
    }

    /// The prefix of `instance`'s shape catalog rows.
    #[must_use]
    pub fn shape_prefix(&self, instance: &str) -> String {
        format!("{}shape/{}/", self.0, IndexRoot::segment(instance))
    }

    /// What follows an instance in a shape or status row's key: `<shape>/<cfg>`.
    #[must_use]
    pub fn shape_tail(shape: u64, cfg: &str) -> String {
        format!("{shape:016x}/{}", IndexRoot::segment(cfg))
    }

    /// The catalog row of (`instance`, `shape`, `cfg`).
    #[must_use]
    pub fn shape(&self, instance: &str, shape: u64, cfg: &str) -> String {
        format!(
            "{}{}",
            self.shape_prefix(instance),
            IndexRoot::shape_tail(shape, cfg)
        )
    }

    /// The status row of a test (`instance`, `shape`, `cfg`) on its branch. No farm
    /// source names a branch before the client report, so the branch is `_`.
    #[must_use]
    pub fn status(&self, instance: &str, shape: u64, cfg: &str) -> String {
        format!(
            "{}status/{}/{}/_",
            self.0,
            IndexRoot::segment(instance),
            IndexRoot::shape_tail(shape, cfg)
        )
    }

    /// The prefix of `instance`'s status rows.
    #[must_use]
    pub fn status_prefix(&self, instance: &str) -> String {
        format!("{}status/{}/", self.0, IndexRoot::segment(instance))
    }

    /// The by-status index row of a test whose status is `status` (a name from
    /// [`StatusKind::name`](crate::StatusKind::name)).
    #[must_use]
    pub fn statusx(&self, status: &str, instance: &str, shape: u64, cfg: &str) -> String {
        format!(
            "{}{}/{}",
            self.statusx_prefix(status),
            IndexRoot::segment(instance),
            IndexRoot::shape_tail(shape, cfg)
        )
    }

    /// The prefix of the by-status index rows of `status`.
    #[must_use]
    pub fn statusx_prefix(&self, status: &str) -> String {
        format!("{}statusx/{status}/", self.0)
    }

    /// The open time of a stream row's key, if it is one.
    #[must_use]
    pub fn stream_first_ms(key: &str) -> Option<u64> {
        let hex = key.rsplit('/').next()?;
        if hex.len() != 12 {
            return None;
        }
        u64::from_str_radix(hex, 16).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_are_utc_calendar_days() {
        // Catches: an off-by-one in the civil-date algorithm, a local-time day, or a day
        // boundary one millisecond out.
        let midnight = 1_792_108_800_000; // the first millisecond of 2026-10-16
        assert_eq!(Day::from_ms(midnight - 1).to_string(), "20261015");
        assert_eq!(Day::from_ms(midnight).to_string(), "20261016");
        assert_eq!(
            Day::from_ms(midnight + MS_PER_DAY - 1).to_string(),
            "20261016"
        );
        // The last day of a year and the first of the next.
        assert_eq!(Day::from_ms(1_798_675_200_000).to_string(), "20261231");
        assert_eq!(
            Day::from_ms(1_798_675_200_000 + MS_PER_DAY).number(),
            20_270_101
        );
        // A leap day, and a century year that is not a leap year.
        assert_eq!(Day::from_ms(1_835_395_200_000).to_string(), "20280229");
        assert_eq!(Day::from_ms(4_107_456_000_000).to_string(), "21000228");
        assert_eq!(
            Day::from_ms(4_107_456_000_000 + MS_PER_DAY).to_string(),
            "21000301"
        );
        assert_eq!(Day::from_number(20_261_016).to_string(), "20261016");
        // The far end saturates rather than wrapping into a small year.
        assert!(Day::from_ms(u64::MAX).number() > 40_000_000);
    }

    #[test]
    fn invocation_keys_hash_the_two_forms_apart() {
        // Catches: the id-less form colliding with a client id that spells it, or a key
        // that is not 16 hex digits.
        let named = InvKey::from_record_id("b1");
        assert_eq!(named.as_str().len(), 16);
        let writer = WriterId(0xfedc_ba98_7654_3210);
        let idless = InvKey::idless(writer, Day::from_number(20_261_016));
        let spelled = InvKey::from_record_id("idless/fedcba9876543210/20261016");
        assert_ne!(idless, spelled);
        assert_eq!(InvKey::parse(named.as_str()), Ok(named.clone()));
        assert_eq!(named.to_string(), named.as_str());
        for bad in [
            "",
            "0123456789abcde",
            "0123456789abcdeF",
            "0123456789abcdeg",
        ] {
            let e = InvKey::parse(bad).expect_err(bad);
            assert!(e.to_string().contains("not an invocation key"));
        }
    }

    #[test]
    fn classes_keep_their_days_plus_two() {
        // Catches: a class prefix or expiry that drifts from the retention table.
        let got: Vec<_> = Class::ALL
            .iter()
            .map(|c| (c.prefix(), c.days(), c.expire_after_days()))
            .collect();
        assert_eq!(
            got,
            [
                ("r7", 7, 9),
                ("r30", 30, 32),
                ("r45", 45, 47),
                ("r400", 400, 402)
            ]
        );
    }

    #[test]
    fn families_round_trip_their_header_byte() {
        // Catches: a family byte that does not read back, or an unknown byte accepted.
        let f = Family::InvocationChunk;
        assert_eq!(Family::from_number(f.number()), Some(f));
        assert_eq!(Family::from_number(0), None);
        assert_eq!(Family::from_number(2), None);
        assert_eq!((f.class(), f.dir()), (Class::R30, "inv"));
    }

    #[test]
    fn chunk_keys_lead_with_class_family_and_day() {
        // Catches: a key whose class does not lead (one lifecycle rule per class would
        // then miss it) or that lacks the writer, so two writers could share a key.
        let root = HistoryRoot::under(&KeyPrefix::new("kbf/").unwrap()).unwrap();
        assert_eq!(root.prefix().as_str(), "kbf/history/");
        let inv = InvKey::parse("0123456789abcdef").unwrap();
        let key = root.chunk(
            Day::from_number(20_261_016),
            &inv,
            WriterId(0xfedc_ba98_7654_3210),
            10,
        );
        assert_eq!(
            key.as_str(),
            "kbf/history/r30/inv/20261016/0123456789abcdef/fedcba9876543210-0000000a"
        );
        let bare = HistoryRoot::under(&KeyPrefix::default()).unwrap();
        assert_eq!(bare.prefix().as_str(), "history/");
        // A root that is not a whole segment would make `kbfhistory/`.
        assert!(HistoryRoot::under(&KeyPrefix::new("kbf").unwrap()).is_err());
    }

    #[test]
    fn row_keys_sort_in_the_order_reads_need() {
        // Catches: unpadded numbers, which would sort chunk 10 before chunk 9 and a
        // later stream row before an earlier one.
        let rows = IndexRoot::default();
        let inv = InvKey::parse("0123456789abcdef").unwrap();
        assert_eq!(rows.invid(&inv), "ui/invid/0123456789abcdef");
        assert_eq!(rows.streams_counter(), "ui/streams");
        let c9 = rows.ichunk(&inv, WriterId(1), 9);
        let c10 = rows.ichunk(&inv, WriterId(1), 10);
        assert!(c9 < c10);
        assert!(c9.starts_with(&rows.ichunk_prefix(&inv)));
        let day = Day::from_number(20_261_016);
        let s = StreamId(0x80_0001);
        let early = rows.stream(day, s, 9);
        let late = rows.stream(day, s, 10);
        assert!(early < late);
        assert!(late.starts_with(&rows.stream_prefix(day, s)));
        assert_eq!(late, "ui/stream/20261016/800001/00000000000a");
        assert_eq!(IndexRoot::stream_first_ms(&late), Some(10));
        assert_eq!(IndexRoot::stream_first_ms("ui/stream/x/800001/0a"), None);
        assert_eq!(
            IndexRoot::stream_first_ms("ui/stream/x/800001/zzzzzzzzzzzz"),
            None
        );
    }

    #[test]
    fn an_index_root_puts_every_row_under_its_prefix() {
        // Catches: a row built from a hard-coded `ui/`, so two farms or tenants sharing
        // one state store would write the same rows.
        let rows = IndexRoot::under("tenant-a/").unwrap();
        assert_eq!(rows.as_str(), "tenant-a/ui/");
        let inv = InvKey::parse("0123456789abcdef").unwrap();
        let day = Day::from_number(20_261_016);
        let keys = [
            rows.streams_counter(),
            rows.invid(&inv),
            rows.ichunk_prefix(&inv),
            rows.ichunk(&inv, WriterId(1), 0),
            rows.stream_prefix(day, StreamId(1)),
            rows.stream(day, StreamId(1), 0),
            rows.inv_list_root(),
            rows.inv_list("a", 1, &inv),
            rows.shape_root(),
            rows.shape("a", 1, "c"),
            rows.status("a", 1, "c"),
            rows.status_prefix("a"),
            rows.statusx("failing", "a", 1, "c"),
        ];
        assert!(
            keys.iter().all(|k| k.starts_with("tenant-a/ui/")),
            "{keys:?}"
        );
        assert_eq!(IndexRoot::under("").unwrap(), IndexRoot::default());
        let err = IndexRoot::under("tenant-a").unwrap_err();
        assert!(err.to_string().contains("not an index prefix"), "{err}");
    }

    #[test]
    fn segments_never_hold_a_slash_and_read_back() {
        // Catches: an instance or configuration used raw in a key (a `/` would move a
        // row into another instance's range), an empty segment, and a segment that does
        // not decode to what went in.
        for text in [
            "main",
            "projects/p/instances/i",
            "50%",
            "~",
            "a b",
            "é",
            "-._",
        ] {
            let segment = IndexRoot::segment(text);
            assert!(!segment.contains('/'), "{segment}");
            assert!(!segment.is_empty(), "{segment}");
            assert_eq!(IndexRoot::unsegment(&segment).as_deref(), Some(text));
        }
        assert_eq!(IndexRoot::segment(""), "~");
        assert_eq!(IndexRoot::segment("a/b"), "a%2Fb");
        assert_eq!(IndexRoot::segment("~"), "%7E");
        assert_eq!(IndexRoot::unsegment("~").as_deref(), Some(""));
        // What no segment can be.
        for bad in ["", "a/b", "a%", "a%4", "a%zz", "a%4f", "%FF", "a b", "é"] {
            assert_eq!(IndexRoot::unsegment(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn list_and_shape_keys_name_their_parts_and_refuse_other_keys() {
        // Catches: a list key that does not sort by time, a key read back as another
        // (instance, time, invocation), and a foreign key taken for a list entry.
        let rows = IndexRoot::default();
        let inv = InvKey::parse("0123456789abcdef").unwrap();
        let key = rows.inv_list("a/b", 0x1a2b, &inv);
        assert_eq!(key, "ui/inv/a%2Fb/000000001a2b/0123456789abcdef");
        assert_eq!(
            rows.parse_inv_list(&key),
            Some(("a/b".to_owned(), 0x1a2b, inv.clone()))
        );
        assert!(rows.inv_list("a", 9, &inv) < rows.inv_list("a", 10, &inv));
        assert!(key.starts_with(&rows.inv_list_prefix("a/b")));
        // Past the largest time a key holds, the time saturates rather than widening.
        assert!(
            rows.inv_list("a", u64::MAX, &inv)
                .contains("/ffffffffffff/")
        );
        let other = IndexRoot::under("t/").unwrap();
        for bad in [
            "ui/invid/0123456789abcdef".to_owned(),
            "ui/inv/a".to_owned(),
            "ui/inv/a/000000001a2b".to_owned(),
            "ui/inv/a/000000001a2b/0123456789abcdef/x".to_owned(),
            "ui/inv/a/1a2b/0123456789abcdef".to_owned(),
            "ui/inv/a/00000000zzzz/0123456789abcdef".to_owned(),
            "ui/inv/a/000000001a2b/nope".to_owned(),
            "ui/inv//000000001a2b/0123456789abcdef".to_owned(),
            key.clone(),
        ] {
            let root = if bad == key { &other } else { &rows };
            assert_eq!(root.parse_inv_list(&bad), None, "{bad}");
        }
        let tail = IndexRoot::shape_tail(255, "c/f");
        assert_eq!(tail, "00000000000000ff/c%2Ff");
        assert_eq!(
            IndexRoot::parse_shape_tail(&tail),
            Some((255, "c/f".to_owned()))
        );
        for bad in [
            "",
            "ff/cfg",
            "00000000000000ff",
            "000000000000zzzz/cfg",
            "00000000000000ff/a/b",
        ] {
            assert_eq!(IndexRoot::parse_shape_tail(bad), None, "{bad:?}");
        }
        assert_eq!(rows.shape("i", 1, "c"), "ui/shape/i/0000000000000001/c");
        assert_eq!(rows.status("i", 1, "c"), "ui/status/i/0000000000000001/c/_");
        assert_eq!(
            rows.statusx("failing", "i", 1, "c"),
            "ui/statusx/failing/i/0000000000000001/c"
        );
        assert!(
            rows.status("i", 1, "c")
                .starts_with(&rows.status_prefix("i"))
        );
        assert!(rows.shape("i", 1, "c").starts_with(&rows.shape_root()));
        // `status/` and `statusx/` do not share a prefix.
        assert!(
            !rows
                .statusx("failing", "i", 1, "c")
                .starts_with(&rows.status_prefix("i"))
        );
    }
}
