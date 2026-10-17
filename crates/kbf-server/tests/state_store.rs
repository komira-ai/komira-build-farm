//! The farm over a state store: what it writes before it acknowledges, and what a farm
//! started over the same store takes up again.
//!
//! The store is the in-memory fake, which keeps a durable copy a restart reopens, and
//! the farm's clock is tokio's, paused: nothing here waits. The tests that restart the
//! farm over its store run a second time over a real SQLite file, closed and reopened.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_front::{Cache, Described, Dispatch, Finished, Stage, Submission};
use kbf_proto::google::rpc;
use kbf_proto::worker::{self, ServerMessage, server_message};
use kbf_sched::{DaemonInstance, Request};
use kbf_server::farm::{Reported, StreamId};
use kbf_server::{EpochPolicy, Farm, Listeners, ServeError, StateError, StateStore};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::sqlite::{Options, SqliteStore};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, Fenced, ManualClock, Revision, ScanPage, ScanRange,
    Store, StoreError, StoreTime, Txn, Versioned,
};
use kbf_types::{
    ActionKey, Digest, DigestFunction, LeaseId, LeaseKind, MemoryRun, OperationId, Qos, Resources,
    WorkerId,
};
use tokio::sync::mpsc;
use tonic::{Code, Status};

#[path = "state_store/fate.rs"]
mod fate;
#[path = "state_store/handover.rs"]
mod handover;

const GIB: u64 = 1 << 30;

type TestFarm = Farm;
type Inbox = mpsc::UnboundedReceiver<Result<ServerMessage, Status>>;

/// A store whose writes can be made to fail, over the fake (which a restart reopens from
/// what was synced).
struct Flaky {
    inner: Inner,
    down: AtomicBool,
    fenced: AtomicBool,
    /// How many more writes take before the store goes down; negative for no limit.
    allow: AtomicI64,
}

/// What the store is kept in.
#[derive(Clone, Copy)]
enum Backing {
    /// The in-memory fake.
    Fake,
    /// A SQLite file in a directory of the test's own.
    Sqlite,
}

enum Inner {
    Fake(Fake),
    Sqlite(Tracked),
}

/// A SQLite store that records when it is closed, so a test can tell a restart that
/// closed the store and opened the file again from one that kept the same object.
struct Tracked {
    store: SqliteStore,
    closed: Arc<AtomicBool>,
}

impl Tracked {
    fn new(store: SqliteStore) -> Self {
        Self {
            store,
            closed: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

impl Inner {
    fn store(&self) -> &dyn Store {
        match self {
            Inner::Fake(store) => store,
            Inner::Sqlite(tracked) => &tracked.store,
        }
    }
}

/// A directory removed when dropped.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("kbf-state-store-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn open_sqlite(path: &std::path::Path) -> SqliteStore {
    SqliteStore::open(
        path,
        Arc::new(kbf_state::SystemClock::default()),
        Options::default(),
    )
    .expect("a SQLite store")
}

impl Flaky {
    fn new() -> Self {
        let inner = Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None);
        Self::over(Inner::Fake(inner))
    }

    fn over(inner: Inner) -> Self {
        Self {
            inner,
            down: AtomicBool::new(false),
            fenced: AtomicBool::new(false),
            allow: AtomicI64::new(-1),
        }
    }

    /// Another writer holds a newer generation: every write fails `Fenced`.
    fn set_fenced(&self) {
        self.fenced.store(true, Ordering::SeqCst);
    }

    /// Set once the SQLite store this wraps is dropped; none over the fake.
    fn closed(&self) -> Option<Arc<AtomicBool>> {
        match &self.inner {
            Inner::Sqlite(tracked) => Some(Arc::clone(&tracked.closed)),
            Inner::Fake(_) => None,
        }
    }

    fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
        if !down {
            self.allow.store(-1, Ordering::SeqCst);
        }
    }

    /// The store takes `writes` more writes, then goes down.
    fn fail_after(&self, writes: i64) {
        self.allow.store(writes, Ordering::SeqCst);
    }

    /// A crash and reopen: what the store acknowledged survives.
    fn restart(self) -> Self {
        let inner = match self.inner {
            Inner::Fake(fake) => Inner::Fake(fake.restart()),
            Inner::Sqlite(tracked) => {
                // Dropping the store ends its writer and releases the file's lock; the
                // file is then opened again by its path, as a new process does.
                let path = tracked.store.path().to_owned();
                drop(tracked);
                Inner::Sqlite(Tracked::new(open_sqlite(&path)))
            }
        };
        Self::over(inner)
    }
}

impl Store for Flaky {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        if self.allow.load(Ordering::SeqCst) == 0 {
            self.down.store(true, Ordering::SeqCst);
        } else if self.allow.load(Ordering::SeqCst) > 0 {
            self.allow.fetch_sub(1, Ordering::SeqCst);
        }
        if self.down.load(Ordering::SeqCst) {
            return Err(StoreError::Failed("the store is down".to_owned()));
        }
        if self.fenced.load(Ordering::SeqCst) {
            return Err(StoreError::Fenced);
        }
        self.inner.store().apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.inner.store().get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.inner.store().scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.store().scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.store().changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.store().reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.store().commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.store().now()
    }
}

