//! The memory bound of docs/design/invocations.md section 3.5, stage 1, and the drop
//! marks of section 3.4 (section 9, PR 3): which record goes first when the records
//! reach the bound, that an operation in flight keeps its record, that only open
//! records left close the largest to new rows within the bound, and that a dropped
//! event marks its record incomplete. Each test names the defect it catches.

mod support;

use kbf_invocation::{
    CallKind, Event, LeaseEnd, Outcome, ROW_CAP, RecordKey, Records, State, WallTime,
};
use kbf_types::OperationId;
use support::*;

/// A GetActionResult hit of `d(action)` by `inv` at `at`: one row with a listing.
/// Every action from 10 to 99 makes a row of the same size.
fn hit(inv: &str, at: WallTime, action: u8) -> Event {
    ev(call(
        inv,
        at,
        CallKind::GetActionResult {
            action: d(action),
            hit: Some(listing(action, 0)),
        },
    ))
}

/// [`hit`] by a server-minted id (section 2.3).
fn minted_hit(inv: &str, at: WallTime, action: u8) -> Event {
    let mut c = call(
        inv,
        at,
        CallKind::GetActionResult {
            action: d(action),
            hit: Some(listing(action, 0)),
        },
    );
    c.minted = true;
    ev(c)
}

/// The bytes the records hold after `events`, with no bound.
fn unbounded(events: &[Event]) -> usize {
    let mut r = Records::new();
    for e in events {
        r.apply(e.clone());
    }
    r.bytes()
}

/// The records after `events` within `bound`. Catches, after every event of every
/// test that uses it, records held over the bound and a total that misses a change
/// to one record or operation (a recount left out).
fn bounded(bound: usize, events: &[Event]) -> Records {
    let mut r = Records::with_bound(ROW_CAP, bound);
    for e in events {
        r.apply(e.clone());
        assert!(
            r.bytes() <= bound,
            "{} bytes held after {e:?}, over the bound of {bound}",
            r.bytes()
        );
        let each: usize = r.keys().map(|k| r.get(k).expect("held").bytes()).sum();
        assert_eq!(
            r.bytes(),
            each + r.operations_bytes(),
            "the total is every record's and operation's bytes, after {e:?}"
        );
    }
    r
}

fn held(r: &Records) -> Vec<String> {
    r.keys().map(ToString::to_string).collect()
}

/// Catches an eviction order other than section 3.5's: a minted record kept while a
/// client's idle record goes (plant "least recently seen first, minted or not": `a`
/// goes before `m`), and idle records taken newest first (plant: `b` before `a`). And
/// (plant "minted records count") the latest `last_seen` of the client records removed
/// (the `records_since` of a `404`) moved by a minted one, or not the latest kept.
#[test]
fn eviction_takes_minted_then_the_least_recently_seen_idle_record() {
    // Three records of one size; the bound holds exactly three.
    let three = [
        hit("a", t(0), 10),
        hit("b", t(10), 11),
        minted_hit("m", t(650), 12),
    ];
    let bound = unbounded(&three);
    let mut r = bounded(bound, &three);
    assert_eq!(held(&r), ["a", "b", "m"]);

    // At t(700) `a` and `b` are idle (quiet for 10 minutes or more); `m`, seen 50 s
    // ago, is not, but minted records go first.
    r.apply(hit("c", t(700), 13));
    assert_eq!(held(&r), ["a", "b", "c"], "the minted record goes first");
    assert_eq!(
        r.evicted_last_seen(),
        None,
        "a minted record does not count"
    );
    r.apply(hit("d", t(700), 14));
    assert_eq!(
        held(&r),
        ["b", "c", "d"],
        "then the least recently seen idle one"
    );
    assert_eq!(r.evicted_last_seen(), Some(t(0)));
    r.apply(hit("e", t(700), 15));
    assert_eq!(held(&r), ["c", "d", "e"]);
    assert_eq!(r.evicted_last_seen(), Some(t(10)), "the latest removed");
    assert_eq!(r.evicted(), 3);
    assert!(r.bytes() <= bound);
}

