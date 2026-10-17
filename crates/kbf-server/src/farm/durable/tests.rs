//! The rows the scheduler writes, read back as a start reads them.

use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, Ordering};

use kbf_caps::NodeCaps;
use kbf_log::{Bootstrap, FarmLogId};
use kbf_sched::RequeueReason;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange, StoreTime,
    Versioned,
};
use kbf_types::{
    ActionKey, Answer, Digest, DigestFunction, Failure, FarmFault, FarmRun, FencePolicy, LeaseKind,
    LostWhy, MemoryKill, MemoryRun, Outcome, Qos, Refusal, RefusalRecord, Resources, Waiting,
};

use super::*;

/// A store whose writes can be made to fail.
struct Flaky {
    inner: Fake,
    down: AtomicBool,
}

impl Flaky {
    fn new() -> Arc<Self> {
        let inner = Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None);
        Arc::new(Self {
            inner,
            down: AtomicBool::new(false),
        })
    }

    fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }
}

impl Store for Flaky {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
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

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
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

fn request(tag: u8) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
        },
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
    }
}

fn start(operation: u64, seq: u64) -> StartLease {
    StartLease {
        worker: WorkerId::new("node-a"),
        lease: LeaseId::new(5, seq),
        operation: OperationId(operation),
        key: request(1).key,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        fence: FencePolicy::RunOn,
        device: None,
    }
}

fn answer(operation: u64, seq: u64) -> Answer {
    Answer {
        operation: OperationId(operation),
        lease: LeaseId::new(5, seq),
        waiters: Vec::new(),
        outcome: Outcome::Failed(Failure::Invalid),
        memory_runs: Vec::new(),
        farm_runs: Vec::new(),
    }
}

fn requeue(operation: u64, seq: u64) -> Requeue {
    Requeue {
        operation: OperationId(operation),
        lease: LeaseId::new(5, seq),
        worker: WorkerId::new("node-a"),
        reason: RequeueReason::Silent,
    }
}

fn open(store: &Arc<Flaky>) -> Durable {
    let store: Arc<dyn Store> = store.clone();
    Durable::open(store, EpochPolicy::Keep, 100)
        .expect("open")
        .0
}

fn keys(store: &Flaky) -> Vec<String> {
    let rows = store.scan_prefix("").expect("scan");
    rows.into_iter().map(|(key, _)| key).collect()
}

fn put(store: &Flaky, key: &str, value: Vec<u8>) {
    let put = Txn::new().put(key, value, Cond::Any);
    store.txn(put).expect("store").expect("put");
}

/// Catches: an epoch that is not kept across starts that keep it, not raised by one that
/// bumps it (a daemon would keep the runs of leases from before a restored file), a new
/// store that does not start from the seed it is given (two stores could both begin at
/// 1 and a daemon would take one for the other), and an epoch that is not durable.
#[test]
fn the_epoch_is_seeded_kept_and_bumped() {
    let store = Flaky::new();
    let dyn_store = || -> Arc<dyn Store> { store.clone() };
    let epoch = |policy, seed| Durable::open(dyn_store(), policy, seed).expect("open").1;
    assert_eq!(epoch(EpochPolicy::Keep, 100), 100);
    assert_eq!(
        epoch(EpochPolicy::Keep, 5),
        100,
        "a kept epoch is the store's"
    );
    assert_eq!(epoch(EpochPolicy::Bump, 5), 101);
    assert_eq!(epoch(EpochPolicy::Keep, 5), 101, "a bump is durable");
    assert_eq!(epoch(EpochPolicy::Bump, 5), 102);
}

/// Catches: an operation not written at submit (a restart forgets work the client was
/// told is queued), its place in the queue or its next id not kept, and a second write
/// of the same operation accepted over the first.
#[test]
fn a_submitted_operation_is_loaded_queued() {
    let store = Flaky::new();
    let durable = open(&store);
    durable
        .submitted(OperationId(4), &request(1))
        .expect("submit");
    let stored = durable.load().expect("load");
    assert_eq!(
        stored,
        Stored {
            next_operation: 5,
            next_seq: 0,
            operations: vec![StoredOperation {
                operation: OperationId(4),
                request: request(1),
                lease: None,
                losses: 0,
                fate: None,
            }],
            cordons: Vec::new(),
        }
    );
    let again = durable.submitted(OperationId(4), &request(2));
    assert!(matches!(again, Err(StateError::Conflict(_))), "{again:?}");
}