/// The store, and farms started over it one after another.
struct Disk {
    store: Arc<Flaky>,
    /// Kept for the SQLite file, and removed after the store is closed.
    _dir: Option<Scratch>,
}

impl Disk {
    fn new() -> Self {
        Self::over(Backing::Fake)
    }

    fn over(backing: Backing) -> Self {
        let (inner, dir) = match backing {
            Backing::Fake => (None, None),
            Backing::Sqlite => {
                let dir = Scratch::new();
                let store = open_sqlite(&dir.0.join("state.db"));
                (Some(Inner::Sqlite(Tracked::new(store))), Some(dir))
            }
        };
        let store = inner.map_or_else(Flaky::new, Flaky::over);
        Self {
            store: Arc::new(store),
            _dir: dir,
        }
    }

    fn farm(&self, epoch: EpochPolicy) -> TestFarm {
        self.try_farm(epoch).expect("a farm over the store")
    }

    fn try_farm(&self, epoch: EpochPolicy) -> Result<TestFarm, StateError> {
        let store: Arc<dyn Store> = self.store.clone();
        Farm::new(
            Arc::new(Cache::memory()),
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
        )
        .with_state(Some(StateStore {
            store,
            epoch,
            members: Default::default(),
            history: None,
        }))
    }

    /// A farm over the store that holds the writer generation after the newest one, as a
    /// process started over the file does ([`kbf_state::Fenced`]): once another farm takes
    /// the next one, every write of this farm fails and none lands.
    fn generation_farm(&self) -> TestFarm {
        let store: Arc<dyn Store> = self.store.clone();
        let fenced = Fenced::take(store, None).expect("the next generation");
        Farm::new(
            Arc::new(Cache::memory()),
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
        )
        .with_state(Some(StateStore {
            store: Arc::new(fenced),
            epoch: EpochPolicy::Keep,
            members: Default::default(),
            history: None,
        }))
        .expect("a farm over the store")
    }

    /// The process dies: its farm is gone, and the store reopens as the disk left it.
    /// Over SQLite the old store must be closed and the file opened again (not the same
    /// object kept), and the new store must read every row the old one held.
    fn crash(&mut self, farm: TestFarm) {
        drop(farm);
        let closed = self.store.closed();
        let rows = closed.as_ref().map(|_| self.all_rows());
        // The placeholder holds no file: a SQLite store must be closed before it reopens.
        let store = Arc::into_inner(std::mem::replace(&mut self.store, Arc::new(Flaky::new())));
        self.store = Arc::new(store.expect("nothing else holds the store").restart());
        if let (Some(closed), Some(rows)) = (closed, rows) {
            assert!(
                closed.load(Ordering::SeqCst),
                "the restart kept the store open"
            );
            let reopened = self.store.closed().expect("a SQLite store");
            assert!(!Arc::ptr_eq(&closed, &reopened), "the same store object");
            assert!(!reopened.load(Ordering::SeqCst), "the new store is open");
            assert!(!rows.is_empty(), "the test wrote nothing before the crash");
            assert_eq!(
                self.all_rows(),
                rows,
                "the reopened file lost what was written"
            );
        }
    }

    fn all_rows(&self) -> Vec<(String, Versioned)> {
        self.store.scan_prefix("").expect("scan")
    }

    fn rows(&self, prefix: &str) -> usize {
        self.store.scan_prefix(prefix).expect("scan").len()
    }
}

fn request(tag: u8) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
        },
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, GIB),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
    }
}

fn submit(farm: &TestFarm, request: Request) -> Result<(), Status> {
    farm.submit(Submission {
        request,
        call: None,
        described: Described::default(),
    })
    .map(drop)
}

fn node() -> WorkerId {
    WorkerId::new("node-a")
}

/// Registers `node-a` with room for four actions, as a daemon opening a stream does.
fn register(farm: &TestFarm) -> (StreamId, Inbox) {
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let reported = Reported::serving(Resources::new(4_000, 8 * GIB), caps);
    let (outbound, inbox) = mpsc::unbounded_channel();
    let stream = farm.register(
        &node(),
        DaemonInstance::new("daemon"),
        DaemonInstance::new(""),
        reported,
        outbound,
        ServerMessage::default(),
    );
    (stream, inbox)
}

/// The leases of the `Start`s waiting in `inbox`, oldest first.
fn starts(inbox: &mut Inbox) -> Vec<LeaseId> {
    let mut leases = Vec::new();
    while let Ok(message) = inbox.try_recv() {
        if let Some(server_message::Message::Start(start)) = message.expect("no error").message {
            let lease = start.lease_id.expect("a lease id");
            leases.push(LeaseId::new(lease.term, lease.seq));
        }
    }
    leases
}

