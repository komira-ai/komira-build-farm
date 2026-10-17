//! Membership in the state store (`kbf_server::members`): identity assigned by the store,
//! expiry judged by the store's clock, the map rebuilt from the rows with its generation
//! bumped by compare-and-set, and what the rebuilt map does to placement. Every test
//! runs on the fake store with its injectable clock.

#[path = "support/hooked.rs"]
mod hooked;

use std::sync::Arc;
use std::time::Duration;

use hooked::Hooked;
use kbf_map::{Map, MapError, MemberId, MemberState};
use kbf_proto::kbf::map::v1 as pb;
use kbf_server::members::{
    Members, MembershipConfig, MembershipError, PROTOCOL, Progress, Registrant, Registration,
};
use kbf_server::storage_health::Debts;
use kbf_state::{Cond, Store, Txn};
use kbf_types::{Digest, DigestFunction};
use prost::Message as _;
use sha2::{Digest as _, Sha256};

const SECOND: Duration = Duration::from_secs(1);

fn config(ttl: u64, dwell: u64) -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_secs(ttl),
        dwell: Duration::from_secs(dwell),
        replication: 2,
    }
}

/// Membership through the (hookable) store.
fn members(store: &Arc<Hooked>, config: MembershipConfig) -> Members {
    Members::new(Arc::clone(store) as Arc<dyn Store>, config)
}

/// Membership straight on the fake: another frontend, which no hook interrupts.
fn other_frontend(store: &Arc<Hooked>, config: MembershipConfig) -> Members {
    Members::new(Arc::clone(&store.fake) as Arc<dyn Store>, config)
}

fn node(i: u32) -> Registrant {
    Registrant {
        instance_id: format!("node-{i}"),
        addr: format!("10.0.0.{i}:8982"),
        zone: String::new(),
        weight: 1,
        protocol: PROTOCOL,
        progress: Progress::CaughtUp,
    }
}

fn state(map: &Map, id: MemberId) -> Option<MemberState> {
    map.member(id).map(|m| m.state)
}

/// Registers `i` and makes it `Joining`, the state a registration never writes: the join
/// handoff and the operator set it by hand. The returned map is the one after that.
fn joining(m: &Members, i: u32) -> Registration {
    let mut r = m.register(&node(i)).expect("register");
    r.map = m
        .set_state(r.id, MemberState::Joining)
        .expect("set joining");
    r
}

fn digests() -> Vec<Digest> {
    (0_u64..300)
        .map(|i| {
            let hash: [u8; 32] = Sha256::digest(i.to_be_bytes()).into();
            Digest::new(DigestFunction::Sha256, hash, 1000 + i)
        })
        .collect()
}

/// Catches: a member id or boot counter that a node, or the order of calls, decides
/// instead of the store: two registrations must get distinct ids, the same instance must
/// keep its id and get a larger boot counter every time (also through another frontend
/// that holds no state of its own), and boot counters are per member.
#[test]
fn the_store_assigns_identity_and_a_boot_counter_per_member() {
    let s = Hooked::new();
    let m = members(&s, config(30, 120));
    let a = m.register(&node(1)).expect("a");
    let b = m.register(&node(2)).expect("b");
    assert_ne!(a.id, b.id);
    assert_eq!((a.boot, b.boot), (1, 1));
    let again = m.register(&node(1)).expect("a again");
    assert_eq!((again.id, again.boot), (a.id, 2));
    let elsewhere = other_frontend(&s, config(30, 120))
        .register(&node(1))
        .expect("a through another frontend");
    assert_eq!((elsewhere.id, elsewhere.boot), (a.id, 3));
    let c = m.register(&node(3)).expect("c");
    assert!(c.id != a.id && c.id != b.id);
    assert_eq!(c.boot, 1, "b and c have registered once each");
    let ids: Vec<MemberId> = c.map.members().iter().map(|x| x.id).collect();
    assert_eq!(ids.len(), 3, "each member appears once: {ids:?}");
}

/// Catches: a caught-up registration that enters as anything but Active (a restarted
/// node would stay Joining for ever), a registration that is not in the map it returns,
/// the cluster not marked formed by it, and a map before any member (there is none).
#[test]
fn a_caught_up_member_enters_the_map_active() {
    let s = Hooked::new();
    let m = members(&s, config(30, 120));
    assert_eq!(m.current_map().expect("read"), None);
    let a = m.register(&node(1)).expect("a");
    assert_eq!(a.map.generation(), 1);
    assert_eq!(state(&a.map, a.id), Some(MemberState::Active));
    assert!(s.fake.get("meta/members_formed").expect("read").is_some());
    assert_eq!(m.current_map().expect("read"), Some(a.map));
}