/// Catches: a grant whose lease is not in the store (a restart forgets the claim a
/// daemon was told), whose operation stays queued, or whose lease sequence is not
/// kept (a restart would grant the lease id again: a stale daemon's result taken as
/// the new lease's).
#[test]
fn a_grant_holds_the_lease_and_leaves_the_queue() {
    let store = Flaky::new();
    let durable = open(&store);
    durable
        .submitted(OperationId(4), &request(1))
        .expect("submit");
    durable
        .persist(&[Effect::Start(start(4, 7))])
        .expect("grant");
    let stored = durable.load().expect("load");
    assert_eq!(stored.next_seq, 8);
    let [held] = stored.operations.as_slice() else {
        panic!("{stored:?}");
    };
    let grant = held.lease.as_ref().expect("a lease");
    assert_eq!(grant.lease, LeaseId::new(5, 7));
    assert_eq!(grant.worker, WorkerId::new("node-a"));
    assert!(!keys(&store).iter().any(|k| k.starts_with("queue/")));
}

/// Catches: a second live lease for one operation (two daemons run it and either
/// result is accepted): a grant needs the operation queued, which the first grant ended.
#[test]
fn an_operation_never_holds_two_leases() {
    let store = Flaky::new();
    let durable = open(&store);
    durable
        .submitted(OperationId(4), &request(1))
        .expect("submit");
    durable
        .persist(&[Effect::Start(start(4, 0))])
        .expect("grant");
    let second = durable.persist(&[Effect::Start(start(4, 1))]);
    assert!(matches!(second, Err(StateError::Missing(_))), "{second:?}");
    let stored = durable.load().expect("load");
    let lease = stored.operations[0].lease.as_ref().map(|g| g.lease);
    assert_eq!(lease, Some(LeaseId::new(5, 0)));
}

/// Catches: a result accepted from a lease the operation no longer holds: the store must
/// compare-and-set on the operation's lease, so a stale `complete` (the first lease's,
/// given up and granted again) is refused and the operation keeps its second lease.
#[test]
fn a_stale_complete_is_rejected() {
    let store = Flaky::new();
    let durable = open(&store);
    durable
        .submitted(OperationId(4), &request(1))
        .expect("submit");
    durable
        .persist(&[Effect::Start(start(4, 0))])
        .expect("first grant");
    durable
        .requeued(&requeue(4, 0), None, None)
        .expect("given up");
    durable
        .persist(&[Effect::Start(start(4, 1))])
        .expect("second grant");

    let stale = durable.persist(&[Effect::Answer(answer(4, 0))]);
    assert!(matches!(stale, Err(StateError::Missing(_))), "{stale:?}");
    let stored = durable.load().expect("load");
    let held = stored.operations[0].lease.as_ref().map(|g| g.lease);
    assert_eq!(
        held,
        Some(LeaseId::new(5, 1)),
        "the stale result moved the operation"
    );

    durable
        .persist(&[Effect::Answer(answer(4, 1))])
        .expect("the current lease's result");
    let stored = durable.load().expect("load");
    assert!(stored.operations.is_empty());
    assert_eq!(
        stored.next_seq, 2,
        "a finished operation's ids are not reused"
    );
    assert_eq!(
        keys(&store),
        ["meta/epoch", "meta/next_op", "meta/next_seq"]
    );
}

