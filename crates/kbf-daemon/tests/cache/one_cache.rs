//! The input cache as the node's one cache with local storage (`CacheConfig::local`,
//! `kbf_storage::local_cache`): one budget for its entries and local storage's bytes,
//! its entries reported to the one cache's fill, pins honoured whatever local storage
//! holds; and every fill written into its file as the blob arrives, never held whole.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use kbf_daemon::cache::{Cache, LocalCache};
use kbf_daemon::cas::{Cas, CasError, label};
use kbf_proto::reapi::Digest;
use kbf_storage::local_cache::Fill;
use tokio::io::AsyncWriteExt as _;

use crate::support::cache_kit::{
    CountingCas, FakeClock, Gates, config, digest, entries, file, key, lease, stop, tmp_files,
};
use crate::support::scratch;

const GIB: u64 = 1 << 30;

/// A one cache whose local storage holds what the returned counter says.
fn one_cache() -> (Arc<LocalCache>, Arc<AtomicU64>) {
    let local = Arc::new(LocalCache::new());
    let storage = Arc::new(AtomicU64::new(0));
    let held = Arc::clone(&storage);
    assert!(local.attach_storage(Box::new(move || held.load(Ordering::SeqCst))));
    (local, storage)
}

/// Catches a budget that leaves local storage's bytes out (the input cache and the hot
/// store would together hold more than the disk's share: the "two budgets" mutant), the
/// one cache's fill missing the entries, and a fill that does not make room: with a
/// budget of 300 bytes, three entries of 100 fit until local storage holds 150; then the
/// two least recently used go, the stats say what local storage holds, and the fill is
/// each half once; a later fill makes room the same way.
#[tokio::test]
async fn entries_make_room_for_local_storage() {
    let dir = scratch("cache-one-budget");
    let gates = Gates::new();
    let clock = FakeClock::at(1000);
    let (local, storage) = one_cache();
    let mut config = config(&dir, 300, Arc::clone(&gates), &clock);
    config.local = Some(Arc::clone(&local));
    let cache = Cache::open(config).expect("open");
    let cas = CountingCas::default();
    for tag in 1..=3_u8 {
        clock.advance(10);
        let root = cas.tree(&[file("f", &[tag; 100], false)]);
        lease(&cas, &cache, &root, &dir, &format!("l{tag}"))
            .await
            .expect("lease");
    }
    assert_eq!(cache.stats().bytes, 300);
    let full = Fill {
        inputs: 300,
        storage: 0,
    };
    assert_eq!(local.fill(), full);
    storage.store(150, Ordering::SeqCst);
    cache.evict();
    let stats = cache.stats();
    assert_eq!(
        (stats.bytes, stats.storage_bytes, stats.evictions),
        (100, 150, 2)
    );
    assert!(cache.holds(&key(&[3; 100], false)));
    assert_eq!(local.fill().total(), 250);
    storage.store(200, Ordering::SeqCst);
    clock.advance(10);
    let root = cas.tree(&[file("f", &[4; 100], false)]);
    lease(&cas, &cache, &root, &dir, "l4").await.expect("lease");
    assert!(cache.holds(&key(&[4; 100], false)));
    assert!(!cache.holds(&key(&[3; 100], false)), "no room was made");
    assert_eq!(cache.stats().bytes, 100);
    stop(cache, &gates);
}