/// Catches: expiry judged early, late or by the wrong clock, a Down member that leaves
/// before its dwell or never, and a flap that costs a generation: member b stops
/// heartbeating at once; it is still Active 29 s later, Down after 32 s (TTL 30 s), still
/// in the map as Down until the dwell (120 s from the moment it went Down) has run out,
/// and then gone, with exactly one generation for each of the two changes.
#[test]
fn a_member_goes_down_after_the_ttl_and_leaves_after_the_dwell() {
    let s = Hooked::new();
    let m = members(&s, config(30, 120));
    let a = m.register(&node(1)).expect("a");
    let b = m.register(&node(2)).expect("b");
    m.set_state(a.id, MemberState::Active).expect("a active");
    let map = m.set_state(b.id, MemberState::Active).expect("b active");
    let g = map.generation();

    s.clock.advance(20 * SECOND);
    m.heartbeat(a.id, a.boot, Progress::CaughtUp, Debts::Unknown)
        .expect("a beats");
    s.clock.advance(9 * SECOND);
    let early = m.refresh().expect("refresh").expect("a map");
    assert_eq!(
        state(&early, b.id),
        Some(MemberState::Active),
        "never early"
    );
    assert_eq!(early.generation(), g, "nothing changed, nothing bumped");

    s.clock.advance(3 * SECOND);
    m.heartbeat(a.id, a.boot, Progress::CaughtUp, Debts::Unknown)
        .expect("a beats");
    let down = m.refresh().expect("refresh").expect("a map");
    assert_eq!(state(&down, b.id), Some(MemberState::Down));
    assert_eq!(state(&down, a.id), Some(MemberState::Active));
    assert_eq!(down.generation(), g + 1);

    for _ in 0..10 {
        s.clock.advance(10 * SECOND);
        m.heartbeat(a.id, a.boot, Progress::CaughtUp, Debts::Unknown)
            .expect("a beats");
    }
    let within = m.refresh().expect("refresh").expect("a map");
    assert_eq!(
        state(&within, b.id),
        Some(MemberState::Down),
        "100 s of 120"
    );
    assert_eq!(within.generation(), g + 1);

    for _ in 0..3 {
        s.clock.advance(10 * SECOND);
        m.heartbeat(a.id, a.boot, Progress::CaughtUp, Debts::Unknown)
            .expect("a beats");
    }
    let gone = m.refresh().expect("refresh").expect("a map");
    assert_eq!(state(&gone, b.id), None, "130 s: the dwell is over");
    assert_eq!(gone.generation(), g + 2);
    assert_eq!(gone.members().len(), 1);
}

/// Catches: a heartbeat that does not restart the time to live (the member dies at the
/// first TTL), and an administrator's `set_state` that does (it would give a dead node a
/// whole new TTL). Six beats 20 s apart span four TTLs and the member stays put; a state
/// change at 25 s does not move the expiry at 30 s.
#[test]
fn heartbeats_keep_a_member_alive_and_set_state_does_not() {
    let s = Hooked::new();
    let m = members(&s, config(30, 120));
    let a = m.register(&node(1)).expect("a");
    let b = joining(&m, 2);
    let g = m.current_map().expect("read").expect("map").generation();
    for round in 1..=6 {
        s.clock.advance(20 * SECOND);
        m.heartbeat(a.id, a.boot, Progress::CaughtUp, Debts::Unknown)
            .expect("a beats");
        m.heartbeat(b.id, b.boot, Progress::CaughtUp, Debts::Unknown)
            .expect("b beats");
        let map = m.refresh().expect("refresh").expect("map");
        assert_eq!(
            state(&map, a.id),
            Some(MemberState::Active),
            "round {round}"
        );
        assert_eq!(
            state(&map, b.id),
            Some(MemberState::Joining),
            "round {round}"
        );
        assert_eq!(map.generation(), g, "round {round}: no change, no bump");
    }

    s.clock.advance(25 * SECOND);
    m.set_state(a.id, MemberState::Draining).expect("drain");
    s.clock.advance(6 * SECOND);
    let map = m.refresh().expect("refresh").expect("map");
    assert_eq!(
        state(&map, a.id),
        Some(MemberState::Down),
        "31 s after its beat"
    );
}

