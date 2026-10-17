//! An operator's planned restart of a storage member (`Members::plan_down`): the member is
//! taken out of the write path at once, so an upload during its restart is not refused, and
//! it comes back by registering again.
//!
//! The write path is the real router over the real members' map and fake storage nodes; the
//! rest is the membership rows over the fake store with its injectable clock.
//!
//! Catches, in one line each: a write sent to the member that is about to stop; a planned
//! restart that a heartbeat undoes, or that is lost with the frontend that made it, or that
//! the member's return does not clear; one accepted for a member that is not `Active`; a
//! planned member left in the map past its dwell; a planned restart not shown to operators.

#[path = "support/hooked.rs"]
mod hooked;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use hooked::Hooked;
use kbf_map::{Member, MemberId, MemberState};
use kbf_proto::worker::segments_server::Segments;
use kbf_route::{Nodes, Router};
use kbf_server::front_maps::FrontMaps;
use kbf_server::members::{
    Members, MembershipConfig, MembershipError, PROTOCOL, Progress, Registrant,
};
use kbf_server::storage_health::{NotIdle, Planned, Reported};
use kbf_state::Store;
use kbf_storage::{Answer, PutStream, ReadStream, Storage, StorageError};
use kbf_types::Digest;

const SECOND: Duration = Duration::from_secs(1);
/// How long the plans of these tests last: longer than any of them runs.
const PLAN: Duration = Duration::from_secs(900);

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_secs(30),
        dwell: Duration::from_secs(120),
        replication: 2,
    }
}

fn registrant(i: u32) -> Registrant {
    Registrant {
        instance_id: format!("node-{i}"),
        addr: format!("10.0.0.{i}:8982"),
        zone: String::new(),
        weight: 1,
        protocol: PROTOCOL,
        progress: Progress::CaughtUp,
    }
}

/// Two `Active` members, replication 2, over a store whose clock a test moves.
struct Farm {
    store: Arc<Hooked>,
    members: Arc<Members>,
    ids: [MemberId; 2],
    boots: [u64; 2],
}

impl Farm {
    fn new() -> Self {
        let store = Hooked::new();
        let members = Arc::new(Members::new(Arc::clone(&store) as Arc<dyn Store>, config()));
        let a = members.register(&registrant(1)).expect("a");
        let b = members.register(&registrant(2)).expect("b");
        Self {
            store,
            members,
            ids: [a.id, b.id],
            boots: [a.boot, b.boot],
        }
    }

    fn beat(&self, which: usize) -> u64 {
        self.members
            .heartbeat(
                self.ids[which],
                self.boots[which],
                Progress::CaughtUp,
                Reported::default(),
            )
            .expect("beats")
    }

    fn state(&self, id: MemberId) -> Option<MemberState> {
        let map = self.members.current_map().expect("read").expect("a map");
        map.member(id).map(|m| m.state)
    }
}

/// A storage node that records what it is given and refuses everything once stopped, as a
/// node whose process has ended does. Only `put` is used by the write path under test.
struct Node {
    stopped: AtomicBool,
    puts: Mutex<Vec<Digest>>,
}

impl Node {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            stopped: AtomicBool::new(false),
            puts: Mutex::default(),
        })
    }

    fn put_log(&self) -> Vec<Digest> {
        self.puts.lock().expect("lock").clone()
    }
}

fn refused() -> StorageError {
    StorageError::Unavailable(Arc::new(std::fmt::Error))
}

impl Storage for Node {
    fn find_missing<'a>(&'a self, _: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        Box::pin(async { Err(StorageError::Internal("not used".to_owned())) })
    }

    fn read_blob<'a>(&'a self, _: &'a Digest) -> Answer<'a, Bytes> {
        Box::pin(async { Err(StorageError::Internal("not used".to_owned())) })
    }

    fn read_range<'a>(&'a self, _: &'a Digest, _: u64, _: u64) -> Answer<'a, ReadStream> {
        Box::pin(async { Err(StorageError::Internal("not used".to_owned())) })
    }

    fn put(&self, bytes: Bytes) -> Answer<'_, Digest> {
        Box::pin(async move {
            if self.stopped.load(Ordering::SeqCst) {
                return Err(refused());
            }
            let digest = kbf_segments::sha256(&bytes);
            self.puts.lock().expect("lock").push(digest);
            Ok(digest)
        })
    }

    fn stream(&self, _: u64) -> Option<Box<dyn PutStream>> {
        None
    }

    fn put_streamed(
        &self,
        _: u64,
        _: futures::stream::BoxStream<'static, Result<Bytes, std::io::Error>>,
        _: futures::future::BoxFuture<'static, Option<Digest>>,
    ) -> Answer<'_, Option<Digest>> {
        Box::pin(async { Err(StorageError::Internal("not used".to_owned())) })
    }

    fn segments(&self) -> Arc<dyn Segments> {
        unreachable!("the write path under test does not use segments")
    }

    fn action_result<'a>(
        &'a self,
        _: &'a Digest,
    ) -> Answer<'a, Option<(Digest, kbf_proto::reapi::ActionResult)>> {
        Box::pin(async { Err(StorageError::Internal("not used".to_owned())) })
    }

    fn prepare_action_result<'a>(
        &'a self,
        _: &'a kbf_proto::reapi::ActionResult,
    ) -> Answer<'a, kbf_meta::ActionRecord> {
        Box::pin(async { Err(StorageError::Internal("not used".to_owned())) })
    }

    fn commit_action_record(
        &self,
        _: kbf_meta::Role,
        _: Digest,
        _: kbf_meta::ActionRecord,
    ) -> Answer<'_, ()> {
        Box::pin(async { Err(StorageError::Internal("not used".to_owned())) })
    }
}

