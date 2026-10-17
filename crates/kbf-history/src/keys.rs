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

use crate::run_id::{RunId, StreamId};

const MS_PER_DAY: u64 = 86_400_000;

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
    /// The copy of a failed run's stdout, stderr and ActionResult, kept after the log
    /// blobs themselves are gone (ui-data.md Q21): one object per run.
    FailureLog,
}

impl Family {
    /// The family byte of the container header.
    #[must_use]
    pub fn number(self) -> u8 {
        match self {
            Family::InvocationChunk => 1,
            Family::FailureLog => 2,
        }
    }

    /// The family with this header byte, if this build knows it.
    #[must_use]
    pub fn from_number(n: u8) -> Option<Family> {
        match n {
            1 => Some(Family::InvocationChunk),
            2 => Some(Family::FailureLog),
            _ => None,
        }
    }

    /// The retention class of the family's objects.
    #[must_use]
    pub fn class(self) -> Class {
        match self {
            Family::InvocationChunk | Family::FailureLog => Class::R30,
        }
    }

    /// The key segment after the class.
    #[must_use]
    pub fn dir(self) -> &'static str {
        match self {
            Family::InvocationChunk => "inv",
            Family::FailureLog => "flog",
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

    /// The key of the failure-log copy of run `run`:
    /// `<root>history/r30/flog/<day>/<run id>`. The day is the run id's own (the UTC day
    /// of its commit time), not the day the copy was written, so the key follows from
    /// the id alone and a sweeper deleting by the day in the key sees the run's day.
    #[must_use]
    pub fn flog(&self, run: RunId) -> ObjectKey {
        let family = Family::FailureLog;
        let name = format!(
            "{}/{}/{}/{run}",
            family.class().prefix(),
            family.dir(),
            run.day()
        );
        // Hex digits and fixed words under a checked prefix, as for a chunk.
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

    /// The row of the object `name` of `family` written on `day`:
    /// `ui/obj/<family dir>/<day>/<name>`, holding where its footer is.
    #[must_use]
    pub fn obj(&self, family: Family, day: Day, name: &str) -> String {
        format!("{}obj/{}/{day}/{name}", self.0, family.dir())
    }

    /// The row of the failure-log copy of `run` (see [`HistoryRoot::flog`]).
    #[must_use]
    pub fn obj_flog(&self, run: RunId) -> String {
        self.obj(Family::FailureLog, run.day(), &run.to_string())
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
        assert_eq!((f.class(), f.dir()), (Class::R30, "inv"));
        // The failure-log copy is family 2, in class r30 under `flog`; a byte past it is
        // still unknown.
        let flog = Family::FailureLog;
        assert_eq!(flog.number(), 2);
        assert_eq!(Family::from_number(2), Some(flog));
        assert_eq!(Family::from_number(3), None);
        assert_eq!((flog.class(), flog.dir()), (Class::R30, "flog"));
    }

    #[test]
    fn failure_log_keys_follow_from_the_run_id_alone() {
        // Catches: a key or row that needs more than the run id (a reader holds nothing
        // else), a day that is not the run id's (the sweeper deletes by the day in the
        // key), and a row key that does not sit under the retention class and family of
        // its object.
        let root = HistoryRoot::under(&KeyPrefix::new("kbf/").unwrap()).unwrap();
        let run = RunId::new(1_792_108_800_000, StreamId(0x80_0001), 5);
        let key = root.flog(run);
        assert_eq!(
            key.as_str(),
            format!("kbf/history/r30/flog/20261016/{run}"),
            "{key:?}"
        );
        let rows = IndexRoot::under("tenant-a/").unwrap();
        assert_eq!(
            rows.obj_flog(run),
            format!("tenant-a/ui/obj/flog/20261016/{run}")
        );
        assert_eq!(
            rows.obj(Family::InvocationChunk, Day::from_number(20_261_016), "x"),
            "tenant-a/ui/obj/inv/20261016/x"
        );
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
        ];
        assert!(
            keys.iter().all(|k| k.starts_with("tenant-a/ui/")),
            "{keys:?}"
        );
        assert_eq!(IndexRoot::under("").unwrap(), IndexRoot::default());
        let err = IndexRoot::under("tenant-a").unwrap_err();
        assert!(err.to_string().contains("not an index prefix"), "{err}");
    }
}