/// Catches: a last member that leaves (a map cannot be empty, so it would be unreadable),
/// and a member that returns inside its dwell keeping its Down state or losing its id: it
/// is Active again with the same id and a larger boot counter, and its dwell mark is
/// gone.
#[test]
fn the_last_member_stays_and_a_returning_member_is_active() {
    let s = Hooked::new();
    let m = members(&s, config(30, 120));
    let a = m.register(&node(1)).expect("a");
    s.clock.advance(31 * SECOND);
    let down = m.refresh().expect("refresh").expect("map");
    assert_eq!(state(&down, a.id), Some(MemberState::Down));
    s.clock.advance(500 * SECOND);
    let still = m.refresh().expect("refresh").expect("map");
    assert_eq!(
        state(&still, a.id),
        Some(MemberState::Down),
        "the only member"
    );
    assert_eq!(still.generation(), down.generation());

    let back = m.register(&node(1)).expect("back");
    assert_eq!((back.id, back.boot), (a.id, 2));
    assert_eq!(state(&back.map, a.id), Some(MemberState::Active));
    assert_eq!(s.fake.get("down/1").expect("read"), None);
}

/// Catches: a lost update when another frontend changes the map between this one's reads
/// and its commit: the dwell of member d runs out (another frontend's refresh removes it,
/// generation g+1, and member b's registration is in it) while this frontend, which read
/// d's mark as live, is about to commit g+1 with d still in. Exactly one commit wins; this
/// one conflicts once and its retry finds nothing left to do. A compare-and-set that
/// ignores the map's version would put d back.
#[test]
fn a_racing_refresh_gives_one_winner_and_one_retry() {
    let s = Hooked::new();
    let cfg = config(60, 10);
    let m = members(&s, cfg);
    let a = m.register(&node(1)).expect("a");
    let d = m.register(&node(4)).expect("d");
    m.set_state(a.id, MemberState::Active).expect("a");
    m.set_state(d.id, MemberState::Active).expect("d");
    s.clock.advance(40 * SECOND);
    m.heartbeat(a.id, a.boot, Progress::CaughtUp, Debts::Unknown)
        .expect("a beats");
    s.clock.advance(25 * SECOND);
    let down = m.refresh().expect("refresh").expect("map");
    assert_eq!(state(&down, d.id), Some(MemberState::Down));

    s.map_writes.lock().expect("lock").clear();
    let rival = other_frontend(&s, cfg);
    let clock = Arc::clone(&s.clock);
    s.after_get("down/", move || {
        clock.advance(11 * SECOND);
        rival.refresh().expect("the rival refreshes");
    });
    let b = m
        .register(&node(2))
        .expect("b registers through the racing frontend");

    let map = m.current_map().expect("read").expect("map");
    assert_eq!(state(&map, d.id), None, "d left and stayed gone");
    assert_eq!(state(&map, b.id), Some(MemberState::Active));
    assert_eq!(map.generation(), down.generation() + 1);
    assert_eq!(b.map, map, "the loser answers with the winner's map");
    assert_eq!(
        *s.map_writes.lock().expect("lock"),
        vec![true],
        "one commit of ours, and it conflicted"
    );
}

/// Catches: a refresh that decides on a row it no longer holds: member a's row has
/// expired, so this frontend plans to mark it Down; before it commits, a registers again
/// (its own refresh finds the map already says Active and bumps nothing). The map's
/// version has not moved, so only the check on a's row stops this frontend from marking a
/// Down beside its live row. The commit conflicts and the retry sees the row.
#[test]
fn a_refresh_does_not_down_a_member_that_registered_meanwhile() {
    let s = Hooked::new();
    let cfg = config(30, 120);
    let m = members(&s, cfg);
    let a = m.register(&node(1)).expect("a");
    s.clock.advance(31 * SECOND);

    s.map_writes.lock().expect("lock").clear();
    let rival = other_frontend(&s, cfg);
    s.after_get("member/", move || {
        rival.register(&node(1)).expect("a registers again");
    });
    let map = m.refresh().expect("refresh").expect("map");

    assert_eq!(state(&map, a.id), Some(MemberState::Active));
    assert_eq!(map.generation(), 1);
    assert_eq!(s.fake.get("down/1").expect("read"), None);
    assert_eq!(*s.map_writes.lock().expect("lock"), vec![true]);
}

