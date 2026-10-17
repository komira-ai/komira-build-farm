//! A registered node's rebuild of its index, end to end through [`super::assemble`]: the
//! node registers with a frontend (a fake that records what each registration said),
//! serves while it rebuilds, and holds every blob afterwards. Each test's comment names
//! the defect it is there to catch.

use std::future::pending;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use kbf_front::{Cache, MemoryMetaLog, ObjectKeys, RebuildConfig, VerifiedBlob};
use kbf_meta::Retention;
use kbf_objstore::footers::FooterReads;
use kbf_objstore::{
    ByteRange, Capabilities, Floor, FsStore, KeyPrefix, ListPage, ListToken, MemoryStore,
    ObjectKey, ObjectStore, ObjectStoreError, PageSize,
};
use kbf_proto::kbf::map::v1 as pb;
use kbf_storage::service::{CatchUp, RemoteStorage};
use kbf_storage::{Storage as _, StorageError};
use kbf_types::Digest;
use tokio::sync::{Notify, watch};
use tonic::transport::Endpoint;

use super::{assemble, command, parse};
use crate::RaftMetaLog;
use crate::replication::REPAIR_DIR;

/// The fake frontend: every registration is member 1 at the next boot, answered with a
/// map the node can read; each call's "still rebuilding" report is recorded.
struct Front {
    boots: AtomicU64,
    registered: Mutex<Vec<bool>>,
    map: pb::Map,
}

struct Fake(Arc<Front>);

#[tonic::async_trait]
impl pb::membership_server::Membership for Fake {
    async fn register(
        &self,
        request: tonic::Request<pb::RegisterRequest>,
    ) -> Result<tonic::Response<pb::RegisterResponse>, tonic::Status> {
        let front = &self.0;
        front
            .registered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.into_inner().catching_up);
        Ok(tonic::Response::new(pb::RegisterResponse {
            member_id: 1,
            boot: front.boots.fetch_add(1, Ordering::SeqCst) + 1,
            heartbeat_interval_ms: 20,
            map: Some(front.map.clone()),
        }))
    }

    async fn heartbeat(
        &self,
        _: tonic::Request<pb::HeartbeatRequest>,
    ) -> Result<tonic::Response<pb::HeartbeatResponse>, tonic::Status> {
        Ok(tonic::Response::new(pb::HeartbeatResponse {
            map_generation: self.0.map.generation,
            map: None,
        }))
    }
}

/// A frontend listening on a port of its own.
struct Frontend {
    addr: SocketAddr,
    state: Arc<Front>,
}