/// Catches an idle test that ignores the operation in flight (plant `state` without
/// `in_flight == 0`: `r`, quiet for 11 minutes but with its operation still running,
/// is evicted instead of `q`).
#[test]
fn an_invocation_whose_operation_still_runs_is_not_idle_and_not_evicted() {
    let events = [
        queued("r", t(0), 10, 1),
        granted(1, 1, "node-a", t(1)),
        started(1, 1, t(2)),
        hit("q", t(5), 11),
        hit("n", t(700), 12),
    ];
    let bound = unbounded(&events) - 1;
    let r = bounded(bound, &events);
    let running = r.get(&key("r")).expect("r is kept");
    assert_eq!(running.state(t(700)), State::Open);
    assert_eq!(held(&r), ["n", "r"], "the idle record `q` goes, `r` stays");
}

/// Catches `last_seen` taken from calls only (plant: drop `inv.touch(at)` in
/// `Records::spread`): `r`'s only operation ran for 11 minutes after its one call and
/// finished at t(690); with no other call `r` would look idle at t(700) and, seen
/// before `q`, be evicted first.
#[test]
fn a_record_whose_operation_ran_past_the_idle_time_is_seen_at_its_finish() {
    let events = [
        queued("r", t(0), 10, 1),
        granted(1, 1, "node-a", t(1)),
        started(1, 1, t(2)),
        hit("q", t(5), 11),
        finished_ok(1, 1, 20, 0, t(690)),
        hit("n", t(700), 12),
    ];
    let bound = unbounded(&events) - 1;
    let r = bounded(bound, &events);
    let ran = r.get(&key("r")).expect("r is kept");
    assert_eq!(ran.last_seen(), t(690), "the finish is r's last event");
    assert_eq!(ran.state(t(700)), State::Open);
    assert_eq!(held(&r), ["n", "r"], "the idle record `q` goes, `r` stays");
}

/// Catches a bound passed when only open records remain (rows taken anyway), a
/// record other than the largest closed (plant "close the smallest": `b` says
/// `rows_capped` after `a`'s overflow), rows refused without `rows_capped`, and
/// actions not counted once rows stop.
#[test]
fn at_the_bound_with_only_open_records_the_largest_stops_taking_rows() {
    let mut events = vec![hit("b", t(0), 10)];
    events.extend((11..21).map(|n| hit("a", t(1), n)));
    let bound = unbounded(&events);
    events.extend((21..41).map(|n| hit("a", t(2), n)));
    let r = bounded(bound, &events);
    let a = r.get(&key("a")).expect("a");
    assert!(a.rows_capped(), "the largest record says rows_capped");
    assert_eq!(a.rows().len(), 10, "no row past the bound");
    assert_eq!(a.counts().ac_hit, 30, "its actions are still counted");
    let b = r.get(&key("b")).expect("b");
    assert!(
        !b.rows_capped(),
        "a smaller record that took no row is not capped"
    );

    // `b`'s next row does not fit either: refused, and `b` says so.
    let mut r = r;
    r.apply(hit("b", t(3), 41));
    assert!(r.bytes() <= bound);
    let b = r.get(&key("b")).expect("b");
    assert!(b.rows_capped());
    assert_eq!(b.rows().len(), 1);
    assert_eq!(r.evicted(), 0, "no open record is removed");
}

/// Catches a collision key reused after the owner's record is evicted (plant the old
/// rule `n = keys held + 1`): the fourth caller, routed once `x` is gone, would get
/// `x~3`, the key of a record still held, and overwrite it.
#[test]
fn a_collision_after_the_owner_is_evicted_gets_a_new_key() {
    let mut second = call(
        "x",
        t(650),
        CallKind::GetActionResult {
            action: d(11),
            hit: Some(listing(11, 0)),
        },
    );
    second.owner = owner("o2");
    let events = [hit("x", t(0), 10), ev(second.clone())];
    // Room for two records and a record with no row (and its index entries): the
    // third, with a row, evicts the idle owner record `x`; the fourth fits.
    let header = unbounded(&[ev(call("y", t(0), CallKind::Downloaded(1)))]);
    let bound = unbounded(&events) + header + 100;
    let mut r = bounded(bound, &events);
    let mut third = second;
    third.owner = owner("o3");
    third.at = t(700);
    r.apply(ev(third));
    assert_eq!(held(&r), ["x~2", "x~3"]);
    let x2 = r
        .get(&RecordKey::collision(id("x"), 2))
        .expect("x~2 is still held");
    assert_eq!(x2.owner(), &owner("o2"), "x~2 is not overwritten");
    // Two records of `x` are held now; the next caller's key is still a new one.
    let mut fourth = call("x", t(700), CallKind::Downloaded(1));
    fourth.owner = owner("o4");
    r.apply(ev(fourth));
    assert_eq!(held(&r), ["x~2", "x~3", "x~4"]);
    let x3 = r
        .get(&RecordKey::collision(id("x"), 3))
        .expect("x~3 is still held");
    assert_eq!(x3.owner(), &owner("o3"), "x~3 is not overwritten");
}

