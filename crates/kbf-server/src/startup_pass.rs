//! The start-up consistency pass over a `file://` cold store (`docs/deploy/storage-node.md`,
//! "The start-up pass"): `kbf-front`'s [`Cache::consistency_pass`] spawned once the
//! write tier is attached, never awaited before the server binds or reports ready;
//! then, in the same task, the evictor ([`Cache::evict_round`] every [`EVICT_EVERY`],
//! and at once when a write is refused for want of room), seeded with what the pass
//! kept.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use kbf_front::{Cache, EVICT_EVERY, EvictReady, MetaLog, MetaLogError, PASS_OPENS_PER_SEC};
use kbf_log::MachineVersion;
use kbf_objstore::{FsStore, ObjectStore};

use crate::RaftMetaLog;

/// What a failed pass logs.
const PASS_FAILED: &str = "start-up pass over the cold store directory failed; the next start runs it again, and \
     nothing is evicted until then";

/// What a failed round of eviction logs.
const ROUND_FAILED: &str = "eviction round failed; the next one tries again";

/// A pass to run over the file store `files` once the server is assembled, and the
/// evictor after it.
pub(crate) struct StartPass<M> {
    files: FsStore,
    /// Readies the log for `EvictObjects`, if it may, called only when there is
    /// something to forget.
    ready_to_evict: fn(&M) -> Result<EvictReady, MetaLogError>,
    /// How long after its last use an object is never evicted.
    grace: Duration,
    /// What the pass waits for before it starts: nothing in the server; a test holds
    /// it to see the server serve first.
    hold: Pin<Box<dyn Future<Output = ()> + Send>>,
}

/// Readies a Raft log for `EvictObjects` ([`ready_among`] its own voters).
fn ready_to_evict(log: &RaftMetaLog) -> Result<EvictReady, MetaLogError> {
    ready_among(log, log.voters())
}

/// Whether a Raft log in a cell of `voters` voters takes `EvictObjects`: one at
/// machine version 3 (whose format carries the command) does. One below it is raised
/// to it here only if `voters` is one (a single-host farm): a raise in a cell of more
/// would make every replica that does not run version 3 yet refuse the log, so there
/// the operator raises it once every replica does, and until then the answer is
/// [`EvictReady::NeedsV3`]. Called only when something is to be forgotten, so a log
/// with nothing to forget stays one an older binary reads.
fn ready_among(log: &RaftMetaLog, voters: usize) -> Result<EvictReady, MetaLogError> {
    if log.machine_version() >= MachineVersion::V3 {
        return Ok(EvictReady::Ready);
    }
    if voters > 1 {
        return Ok(EvictReady::NeedsV3 { voters });
    }
    log.raise_machine_version(MachineVersion::V3)?;
    Ok(EvictReady::Ready)
}

impl StartPass<RaftMetaLog> {
    /// The pass and evictor over `files` under a Raft log ([`ready_to_evict`]),
    /// sparing objects used within `grace`.
    pub(crate) fn over_raft(files: FsStore, grace: Duration) -> Self {
        Self {
            files,
            ready_to_evict,
            grace,
            hold: Box::pin(std::future::ready(())),
        }
    }
}

impl<M> StartPass<M> {
    /// The same pass, started only once `hold` completes: the rebuild of the index, in
    /// the server; in a test, a release the test sends.
    pub(crate) fn held(mut self, hold: impl Future<Output = ()> + Send + 'static) -> Self {
        self.hold = Box::pin(hold);
        self
    }
}