/// The nodes a router dials, by member id.
struct Fleet(BTreeMap<u64, Arc<Node>>);

impl Nodes for Fleet {
    fn node(&self, member: &Member, _: u64) -> Result<Arc<dyn Storage>, StorageError> {
        self.0
            .get(&member.id.0)
            .map(|node| Arc::clone(node) as Arc<dyn Storage>)
            .ok_or_else(refused)
    }
}

/// A router over `farm`'s members and a node for each, and a blob whose primary owner in
/// the farm's map is member `primary`.
fn router_over(farm: &Farm, primary: MemberId) -> (Router, [Arc<Node>; 2], Bytes) {
    let nodes = [Node::new(), Node::new()];
    let fleet = Fleet(BTreeMap::from([
        (farm.ids[0].0, Arc::clone(&nodes[0])),
        (farm.ids[1].0, Arc::clone(&nodes[1])),
    ]));
    let router = Router::new(
        Arc::new(FrontMaps::new(Arc::clone(&farm.members))),
        Arc::new(fleet),
    );
    let map = farm.members.current_map().expect("read").expect("a map");
    let blob = (0_u64..10_000)
        .map(|i| Bytes::from(format!("blob number {i}")))
        .find(|b| map.write_owners(&kbf_segments::sha256(b)).first() == Some(&primary))
        .expect("a blob that fits");
    (router, nodes, blob)
}

// Catches: the upload failure a restart causes today, and that planning it away works. Node
// a is the primary of the blob. Control: a stops with no plan, and for as long as the map
// says it is Active (up to a TTL) the upload is refused. Then, on a fresh farm, a is planned
// down while it still runs: the upload goes to b, and goes on going to b after a stops.
// Mutants (planted locally): `plan_down` leaving the member `Active` in the map; the
// router's write order not skipping `Down` members.
#[tokio::test]
async fn a_write_during_a_planned_restart_goes_to_the_other_owner() {
    let farm = Farm::new();
    let (router, nodes, blob) = router_over(&farm, farm.ids[0]);
    nodes[0].stopped.store(true, Ordering::SeqCst);
    let error = router
        .put(blob.clone())
        .await
        .expect_err("a stopped primary");
    assert!(matches!(error, StorageError::Unavailable(_)), "{error:?}");
    assert!(
        nodes[1].put_log().is_empty(),
        "a write goes to one owner only"
    );

    let farm = Farm::new();
    let (router, nodes, blob) = router_over(&farm, farm.ids[0]);
    farm.members.plan_down(farm.ids[0], PLAN).expect("planned");
    let digest = kbf_segments::sha256(&blob);
    assert_eq!(router.put(blob.clone()).await.expect("b takes it"), digest);
    assert!(
        nodes[0].put_log().is_empty(),
        "nothing is sent to the member restarting"
    );
    assert_eq!(nodes[1].put_log(), [digest]);

    nodes[0].stopped.store(true, Ordering::SeqCst);
    assert_eq!(router.put(blob).await.expect("still b"), digest);
    assert_eq!(nodes[1].put_log().len(), 2);
}

