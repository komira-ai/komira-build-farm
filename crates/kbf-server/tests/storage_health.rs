//! Storage health (`kbf_server::storage_health`): the number of blobs held once that
//! should be held twice, and the cases where nobody can say.
//!
//! The pure judgement is tested on its own, then over the membership rows (the fake store
//! with its injectable clock, so no test waits for time), the wire's mapping of a report,
//! and the two metric families.
//!
//! Catches, in one line each: an empty sum read as zero; a member that is down, joining,
//! catching up or draining left out of the count; a report kept after the member went
//! quiet; a node that does not know its debts counted as owing none; only some members'
//! debts summed; a report that outlives its row or survives a re-registration; a gauge of
//! zero exported when the number is unknown.

#[path = "support/hooked.rs"]
mod hooked;

use std::sync::Arc;
use std::time::Duration;

use hooked::Hooked;
use kbf_map::{MemberId, MemberState};
use kbf_server::members::{
    Members, MembershipConfig, MembershipError, PROTOCOL, Progress, Registrant,
};
use kbf_server::storage_health::{
    Debts, MemberReport, NotReady, Report, Reported, StorageGate, StorageHealth, render,
    storage_health,
};
use kbf_state::{Store, StoreTime};

const SECOND: Duration = Duration::from_secs(1);
const FRESH: Duration = Duration::from_secs(20);

fn at(seconds: u64) -> StoreTime {
    StoreTime::from_duration(Duration::from_secs(seconds))
}

fn reported(id: u64, state: MemberState, debts: Debts, seconds: u64) -> MemberReport {
    MemberReport {
        id: MemberId(id),
        state,
        planned: kbf_server::storage_health::Planned::No,
        report: Some(Report {
            said: Reported::debts(debts),
            at: at(seconds),
        }),
    }
}

fn active(id: u64, debts: u64) -> MemberReport {
    reported(id, MemberState::Active, Debts::Known(debts), 100)
}

fn judge(members: &[MemberReport]) -> StorageHealth {
    storage_health(members, at(100), FRESH)
}

// Catches: a sum that is not the sum (only the first member's, only the largest, or the
// last report overwriting), and a farm that owes nothing not being ready.
#[test]
fn every_member_active_and_reporting_is_the_sum_of_their_debts() {
    let none = judge(&[active(1, 0), active(2, 0), active(3, 0)]);
    assert_eq!(none.under_replicated_blobs, Some(0));
    assert_eq!(none.ready(), Ok(()));
    assert!(none.reasons().is_empty());
    assert_eq!((none.members_total, none.members_active), (3, 3));

    // The first member alone would say 0, the largest alone 3.
    let owed = judge(&[active(1, 0), active(2, 3), active(3, 0)]);
    assert_eq!(owed.under_replicated_blobs, Some(3));
    assert_eq!(owed.ready(), Err(NotReady::Owed(3)));

    let several = judge(&[active(1, 2), active(2, 3), active(3, 1)]);
    assert_eq!(several.under_replicated_blobs, Some(6));
    assert_eq!(several.debts_total, 6);
}

// Catches: a member that is not active ignored. A member that is down holds blobs nobody
// counts: with every other member owing nothing the number must still be unknown, which is
// the case the gate exists for.
#[test]
fn a_member_that_is_down_makes_the_number_unknown_however_little_the_others_owe() {
    let down = MemberReport {
        planned: kbf_server::storage_health::Planned::No,
        id: MemberId(2),
        state: MemberState::Down,
        report: None,
    };
    let health = judge(&[active(1, 0), down, active(3, 0)]);
    assert_eq!(health.under_replicated_blobs, None);
    assert_eq!(health.not_active, [MemberId(2)]);
    assert_eq!(
        health.ready(),
        Err(NotReady::NotActive {
            member: MemberId(2)
        })
    );
    assert_eq!((health.members_total, health.members_active), (3, 2));
}