/// Catches an entry a running lease pins evicted to make room for local storage (the
/// lease's input would vanish from under a later link, and the bytes stay on disk
/// through its links anyway): with local storage over the whole budget, only the pinned
/// entry is left; once the pin goes, it goes too.
#[tokio::test]
async fn a_pinned_input_is_never_evicted_for_local_storage() {
    let dir = scratch("cache-one-pinned");
    let gates = Gates::new();
    let clock = FakeClock::at(1000);
    let (local, storage) = one_cache();
    let mut config = config(&dir, 300, Arc::clone(&gates), &clock);
    config.local = Some(local);
    let cache = Cache::open(config).expect("open");
    let cas = CountingCas::default();
    for tag in 1..=2_u8 {
        clock.advance(10);
        let root = cas.tree(&[file("f", &[tag; 100], false)]);
        lease(&cas, &cache, &root, &dir, &format!("l{tag}"))
            .await
            .expect("lease");
    }
    let (pinned, other) = (key(&[1; 100], false), key(&[2; 100], false));
    let pins = cache.pin(vec![pinned]);
    storage.store(1000, Ordering::SeqCst);
    cache.evict();
    assert!(cache.holds(&pinned), "the pinned input was evicted");
    assert!(!cache.holds(&other));
    drop(pins);
    cache.evict();
    assert!(!cache.holds(&pinned));
    assert_eq!(cache.stats().bytes, 0);
    stop(cache, &gates);
}

/// Catches the input cache's entries reported to the one cache other than live, a
/// second input cache on the same one cache counted (two caches would share one
/// budget's count), and a stopped cache still reported: the first cache's entries are
/// the fill's inputs, the second's are not, and once the first is gone they read 0.
#[tokio::test]
async fn the_one_cache_counts_one_input_cache_while_it_lives() {
    let (dir, other) = (scratch("cache-one-fill"), scratch("cache-one-second"));
    let (gates, clock) = (Gates::new(), FakeClock::at(1000));
    let local = Arc::new(LocalCache::new());
    let mut first = config(&dir, GIB, Arc::clone(&gates), &clock);
    first.local = Some(Arc::clone(&local));
    let cache = Cache::open(first).expect("open");
    let mut second = config(&other, GIB, Arc::clone(&gates), &clock);
    second.local = Some(Arc::clone(&local));
    let second = Cache::open(second).expect("open");
    let cas = CountingCas::default();
    let root = cas.tree(&[file("f", &[7; 64], false)]);
    lease(&cas, &second, &root, &other, "second")
        .await
        .expect("lease");
    assert_eq!(local.input_bytes(), 0, "the second cache was counted");
    lease(&cas, &cache, &root, &dir, "first")
        .await
        .expect("lease");
    assert_eq!(local.input_bytes(), 64);
    stop(cache, &gates);
    assert_eq!(local.input_bytes(), 0, "a stopped cache is still counted");
    stop(second, &gates);
}

/// The size of each write [`Chunked`] makes into a fill's file.
const CHUNK: usize = 64 << 10;

/// A CAS that writes a blob into a fill's file a chunk at a time, checking after each
/// that the file holds every byte so far (the fill's destination is the file itself,
/// not a buffer), counting its chunks; and that counts whole-blob reads of file blobs
/// (through [`CountingCas`]). Its first `read_to` fails as a full disk would, if asked.
struct Chunked {
    cas: CountingCas,
    chunks: AtomicUsize,
    fail_first: AtomicBool,
}

impl Chunked {
    fn new(fail_first: bool) -> Self {
        Self {
            cas: CountingCas::default(),
            chunks: AtomicUsize::new(0),
            fail_first: AtomicBool::new(fail_first),
        }
    }
}

impl Cas for Chunked {
    type Lease = Arc<Self>;