/// Catches an evicted record left among an operation's waiters: a lease event of
/// the minted record's operation after its eviction would look the record up and
/// panic, or re-create it.
#[test]
fn lease_events_of_an_evicted_record_are_ignored() {
    let mut c = call(
        "m",
        t(0),
        CallKind::Execute {
            action: d(10),
            answer: kbf_invocation::ExecuteAnswer::Queued {
                name: name("m", 5),
                operation: OperationId(5),
            },
        },
    );
    c.minted = true;
    // Its grant moves its `last_seen`, so the minted index moves with it.
    let events = [ev(c), granted(5, 1, "node-a", t(1)), hit("a", t(1), 11)];
    let bound = unbounded(&events);
    let mut r = bounded(bound, &events);
    r.apply(hit("b", t(2), 12));
    assert_eq!(
        held(&r),
        ["a", "b"],
        "the minted record goes, in flight or not"
    );
    r.apply(started(5, 1, t(3)));
    r.apply(finished(
        5,
        Some((lease(1), LeaseEnd::Accepted)),
        Outcome::Ok { exit_code: 0 },
        t(4),
    ));
    assert_eq!(held(&r), ["a", "b"]);
}

/// Catches a drop mark that reaches the wrong record (another owner's record of the
/// same id, or a record of another id), and one lost when the dropped call was the
/// record's first (plant "ignore a mark for a record not held": `late` stays
/// complete).
#[test]
fn a_dropped_call_marks_its_own_record_incomplete() {
    let mut squat = call("x", t(1), CallKind::Downloaded(1));
    squat.owner = owner("o2");
    let mut r = Records::new();
    r.apply(ev(call("x", t(0), CallKind::Downloaded(1))));
    r.apply(ev(squat));
    r.apply(ev(call("y", t(0), CallKind::Downloaded(1))));
    r.mark_incomplete_call(&id("x"), &owner(""));
    // The first call of `late` was dropped: its record does not exist yet.
    r.mark_incomplete_call(&id("late"), &owner(""));
    r.apply(ev(call("late", t(2), CallKind::Downloaded(1))));
    let complete = |k: RecordKey| r.get(&k).expect("held").complete();
    assert!(!complete(key("x")));
    assert!(complete(RecordKey::collision(id("x"), 2)));
    assert!(complete(key("y")));
    assert!(
        !complete(key("late")),
        "the record a dropped call would have made"
    );
}

/// Catches a dropped lease event that marks no record, or records that do not wait
/// on its operation.
#[test]
fn a_dropped_lease_event_marks_the_records_waiting_on_its_operation() {
    let mut r = Records::new();
    r.apply(queued("a", t(0), 10, 1));
    r.apply(joined("b", t(0), 10, 1));
    r.apply(queued("c", t(0), 11, 2));
    r.mark_incomplete_operation(OperationId(1));
    let complete = |r: &Records, k: &str| r.get(&key(k)).expect("held").complete();
    assert!(!complete(&r, "a"));
    assert!(!complete(&r, "b"));
    assert!(complete(&r, "c"));
    r.mark_all_incomplete();
    assert!(!complete(&r, "c"));
}

/// A listing of `n` outputs: rows that grow when their operation finishes.
fn wide(result: u8, n: usize) -> kbf_invocation::Listing {
    let mut l = listing(result, 0);
    l.outputs = (0..n)
        .map(|i| kbf_invocation::Output {
            path: format!("out/{i:04}.o"),
            digest: d(result),
            executable: false,
            directory: false,
        })
        .collect();
    l
}

