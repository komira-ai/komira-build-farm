//! `footers::read_footers`, the reader a storage node rebuilds its index with: it gets
//! back exactly the index the segment writer laid out, over the in-memory store and a
//! `file://` directory; it reads only each segment's tail; it reports and skips
//! objects that are not segments; and it keeps to its concurrency bound.

use std::collections::BTreeMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures::TryStreamExt as _;
use kbf_objstore::footers::{FooterRead, FooterReads, SkipReason, Totals, read_footers};
use kbf_objstore::{
    ByteRange, Capabilities, Floor, FsStore, KeyPrefix, ListPage, ListToken, MemoryStore,
    ObjectInfo, ObjectKey, ObjectStore, ObjectStoreError, PageSize,
};
use kbf_segments::layout::{MAGIC, TRAILER_LEN, VERSION, footer_len};
use kbf_segments::{MAX_SEGMENT_BYTES, SegmentError, SegmentWriter};
use kbf_types::Digest;

/// Where each blob is, as the writer laid it out: object, offset.
type Index = BTreeMap<Digest, (ObjectKey, u64)>;

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name).expect("a valid key")
}

fn prefix(p: &str) -> KeyPrefix {
    KeyPrefix::new(p).expect("a valid prefix")
}

/// Bytes that differ per blob, so no two blobs share a digest.
fn blob(segment: usize, record: usize, len: usize) -> Vec<u8> {
    let seed = (segment as u32) << 16 | record as u32;
    let mut bytes = seed.to_le_bytes().to_vec();
    bytes.extend((4..len).map(|i| (i as u32).wrapping_mul(31) as u8));
    bytes
}

/// Writes `segments` segments under `dir`, segment `s` holding `records(s)` blobs, and
/// returns the index the writer laid out. The oracle is independent of the footer: a
/// writer puts records back to back in the order they are pushed.
async fn write_segments<S: ObjectStore>(
    store: &S,
    dir: &str,
    segments: usize,
    records: impl Fn(usize) -> usize,
    record_len: usize,
) -> Index {
    let mut index = Index::new();
    for s in 0..segments {
        let k = key(&format!("{dir}{s:04}"));
        let mut writer = SegmentWriter::new(64 << 20);
        let mut offset = 0;
        for r in 0..records(s) {
            let bytes = blob(s, r, record_len + r % 7);
            let digest = writer.push(&bytes).expect("fits");
            index.insert(digest, (k.clone(), offset));
            offset += bytes.len() as u64;
        }
        store
            .put_new(&k, writer.finish().into(), None)
            .await
            .expect("put");
    }
    index
}

/// Reads every footer under `p`, failing on a listing error.
async fn read_all<S: ObjectStore>(store: &S, p: &KeyPrefix, reads: FooterReads) -> Vec<FooterRead> {
    read_footers(store, p, None, reads)
        .try_collect()
        .await
        .expect("listing")
}

/// The index the footers describe, and the skipped objects with why.
fn index_of(reads: &[FooterRead]) -> (Index, BTreeMap<String, String>) {
    let mut index = Index::new();
    let mut skipped = BTreeMap::new();
    for r in reads {
        match r {
            FooterRead::Segment(s) => {
                for e in s.footer.entries() {
                    let old = index.insert(e.digest, (s.key.clone(), e.offset));
                    assert!(old.is_none(), "{} read twice", e.digest);
                }
            }
            FooterRead::Skipped(s) => {
                skipped.insert(s.key.as_str().to_owned(), s.why.to_string());
            }
        }
    }
    (index, skipped)
}

fn reads(concurrency: usize, page: u16, tail: u64) -> FooterReads {
    FooterReads {
        concurrency: NonZeroUsize::new(concurrency).expect("not zero"),
        page: PageSize::new(page).expect("in range"),
        tail: NonZeroU64::new(tail).expect("not zero"),
    }
}