/// Catches: a lease given up that stays in the store (a restart re-binds a run nobody
/// holds), an operation not queued again after it, and a give-up that cannot be written
/// again: the farm repeats a refused one until the store takes it, so a repeat after
/// it was taken changes nothing, and one for an operation that finished writes nothing.
#[test]
fn a_given_up_lease_requeues_its_operation_once() {
    let store = Flaky::new();
    let durable = open(&store);
    durable
        .submitted(OperationId(4), &request(1))
        .expect("submit");
    durable
        .persist(&[Effect::Start(start(4, 0))])
        .expect("grant");
    durable
        .requeued(&requeue(4, 0), None, None)
        .expect("given up");
    let stored = durable.load().expect("load");
    assert_eq!(stored.operations[0].lease, None);
    assert!(keys(&store).contains(&"queue/0000000000000004".to_owned()));
    durable
        .requeued(&requeue(4, 0), None, None)
        .expect("again, nothing changes");
    let again = durable.load().expect("load");
    assert_eq!(again, stored);
    durable
        .persist(&[Effect::Refuse(Refusal {
            operation: OperationId(4),
            waiters: Vec::new(),
            reason: String::new(),
        })])
        .expect("refused");
    durable
        .requeued(&requeue(4, 0), None, None)
        .expect("a finished operation has nothing to put back");
    assert!(!keys(&store).iter().any(|k| k.starts_with("queue/")));
}

/// Catches: a refused operation left in the store (it would be refused, or run, again
/// after a restart), and effects that settle nothing (a wait notice, a proposed record)
/// writing anything.
#[test]
fn a_refusal_removes_the_operation_and_other_effects_write_nothing() {
    let store = Flaky::new();
    let durable = open(&store);
    durable
        .submitted(OperationId(4), &request(1))
        .expect("submit");
    let before = store.commit_stats().txns;
    let unsettled = [
        Effect::Waiting(Waiting {
            operation: OperationId(4),
            reason: None,
        }),
        Effect::Commit(ControlRecord::Refusal(RefusalRecord {
            operation: OperationId(4),
            reason: "no worker".to_owned(),
        })),
    ];
    durable.persist(&unsettled).expect("nothing to write");
    assert_eq!(store.commit_stats().txns, before);
    assert_eq!(durable.load().expect("load").operations.len(), 1);

    let refusal = Refusal {
        operation: OperationId(4),
        waiters: Vec::new(),
        reason: "no worker".to_owned(),
    };
    durable
        .persist(&[Effect::Refuse(refusal)])
        .expect("refused");
    assert!(durable.load().expect("load").operations.is_empty());
    assert_eq!(keys(&store), ["meta/epoch", "meta/next_op"]);
}

/// Catches: a loss count that is not written with the give-up that made it (a restart
/// forgets the losses and a poison action loops), one that a later give-up that is not a
/// loss erases, and one that outlives its operation (a row nobody removes).
#[test]
fn a_loss_count_lives_with_its_operation() {
    let store = Flaky::new();
    let durable = open(&store);
    durable
        .submitted(OperationId(4), &request(1))
        .expect("submit");
    durable
        .persist(&[Effect::Start(start(4, 0))])
        .expect("grant");
    durable
        .requeued(&requeue(4, 0), Some(2), None)
        .expect("a lost lease");
    assert_eq!(durable.load().expect("load").operations[0].losses, 2);
    durable
        .persist(&[Effect::Start(start(4, 1))])
        .expect("grant");
    durable
        .requeued(&requeue(4, 1), None, None)
        .expect("not a lost lease");
    assert_eq!(durable.load().expect("load").operations[0].losses, 2);
    durable
        .persist(&[Effect::Start(start(4, 2))])
        .expect("grant");
    durable
        .persist(&[Effect::Answer(answer(4, 2))])
        .expect("finished");
    assert!(!keys(&store).iter().any(|k| k.starts_with("loss/")));
}

/// Catches: a refused operation whose loss count stays (a row nobody removes).
#[test]
fn a_refusal_removes_the_loss_count() {
    let store = Flaky::new();
    let durable = open(&store);
    durable
        .submitted(OperationId(4), &request(1))
        .expect("submit");
    durable
        .persist(&[Effect::Start(start(4, 0))])
        .expect("grant");
    durable
        .requeued(&requeue(4, 0), Some(3), None)
        .expect("a lost lease");
    let refusal = Refusal {
        operation: OperationId(4),
        waiters: Vec::new(),
        reason: "lost".to_owned(),
    };
    durable
        .persist(&[Effect::Refuse(refusal)])
        .expect("refused");
    assert!(!keys(&store).iter().any(|k| k.starts_with("loss/")));
}