/// A result of `lease` the farm accepts without a cache write: INVALID_ARGUMENT.
fn invalid(lease: LeaseId) -> worker::Result {
    worker::Result {
        lease_id: Some(worker::LeaseId {
            term: lease.term,
            seq: lease.seq,
        }),
        status: Some(rpc::Status {
            code: Code::InvalidArgument as i32,
            message: "test".to_owned(),
            details: Vec::new(),
        }),
        action_result: None,
        action_digest: None,
        memory_kill: 0,
        lost: 0,
    }
}

async fn accepted(farm: &TestFarm, lease: LeaseId) -> bool {
    let ack = farm.report(&node(), invalid(lease)).await;
    ack.expect("an acknowledgement").accepted
}

/// Catches: a farm that forgets a claim it acknowledged (the daemon runs a lease the
/// restarted farm has never heard of and refuses its result), and one that accepts it
/// for another reason than the store (the control, a farm with no store, refuses it).
async fn a_restart_replays_the_claims_it_acknowledged(backing: Backing) {
    let mut disk = Disk::over(backing);
    let before = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&before);
    submit(&before, request(1)).expect("submitted");
    let [lease] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };
    assert_eq!(
        disk.rows("session/"),
        1,
        "the registration is a session row"
    );
    disk.crash(before);

    let control = Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    let (stream, _inbox) = register(&control);
    assert!(control.heartbeat(&node(), stream, 1, vec![lease]).await);
    assert!(
        !accepted(&control, lease).await,
        "a farm with no store knows the lease"
    );

    let after = disk.farm(EpochPolicy::Keep);
    assert_eq!(
        after.lease_epoch(),
        lease.term,
        "the store's epoch is the lease epoch"
    );
    let (stream, _inbox) = register(&after);
    assert!(after.heartbeat(&node(), stream, 1, vec![lease]).await);
    assert!(accepted(&after, lease).await);
    assert_eq!(
        disk.rows("op/"),
        0,
        "the finished operation is gone from the store"
    );
    assert_eq!(disk.rows("lease/"), 0);
}

/// Catches: a scheduler rebuilt from the store that loses the running lease the daemon
/// still holds (the run is wasted), keeps one the daemon never received (the operation
/// never runs), grants a lease id again (the old run's result taken for the new one), or
/// accepts a result of the lease it gave up.
async fn after_a_kill_running_leases_are_rebound_and_undelivered_ones_requeued(backing: Backing) {
    let mut disk = Disk::over(backing);
    let before = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&before);
    submit(&before, request(1)).expect("submitted");
    submit(&before, request(2)).expect("submitted");
    let [running, undelivered] = starts(&mut inbox)[..] else {
        panic!("two leases expected");
    };
    disk.crash(before);

    let after = disk.farm(EpochPolicy::Keep);
    let (stream, mut inbox) = register(&after);
    // The daemon received the first `Start`, not the second.
    assert!(after.heartbeat(&node(), stream, 1, vec![running]).await);
    let regranted = starts(&mut inbox);
    assert_eq!(
        regranted,
        [LeaseId::new(running.term, 2)],
        "a new lease for the second only"
    );
    assert!(
        !accepted(&after, undelivered).await,
        "the given-up lease's result"
    );
    assert!(accepted(&after, running).await, "the re-bound run's result");
    assert!(accepted(&after, regranted[0]).await);
    assert_eq!(disk.rows("op/"), 0);
}

/// Catches: an epoch that does not follow the policy (a SQLite start that keeps it would
/// let a daemon keep the runs of a restored file's leases; a Postgres start that bumps
/// it would kill every run at each frontend start), and leases granted under another
/// term than `Welcome` names.
async fn the_epoch_follows_the_policy_and_names_the_leases(backing: Backing) {
    let mut disk = Disk::over(backing);
    let first = disk.farm(EpochPolicy::Keep);
    let epoch = first.lease_epoch();
    assert_eq!(first.term(), epoch);
    submit(&first, request(1)).expect("queued, no worker yet");
    disk.crash(first);

    let kept = disk.farm(EpochPolicy::Keep);
    assert_eq!(kept.lease_epoch(), epoch);
    disk.crash(kept);

    let bumped = disk.farm(EpochPolicy::Bump);
    assert_eq!(bumped.lease_epoch(), epoch + 1);
    assert_eq!(bumped.term(), epoch + 1);
    let (_, mut inbox) = register(&bumped);
    assert_eq!(starts(&mut inbox), [LeaseId::new(epoch + 1, 0)]);
}

/// Catches: a request acknowledged to its client that the store did not take (a restart
/// would forget work the client believes queued): the submission must fail UNAVAILABLE
/// and queue nothing.
#[tokio::test(start_paused = true)]
async fn a_request_the_store_refuses_is_not_queued() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    disk.store.set_down(true);
    let refused = submit(&farm, request(1)).expect_err("refused");
    assert_eq!(refused.code(), Code::Unavailable);
    disk.store.set_down(false);
    let (_, mut inbox) = register(&farm);
    assert!(
        starts(&mut inbox).is_empty(),
        "a refused request was queued"
    );
    assert_eq!(disk.rows("op/"), 0);
}