/// Turns on `cache`'s recency, then runs `pass` over it in a task of its own, logs
/// what it did, and evicts until the task is aborted. A pass that fails is logged and
/// leaves eviction off; the next start runs it again.
pub(crate) fn spawn<M: MetaLog, O: ObjectStore + 'static>(
    cache: Arc<Cache<M, O>>,
    pass: StartPass<M>,
) -> tokio::task::JoinHandle<()> {
    cache.track_recency();
    tokio::spawn(async move {
        let StartPass {
            files,
            ready_to_evict,
            grace,
            hold,
        } = pass;
        hold.await;
        match cache
            .consistency_pass(&files, PASS_OPENS_PER_SEC, ready_to_evict)
            .await
        {
            Ok(report) => {
                tracing::info!(
                    files = report.files,
                    kept = report.kept,
                    orphans_deleted = report.orphans_deleted,
                    foreign = report.foreign,
                    unreadable = report.unreadable,
                    evicted_objects = report.evicted_objects,
                    evicted_blobs = report.evicted_blobs,
                    evicted_actions = report.evicted_actions,
                    marked_missing = report.marked_missing,
                    withheld = report.withheld,
                    held_bytes = report.held_bytes(),
                    duration_ms = report.duration.as_millis() as u64,
                    "start-up pass over the cold store directory done"
                );
                cache.seed_recency(&report.held);
                loop {
                    if let Err(error) = cache.evict_round(&files, grace, ready_to_evict).await {
                        tracing::error!(%error, "{ROUND_FAILED}");
                    }
                    let _woken = tokio::time::timeout(EVICT_EVERY, files.refused_full()).await;
                }
            }
            Err(error) => tracing::error!(%error, "{PASS_FAILED}"),
        }
    })
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Mutex, PoisonError};
    use std::time::Duration;

    use bytes::Bytes;
    use clap::Parser;
    use kbf_front::{EVICT_GRACE, NEEDS_V3};
    use kbf_meta::{Applied, Command, Epoch, Location, ObjectId, Retention, StoreId};
    use kbf_objstore::{Floor, KeyPrefix, ObjectKey, Space};
    use kbf_proto::reapi::content_addressable_storage_client::ContentAddressableStorageClient;
    use kbf_proto::reapi::{self, FindMissingBlobsRequest};
    use kbf_types::Digest;
    use tonic::transport::Channel;

    use super::*;
    use crate::Args;
    use crate::start::assemble;

    /// How long the test waits for the server to bind, or for the pass to finish.
    const BOUND: Duration = Duration::from_secs(10);

    fn object_key(prefix: &KeyPrefix, seq: u64) -> ObjectKey {
        prefix
            .key(&format!("cas/{:016x}/{seq:016x}", 1))
            .expect("a key")
    }

    async fn missing(channel: &Channel, digests: &[Digest]) -> Vec<String> {
        let request = FindMissingBlobsRequest {
            blob_digests: digests
                .iter()
                .map(|d| reapi::Digest {
                    hash: d.hash_hex(),
                    size_bytes: d.size_bytes as i64,
                })
                .collect(),
            ..FindMissingBlobsRequest::default()
        };
        let answer = ContentAddressableStorageClient::new(channel.clone())
            .find_missing_blobs(request)
            .await
            .expect("FindMissingBlobs");
        let missing = answer.into_inner().missing_blob_digests.into_iter();
        missing.map(|d| d.hash).collect()
    }

    /// Catches a server that waits for the start-up pass before it binds (mutant: the
    /// pass awaited in the assembly), so a large directory holds up every start: with
    /// the pass held, the server must bind and answer FindMissing, which still reports
    /// a blob whose object file is gone. Released, the pass must delete the old
    /// epoch's orphan and forget the lost blob over the Raft log, which proves the log
    /// is raised to the format that carries `EvictObjects` (mutant: no raise), and log
    /// what it did.
    #[tokio::test(flavor = "current_thread")]
    async fn serves_before_pass_finishes() {
        let logs = Capture::waiting_for(PASS_LINES);
        let _default = logs.subscribe();
        let root = std::env::temp_dir().join(format!("kbf-server-pass-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (blobs, raft) = (root.join("blobs"), root.join("raft"));
        let args = Args::parse_from([
            "kbf-server".to_owned(),
            format!("--cold-store=file://{}", blobs.display()),
            format!("--meta-dir={}", raft.display()),
            "--listen=127.0.0.1:0".to_owned(),
            "--worker-listen=127.0.0.1:0".to_owned(),
        ]);
        let files = FsStore::open(&blobs, Floor::Bytes(1)).expect("open the store");
        let log = RaftMetaLog::open_dir(&raft, Retention::default(), "kbf/")
            .await
            .expect("open the log");
        let prefix = log.store_prefix();
        // An earlier process's epoch: an orphan, a lost object and a kept one.
        assert_eq!(
            log.commit(Command::AllocEpoch).await,
            Ok(Applied::Epoch(Epoch::new(1)))
        );
        let (lost, kept) = (kbf_segments::sha256(b"lost"), kbf_segments::sha256(b"kept"));
        let at = |seq| Location {
            store: StoreId::CONFIGURED,
            object: ObjectId::new(Epoch::new(1), seq),
            offset: 0,
        };
        let put = log
            .commit(Command::PutBlobs(vec![(lost, at(1)), (kept, at(2))]))
            .await;
        assert!(matches!(put, Ok(Applied::Blobs(Ok(_)))), "{put:?}");
        let orphan = object_key(&prefix, 0xff);
        for key in [&orphan, &object_key(&prefix, 2)] {
            let body = Bytes::from_static(b"some bytes");
            files.put_new(key, body, None).await.expect("plant");
        }
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        let pass = StartPass::over_raft(files.clone(), EVICT_GRACE).held(async {
            let _ = held.await;
        });

        let id = log.log_id();
        let assembled = assemble(
            &args,
            None,
            log,
            files.clone(),
            prefix,
            id,
            || Ok(std::future::pending::<()>()),
            Some(pass),
        );
        let server = tokio::time::timeout(BOUND, assembled)
            .await
            .expect("bound while the pass is held")
            .expect("assembled");
        let channel = Channel::from_shared(format!("http://{}", server.reapi))
            .expect("a URI")
            .connect_lazy();
        // Serves until aborted; its outcome is not looked at.
        let serving = tokio::spawn(async move { drop(server.serve().await) });
        assert!(missing(&channel, &[lost, kept]).await.is_empty());
        assert!(files.may_hold(&orphan).await.expect("may_hold"));

        release.send(()).expect("the pass is waiting");
        logs.ended().await;
        assert_eq!(missing(&channel, &[lost, kept]).await, [lost.hash_hex()]);
        assert!(!files.may_hold(&orphan).await.expect("may_hold"));
        let text = logs.text();
        for field in [
            "orphans_deleted=1",
            "evicted_objects=1",
            "evicted_blobs=1",
            "kept=1",
        ] {
            assert!(text.contains(field), "{field}: {text}");
        }
        serving.abort();
        drop(files);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Catches a pass that raises the log's machine version in a cell of more than
    /// one voter (mutant: always raise), which would make every replica that does not
    /// run version 3 yet refuse the log; one that forgets there anyway, or marks
    /// nothing missing; and one that logs the refusal per object (mutant: in the
    /// loop) rather than once. Three voters: the log stays at version 1, the two lost
    /// objects' blobs read as missing and one line says so. Once the operator raises
    /// the log, the next pass forgets them.
    #[tokio::test(flavor = "current_thread")]
    async fn a_cell_of_three_marks_missing_until_the_operator_raises() {
        let logs = Capture::waiting_for(PASS_LINES);
        let _default = logs.subscribe();
        let root =
            std::env::temp_dir().join(format!("kbf-server-pass-three-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let files = FsStore::open(&root.join("blobs"), Floor::Bytes(1)).expect("open the store");
        let meta = RaftMetaLog::open_dir(&root.join("raft"), Retention::default(), "kbf/")
            .await
            .expect("open the log");
        assert_eq!(
            meta.commit(Command::AllocEpoch).await,
            Ok(Applied::Epoch(Epoch::new(1)))
        );
        let prefix = meta.store_prefix();
        let at = |seq| Location {
            store: StoreId::CONFIGURED,
            object: ObjectId::new(Epoch::new(1), seq),
            offset: 0,
        };
        let [lost, gone, kept] = [b"lost", b"gone", b"kept"].map(|t| kbf_segments::sha256(t));
        let put = meta.commit(Command::PutBlobs(vec![
            (lost, at(1)),
            (gone, at(3)),
            (kept, at(2)),
        ]));
        assert!(matches!(put.await, Ok(Applied::Blobs(Ok(_)))));
        let body = Bytes::from_static(b"some bytes");
        let planted = files.put_new(&object_key(&prefix, 2), body, None).await;
        planted.expect("plant");
        let cache = Arc::new(
            Cache::open(meta, files.clone(), prefix)
                .await
                .expect("a cache"),
        );
        let three = || StartPass {
            files: files.clone(),
            ready_to_evict: |log| ready_among(log, 3),
            grace: EVICT_GRACE,
            hold: Box::pin(std::future::ready(())),
        };

        // The task goes on as the evictor, which has nothing to do here.
        let task = spawn(Arc::clone(&cache), three());
        logs.ended().await;
        task.abort();

        let text = logs.text();
        assert_eq!(text.matches(NEEDS_V3).count(), 1, "{text}");
        assert!(
            text.contains(&format!("{NEEDS_V3}: 2 objects held")),
            "{text}"
        );
        assert!(text.contains("marked_missing=2"), "{text}");
        assert!(text.contains("evicted_objects=0"), "{text}");
        assert_eq!(cache.meta().machine_version(), MachineVersion::V1);
        let present = cache.present(&[lost, gone, kept]).await.expect("present");
        assert_eq!(present, [false, false, true]);
        let blobs = cache.meta().query(|s| s.blob_count()).await;
        assert_eq!(blobs, Ok(3), "forgotten below version 3");

        cache
            .meta()
            .raise_machine_version(MachineVersion::V3)
            .expect("the operator's raise");
        logs.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        let task = spawn(Arc::clone(&cache), three());
        logs.ended().await;
        task.abort();

        let text = logs.text();
        assert!(!text.contains(NEEDS_V3), "{text}");
        assert!(text.contains("evicted_objects=2"), "{text}");
        let blobs = cache.meta().query(|s| s.blob_count()).await;
        assert_eq!(blobs, Ok(1));
        drop((cache, files));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Catches a readiness that counts the log at version 3 as not ready in a cell of
    /// many (mutant: the version check dropped), or the boundary moved (mutants: `>= 1`
    /// or `> 2` for `> 1`): two voters need the operator, one is raised here.
    #[tokio::test(flavor = "current_thread")]
    async fn only_a_single_voter_is_raised_here() {
        let root =
            std::env::temp_dir().join(format!("kbf-server-pass-voters-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let log = RaftMetaLog::open_dir(&root, Retention::default(), "kbf/")
            .await
            .expect("open the log");
        let two = ready_among(&log, 2);
        assert_eq!(two, Ok(EvictReady::NeedsV3 { voters: 2 }));
        assert_eq!(log.machine_version(), MachineVersion::V1);
        assert_eq!(ready_to_evict(&log), Ok(EvictReady::Ready));
        assert_eq!(log.machine_version(), MachineVersion::V3);
        assert_eq!(ready_among(&log, 2), Ok(EvictReady::Ready));
        drop(log);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// What every line the pass ends with says.
    const PASS_LINES: &[u8] = b"pass over the cold store directory";

    /// Log lines written by the test's subscriber, and a wake-up when a line holding
    /// the text waited for is written.
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>, Arc<tokio::sync::Notify>, &'static [u8]);

    impl Capture {
        fn waiting_for(text: &'static [u8]) -> Self {
            Self(Arc::default(), Arc::default(), text)
        }

        /// Sends this thread's log lines here until the guard drops.
        fn subscribe(&self) -> tracing::subscriber::DefaultGuard {
            let writer = self.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(move || writer.clone())
                .with_ansi(false)
                .finish();
            tracing::subscriber::set_default(subscriber)
        }

        /// Waits, at most `BOUND`, for a line holding the text.
        async fn ended(&self) {
            let ended = tokio::time::timeout(BOUND, self.1.notified()).await;
            ended.expect("no line holding the text waited for");
        }

        fn text(&self) -> String {
            let bytes = self
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            String::from_utf8(bytes).expect("UTF-8 logs")
        }
    }

    impl io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            held.extend_from_slice(bytes);
            if bytes.windows(self.2.len()).any(|w| w == self.2) {
                self.1.notify_one();
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Catches a pass whose failure is dropped without a word (mutant: the error arm
    /// logs nothing) or brings its task down: a log that cannot be readied for the
    /// eviction fails the pass, which must log that it failed, with the cause, and end.
    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_pass_is_logged() {
        let mut logs = Capture::waiting_for(PASS_LINES);
        let _default = logs.subscribe();
        let root =
            std::env::temp_dir().join(format!("kbf-server-pass-fails-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let files = FsStore::open(&root.join("blobs"), Floor::Bytes(1)).expect("open the store");
        // A Raft log, as in the server: one instantiation of the task covers both arms.
        let meta = RaftMetaLog::open_dir(&root.join("raft"), Retention::default(), "kbf/")
            .await
            .expect("open the log");
        assert_eq!(
            meta.commit(Command::AllocEpoch).await,
            Ok(Applied::Epoch(Epoch::new(1)))
        );
        let prefix = meta.store_prefix();
        let at = |seq| Location {
            store: StoreId::CONFIGURED,
            object: ObjectId::new(Epoch::new(1), seq),
            offset: 0,
        };
        let (lost, kept) = (kbf_segments::sha256(b"lost"), kbf_segments::sha256(b"kept"));
        let put = meta.commit(Command::PutBlobs(vec![(lost, at(1)), (kept, at(2))]));
        assert!(matches!(put.await, Ok(Applied::Blobs(Ok(_)))));
        let body = Bytes::from_static(b"some bytes");
        let planted = files.put_new(&object_key(&prefix, 2), body, None).await;
        planted.expect("plant");
        let cache = Cache::open(meta, files.clone(), prefix)
            .await
            .expect("a cache");
        let pass = StartPass {
            files,
            ready_to_evict: |_| Err(MetaLogError::Unavailable("not raised".to_owned())),
            grace: EVICT_GRACE,
            hold: Box::pin(std::future::ready(())),
        };

        spawn(Arc::new(cache), pass)
            .await
            .expect("the pass task ends without a panic");

        io::Write::flush(&mut logs).expect("flush");
        let text = logs.text();
        assert!(text.contains(PASS_FAILED), "{text}");
        assert!(text.contains("not raised"), "{text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A cache over a Raft log whose earlier epoch indexed the blob `kept` in object
    /// 2, whose file is in a store that sees a file system with nothing free (its
    /// floor 1000 bytes): what the pass keeps, and the evictor must then evict.
    async fn short_of_room(
        name: &str,
    ) -> (std::path::PathBuf, FsStore, Cache<RaftMetaLog, FsStore>) {
        let root = std::env::temp_dir().join(format!("kbf-server-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // Room to plant the object file, then none.
        let room = Arc::new(AtomicBool::new(true));
        let fake = Arc::clone(&room);
        let none_free = Arc::new(move |_: &std::path::Path| -> io::Result<Space> {
            let available = if fake.load(Ordering::Relaxed) {
                1 << 20
            } else {
                0
            };
            Ok(Space {
                total: 1 << 30,
                available,
            })
        });
        let files = FsStore::open_with_space(&root.join("blobs"), Floor::Bytes(1000), none_free);
        let files = files.expect("open the store");
        let meta = RaftMetaLog::open_dir(&root.join("raft"), Retention::default(), "kbf/").await;
        let meta = meta.expect("open the log");
        assert_eq!(
            meta.commit(Command::AllocEpoch).await,
            Ok(Applied::Epoch(Epoch::new(1)))
        );
        let prefix = meta.store_prefix();
        let at = Location {
            store: StoreId::CONFIGURED,
            object: ObjectId::new(Epoch::new(1), 2),
            offset: 0,
        };
        let put = meta.commit(Command::PutBlobs(vec![(kbf_segments::sha256(b"kept"), at)]));
        assert!(matches!(put.await, Ok(Applied::Blobs(Ok(_)))));
        let body = Bytes::from_static(b"some bytes");
        files
            .put_new(&object_key(&prefix, 2), body, None)
            .await
            .expect("plant");
        room.store(false, Ordering::Relaxed);
        let cache = Cache::open(meta, files.clone(), prefix)
            .await
            .expect("a cache");
        (root, files, cache)
    }

    /// Catches a server whose evictor is never armed after the pass (mutant: the loop
    /// removed), is not seeded with what the pass kept (mutant: no seed; the object is
    /// then of unknown length and never evicted), or evicts without raising the Raft
    /// log to the format that carries `EvictObjects` (the commit is refused): with no
    /// room on the file system, the object the pass kept is forgotten, its file
    /// deleted, and the round logged.
    #[tokio::test(flavor = "current_thread")]
    async fn the_evictor_runs_after_the_pass() {
        let logs = Capture::waiting_for(b"evicted the least recently used objects");
        let _default = logs.subscribe();
        let (root, files, cache) = short_of_room("evictor").await;
        let key = cache
            .object_key(ObjectId::new(Epoch::new(1), 2))
            .expect("a key");
        let cache = Arc::new(cache);

        let task = spawn(
            Arc::clone(&cache),
            StartPass::over_raft(files.clone(), EVICT_GRACE),
        );
        logs.ended().await;
        task.abort();

        let kept = kbf_segments::sha256(b"kept");
        assert_eq!(cache.present(&[kept]).await.expect("present"), [false]);
        assert!(!files.may_hold(&key).await.expect("may_hold"));
        let text = logs.text();
        assert!(
            text.contains("objects=1 bytes=10 blobs=1 actions=0"),
            "{text}"
        );
        drop(files);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// How many times [`three_voters_counted`] was asked.
    static ASKED: AtomicUsize = AtomicUsize::new(0);

    /// [`ready_among`] three voters, counting the rounds that ask.
    fn three_voters_counted(log: &RaftMetaLog) -> Result<EvictReady, MetaLogError> {
        ASKED.fetch_add(1, Ordering::Relaxed);
        ready_among(log, 3)
    }

    /// Catches an evictor that raises the log's machine version in a cell of more
    /// than one voter (mutant: always raise), evicts there anyway (mutant: `NeedsV3`
    /// taken as ready), or says it is off every round (mutant: no once-per-start
    /// check): with no room and three voters, rounds woken by refused writes evict
    /// nothing, the log stays at version 1, and one line says eviction is off. Once
    /// the operator raises the log, the next round evicts.
    #[tokio::test(flavor = "current_thread")]
    async fn the_evictor_of_a_cell_of_three_waits_for_the_operator() {
        let logs = Capture::waiting_for(b"evicted the least recently used objects");
        let _default = logs.subscribe();
        let (root, files, cache) = short_of_room("evict-three").await;
        let key = cache
            .object_key(ObjectId::new(Epoch::new(1), 2))
            .expect("a key");
        let refused = cache
            .object_key(ObjectId::new(Epoch::new(2), 9))
            .expect("a key");
        let cache = Arc::new(cache);
        let pass = StartPass {
            files: files.clone(),
            ready_to_evict: three_voters_counted,
            grace: EVICT_GRACE,
            hold: Box::pin(std::future::ready(())),
        };
        let refuse = || async {
            let body = Bytes::from_static(b"no room");
            let put = files.put_new(&refused, body, None).await;
            assert!(put.is_err(), "{put:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        let task = spawn(Arc::clone(&cache), pass);
        let rounds = async {
            while ASKED.load(Ordering::Relaxed) < 3 {
                refuse().await;
            }
        };
        tokio::time::timeout(BOUND, rounds)
            .await
            .expect("three rounds");

        let text = logs.text();
        assert_eq!(text.matches(NEEDS_V3).count(), 1, "{text}");
        assert!(
            text.contains(&format!("{NEEDS_V3}: eviction off")),
            "{text}"
        );
        assert_eq!(cache.meta().machine_version(), MachineVersion::V1);
        assert!(files.may_hold(&key).await.expect("may_hold"));
        let kept = kbf_segments::sha256(b"kept");
        assert_eq!(cache.present(&[kept]).await.expect("present"), [true]);

        cache
            .meta()
            .raise_machine_version(MachineVersion::V3)
            .expect("the operator's raise");
        refuse().await;
        logs.ended().await;
        task.abort();

        assert!(!files.may_hold(&key).await.expect("may_hold"));
        assert_eq!(cache.present(&[kept]).await.expect("present"), [false]);
        assert_eq!(logs.text().matches(NEEDS_V3).count(), 1);
        drop(files);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Catches a start-up pass that runs before the rebuild of the index is done (mutant:
    /// the pass not held for the rebuild): it would walk the directory against an index
    /// that does not name the old server's objects yet, keep none of them, and seed the
    /// evictor with none, so the objects the rebuild then adopts would never be evicted.
    /// A node over a new log, registered as member 7, rebuilds over three segments of an
    /// old server in the directory: the rebuild is logged done before the pass is, and the
    /// pass keeps all three (`kept=3`, no foreign file).
    #[tokio::test(flavor = "current_thread")]
    async fn the_start_pass_waits_for_the_rebuild() {
        use kbf_front::{MemoryMetaLog, ObjectKeys, RebuildConfig, VerifiedBlob};
        use kbf_meta::NodeWriter;
        use kbf_storage::service::CatchUp;

        let logs = Capture::waiting_for(PASS_LINES);
        let _default = logs.subscribe();
        let root =
            std::env::temp_dir().join(format!("kbf-server-pass-rebuild-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let files = FsStore::open(&root.join("blobs"), Floor::Bytes(1)).expect("open the store");
        let old_keys = ObjectKeys {
            legacy: KeyPrefix::new(format!("kbf/{:016x}/", 0x11)).expect("prefix"),
            root: KeyPrefix::new("kbf/").expect("root"),
        };
        let old = Cache::open_under(
            MemoryMetaLog::new(Retention::default()),
            files.clone(),
            old_keys,
        )
        .await
        .expect("an old server's cache");
        for i in 0..3 {
            let blob = VerifiedBlob::hashed(Bytes::from(format!("old blob {i}")));
            old.store_blobs(vec![blob]).await.expect("store");
        }

        let meta = RaftMetaLog::open_dir(&root.join("raft"), Retention::default(), "kbf/")
            .await
            .expect("open the log");
        let keys = ObjectKeys {
            legacy: meta.store_prefix(),
            root: KeyPrefix::new("kbf/").expect("root"),
        };
        let one = std::num::NonZeroU64::MIN;
        let writer = NodeWriter::new(one, one);
        meta.raise_machine_version(MachineVersion::V4)
            .expect("a node's log takes format 4");
        let cache = Cache::open_node(meta, files.clone(), keys, writer)
            .await
            .expect("a node's cache");
        let cache = Arc::new(cache);
        let catch_up = CatchUp::rebuilding();
        let pass = StartPass::over_raft(files.clone(), EVICT_GRACE);
        let _background =
            crate::rebuild::spawn(&cache, &catch_up, Some(pass), RebuildConfig::default());
        logs.ended().await;

        assert!(!catch_up.is_catching_up());
        let text = logs.text();
        let rebuilt = text
            .find("rebuild of the index from the store done")
            .expect("the rebuild is logged");
        let passed = text
            .find("start-up pass over the cold store directory done")
            .expect("the pass is logged");
        assert!(rebuilt < passed, "{text}");
        for field in [
            "kept=3",
            "foreign=0",
            "orphans_deleted=0",
            "evicted_objects=0",
        ] {
            assert!(text.contains(field), "{field}: {text}");
        }
        drop((cache, old, files));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Catches a failed round of eviction dropped without a word or ending the
    /// evictor (mutant: the error arm logs nothing): a log that cannot be readied for
    /// `EvictObjects` fails the round, which is logged with the cause; nothing is
    /// forgotten and the file stays.
    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_eviction_round_is_logged() {
        let logs = Capture::waiting_for(ROUND_FAILED.as_bytes());
        let _default = logs.subscribe();
        let (root, files, cache) = short_of_room("evict-fails").await;
        let key = cache
            .object_key(ObjectId::new(Epoch::new(1), 2))
            .expect("a key");
        let pass = StartPass {
            files: files.clone(),
            ready_to_evict: |_| Err(MetaLogError::Unavailable("not raised".to_owned())),
            grace: EVICT_GRACE,
            hold: Box::pin(std::future::ready(())),
        };

        let task = spawn(Arc::new(cache), pass);
        logs.ended().await;
        task.abort();

        assert!(logs.text().contains("not raised"), "{}", logs.text());
        assert!(files.may_hold(&key).await.expect("may_hold"));
        drop(files);
        let _ = std::fs::remove_dir_all(&root);
    }
}
