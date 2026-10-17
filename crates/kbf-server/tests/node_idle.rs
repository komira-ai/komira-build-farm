//! When a storage node may be stopped (`kbf_server::storage_health::node_idle`): the
//! frontend's judgement over what each member's heartbeat reports about its copies owed and
//! its open writes.
//!
//! The pure judgement is tested on its own, then over the membership rows (the fake store
//! with its injectable clock), and in the scrape.
//!
//! Catches, in one line each: a value nobody reported (never, stale, or from an older node
//! that does not count) read as zero; a count of writes that is not the node's own; a debt
//! anywhere in the farm ignored; another member that is not `Active` ignored; a count taken
//! before the node held the map that stopped writes to it; a report that does not survive a
//! frontend restart; a gauge exported for a value that is unknown.

#[path = "support/hooked.rs"]
mod hooked;

use std::sync::Arc;
use std::time::Duration;

use hooked::Hooked;
use kbf_map::{MemberId, MemberState};
use kbf_server::members::{Members, MembershipConfig, PROTOCOL, Progress, Registrant};
use kbf_server::storage_health::{
    Class, Debts, MemberReport, NotIdle, Report, Reported, Writes, node_idle, render,
    storage_health,
};
use kbf_state::{Store, StoreTime};

const FRESH: Duration = Duration::from_secs(20);

fn at(seconds: u64) -> StoreTime {
    StoreTime::from_duration(Duration::from_secs(seconds))
}

/// A report of `debts` and `writes` from a node holding map `generation`, received at `seconds`.
fn said(debts: Debts, writes: Writes, generation: u64, seconds: u64) -> Option<Report> {
    Some(Report {
        said: Reported::debts(debts).with_writes(writes, generation),
        at: at(seconds),
    })
}

fn member(id: u64, state: MemberState, report: Option<Report>) -> MemberReport {
    MemberReport {
        id: MemberId(id),
        state,
        report,
    }
}

/// An `Active` member that owes nothing, has no write open and held map 5, a moment ago.
fn quiet(id: u64) -> MemberReport {
    member(
        id,
        MemberState::Active,
        said(Debts::Known(0), Writes::Known(0), 5, 100),
    )
}

fn idle(members: &[MemberReport], node: u64, generation: u64) -> Vec<NotIdle> {
    node_idle(members, at(100), FRESH, MemberId(node), generation)
}

// Catches: a node that is not idle called idle, and one that is called busy. A farm of
// three quiet members is idle at the map the node held; each change below is one reason.
// Mutants (planted locally): `Unknown` writes read as zero; the node's own debts left out of
// the total; the generation not compared.
#[test]
fn a_quiet_node_in_a_quiet_farm_is_idle_and_each_unknown_or_busy_value_is_a_reason() {
    let farm = [quiet(1), quiet(2), quiet(3)];
    assert_eq!(idle(&farm, 1, 5), []);
    assert_eq!(idle(&farm, 1, 0), [], "no generation asked");

    let with =
        |report: Option<Report>| [member(1, MemberState::Active, report), quiet(2), quiet(3)];
    let node = MemberId(1);
    let cases = [
        (
            "writes open",
            said(Debts::Known(0), Writes::Known(3), 5, 100),
            NotIdle::OpenWrites { member: node, n: 3 },
        ),
        (
            "writes not counted (an older node)",
            said(Debts::Known(0), Writes::Unknown, 5, 100),
            NotIdle::OpenWritesUnknown { member: node },
        ),
        (
            "debts not known",
            said(Debts::Unknown, Writes::Known(0), 5, 100),
            NotIdle::DebtsUnknown { member: node },
        ),
        (
            "its own debts",
            said(Debts::Known(2), Writes::Known(0), 5, 100),
            NotIdle::CopiesOwed { n: 2 },
        ),
        (
            "an older map than the one that stopped writes",
            said(Debts::Known(0), Writes::Known(0), 4, 100),
            NotIdle::ReportOlderThan {
                member: node,
                reported: 4,
                wanted: 5,
            },
        ),
        (
            "a report older than the freshness window",
            said(Debts::Known(0), Writes::Known(0), 5, 79),
            NotIdle::NodeStale { member: node },
        ),
        (
            "no report at all",
            None,
            NotIdle::NodeStale { member: node },
        ),
    ];
    for (what, report, reason) in cases {
        assert_eq!(idle(&with(report), 1, 5), [reason], "{what}");
    }
    let at_the_edge = said(Debts::Known(0), Writes::Known(0), 5, 80);
    assert_eq!(
        idle(&with(at_the_edge), 1, 5),
        [],
        "20 s old is still fresh"
    );
}