/// Catches: a `Start` sent for a grant the store did not take (the daemon would run a
/// lease a restart forgets), and a farm that does not recover from it: the grant that
/// never started is given up after the start grace and granted again, once the store
/// takes it. (The submission's write takes; the grant's, the next, does not.)
#[tokio::test(start_paused = true)]
async fn a_grant_the_store_refuses_is_not_started_and_is_granted_again() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (stream, mut inbox) = register(&farm);
    disk.store.fail_after(1);
    submit(&farm, request(1)).expect("queued");
    assert!(
        starts(&mut inbox).is_empty(),
        "a Start went out for an unwritten grant"
    );
    disk.store.set_down(false);

    tokio::time::advance(kbf_sched::fence::START_GRACE + Duration::from_secs(1)).await;
    assert!(farm.heartbeat(&node(), stream, 1, Vec::new()).await);
    let regranted = starts(&mut inbox);
    assert_eq!(regranted, [LeaseId::new(farm.lease_epoch(), 1)]);
    assert_eq!(disk.rows("lease/"), 1);
    assert_eq!(disk.rows("queue/"), 0);
}

/// Catches: a grant made while the store is known to be down (it cannot be written, so
/// its `Start` is dropped round after round, each burning a lease sequence): nothing is
/// placed from the first refused write until one takes, and the work is granted at once
/// then.
#[tokio::test(start_paused = true)]
async fn nothing_is_granted_while_the_store_is_down() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    submit(&farm, request(1)).expect("queued, no worker yet");
    disk.store.set_down(true);
    let (stream, mut inbox) = register(&farm);
    assert!(starts(&mut inbox).is_empty());
    farm.tick().await;
    disk.store.set_down(false);
    assert!(farm.heartbeat(&node(), stream, 1, Vec::new()).await);
    assert_eq!(
        starts(&mut inbox),
        [LeaseId::new(farm.lease_epoch(), 0)],
        "no lease sequence was spent while the store was down"
    );
    assert_eq!(disk.rows("lease/"), 1);
}

/// Catches: a superseded scheduler (its writes fail Fenced) treated as a store outage
/// (pinging forever, answering UNAVAILABLE so clients retry it) or still granting work: it
/// must answer FAILED_PRECONDITION, grant nothing, and stay refusing when writes could
/// again succeed.
#[tokio::test(start_paused = true)]
async fn a_superseded_scheduler_refuses_work_and_does_not_wait_for_the_store() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (stream, mut inbox) = register(&farm);
    disk.store.set_fenced();
    let refused = submit(&farm, request(1)).expect_err("refused");
    assert_eq!(refused.code(), Code::FailedPrecondition);
    farm.tick().await;
    assert!(farm.heartbeat(&node(), stream, 1, Vec::new()).await);
    assert!(
        starts(&mut inbox).is_empty(),
        "a superseded scheduler granted"
    );
    assert_eq!(disk.rows("op/"), 0);
    assert_eq!(disk.rows("lease/"), 0);
}

/// Catches: a lease given up while the store is never told of.
/// The store would still hold the old lease and no queue row, so the grant that follows
/// is refused for want of a queue row, its `Start` is dropped, and the operation never
/// runs until a restart. Once the store is back, the given-up lease is written and the
/// operation is granted again.
#[tokio::test(start_paused = true)]
async fn a_requeue_the_store_refused_is_written_when_it_returns() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (stream, mut inbox) = register(&farm);
    submit(&farm, request(1)).expect("submitted");
    assert_eq!(starts(&mut inbox).len(), 1);
    disk.store.set_down(true);
    tokio::time::advance(kbf_sched::fence::LEASE_GRACE + Duration::from_secs(1)).await;
    farm.tick().await;
    assert_eq!(
        disk.rows("lease/"),
        1,
        "the store was down: it holds the lease"
    );
    disk.store.set_down(false);

    assert!(farm.heartbeat(&node(), stream, 1, Vec::new()).await);
    let regranted = starts(&mut inbox);
    assert_eq!(
        regranted,
        [LeaseId::new(farm.lease_epoch(), 1)],
        "the operation was not granted again"
    );
    assert_eq!((disk.rows("lease/"), disk.rows("queue/")), (1, 0));
    assert!(accepted(&farm, regranted[0]).await);
    assert_eq!(disk.rows("op/"), 0);
}

/// Pins (it passes without the repair above): a start that bumps the epoch over a store
/// holding a granted lease takes the lease up, and when the daemon, which kills the runs
/// of an earlier epoch, does not list it, requeues it at once under the new term. The
/// old lease's result is refused.
async fn a_bumped_epoch_requeues_the_old_term_leases_the_daemon_killed(backing: Backing) {
    let mut disk = Disk::over(backing);
    let before = disk.farm(EpochPolicy::Keep);
    let epoch = before.lease_epoch();
    let (_, mut inbox) = register(&before);
    submit(&before, request(1)).expect("submitted");
    let [old] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };
    assert_eq!(old.term, epoch);
    disk.crash(before);

    let after = disk.farm(EpochPolicy::Bump);
    assert_eq!(after.lease_epoch(), epoch + 1);
    let (stream, mut inbox) = register(&after);
    assert!(after.heartbeat(&node(), stream, 1, Vec::new()).await);
    assert_eq!(starts(&mut inbox), [LeaseId::new(epoch + 1, 1)]);
    assert!(!accepted(&after, old).await, "the old term's result");
    assert_eq!(disk.rows("lease/"), 1);
}