/// Catches: removal that moves more than the removed member's blobs. Of four members one
/// stops; while it is Down it keeps its slot (nothing moves), and when its dwell is over
/// every digest it did not own has the same owners as before, and some it owned have new
/// ones (the property is not vacuous).
#[test]
fn removing_a_member_remaps_only_its_blobs() {
    let s = Hooked::new();
    let m = members(&s, config(30, 120));
    let regs: Vec<_> = (1..=4)
        .map(|i| m.register(&node(i)).expect("reg"))
        .collect();
    for r in &regs {
        m.set_state(r.id, MemberState::Active).expect("active");
    }
    // Not the last id: a map that renumbered its survivors would be caught.
    let leaving = regs[1].id;
    let survivors = [&regs[0], &regs[2], &regs[3]];
    let beat = |secs: u64| {
        for _ in 0..secs / 10 {
            s.clock.advance(10 * SECOND);
            for r in survivors {
                m.heartbeat(r.id, r.boot, Progress::CaughtUp, Debts::Unknown)
                    .expect("beat");
            }
        }
    };
    let active = m.current_map().expect("read").expect("map");
    beat(40);
    let down = m.refresh().expect("refresh").expect("map");
    assert_eq!(state(&down, leaving), Some(MemberState::Down));
    beat(130);
    let gone = m.refresh().expect("refresh").expect("map");
    assert_eq!(state(&gone, leaving), None);

    let (mut owned, mut moved) = (0, 0);
    for d in digests() {
        let before = down.write_owners(&d);
        assert_eq!(before, active.write_owners(&d), "Down keeps its slot");
        let after = gone.write_owners(&d);
        if before.contains(&leaving) {
            owned += 1;
            assert!(!after.contains(&leaving));
            moved += usize::from(after != before);
        } else {
            assert_eq!(after, before, "a blob it did not own must not move");
        }
    }
    assert!(owned > 0 && moved == owned, "{owned} owned, {moved} moved");
}

/// Catches: a joiner that is a read owner before it is Active, or a map that never
/// promotes it. A Joining member is read last whenever it is listed, is never rank 1 of a
/// write, and takes some writes; after `set_state(Active)` it is first for some digests.
#[test]
fn a_joining_node_is_read_last_until_it_is_active() {
    let s = Hooked::new();
    let m = members(&s, config(30, 120));
    for i in 1..=2 {
        m.register(&node(i)).expect("reg");
    }
    joining(&m, 3);
    let joiner = joining(&m, 4);
    let map = m.current_map().expect("read").expect("map");
    assert_eq!(state(&map, joiner.id), Some(MemberState::Joining));

    let (mut listed, mut writes) = (0, 0);
    for d in digests() {
        let order = map.read_order(&d);
        if let Some(at) = order.iter().position(|id| *id == joiner.id) {
            listed += 1;
            assert_eq!(at, order.len() - 1, "the joiner is read last");
        }
        let owners = map.write_owners(&d);
        writes += usize::from(owners.contains(&joiner.id));
        assert_ne!(owners.first(), Some(&joiner.id), "never rank 1");
    }
    assert!(
        listed > 0 && writes > 0,
        "{listed} listed, {writes} written"
    );

    let promoted = m
        .set_state(joiner.id, MemberState::Active)
        .expect("promote");
    assert_eq!(state(&promoted, joiner.id), Some(MemberState::Active));
    assert!(
        digests()
            .iter()
            .any(|d| promoted.read_order(d).first() == Some(&joiner.id)),
        "an Active member is read first for some digests"
    );
}

fn rebuilding(i: u32) -> Registrant {
    Registrant {
        progress: Progress::CatchingUp,
        ..node(i)
    }
}

/// Catches: a report of "done" that changes a state other than CatchingUp, and one of
/// "still rebuilding" that demotes. A member reporting done stays Draining, Joining (the
/// join handoff promotes it, not its own report) and Active; one reporting it is
/// rebuilding stays Active; a CatchingUp member whose row expired is Down, and its report
/// is NOT_REGISTERED and leaves it Down. No such beat bumps the generation. Mutants: the
/// promotion without the CatchingUp condition (Draining and Joining become Active); the
/// report written as the state (Active demoted to CatchingUp).
#[test]
fn a_report_cannot_change_a_member_that_is_not_catching_up() {
    let s = Hooked::new();
    let m = members(&s, config(30, 120));
    let draining = m.register(&rebuilding(1)).expect("draining");
    let joining = joining(&m, 2);
    let active = m.register(&node(3)).expect("active");
    let down = m.register(&rebuilding(4)).expect("down");
    m.set_state(draining.id, MemberState::Draining)
        .expect("drain");
    let map = m.current_map().expect("read").expect("map");
    assert_eq!(state(&map, active.id), Some(MemberState::Active));
    assert_eq!(state(&map, down.id), Some(MemberState::CatchingUp));

    let g = map.generation();
    for (who, progress) in [
        (&draining, Progress::CaughtUp),
        (&joining, Progress::CaughtUp),
        (&active, Progress::CaughtUp),
        (&active, Progress::CatchingUp),
    ] {
        assert_eq!(
            m.heartbeat(who.id, who.boot, progress, Debts::Unknown)
                .expect("beat"),
            g
        );
    }
    let map = m.current_map().expect("read").expect("map");
    assert_eq!(state(&map, draining.id), Some(MemberState::Draining));
    assert_eq!(state(&map, joining.id), Some(MemberState::Joining));
    assert_eq!(state(&map, active.id), Some(MemberState::Active));

    s.clock.advance(20 * SECOND);
    for who in [&draining, &joining, &active] {
        m.heartbeat(who.id, who.boot, Progress::CatchingUp, Debts::Unknown)
            .expect("beat");
    }
    s.clock.advance(11 * SECOND);
    let map = m.refresh().expect("refresh").expect("map");
    assert_eq!(state(&map, down.id), Some(MemberState::Down));
    assert!(matches!(
        m.heartbeat(down.id, down.boot, Progress::CaughtUp, Debts::Unknown),
        Err(MembershipError::NotRegistered(id)) if id == down.id
    ));
    let after = m.current_map().expect("read").expect("map");
    assert_eq!(state(&after, down.id), Some(MemberState::Down));
    assert_eq!(after.generation(), map.generation());
}