// Catches: the rest of the farm ignored. Another member that owes, is not Active, is silent
// or does not know its debts keeps the node busy; another member's open writes are not the
// node's; a node alone is not idle (no other copy); a stranger is unknown.
#[test]
fn the_rest_of_the_farm_must_hold_the_nodes_blobs() {
    let busy_elsewhere = member(
        2,
        MemberState::Active,
        said(Debts::Known(0), Writes::Known(9), 1, 100),
    );
    assert_eq!(
        idle(&[quiet(1), busy_elsewhere], 1, 5),
        [],
        "another member's writes and map are its own"
    );

    let owes = member(
        2,
        MemberState::Active,
        said(Debts::Known(4), Writes::Known(0), 5, 100),
    );
    assert_eq!(
        idle(&[quiet(1), owes], 1, 5),
        [NotIdle::CopiesOwed { n: 4 }]
    );
    let silent = member(
        2,
        MemberState::Active,
        said(Debts::Known(0), Writes::Known(0), 5, 50),
    );
    assert_eq!(
        idle(&[quiet(1), silent], 1, 5),
        [NotIdle::OtherMemberStale {
            member: MemberId(2)
        }]
    );
    let blind = member(
        2,
        MemberState::Active,
        said(Debts::Unknown, Writes::Known(0), 5, 100),
    );
    assert_eq!(
        idle(&[quiet(1), blind], 1, 5),
        [NotIdle::OtherMemberDebtsUnknown {
            member: MemberId(2)
        }]
    );
    for state in [
        MemberState::Joining,
        MemberState::CatchingUp,
        MemberState::Draining,
        MemberState::Down,
    ] {
        let other = member(2, state, None);
        assert_eq!(
            idle(&[quiet(1), other], 1, 5),
            [NotIdle::OtherMemberNotActive {
                member: MemberId(2)
            }],
            "{state:?}"
        );
    }

    assert_eq!(
        idle(&[quiet(1)], 1, 5),
        [NotIdle::NoOtherMembers],
        "alone: no other copy"
    );
    assert_eq!(
        idle(&[quiet(1), quiet(2)], 7, 5),
        [NotIdle::UnknownNode {
            member: MemberId(7)
        }]
    );
    let joining = member(
        1,
        MemberState::CatchingUp,
        said(Debts::Known(0), Writes::Known(0), 5, 100),
    );
    assert_eq!(
        idle(&[joining, quiet(2)], 1, 5),
        [NotIdle::NodeNotActive {
            member: MemberId(1),
            state: MemberState::CatchingUp
        }]
    );
}

/// The position of a reason in the table below. Exhaustive on purpose: a new reason fails to
/// compile here until the table has a case for it.
fn position(reason: &NotIdle) -> usize {
    match reason {
        NotIdle::UnknownNode { .. } => 0,
        NotIdle::NoOtherMembers => 1,
        NotIdle::OtherMemberNotActive { .. } => 2,
        NotIdle::OtherMemberStale { .. } => 3,
        NotIdle::OtherMemberDebtsUnknown { .. } => 4,
        NotIdle::CopiesOwed { .. } => 5,
        NotIdle::NodeNotActive { .. } => 6,
        NotIdle::NodeStale { .. } => 7,
        NotIdle::DebtsUnknown { .. } => 8,
        NotIdle::OpenWritesUnknown { .. } => 9,
        NotIdle::OpenWrites { .. } => 10,
        NotIdle::ReportOlderThan { .. } => 11,
        NotIdle::Unreadable(_) => 12,
    }
}