/// Catches: an outage that makes every lease look expired the moment the store returns.
/// The farm could not write, so it could not hear its workers either; the lease must not
/// be given up (and, here, requeued in the store) until the grace after the store's return
/// has passed, and a lease nobody renewed goes after it.
#[tokio::test(start_paused = true)]
async fn an_outage_requeues_nothing_inside_the_grace_and_the_unrenewed_lease_after_it() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (_stream, mut inbox) = register(&farm);
    submit(&farm, request(1)).expect("submitted");
    assert_eq!(starts(&mut inbox).len(), 1);

    disk.store.set_down(true);
    let refused = submit(&farm, request(2)).expect_err("the outage is noticed");
    assert_eq!(refused.code(), Code::Unavailable);
    // The farm's clock runs on through the outage; nobody is heard meanwhile.
    tokio::time::advance(kbf_sched::fence::LEASE_GRACE * 10).await;
    farm.tick().await;
    disk.store.set_down(false);
    farm.tick().await;
    assert_eq!(
        (disk.rows("lease/"), disk.rows("queue/")),
        (1, 0),
        "the lease was given up the moment the store returned"
    );

    tokio::time::advance(kbf_sched::fence::LEASE_GRACE - Duration::from_secs(5)).await;
    farm.tick().await;
    assert_eq!((disk.rows("lease/"), disk.rows("queue/")), (1, 0));
    assert!(starts(&mut inbox).is_empty());

    tokio::time::advance(Duration::from_secs(10)).await;
    farm.tick().await;
    assert_eq!(
        (disk.rows("lease/"), disk.rows("queue/")),
        (0, 1),
        "the unrenewed lease outlived the grace"
    );
}

/// Catches: renewal per lease (a heartbeat of a node holding three leases writing three
/// rows, or any write per heartbeat) instead of one row per daemon session per interval;
/// a renewal that touches a lease row (the fencing token is written at the grant only);
/// and a session row that never expires (a daemon that left would look live for ever).
#[tokio::test(start_paused = true)]
async fn a_daemon_session_is_renewed_once_per_interval_not_per_lease() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (stream, mut inbox) = register(&farm);
    for tag in 1..=3 {
        submit(&farm, request(tag)).expect("submitted");
    }
    let leases = starts(&mut inbox);
    assert_eq!(leases.len(), 3);
    let session = || {
        let row = disk.store.get("session/node-a").expect("get");
        row.expect("a session row")
    };
    let first = session();
    assert!(first.expires_at.is_some(), "a session row must expire");
    let lease_rows = disk.store.scan_prefix("lease/").expect("scan");
    let writes = disk.store.commit_stats().txns;

    let mut seq = 0;
    for _ in 0..5 {
        tokio::time::advance(Duration::from_secs(1)).await;
        seq += 1;
        assert!(farm.heartbeat(&node(), stream, seq, leases.clone()).await);
    }
    assert_eq!(
        disk.store.commit_stats().txns,
        writes,
        "a heartbeat inside the interval wrote"
    );

    tokio::time::advance(kbf_server::SESSION_RENEWAL).await;
    seq += 1;
    assert!(farm.heartbeat(&node(), stream, seq, leases).await);
    assert_eq!(
        disk.store.commit_stats().txns,
        writes + 1,
        "one write renews the session, whatever it holds"
    );
    let renewed = session();
    assert!(renewed.version > first.version);
    assert!(renewed.expires_at > first.expires_at);
    assert_eq!(disk.store.scan_prefix("lease/").expect("scan"), lease_rows);
}

/// Catches: a write refused as stale (its row is gone: the store working) taken for an
/// outage, which would hold every lease for the grace: a result whose lease row was
/// deleted behind the farm's back is dropped, and a silent worker's other lease is
/// still given up at the lease grace.
#[tokio::test(start_paused = true)]
async fn a_write_refused_as_stale_is_not_an_outage() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (_stream, mut inbox) = register(&farm);
    submit(&farm, request(1)).expect("submitted");
    submit(&farm, request(2)).expect("submitted");
    let [first, _second] = starts(&mut inbox)[..] else {
        panic!("two leases expected");
    };
    let stale = Txn::new().delete(
        format!("lease/{:016x}.{:016x}", first.term, first.seq),
        kbf_state::Cond::Any,
    );
    disk.store.txn(stale).expect("store").expect("delete");
    assert!(
        !accepted(&farm, first).await,
        "a result whose lease row is gone was acknowledged"
    );
    assert_eq!(disk.rows("op/"), 2);

    tokio::time::advance(kbf_sched::fence::LEASE_GRACE + Duration::from_secs(1)).await;
    farm.tick().await;
    assert_eq!(
        disk.rows("queue/"),
        1,
        "the silent worker's lease was held for an outage's grace"
    );
}