/// Catches: a promotion on a report that overwrites a state set after the member
/// registered: a CatchingUp member an administrator sets Draining stays Draining when it
/// reports done. (The promotion is decided in the heartbeat's own compare-and-set on the
/// row, so a state written between registration and report is what it sees.) And the
/// rebuilding member that does report done is Active, in one new generation.
#[test]
fn a_report_of_done_promotes_only_a_catching_up_member() {
    let s = Hooked::new();
    let m = members(&s, config(30, 120));
    let a = m.register(&rebuilding(1)).expect("a");
    let b = m.register(&rebuilding(2)).expect("b");
    let map = m.set_state(b.id, MemberState::Draining).expect("drain b");
    assert_eq!(state(&map, a.id), Some(MemberState::CatchingUp));
    let g = map.generation();
    assert_eq!(
        m.heartbeat(a.id, a.boot, Progress::CatchingUp, Debts::Unknown)
            .expect("beat"),
        g
    );
    assert_eq!(
        m.heartbeat(a.id, a.boot, Progress::CaughtUp, Debts::Unknown)
            .expect("beat"),
        g + 1
    );
    assert_eq!(
        m.heartbeat(b.id, b.boot, Progress::CaughtUp, Debts::Unknown)
            .expect("beat"),
        g + 1
    );
    let map = m.current_map().expect("read").expect("map");
    assert_eq!(state(&map, a.id), Some(MemberState::Active));
    assert_eq!(state(&map, b.id), Some(MemberState::Draining));
    assert_eq!(
        m.heartbeat(a.id, a.boot, Progress::CaughtUp, Debts::Unknown)
            .expect("beat"),
        g + 1,
        "a second report changes nothing"
    );
}

/// Catches: a malformed registration or an old protocol that is accepted, a promotion
/// that works on a member with no row, a state a hand cannot set, a heartbeat from an
/// older boot that is allowed to keep a newer registration's row alive, and a
/// registration that lost to a newer one of the same instance and still claims the row.
#[test]
fn bad_calls_are_refused_by_name() {
    let s = Hooked::new();
    let m = members(&s, config(30, 120));
    let blank = |f: fn(&mut Registrant)| {
        let mut who = node(1);
        f(&mut who);
        m.register(&who).expect_err("refused")
    };
    for e in [
        blank(|w| w.instance_id.clear()),
        blank(|w| w.addr.clear()),
        blank(|w| w.weight = 0),
    ] {
        assert!(matches!(e, MembershipError::Invalid(_)), "{e}");
    }
    let old = blank(|w| w.protocol = 0);
    assert!(
        matches!(old, MembershipError::Protocol { got: 0, min: 1 }),
        "{old}"
    );

    let a = m.register(&node(1)).expect("a");
    let b = m.register(&node(1)).expect("a again");
    assert!(matches!(
        m.heartbeat(a.id, a.boot, Progress::CaughtUp, Debts::Unknown),
        Err(MembershipError::Superseded {
            boot: 1,
            current: 2,
            ..
        })
    ));
    assert!(
        m.heartbeat(b.id, b.boot, Progress::CaughtUp, Debts::Unknown)
            .is_ok()
    );
    assert!(matches!(
        m.heartbeat(MemberId(99), 1, Progress::CaughtUp, Debts::Unknown),
        Err(MembershipError::NotRegistered(MemberId(99)))
    ));
    assert!(matches!(
        m.set_state(MemberId(99), MemberState::Active),
        Err(MembershipError::NotRegistered(_))
    ));
    assert!(matches!(
        m.set_state(a.id, MemberState::Down),
        Err(MembershipError::NotSettable(MemberState::Down))
    ));

    // A registration that is overtaken between taking its boot and writing its row.
    let rival = other_frontend(&s, config(30, 120));
    s.after_get("member/1", move || {
        rival.register(&node(1)).expect("the rival");
    });
    let lost = m.register(&node(1)).expect_err("overtaken");
    assert!(
        matches!(
            lost,
            MembershipError::Superseded {
                boot: 3,
                current: 4,
                ..
            }
        ),
        "{lost}"
    );
    assert!(lost.to_string().contains("superseded"));
}

