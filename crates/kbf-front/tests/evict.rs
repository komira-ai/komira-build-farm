//! Eviction from a file store under disk pressure (`Cache::evict_round`): which
//! objects go (the least recently used, as FindMissing, reads and writes count use),
//! which stay (used within the grace window, staged), how many (the shortfall to twice
//! the floor), and in what order the index and the files change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use kbf_front::{
    Cache, CacheError, EVICT_GRACE, EvictReady, EvictRound, HeldObject, MemoryMetaLog, MetaLog,
    MetaLogError, VerifiedBlob,
};
use kbf_meta::{
    Applied, BlobAnswer, Collected, Command, MetaState, ObjectId, Retention, Role, ServerId,
    StagedPut, TierId,
};
use kbf_objstore::{Floor, FsStore, KeyPrefix, ObjectKey, ObjectStore, Space};
use kbf_proto::reapi;
use kbf_types::Digest;

/// One metadata log type for every test, so one instantiation of `evict_round` covers
/// it all (the coverage ratchet counts the best single instantiation).
#[derive(Clone)]
struct Log(Arc<Shared>);

struct Shared {
    inner: MemoryMetaLog,
    /// When set, every `EvictObjects` checks that the file of each object it names is
    /// still there: the index changes before the files do.
    files: OnceLock<FsStore>,
    /// Answers `EvictObjects` with another command's outcome: a broken log.
    misanswers: AtomicBool,
}

impl Log {
    fn new() -> Self {
        Self(Arc::new(Shared {
            inner: MemoryMetaLog::new(Retention::default()),
            files: OnceLock::new(),
            misanswers: AtomicBool::new(false),
        }))
    }

    async fn run(&self, command: Command) -> Applied {
        self.0
            .inner
            .commit(command)
            .await
            .expect("a memory log commits")
    }

    async fn ask<R: Send>(&self, f: impl FnOnce(&MetaState) -> R + Send) -> R {
        self.0.inner.query(f).await.expect("a memory log answers")
    }
}

impl MetaLog for Log {
    async fn commit(&self, command: Command) -> Result<Applied, MetaLogError> {
        if let Command::EvictObjects { objects } = &command {
            if self.0.misanswers.load(Ordering::Relaxed) {
                return Ok(Applied::Collected(Collected::default()));
            }
            if let Some(files) = self.0.files.get() {
                for object in objects {
                    let held = files.may_hold(&object_key(*object)).await;
                    assert!(
                        held.expect("may_hold"),
                        "{object} deleted before its eviction committed"
                    );
                }
            }
        }
        self.0.inner.commit(command).await
    }

    async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        self.0.inner.query(f).await
    }
}

const PREFIX: &str = "kbf/log-a/";

/// The floor of every test's store, and so a target of twice it.
const FLOOR: u64 = 1 << 20;

/// A store, its cache and the free space its fake file system reports.
struct Fixture {
    dir: PathBuf,
    log: Log,
    files: FsStore,
    cache: Cache<Log, FsStore>,
    available: Arc<AtomicU64>,
}

impl Fixture {
    /// A cache recording recency over a fresh store whose file system has room.
    async fn new(name: &str) -> Self {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join("kbf-front-evict")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let available = Arc::new(AtomicU64::new(1 << 40));
        let fake = Arc::clone(&available);
        let space = Arc::new(move |_: &Path| -> std::io::Result<Space> {
            let available = fake.load(Ordering::Relaxed);
            Ok(Space {
                total: 1 << 40,
                available,
            })
        });
        let files = FsStore::open_with_space(&dir, Floor::Bytes(FLOOR), space).expect("open");
        let log = Log::new();
        let prefix = KeyPrefix::new(PREFIX).expect("a prefix");
        let cache = Cache::open(log.clone(), files.clone(), prefix)
            .await
            .expect("a cache");
        cache.track_recency();
        Self {
            dir,
            log,
            files,
            cache,
            available,
        }
    }

    /// Makes the file system `shortfall` bytes short of twice the floor.
    fn short_by(&self, shortfall: u64) {
        self.available
            .store(2 * FLOOR - shortfall, Ordering::Relaxed);
    }

    /// Stores the blob `text` (a segment of its own) and returns its object.
    async fn store(&self, text: &str) -> ObjectId {
        self.cache
            .store_blobs(vec![blob(text)])
            .await
            .expect("store");
        self.object_of(text).await
    }