// Catches: a reason that is never produced, worded wrongly, or classed wrongly. Every
// variant is made by the judgement from the one situation that makes it (the first reason,
// with the node's own report fine unless it is the cause), says what an operator waits for,
// and is a Wait (it passes by itself) or a Held (the farm is not fit for the node to go).
// A controller waits on the first and holds on the second, so a Held read as a Wait would
// restart a node beside a second member that is not active.
#[test]
fn every_reason_is_made_worded_and_classed() {
    let cause = |report| vec![member(1, MemberState::Active, report), quiet(2)];
    let own = |debts, writes, generation| said(debts, writes, generation, 100);
    let ok = || own(Debts::Known(0), Writes::Known(0), 5);
    let other = |state, report| vec![quiet(1), member(2, state, report)];
    let cases = vec![
        (
            vec![quiet(1), quiet(2)],
            7,
            5,
            0,
            "member 7 is not registered",
            Class::Held,
        ),
        (
            vec![quiet(1)],
            1,
            5,
            1,
            "no other storage member is registered",
            Class::Held,
        ),
        (
            other(MemberState::Down, None),
            1,
            5,
            2,
            "member 2 not active",
            Class::Held,
        ),
        (
            other(
                MemberState::Active,
                own(Debts::Known(0), Writes::Known(0), 5).map(|r| Report { at: at(50), ..r }),
            ),
            1,
            5,
            3,
            "member 2 silent",
            Class::Held,
        ),
        (
            other(
                MemberState::Active,
                own(Debts::Unknown, Writes::Known(0), 5),
            ),
            1,
            5,
            4,
            "member 2 does not know what it owes",
            Class::Wait,
        ),
        (
            cause(own(Debts::Known(2), Writes::Known(0), 5)),
            1,
            5,
            5,
            "2 blobs owed",
            Class::Wait,
        ),
        (
            vec![member(1, MemberState::Draining, ok()), quiet(2)],
            1,
            5,
            6,
            "member 1 is draining, not active",
            Class::Held,
        ),
        (cause(None), 1, 5, 7, "member 1 silent", Class::Wait),
        (
            cause(own(Debts::Unknown, Writes::Known(0), 5)),
            1,
            5,
            8,
            "member 1 does not know what it owes",
            Class::Wait,
        ),
        (
            cause(own(Debts::Known(0), Writes::Unknown, 5)),
            1,
            5,
            9,
            "member 1 does not count its open writes",
            Class::Wait,
        ),
        (
            cause(own(Debts::Known(0), Writes::Known(2), 5)),
            1,
            5,
            10,
            "member 1 has 2 open writes",
            Class::Wait,
        ),
        (
            cause(own(Debts::Known(0), Writes::Known(0), 4)),
            1,
            6,
            11,
            "member 1 reported at map 4, before map 6",
            Class::Wait,
        ),
    ];
    let mut made = Vec::new();
    for (members, who, generation, want, text, class) in cases {
        let reasons = idle(&members, who, generation);
        let first = reasons.first().unwrap_or_else(|| panic!("{text}: idle"));
        assert_eq!(position(first), want, "{text}: {reasons:?}");
        assert_eq!(first.to_string(), text);
        assert_eq!(first.class(), class, "{text}");
        made.push(position(first));
    }

    let unreadable = NotIdle::Unreadable("x".to_owned());
    assert_eq!(unreadable.to_string(), "membership unreadable: x");
    assert_eq!(unreadable.class(), Class::Held);
    made.push(position(&unreadable));
    made.sort_unstable();
    assert_eq!(made, (0..13).collect::<Vec<_>>(), "a reason has no case");
}

// Catches: the per-node view showing a value that is not fresh and known, and showing zero
// for one that is not. The view lists every member in any state.
#[test]
fn the_node_view_has_a_number_only_where_the_node_said_one() {
    let members = [
        quiet(1),
        member(
            2,
            MemberState::Active,
            said(Debts::Known(6), Writes::Unknown, 3, 100),
        ),
        member(
            3,
            MemberState::CatchingUp,
            said(Debts::Unknown, Writes::Known(4), 3, 100),
        ),
        member(
            4,
            MemberState::Active,
            said(Debts::Known(1), Writes::Known(1), 3, 10),
        ),
        member(5, MemberState::Down, None),
    ];
    let health = storage_health(&members, at(100), FRESH);
    let view: Vec<_> = health
        .nodes
        .iter()
        .map(|n| (n.id.0, n.repair_debts, n.open_writes, n.generation))
        .collect();
    assert_eq!(
        view,
        [
            (1, Some(0), Some(0), Some(5)),
            (2, Some(6), None, Some(3)),
            (3, None, Some(4), Some(3)),
            (4, None, None, None),
            (5, None, None, None),
        ]
    );
}

fn scrape(members: &[MemberReport]) -> String {
    let health = storage_health(members, at(100), FRESH);
    let mut out = String::new();
    render(&mut out, Some(&health));
    out
}