/// Catches: the map's replication factor not following the member count (a one-member
/// map must have R = 1 or it is refused), a replication of 0 reaching the store as a
/// panic instead of an error, and the generation a heartbeat reports.
#[test]
fn the_map_clamps_replication_and_reports_its_generation() {
    let s = Hooked::new();
    let m = members(&s, config(30, 120));
    let a = m.register(&node(1)).expect("a");
    assert_eq!(a.map.r(), 1);
    let b = m.register(&node(2)).expect("b");
    assert_eq!(b.map.r(), 2);
    assert_eq!(
        m.heartbeat(a.id, a.boot, Progress::CaughtUp, Debts::Unknown)
            .expect("beat"),
        b.map.generation()
    );
    assert_eq!(m.config().beat(), 10 * SECOND);

    let zero = Hooked::new();
    let broken = members(
        &zero,
        MembershipConfig {
            replication: 0,
            ..config(30, 120)
        },
    );
    assert!(matches!(
        broken.register(&node(1)),
        Err(MembershipError::Map(MapError::ZeroReplication))
    ));
}

/// Catches: values in the store that the code cannot read being treated as empty or
/// panicking: a member row that is not a message, a row without a member, a map that is
/// not a message, a map the domain refuses, and an instance entry that is not an id. Each
/// is an error that names it.
#[test]
fn unreadable_rows_are_errors_not_defaults() {
    let put = |s: &Arc<Hooked>, key: &str, value: Vec<u8>| {
        s.fake
            .txn(Txn::new().put(key, value, Cond::Any))
            .expect("store")
            .expect("commit");
    };
    let corrupt = |e: MembershipError| match e {
        MembershipError::Corrupt(what) => what,
        other => panic!("expected Corrupt, got {other}"),
    };

    let s = Hooked::new();
    let m = members(&s, config(30, 120));
    put(&s, "member/9", vec![0xff, 0xff]);
    assert!(corrupt(m.refresh().expect_err("garbage row")).starts_with("member row"));
    put(&s, "member/9", Vec::new());
    assert!(corrupt(m.refresh().expect_err("row without member")).starts_with("member row"));

    let s = Hooked::new();
    let m = members(&s, config(30, 120));
    put(&s, "map", vec![0xff, 0xff]);
    assert!(corrupt(m.current_map().expect_err("garbage map")).starts_with("map"));
    put(&s, "map", pb::Map::default().encode_to_vec());
    assert!(corrupt(m.current_map().expect_err("generation 0")).starts_with("map"));

    put(&s, "instance/node-1", b"abc".to_vec());
    let e = m.register(&node(1)).expect_err("bad id");
    assert!(corrupt(e).contains("instance"));
}

/// Catches: a retry loop that spins for ever against writers it keeps losing to, and a
/// store failure that is swallowed. Each write path gives up with `Contended` when every
/// commit conflicts, and a failing read is a `Store` error.
#[test]
fn writers_that_keep_losing_give_up_and_store_failures_surface() {
    let s = Hooked::new();
    let m = members(&s, config(30, 120));
    s.conflict_on(Some("member-id"));
    assert!(matches!(
        m.register(&node(1)),
        Err(MembershipError::Contended)
    ));
    s.conflict_on(None);
    let a = m.register(&node(1)).expect("a");

    for (prefix, call, who) in [
        ("boot/", "assign", 1),
        ("member/", "write_row", 1),
        // A new member changes the map; a known one that is unchanged would not write it.
        ("map", "refresh", 2),
    ] {
        s.conflict_on(Some(prefix));
        let e = m.register(&node(who)).expect_err(call);
        assert!(matches!(e, MembershipError::Contended), "{call}: {e}");
    }
    s.conflict_on(Some("member/"));
    assert!(matches!(
        m.heartbeat(a.id, a.boot, Progress::CaughtUp, Debts::Unknown),
        Err(MembershipError::Contended)
    ));
    s.conflict_on(None);

    s.fail_gets.store(1, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(m.current_map(), Err(MembershipError::Store(_))));
}