    async fn object_of(&self, text: &str) -> ObjectId {
        match self.log.ask(|s| s.blob(&digest(text))).await {
            BlobAnswer::Present { location, .. } => location.object,
            other => panic!("{text} is not present: {other:?}"),
        }
    }

    /// One round with `grace`.
    async fn round(&self, grace: Duration) -> Result<EvictRound, CacheError> {
        self.cache.evict_round(&self.files, grace, ready).await
    }

    async fn present(&self, texts: &[&str]) -> Vec<bool> {
        let digests: Vec<Digest> = texts.iter().map(|t| digest(t)).collect();
        self.cache.present(&digests).await.expect("present")
    }

    async fn holds(&self, object: ObjectId) -> bool {
        self.files
            .may_hold(&object_key(object))
            .await
            .expect("may_hold")
    }

    /// The length of `object`, as the evictor counts it (the file's, less its header).
    fn len_of(&self, object: ObjectId) -> u64 {
        let key = object_key(object);
        let header = 8 + 2 + key.as_str().len() as u64;
        let name = sha256_hex(key.as_str());
        let file = self.dir.join("objects").join(&name[..2]).join(&name);
        std::fs::metadata(file).expect("an object file").len() - header
    }

    fn remove(self) {
        drop(self.cache);
        drop(self.files);
        std::fs::remove_dir_all(&self.dir).expect("remove");
    }
}

fn sha256_hex(text: &str) -> String {
    kbf_segments::sha256(text.as_bytes()).hash_hex()
}

fn object_key(object: ObjectId) -> ObjectKey {
    let (epoch, seq) = (
        object.epoch().expect("an epoch writer's").get(),
        object.seq(),
    );
    ObjectKey::new(format!("{PREFIX}cas/{epoch:016x}/{seq:016x}")).expect("a key")
}

fn blob(text: &str) -> VerifiedBlob {
    VerifiedBlob::hashed(Bytes::copy_from_slice(text.as_bytes()))
}

fn digest(text: &str) -> Digest {
    kbf_segments::sha256(text.as_bytes())
}

/// A log that needs no readying for `EvictObjects`.
#[expect(clippy::unnecessary_wraps, reason = "the readiness hook's signature")]
fn ready(_: &Log) -> Result<EvictReady, MetaLogError> {
    Ok(EvictReady::Ready)
}

/// A log of a three-voter cell below the version that takes `EvictObjects`.
#[expect(clippy::unnecessary_wraps, reason = "the readiness hook's signature")]
fn needs_v3(_: &Log) -> Result<EvictReady, MetaLogError> {
    Ok(EvictReady::NeedsV3 { voters: 3 })
}

/// An action result whose one output file is `text`'s blob.
fn result_of(text: &str) -> reapi::ActionResult {
    let d = digest(text);
    reapi::ActionResult {
        output_files: vec![reapi::OutputFile {
            path: "out".to_owned(),
            digest: Some(reapi::Digest {
                hash: d.hash_hex(),
                size_bytes: d.size_bytes as i64,
            }),
            ..reapi::OutputFile::default()
        }],
        ..reapi::ActionResult::default()
    }
}

/// Catches an evictor that ranks by when objects were written, not last used (mutant:
/// reads do not count, so the order is first in, first out): of three objects written
/// a, b, c and then a read, b is the least recently used, and a shortfall of one
/// object's length takes b alone.
#[tokio::test]
async fn evicts_least_recently_used_first() {
    let f = Fixture::new("lru").await;
    let a = f.store("blob-a").await;
    let b = f.store("blob-b").await;
    f.store("blob-c").await;
    f.cache.read_blob(&digest("blob-a")).await.expect("read");
    let len = f.len_of(b);
    assert_eq!(f.len_of(a), len);
    f.short_by(len);

    let round = f.round(Duration::ZERO).await.expect("a round");

    assert_eq!((round.objects, round.bytes, round.blobs), (1, len, 1));
    assert_eq!(
        f.present(&["blob-a", "blob-b", "blob-c"]).await,
        [true, false, true]
    );
    assert!(!f.holds(b).await);
    f.remove();
}