    fn lease(self: &Arc<Self>, _: &kbf_types::LeaseId) -> Arc<Self> {
        Arc::clone(self)
    }

    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        self.cas.get(digest).await
    }

    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        self.cas.put(bytes).await
    }

    async fn read_to(&self, digest: &Digest, file: &mut tokio::fs::File) -> Result<(), CasError> {
        if self.fail_first.swap(false, Ordering::SeqCst) {
            return Err(CasError::Write(label(digest), "no space".to_owned()));
        }
        let bytes = self
            .cas
            .blobs
            .blob(digest)
            .ok_or(CasError::Missing(label(digest)))?;
        let mut written = 0;
        for chunk in bytes.chunks(CHUNK) {
            file.write_all(chunk).await.expect("write");
            file.flush().await.expect("flush");
            written += chunk.len() as u64;
            let on_disk = file.metadata().await.expect("stat").len();
            assert_eq!(on_disk, written, "the chunk is not in the file");
            self.chunks.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// Catches a fill that reads the blob whole (the "fetch, then write" mutant: a blob as
/// large as the disk's share would be held in memory) or writes it anywhere but its
/// file: a blob of 5 MiB and 3 bytes (more than any buffer of the read path) arrives in
/// 81 chunks of 64 KiB, each in the entry's file before the next, with no whole read of
/// it; the executable copy of the same blob is then copied from the entry, not fetched.
#[tokio::test]
async fn a_fill_writes_the_blob_into_its_file_as_it_arrives() {
    let dir = scratch("cache-one-stream");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    let cas = Chunked::new(false);
    let blob: Vec<u8> = (0..(5 << 20) + 3).map(|i: u32| (i % 251) as u8).collect();
    let root = cas
        .cas
        .tree(&[file("plain", &blob, false), file("x", &blob, true)]);
    let lease_dir = crate::support::cache_kit::lease_dir(&dir, "big");
    let stats = kbf_daemon::tree::materialize_cached(&cas, &cache, &root, &lease_dir, &[])
        .await
        .expect("written");
    assert_eq!(cas.cas.reads_of(&digest(&blob)), 0, "a whole read");
    assert_eq!(
        cas.chunks.load(Ordering::SeqCst),
        blob.len().div_ceil(CHUNK)
    );
    assert_eq!(
        stats.fetched_bytes,
        blob.len() as u64,
        "the sibling was fetched"
    );
    for name in ["plain", "x"] {
        assert_eq!(std::fs::read(lease_dir.join(name)).expect("read"), blob);
    }
    assert!(cache.holds(&key(&blob, false)) && cache.holds(&key(&blob, true)));
    assert_eq!(entries(&dir).len(), 2);
    stop(cache, &gates);
}

/// Catches a fill whose write into `tmp/` fails (a full disk) failing the lease or
/// leaving its file, and a lease's own fetch that reads the blob whole (the "fetch,
/// then write" mutant of the fallback): the lease fetches the blob into its own file,
/// for a blob of one chunk and one of 81 chunks of 64 KiB each in the file before the
/// next, with no whole read; each of its names is right and linked to one inode,
/// nothing is indexed, and `tmp/` is empty.
#[tokio::test]
async fn a_fill_that_cannot_write_its_file_leaves_the_lease_to_fetch() {
    use std::os::unix::fs::MetadataExt as _;
    let dir = scratch("cache-one-full");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    let big: Vec<u8> = (0..(5 << 20) + 3).map(|i: u32| (i % 249) as u8).collect();
    for blob in [b"the bytes".to_vec(), big] {
        let cas = Chunked::new(true);
        let root = cas
            .cas
            .tree(&[file("a", &blob, false), file("b", &blob, false)]);
        let lease_dir = crate::support::cache_kit::lease_dir(&dir, &format!("full-{}", blob.len()));
        let stats = kbf_daemon::tree::materialize_cached(&cas, &cache, &root, &lease_dir, &[])
            .await
            .expect("the lease ran without the cache");
        let ino = |name: &str| std::fs::metadata(lease_dir.join(name)).expect("stat").ino();
        assert_eq!(ino("a"), ino("b"));
        assert_eq!(std::fs::read(lease_dir.join("b")).expect("read"), blob);
        let fetched = blob.len() as u64;
        assert_eq!((stats.link_fallbacks, stats.fetched_bytes), (1, fetched));
        assert_eq!(cas.cas.reads_of(&digest(&blob)), 0, "a whole read");
        assert_eq!(
            cas.chunks.load(Ordering::SeqCst),
            blob.len().div_ceil(CHUNK)
        );
        assert!(!cache.holds(&key(&blob, false)));
        assert_eq!(tmp_files(&dir), 0);
    }
    stop(cache, &gates);
}

/// Catches a lease's own fetch (the cache could not write) whose failure is taken for
/// success or for an I/O error: a blob the CAS lacks fails the lease with the CAS's
/// error.
#[tokio::test]
async fn a_lease_whose_own_fetch_fails_fails_with_the_cas_error() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch("cache-one-missing");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    let cas = CountingCas::default();
    let root = cas.tree(&[file("a", b"gone", false)]);
    cas.blobs.remove(&digest(b"gone"));
    let tmp = dir.join("cache/v1/tmp");
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o500)).expect("chmod");
    let outcome = lease(&cas, &cache, &root, &dir, "missing").await;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    assert!(
        matches!(
            outcome,
            Err(kbf_daemon::tree::TreeError::Cas(CasError::Missing(_)))
        ),
        "{outcome:?}"
    );
    stop(cache, &gates);
}

