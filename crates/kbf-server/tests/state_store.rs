//! The farm over a state store: what it writes before it acknowledges, and what a farm
//! started over the same store takes up again.
//!
//! The store is the in-memory fake, which keeps a durable copy a restart reopens, and
//! the farm's clock is tokio's, paused: nothing here waits.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_front::{Cache, Described, Dispatch, Finished, MemoryMetaLog, Stage, Submission};
use kbf_objstore::MemoryStore;
use kbf_proto::google::rpc;
use kbf_proto::worker::{self, ServerMessage, server_message};
use kbf_sched::{DaemonInstance, Request};
use kbf_server::farm::{Reported, StreamId};
use kbf_server::{EpochPolicy, Farm, Listeners, ServeError, StateError, StateStore};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, Store, StoreError, StoreTime,
    Txn, Versioned,
};
use kbf_types::{ActionKey, Digest, DigestFunction, LeaseId, LeaseKind, Qos, Resources, WorkerId};
use tokio::sync::mpsc;
use tonic::{Code, Status};

const GIB: u64 = 1 << 30;

type TestFarm = Farm<MemoryMetaLog, MemoryStore>;
type Inbox = mpsc::UnboundedReceiver<Result<ServerMessage, Status>>;

/// A store whose writes can be made to fail, over the fake (which a restart reopens from
/// what was synced).
struct Flaky {
    inner: Fake,
    down: AtomicBool,
    /// How many more writes take before the store goes down; negative for no limit.
    allow: AtomicI64,
}

impl Flaky {
    fn new() -> Self {
        let inner = Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None);
        Self {
            inner,
            down: AtomicBool::new(false),
            allow: AtomicI64::new(-1),
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
        Self {
            inner: self.inner.restart(),
            down: AtomicBool::new(false),
            allow: AtomicI64::new(-1),
        }
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
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.inner.scan_prefix(prefix)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}

/// The store, and farms started over it one after another.
struct Disk {
    store: Arc<Flaky>,
}

impl Disk {
    fn new() -> Self {
        Self {
            store: Arc::new(Flaky::new()),
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
        .with_state(Some(StateStore { store, epoch }))
    }

    /// The process dies: its farm is gone, and the store reopens as the disk left it.
    fn crash(&mut self, farm: TestFarm) {
        drop(farm);
        let store = Arc::into_inner(std::mem::replace(&mut self.store, Arc::new(Flaky::new())));
        self.store = Arc::new(store.expect("nothing else holds the store").restart());
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
#[tokio::test(start_paused = true)]
async fn a_restart_replays_the_claims_it_acknowledged() {
    let mut disk = Disk::new();
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
#[tokio::test(start_paused = true)]
async fn after_a_kill_running_leases_are_rebound_and_undelivered_ones_requeued() {
    let mut disk = Disk::new();
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
#[tokio::test(start_paused = true)]
async fn the_epoch_follows_the_policy_and_names_the_leases() {
    let mut disk = Disk::new();
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

/// Catches: a lease given up while the store was down that the store is never told of.
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
#[tokio::test(start_paused = true)]
async fn a_bumped_epoch_requeues_the_old_term_leases_the_daemon_killed() {
    let mut disk = Disk::new();
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
#[tokio::test(start_paused = true)]
async fn lost_leases_are_counted_across_a_restart_and_the_third_is_refused() {
    let mut disk = Disk::new();
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
    let refused = disk.try_farm(EpochPolicy::Keep).err().expect("refused");
    assert!(matches!(refused, StateError::Corrupt { .. }), "{refused:?}");
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
        timing: kbf_server::StreamTiming::default(),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        streams: kbf_server::StreamLimits::DEFAULT,
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
