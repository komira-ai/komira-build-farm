//! The rollout store and driver: the record is written before anything moves, nodes
//! go out no faster than the strategy allows, and every gate that fails holds.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_front::Cache;
use kbf_proto::worker::ServerMessage;
use kbf_server::Farm;
use kbf_server::farm::NodeAction;
use kbf_server::fleet::PlacementView;
use kbf_server::rollout::{
    Applier, Change, DriveError, Fleet, MemoryRolloutStore, NodePlacement, RolloutDriver,
    RolloutStore, StoreError, Update,
};
use kbf_types::{
    Actor, IllegalStep, NodeStep, Resources, Rollout, RolloutId, RolloutState, Selector, Strategy,
    WorkerId,
};

fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

fn rollout(nodes: &[&str], max_unavailable: u32) -> Rollout {
    let strategy = Strategy {
        max_unavailable,
        drain_deadline: Duration::from_secs(600),
        ..Strategy::default()
    };
    let names: Vec<WorkerId> = nodes.iter().map(|n| w(n)).collect();
    Rollout::new(
        RolloutId(1),
        "sha256:set",
        Selector::Nodes(names.clone()),
        strategy,
        Actor::new("operator", 1_000),
        names,
    )
    .expect("a valid strategy")
}

/// A fleet that records what it is asked and answers placements a test sets: each
/// read takes the next answer of a node's script, and the last one stays.
#[derive(Default)]
struct FakeFleet {
    actions: RefCell<Vec<(WorkerId, NodeAction)>>,
    placements: RefCell<BTreeMap<WorkerId, Vec<NodePlacement>>>,
    refuse: RefCell<Option<WorkerId>>,
}

fn connected(placement: PlacementView) -> NodePlacement {
    NodePlacement {
        placement,
        connected: true,
    }
}

fn away(placement: PlacementView) -> NodePlacement {
    NodePlacement {
        placement,
        connected: false,
    }
}

impl FakeFleet {
    /// `node` is at `placement` and connected from now on.
    fn set(&self, node: &str, placement: PlacementView) {
        self.script(node, vec![connected(placement)]);
    }

    /// `node`'s next reads answer `answers` in order; the last one stays.
    fn script(&self, node: &str, answers: Vec<NodePlacement>) {
        self.placements.borrow_mut().insert(w(node), answers);
    }

    fn actions(&self) -> Vec<(WorkerId, NodeAction)> {
        self.actions.borrow().clone()
    }
}

impl Fleet for FakeFleet {
    fn place(&self, node: &WorkerId, action: NodeAction) -> Result<(), String> {
        if self.refuse.borrow().as_ref() == Some(node) {
            return Err(format!("no node {node}"));
        }
        self.actions.borrow_mut().push((node.clone(), action));
        Ok(())
    }

    fn placement(&self, node: &WorkerId) -> Option<NodePlacement> {
        let mut placements = self.placements.borrow_mut();
        let answers = placements.get_mut(node)?;
        if answers.len() > 1 {
            Some(answers.remove(0))
        } else {
            answers.first().cloned()
        }
    }
}

#[derive(Default)]
struct FakeApplier {
    handed: RefCell<Vec<Update>>,
    refuse: bool,
}

impl Applier for FakeApplier {
    fn apply(&self, update: &Update) -> Result<(), String> {
        if self.refuse {
            return Err("the updater refused the set".to_owned());
        }
        self.handed.borrow_mut().push(update.clone());
        Ok(())
    }
}

/// The update `node` is handed in rollout `id`.
fn update(id: u64, node: &str) -> Update {
    Update {
        rollout: RolloutId(id),
        step: NodeStep::Updating,
        node: w(node),
        set_digest: "sha256:set".to_owned(),
    }
}

fn draining() -> PlacementView {
    PlacementView::Draining {
        deadline_unix_ms: 1,
        leases: vec!["1.0".to_owned()],
    }
}

fn steps(r: &Rollout) -> Vec<NodeStep> {
    r.nodes().values().map(|p| p.step()).collect()
}