// Catches: a joiner, a catching-up member or a draining one counted as if active, even
// with a fresh report of no debts.
#[test]
fn a_member_that_is_not_active_is_never_counted_whatever_it_reports() {
    for state in [
        MemberState::Joining,
        MemberState::CatchingUp,
        MemberState::Draining,
        MemberState::Down,
    ] {
        let health = judge(&[active(1, 0), reported(2, state, Debts::Known(0), 100)]);
        assert_eq!(health.under_replicated_blobs, None, "{state:?}");
        assert_eq!(health.not_active, [MemberId(2)], "{state:?}");
        assert_eq!(health.members_active, 1, "{state:?}");
    }
}

// Catches: a report kept for ever (the freshness test dropped), and an off-by-one at the
// window: a report exactly as old as the window still counts, one nanosecond older does
// not; no report at all is silent too.
#[test]
fn a_member_that_stopped_reporting_is_silent() {
    let on_the_edge = reported(2, MemberState::Active, Debts::Known(0), 80);
    assert_eq!(
        judge(&[active(1, 0), on_the_edge]).under_replicated_blobs,
        Some(0)
    );

    let mut stale = on_the_edge;
    stale.report = stale.report.map(|r| Report {
        at: StoreTime::from_duration(79 * SECOND + Duration::from_nanos(999_999_999)),
        ..r
    });
    let health = judge(&[active(1, 0), stale]);
    assert_eq!(health.under_replicated_blobs, None);
    assert_eq!(health.silent, [MemberId(2)]);
    assert_eq!(
        health.ready(),
        Err(NotReady::Silent {
            member: MemberId(2)
        })
    );

    let never = MemberReport {
        planned: kbf_server::storage_health::Planned::No,
        report: None,
        ..on_the_edge
    };
    assert_eq!(judge(&[active(1, 0), never]).silent, [MemberId(2)]);
}

// Catches: a node that does not know its debts counted as owing none (a node that has just
// restarted must not claim zero), however the count it sent alongside looks.
#[test]
fn a_member_that_does_not_know_its_debts_is_not_a_zero() {
    let blind = reported(2, MemberState::Active, Debts::Unknown, 100);
    let health = judge(&[active(1, 0), blind]);
    assert_eq!(health.under_replicated_blobs, None);
    assert_eq!(health.unreported, [MemberId(2)]);
    assert_eq!(
        health.ready(),
        Err(NotReady::Unreported {
            member: MemberId(2)
        })
    );
    assert_eq!(Debts::from_wire(false, 0), Debts::Unknown);
    assert_eq!(Debts::from_wire(false, 7), Debts::Unknown);
    assert_eq!(Debts::from_wire(true, 7), Debts::Known(7));
    assert_eq!(Debts::Known(7).to_wire(), (true, 7));
    assert_eq!(Debts::Unknown.to_wire(), (false, 0));
}

// Catches: an empty sum read as zero. With nobody registered nothing is known.
#[test]
fn no_member_is_unknown_not_zero() {
    let health = judge(&[]);
    assert_eq!(health.under_replicated_blobs, None);
    assert_eq!(health.members_total, 0);
    assert_eq!(health.ready(), Err(NotReady::NoMembers));
}

// Catches: what the members who did say being dropped when others did not (an operator
// reads the debts among those who answered), and a sum that wraps.
#[test]
fn the_debts_of_those_who_answered_are_shown_when_the_total_is_unknown() {
    let down = reported(3, MemberState::Down, Debts::Known(9), 100);
    let health = judge(&[active(1, 4), active(2, 2), down]);
    assert_eq!(health.under_replicated_blobs, None);
    assert_eq!(health.debts_total, 6);

    let huge = judge(&[active(1, u64::MAX), active(2, 1)]);
    assert_eq!(huge.under_replicated_blobs, Some(u64::MAX));
}