/// Catches: a session row that never expires (a daemon that left would look live for
/// ever), a renewal that does not extend it, one that writes a row for a daemon with
/// none, and a probe write that does not reach the store.
#[test]
fn a_session_row_expires_unless_renewed() {
    let store = Flaky::new();
    let durable = open(&store);
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).expect("caps");
    let reported = Reported::serving(Resources::new(2_000, 1 << 30), caps);
    let worker = WorkerId::new("node-a");
    let before = store.commit_stats().txns;
    durable.renew(&worker).expect("no row, nothing to renew");
    assert_eq!(store.commit_stats().txns, before);

    durable
        .session(&worker, &DaemonInstance::new("daemon-1"), false, &reported)
        .expect("session");
    let first = store.get("session/node-a").expect("get").expect("a row");
    assert!(first.expires_at.is_some());
    durable.renew(&worker).expect("renewed");
    let second = store.get("session/node-a").expect("get").expect("a row");
    assert_eq!(second.value, first.value);
    assert!(second.version > first.version);
    assert!(second.expires_at > first.expires_at);

    store.set_down(true);
    assert!(matches!(durable.ping(), Err(StateError::Store(_))));
    store.set_down(false);
    durable.ping().expect("up again");
    assert!(store.get("meta/alive").expect("get").is_some());
}

/// Catches: a registration that leaves no session row, or one a start could not read
/// back as what the scheduler was told (instance, capacity).
#[test]
fn a_registration_writes_the_session_row() {
    let store = Flaky::new();
    let durable = open(&store);
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")]).expect("caps");
    let reported = Reported::serving(Resources::new(2_000, 1 << 30), caps);
    let worker = WorkerId::new("node-a");
    let instance = DaemonInstance::new("daemon-1");
    durable
        .session(&worker, &instance, true, &reported)
        .expect("session");
    let row = store.get("session/node-a").expect("get").expect("a row");
    let FarmInput::WorkerUp {
        instance: stored,
        capacity,
        predecessor_stopped,
        ..
    } = decode("session/node-a", &row.value).expect("decode")
    else {
        panic!("not a registration");
    };
    assert_eq!(stored, instance);
    assert_eq!(capacity, Resources::new(2_000, 1 << 30));
    assert!(predecessor_stopped);
}

/// Catches: a row the scheduler cannot read skipped (work silently lost at a restart)
/// instead of refused, and a refusal that does not name the row.
#[test]
fn an_unreadable_row_is_refused_naming_it() {
    let lease = encode(FarmInput::Committed(ControlRecord::Lease(LeaseGrant {
        lease: LeaseId::new(5, 0),
        operation: OperationId(1),
        worker: WorkerId::new("node-a"),
    })))
    .expect("encode");
    let submit = encode(FarmInput::Submit(request(1))).expect("encode");
    let other = kbf_log::encode(
        Format::V1,
        &LogCommand::Bootstrap(Bootstrap {
            log_id: FarmLogId::new(NonZeroU64::new(1).expect("non-zero")),
            store_prefix: "kbf/".to_owned(),
        }),
    )
    .expect("encode");
    let pair = LogCommand::Farm(FarmBatch {
        now: FarmTime::default(),
        wall: UNIX_EPOCH,
        inputs: vec![FarmInput::Tick, FarmInput::Tick],
    });
    let pair = kbf_log::encode(Format::V1, &pair).expect("encode");
    let big = u64::MAX.to_be_bytes().to_vec();
    let rows: [(&str, Vec<u8>, &str); 11] = [
        ("op/0000000000000001", vec![0xff, 0xff, 0xff], "malformed"),
        ("op/0000000000000001", lease.clone(), "not a request"),
        ("op/0000000000000001", other, "not a single scheduler input"),
        ("op/0000000000000001", pair, "not a single scheduler input"),
        ("op/zz", submit.clone(), "not an operation id"),
        (
            "lease/0000000000000005.0000000000000000",
            submit,
            "not a lease grant",
        ),
        ("loss/zz", vec![0; 8], "not an operation id"),
        ("loss/0000000000000001", vec![1, 2], "not an 8-byte number"),
        ("loss/0000000000000001", big, "not a loss count"),
        ("meta/next_op", vec![1, 2, 3], "not an 8-byte number"),
        ("meta/next_seq", vec![1, 2, 3], "not an 8-byte number"),
    ];
    for (key, value, why) in rows {
        let store = Flaky::new();
        let durable = open(&store);
        put(&store, key, value);
        let error = durable.load().expect_err(key);
        let text = error.to_string();
        assert!(text.contains(key) && text.contains(why), "{key}: {text}");
    }
    let store = Flaky::new();
    put(&store, "meta/epoch", vec![1]);
    let dyn_store: Arc<dyn Store> = store.clone();
    let opened = Durable::open(dyn_store, EpochPolicy::Keep, 1);
    assert!(matches!(opened, Err(StateError::Corrupt { .. })));
}