// Catches: a gauge for a value nobody reported. Member 2 owes 6 and has no count; member 3
// has a count and no debts; each appears only in its own family, with a member label. With
// nothing known, neither family is there at all, not even as a zero.
#[test]
fn the_scrape_has_a_sample_per_member_only_for_what_is_known() {
    let text = scrape(&[
        quiet(1),
        member(
            2,
            MemberState::Active,
            said(Debts::Known(6), Writes::Unknown, 3, 100),
        ),
        member(
            3,
            MemberState::CatchingUp,
            said(Debts::Unknown, Writes::Known(4), 3, 100),
        ),
    ]);
    for line in [
        "kbf_member_repair_debts{member=\"1\"} 0\n",
        "kbf_member_repair_debts{member=\"2\"} 6\n",
        "kbf_member_open_writes{member=\"1\"} 0\n",
        "kbf_member_open_writes{member=\"3\"} 4\n",
        "# TYPE kbf_member_open_writes gauge\n",
        "# TYPE kbf_member_repair_debts gauge\n",
    ] {
        assert!(text.contains(line), "{line:?} missing from {text}");
    }
    for line in [
        "kbf_member_repair_debts{member=\"3\"}",
        "kbf_member_open_writes{member=\"2\"}",
    ] {
        assert!(!text.contains(line), "{line:?} in {text}");
    }

    let nothing = scrape(&[member(1, MemberState::Active, None)]);
    assert!(!nothing.contains("kbf_member_"), "{nothing}");
    let mut unreadable = String::new();
    render(&mut unreadable, None);
    assert!(!unreadable.contains("kbf_member_"), "{unreadable}");
}

// ---- over the membership rows ----

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

/// Two `Active` members and a store whose clock a test moves.
struct Pair {
    store: Arc<Hooked>,
    members: Members,
    a: (MemberId, u64),
    b: (MemberId, u64),
}

impl Pair {
    fn new() -> Self {
        let store = Hooked::new();
        let members = Members::new(Arc::clone(&store) as Arc<dyn Store>, config());
        let a = members.register(&registrant(1)).expect("a");
        let b = members.register(&registrant(2)).expect("b");
        Self {
            members,
            a: (a.id, a.boot),
            b: (b.id, b.boot),
            store,
        }
    }

    fn beat(&self, who: (MemberId, u64), said: Reported) {
        self.members
            .heartbeat(who.0, who.1, Progress::CaughtUp, said)
            .expect("beats");
    }
}

fn quiet_report(generation: u64) -> Reported {
    Reported::debts(Debts::Known(0)).with_writes(Writes::Known(0), generation)
}

// Catches: the heartbeat's counts not reaching the judgement, and the gate not following
// them. Writes open on a: it is not idle; they end: it is. Debts count up and down: a's two
// copies owed show per node and in the total, b's one adds to it, and settling brings each
// back. Mutants: the writes read from the wrong field; the debts summed without the node's
// own.
#[test]
fn open_writes_and_repair_debt_count_up_and_down() {
    let pair = Pair::new();
    let (a, b) = (pair.a.0, pair.b.0);
    pair.beat(pair.b, quiet_report(4));
    let shown = |pair: &Pair| {
        let health = pair.members.storage_health().expect("health");
        let by_node: Vec<(Option<u64>, Option<u64>)> = health
            .nodes
            .iter()
            .map(|n| (n.repair_debts, n.open_writes))
            .collect();
        (by_node, health.under_replicated_blobs)
    };

    pair.beat(
        pair.a,
        Reported::debts(Debts::Known(2)).with_writes(Writes::Known(3), 4),
    );
    assert_eq!(
        shown(&pair),
        (vec![(Some(2), Some(3)), (Some(0), Some(0))], Some(2))
    );
    assert_eq!(
        pair.members.node_idle(a, 4),
        Err(NotIdle::CopiesOwed { n: 2 })
    );
    pair.beat(
        pair.a,
        Reported::debts(Debts::Known(0)).with_writes(Writes::Known(3), 4),
    );
    assert_eq!(
        pair.members.node_idle(a, 4),
        Err(NotIdle::OpenWrites { member: a, n: 3 })
    );
    pair.beat(
        pair.a,
        Reported::debts(Debts::Known(2)).with_writes(Writes::Known(3), 4),
    );

    pair.beat(
        pair.b,
        Reported::debts(Debts::Known(1)).with_writes(Writes::Known(0), 4),
    );
    assert_eq!(
        shown(&pair),
        (vec![(Some(2), Some(3)), (Some(1), Some(0))], Some(3))
    );

    pair.beat(
        pair.a,
        Reported::debts(Debts::Known(0)).with_writes(Writes::Known(1), 4),
    );
    assert_eq!(
        shown(&pair),
        (vec![(Some(0), Some(1)), (Some(1), Some(0))], Some(1))
    );
    // The copy b owes is named before a's one open write.
    assert_eq!(
        pair.members.node_idle(a, 4),
        Err(NotIdle::CopiesOwed { n: 1 }),
        "the newest report replaces the last"
    );

    pair.beat(pair.a, quiet_report(4));
    assert_eq!(
        pair.members.node_idle(a, 4),
        Err(NotIdle::CopiesOwed { n: 1 }),
        "b still owes a copy"
    );
    pair.beat(pair.b, quiet_report(4));
    assert_eq!(shown(&pair), (vec![(Some(0), Some(0)); 2], Some(0)));
    assert_eq!(pair.members.node_idle(a, 4), Ok(()));
    assert_eq!(pair.members.node_idle(b, 4), Ok(()));
    assert_eq!(
        pair.members.node_idle(a, 5),
        Err(NotIdle::ReportOlderThan {
            member: a,
            reported: 4,
            wanted: 5
        })
    );
}