// Catches: the reasons losing members or their order, and a message that does not name
// the member or the count. Every reason is listed; the first is the one `ready` returns.
#[test]
fn every_reason_is_listed_and_named() {
    let down = reported(3, MemberState::Down, Debts::Known(0), 100);
    let silent = MemberReport {
        planned: kbf_server::storage_health::Planned::No,
        report: None,
        ..active(2, 0)
    };
    let blind = reported(4, MemberState::Active, Debts::Unknown, 100);
    let health = judge(&[active(1, 5), silent, down, blind]);
    let said: Vec<String> = health.reasons().iter().map(ToString::to_string).collect();
    assert_eq!(
        said,
        [
            "member 3 not active",
            "member 2 silent",
            "member 4 does not know what it owes"
        ]
    );
    assert_eq!(health.ready().expect_err("not ready").to_string(), said[0]);

    assert_eq!(NotReady::Owed(5).to_string(), "5 blobs owed");
    assert_eq!(
        NotReady::NoMembers.to_string(),
        "no storage member is registered"
    );
    assert_eq!(
        NotReady::Unreadable("x".to_owned()).to_string(),
        "membership unreadable: x"
    );
    assert_eq!(judge(&[active(1, 5)]).reasons(), [NotReady::Owed(5)]);
}

// ---- over the membership rows ----

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: Duration::from_secs(30),
        dwell: Duration::from_secs(120),
        replication: 2,
    }
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

/// Two members, both `Active` and both having just said what they owe.
struct Pair {
    store: Arc<Hooked>,
    members: Members,
    a: (MemberId, u64),
    b: (MemberId, u64),
}

impl Pair {
    fn new(a_owes: Debts, b_owes: Debts) -> Self {
        let store = Hooked::new();
        let members = Members::new(Arc::clone(&store) as Arc<dyn Store>, config());
        let a = members.register(&node(1)).expect("a registers");
        let b = members.register(&node(2)).expect("b registers");
        let pair = Self {
            members,
            a: (a.id, a.boot),
            b: (b.id, b.boot),
            store,
        };
        for id in [pair.a.0, pair.b.0] {
            pair.members
                .set_state(id, MemberState::Active)
                .expect("promoted");
        }
        pair.beat_a(a_owes);
        pair.beat_b(b_owes);
        pair
    }

    fn beat_a(&self, debts: Debts) {
        self.members
            .heartbeat(
                self.a.0,
                self.a.1,
                Progress::CaughtUp,
                Reported::debts(debts),
            )
            .expect("a beats");
    }

    fn beat_b(&self, debts: Debts) {
        self.members
            .heartbeat(
                self.b.0,
                self.b.1,
                Progress::CaughtUp,
                Reported::debts(debts),
            )
            .expect("b beats");
    }

    fn health(&self) -> StorageHealth {
        self.members.storage_health().expect("health")
    }

    fn advance(&self, seconds: u64) {
        self.store.clock.advance(Duration::from_secs(seconds));
    }
}

// Catches: the heartbeat's report not reaching the judgement, or reaching it summed
// wrongly, and the gate not following it. The gate is asked through the trait object a
// rollout will hold.
#[test]
fn heartbeats_give_the_number_and_the_gate_follows_it() {
    let pair = Pair::new(Debts::Known(0), Debts::Known(0));
    let gate: &dyn StorageGate = &pair.members;
    assert_eq!(pair.health().under_replicated_blobs, Some(0));
    assert_eq!(gate.ready(), Ok(()));

    pair.beat_b(Debts::Known(3));
    assert_eq!(pair.health().under_replicated_blobs, Some(3));
    assert_eq!(gate.ready(), Err(NotReady::Owed(3)));

    pair.beat_b(Debts::Known(0));
    assert_eq!(gate.ready(), Ok(()), "the newest report replaces the last");
}

// Catches: a member that went down with no debts reported by anyone else leaving the
// number known: a is Down in the map (its row expired) while b, which owes nothing,
// keeps beating.
#[test]
fn a_member_that_expired_makes_the_number_unknown() {
    let pair = Pair::new(Debts::Known(0), Debts::Known(0));
    pair.advance(20);
    pair.beat_b(Debts::Known(0));
    pair.advance(11);
    pair.members.refresh().expect("refresh");
    let health = pair.health();
    assert_eq!(health.under_replicated_blobs, None);
    assert_eq!(health.not_active, [pair.a.0]);
    assert_eq!(
        pair.members.ready(),
        Err(NotReady::NotActive { member: pair.a.0 })
    );
}