/// Catches: a store failure turned into success (an acknowledgement for a write that is
/// not durable), at open, at read and at write.
#[test]
fn a_failing_store_fails_the_write() {
    let store = Flaky::new();
    let durable = open(&store);
    store.set_down(true);
    let submit = durable.submitted(OperationId(1), &request(1));
    assert!(matches!(submit, Err(StateError::Store(_))), "{submit:?}");
    let dyn_store: Arc<dyn Store> = store.clone();
    let opened = Durable::open(dyn_store, EpochPolicy::Bump, 1);
    assert!(matches!(opened, Err(StateError::Store(_))));
    store.set_down(false);
    durable
        .submitted(OperationId(1), &request(1))
        .expect("up again");
}

/// Catches: a store wrapper that answers reads and the feed from somewhere but the fake
/// it wraps (the tests above would pass over an empty store), and a handle that does not
/// say what it is when a farm holding it is printed.
#[test]
fn the_flaky_store_reads_through_and_the_handle_prints() {
    let store = Flaky::new();
    let durable = open(&store);
    durable
        .submitted(OperationId(1), &request(1))
        .expect("submit");
    assert_eq!(store.reap_expired(), Ok(0));
    assert!(
        store.now() > StoreTime::ZERO,
        "the fake's clock pays for each sync"
    );
    let changes = store.changes(Revision::ZERO, 100).expect("changes");
    let written = changes
        .rows
        .iter()
        .any(|row| row.key == "op/0000000000000001");
    assert!(written);
    assert_eq!(format!("{durable:?}"), "Durable");
}

/// Catches: a result row that goes with the finished retention (a retention of zero would
/// drop it at once, before a stream kept open by an earlier process reads it), one that
/// outlives a long retention's end, and one nobody frees: a short retention keeps the row
/// for the floor, a long one for the retention, and the reaper frees it after.
#[test]
fn a_result_row_is_kept_for_the_longer_of_the_retention_and_the_floor() {
    let clock = Arc::new(ManualClock::default());
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(Arc::clone(&clock), Flaw::None));
    let durable = Durable::open(Arc::clone(&store), EpochPolicy::Keep, 100)
        .expect("open")
        .0;
    let finished = Finished::Failed(kbf_proto::google::rpc::Status {
        code: 3,
        message: "no".to_owned(),
        details: Vec::new(),
    });
    let short = OperationId(1);
    let long = OperationId(2);
    durable
        .done(short, &finished, Duration::from_secs(1))
        .expect("short");
    durable
        .done(long, &finished, Duration::from_secs(3600))
        .expect("long");
    let seconds = |n| Duration::from_secs(n);
    clock.advance(DONE_FLOOR - seconds(1));
    assert_eq!(durable.ended(short).expect("read"), Some(finished.clone()));
    clock.advance(seconds(2));
    assert_eq!(durable.ended(short).expect("read"), None, "past the floor");
    assert_eq!(durable.ended(long).expect("read"), Some(finished.clone()));
    assert_eq!(store.reap_expired().expect("reap"), 1, "only the short row");
    clock.advance(seconds(3600));
    assert_eq!(
        durable.ended(long).expect("read"),
        None,
        "past the retention"
    );
}