// Catches: a gate that reads "never reported" or "reported long ago" as idle, in the rows.
// The window is two heartbeats; time moves only as the test moves it.
#[test]
fn a_node_that_has_not_reported_lately_is_not_idle() {
    let pair = Pair::new();
    pair.beat(pair.b, quiet_report(1));
    assert_eq!(
        pair.members.node_idle(pair.a.0, 0),
        Err(NotIdle::NodeStale { member: pair.a.0 }),
        "registered, never reported"
    );
    pair.beat(pair.a, quiet_report(1));
    assert_eq!(pair.members.node_idle(pair.a.0, 1), Ok(()));
    pair.store.clock.advance(Duration::from_secs(21));
    pair.beat(pair.b, quiet_report(1));
    assert_eq!(
        pair.members.node_idle(pair.a.0, 1),
        Err(NotIdle::NodeStale { member: pair.a.0 })
    );
}

// Catches: a report held only in memory. A frontend that restarts (a new `Members` over the
// same store) still judges the node by what it last said, and a node that registers again
// (a restart of the node) starts without one, so it is not idle until it reports.
#[test]
fn the_reports_live_in_the_rows_and_a_registration_clears_them() {
    let pair = Pair::new();
    pair.beat(
        pair.a,
        Reported::debts(Debts::Known(0)).with_writes(Writes::Known(2), 9),
    );
    pair.beat(pair.b, quiet_report(9));

    let restarted = Members::new(Arc::clone(&pair.store) as Arc<dyn Store>, config());
    let health = restarted.storage_health().expect("health");
    assert_eq!(health.nodes[0].open_writes, Some(2));
    assert_eq!(health.nodes[0].generation, Some(9));
    assert_eq!(
        restarted.node_idle(pair.a.0, 9),
        Err(NotIdle::OpenWrites {
            member: pair.a.0,
            n: 2
        })
    );

    let again = restarted.register(&registrant(1)).expect("registers again");
    assert_eq!(
        restarted.node_idle(again.id, 0),
        Err(NotIdle::NodeStale { member: again.id }),
        "the old count is not the new process's"
    );
}

// Catches: a membership that cannot be read called idle, or its error lost. A stored map that
// does not decode is `Unreadable`, and a controller holds on it.
#[test]
fn a_membership_that_cannot_be_read_is_unreadable_and_held() {
    let pair = Pair::new();
    pair.beat(pair.a, quiet_report(1));
    pair.beat(pair.b, quiet_report(1));
    pair.store
        .fake
        .txn(kbf_state::Txn::new().put("map", vec![0xff], kbf_state::Cond::Any))
        .expect("store")
        .expect("commit");
    let refused = pair.members.node_idle(pair.a.0, 0).expect_err("unreadable");
    assert!(matches!(&refused, NotIdle::Unreadable(why) if why.contains("unreadable")));
    assert_eq!(refused.class(), Class::Held);
}
