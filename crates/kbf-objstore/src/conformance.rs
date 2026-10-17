//! The conformance suite: what any [`ObjectStore`] must do before kbf trusts it.
//!
//! [`run`] drives every check against one store under a scratch prefix and returns a
//! [`Report`]; it never panics on a misbehaving store. It checks the base contract
//! (round trip, ranged reads at the boundaries, a missing key, idempotent delete, list
//! paging (also past S3's 1000-key page), a listing that starts after a key or covers the whole bucket, and the footer
//! reads a rebuild makes; the listing checks in `list` page through every page size, a
//! prefix that matches nothing, nested keys, and keys written during a listing) and every claim in [`Capabilities`]: a store claiming conditional writes must
//! refuse an overwrite, a store claiming Object Lock must refuse deleting a retained
//! object and keep it readable, and a store not claiming Object Lock must refuse a
//! retention date rather than store the object unlocked.
//!
//! Objects written under Object Lock cannot be removed until their date (one hour), so
//! give each run a fresh prefix when running against a real store.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::num::{NonZeroU64, NonZeroUsize};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::{StreamExt as _, TryStreamExt as _, stream};
use kbf_segments::{SegmentWriter, layout::footer_len};
use kbf_types::Digest;

use crate::footers::{self, FooterRead, FooterReads};
use crate::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ListToken, ObjectInfo, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};

/// How long the Object Lock check retains its object.
const LOCK_FOR: Duration = Duration::from_secs(3600);
/// Objects the list check writes; with pages of [`LIST_PAGE`], the last page is short.
const LIST_OBJECTS: usize = 7;
const LIST_PAGE: u16 = 3;
/// Objects the page-cap check writes: one past S3's largest page.
const PAST_PAGE_CAP: usize = 1001;
/// How many of those writes are in flight at once.
const PAST_PAGE_CAP_PUTS: usize = 16;

/// One check in the suite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    /// `put_new` then reading the whole object returns the same bytes.
    RoundTrip,
    /// Ranged reads at the first byte, the last byte, across the end and past the end.
    RangeBoundaries,
    /// Reading an absent key is `NotFound`.
    MissingObject,
    /// Delete removes the object; deleting it again, or a key never written, succeeds.
    DeleteIdempotent,
    /// Paged listing returns every key under the prefix once, in order, in pages no
    /// larger than asked, and nothing outside the prefix.
    ListPagination,
    /// A listing [`ListToken::after`] a key (present or not) starts at the next key
    /// under the prefix and pages on from there.
    ListStartAfter,
    /// A listing of one key more than the largest page ([`PageSize::MAX`], S3's 1000)
    /// in pages of that size returns every key once, in order (so over at least two pages).
    ListPastPageCap,
    /// A listing under the empty prefix covers the whole bucket, in order, across pages
    /// (started after a key just before this run's own, so the check reads only those).
    ListEmptyPrefix,
    /// Six keys listed in pages of 1, 2, 3, 4, 6 and 7 (one key a page, exact
    /// multiples, a short last page, one page), page by page and streamed through
    /// [`listing::objects`](crate::listing::objects): every key and size once, in order,
    /// no page larger than asked.
    ListPageSizes,
    /// A prefix that matches no key, between two keys that share its text, lists
    /// nothing, page by page and streamed.
    ListNoMatch,
    /// Keys nested under `/` at several depths, and the characters either side of `/`:
    /// a listing returns every key under the prefix at any depth (no grouping at `/`),
    /// in byte order, under a prefix ending in `/` or not.
    ListNested,
    /// Between pages, a key written after the cursor and keys deleted behind and ahead
    /// of it: every key present throughout is listed exactly once, in order. Keys
    /// written or deleted during the listing may or may not be listed.
    ListDuringWrites,
    /// [`footers::read_footers`] over segments, a torn segment, a foreign object and a
    /// short one gives every segment's index exactly, skips the rest for the right
    /// reason, and reads only the tails.
    FooterReads,
    /// With conditional writes, `put_new` refuses an existing key and keeps its bytes.
    PutNewRefusesOverwrite,
    /// With Object Lock, a retained object refuses delete and stays readable; without
    /// it, a retention date is refused.
    ObjectLock,
    /// `put_stream` of an object of several parts, its body in pieces that do not line
    /// up with them, reads back whole; a body that fails, or ends short, stores
    /// nothing; with conditional writes, an existing key is refused and kept.
    StreamedPut,
}