/// Catches: a session renewal the store refused that is not tried again (the row would
/// expire under a daemon that heartbeats): the next heartbeat writes it.
#[tokio::test(start_paused = true)]
async fn a_session_renewal_the_store_refused_is_tried_at_the_next_heartbeat() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (stream, _inbox) = register(&farm);
    let session = || {
        disk.store
            .get("session/node-a")
            .expect("get")
            .expect("a row")
    };
    let first = session();
    tokio::time::advance(kbf_server::SESSION_RENEWAL + Duration::from_secs(1)).await;
    disk.store.set_down(true);
    assert!(farm.heartbeat(&node(), stream, 1, Vec::new()).await);
    assert_eq!(session().version, first.version);
    disk.store.set_down(false);
    assert!(farm.heartbeat(&node(), stream, 2, Vec::new()).await);
    assert!(
        session().version > first.version,
        "the refused renewal was not tried again"
    );
}

/// Loses operation 1's lease as a daemon that reconnected without it does, on `farm`: a
/// new session of the daemon, and a heartbeat that does not list the lease. Returns the
/// new session's stream and inbox.
async fn lose_lease(farm: &TestFarm) -> (StreamId, Inbox) {
    let (stream, inbox) = register(farm);
    assert!(farm.heartbeat(&node(), stream, 1, Vec::new()).await);
    (stream, inbox)
}

/// Catches: a loss count that a restart forgets (an action that kills the scheduler's
/// host starts from none each time and loops for ever): two losses are written, survive a
/// crash, and the third, the recovered lease's, refuses the operation, whose rows then go.
async fn lost_leases_are_counted_across_a_restart_and_the_third_is_refused(backing: Backing) {
    let mut disk = Disk::over(backing);
    let before = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&before);
    submit(&before, request(1)).expect("submitted");
    assert_eq!(starts(&mut inbox).len(), 1);
    for lost in 1..=2u8 {
        let (_, mut inbox) = lose_lease(&before).await;
        assert_eq!(
            starts(&mut inbox).len(),
            1,
            "granted again after loss {lost}"
        );
        assert_eq!(disk.rows("loss/"), 1, "the count is written");
    }
    disk.crash(before);
    assert_eq!(disk.rows("loss/"), 1, "the count survived the restart");
    assert_eq!(disk.rows("op/"), 1, "the operation survived the restart");

    let after = disk.farm(EpochPolicy::Keep);
    let (stream, mut inbox) = register(&after);
    assert!(after.heartbeat(&node(), stream, 1, Vec::new()).await);
    assert!(
        starts(&mut inbox).is_empty(),
        "a third lost lease was granted again"
    );
    assert_eq!(disk.rows("op/"), 0, "the refused operation was kept");
    assert_eq!(disk.rows("loss/"), 0, "its count was kept");
    assert_eq!((disk.rows("lease/"), disk.rows("queue/")), (0, 0));
}

/// Catches: a refusal that leaves the caller waiting, or answers with another code than
/// FAILED_PRECONDITION (the code of every refusal), or with no reason.
#[tokio::test(start_paused = true)]
async fn the_third_lost_lease_answers_the_caller_with_the_reason() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&farm);
    let ticket = farm
        .submit(Submission {
            request: request(1),
            call: None,
            described: Described::default(),
        })
        .expect("submitted");
    assert_eq!(starts(&mut inbox).len(), 1);
    for _ in 1..=2 {
        let (_, mut inbox) = lose_lease(&farm).await;
        assert_eq!(starts(&mut inbox).len(), 1);
    }
    assert!(!matches!(*ticket.stage.borrow(), Stage::Done(_)));
    let (_, mut inbox) = lose_lease(&farm).await;
    assert!(starts(&mut inbox).is_empty());
    let Stage::Done(Finished::Failed(status)) = ticket.stage.borrow().clone() else {
        panic!("the caller was not answered");
    };
    assert_eq!(status.code, Code::FailedPrecondition as i32);
    assert!(status.message.contains("lost"), "{}", status.message);
}

/// Catches: a lease given up for silence that stays in the store as held (a restart
/// would re-bind a run nobody holds), and a refused operation that stays in it (it
/// would be refused again, or run).
#[tokio::test(start_paused = true)]
async fn a_given_up_lease_and_a_refusal_are_written() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&farm);
    submit(&farm, request(1)).expect("submitted");
    assert_eq!(starts(&mut inbox).len(), 1);
    assert_eq!((disk.rows("lease/"), disk.rows("queue/")), (1, 0));
    tokio::time::advance(kbf_sched::fence::LEASE_GRACE + Duration::from_secs(1)).await;
    farm.tick().await;
    assert_eq!(
        (disk.rows("lease/"), disk.rows("queue/")),
        (0, 1),
        "the lease was kept"
    );

    tokio::time::advance(kbf_sched::UNSERVABLE_WAIT + Duration::from_secs(1)).await;
    farm.tick().await;
    assert_eq!(disk.rows("op/"), 0, "the refused operation was kept");
    assert_eq!(disk.rows("queue/"), 0);
}