/// Catches: a driver that takes more nodes out than `max_unavailable` (or counts a node
/// that is updating as back in service), one that drains without the strategy's
/// deadline, updates before the node is drained, or hands over another target, or an
/// update that does not name its rollout and step (section 4.2, step 5).
#[test]
fn nodes_go_out_one_at_a_time_and_stop_at_updating() {
    let (store, fleet, applier) = (
        MemoryRolloutStore::default(),
        FakeFleet::default(),
        FakeApplier::default(),
    );
    let driver = RolloutDriver::new(&store, &fleet, &applier);
    let id = driver
        .start(rollout(&["a", "b", "c"], 1))
        .expect("start")
        .id;

    let r = driver.step(id).expect("step");
    assert_eq!(
        steps(&r),
        [NodeStep::Cordoned, NodeStep::Pending, NodeStep::Pending]
    );
    assert_eq!(fleet.actions(), [(w("a"), NodeAction::Cordon)]);

    let r = driver.step(id).expect("step");
    assert_eq!(steps(&r)[0], NodeStep::Draining);
    let drain = NodeAction::Drain(Duration::from_secs(600));
    assert_eq!(fleet.actions()[1], (w("a"), drain));

    fleet.set("a", draining());
    for _ in 0..3 {
        let r = driver.step(id).expect("step");
        assert_eq!(
            steps(&r),
            [NodeStep::Draining, NodeStep::Pending, NodeStep::Pending]
        );
    }
    assert!(applier.handed.borrow().is_empty(), "applied before drained");

    fleet.set("a", PlacementView::Drained);
    let r = driver.step(id).expect("step");
    assert_eq!(
        steps(&r),
        [NodeStep::Updating, NodeStep::Pending, NodeStep::Pending]
    );
    assert_eq!(*applier.handed.borrow(), [update(1, "a")]);
    // Updating is still out of service: the next node waits.
    let r = driver.step(id).expect("step");
    assert_eq!(steps(&r)[1], NodeStep::Pending);
    assert_eq!(fleet.actions().len(), 2);
    assert_eq!(r.state(), RolloutState::Running);

    // Two at once when the strategy allows two.
    let (store, fleet, applier) = (
        MemoryRolloutStore::default(),
        FakeFleet::default(),
        FakeApplier::default(),
    );
    let driver = RolloutDriver::new(&store, &fleet, &applier);
    let mut two = rollout(&["a", "b", "c"], 2);
    two.id = RolloutId(2);
    let r = driver
        .step(driver.start(two).expect("start").id)
        .expect("step");
    assert_eq!(
        steps(&r),
        [NodeStep::Cordoned, NodeStep::Cordoned, NodeStep::Pending]
    );
}

/// Catches: a drain that paused at its deadline treated as drained (work would be cut
/// off), a node someone uncordoned taken as still draining, a refused hand-over or a
/// fleet that refuses the cordon not holding, and a held rollout that moves on.
#[test]
fn a_failed_gate_holds_the_node_and_the_rollout() {
    let paused = PlacementView::DrainPaused {
        deadline_unix_ms: 1,
        leases: vec!["1.4".to_owned()],
    };
    for (gate, refuse_apply) in [
        (Some(connected(paused.clone())), false),
        (Some(away(paused)), false),
        (Some(connected(PlacementView::Serving)), false),
        (Some(connected(PlacementView::Cordoned)), false),
        (None, false),
        (Some(connected(PlacementView::Drained)), true),
    ] {
        let fleet = FakeFleet::default();
        let applier = FakeApplier {
            refuse: refuse_apply,
            ..FakeApplier::default()
        };
        let store = MemoryRolloutStore::default();
        let driver = RolloutDriver::new(&store, &fleet, &applier);
        let id = driver.start(rollout(&["a", "b"], 1)).expect("start").id;
        driver.step(id).expect("cordon");
        driver.step(id).expect("drain");
        if let Some(gate) = gate.clone() {
            fleet.script("a", vec![gate]);
        }
        let r = driver.step(id).expect("step");
        assert_eq!(r.state(), RolloutState::Held, "{gate:?}");
        let a = r.node(&w("a")).expect("a");
        assert_eq!(a.step(), NodeStep::Held, "{gate:?}");
        let expected_at = if refuse_apply {
            NodeStep::Updating
        } else {
            NodeStep::Draining
        };
        assert_eq!(a.held_at(), Some(expected_at));
        let actions = fleet.actions().len();
        let again = driver.step(id).expect("step");
        assert_eq!(again, r, "nothing proceeds by itself");
        assert_eq!(fleet.actions().len(), actions);
    }

    // The fleet refuses the cordon (the node is unknown) or the drain.
    for refused_at in [0, 1] {
        let (store, fleet, applier) = (
            MemoryRolloutStore::default(),
            FakeFleet::default(),
            FakeApplier::default(),
        );
        let driver = RolloutDriver::new(&store, &fleet, &applier);
        let id = driver.start(rollout(&["a"], 1)).expect("start").id;
        if refused_at == 1 {
            driver.step(id).expect("cordon");
        }
        *fleet.refuse.borrow_mut() = Some(w("a"));
        let r = driver.step(id).expect("step");
        assert_eq!(r.state(), RolloutState::Held);
        let a = r.node(&w("a")).expect("a");
        let at = [NodeStep::Cordoned, NodeStep::Draining][refused_at];
        assert_eq!((a.step(), a.held_at()), (NodeStep::Held, Some(at)));
    }
}