/// The outcome of one check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The store did what the check requires.
    Passed,
    /// The check does not apply: the store does not claim the capability it tests.
    Skipped(&'static str),
    /// The store did something else; the text says what.
    Failed(String),
}

/// The outcome of every check, in the order they ran.
#[derive(Clone, Debug)]
pub struct Report {
    /// One entry per [`Check`].
    pub results: Vec<(Check, Outcome)>,
}

impl Report {
    /// Whether no check failed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.failures().next().is_none()
    }

    /// The checks that failed, with what went wrong.
    pub fn failures(&self) -> impl Iterator<Item = (Check, &str)> {
        self.results.iter().filter_map(|(c, o)| match o {
            Outcome::Failed(why) => Some((*c, why.as_str())),
            _ => None,
        })
    }

    /// The checks that were skipped.
    pub fn skipped(&self) -> impl Iterator<Item = Check> {
        self.results
            .iter()
            .filter_map(|(c, o)| matches!(o, Outcome::Skipped(_)).then_some(*c))
    }

    /// The outcome of `check`.
    #[must_use]
    pub fn outcome(&self, check: Check) -> Option<&Outcome> {
        self.results
            .iter()
            .find(|(c, _)| *c == check)
            .map(|(_, o)| o)
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (check, outcome) in &self.results {
            match outcome {
                Outcome::Passed => writeln!(f, "pass {check:?}")?,
                Outcome::Skipped(why) => writeln!(f, "skip {check:?}: {why}")?,
                Outcome::Failed(why) => writeln!(f, "FAIL {check:?}: {why}")?,
            }
        }
        Ok(())
    }
}

/// Runs every check against `store`, writing only under `prefix`.
pub async fn run<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> Report {
    let caps = store.capabilities();
    let mut results = Vec::new();
    let mut record = |check, r: Result<Outcome, String>| {
        results.push((check, r.unwrap_or_else(Outcome::Failed)));
    };
    record(Check::RoundTrip, round_trip(store, prefix).await);
    record(
        Check::RangeBoundaries,
        range_boundaries(store, prefix).await,
    );
    record(Check::MissingObject, missing_object(store, prefix).await);
    record(
        Check::DeleteIdempotent,
        delete_idempotent(store, prefix).await,
    );
    record(Check::ListPagination, list_pagination(store, prefix).await);
    record(Check::ListStartAfter, list_start_after(store, prefix).await);
    record(
        Check::ListPastPageCap,
        list_past_page_cap(store, prefix).await,
    );
    record(
        Check::ListEmptyPrefix,
        list_empty_prefix(store, prefix).await,
    );
    record(
        Check::ListPageSizes,
        list::list_page_sizes(store, prefix).await,
    );
    record(Check::ListNoMatch, list::list_no_match(store, prefix).await);
    record(Check::ListNested, list::list_nested(store, prefix).await);
    record(
        Check::ListDuringWrites,
        list::list_during_writes(store, prefix).await,
    );
    record(Check::FooterReads, footer_reads(store, prefix).await);
    record(
        Check::PutNewRefusesOverwrite,
        refuses_overwrite(store, prefix, caps).await,
    );
    record(Check::ObjectLock, object_lock(store, prefix, caps).await);
    record(Check::StreamedPut, streamed_put(store, prefix, caps).await);
    Report { results }
}

type CheckResult = Result<Outcome, String>;

macro_rules! ensure {
    ($cond:expr, $($why:tt)+) => {
        if !$cond {
            return Err(format!($($why)+));
        }
    };
}