// Catches: a report kept after the member went quiet. The row is still live (the time to
// live is longer than the window), but b has not reported for longer than the window.
// Mutant: the last report kept for ever.
#[test]
fn a_member_whose_row_is_live_but_whose_report_is_old_is_silent() {
    let pair = Pair::new(Debts::Known(0), Debts::Known(0));
    pair.advance(21);
    pair.beat_a(Debts::Known(0));
    let health = pair.health();
    assert_eq!(health.under_replicated_blobs, None);
    assert_eq!(health.silent, [pair.b.0]);
    assert_eq!(health.members_active, 2, "its row has not expired");

    pair.beat_b(Debts::Known(0));
    assert_eq!(pair.health().under_replicated_blobs, Some(0));
}

// Catches: a node that says it cannot say counted as zero.
#[test]
fn a_heartbeat_that_does_not_know_the_debts_is_not_zero() {
    let pair = Pair::new(Debts::Known(0), Debts::Unknown);
    let health = pair.health();
    assert_eq!(health.under_replicated_blobs, None);
    assert_eq!(health.unreported, [pair.b.0]);
}

// Catches: a joiner, catching-up or draining member counted. a is moved through each
// state by the promotion hook while b reports zero.
#[test]
fn a_member_that_is_not_active_in_the_rows_makes_the_number_unknown() {
    let pair = Pair::new(Debts::Known(0), Debts::Known(0));
    for state in [
        MemberState::Joining,
        MemberState::CatchingUp,
        MemberState::Draining,
    ] {
        pair.members.set_state(pair.a.0, state).expect("set");
        let health = pair.health();
        assert_eq!(health.under_replicated_blobs, None, "{state:?}");
        assert_eq!(health.not_active, [pair.a.0], "{state:?}");
    }
    pair.members
        .set_state(pair.a.0, MemberState::Active)
        .expect("set");
    assert_eq!(
        pair.health().under_replicated_blobs,
        Some(0),
        "a promotion keeps the report it had"
    );
}

// Catches: a report that survives a registration. A node that restarts registers again
// with a larger boot counter; whatever it reported before is not what it owes now, so
// even once an administrator has promoted it, it is silent until it reports. Mutant: the
// registration keeps the old row's report.
#[test]
fn a_member_that_registers_again_starts_without_a_report() {
    let pair = Pair::new(Debts::Known(0), Debts::Known(0));
    let again = pair.members.register(&node(1)).expect("registers again");
    assert_eq!((again.id, again.boot), (pair.a.0, pair.a.1 + 1));
    pair.members
        .set_state(pair.a.0, MemberState::Active)
        .expect("promoted");
    let health = pair.health();
    assert_eq!(health.under_replicated_blobs, None);
    assert_eq!(health.silent, [pair.a.0]);

    pair.members
        .heartbeat(
            again.id,
            again.boot,
            Progress::CaughtUp,
            Reported::debts(Debts::Known(0)),
        )
        .expect("beats");
    assert_eq!(pair.health().under_replicated_blobs, Some(0));
}

// Catches: a heartbeat of a boot that was superseded writing its report. The old process
// says zero; the new one has not said anything.
#[test]
fn the_report_of_a_superseded_boot_is_refused() {
    let pair = Pair::new(Debts::Known(0), Debts::Known(0));
    pair.members.register(&node(1)).expect("registers again");
    pair.members
        .set_state(pair.a.0, MemberState::Active)
        .expect("promoted");
    let refused = pair.members.heartbeat(
        pair.a.0,
        pair.a.1,
        Progress::CaughtUp,
        Reported::debts(Debts::Known(0)),
    );
    assert!(matches!(refused, Err(MembershipError::Superseded { .. })));
    assert_eq!(pair.health().silent, [pair.a.0]);
}