/// Catches: a drained node handed its update while disconnected (`drained` also means
/// it went away and its leases were requeued; the update needs its stream), a
/// disconnected drained node held instead of waited for, and the update handed on the
/// placement read before `updating` was recorded (an uncordon in between would let
/// work land on a node that is being updated).
#[test]
fn a_node_is_updated_only_while_drained_and_connected() {
    let (store, fleet, applier) = (
        MemoryRolloutStore::default(),
        FakeFleet::default(),
        FakeApplier::default(),
    );
    let driver = RolloutDriver::new(&store, &fleet, &applier);
    let id = driver.start(rollout(&["a", "b"], 1)).expect("start").id;
    driver.step(id).expect("cordon");
    driver.step(id).expect("drain");
    fleet.script("a", vec![away(PlacementView::Drained)]);
    for _ in 0..3 {
        let r = driver.step(id).expect("step");
        assert_eq!(r.state(), RolloutState::Running, "waited for, not held");
        assert_eq!(steps(&r), [NodeStep::Draining, NodeStep::Pending]);
    }
    assert!(applier.handed.borrow().is_empty(), "updated while away");
    // Back: updated.
    fleet.set("a", PlacementView::Drained);
    let r = driver.step(id).expect("step");
    assert_eq!(steps(&r), [NodeStep::Updating, NodeStep::Pending]);
    assert_eq!(*applier.handed.borrow(), [update(1, "a")]);

    // Drained and connected when read, but uncordoned (or gone) by the time `updating`
    // is recorded: held at updating, and never handed the update.
    for second in [
        connected(PlacementView::Serving),
        away(PlacementView::Drained),
    ] {
        let (store, fleet, applier) = (
            MemoryRolloutStore::default(),
            FakeFleet::default(),
            FakeApplier::default(),
        );
        let driver = RolloutDriver::new(&store, &fleet, &applier);
        let id = driver.start(rollout(&["a"], 1)).expect("start").id;
        driver.step(id).expect("cordon");
        driver.step(id).expect("drain");
        fleet.script("a", vec![connected(PlacementView::Drained), second.clone()]);
        let r = driver.step(id).expect("step");
        assert_eq!(r.state(), RolloutState::Held, "{second:?}");
        let a = r.node(&w("a")).expect("a");
        assert_eq!(
            (a.step(), a.held_at()),
            (NodeStep::Held, Some(NodeStep::Updating))
        );
        assert!(applier.handed.borrow().is_empty(), "{second:?}");
    }
}

/// Catches: the driver taking a third node out with `max_unavailable` 2 (it relies on
/// the record to refuse the cordon), or failing the step when the record refuses it
/// instead of leaving the node pending.
#[test]
fn the_driver_takes_nodes_out_only_while_the_record_allows() {
    let (store, fleet, applier) = (
        MemoryRolloutStore::default(),
        FakeFleet::default(),
        FakeApplier::default(),
    );
    let driver = RolloutDriver::new(&store, &fleet, &applier);
    let id = driver
        .start(rollout(&["a", "b", "c"], 2))
        .expect("start")
        .id;
    fleet.set("a", draining());
    fleet.set("b", draining());
    for _ in 0..3 {
        let r = driver.step(id).expect("step");
        assert_eq!(r.out_of_service(), 2);
        assert_eq!(steps(&r)[2], NodeStep::Pending);
    }
    let refused = store.update(id, &|r| r.advance(&w("c"), NodeStep::Cordoned));
    assert_eq!(
        refused,
        Err(StoreError::Illegal(IllegalStep::Unavailable {
            max_unavailable: 2
        }))
    );
}