mod list;

fn key(prefix: &KeyPrefix, name: &str) -> Result<ObjectKey, String> {
    prefix
        .key(name)
        .map_err(|e| format!("scratch key {name}: {e}"))
}

fn child(prefix: &KeyPrefix, name: &str) -> Result<KeyPrefix, String> {
    prefix
        .child(name)
        .map_err(|e| format!("scratch prefix {name}: {e}"))
}

fn range(offset: u64, len: u64) -> ByteRange {
    ByteRange::new(offset, len).expect("the suite's ranges are non-empty and small")
}

/// Bytes whose value at each offset differs from its neighbours', so a read at the
/// wrong offset or of the wrong length cannot match by accident.
fn pattern(len: usize, seed: u8) -> Bytes {
    (0..len)
        .map(|i| (i.wrapping_mul(31) as u8) ^ seed ^ ((i >> 8) as u8))
        .collect::<Vec<u8>>()
        .into()
}

async fn put<S: ObjectStore>(store: &S, key: &ObjectKey, body: &Bytes) -> Result<(), String> {
    store
        .put_new(key, body.clone(), None)
        .await
        .map_err(|e| format!("put_new {key}: {e}"))
}

async fn read<S: ObjectStore>(store: &S, key: &ObjectKey, r: ByteRange) -> Result<Bytes, String> {
    store
        .get_range(key, r)
        .await
        .map_err(|e| format!("get_range {key} {r:?}: {e}"))
}

async fn round_trip<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> CheckResult {
    for (name, len) in [("round-trip-1", 1), ("round-trip-big", 300_000)] {
        let k = key(prefix, name)?;
        let body = pattern(len, 7);
        put(store, &k, &body).await?;
        let got = read(store, &k, range(0, len as u64)).await?;
        ensure!(
            got == body,
            "{k}: read {} bytes that differ from the {len} written",
            got.len()
        );
    }
    Ok(Outcome::Passed)
}

async fn range_boundaries<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> CheckResult {
    const SIZE: u64 = 1000;
    let k = key(prefix, "ranges")?;
    let body = pattern(SIZE as usize, 3);
    put(store, &k, &body).await?;
    // (offset, len, the bytes the store must return)
    let cases = [
        (0, 1, 0..1),
        (SIZE - 1, 1, 999..1000),
        (1, 10, 1..11),
        (500, 100, 500..600),
        (0, SIZE, 0..1000),
        (990, 100, 990..1000), // runs past the end: cut at the end
        (0, SIZE + 5, 0..1000),
    ];
    for (offset, len, want) in cases {
        let r = range(offset, len);
        let got = read(store, &k, r).await?;
        let want = body.slice(want);
        ensure!(
            got == want,
            "{r:?} of a {SIZE}-byte object returned {} bytes, expected {} (bytes {}..{})",
            got.len(),
            want.len(),
            offset,
            offset + want.len() as u64,
        );
    }
    for offset in [SIZE, SIZE + 1] {
        match store.get_range(&k, range(offset, 1)).await {
            Err(ObjectStoreError::InvalidRange { .. }) => {}
            other => {
                return Err(format!(
                    "a range starting at {offset} of a {SIZE}-byte object: expected InvalidRange, got {}",
                    describe(&other)
                ));
            }
        }
    }
    Ok(Outcome::Passed)
}

async fn missing_object<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> CheckResult {
    let k = key(prefix, "never-written")?;
    match store.get_range(&k, range(0, 1)).await {
        Err(ObjectStoreError::NotFound(_)) => Ok(Outcome::Passed),
        other => Err(format!(
            "reading {k}, never written: expected NotFound, got {}",
            describe(&other)
        )),
    }
}