impl Frontend {
    fn start() -> Self {
        let member = kbf_map::Member {
            id: kbf_map::MemberId(1),
            addr: "192.0.2.5:1".to_owned(),
            zone: String::new(),
            weight: 1,
            state: kbf_map::MemberState::Active,
        };
        let map = kbf_map::Map::new(4, vec![member], kbf_map::Rule::Rendezvous, 1, 1, Vec::new())
            .expect("a map");
        let state = Arc::new(Front {
            boots: AtomicU64::new(0),
            registered: Mutex::new(Vec::new()),
            map: pb::Map::from(&map),
        });
        let incoming =
            tonic::transport::server::TcpIncoming::bind("127.0.0.1:0".parse().expect("an address"))
                .expect("bind");
        let addr = incoming.local_addr().expect("address");
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(pb::membership_server::MembershipServer::new(Fake(
                    Arc::clone(&state),
                )))
                .serve_with_incoming(incoming),
        );
        Self { addr, state }
    }

    /// What each registration said about whether the node was rebuilding, in order.
    fn registrations(&self) -> Vec<bool> {
        self.state
            .registered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// A bucket that outlives each start over it, which a test can make hold its listing.
#[derive(Clone)]
struct Bucket {
    memory: Arc<MemoryStore>,
    script: Arc<Script>,
}

/// What a start's view of the bucket does when listed.
struct Script {
    lists: AtomicUsize,
    /// The listing call (counting from 1) from which it waits for the release; 0: never.
    hold_from: usize,
    release: watch::Sender<bool>,
    /// While set, every listing call fails.
    failing: AtomicBool,
    /// Notified when a listing call begins to wait.
    entered: Notify,
    /// The token of the first listing call.
    first: Mutex<Option<Option<String>>>,
}

impl Script {
    fn new(hold_from: usize) -> Arc<Self> {
        Arc::new(Self {
            lists: AtomicUsize::new(0),
            hold_from,
            release: watch::channel(false).0,
            failing: AtomicBool::new(false),
            entered: Notify::new(),
            first: Mutex::new(None),
        })
    }
}

impl Bucket {
    fn new() -> Self {
        let capabilities = Capabilities {
            conditional_put: true,
            object_lock: false,
        };
        Self {
            memory: Arc::new(MemoryStore::new(capabilities)),
            script: Script::new(0),
        }
    }

    /// The same objects, listed by a new view that waits from listing call `hold_from`.
    fn scripted(&self, hold_from: usize) -> Self {
        Self {
            memory: Arc::clone(&self.memory),
            script: Script::new(hold_from),
        }
    }

    /// Makes every listing call of this view fail, or stop failing.
    fn fail_lists(&self, failing: bool) {
        self.script.failing.store(failing, Ordering::SeqCst);
    }

    fn release(&self) {
        self.script.release.send_replace(true);
    }

    fn lists(&self) -> usize {
        self.script.lists.load(Ordering::SeqCst)
    }

    /// The text of the token of this view's first listing call.
    fn first_token(&self) -> Option<Option<String>> {
        self.script
            .first
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    async fn keys(&self) -> Vec<ObjectKey> {
        let size = PageSize::new(1000).expect("size");
        let page = self.memory.list(&KeyPrefix::default(), None, size).await;
        page.expect("list")
            .objects
            .into_iter()
            .map(|o| o.key)
            .collect()
    }
}

impl ObjectStore for Bucket {
    fn capabilities(&self) -> Capabilities {
        self.memory.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.memory.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.memory.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.memory.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        let script = &self.script;
        let call = script.lists.fetch_add(1, Ordering::SeqCst) + 1;
        if call == 1 {
            *script.first.lock().unwrap_or_else(PoisonError::into_inner) =
                Some(after.map(|token| format!("{token:?}")));
        }
        if script.failing.load(Ordering::SeqCst) {
            return Err(ObjectStoreError::Protocol(
                "a scripted listing failure".into(),
            ));
        }
        if script.hold_from != 0 && call >= script.hold_from {
            script.entered.notify_one();
            let mut released = script.release.subscribe();
            let _ = released.wait_for(|released| *released).await;
        }
        self.memory.list(prefix, after, max_keys).await
    }
}

/// A node under test: serving in a task, with a client of its storage service.
struct Node {
    catch_up: CatchUp,
    client: RemoteStorage,
    serving: tokio::task::JoinHandle<()>,
}

impl Node {
    /// Stops the node and waits until its task is gone, as a kill.
    async fn kill(self) {
        self.serving.abort();
        let _ = self.serving.await;
    }

    /// Every blob reads back byte for byte, and none is reported missing.
    async fn holds(&self, blobs: &[(Digest, Bytes)]) {
        for (digest, bytes) in blobs {
            assert_eq!(&self.client.read_blob(digest).await.expect("read"), bytes);
        }
        let digests: Vec<Digest> = blobs.iter().map(|(d, _)| *d).collect();
        let missing = self.client.find_missing(&digests).await.expect("find");
        assert_eq!(missing, Vec::<Digest>::new());
    }
}

/// `wait`, which must complete: a hang guard, so a node that never does what the test
/// waits for fails the test instead of hanging it. Asserts nothing about speed.
async fn within<T>(what: &str, wait: impl std::future::Future<Output = T>) -> T {
    let guard = Duration::from_secs(60);
    match tokio::time::timeout(guard, wait).await {
        Ok(done) => done,
        Err(_) => panic!("timed out waiting for {what}"),
    }
}

/// A scratch directory of its own for `name`, empty.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kbf-rebuild-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Pages of two objects and batches of two, so a rebuild of a few objects is several
/// pages and several progress commits.
fn small() -> RebuildConfig {
    RebuildConfig {
        reads: FooterReads {
            concurrency: std::num::NonZeroUsize::new(4).expect("not zero"),
            page: PageSize::new(2).expect("in range"),
            tail: FooterReads::default().tail,
        },
        batch: std::num::NonZeroUsize::new(2).expect("not zero"),
    }
}

/// Starts a registered node over the metadata log in `dir/meta` and `store`, registered
/// with `frontend`, and serving in a task. It returns once the node has registered and is
/// assembled (the rebuild, if it owes one, runs in the background).
async fn start<O: ObjectStore>(
    dir: &Path,
    meta: &str,
    store: O,
    frontend: Option<&Frontend>,
    rebuild: RebuildConfig,
) -> Node {
    let meta_dir = dir.join(meta);
    let mut argv = vec![
        "storage".to_owned(),
        "--storage-listen=127.0.0.1:0".to_owned(),
        format!("--cold-store=file://{}", dir.join("unused-cold").display()),
        format!("--meta-dir={}", meta_dir.display()),
    ];
    if let Some(frontend) = frontend {
        argv.push(format!("--frontend=http://{}", frontend.addr));
    }
    let matches = command().try_get_matches_from(argv).expect("the flags");
    let flags = parse(&matches).expect("the flags");
    let log = RaftMetaLog::open_dir(&meta_dir, Retention::default(), "kbf/")
        .await
        .expect("open the log");
    let (prefix, id) = (log.store_prefix(), log.log_id());
    let repair_dir = meta_dir.join(REPAIR_DIR);
    let node = assemble(
        &flags,
        flags.listener().expect("a listener"),
        None,
        (log, store, prefix, id),
        (|| Ok(pending::<()>()), &repair_dir),
        None,
        rebuild,
    )
    .await
    .expect("the node assembles");
    let channel = Endpoint::from_shared(format!("http://{}", node.addr))
        .expect("endpoint")
        .connect_lazy();
    let catch_up = node.catch_up();
    Node {
        catch_up,
        // The map of a registered node is the fake frontend's, at generation 4.
        client: RemoteStorage::new(channel, if frontend.is_some() { 4 } else { 0 }),
        serving: tokio::spawn(async move { drop(node.serve().await) }),
    }
}

/// Waits until the log in `dir` can be opened again after its node was killed (its lock is
/// released when the last task holding it has gone), then lets it go.
async fn reopen_when_free(dir: &Path) {
    for _ in 0..1000 {
        if let Ok(log) = RaftMetaLog::open_dir(dir, Retention::default(), "kbf/").await {
            drop(log);
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the log in {} is still held", dir.display());
}

/// `n` blobs, one segment each, written as an old server over a log with id `log`
/// writes them: the objects of an epoch writer under that log's prefix.
async fn old_segments<O: ObjectStore + Clone>(
    store: &O,
    log: u64,
    n: usize,
) -> Vec<(Digest, Bytes)> {
    let keys = ObjectKeys {
        legacy: KeyPrefix::new(format!("kbf/{log:016x}/")).expect("prefix"),
        root: KeyPrefix::new("kbf/").expect("root"),
    };
    let cache = Cache::open_under(
        MemoryMetaLog::new(Retention::default()),
        store.clone(),
        keys,
    )
    .await
    .expect("open as a server");
    let mut blobs = Vec::new();
    for i in 0..n {
        let bytes = Bytes::from(format!("old server {log} blob {i}"));
        let blob = VerifiedBlob::hashed(bytes.clone());
        blobs.push((blob.digest(), bytes));
        cache.store_blobs(vec![blob]).await.expect("store");
    }
    blobs
}

/// Writes through a node, wipes its metadata directory, restarts over the same store with
/// a larger boot, and checks the new node holds every blob.
async fn wipe_and_rebuild<O: ObjectStore + Clone>(store: O, dir: &Path) {
    let frontend = Frontend::start();
    let first = start(dir, "meta-first", store.clone(), Some(&frontend), small()).await;
    within("the rebuild to finish", first.catch_up.finished()).await;
    let mut blobs = Vec::new();
    for i in 0..5 {
        let bytes = Bytes::from(format!("blob {i} of the first node"));
        let digest = first.client.put(bytes.clone()).await.expect("put");
        blobs.push((digest, bytes));
    }
    first.holds(&blobs).await;
    first.kill().await;

    // The metadata directory is gone: a new log, and the frontend hands out boot 2.
    let second = start(dir, "meta-second", store, Some(&frontend), small()).await;
    within("the rebuild to finish", second.catch_up.finished()).await;
    second.holds(&blobs).await;
    assert_eq!(frontend.registrations(), [true, true]);
}

/// Catches: a wiped node that does not hold what the store holds. A node registers, writes
/// 5 blobs (a segment each) to an object store with conditional puts (the shape of S3),
/// and is killed; a new node with no metadata log registers (boot 2) over the same store:
/// once its rebuild is done every blob reads back with its bytes and FindMissing is empty.
/// Mutants seen red: `rebuild_index` that returns done without adopting (every read is
/// NOT_FOUND once the gate lifts); a rebuild that stops after the first page (small
/// pages: 2 of the 5 held).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wipe_rebuild_holds_blobs_in_a_bucket() {
    wipe_and_rebuild(Bucket::new(), &scratch("wipe-bucket")).await;
}

/// The same over a `file://` cold store: the rebuild lists a directory and reads each
/// object file's tail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wipe_rebuild_holds_blobs_in_a_directory() {
    let dir = scratch("wipe-files");
    let files = FsStore::open(&dir.join("cold"), Floor::Bytes(1)).expect("open the store");
    wipe_and_rebuild(files, &dir).await;
}

/// Catches: a node that restarts in the middle of its rebuild and either starts over from
/// the first key, or believes it finished, or serves a false miss meanwhile. The store
/// holds 6 old segments; the first start's listing holds after its second page (4 objects
/// handled) and the node is killed; the second start (the same log) registers as
/// rebuilding again, is still catching up while its listing is held, resumes after a key
/// of the first start's progress (not from the top), answers every read of a blob not yet
/// indexed UNAVAILABLE (never NOT_FOUND), and once released holds all 6. Mutants seen red:
/// progress not persisted (the first listing call has no token); `RebuildDone` committed
/// before the listing (the second start registers as caught up, and its reads of the two
/// unindexed blobs are NOT_FOUND).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_mid_rebuild_resumes_and_stays_catching_up() {
    let dir = scratch("mid-rebuild");
    let frontend = Frontend::start();
    let bucket = Bucket::new();
    let old = old_segments(&bucket, 0x11, 6).await;
    let keys = bucket.keys().await;
    assert_eq!(keys.len(), 6);

    let held = bucket.scripted(3);
    let first = start(&dir, "meta", held.clone(), Some(&frontend), small()).await;
    within("the first start to list", held.script.entered.notified()).await;
    // The four objects of the first two pages are indexed; wait until they read back.
    let mut indexed = 0;
    for _ in 0..2000 {
        indexed = 0;
        for (digest, _) in &old {
            match first.client.read_blob(digest).await {
                Ok(_) => indexed += 1,
                Err(StorageError::Unavailable(_)) => {}
                Err(other) => panic!("a read during catch-up: {other:?}"),
            }
        }
        if indexed >= 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(indexed, 4, "the first two pages are indexed, and no more");
    first.kill().await;
    reopen_when_free(&dir.join("meta")).await;

    let gated = bucket.scripted(1);
    let second = start(&dir, "meta", gated.clone(), Some(&frontend), small()).await;
    within(
        "the restarted node to list",
        gated.script.entered.notified(),
    )
    .await;
    assert_eq!(frontend.registrations(), [true, true], "rebuilding again");
    assert!(second.catch_up.is_catching_up());
    let token = gated
        .first_token()
        .expect("a listing call")
        .expect("resumed, not from the top");
    assert!(
        [1, 3].iter().any(|&i| token.contains(keys[i].as_str())),
        "resumed after a batch boundary of the first start: {token}"
    );
    let mut unavailable = 0;
    for (digest, _) in &old {
        match second.client.read_blob(digest).await {
            Ok(_) => {}
            Err(StorageError::Unavailable(_)) => unavailable += 1,
            Err(other) => panic!("a read while catching up must not be a miss: {other:?}"),
        }
    }
    assert!(unavailable >= 1, "two blobs are not indexed yet");

    gated.release();
    within("the rebuild to finish", second.catch_up.finished()).await;
    second.holds(&old).await;
}

/// Catches: a rebuild that runs again at every start (it lists the store at each restart,
/// and registers as catching up each time): a log that finished one is done. The first
/// start rebuilds (an empty store), writes, and is killed; the second over the same log
/// registers as caught up, is not catching up when it returns, answers a miss NOT_FOUND
/// (not UNAVAILABLE), and never lists the store. Mutant seen red: the marker not read
/// (the second start registers as rebuilding and lists).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_rebuild_is_not_run_again_by_a_restart() {
    let dir = scratch("finished");
    let frontend = Frontend::start();
    let bucket = Bucket::new();
    let first = start(&dir, "meta", bucket.scripted(0), Some(&frontend), small()).await;
    within("the rebuild to finish", first.catch_up.finished()).await;
    let bytes = Bytes::from_static(b"a blob of the first start");
    let digest = first.client.put(bytes.clone()).await.expect("put");
    first.kill().await;
    reopen_when_free(&dir.join("meta")).await;

    let view = bucket.scripted(0);
    let second = start(&dir, "meta", view.clone(), Some(&frontend), small()).await;
    assert_eq!(frontend.registrations(), [true, false]);
    assert!(!second.catch_up.is_catching_up());
    assert_eq!(second.client.read_blob(&digest).await.expect("read"), bytes);
    let unknown = kbf_segments::sha256(b"never stored");
    assert!(matches!(
        second.client.read_blob(&unknown).await,
        Err(StorageError::NotFound(_))
    ));
    assert_eq!(view.lists(), 0, "a finished rebuild lists nothing");
}

/// Catches: a rebuild that gives up, or declares itself done, when the store cannot be
/// listed: the node would then serve misses as NOT_FOUND over an index it never built. The
/// listing fails (every call); the rebuild is attempted again after a back-off (the second
/// attempt is seen), the node is still catching up and still answers a miss UNAVAILABLE;
/// once the store lists again the rebuild completes and every blob is held. Mutants seen
/// red: the failure arm lifting the gate (the node is not catching up after the first
/// failure); the failure arm ending the task (no second listing call).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_rebuild_is_tried_again_and_the_node_stays_catching_up() {
    let dir = scratch("retry");
    let frontend = Frontend::start();
    let bucket = Bucket::new();
    let old = old_segments(&bucket, 0x11, 3).await;
    let view = bucket.scripted(0);
    view.fail_lists(true);
    let node = start(&dir, "meta", view.clone(), Some(&frontend), small()).await;
    within("a second attempt to list", async {
        while view.lists() < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(node.catch_up.is_catching_up());
    for (digest, _) in &old {
        let read = node.client.read_blob(digest).await;
        assert!(
            matches!(read, Err(StorageError::Unavailable(_))),
            "{read:?}"
        );
    }
    view.fail_lists(false);
    within("the rebuild to finish", node.catch_up.finished()).await;
    node.holds(&old).await;
}

/// Catches: a node without `--frontend` that rebuilds (it keeps its objects under its own
/// log's prefix, and nothing registers it as catching up, so a rebuild there would only
/// hold its misses back): it is not catching up when it returns, answers a miss NOT_FOUND
/// at once, and never lists the store. Mutant seen red: `open_cache` taking every node as
/// rebuilding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_without_a_frontend_owes_no_rebuild() {
    let dir = scratch("alone");
    let bucket = Bucket::new();
    let old = old_segments(&bucket, 0x11, 2).await;
    let view = bucket.scripted(0);
    let node = start(&dir, "meta", view.clone(), None, small()).await;
    assert!(!node.catch_up.is_catching_up());
    let read = node.client.read_blob(&old[0].0).await;
    assert!(matches!(read, Err(StorageError::NotFound(_))), "{read:?}");
    assert_eq!(view.lists(), 0);
}