/// Catches an evictor that evicts under a log that does not take `EvictObjects`
/// (mutant: `NeedsV3` taken as ready), which would commit a command the other replicas
/// of its cell refuse, or deletes the files without it (mutant: files only), leaving
/// index entries with no file: round after round, nothing is evicted and the round
/// says why and how short it stays. Once the log takes the command, the next round
/// evicts.
#[tokio::test]
async fn evict_is_off_until_the_log_takes_evict() {
    let f = Fixture::new("needs-v3").await;
    let a = f.store("blob-a").await;
    let len = f.len_of(a);
    f.short_by(len);

    for _ in 0..2 {
        let off = f
            .cache
            .evict_round(&f.files, Duration::ZERO, needs_v3)
            .await;
        let off = off.expect("a round");
        assert_eq!((off.objects, off.short, off.needs_v3), (0, len, Some(3)));
        assert_eq!(f.present(&["blob-a"]).await, [true]);
        assert!(f.holds(a).await);
    }

    let round = f.round(Duration::ZERO).await.expect("a round");
    assert_eq!((round.objects, round.needs_v3), (1, None));
    assert!(!f.holds(a).await);
    f.remove();
}

/// Catches recency bumped only where the store is read (mutant: FindMissing's present
/// answers do not count): a client that asks whether a blob is present and is told yes
/// relies on it as much as one that reads it. Of a and b, written in that order, a
/// FindMissing that reports a present makes b the least recently used.
#[tokio::test]
async fn lru_counts_find_missing_reports() {
    let f = Fixture::new("find-missing").await;
    let a = f.store("blob-a").await;
    f.store("blob-b").await;
    let missing = f
        .cache
        .find_missing(&[digest("blob-a"), digest("none")])
        .await;
    assert_eq!(missing.expect("find_missing"), [digest("none")]);
    f.short_by(f.len_of(a));

    f.round(Duration::ZERO).await.expect("a round");

    assert_eq!(f.present(&["blob-a", "blob-b"]).await, [true, false]);
    f.remove();
}

/// Catches an evictor that takes an object used within the grace window (mutant: the
/// window removed), cutting short a client that was just told its blob is present:
/// with a 60 s window, two objects written just now stay whatever the shortfall, and
/// only an object not used since the start (seeded by the start-up pass) goes. What the
/// round could not cover is reported short.
#[tokio::test]
async fn evict_spares_grace_window() {
    let f = Fixture::new("grace").await;
    let a = f.store("blob-a").await;
    f.store("blob-b").await;
    let old = ObjectId::of(f.cache.writer(), 99);
    f.files
        .put_new(&object_key(old), Bytes::from(vec![7; 10]), None)
        .await
        .expect("plant");
    let held = HeldObject {
        len: 10,
        mtime: SystemTime::UNIX_EPOCH,
    };
    f.cache.seed_recency(&BTreeMap::from([(old, held)]));
    let shortfall = 3 * f.len_of(a);
    f.short_by(shortfall);

    let round = f.round(EVICT_GRACE).await.expect("a round");

    assert_eq!((round.objects, round.short), (1, shortfall - 10));
    assert!(!f.holds(old).await);
    assert_eq!(f.present(&["blob-a", "blob-b"]).await, [true, true]);
    f.remove();
}

/// Catches an evictor that frees more than the shortfall (mutants: everything not in
/// the window evicted; `>` for `>=` in the "covered" check, one object too many) or
/// works when nothing is short: with four objects of one length and a shortfall of
/// exactly two lengths, the two least recently used go; once twice the floor is free a
/// round evicts nothing and says what it saw.
#[tokio::test]
async fn eviction_stops_at_target() {
    let f = Fixture::new("target").await;
    let mut objects = Vec::new();
    for text in ["blob-a", "blob-b", "blob-c", "blob-d"] {
        objects.push(f.store(text).await);
    }
    let len = f.len_of(objects[0]);
    f.short_by(2 * len);

    let round = f.round(Duration::ZERO).await.expect("a round");

    assert_eq!((round.objects, round.bytes, round.short), (2, 2 * len, 0));
    assert_eq!(round.target, 2 * FLOOR);
    let present = f.present(&["blob-a", "blob-b", "blob-c", "blob-d"]).await;
    assert_eq!(present, [false, false, true, true]);
    f.available.store(2 * FLOOR, Ordering::Relaxed);
    let idle = f.round(Duration::ZERO).await.expect("a round");
    let want = EvictRound {
        available: 2 * FLOOR,
        target: 2 * FLOOR,
        ..EvictRound::default()
    };
    assert_eq!(idle, want);
    assert_eq!(f.present(&["blob-c", "blob-d"]).await, [true, true]);
    f.remove();
}