async fn delete_idempotent<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> CheckResult {
    let k = key(prefix, "deleted")?;
    put(store, &k, &pattern(10, 1)).await?;
    for attempt in ["first", "second"] {
        store
            .delete(&k)
            .await
            .map_err(|e| format!("{attempt} delete of {k}: {e}"))?;
        match store.get_range(&k, range(0, 1)).await {
            Err(ObjectStoreError::NotFound(_)) => {}
            other => {
                return Err(format!(
                    "{k} after the {attempt} delete: expected NotFound, got {}",
                    describe(&other)
                ));
            }
        }
    }
    let never = key(prefix, "never-deleted")?;
    store
        .delete(&never)
        .await
        .map_err(|e| format!("deleting {never}, never written: {e}"))?;
    Ok(Outcome::Passed)
}

async fn list_pagination<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> CheckResult {
    let dir = child(prefix, "list/")?;
    let mut want = BTreeSet::new();
    for i in 0..LIST_OBJECTS {
        let k = key(&dir, &format!("k{i}"))?;
        put(store, &k, &pattern(i + 1, 9)).await?;
        want.insert((k, i as u64 + 1));
    }
    // Shares the text `<prefix>list` but not `<prefix>list/`: must not be listed.
    put(store, &key(prefix, "list-outside")?, &pattern(1, 0)).await?;

    let (seen, pages) = list_from(store, &dir, None, LIST_PAGE, usize::MAX).await?;
    let keys: Vec<&ObjectKey> = seen.iter().map(|o| &o.key).collect();
    ensure!(
        keys.windows(2).all(|w| w[0] < w[1]),
        "listed keys are not strictly ascending (out of order or repeated): {keys:?}"
    );
    let got: BTreeSet<_> = seen.into_iter().map(|o| (o.key, o.size)).collect();
    let missing: Vec<_> = want.difference(&got).map(|(k, _)| k.as_str()).collect();
    let extra: Vec<_> = got.difference(&want).map(|(k, _)| k.as_str()).collect();
    ensure!(
        missing.is_empty() && extra.is_empty(),
        "listing {dir:?} in pages of {LIST_PAGE}: missing {missing:?}, unexpected {extra:?} (sizes included)"
    );
    let min_pages = LIST_OBJECTS.div_ceil(usize::from(LIST_PAGE));
    ensure!(
        pages >= min_pages,
        "{LIST_OBJECTS} keys came in {pages} pages of at most {LIST_PAGE}"
    );
    Ok(Outcome::Passed)
}

/// Lists `prefix` from `after` in pages of `page` keys until the listing ends or
/// `enough` keys came, failing past [`LIST_OBJECTS`] + 1 pages or on a page larger than
/// asked: the objects in the order they came, and how many pages.
async fn list_from<S: ObjectStore>(
    store: &S,
    prefix: &KeyPrefix,
    mut after: Option<ListToken>,
    page: u16,
    enough: usize,
) -> Result<(Vec<ObjectInfo>, usize), String> {
    let page_size = PageSize::new(page).expect("in range");
    let mut keys = Vec::new();
    let mut pages = 0;
    while keys.len() < enough {
        pages += 1;
        ensure!(
            pages <= LIST_OBJECTS + 1,
            "list did not end after {pages} pages"
        );
        let got = store
            .list(prefix, after.as_ref(), page_size)
            .await
            .map_err(|e| format!("list {prefix:?} page {pages}: {e}"))?;
        ensure!(
            got.objects.len() <= usize::from(page),
            "page {pages} has {} keys; at most {page} were asked for",
            got.objects.len()
        );
        keys.extend(got.objects);
        match got.next {
            Some(t) => after = Some(t),
            None => break,
        }
    }
    Ok((keys, pages))
}