// Catches: a report outliving its row. A member whose row expired and which comes back
// does not bring the report it had; here it also comes back Active only after a
// promotion, so a report that survived would make the number known again.
#[test]
fn an_expired_row_loses_its_report() {
    let pair = Pair::new(Debts::Known(5), Debts::Known(0));
    pair.advance(31);
    pair.members.refresh().expect("refresh");
    let again = pair.members.register(&node(1)).expect("comes back");
    let other = pair.members.register(&node(2)).expect("comes back");
    for id in [again.id, other.id] {
        pair.members
            .set_state(id, MemberState::Active)
            .expect("promoted");
    }
    let health = pair.health();
    assert_eq!(health.under_replicated_blobs, None);
    assert_eq!(health.silent, [again.id, other.id]);
    assert_eq!(health.debts_total, 0, "the old 5 is gone");
}

// Catches: an empty membership read as zero, and a store that cannot be read read as
// anything but not ready.
#[test]
fn no_registered_member_and_an_unreadable_store_are_not_ready() {
    let store = Hooked::new();
    let members = Members::new(Arc::clone(&store) as Arc<dyn Store>, config());
    let health = members.storage_health().expect("health");
    assert_eq!(health.under_replicated_blobs, None);
    assert_eq!(members.ready(), Err(NotReady::NoMembers));

    store
        .fail_gets
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let unreadable = members.ready().expect_err("the store failed");
    assert!(
        matches!(&unreadable, NotReady::Unreadable(why) if why.contains("injected")),
        "{unreadable}"
    );
}

// Catches: a freshness window other than two beats (one lost heartbeat must be forgiven,
// two must not).
#[test]
fn the_window_is_two_beats() {
    assert_eq!(config().freshness(), 2 * config().beat());
    assert_eq!(config().freshness(), FRESH);
}

// ---- the metric ----

fn scrape_of(health: Option<&StorageHealth>) -> String {
    let mut out = String::new();
    render(&mut out, health);
    out
}

// Catches: a gauge of 0 exported when the number is unknown (a dashboard would read
// "nothing owed" while a node is down); the family must be absent, not zero. And the
// reverse: a known number missing, or the known flag not following it. The help text
// carries the definition.
#[test]
fn the_metric_is_absent_when_the_number_is_unknown() {
    let unknown = scrape_of(Some(&judge(&[active(1, 0), active(2, 0).down()])));
    assert!(!unknown.contains("kbf_under_replicated_blobs"), "{unknown}");
    assert!(
        unknown.contains("\nkbf_storage_health_known 0\n"),
        "{unknown}"
    );

    let unreadable = scrape_of(None);
    assert!(
        !unreadable.contains("kbf_under_replicated_blobs"),
        "{unreadable}"
    );
    assert!(
        unreadable.contains("\nkbf_storage_health_known 0\n"),
        "{unreadable}"
    );

    let none = scrape_of(Some(&judge(&[])));
    assert!(!none.contains("kbf_under_replicated_blobs"), "{none}");

    let known = scrape_of(Some(&judge(&[active(1, 2), active(2, 3)])));
    assert!(
        known.contains("\nkbf_under_replicated_blobs 5\n"),
        "{known}"
    );
    assert!(known.contains("\nkbf_storage_health_known 1\n"), "{known}");
    assert!(
        known.contains("# TYPE kbf_under_replicated_blobs gauge"),
        "{known}"
    );
    assert!(
        known.contains("# HELP kbf_under_replicated_blobs Blobs held by one storage member"),
        "{known}"
    );

    let zero = scrape_of(Some(&judge(&[active(1, 0)])));
    assert!(zero.contains("\nkbf_under_replicated_blobs 0\n"), "{zero}");
}

trait Down {
    fn down(self) -> MemberReport;
}

impl Down for MemberReport {
    fn down(self) -> MemberReport {
        MemberReport {
            state: MemberState::Down,
            ..self
        }
    }
}