/// A store whose first read returns a snapshot taken earlier, while its writes and
/// later reads go to the record: what a driver sees when another one stepped the
/// rollout between its read and its write.
struct StaleStore {
    inner: MemoryRolloutStore,
    snapshot: Mutex<Option<Rollout>>,
}

impl RolloutStore for StaleStore {
    fn create(&self, rollout: Rollout) -> Result<(), StoreError> {
        self.inner.create(rollout)
    }

    fn get(&self, id: RolloutId) -> Option<Rollout> {
        let stale = self.snapshot.lock().expect("lock").take();
        stale.or_else(|| self.inner.get(id))
    }

    fn update(&self, id: RolloutId, change: Change<'_>) -> Result<Rollout, StoreError> {
        self.inner.update(id, change)
    }
}

/// Catches: a step another driver already took turned into an error (one driver per
/// rollout is expected; a second one must stop quietly, not fail), and a driver that
/// acts on a node whose step the record refused.
#[test]
fn a_step_already_taken_elsewhere_stops_quietly() {
    let fleet = FakeFleet::default();
    fleet.set("a", PlacementView::Drained);
    let applier = FakeApplier::default();
    for (taken, stale) in [
        (&[NodeStep::Cordoned][..], &[][..]),
        (
            &[NodeStep::Cordoned, NodeStep::Draining],
            &[NodeStep::Cordoned],
        ),
        (
            &[NodeStep::Cordoned, NodeStep::Draining, NodeStep::Updating],
            &[NodeStep::Cordoned, NodeStep::Draining],
        ),
    ] {
        let mut snapshot = rollout(&["a"], 1);
        snapshot.set_state(RolloutState::Running).expect("start");
        let mut record = snapshot.clone();
        for &step in stale {
            snapshot.advance(&w("a"), step).expect("a legal step");
        }
        for &step in taken {
            record.advance(&w("a"), step).expect("a legal step");
        }
        let store = StaleStore {
            inner: MemoryRolloutStore::default(),
            snapshot: Mutex::new(Some(snapshot)),
        };
        store.create(record.clone()).expect("create");
        let driver = RolloutDriver::new(&store, &fleet, &applier);
        let got = driver.step(RolloutId(1)).expect("not an error");
        assert_eq!(got.node(&w("a")), record.node(&w("a")), "{taken:?}");
        assert_eq!(store.inner.get(RolloutId(1)), Some(record));
    }
    assert!(fleet.actions().is_empty(), "{:?}", fleet.actions());
    assert!(applier.handed.borrow().is_empty());
}

std::thread_local! {
    static LOG: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Writes log lines to this thread's buffer.
struct ThreadLog;

impl std::io::Write for ThreadLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        LOG.with(|log| log.borrow_mut().extend_from_slice(buf));
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Log lines written on this thread while `f` runs, at info and above. The subscriber
/// is global (installed once), so every callsite is enabled whichever test reaches
/// it first; each test thread reads only its own lines.
fn logged(f: impl FnOnce()) -> String {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(|| ThreadLog)
            .with_ansi(false)
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
    });
    LOG.with(|log| log.borrow_mut().clear());
    f();
    LOG.with(|log| String::from_utf8(log.borrow().clone()).expect("UTF-8 log"))
}