async fn list_start_after<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> CheckResult {
    let dir = child(prefix, "after/")?;
    let mut all = Vec::new();
    for i in 0..LIST_OBJECTS {
        let k = key(&dir, &format!("k{i}"))?;
        put(store, &k, &pattern(1, 3)).await?;
        all.push(k);
    }
    // After a key that exists, and after one that does not but sorts between k2 and k3.
    for from in ["k2", "k2x"] {
        let after = ListToken::after(&key(&dir, from)?);
        let (got, _) = list_from(store, &dir, Some(after), LIST_PAGE, usize::MAX).await?;
        let got: Vec<ObjectKey> = got.into_iter().map(|o| o.key).collect();
        ensure!(
            got == all[3..],
            "listing {dir:?} after {from}: expected {:?}, got {got:?}",
            &all[3..]
        );
    }
    Ok(Outcome::Passed)
}

async fn list_past_page_cap<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> CheckResult {
    let dir = child(prefix, "many/")?;
    let mut want = Vec::with_capacity(PAST_PAGE_CAP);
    for i in 0..PAST_PAGE_CAP {
        want.push(key(&dir, &format!("m{i:04}"))?);
    }
    let body = pattern(1, 5);
    stream::iter(want.iter().map(|k| put(store, k, &body)))
        .buffer_unordered(PAST_PAGE_CAP_PUTS)
        .try_collect::<()>()
        .await?;
    let (got, pages) = list_from(store, &dir, None, PageSize::MAX.get(), usize::MAX).await?;
    let got: Vec<ObjectKey> = got.into_iter().map(|o| o.key).collect();
    if got != want {
        let (first, last) = (got.first(), got.last());
        return Err(format!(
            "listing {PAST_PAGE_CAP} keys under {dir:?} in pages of {}: got {} keys in {pages} pages, first {first:?}, last {last:?}",
            PageSize::MAX.get(),
            got.len()
        ));
    }
    Ok(Outcome::Passed)
}

async fn list_empty_prefix<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> CheckResult {
    let dir = child(prefix, "empty/")?;
    let mut want = Vec::new();
    for i in 0..5 {
        let k = key(&dir, &format!("e{i}"))?;
        put(store, &k, &pattern(1, 4)).await?;
        want.push(k);
    }
    // `<prefix>empty` sorts just before `<prefix>empty/e0`, and no key of this run lies
    // between them, so the whole-bucket listing from there starts with `want`.
    let after = ListToken::after(&key(prefix, "empty")?);
    let (got, _) = list_from(store, &KeyPrefix::default(), Some(after), 2, want.len()).await?;
    let got: Vec<ObjectKey> = got.into_iter().map(|o| o.key).collect();
    ensure!(
        got.get(..want.len()) == Some(&want[..]),
        "listing the whole bucket after {prefix:?}empty: expected {want:?} first, got {got:?}"
    );
    Ok(Outcome::Passed)
}

/// The tail the footer check reads first: longer than the one-record segment's footer
/// (1108 bytes), shorter than the nine-record one's (1524), which takes a second read.
const FOOTER_TAIL: u64 = 1200;