// Catches: a planned restart that is not Down in the map, or that a heartbeat undoes (the
// member keeps heartbeating until it stops), or that is not shown: the new generation has
// the member Down and the other Active, the heartbeats leave it so, and the operator's view
// says state down and planned_down true for it only.
#[test]
fn planning_down_is_down_in_the_map_survives_heartbeats_and_is_shown() {
    let farm = Farm::new();
    let [a, b] = farm.ids;
    let before = farm
        .members
        .current_map()
        .expect("read")
        .expect("map")
        .generation();
    let map = farm.members.plan_down(a, PLAN).expect("planned");
    assert_eq!(map.generation(), before + 1);
    assert_eq!(map.member(a).map(|m| m.state), Some(MemberState::Down));
    assert_eq!(map.member(b).map(|m| m.state), Some(MemberState::Active));

    for _ in 0..3 {
        assert_eq!(farm.beat(0), map.generation(), "a beat changes nothing");
        farm.beat(1);
    }
    assert_eq!(farm.state(a), Some(MemberState::Down));
    assert!(farm.members.planned_down(a).expect("read"));
    assert!(!farm.members.planned_down(b).expect("read"));

    let health = farm.members.storage_health().expect("health");
    let shown: Vec<_> = health
        .nodes
        .iter()
        .map(|n| (n.id, n.state, n.planned_down))
        .collect();
    assert_eq!(
        shown,
        [
            (a, MemberState::Down, true),
            (b, MemberState::Active, false)
        ]
    );
}

// Catches: a restart planned for a member that cannot return to where it was (a joiner or a
// rebuilding member would be made Active by the undo), and one planned for a member nobody
// registered.
#[test]
fn only_an_active_member_can_be_planned_down() {
    let farm = Farm::new();
    let [a, b] = farm.ids;
    for state in [
        MemberState::Joining,
        MemberState::CatchingUp,
        MemberState::Draining,
    ] {
        farm.members.set_state(a, state).expect("set");
        let refused = farm.members.plan_down(a, PLAN).expect_err("not active");
        assert!(
            matches!(refused, MembershipError::NotActive { id, state: s } if id == a && s == state),
            "{state:?}: {refused:?}"
        );
        assert!(!farm.members.planned_down(a).expect("read"));
    }
    farm.members.plan_down(b, PLAN).expect("planned");
    let stranger = farm
        .members
        .plan_down(MemberId(9), PLAN)
        .expect_err("nobody");
    assert!(matches!(
        stranger,
        MembershipError::NotRegistered(MemberId(9))
    ));
}

// Catches: a planned restart kept only in the frontend that made it. A second `Members` over
// the same store (a frontend that restarted, or another) sees the member Down and planned,
// and the shown view says so. Mutant: the mark left unwritten.
#[test]
fn a_planned_restart_survives_a_frontend_restart() {
    let farm = Farm::new();
    let a = farm.ids[0];
    farm.members.plan_down(a, PLAN).expect("planned");

    let restarted = Members::new(Arc::clone(&farm.store) as Arc<dyn Store>, config());
    let map = restarted.current_map().expect("read").expect("map");
    assert_eq!(map.member(a).map(|m| m.state), Some(MemberState::Down));
    assert!(restarted.planned_down(a).expect("read"));
    let health = restarted.storage_health().expect("health");
    assert!(health.nodes[0].planned_down);
    assert!(!health.nodes[1].planned_down);
    assert_eq!(
        restarted
            .refresh()
            .expect("refresh")
            .map(|m| m.generation()),
        Some(map.generation())
    );
}

// Catches: a planned member that leaves the map when its process stops, which would move its
// slice after one TTL instead of one dwell; and one that never leaves. The member stops after
// being planned (its row lapses 30 s later): it is still in the map, Down and planned, at 100 s
// from the plan; and after the dwell (120 s from the plan) it is gone and no longer planned.
#[test]
fn a_planned_member_that_stops_keeps_its_slot_for_the_dwell() {
    let farm = Farm::new();
    let a = farm.ids[0];
    farm.members.plan_down(a, PLAN).expect("planned");
    // b keeps beating every 20 s; a has stopped.
    let pass = |seconds: u64| {
        for _ in 0..seconds / 20 {
            farm.store.clock.advance(SECOND * 20);
            farm.beat(1);
        }
        farm.members.refresh().expect("refresh").expect("map")
    };
    let map = pass(40);
    assert_eq!(
        map.member(a).map(|m| m.state),
        Some(MemberState::Down),
        "40 s: the row lapsed"
    );
    assert!(farm.members.planned_down(a).expect("read"));

    let map = pass(60);
    assert_eq!(
        map.member(a).map(|m| m.state),
        Some(MemberState::Down),
        "100 s"
    );

    let map = pass(40);
    assert_eq!(map.member(a), None, "140 s: the dwell is over");
    assert!(!farm.members.planned_down(a).expect("read"));
}