/// Catches: a `done/` row that is not a result read as one (a caller would be sent
/// whatever the bytes decode to), and one that names its row.
#[test]
fn a_result_row_that_is_not_a_result_is_refused_naming_it() {
    let store = Flaky::new();
    let durable = open(&store);
    put(&store, &done_key(OperationId(7)), vec![9, 9]);
    let refused = durable.ended(OperationId(7)).expect_err("junk");
    assert!(
        matches!(&refused, StateError::Corrupt { key, .. } if key == "done/0000000000000007"),
        "{refused:?}"
    );
}

/// Catches: a generation read from a row that is not there as 0 (a farm over an unfenced
/// store names its operations by the epoch, as before) or one that is read wrong.
#[test]
fn the_generation_is_the_stored_number_or_none() {
    let store = Flaky::new();
    let durable = open(&store);
    assert_eq!(durable.generation().expect("read"), None);
    put(&store, GENERATION, 42u64.to_be_bytes().to_vec());
    assert_eq!(durable.generation().expect("read"), Some(42));
}

/// Catches: an operator's cordon or drain not kept (a successor process would offer
/// leases to a node the operator took out of placement), a drain that loses its deadline,
/// a node returned to placement whose row stays (it would come back cordoned at every
/// start), and a row that is not a cordon taken for one. Each row is read back as it was
/// written, in node-id order.
#[test]
fn a_cordon_is_kept_until_the_node_returns_to_placement() {
    let store = Flaky::new();
    let durable = open(&store);
    let (a, b) = (WorkerId::new("node-a"), WorkerId::new("node-b"));
    durable
        .cordoned(
            &b,
            Some(StoredCordon::Draining {
                deadline_unix_ms: 1_234_567,
            }),
        )
        .expect("drain");
    durable
        .cordoned(&a, Some(StoredCordon::Cordoned))
        .expect("cordon");
    assert_eq!(
        durable.load().expect("load").cordons,
        [
            (a.clone(), StoredCordon::Cordoned),
            (
                b,
                StoredCordon::Draining {
                    deadline_unix_ms: 1_234_567
                }
            ),
        ]
    );
    durable.cordoned(&a, None).expect("uncordon");
    durable
        .cordoned(&a, None)
        .expect("uncordoning twice changes nothing");
    let left: Vec<String> = keys(&store)
        .into_iter()
        .filter(|k| k.starts_with("cordon/"))
        .collect();
    assert_eq!(left, ["cordon/node-b"]);

    for (row, why) in [
        (vec![], "not a cordon"),
        (vec![2], "not a cordon"),
        (vec![0, 0], "not a cordon"),
        (vec![1, 1, 2], "a drain deadline is not 8 bytes"),
    ] {
        put(&store, "cordon/node-c", row);
        let error = durable.load().expect_err("a corrupt row").to_string();
        assert!(error.contains(why), "{error}");
    }
}

/// Catches: a follower that cannot tell the four states of an operation in the store
/// apart: ended (its `done/` row, whichever other rows are still there), waiting for a
/// lease, holding one, and no trace of it at all.
#[test]
fn an_operation_is_ended_queued_running_or_gone() {
    let store = Flaky::new();
    let durable = open(&store);
    let operation = OperationId(4);
    assert_eq!(
        durable.progress(operation).expect("progress"),
        Progress::Gone
    );
    assert!(Progress::Gone.is_gone());
    assert!(!Progress::Queued.is_gone());
    durable.submitted(operation, &request(1)).expect("submit");
    assert_eq!(
        durable.progress(operation).expect("progress"),
        Progress::Queued
    );
    durable
        .persist(&[Effect::Start(start(4, 0))])
        .expect("grant");
    assert_eq!(
        durable.progress(operation).expect("progress"),
        Progress::Running
    );
    let finished = Finished::Failed(kbf_proto::google::rpc::Status {
        code: 3,
        message: "invalid".to_owned(),
        details: Vec::new(),
    });
    durable
        .done(operation, &finished, Duration::from_secs(1))
        .expect("done");
    assert_eq!(
        durable.progress(operation).expect("progress"),
        Progress::Ended(finished),
        "the result wins over the rows of the operation that are still there"
    );
}