async fn footer_reads<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> CheckResult {
    let dir = child(prefix, "footers/")?;
    // What the segments hold, as their writer laid them out: records back to back.
    let mut want: BTreeMap<Digest, (ObjectKey, u64)> = BTreeMap::new();
    let mut footers_len = BTreeMap::new();
    for (name, records) in [("seg-a", 1_usize), ("seg-b", 9), ("seg-torn", 1)] {
        let k = key(&dir, name)?;
        let mut writer = SegmentWriter::new(1 << 20);
        let mut offset = 0;
        let mut index = Vec::new();
        for r in 0..records {
            let blob = pattern(100 + r, name.len() as u8 + r as u8);
            let digest = writer.push(&blob).map_err(|e| format!("{name}: {e}"))?;
            index.push((digest, (k.clone(), offset)));
            offset += blob.len() as u64;
        }
        let mut bytes = writer.finish();
        if name == "seg-torn" {
            // The first byte of the fan-out flipped: the trailer reads, the footer's
            // CRC does not.
            bytes[offset as usize] ^= 1;
        } else {
            want.extend(index);
            footers_len.insert(k.clone(), footer_len(records as u64));
        }
        put(store, &k, &bytes.into()).await?;
    }
    put(store, &key(&dir, "foreign")?, &pattern(4096, 8)).await?;
    put(store, &key(&dir, "short")?, &pattern(10, 9)).await?;

    let reads = FooterReads {
        concurrency: NonZeroUsize::new(2).expect("not zero"),
        page: PageSize::new(2).expect("in range"),
        tail: NonZeroU64::new(FOOTER_TAIL).expect("not zero"),
    };
    let read: Vec<FooterRead> = footers::read_footers(store, &dir, None, reads)
        .try_collect()
        .await
        .map_err(|e| format!("reading footers under {dir:?}: {e}"))?;
    let mut got = BTreeMap::new();
    let mut skipped = BTreeMap::new();
    for r in read {
        match r {
            FooterRead::Segment(s) => {
                // The first read, or the footer if it is longer: never the records
                // before them.
                let most = footers_len[&s.key].max(FOOTER_TAIL);
                ensure!(
                    s.bytes_read <= most,
                    "{}: read {} bytes for its footer; at most {most} are needed",
                    s.key,
                    s.bytes_read
                );
                for e in s.footer.entries() {
                    got.insert(e.digest, (s.key.clone(), e.offset));
                }
            }
            FooterRead::Skipped(s) => {
                // The reason without its detail: "not a segment: <why>" or another.
                let why = s.why.to_string();
                let why = why.split(':').next().unwrap_or_default().to_owned();
                let name = s.key.as_str().rsplit('/').next().unwrap_or_default();
                skipped.insert(name.to_owned(), why);
            }
        }
    }
    ensure!(
        got == want,
        "segment indexes read back: expected {want:?}, got {got:?}"
    );
    let want_skipped: BTreeMap<String, String> = [
        ("foreign", "not a segment"),
        ("seg-torn", "not a segment"),
        ("short", "shorter than a segment trailer"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    ensure!(
        skipped == want_skipped,
        "skipped objects: expected {want_skipped:?}, got {skipped:?}"
    );
    Ok(Outcome::Passed)
}

async fn refuses_overwrite<S: ObjectStore>(
    store: &S,
    prefix: &KeyPrefix,
    caps: Capabilities,
) -> CheckResult {
    if !caps.conditional_put {
        return Ok(Outcome::Skipped(
            "the store does not claim conditional writes",
        ));
    }
    let k = key(prefix, "put-once")?;
    let first = pattern(64, 1);
    put(store, &k, &first).await?;
    match store.put_new(&k, pattern(64, 2), None).await {
        Err(ObjectStoreError::AlreadyExists(_)) => {}
        other => {
            return Err(format!(
                "a second put_new of {k}: expected AlreadyExists, got {}",
                describe(&other)
            ));
        }
    }
    let got = read(store, &k, range(0, 64)).await?;
    ensure!(got == first, "{k} changed after a refused overwrite");
    Ok(Outcome::Passed)
}

async fn object_lock<S: ObjectStore>(
    store: &S,
    prefix: &KeyPrefix,
    caps: Capabilities,
) -> CheckResult {
    let k = key(prefix, "locked")?;
    let body = pattern(32, 5);
    let until = SystemTime::now() + LOCK_FOR;
    if !caps.object_lock {
        return match store.put_new(&k, body, Some(until)).await {
            Err(ObjectStoreError::Unsupported(_)) => Ok(Outcome::Passed),
            other => Err(format!(
                "a retention date on a store without Object Lock: expected Unsupported, got {}",
                describe(&other)
            )),
        };
    }
    store
        .put_new(&k, body.clone(), Some(until))
        .await
        .map_err(|e| format!("put_new {k} with retention: {e}"))?;
    if store.delete(&k).await.is_ok() {
        // A success is a failure whatever the bytes did: kbf would believe them gone.
        // A plain DELETE on a versioned bucket succeeds by adding a delete marker, so the
        // object stops reading while an old version may keep the bytes.
        return Err(match store.get_range(&k, range(0, 32)).await {
            Ok(b) if b == body => format!(
                "delete of retained {k} reported success (the bytes survive, but kbf would believe them gone)"
            ),
            other => format!(
                "delete of retained {k} succeeded and the object no longer reads (removed, or hidden by a delete marker): {}",
                describe(&other)
            ),
        });
    }
    let got = read(store, &k, range(0, 32)).await?;
    ensure!(got == body, "{k} changed after a refused delete");
    Ok(Outcome::Passed)
}

fn describe<T>(r: &Result<T, ObjectStoreError>) -> String {
    match r {
        Ok(_) => "success".to_owned(),
        Err(e) => format!("error: {e}"),
    }
}

/// The length of the streamed object: three S3 parts, the last short.
const STREAMED_LEN: usize = 2 * crate::s3::PART_BYTES as usize + 12_345;

/// The pieces a streamed body comes in: not a divisor of the part size.
const STREAMED_PIECE: usize = (1 << 20) + 13;

/// `bytes` as a body of `STREAMED_PIECE`-byte pieces, then `tail` (an error) if given.
fn streamed(bytes: &Bytes, tail: Option<ObjectStoreError>) -> BodyStream {
    let mut pieces: Vec<Result<Bytes, ObjectStoreError>> = (0..bytes.len())
        .step_by(STREAMED_PIECE)
        .map(|at| Ok(bytes.slice(at..(at + STREAMED_PIECE).min(bytes.len()))))
        .collect();
    pieces.extend(tail.map(Err));
    futures::stream::iter(pieces).boxed()
}

async fn streamed_put<S: ObjectStore>(
    store: &S,
    prefix: &KeyPrefix,
    caps: Capabilities,
) -> CheckResult {
    let body = pattern(STREAMED_LEN, 11);
    let len = body.len() as u64;
    let k = key(prefix, "streamed")?;
    let wrote = store.put_stream(&k, len, streamed(&body, None)).await;
    ensure!(wrote.is_ok(), "put_stream {k}: {wrote:?}");
    const WINDOW: usize = 4 << 20;
    for at in (0..body.len()).step_by(WINDOW) {
        let n = WINDOW.min(body.len() - at);
        let got = read(store, &k, range(at as u64, n as u64)).await?;
        ensure!(
            got == body.slice(at..at + n),
            "{k}: bytes {at}..{} read back other than streamed",
            at + n
        );
    }
    let failed = key(prefix, "streamed-failed")?;
    let broke = ObjectStoreError::Body("the source broke".into());
    let wrote = store
        .put_stream(&failed, len, streamed(&body, Some(broke)))
        .await;
    ensure!(wrote.is_err(), "{failed}: a body that failed was written");
    absent(store, &failed).await?;
    let short = key(prefix, "streamed-short")?;
    let wrote = store
        .put_stream(&short, len + 1, streamed(&body, None))
        .await;
    let refused = matches!(wrote, Err(ObjectStoreError::Body(_)));
    ensure!(refused, "{short}: a short body answered {wrote:?}");
    absent(store, &short).await?;
    if !caps.conditional_put {
        return Ok(Outcome::Passed);
    }
    let other = pattern(STREAMED_LEN, 12);
    let again = store.put_stream(&k, len, streamed(&other, None)).await;
    let refused = matches!(again, Err(ObjectStoreError::AlreadyExists(_)));
    ensure!(refused, "{k}: a streamed overwrite answered {again:?}");
    let kept = read(store, &k, range(0, 64)).await?;
    ensure!(
        kept == body.slice(..64),
        "{k}: a refused overwrite changed it"
    );
    Ok(Outcome::Passed)
}

/// Fails unless reading `k` finds no object.
async fn absent<S: ObjectStore>(store: &S, k: &ObjectKey) -> Result<(), String> {
    match store.get_range(k, range(0, 1)).await {
        Err(ObjectStoreError::NotFound(_)) => Ok(()),
        other => Err(format!("{k}: a write that failed left {other:?}")),
    }
}