/// Catches files deleted before the index forgets them (mutant: the order swapped),
/// so a crash between the two leaves index entries, and action-cache hits, for files
/// that are gone: the log checks at each `EvictObjects` that the files are still
/// there, and after the round the action entry that needed the blob is gone with the
/// files. A log that answers the eviction with another outcome fails the round and no
/// file is deleted.
#[tokio::test]
async fn evict_commits_before_delete() {
    let f = Fixture::new("order").await;
    f.log.0.files.set(f.files.clone()).ok().expect("set once");
    let a = f.store("blob-a").await;
    let action = digest("action");
    f.cache
        .write_action_result(Role::Daemon, action, &result_of("blob-a"))
        .await
        .expect("write");
    assert!(
        f.cache
            .action_result(&action)
            .await
            .expect("a lookup")
            .is_some()
    );
    f.short_by(1);

    f.log.0.misanswers.store(true, Ordering::Relaxed);
    let refused = f.round(Duration::ZERO).await;
    assert!(
        matches!(refused, Err(CacheError::Internal(_))),
        "{refused:?}"
    );
    assert!(f.holds(a).await, "a file deleted though the index kept it");
    f.log.0.misanswers.store(false, Ordering::Relaxed);
    let round = f.round(Duration::ZERO).await.expect("a round");

    assert_eq!((round.objects, round.actions), (1, 1));
    assert!(!f.holds(a).await);
    assert!(f.log.ask(|s| s.action_count()).await == 0);
    f.remove();
}

/// Catches an evictor that deletes a staged object's file (mutant: the staged check
/// dropped): the index keeps a staged object, so its file must stay too.
#[tokio::test]
async fn evict_skips_staged_objects() {
    let f = Fixture::new("staged").await;
    let staged = ObjectId::of(f.cache.writer(), 50);
    f.files
        .put_new(&object_key(staged), Bytes::from(vec![7; 10]), None)
        .await
        .expect("plant");
    let held = HeldObject {
        len: 10,
        mtime: SystemTime::UNIX_EPOCH,
    };
    f.cache.seed_recency(&BTreeMap::from([(staged, held)]));
    let tier = TierId::new(7);
    let added = f
        .log
        .run(Command::AddTier {
            server: ServerId::new(1),
            tier,
        })
        .await;
    assert_eq!(added, Applied::Tier(Ok(())));
    let put = StagedPut {
        object: staged,
        end: 10,
        seal: true,
        blobs: vec![(digest("x"), 0)],
    };
    let applied = f
        .log
        .run(Command::PutStaged {
            tier,
            objects: vec![put],
        })
        .await;
    assert!(matches!(applied, Applied::Staged(Ok(_))), "{applied:?}");
    f.short_by(1);

    let round = f.round(Duration::ZERO).await.expect("a round");

    assert_eq!((round.objects, round.short), (0, 1));
    assert!(f.holds(staged).await);
    f.remove();
}

/// Catches an eviction that leaves the farm unable to heal (the blob reported held,
/// or the action still a hit, with its bytes gone): after the round, FindMissing
/// reports the evicted blob, the action that needed it is a miss, and a re-upload and
/// a new action-cache entry make it a hit again.
#[tokio::test]
async fn evicted_blob_missing_then_heals() {
    let f = Fixture::new("heals").await;
    f.store("blob-a").await;
    let action = digest("action");
    let result = result_of("blob-a");
    f.cache
        .write_action_result(Role::Daemon, action, &result)
        .await
        .expect("write");
    f.short_by(2 * FLOOR);

    let round = f.round(Duration::ZERO).await.expect("a round");

    assert_eq!(round.objects, 2, "the output's object and the result's");
    let missing = f
        .cache
        .find_missing(&[digest("blob-a")])
        .await
        .expect("find_missing");
    assert_eq!(missing, [digest("blob-a")]);
    assert!(
        f.cache
            .action_result(&action)
            .await
            .expect("a lookup")
            .is_none()
    );
    f.available.store(1 << 40, Ordering::Relaxed);
    f.store("blob-a").await;
    f.cache
        .write_action_result(Role::Daemon, action, &result)
        .await
        .expect("write");
    let hit = f.cache.action_result(&action).await.expect("a lookup");
    assert_eq!(hit.map(|(_, r)| r), Some(result));
    f.remove();
}