/// Catches: a hold another driver already made turned into an error (the quiet-stop
/// rule must cover holds too), a hold recorded twice, and a refused step or hold that
/// leaves no trace in the log (a stalled rollout could not be diagnosed).
#[test]
fn a_hold_or_step_already_taken_elsewhere_is_logged_and_not_an_error() {
    let fleet = FakeFleet::default();
    fleet.set(
        "a",
        PlacementView::DrainPaused {
            deadline_unix_ms: 1,
            leases: vec!["1.4".to_owned()],
        },
    );
    let applier = FakeApplier::default();
    // This driver read `a` draining; another one has since held it, and the rollout.
    let mut snapshot = rollout(&["a"], 1);
    snapshot.set_state(RolloutState::Running).expect("start");
    snapshot
        .advance(&w("a"), NodeStep::Cordoned)
        .expect("cordon");
    snapshot
        .advance(&w("a"), NodeStep::Draining)
        .expect("drain");
    let mut record = snapshot.clone();
    record
        .advance(&w("a"), NodeStep::Held)
        .expect("held elsewhere");
    record
        .set_state(RolloutState::Held)
        .expect("held elsewhere");
    let store = StaleStore {
        inner: MemoryRolloutStore::default(),
        snapshot: Mutex::new(Some(snapshot)),
    };
    store.create(record.clone()).expect("create");
    let driver = RolloutDriver::new(&store, &fleet, &applier);
    let mut got = None;
    let log = logged(|| got = Some(driver.step(RolloutId(1))));
    assert_eq!(got, Some(Ok(record.clone())), "{log}");
    assert_eq!(store.inner.get(RolloutId(1)), Some(record));
    for part in ["rollout-1", "node=a", "step=held", "rollout step not taken"] {
        assert!(log.contains(part), "{part} missing from {log}");
    }
    assert!(
        !log.contains("rollout held"),
        "a hold not made was logged: {log}"
    );

    // A refused step is logged the same way.
    let mut snapshot = rollout(&["a"], 1);
    snapshot.set_state(RolloutState::Running).expect("start");
    let mut record = snapshot.clone();
    record
        .advance(&w("a"), NodeStep::Cordoned)
        .expect("cordoned elsewhere");
    let store = StaleStore {
        inner: MemoryRolloutStore::default(),
        snapshot: Mutex::new(Some(snapshot)),
    };
    store.create(record).expect("create");
    let driver = RolloutDriver::new(&store, &fleet, &applier);
    let log = logged(|| {
        driver.step(RolloutId(1)).expect("not an error");
    });
    for part in [
        "rollout-1",
        "node=a",
        "step=cordoned",
        "refused=a node may not move",
    ] {
        assert!(log.contains(part), "{part} missing from {log}");
    }
}

/// A store that keeps nothing once `writes` is spent.
struct FailingStore {
    inner: MemoryRolloutStore,
    writes: Mutex<u32>,
}

impl RolloutStore for FailingStore {
    fn create(&self, rollout: Rollout) -> Result<(), StoreError> {
        self.inner.create(rollout)
    }

    fn get(&self, id: RolloutId) -> Option<Rollout> {
        self.inner.get(id)
    }

    fn update(&self, id: RolloutId, change: Change<'_>) -> Result<Rollout, StoreError> {
        let mut left = self.writes.lock().expect("lock");
        if *left == 0 {
            return Err(StoreError::Unavailable("disk full".to_owned()));
        }
        *left -= 1;
        self.inner.update(id, change)
    }
}

/// Catches: a driver that acts on a node before the step is recorded (a restart would
/// find a cordoned or updating node the record does not know of, section 4.1).
#[test]
fn nothing_moves_unless_the_step_is_recorded_first() {
    for (writes, done) in [(1, 0), (2, 1), (3, 2)] {
        let fleet = FakeFleet::default();
        fleet.set("a", PlacementView::Drained);
        let applier = FakeApplier::default();
        let store = FailingStore {
            inner: MemoryRolloutStore::default(),
            writes: Mutex::new(writes),
        };
        let driver = RolloutDriver::new(&store, &fleet, &applier);
        let id = driver.start(rollout(&["a"], 1)).expect("start").id;
        let mut failed = None;
        for _ in 0..3 {
            if let Err(e) = driver.step(id) {
                failed = Some(e);
                break;
            }
        }
        assert_eq!(
            failed,
            Some(DriveError::Store(StoreError::Unavailable(
                "disk full".to_owned()
            )))
        );
        let acted = fleet.actions().len() + applier.handed.borrow().len();
        assert_eq!(
            acted, done,
            "{writes} write(s): only recorded steps were carried out"
        );
        let recorded = store.get(id).expect("recorded");
        let moved = recorded.node(&w("a")).expect("a").step();
        assert_eq!(
            moved,
            [NodeStep::Pending, NodeStep::Cordoned, NodeStep::Draining][done]
        );
    }
}