/// The outputs of a wide listing: more than the idle record's bytes make room for.
const WIDE: usize = 80;

fn finished_wide(op: u64, result: u8, at: WallTime) -> Event {
    Event::Finished(kbf_invocation::Finished {
        operation: OperationId(op),
        lease: None,
        outcome: Outcome::Ok { exit_code: 0 },
        result: Some(wide(result, WIDE)),
        at,
    })
}

/// Catches a bound held only when a row is added: rows already taken grow when their
/// operation finishes (here by up to [`WIDE`] outputs each). The idle record goes first
/// (plant "no room made for a listing": `q` stays and the listing is cut at once),
/// then the listing keeps the outputs that fit and its record says
/// `complete: false`; once nothing fits, a new row is refused with `rows_capped`.
#[test]
fn growth_past_the_bound_evicts_idle_records_then_trims_what_fills_in() {
    let mut events = vec![
        hit("q", t(0), 10),
        queued("r", t(700), 11, 1),
        queued("s", t(700), 12, 2),
    ];
    let bound = unbounded(&events);
    events.push(finished_wide(1, 20, t(701)));
    let r = bounded(bound, &events);
    assert_eq!(held(&r), ["r", "s"], "the idle record goes first");
    let ran = &r.get(&key("r")).expect("r").rows()[0];
    let kept = ran.listing.as_ref().expect("listed").outputs.len();
    assert!(kept > 0 && kept < WIDE, "{kept} of {WIDE} outputs kept");
    assert!(ran.outputs_truncated);
    assert!(!r.get(&key("r")).expect("r").complete());
    assert!(r.get(&key("s")).expect("s").complete());

    events.push(finished_wide(2, 21, t(702)));
    events.push(hit("s", t(703), 13));
    let r = bounded(bound, &events);
    assert_eq!(held(&r), ["r", "s"]);
    let s = r.get(&key("s")).expect("s");
    assert!(!s.complete());
    assert!(s.rows_capped(), "no room for a new row");
    assert_eq!(s.rows().len(), 1);
}

/// Catches an evicted collision left in its owner's list (the owner's page would
/// list a record that is gone).
#[test]
fn an_evicted_collision_leaves_its_owners_list() {
    let mut squat = call("x", t(1), CallKind::Downloaded(1));
    squat.owner = owner("o2");
    let events = [
        ev(call("x", t(0), CallKind::Downloaded(1))),
        ev(squat),
        ev(call("x", t(700), CallKind::Downloaded(1))),
    ];
    let bound = unbounded(&events);
    let mut r = bounded(bound, &events);
    assert_eq!(
        r.get(&key("x")).expect("x").collisions(),
        [RecordKey::collision(id("x"), 2)]
    );
    r.apply(hit("n", t(700), 10));
    assert_eq!(held(&r), ["n", "x"], "the idle collision goes");
    assert!(r.get(&key("x")).expect("x").collisions().is_empty());
}

/// Catches dropped-call marks for records not made yet kept without end: past 1024,
/// the oldest is forgotten (its record starts complete), the newest still marks.
#[test]
fn marks_for_records_not_made_yet_are_capped() {
    let mut r = Records::new();
    for n in 0..1025 {
        r.mark_incomplete_call(&id(&format!("u{n}")), &owner(""));
    }
    r.apply(ev(call("u0", t(0), CallKind::Downloaded(1))));
    r.apply(ev(call("u1024", t(0), CallKind::Downloaded(1))));
    assert!(
        r.get(&key("u0")).expect("u0").complete(),
        "the oldest is forgotten"
    );
    assert!(!r.get(&key("u1024")).expect("u1024").complete());
}

/// Applies `events` within every bound from 0 to what they take with none, checking
/// after every event (see [`bounded`]); the records within the tightest bound that
/// leaves out only the last addition. Catches an addition admitted at less than it
/// adds: some bound has room for what was counted and not for what was added.
fn swept(events: &[Event]) -> Records {
    let top = unbounded(events);
    for bound in 0..top {
        bounded(bound, events);
    }
    bounded(top - 1, events)
}