/// Catches a fill the cache could not rename into `blobs/` (its prefix directory not
/// writable) failing the lease, served from anywhere but its checked file, or left in
/// `tmp/`: the lease's names are right and linked to one inode of their own, a private
/// name is its own writable copy, nothing is indexed, and `tmp/` is empty.
#[tokio::test]
async fn a_fill_the_cache_cannot_rename_is_copied_then_removed() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let dir = scratch("cache-one-loose");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    let cas = CountingCas::default();
    let bytes = b"kept loose";
    let k = key(bytes, false);
    let root = cas.tree(&[
        file("a", bytes, false),
        file("b", bytes, false),
        file("out/c", bytes, false),
    ]);
    let prefix = dir.join("cache/v1/blobs").join(k.prefix());
    std::fs::set_permissions(&prefix, std::fs::Permissions::from_mode(0o500)).expect("chmod");
    let at = crate::support::cache_kit::lease_dir(&dir, "loose");
    let outcome =
        kbf_daemon::tree::materialize_cached(&cas, &cache, &root, &at, &["out".to_owned()]).await;
    std::fs::set_permissions(&prefix, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    let stats = outcome.expect("the lease ran without the cache");
    let meta = |name: &str| std::fs::metadata(at.join(name)).expect("stat");
    assert_eq!(meta("a").ino(), meta("b").ino());
    assert_ne!(meta("a").ino(), meta("out/c").ino());
    assert_eq!(meta("out/c").permissions().mode() & 0o777, 0o644);
    for name in ["a", "b", "out/c"] {
        assert_eq!(std::fs::read(at.join(name)).expect("read"), bytes);
    }
    assert_eq!(stats.link_fallbacks, 1);
    assert!(!cache.holds(&k));
    assert_eq!(tmp_files(&dir), 0, "the checked file was left in tmp/");
    // A key whose names are all private: its own writable copy, and no fallback.
    let only = key(b"private only", false);
    let root = cas.tree(&[file("out/d", b"private only", false)]);
    let prefix = dir.join("cache/v1/blobs").join(only.prefix());
    std::fs::set_permissions(&prefix, std::fs::Permissions::from_mode(0o500)).expect("chmod");
    let at = crate::support::cache_kit::lease_dir(&dir, "loose-private");
    let outcome =
        kbf_daemon::tree::materialize_cached(&cas, &cache, &root, &at, &["out".to_owned()]).await;
    std::fs::set_permissions(&prefix, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    assert_eq!(outcome.expect("written").link_fallbacks, 0);
    assert_eq!(
        std::fs::read(at.join("out/d")).expect("read"),
        b"private only"
    );
    let mode = std::fs::metadata(at.join("out/d"))
        .expect("stat")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o644);
    assert_eq!(tmp_files(&dir), 0);
    stop(cache, &gates);
}

/// What `clonefile` does, off macOS: a new file (an existing name is an error) with
/// the source's bytes and mode.
#[cfg(not(target_os = "macos"))]
fn copying_clone(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    let mut source = std::fs::File::open(from)?;
    let mut clone = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
    std::io::copy(&mut source, &mut clone)?;
    clone.set_permissions(source.metadata()?.permissions())
}