fn killed(operation: u64, seq: u64) -> Requeue {
    Requeue {
        reason: RequeueReason::OutOfMemory {
            booked: 4 << 30,
            raised: 16 << 30,
        },
        ..requeue(operation, seq)
    }
}

fn a_fate() -> Fate {
    Fate {
        booked_bytes: 16 << 30,
        memory_runs: vec![
            MemoryRun {
                worker: WorkerId::new("node-a"),
                booked: 4 << 30,
                kill: MemoryKill::OwnLimit { cap: 64 << 30 },
            },
            MemoryRun {
                worker: WorkerId::new("node-b"),
                booked: 16 << 30,
                kill: MemoryKill::NodePressure,
            },
        ],
        farm_runs: vec![
            FarmRun {
                worker: WorkerId::new("node-a"),
                fault: FarmFault::NodeMemoryPressure,
            },
            FarmRun {
                worker: WorkerId::new("node-b"),
                fault: FarmFault::Lost(LostWhy::DaemonRestarted),
            },
            FarmRun {
                worker: WorkerId::new("node-c"),
                fault: FarmFault::Lost(LostWhy::Fenced),
            },
            FarmRun {
                worker: WorkerId::new("node-c"),
                fault: FarmFault::Infra,
            },
        ],
        avoid: ["node-a", "node-b", "node-c"]
            .map(WorkerId::new)
            .into_iter()
            .collect(),
        not_started: 3,
    }
}

/// Catches: a fate that is written apart from the queue row (a crash between them leaves
/// a queued operation at its old booking, or a fate for an operation not queued), one the
/// load does not return with its operation, and one that outlives its operation's end by
/// a finish or by a refusal.
#[test]
fn a_fate_lives_and_dies_with_its_operation() {
    let store = Flaky::new();
    let durable = open(&store);
    for operation in [4, 5] {
        durable
            .submitted(OperationId(operation), &request(1 + operation as u8))
            .expect("submit");
        durable
            .persist(&[Effect::Start(start(operation, 0))])
            .expect("grant");
        durable
            .requeued(&killed(operation, 0), None, Some(&a_fate()))
            .expect("a kill");
    }
    let stored = durable.load().expect("load");
    assert!(stored.operations.iter().all(|op| op.fate == Some(a_fate())));
    assert_eq!(
        keys(&store)
            .iter()
            .filter(|k| k.starts_with("fate/"))
            .count(),
        2
    );

    // Finished by a result, and refused: neither leaves the row.
    durable
        .persist(&[Effect::Start(start(4, 1))])
        .expect("grant");
    durable
        .persist(&[Effect::Answer(answer(4, 1))])
        .expect("finished");
    durable
        .persist(&[Effect::Refuse(Refusal {
            operation: OperationId(5),
            waiters: Vec::new(),
            reason: "no worker".to_owned(),
        })])
        .expect("refused");
    assert_eq!(
        keys(&store)
            .iter()
            .filter(|k| k.starts_with("fate/"))
            .count(),
        0
    );
}