/// Catches: a second caller for the same action written as another operation (the work
/// would run twice after a restart), and a networked action's twin joined (those runs are
/// not interchangeable).
#[tokio::test(start_paused = true)]
async fn a_twin_joins_and_writes_no_operation() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    submit(&farm, request(1)).expect("first");
    submit(&farm, request(1)).expect("twin");
    assert_eq!(disk.rows("op/"), 1);
    let networked = Request {
        hermetic: false,
        ..request(1)
    };
    submit(&farm, networked.clone()).expect("networked");
    submit(&farm, networked).expect("networked again");
    assert_eq!(disk.rows("op/"), 3);
}

/// Catches: a store holding a row the scheduler did not write taken as empty (work
/// silently lost at a start): the farm must refuse to start over it.
#[tokio::test(start_paused = true)]
async fn a_store_with_an_unreadable_row_refuses_the_farm() {
    let disk = Disk::new();
    let put = Txn::new().put("op/0000000000000001", vec![0xff], kbf_state::Cond::Any);
    disk.store.txn(put).expect("store").expect("put");
    let refused = disk.try_farm(EpochPolicy::Keep).expect_err("refused");
    assert!(matches!(refused, StateError::Corrupt { .. }), "{refused:?}");
}

/// Runs each restart test over the fake and over a SQLite file: what the fake keeps
/// across a crash is what the real store must.
macro_rules! restarts {
    ($($name:ident),* $(,)?) => {$(
        mod $name {
            use super::*;

            #[tokio::test(start_paused = true)]
            async fn over_the_fake() {
                super::$name(Backing::Fake).await;
            }

            #[tokio::test(start_paused = true)]
            async fn over_sqlite() {
                super::$name(Backing::Sqlite).await;
            }
        }
    )*};
}

restarts!(
    a_restart_replays_the_claims_it_acknowledged,
    after_a_kill_running_leases_are_rebound_and_undelivered_ones_requeued,
    the_epoch_follows_the_policy_and_names_the_leases,
    a_bumped_epoch_requeues_the_old_term_leases_the_daemon_killed,
    lost_leases_are_counted_across_a_restart_and_the_third_is_refused
);

/// A state store opened as a start opens it: from the flag, over `dir`.
fn store_from_flag(dir: &std::path::Path) -> Result<StateStore, StateError> {
    let uri = format!("--state-store=sqlite://{}", dir.join("state.db").display());
    let args = <kbf_server::Args as clap::Parser>::parse_from(["kbf-server", &uri]);
    args.open_state_store()
        .map(|state| state.expect("the flag is given"))
}

fn farm_over(state: StateStore) -> TestFarm {
    Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .with_state(Some(state))
    .expect("a farm over the store")
}

/// Catches: `--state-store=sqlite:///...` parsed and then not opened (the scheduler runs
/// in memory while the operator believes its queue is kept): an operation submitted
/// through a farm over the flag's store is granted by the farm a restart starts over the
/// same file, with the term a SQLite start owes (one past the first's).
#[tokio::test(start_paused = true)]
async fn an_operation_submitted_over_the_flag_survives_a_restart_of_the_file() {
    let dir = Scratch::new();
    let before = farm_over(store_from_flag(&dir.0).expect("a first start"));
    let epoch = before.lease_epoch();
    submit(&before, request(1)).expect("queued, no worker yet");
    drop(before);

    let after = farm_over(store_from_flag(&dir.0).expect("a second start"));
    assert_eq!(after.lease_epoch(), epoch + 1, "every SQLite start is new");
    let (_, mut inbox) = register(&after);
    let leases = starts(&mut inbox);
    assert_eq!(leases.len(), 1, "the queued operation was recovered");
    assert_eq!(leases[0].term, epoch + 1);
}

/// Catches: a second frontend started on a state file another holds (two schedulers
/// would hand out the same leases): refused naming the lock, and the file is free again
/// once the first goes.
#[test]
fn a_second_frontend_on_the_same_sqlite_file_is_refused() {
    let dir = Scratch::new();
    let first = store_from_flag(&dir.0).expect("a first start");
    let refused = store_from_flag(&dir.0).err().expect("the file is held");
    assert!(matches!(refused, StateError::Open(_)), "{refused:?}");
    let said = refused.to_string();
    assert!(said.contains("another kbf process"), "{said}");
    assert!(said.contains("state.db.lock"), "{said}");
    drop(first);
    store_from_flag(&dir.0).expect("free again");
}

fn listeners() -> Listeners {
    let loopback = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
    Listeners {
        reapi: loopback,
        worker: loopback,
        reapi_tls: None,
        worker_tls: None,
        heartbeat_interval: Duration::from_secs(1),
        hello_wait: Duration::from_secs(1),
        tick: Duration::from_millis(50),
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
        memory_ladder: kbf_server::MemoryLadder::default(),
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        streams: kbf_server::StreamTiming::default(),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    }
}