/// Catches: a reader that loses a page of the listing (pages of 7 over 40 segments),
/// misreads an offset or a length, drops or duplicates a record, or reads a segment
/// twice; and footers longer than the first read (up to 60 records, 4176 bytes, past
/// the 2 KiB tail) read wrong. The totals agree with the index.
#[tokio::test]
async fn a_rebuild_recovers_exactly_the_written_index() {
    let store = MemoryStore::new(Capabilities::default());
    let want = write_segments(&store, "cas/", 40, |s| 1 + s * 3 / 2, 200).await;
    // Outside the prefix: never read.
    write_segments(&store, "other/", 2, |_| 3, 50).await;

    let got = read_all(&store, &prefix("cas/"), reads(4, 7, 2048)).await;
    let (index, skipped) = index_of(&got);
    assert!(skipped.is_empty(), "{skipped:?}");
    assert_eq!(index, want);
    let mut totals = Totals::default();
    for r in &got {
        totals.add(r);
    }
    assert_eq!(totals.objects, 40);
    assert_eq!(totals.segments, 40);
    assert_eq!(totals.skipped, 0);
    assert_eq!(totals.entries, want.len() as u64);
    assert_eq!(
        totals.record_bytes,
        want.keys().map(|d| d.size_bytes).sum::<u64>()
    );
}

/// A fresh, absent directory for `name`, under the target directory.
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-objstore-footers")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Catches the same over a `file://` store, whose listing pages are cut from one walk
/// of the directory: a page that skips or repeats keys loses or doubles a segment.
#[tokio::test]
async fn a_rebuild_over_a_directory_recovers_the_written_index() {
    let dir = scratch("rebuild");
    let store = FsStore::open(&dir, Floor::Bytes(1)).expect("open");
    let want = write_segments(&store, "cas/", 25, |s| 1 + s % 5, 300).await;
    let got = read_all(&store, &prefix("cas/"), reads(3, 4, 4096)).await;
    let (index, skipped) = index_of(&got);
    assert!(skipped.is_empty(), "{skipped:?}");
    assert_eq!(index, want);
    drop(store);
    std::fs::remove_dir_all(&dir).expect("remove");
}

/// A trailer that passes every check of its own and claims `entries` records before
/// `index_offset` bytes: what a reader trusts before the footer CRC.
fn bare_trailer(index_offset: u64, entries: u32) -> Vec<u8> {
    let mut t = Vec::with_capacity(TRAILER_LEN);
    t.extend_from_slice(&index_offset.to_le_bytes());
    t.extend_from_slice(&entries.to_le_bytes());
    t.extend_from_slice(&[1, 0, 0, 0]);
    t.extend_from_slice(&0_u32.to_le_bytes());
    t.extend_from_slice(&VERSION.to_le_bytes());
    t.extend_from_slice(&MAGIC);
    t
}

/// Catches: a torn footer (one flipped byte), a short object, a foreign object, a
/// segment cut short (its last bytes lost), and a trailer claiming a footer longer than
/// its object or than any segment (2^32 - 1 records), each taken as a segment, or
/// panicking the reader, or ending the stream; the good segments beside them are all
/// still read.
#[tokio::test]
async fn damaged_short_and_foreign_objects_are_reported_and_skipped() {
    let store = MemoryStore::new(Capabilities::default());
    let want = write_segments(&store, "cas/", 3, |s| s + 1, 100).await;

    let mut torn = SegmentWriter::new(1 << 20);
    torn.push(&blob(9, 0, 500)).expect("fits");
    let torn = torn.finish();
    let mut flipped = torn.clone();
    flipped[600] ^= 0x40;
    let put = |name: &str, bytes: Vec<u8>| {
        let k = key(&format!("cas/{name}"));
        let store = &store;
        async move { store.put_new(&k, bytes.into(), None).await.expect("put") }
    };
    put("torn", flipped).await;
    put("cut", torn[..torn.len() - 1].to_vec()).await;
    put("short", vec![7; TRAILER_LEN - 1]).await;
    put("foreign", b"{\"not\": \"a segment\"}".repeat(100)).await;
    let mut huge = vec![0; 64];
    huge.extend(bare_trailer(64, u32::MAX));
    put("huge-count", huge).await;
    let mut longer = vec![0; 4000];
    longer.extend(bare_trailer(0, 100));
    put("longer-than-object", longer).await;

    let got = read_all(&store, &prefix("cas/"), reads(2, 3, 1024)).await;
    let (index, skipped) = index_of(&got);
    assert_eq!(index, want);
    let mut totals = Totals::default();
    for r in &got {
        totals.add(r);
    }
    assert_eq!((totals.objects, totals.segments, totals.skipped), (9, 3, 6));
    let names: Vec<(&str, &str)> = skipped
        .iter()
        .map(|(k, why)| (k.as_str(), why.split(':').next().unwrap_or(why)))
        .collect();
    assert_eq!(
        names,
        [
            ("cas/cut", "not a segment"),
            ("cas/foreign", "not a segment"),
            ("cas/huge-count", "not a segment"),
            ("cas/longer-than-object", "not a segment"),
            ("cas/short", "shorter than a segment trailer"),
            ("cas/torn", "not a segment"),
        ],
        "{skipped:?}"
    );
    assert!(skipped["cas/torn"].contains("CRC"), "{skipped:?}");
}