/// Catches a collision made without room for its entry in the owner's list (plant
/// `listed = 0` in `Records::route`: within 2,036 bytes the records held 2,077): a
/// second caller of an id is the only growth.
#[test]
fn a_collision_is_admitted_with_its_entry_in_the_owners_list() {
    let mut other = call("x", t(1), CallKind::Downloaded(1));
    other.owner = owner("o2");
    let r = swept(&[ev(call("x", t(0), CallKind::Downloaded(1))), ev(other)]);
    assert_eq!(held(&r), ["x"], "one byte short, the collision is not made");
}

/// The first call of `x`, then a second that sends `metadata` for the first time.
fn sent_later(metadata: kbf_invocation::Metadata) -> [Event; 2] {
    let mut later = call("x", t(1), CallKind::Downloaded(1));
    later.metadata = metadata;
    [ev(call("x", t(0), CallKind::Downloaded(1))), ev(later)]
}

/// Catches a correlated id sent after a record's first call admitted at nothing
/// (plant `0` for it in `Invocation::seen_bytes`: within 1,058 bytes the records
/// held 1,117); one byte short, the id is not set and the record says `complete: false`.
#[test]
fn a_correlated_id_sent_later_is_admitted() {
    let r = swept(&sent_later(kbf_invocation::Metadata {
        correlated_invocations_id: Some("c".repeat(100)),
        ..kbf_invocation::Metadata::default()
    }));
    let x = r.get(&key("x")).expect("x");
    assert_eq!(x.correlated_invocations_id(), None);
    assert!(!x.complete());
}

/// Catches a tool sent after a record's first call admitted at nothing (plant `0`
/// for it in `Invocation::seen_bytes`: within 1,058 bytes the records held 1,217);
/// one byte short, it is not set and the record
/// says `complete: false`.
#[test]
fn a_tool_sent_later_is_admitted() {
    let r = swept(&sent_later(kbf_invocation::Metadata {
        tool: Some("t".repeat(100)),
        tool_version: Some("v".repeat(100)),
        ..kbf_invocation::Metadata::default()
    }));
    let x = r.get(&key("x")).expect("x");
    assert_eq!(x.tool(), (None, None));
    assert!(!x.complete());
}

/// Catches marks for records not made yet kept past their share of the bound (plant:
/// no byte check in `Records::mark_incomplete_call`): within 32 kB they hold 2 kB at
/// most, about 40 marks of short ids, so of 100 the oldest is forgotten and the
/// newest still marks.
#[test]
fn marks_for_records_not_made_yet_hold_a_sixteenth_of_the_bound() {
    let mut r = Records::with_bound(ROW_CAP, 32 * 1024);
    for n in 0..100 {
        r.mark_incomplete_call(&id(&format!("u{n}")), &owner(""));
    }
    r.apply(ev(call("u0", t(0), CallKind::Downloaded(1))));
    r.apply(ev(call("u99", t(0), CallKind::Downloaded(1))));
    assert!(r.get(&key("u0")).expect("u0").complete(), "forgotten");
    assert!(!r.get(&key("u99")).expect("u99").complete());
}

/// Catches a mark larger than its share kept anyway: an owner of 3 kB does not fit
/// in the 2 kB of a 32 kB bound, so its record starts complete, as a forgotten
/// mark's does.
#[test]
fn a_mark_larger_than_the_share_is_not_kept() {
    let mut r = Records::with_bound(ROW_CAP, 32 * 1024);
    let big = owner(&"o".repeat(3000));
    r.mark_incomplete_call(&id("u"), &big);
    let mut c = call("u", t(0), CallKind::Downloaded(1));
    c.owner = big;
    r.apply(ev(c));
    assert!(r.get(&key("u")).expect("u").complete());
}