/// Catches: a server that binds with a state store and ignores it (the scheduler would
/// run in memory while the operator believes it durable), and one that starts over a
/// store it cannot read.
#[tokio::test]
async fn a_server_binds_over_a_state_store_or_refuses_it() {
    let disk = Disk::new();
    let state = |disk: &Disk| StateStore {
        store: disk.store.clone(),
        epoch: EpochPolicy::Keep,
        members: Default::default(),
        history: None,
    };
    let bind = |state| {
        kbf_server::bind_server_with_state(
            Arc::new(Cache::memory()),
            listeners(),
            None,
            kbf_auth::Policy::allow_all(),
            Some(state),
            std::future::pending::<()>(),
        )
    };
    let bound = bind(state(&disk)).expect("binds over an empty store");
    drop(bound);
    assert_eq!(
        disk.rows("meta/epoch"),
        1,
        "the server settled the epoch in its store"
    );

    let put = Txn::new().put("op/0000000000000001", vec![0xff], kbf_state::Cond::Any);
    disk.store.txn(put).expect("store").expect("put");
    let refused = bind(state(&disk)).err().expect("refused");
    assert!(
        matches!(refused, ServeError::State(StateError::Corrupt { .. })),
        "{refused:?}"
    );
    let said = refused.to_string();
    assert!(
        said.starts_with("--state-store: ") && said.contains("op/0000000000000001"),
        "{said}"
    );
    assert!(
        std::error::Error::source(&refused).is_some(),
        "the cause is kept"
    );
}

/// How a caller's ticket ended, once it is done.
fn ended(ticket: &kbf_front::Ticket) -> Finished {
    let Stage::Done(finished) = ticket.stage.borrow().clone() else {
        panic!("the caller was not answered");
    };
    finished
}

/// Catches: a result that lives only in the process that finished the operation (a
/// process that kept a caller's stream open across a handover has nothing to answer it
/// with): once a worker's result is accepted, the store holds the result the caller was
/// sent, as a `done/` row, and nothing else of the operation.
#[tokio::test(start_paused = true)]
async fn a_finished_operation_leaves_its_result_in_the_store() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&farm);
    let ticket = farm
        .submit(Submission {
            request: request(1),
            call: None,
            described: Described::default(),
        })
        .expect("submitted");
    let leases = starts(&mut inbox);
    assert!(
        farm.ended(kbf_types::OperationId(0))
            .expect("read")
            .is_none()
    );
    assert!(accepted(&farm, leases[0]).await);
    let sent = ended(&ticket);
    assert!(matches!(&sent, Finished::Failed(s) if s.code == Code::InvalidArgument as i32));
    assert_eq!(disk.rows("done/"), 1);
    assert_eq!(disk.rows("op/"), 0, "the operation itself is finished");
    assert_eq!(
        farm.ended(kbf_types::OperationId(0)).expect("read"),
        Some(sent)
    );
}

/// Catches: a refusal that leaves no row (a stream kept open by an old process would wait
/// on an operation that was refused before the handover): the row holds the refusal the
/// caller was sent.
#[tokio::test(start_paused = true)]
async fn a_refused_operation_leaves_its_refusal_in_the_store() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&farm);
    let ticket = farm
        .submit(Submission {
            request: request(1),
            call: None,
            described: Described::default(),
        })
        .expect("submitted");
    assert_eq!(starts(&mut inbox).len(), 1);
    for _ in 0..2 {
        let (_, mut inbox) = lose_lease(&farm).await;
        assert_eq!(starts(&mut inbox).len(), 1);
    }
    let (_, _inbox) = lose_lease(&farm).await;
    let sent = ended(&ticket);
    assert!(matches!(&sent, Finished::Failed(s) if s.code == Code::FailedPrecondition as i32));
    assert_eq!(disk.rows("done/"), 1);
    assert_eq!(
        farm.ended(kbf_types::OperationId(0)).expect("read"),
        Some(sent)
    );
}

/// Catches: a farm with no state store pretending to answer from one: it has no result
/// for any operation.
#[test]
fn a_farm_without_a_store_has_no_stored_results() {
    let farm = Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    assert!(
        farm.ended(kbf_types::OperationId(0))
            .expect("read")
            .is_none()
    );
}

/// Catches: operation names that repeat across starts that keep the epoch (the hand-over
/// to a new process keeps it, so a daemon keeps its runs). The names carry the writer
/// generation, which every start takes: a client holding the first process's name for its
/// operation is told NOT_FOUND by the second, not handed whichever operation of the second
/// got the same number (issue #154).
#[tokio::test(start_paused = true)]
async fn a_kept_epoch_does_not_reuse_operation_names() {
    let dir = Scratch::new();
    let first = farm_over(store_from_flag(&dir.0).expect("a first start"));
    let epoch = first.lease_epoch();
    let name = first
        .submit(Submission {
            request: request(1),
            call: None,
            described: Described::default(),
        })
        .expect("submitted")
        .name;
    assert!(first.wait(&name).is_some());
    drop(first);

    let kept = StateStore {
        epoch: EpochPolicy::Keep,
        ..store_from_flag(&dir.0).expect("a second start")
    };
    let second = farm_over(kept);
    assert_eq!(second.lease_epoch(), epoch, "the epoch was kept");
    assert!(
        second.wait(&name).is_none(),
        "the first process's name names an operation of the second"
    );
}