/// One object of `size` bytes, all zero but its last bytes, `tail`, served without
/// holding the object: a store for objects too large to keep in a test. It records
/// every range asked for and refuses (as a failed read) one longer than 1 MiB, so a
/// reader that would hold a huge footer reports that instead of allocating it.
struct Sparse {
    key: ObjectKey,
    size: u64,
    tail: Vec<u8>,
    asked: std::sync::Mutex<Vec<(u64, u64)>>,
}

impl ObjectStore for Sparse {
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    async fn put_new(
        &self,
        _: &ObjectKey,
        _: Bytes,
        _: Option<std::time::SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        Err(ObjectStoreError::Protocol("read-only".into()))
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        assert_eq!(*key, self.key);
        self.asked
            .lock()
            .expect("lock")
            .push((range.offset(), range.size()));
        let end = self.size.min(range.offset() + range.size());
        let len = end - range.offset();
        if len > 1 << 20 {
            return Err(ObjectStoreError::Protocol(format!("{len} bytes asked for")));
        }
        let tail_at = self.size - self.tail.len() as u64;
        let bytes = (range.offset()..end)
            .map(|i| i.checked_sub(tail_at).map_or(0, |t| self.tail[t as usize]))
            .collect::<Vec<u8>>();
        Ok(bytes.into())
    }

    async fn delete(&self, _: &ObjectKey) -> Result<(), ObjectStoreError> {
        Ok(())
    }

    async fn list(
        &self,
        _: &KeyPrefix,
        _: Option<&ListToken>,
        _: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        Ok(ListPage {
            objects: vec![ObjectInfo {
                key: self.key.clone(),
                size: self.size,
            }],
            next: None,
        })
    }
}

/// Catches a reader that drops the [`MAX_SEGMENT_BYTES`] bound on the second read: a
/// trailer whose record count makes its footer just over 128 MiB, in an object longer
/// than that, would have the reader fetch and hold the whole claimed footer (here
/// refused as a failed read, which a caller would retry forever). The reader makes its
/// one tail read and reports the footer as truncated: not a segment, needing the
/// claimed length and having the tail.
#[tokio::test]
async fn a_footer_longer_than_any_segment_is_not_read_past_the_tail() {
    let entries = (MAX_SEGMENT_BYTES - footer_len(0)) / 52 + 1;
    let claimed = footer_len(entries);
    assert!(claimed > MAX_SEGMENT_BYTES && footer_len(entries - 1) <= MAX_SEGMENT_BYTES);
    let size = claimed + 100;
    let store = Sparse {
        key: key("cas/huge-footer"),
        size,
        tail: bare_trailer(size - claimed, u32::try_from(entries).expect("fits")),
        asked: std::sync::Mutex::default(),
    };
    let tail = 64 << 10;
    let got = read_all(&store, &prefix("cas/"), reads(1, 10, tail)).await;
    let [FooterRead::Skipped(skipped)] = &got[..] else {
        panic!("{got:?}")
    };
    assert!(
        matches!(
            skipped.why,
            SkipReason::NotASegment(SegmentError::Truncated { needed, available })
                if needed == claimed && available == tail
        ),
        "{skipped:?}"
    );
    assert_eq!(*store.asked.lock().expect("lock"), [(size - tail, tail)]);
}