/// Catches, in clone mode, a fill that is not an entry (the cache could not rename it,
/// or could not write it at all) giving a lease's names anything but their own
/// writable files, or failing the lease: for an entry, a checked file the cache could
/// not keep, and a lease's own fetch, every name (shared or private) is its own inode,
/// 0644 (0755 executable), with the bytes; and only the entry is indexed.
#[tokio::test]
async fn in_clone_mode_every_fill_gives_each_name_its_own_writable_file() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let dir = scratch("cache-one-clone");
    let gates = Gates::new();
    let mut config = config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000));
    config.link_mode = Some(kbf_daemon::cache::LinkMode::Clone);
    #[cfg(not(target_os = "macos"))]
    {
        config.link = Arc::new(copying_clone);
    }
    let cache = Cache::open(config).expect("open");
    let cas = CountingCas::default();
    let chmod = |path: &std::path::Path, mode| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    };
    for (case, bytes) in [
        ("entry", &b"an entry"[..]),
        ("loose", b"kept loose"),
        ("fetch", b"own fetch"),
    ] {
        let k = key(bytes, false);
        let blocked = match case {
            "loose" => Some(dir.join("cache/v1/blobs").join(k.prefix())),
            "fetch" => Some(dir.join("cache/v1/tmp")),
            _ => None,
        };
        let root = cas.tree(&[
            file("a", bytes, false),
            file("b", bytes, true),
            file("out/c", bytes, false),
        ]);
        let at = crate::support::cache_kit::lease_dir(&dir, case);
        if let Some(path) = &blocked {
            chmod(path, 0o500);
        }
        let outcome =
            kbf_daemon::tree::materialize_cached(&cas, &cache, &root, &at, &["out".to_owned()])
                .await;
        if let Some(path) = &blocked {
            chmod(path, 0o700);
        }
        outcome.expect(case);
        let meta = |name: &str| std::fs::metadata(at.join(name)).expect("stat");
        for (name, mode) in [("a", 0o644), ("b", 0o755), ("out/c", 0o644)] {
            assert_eq!(
                meta(name).permissions().mode() & 0o777,
                mode,
                "{case} {name}"
            );
            assert_eq!(
                std::fs::read(at.join(name)).expect("read"),
                bytes,
                "{case} {name}"
            );
        }
        assert_ne!(meta("a").ino(), meta("out/c").ino(), "{case}");
        assert_eq!(cache.holds(&k), case == "entry", "{case}");
    }
    assert_eq!(tmp_files(&dir), 0);
    stop(cache, &gates);
}

/// Catches a budget that reacts to local storage only when an input is fetched (the
/// "no room check" mutant: on a node that only uploads, the hot store would grow past
/// the share the budget leaves it while the entries stay): once the first adopt pass
/// (and its eviction round) is over, three entries of 100 fill a budget of 300; local
/// storage then grows to 150 with no lease running, and within seconds the two least
/// recently used entries are gone on the cache's own thread.
#[tokio::test]
async fn local_storage_that_grows_makes_room_with_no_fill() {
    let dir = scratch("cache-one-room");
    let gates = Gates::running();
    let clock = FakeClock::at(1000);
    let (local, storage) = one_cache();
    let mut config = config(&dir, 300, Arc::clone(&gates), &clock);
    config.local = Some(local);
    config.room_every = std::time::Duration::from_millis(20);
    let cache = Cache::open(config).expect("open");
    // The first pass ends with an eviction round and the second snapshot; after it,
    // only the room checks evict (the next pass and snapshot are an hour away).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while cache.stats().snapshots < 2 {
        assert!(std::time::Instant::now() < deadline, "no first pass");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let cas = CountingCas::default();
    for tag in 1..=3_u8 {
        clock.advance(10);
        let root = cas.tree(&[file("f", &[tag; 100], false)]);
        lease(&cas, &cache, &root, &dir, &format!("l{tag}"))
            .await
            .expect("lease");
    }
    assert_eq!(cache.stats().bytes, 300);
    storage.store(150, Ordering::SeqCst);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while cache.stats().bytes > 150 {
        assert!(std::time::Instant::now() < deadline, "no room was made");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(cache.stats().bytes, 100);
    assert!(cache.holds(&key(&[3; 100], false)));
    stop(cache, &gates);
}