/// Catches (security): a row moved to an instance name its record does not hold. At
/// the bound the record cannot take the new name `b` (its bytes do not fit); a later
/// Execute of the same action by `b` replaced the row anyway (a shorter name costs
/// 0 bytes more), so a reader authorized on the record's names read a row of `b`.
/// The row stays as it was and the record says it is incomplete.
#[test]
fn a_row_never_takes_an_instance_name_the_record_does_not_hold() {
    let on = |e: Event, instance: &str| match e {
        Event::Call(mut c) => {
            c.instance = instance.to_owned();
            Event::Call(c)
        }
        e => e,
    };
    let first = on(queued("inv", t(1), 10, 1), "aaaaaaaaaaaaaaaa");
    let mut r = bounded(unbounded(std::slice::from_ref(&first)), &[first]);
    r.apply(on(queued("inv", t(2), 10, 2), "b"));
    let inv = r.get(&key("inv")).expect("held");
    assert_eq!(
        inv.instances().iter().collect::<Vec<_>>(),
        ["aaaaaaaaaaaaaaaa"]
    );
    let rows: Vec<_> = inv
        .rows()
        .iter()
        .map(|x| (x.instance.as_str(), x.operation.as_deref()))
        .collect();
    assert_eq!(rows, [("aaaaaaaaaaaaaaaa", Some(name("inv", 1).as_str()))]);
    assert!(!inv.complete());
}

/// Catches (security; mutant "a new row skips the check"): a new row of an instance
/// name the record could not take. The call's name and its first `tool` do not fit
/// together, while its row alone does, so the record leaves the name out (the bound
/// closes the larger open record `big` to rows, not this one) and would have added a
/// row of it. The row is not added; the record says it is incomplete.
#[test]
fn a_new_row_of_a_name_the_record_could_not_take_is_not_added() {
    let on = |e: Event, instance: &str, tool: Option<String>| match e {
        Event::Call(mut c) => {
            c.instance = instance.to_owned();
            c.metadata.tool = tool;
            Event::Call(c)
        }
        e => e,
    };
    let first = on(queued("inv", t(1), 10, 1), "a", None);
    let fits = on(queued("inv", t(2), 11, 2), "b", None);
    let mut events: Vec<_> = (20..26)
        .map(|n| queued("big", t(1), n, u64::from(n)))
        .collect();
    events.push(first);
    let mut all = events.clone();
    all.push(fits);
    let mut r = bounded(unbounded(&all), &events);
    r.apply(on(queued("inv", t(2), 11, 2), "b", Some("x".repeat(4096))));
    let inv = r.get(&key("inv")).expect("held");
    assert_eq!(inv.instances().iter().collect::<Vec<_>>(), ["a"]);
    let rows: Vec<_> = inv.rows().iter().map(|x| x.instance.as_str()).collect();
    assert_eq!(rows, ["a"]);
    assert!(!inv.complete());
}

/// Catches (security; mutant "count before the admit check"): a call whose instance
/// name the record could not take still applied to it. The name's bytes do not fit,
/// while an operation and its waiter do; the call must change none of the counts,
/// `last_seen`, the state (no operation in flight) or the operations held, and the
/// record says it is incomplete.
#[test]
fn a_call_of_a_name_the_record_could_not_take_changes_nothing_else() {
    let secret = |mut e: Event| {
        if let Event::Call(c) = &mut e {
            c.instance = format!("secret/{}", "x".repeat(4096));
        }
        e
    };
    let prefix = [
        queued("inv", t(1), 10, 1),
        finished(1, None, Outcome::Ok { exit_code: 0 }, t(2)),
    ];
    let mut room = prefix.to_vec();
    room.push(queued("inv", t(3), 11, 2));
    let mut r = bounded(unbounded(&room), &prefix);
    let before = r.get(&key("inv")).expect("held").clone();
    let later = t(1_000_000);
    assert_eq!(before.state(later), State::Idle);
    r.apply(secret(ev(call(
        "inv",
        t(5),
        CallKind::Uploaded(987_654_321),
    ))));
    r.apply(secret(queued("inv", t(6), 11, 2)));
    let inv = r.get(&key("inv")).expect("held");
    assert_eq!(inv.instances().iter().collect::<Vec<_>>(), ["main"]);
    assert_eq!(inv.counts(), before.counts());
    assert_eq!(inv.last_seen(), before.last_seen());
    assert_eq!(inv.state(later), State::Idle, "no operation in flight");
    assert_eq!(r.operations_bytes(), 0, "no operation held");
    assert_eq!(inv.rows().len(), before.rows().len());
    assert!(!inv.complete());
}