/// Catches: a fate row read as anything but what was written: another version, a row cut
/// short, bytes after the last field, an unknown kind of kill, of farm fault or of loss, a
/// worker name that is not text. The farm refuses to start over such a store rather than
/// book a guess.
#[test]
fn a_fate_row_that_is_not_one_is_refused() {
    let row = fate::encode(&a_fate()).expect("encodes");
    assert_eq!(fate::decode(&row), Ok(a_fate()));
    let mut other = row.clone();
    other[0] = 3;
    let mut longer = row.clone();
    longer.push(0);
    // Two runs, a worker's name and the kind of its kill, in a row of memory runs only.
    let two_kills = Fate {
        farm_runs: Vec::new(),
        avoid: std::collections::BTreeSet::new(),
        not_started: 0,
        ..a_fate()
    };
    let mut kind = fate::encode(&two_kills).expect("encodes");
    // Version 2 ends with the three empty budgets: two counts (4 bytes each) and a number
    // (4); before them, the last memory run's kind.
    let at = kind.len() - 12 - 1;
    kind[at] = 9;
    let mut rows = vec![
        (other, "not a version 1 or 2 fate"),
        (row[..row.len() - 1].to_vec(), "the row ends early"),
        (longer, "bytes after the last field"),
        (kind, "not a kind of memory kill"),
        (Vec::new(), "the row ends early"),
    ];
    let mut bad = |fate: &Fate, change: &dyn Fn(&mut Vec<u8>), why| {
        let mut row = fate::encode(fate).expect("encodes");
        change(&mut row);
        rows.push((row, why));
    };
    let one_run = |fault| Fate {
        booked_bytes: 1,
        farm_runs: vec![FarmRun {
            worker: WorkerId::new("n"),
            fault,
        }],
        ..Fate::default()
    };
    // [version, booked 8, memory runs 4, farm runs 4, name 2+1, kind, (reason), avoid 4, not started 4]
    let kind_at = 1 + 8 + 4 + 4 + 3;
    bad(
        &one_run(FarmFault::Infra),
        &|r| r[kind_at] = 9,
        "not a kind of farm fault",
    );
    bad(
        &one_run(FarmFault::Lost(LostWhy::Fenced)),
        &|r| r[kind_at + 1] = 9,
        "not a reason the farm lost a run",
    );
    let named = Fate {
        avoid: [WorkerId::new("n")].into_iter().collect(),
        ..Fate::default()
    };
    // The avoided worker's name is the byte after its 2-byte length.
    let name_at = 1 + 8 + 4 + 4 + 4 + 2;
    bad(
        &Fate {
            booked_bytes: 1,
            ..named
        },
        &|r| r[name_at] = 0xff,
        "a worker name is not text",
    );
    for (bad, why) in rows {
        assert_eq!(fate::decode(&bad), Err(why));
    }
}

/// Catches: a row written before the budgets were (version 1: the booking and the memory
/// runs only) refused, or read with budgets it never held. It loads with every budget
/// empty.
#[test]
fn a_version_1_fate_row_still_loads() {
    let mut row = vec![1];
    row.extend((16u64 << 30).to_be_bytes());
    row.extend(1u32.to_be_bytes());
    row.extend(6u16.to_be_bytes());
    row.extend(b"node-a");
    row.extend((4u64 << 30).to_be_bytes());
    row.push(0);
    assert_eq!(
        fate::decode(&row),
        Ok(Fate {
            booked_bytes: 16 << 30,
            memory_runs: vec![MemoryRun {
                worker: WorkerId::new("node-a"),
                booked: 4 << 30,
                kill: MemoryKill::NodePressure,
            }],
            ..Fate::default()
        })
    );
}

/// Catches: a fate row with a worker name that is not text accepted, a fate too large to
/// write encoded anyway (a truncated length), a requeue whose fate cannot be written
/// reported as success, and a load that skips a fate row it cannot read instead of
/// refusing the store.
#[test]
fn a_fate_that_cannot_be_written_or_read_is_refused() {
    let mut row = fate::encode(&a_fate()).expect("encodes");
    // Version, booking, count, then the first name's length (2): its first byte.
    row[1 + 8 + 4 + 2] = 0xff;
    assert_eq!(fate::decode(&row), Err("a worker name is not text"));

    let mut huge = a_fate();
    huge.memory_runs[0].worker = WorkerId::new("n".repeat(usize::from(u16::MAX) + 1));
    assert_eq!(fate::encode(&huge), None);

    let store = Flaky::new();
    let durable = open(&store);
    durable
        .submitted(OperationId(4), &request(5))
        .expect("submit");
    durable
        .persist(&[Effect::Start(start(4, 0))])
        .expect("grant");
    let error = durable
        .requeued(&killed(4, 0), None, Some(&huge))
        .expect_err("too large");
    assert!(error.to_string().contains("fate/0000000000000004"));
    assert!(
        !keys(&store).iter().any(|k| k.starts_with("queue/")),
        "nothing of the requeue landed"
    );

    put(&store, "fate/0000000000000004", vec![3]);
    let error = durable.load().expect_err("a fate of another version");
    assert!(error.to_string().contains("not a version 1 or 2 fate"));
}