/// Catches: a map source that is not the map: it must say there is no map (and generation 0)
/// before any member exists, answer with the current map, and wake its receivers on every bump this frontend
/// commits (registration, a state change, expiry) and on one another frontend committed, once
/// this one next refreshes.
#[test]
fn the_map_source_follows_every_bump() {
    let s = Hooked::new();
    let m = Arc::new(members(&s, config(30, 120)));
    let source = m.map_source().expect("source");
    let mut changes = source.changes();
    assert!(source.current().is_none(), "no member, no map");
    assert_eq!(*changes.borrow_and_update(), 0);
    let a = m.register(&node(1)).expect("a");
    assert!(changes.has_changed().expect("open"), "the first map wakes");
    assert_eq!(*changes.borrow_and_update(), 1);
    assert_eq!(*source.current().expect("map"), a.map);

    let promoted = m.set_state(a.id, MemberState::Draining).expect("drain");
    assert!(changes.has_changed().expect("open"));
    assert_eq!(*changes.borrow_and_update(), promoted.generation());
    assert_eq!(*source.current().expect("map"), promoted);

    let elsewhere = other_frontend(&s, config(30, 120));
    let b = elsewhere
        .register(&node(2))
        .expect("b via another frontend");
    assert!(!changes.has_changed().expect("open"), "not seen yet");
    m.refresh().expect("refresh");
    assert_eq!(*changes.borrow_and_update(), b.map.generation());
    assert_eq!(*source.current().expect("map"), b.map);

    s.clock.advance(31 * SECOND);
    let down = m.refresh().expect("refresh").expect("map");
    assert_eq!(*changes.borrow_and_update(), down.generation());
}

/// Catches a view that forgets the generation a bump replaced (a blob written before a
/// join would be looked for only where the new map puts it), one that keeps a map that is
/// not older, and one that is a map the frontend has not seen: each bump moves the newest
/// map to `previous`, a refresh that changes nothing moves nothing, and there is no view
/// before the first member.
#[test]
fn the_view_keeps_the_generation_a_bump_replaced() {
    let s = Hooked::new();
    let m = members(&s, config(30, 120));
    assert!(m.view().is_none(), "no member, no map");
    let a = m.register(&node(1)).expect("a");
    let (latest, previous) = m.view().expect("a view");
    assert_eq!(*latest, a.map);
    assert!(previous.is_none());

    let b = m.register(&node(2)).expect("b");
    let (latest, previous) = m.view().expect("a view");
    assert_eq!(*latest, b.map);
    assert_eq!(previous.as_deref(), Some(&a.map));

    // Nothing changed: the same two maps.
    m.refresh().expect("refresh");
    m.current_map().expect("read");
    let (latest, previous) = m.view().expect("a view");
    assert_eq!(*latest, b.map);
    assert_eq!(previous.as_deref(), Some(&a.map));

    // A bump another frontend made is seen at a read, and replaces the newest.
    let c = other_frontend(&s, config(30, 120))
        .register(&node(3))
        .expect("c via another frontend");
    m.current_map().expect("read");
    let (latest, previous) = m.view().expect("a view");
    assert_eq!(*latest, c.map);
    assert_eq!(previous.as_deref(), Some(&b.map));
}

/// Catches a map source of a restarted frontend that starts with no map although its
/// members left one (a daemon welcomed with no map until the first refresh), and one that
/// hides a store that cannot be read (a frontend that serves an empty map as if it were
/// the farm's).
#[test]
fn a_map_source_is_seeded_from_the_store() {
    let s = Hooked::new();
    let before = Arc::new(members(&s, config(30, 120)));
    let a = before.register(&node(1)).expect("a");
    let after = Arc::new(other_frontend(&s, config(30, 120)));
    let source = after.map_source().expect("source");
    assert_eq!(*source.current().expect("the stored map"), a.map);
    assert_eq!(*source.changes().borrow(), a.map.generation());

    s.fail_gets.store(1, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(
        before.map_source().err(),
        Some(MembershipError::Store(_))
    ));
}

fn states(map: &Map) -> Vec<MemberState> {
    map.members().iter().map(|m| m.state).collect()
}