/// A store that counts the bytes it returns and the reads in flight, and can make a
/// read wait, fail, or find its object gone.
struct Counting {
    inner: MemoryStore,
    bytes: AtomicU64,
    in_flight: AtomicUsize,
    most_in_flight: AtomicUsize,
    delay: Duration,
    gone: Option<ObjectKey>,
    fail_read: Option<ObjectKey>,
    fail_list_after_first: AtomicBool,
}

impl Counting {
    fn new(delay: Duration) -> Self {
        Self {
            inner: MemoryStore::new(Capabilities::default()),
            bytes: AtomicU64::new(0),
            in_flight: AtomicUsize::new(0),
            most_in_flight: AtomicUsize::new(0),
            delay,
            gone: None,
            fail_read: None,
            fail_list_after_first: AtomicBool::new(false),
        }
    }
}

impl ObjectStore for Counting {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<std::time::SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.most_in_flight.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        if self.gone.as_ref() == Some(key) {
            return Err(ObjectStoreError::NotFound(key.clone()));
        }
        if self.fail_read.as_ref() == Some(key) {
            return Err(ObjectStoreError::Protocol("planted read failure".into()));
        }
        let got = self.inner.get_range(key, range).await?;
        self.bytes.fetch_add(got.len() as u64, Ordering::SeqCst);
        Ok(got)
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.inner.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        if after.is_some() && self.fail_list_after_first.load(Ordering::SeqCst) {
            return Err(ObjectStoreError::Protocol("planted list failure".into()));
        }
        self.inner.list(prefix, after, max_keys).await
    }
}

/// Catches a reader that fetches whole objects (or more than the footer and one tail
/// before it) to find their footers: 10 segments of 64 records of 4 KiB, each about
/// 260 KiB with a 4384-byte footer. A footer longer than the 1 KiB first read takes
/// exactly one more read of what is missing, so a segment costs its footer and no
/// more; `bytes_read` says the same.
#[tokio::test]
async fn a_footer_read_fetches_the_tail_only() {
    let store = Counting::new(Duration::ZERO);
    write_segments(&store, "cas/", 10, |_| 64, 4096).await;
    let stored: u64 = store
        .inner
        .list(&prefix("cas/"), None, PageSize::MAX)
        .await
        .expect("list")
        .objects
        .iter()
        .map(|o| o.size)
        .sum();
    store.bytes.store(0, Ordering::SeqCst);

    let got = read_all(&store, &prefix("cas/"), reads(4, 1000, 1024)).await;
    let footer = footer_len(64);
    assert_eq!(store.bytes.load(Ordering::SeqCst), 10 * footer);
    assert!(10 * footer * 50 < stored, "{stored} bytes stored");
    for r in &got {
        let FooterRead::Segment(s) = r else {
            panic!("{r:?}")
        };
        assert_eq!(s.bytes_read, footer, "{}", s.key);
    }

    // A first read longer than the footer reads that much once, and nothing more.
    store.bytes.store(0, Ordering::SeqCst);
    read_all(&store, &prefix("cas/"), reads(4, 1000, 8192)).await;
    assert_eq!(store.bytes.load(Ordering::SeqCst), 10 * 8192);
}

/// Catches a reader that ignores its bound (more reads in flight than asked) or does
/// not use it (one read at a time, so a rebuild over a remote store takes a round trip
/// per object): 12 objects, each read held 20 ms, at most 3 at once, and 3 reached.
#[tokio::test]
async fn reads_keep_to_the_concurrency_bound() {
    let store = Counting::new(Duration::from_millis(20));
    write_segments(&store, "cas/", 12, |_| 2, 100).await;
    let got = read_all(&store, &prefix("cas/"), reads(3, 5, 64 << 10)).await;
    assert_eq!(got.len(), 12);
    assert_eq!(store.most_in_flight.load(Ordering::SeqCst), 3);
}