// Catches: a restart that does not return the member to the map: the process registers again
// (boot + 1, its index whole) and is Active with its id, the mark deleted and the map bumped;
// a rebuilding one is CatchingUp, then Active at its first report of done. And an undo with
// no restart: `set_state(Active)` clears the mark as well.
#[test]
fn the_member_returns_by_registering_or_by_an_undo() {
    let farm = Farm::new();
    let [a, b] = farm.ids;
    let planned = farm.members.plan_down(a, PLAN).expect("planned");

    let again = farm
        .members
        .register(&registrant(1))
        .expect("registers again");
    assert_eq!((again.id, again.boot), (a, farm.boots[0] + 1));
    assert_eq!(
        again.map.member(a).map(|m| m.state),
        Some(MemberState::Active)
    );
    assert!(again.map.generation() > planned.generation());
    assert!(
        !farm.members.planned_down(a).expect("read"),
        "cleared on rejoin"
    );

    farm.members.plan_down(b, PLAN).expect("planned");
    let rebuilding = Registrant {
        progress: Progress::CatchingUp,
        ..registrant(2)
    };
    let back = farm.members.register(&rebuilding).expect("registers again");
    assert_eq!(
        back.map.member(b).map(|m| m.state),
        Some(MemberState::CatchingUp)
    );
    assert!(!farm.members.planned_down(b).expect("read"));
    let beat = farm
        .members
        .heartbeat(b, back.boot, Progress::CaughtUp, Reported::default())
        .expect("done");
    assert!(beat > back.map.generation());
    assert_eq!(farm.state(b), Some(MemberState::Active));

    farm.members.plan_down(a, PLAN).expect("planned again");
    let undone = farm
        .members
        .set_state(a, MemberState::Active)
        .expect("undone");
    assert_eq!(undone.member(a).map(|m| m.state), Some(MemberState::Active));
    assert!(
        !farm.members.planned_down(a).expect("read"),
        "the mark goes with the undo"
    );
}

// Catches: a controller that dies leaving a healthy node out of service for ever. A plan
// lasts `for_`; the member keeps beating, and its first beat after that returns it to Active
// in a new generation, the mark gone, and `node_idle` says the plan ran out before that. Before
// that time a beat changes nothing. Mutants: the expiry not checked on a beat; checked
// against the wrong clock (it returns at once).
#[test]
fn a_planned_restart_ends_by_the_next_beat_after_its_time() {
    let farm = Farm::new();
    let [a, _] = farm.ids;
    let planned = farm
        .members
        .plan_down(a, Duration::from_secs(100))
        .expect("planned");
    let quiet = |which: usize| {
        farm.members
            .heartbeat(
                farm.ids[which],
                farm.boots[which],
                Progress::CaughtUp,
                Reported::debts(kbf_server::storage_health::Debts::Known(0))
                    .with_writes(kbf_server::storage_health::Writes::Known(0), 0),
            )
            .expect("beats")
    };
    for _ in 0..4 {
        farm.store.clock.advance(SECOND * 20);
        quiet(0);
        quiet(1);
    }
    assert_eq!(farm.state(a), Some(MemberState::Down), "80 s of 100");
    assert!(farm.members.planned_down(a).expect("read"));
    assert_eq!(quiet(0), planned.generation());

    farm.store.clock.advance(SECOND * 20);
    quiet(1);
    assert!(!farm.members.planned_down(a).expect("read"), "run out");
    assert_eq!(
        farm.members.node_idle(a, 0),
        Err(NotIdle::PlannedDownExpired { member: a }),
        "still Down in the map until the node's beat"
    );
    let back = quiet(0);
    assert!(back > planned.generation(), "a new generation");
    assert_eq!(farm.state(a), Some(MemberState::Active));
    assert!(
        farm.store.fake.get("down/1").expect("read").is_none(),
        "the mark is gone"
    );
    assert_eq!(farm.members.planned(a).expect("read"), Planned::No);
}

// Catches: a second controller (or a retry) refused with an error, or a repeat that bumps the
// map: asking again for a member already planned down is the same plan with its end moved
// to a whole `for_` from now, in the same generation. The plan that would have ended is
// still running after the repeat. Mutant: the repeat leaves the old end.
#[test]
fn planning_down_again_moves_the_end_and_changes_no_generation() {
    let farm = Farm::new();
    let [a, _] = farm.ids;
    let first = farm
        .members
        .plan_down(a, Duration::from_secs(60))
        .expect("planned");
    farm.store.clock.advance(SECOND * 20);
    farm.beat(0);
    farm.beat(1);
    let again = farm
        .members
        .plan_down(a, Duration::from_secs(60))
        .expect("the same plan again");
    assert_eq!(again.generation(), first.generation());
    assert_eq!(farm.state(a), Some(MemberState::Down));
    for _ in 0..2 {
        farm.store.clock.advance(SECOND * 20);
        farm.beat(0);
        farm.beat(1);
    }
    assert_eq!(
        farm.state(a),
        Some(MemberState::Down),
        "60 s: the first plan would have ended here, the repeated one ends at 80 s"
    );
    farm.store.clock.advance(SECOND * 25);
    farm.beat(1);
    farm.beat(0);
    assert_eq!(farm.state(a), Some(MemberState::Active), "85 s: past it");
}