/// Catches: a registration that says the index is whole being written anything but
/// Active, in a fresh cluster and in a formed one (a Joining member is never rank 1), and
/// a registration that does not bump the map: three nodes register in a row; every one is
/// Active in the map its registration returned, and each advanced the generation. The
/// cluster is formed from the first, so the second and third are the formed case.
/// Mutant: `CaughtUp` mapped back to `Joining` in `register` turns this red.
#[test]
fn a_caught_up_registration_is_active_in_a_formed_cluster() {
    let s = Hooked::new();
    let m = members(&s, config(30, 120));
    let mut generation = 0;
    for i in 1..=3 {
        let r = m.register(&node(i)).expect("register");
        assert_eq!(state(&r.map, r.id), Some(MemberState::Active), "node {i}");
        assert!(r.map.generation() > generation, "node {i} advanced the map");
        generation = r.map.generation();
    }
    let map = m.current_map().expect("read").expect("a map");
    assert_eq!(states(&map), vec![MemberState::Active; 3]);
    assert_eq!(map.generation(), 3);
}

/// Catches: a restarted Active node (same instance, new boot, index intact so it reports
/// CaughtUp) left Joining, or demoted, or the restart costing a generation: the node
/// re-registers while its old row is alive and again after the row lapsed and the node
/// is Down; both times it is Active with the same id and a larger boot, and only the
/// lapse-and-return moves the map. Mutant: `CaughtUp` mapped to `Joining` turns this red.
#[test]
fn a_restarted_active_member_is_active_again() {
    let s = Hooked::new();
    let m = members(&s, config(30, 120));
    let a = m.register(&node(1)).expect("a");
    let b = m.register(&node(2)).expect("b");
    let g = b.map.generation();

    let quick = m.register(&node(1)).expect("quick restart");
    assert_eq!((quick.id, quick.boot), (a.id, a.boot + 1));
    assert_eq!(state(&quick.map, a.id), Some(MemberState::Active));
    assert_eq!(quick.map.generation(), g, "nothing changed in the map");

    s.clock.advance(20 * SECOND);
    m.heartbeat(b.id, b.boot, Progress::CaughtUp, Debts::Unknown)
        .expect("b");
    s.clock.advance(11 * SECOND);
    let down = m.refresh().expect("refresh").expect("map");
    assert_eq!(state(&down, a.id), Some(MemberState::Down));
    let back = m.register(&node(1)).expect("back");
    assert_eq!((back.id, back.boot), (a.id, quick.boot + 1));
    assert_eq!(state(&back.map, a.id), Some(MemberState::Active));
    assert_eq!(back.map.generation(), down.generation() + 1);
}

/// Catches: a rolling restart that leaves members Joining: three members register, then
/// each in turn registers again (a new boot, CaughtUp, as a node with its index intact
/// does), and after every step all three are Active. Mutant: `CaughtUp` mapped to
/// `Joining` turns this red at the first re-registration.
#[test]
fn a_rolling_restart_leaves_every_member_active() {
    let s = Hooked::new();
    let m = members(&s, config(30, 120));
    for i in 1..=3 {
        m.register(&node(i)).expect("first boot");
    }
    for i in 1..=3 {
        let r = m.register(&node(i)).expect("restart");
        assert_eq!(r.boot, 2);
        assert_eq!(
            states(&r.map),
            vec![MemberState::Active; 3],
            "after restarting node {i}"
        );
    }
}

/// Catches: a node that says it is rebuilding being made Active on arrival (its index is
/// not whole), and a cluster not marked formed when its first Active member comes from a
/// caught-up beat instead of a registration: the catching-up registration stays
/// CatchingUp and writes no formed mark; its CaughtUp beat makes it Active and writes the
/// mark. Mutant: the `FORMED` put dropped from `touch` turns this red.
#[test]
fn a_catching_up_registration_becomes_active_on_its_caught_up_beat() {
    let s = Hooked::new();
    let m = members(&s, config(30, 120));
    let rebuilding = Registrant {
        progress: Progress::CatchingUp,
        ..node(1)
    };
    let a = m.register(&rebuilding).expect("a");
    assert_eq!(state(&a.map, a.id), Some(MemberState::CatchingUp));
    assert_eq!(s.fake.get("meta/members_formed").expect("read"), None);
    m.heartbeat(a.id, a.boot, Progress::CaughtUp, Debts::Unknown)
        .expect("beat");
    let map = m.current_map().expect("read").expect("a map");
    assert_eq!(state(&map, a.id), Some(MemberState::Active));
    assert!(s.fake.get("meta/members_formed").expect("read").is_some());
}

/// Catches: `Down` being settable by hand (it is the store's verdict).
#[test]
fn down_is_not_settable() {
    let s = Hooked::new();
    let m = members(&s, config(30, 120));
    let a = m.register(&node(1)).expect("a");
    assert!(matches!(
        m.set_state(a.id, MemberState::Down),
        Err(MembershipError::NotSettable(MemberState::Down))
    ));
}