/// Catches: a hold that the store did not keep reported as held (the operator would
/// see a held rollout the record does not have).
#[test]
fn a_hold_the_store_refuses_is_an_error() {
    let fleet = FakeFleet::default();
    *fleet.refuse.borrow_mut() = Some(w("a"));
    let applier = FakeApplier::default();
    // One write starts the rollout and one records the cordon; the hold finds none.
    let store = FailingStore {
        inner: MemoryRolloutStore::default(),
        writes: Mutex::new(2),
    };
    let driver = RolloutDriver::new(&store, &fleet, &applier);
    let id = driver.start(rollout(&["a"], 1)).expect("start").id;
    assert_eq!(
        driver.step(id),
        Err(DriveError::Store(StoreError::Unavailable(
            "disk full".to_owned()
        )))
    );
    let recorded = store.get(id).expect("recorded");
    assert_eq!(recorded.state(), RolloutState::Running);
    assert_eq!(
        recorded.node(&w("a")).map(|p| p.step()),
        Some(NodeStep::Cordoned)
    );
}

/// Catches: a store that keeps half a change, overwrites a rollout on create, or
/// invents an unknown one; and a driver that steps a rollout that is not running.
#[test]
fn the_memory_store_keeps_whole_changes_only() {
    let store = MemoryRolloutStore::default();
    let r = rollout(&["a"], 1);
    store.create(r.clone()).expect("create");
    assert_eq!(
        store.create(r.clone()),
        Err(StoreError::Exists(RolloutId(1)))
    );
    assert_eq!(
        store.update(RolloutId(9), &|_| Ok(())),
        Err(StoreError::Unknown(RolloutId(9)))
    );
    let half = store.update(RolloutId(1), &|r| {
        r.set_state(RolloutState::Running)?;
        r.advance(&w("a"), NodeStep::Updating)
    });
    assert!(matches!(
        half,
        Err(StoreError::Illegal(IllegalStep::Node { .. }))
    ));
    assert_eq!(store.get(RolloutId(1)), Some(r));
    assert_eq!(store.get(RolloutId(9)), None);

    let fleet = FakeFleet::default();
    let applier = FakeApplier::default();
    let driver = RolloutDriver::new(&store, &fleet, &applier);
    assert_eq!(
        driver.step(RolloutId(9)),
        Err(DriveError::Store(StoreError::Unknown(RolloutId(9))))
    );
    let pending = driver.step(RolloutId(1)).expect("step");
    assert_eq!(pending.state(), RolloutState::Pending);
    assert!(fleet.actions().is_empty());
    let messages = [
        StoreError::Exists(RolloutId(1)).to_string(),
        StoreError::Unknown(RolloutId(1)).to_string(),
        StoreError::Unavailable("x".to_owned()).to_string(),
    ];
    assert_eq!(
        messages,
        [
            "rollout-1 is already recorded",
            "no rollout-1 is recorded",
            "the rollout store could not write: x",
        ]
    );
}

/// Catches: the farm's fleet view disagreeing with the scheduler (a node cordoned by a
/// rollout still offered work, an idle node never seen as drained) and an unknown node
/// accepted.
#[tokio::test]
async fn the_driver_runs_against_the_farm() {
    let cache = Arc::new(Cache::memory());
    let farm = Farm::new(
        cache,
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    let (outbound, _responses) = tokio::sync::mpsc::unbounded_channel();
    let caps = NodeCaps::from_report([("arch", "x86_64")]).expect("caps");
    farm.register(
        &w("a"),
        kbf_sched::DaemonInstance::new("a"),
        Resources::new(8_000, 16 << 30),
        caps,
        outbound,
        ServerMessage::default(),
    );
    let applier = FakeApplier::default();
    let store = MemoryRolloutStore::default();
    let driver = RolloutDriver::new(&store, &farm, &applier);
    let id = driver.start(rollout(&["a"], 1)).expect("start").id;
    driver.step(id).expect("cordon");
    assert_eq!(
        Fleet::placement(&farm, &w("a")),
        Some(connected(PlacementView::Cordoned))
    );
    driver.step(id).expect("drain");
    assert_eq!(
        Fleet::placement(&farm, &w("a")),
        Some(connected(PlacementView::Drained))
    );
    let r = driver.step(id).expect("update");
    assert_eq!(r.node(&w("a")).map(|p| p.step()), Some(NodeStep::Updating));
    assert_eq!(*applier.handed.borrow(), [update(1, "a")]);

    assert_eq!(Fleet::placement(&farm, &w("ghost")), None);
    assert_eq!(
        Fleet::place(&farm, &w("ghost"), NodeAction::Cordon),
        Err("no node ghost has registered".to_owned())
    );
}