/// Catches: an object deleted after it was listed, or a failed read, reported as
/// something else (a read failure taken for "not a segment" would let a caller drop a
/// segment it should read again) or ending the stream; and a failed listing call that
/// is swallowed (the caller would believe the rebuild complete).
#[tokio::test]
async fn gone_objects_failed_reads_and_failed_listings_are_told_apart() {
    let mut store = Counting::new(Duration::ZERO);
    store.gone = Some(key("cas/0001"));
    store.fail_read = Some(key("cas/0002"));
    let store = Arc::new(store);
    write_segments(&*store, "cas/", 4, |_| 1, 100).await;
    let got = read_all(&*store, &prefix("cas/"), FooterReads::default()).await;
    let why: BTreeMap<&str, &SkipReason> = got
        .iter()
        .filter_map(|r| match r {
            FooterRead::Skipped(s) => Some((s.key.as_str(), &s.why)),
            FooterRead::Segment(_) => None,
        })
        .collect();
    assert_eq!(why.len(), 2, "{why:?}");
    assert!(matches!(why["cas/0001"], SkipReason::Gone), "{why:?}");
    assert!(matches!(why["cas/0002"], SkipReason::Read(_)), "{why:?}");

    store.fail_list_after_first.store(true, Ordering::SeqCst);
    let cas = prefix("cas/");
    let mut stream = std::pin::pin!(read_footers(&*store, &cas, None, reads(1, 2, 1024)));
    let mut items = Vec::new();
    while let Some(item) = futures::StreamExt::next(&mut stream).await {
        items.push(item);
    }
    assert!(
        items
            .iter()
            .any(|i| matches!(i, Err(ObjectStoreError::Protocol(_)))),
        "{items:?}"
    );
}

/// A store whose reads of the first segment give way to the others many times first, so
/// with concurrency the first segment's footer is the last one ready.
struct SlowFirst(MemoryStore);

impl ObjectStore for SlowFirst {
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<std::time::SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.0.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        if key.as_str().ends_with("0000") {
            for _ in 0..50 {
                tokio::task::yield_now().await;
            }
        }
        self.0.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.0.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.0.list(prefix, after, max_keys).await
    }
}

/// Catches: a reader that yields objects out of key order (a caller that records "handled
/// up to key K" after the K-th result would then skip an object still being read), and one
/// that does not resume: a read from `ListToken::after` the last key handled yields
/// exactly the objects after it, the same as a read that was never stopped. Segments of
/// different sizes with a concurrency of 4, so the reads finish out of order unless the
/// stream orders them.
#[tokio::test]
async fn footers_come_in_key_order_and_resume_after_a_key() {
    let store = SlowFirst(MemoryStore::new(Capabilities::default()));
    write_segments(&store, "cas/", 24, |s| 1 + s % 5, 200).await;
    let cas = prefix("cas/");
    let all = read_all(&store, &cas, reads(4, 5, 2048)).await;
    let keys: Vec<String> = all
        .iter()
        .map(|r| match r {
            FooterRead::Segment(s) => s.key.as_str().to_owned(),
            FooterRead::Skipped(s) => s.key.as_str().to_owned(),
        })
        .collect();
    assert_eq!(keys.len(), 24);
    assert!(keys.windows(2).all(|w| w[0] < w[1]), "{keys:?}");

    let after = ListToken::after(&key(&keys[9]));
    let rest: Vec<FooterRead> = read_footers(&store, &cas, Some(after), reads(4, 5, 2048))
        .try_collect()
        .await
        .expect("listing");
    let resumed: Vec<&str> = rest
        .iter()
        .map(|r| match r {
            FooterRead::Segment(s) => s.key.as_str(),
            FooterRead::Skipped(s) => s.key.as_str(),
        })
        .collect();
    assert_eq!(
        resumed,
        keys[10..].iter().map(String::as_str).collect::<Vec<_>>()
    );
}
