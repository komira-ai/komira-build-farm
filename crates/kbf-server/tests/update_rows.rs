//! The `update/*` rows (`docs/design/split-mode-updates.md` section 6.1 and
//! `docs/design/pull-updates.md` section 6.2): their bytes, frozen, and what reading and
//! writing them does on the in-memory fake and on a SQLite file.
//!
//! The goldens below were worked out by hand from the protobuf wire format (a key is
//! `tag << 3 | wire type`, a varint is seven bits at a time with the high bit saying more
//! follow, a zero is not written), not copied from the code's output: a row that changes its
//! bytes changes what a store written by the last release means to this one.
//!
//! Each test names the defect it would catch. The mutants that were planted to see each
//! group go red are listed in the pull request.

#[path = "update_support/mod.rs"]
mod support;

use kbf_server::update::{
    Desired, Generation, HoldReason, Holder, HostPhase, HostRecord, HostRow, OrderId, Policy,
    PolicyError, Release, RowError, Rows, Slot, SlotClass, Stored,
};
use kbf_state::{Cond, Txn};
use kbf_types::FarmTime;
use std::time::Duration;
use support::harness::{Backing, Cell};
use support::id;

const BACKINGS: [Backing; 2] = [Backing::Fake, Backing::Sqlite];

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn raw(cell: &Cell, key: &str) -> String {
    let row = cell
        .store()
        .get(key)
        .expect("a read")
        .expect("the row exists");
    hex(&row.value)
}

fn put_raw(cell: &Cell, key: &str, value: &[u8]) {
    let txn = Txn::new().put(key, value.to_vec(), Cond::Any);
    cell.store()
        .txn(txn)
        .expect("a write")
        .expect("no condition");
}

fn row(phase: HostPhase) -> HostRow {
    HostRow {
        record: HostRecord {
            phase,
            since: FarmTime::from_millis(5000),
            generation: Generation(3),
            order: OrderId::new(2, 9),
            boot_before: Some(10),
        },
        commit: Release::new("v2"),
    }
}

fn write_host(rows: &Rows, name: &str, row: &HostRow) {
    let absent = Stored {
        value: HostRow::unstarted(),
        version: None,
    };
    rows.put_host(&id(name), &absent, row).expect("a write");
}

// The bytes.

/// Catches: a row whose bytes move: a field renumbered, a default written where it was
/// left out, an enum numbered by its Rust order. Each row of the design, with a value that
/// sets every field, against the bytes the wire format gives it.
#[test]
fn the_bytes_of_every_row_are_frozen() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "frozen");
        let rows = Rows::new(cell.store());

        // update/desired {generation, commit, set_at}: generation 1, "v2", set at 1000 ms,
        // and the pause (beyond the agreed fields), last.
        let now = FarmTime::from_millis(1000);
        rows.offer(&Release::new("v2"), now).expect("an offer");
        rows.set_paused(true).expect("a pause");
        assert_eq!(raw(&cell, "update/desired"), "08011202763218e8072001");

        // update/host/<id> {phase, order_id, generation, since, boot_before}: Held (13),
        // order number 9, generation 3, entered at 5000 ms, boot counter 10 at the mark;
        // then the beyond-agreed fields: the hold reason (RepairWait is 7), the order's
        // epoch 2, and the run's commit "v2".
        let held = HostPhase::Held {
            reason: HoldReason::RepairWait,
        };
        write_host(&rows, "s1", &row(held));
        assert_eq!(
            raw(&cell, "update/host/s1"),
            "080d10091803208827280a3007380242027632"
        );
        // A host nothing was done to: only its phase (Pending is 1) is written.
        write_host(&rows, "d1", &HostRow::unstarted());
        assert_eq!(raw(&cell, "update/host/d1"), "0801");

        // update/slot: a storage holder and two others, each {host, order_id, epoch}.
        let holder = |name: &str, seq: u64| Holder {
            host: id(name),
            order: OrderId::new(1, seq),
        };
        let slot = Slot {
            storage: Some(holder("s1", 5)),
            hosts: vec![holder("d1", 6), holder("m1", 7)],
        };
        let empty = rows.slot().expect("a read");
        rows.put_slot(&empty, &slot).expect("a write");
        assert_eq!(
            raw(&cell, "update/slot"),
            "0a080a02733110051801".to_owned() + "12080a02643110061801" + "12080a026d3110071801"
        );

        // update/policy, the defaults with a floor of one, three places and a Mac force:
        // only the fields that are not the default are written.
        let policy = Policy {
            min_serving: 1,
            max_hosts_out: 3,
            mac_force_after: Some(Duration::from_secs(1800)),
            ..Policy::default()
        };
        rows.set_policy(&policy).expect("a policy");
        assert_eq!(raw(&cell, "update/policy"), "2001280330c0ee6d");
        // Every agreed field set (the eight, in the design's order, 1 to 8), and then two
        // of the node step's limits (the write limit is 13, the rebuild limit 16).
        let all = Policy {
            auto_nodes: true,
            auto_frontend: true,
            idle_wait: Duration::from_secs(30),
            min_serving: 2,
            max_hosts_out: 4,
            mac_force_after: Some(Duration::from_secs(600)),
            new_work_only_to_updated: true,
            accept_failing_builds: true,
            write_limit: Duration::from_secs(90),
            rebuild_limit: Some(Duration::from_secs(900)),
            ..Policy::default()
        };
        rows.set_policy(&all).expect("a policy");
        assert_eq!(
            raw(&cell, "update/policy"),
            "0801".to_owned()
                + "1001"
                + "18b0ea01"
                + "2002"
                + "2804"
                + "30c0cf24"
                + "3801"
                + "4001"
                + "6890bf05"
                + "8001a0f736"
        );
        assert_eq!(rows.policy().expect("a read"), all);
    }
}

/// Catches: a policy row that only knows the eight agreed fields, as the operator API
/// writes it, read with the node step's limits at zero (a dwell of zero cannot be kept) or
/// `max_hosts_out` at the invalid 0. Absent is the default; an explicit zero is a zero.
#[test]
fn a_policy_row_with_only_the_agreed_fields_leaves_the_limits_at_their_defaults() {
    let cell = Cell::new(Backing::Fake, "agreed");
    let rows = Rows::new(cell.store());
    // auto_nodes on, a floor of two: nothing else written.
    put_raw(&cell, "update/policy", &[0x08, 0x01, 0x20, 0x02]);
    let read = rows.policy().expect("a read");
    assert_eq!(
        read,
        Policy {
            auto_nodes: true,
            min_serving: 2,
            ..Policy::default()
        }
    );
    assert_eq!(read.max_hosts_out, 1, "absent is the default of one");
    assert_eq!(read.validate(), Ok(()));
    // An explicit zero is read as a zero, which the controller refuses.
    put_raw(&cell, "update/policy", &[0x28, 0x00]);
    let zero = rows.policy().expect("a read");
    assert_eq!(zero.max_hosts_out, 0);
    assert_eq!(zero.validate(), Err(PolicyError::NoHostsOut));
}

/// Catches: an enum numbered in a way a reordering of the Rust enum would move: every phase
/// and every hold reason reads back as itself and has the number the design's table gives
/// it (the phase's step order from 1, `Held` 13, `Deferred` 14).
#[test]
fn every_phase_and_hold_reason_round_trips_under_its_number() {
    let reasons = [
        HoldReason::SecondMemberDown,
        HoldReason::RestartDeadline,
        HoldReason::MemberGone,
        HoldReason::ActivateStalled,
        HoldReason::HealthLimit,
        HoldReason::RebuildLimit,
        HoldReason::RepairWait,
        HoldReason::ConfirmStalled,
        HoldReason::Reverted,
        HoldReason::RevertRefused,
        HoldReason::StillCordoned,
    ];
    let phases = [
        HostPhase::Pending,
        HostPhase::Selected,
        HostPhase::Cordoned,
        HostPhase::WaitingLeases,
        HostPhase::PlannedDown,
        HostPhase::Gated,
        HostPhase::Activating,
        HostPhase::RejoiningStorage,
        HostPhase::Repairing,
        HostPhase::Confirming,
        HostPhase::RejoiningWork,
        HostPhase::Done,
    ];
    let cell = Cell::new(Backing::Fake, "phases");
    let rows = Rows::new(cell.store());
    let forget = |key: &str| {
        let gone = Txn::new().delete(key, Cond::Any);
        cell.store()
            .txn(gone)
            .expect("a write")
            .expect("no condition");
    };
    // A phase is `08` and its number, then the order number (`10`); a reason, written only
    // for `Held`, is field 6 (`30`), between the boot counter and the order's epoch.
    for (n, phase) in phases.into_iter().enumerate() {
        write_host(&rows, "h", &row(phase.clone()));
        let bytes = raw(&cell, "update/host/h");
        let number = n + 1;
        // No reason is written for a step: the rest of the row is the same for every phase.
        assert_eq!(
            bytes,
            format!("08{number:02x}10091803208827280a380242027632"),
            "{phase:?}"
        );
        assert_eq!(
            rows.hosts().expect("a read")[&id("h")].value,
            row(phase.clone())
        );
        forget("update/host/h");
    }
    for (n, reason) in reasons.into_iter().enumerate() {
        let held = HostPhase::Held { reason };
        write_host(&rows, "r", &row(held.clone()));
        let bytes = raw(&cell, "update/host/r");
        let number = n + 1;
        assert_eq!(
            bytes,
            format!("080d10091803208827280a30{number:02x}380242027632"),
            "{reason:?}"
        );
        assert_eq!(rows.hosts().expect("a read")[&id("r")].value, row(held));
        forget("update/host/r");
    }
    write_host(&rows, "x", &row(HostPhase::Deferred));
    assert!(raw(&cell, "update/host/x").starts_with("080e10"));
}

/// Catches: a reader that guesses. A number this build does not know, a zero (a field
/// that is not there), a hold reason on a phase that has none, a `Held` with no reason, two
/// places held by one host, an empty name, an empty commit, generation 0 and bytes that are
/// no message are each refused with the row's name, never read as the nearest phase.
#[test]
fn a_row_this_build_cannot_read_is_refused_not_guessed() {
    let cell = Cell::new(Backing::Fake, "corrupt");
    let rows = Rows::new(cell.store());
    let bad_hosts: [(&str, &[u8]); 6] = [
        ("no phase", &[0x10, 0x00]),
        ("a phase from the future", &[0x08, 0x63]),
        ("a reason on a step", &[0x08, 0x03, 0x30, 0x01]),
        ("held with no reason", &[0x08, 0x0d]),
        (
            "held for a reason from the future",
            &[0x08, 0x0d, 0x30, 0x63],
        ),
        ("not a message", &[0xff, 0xff, 0xff]),
    ];
    for (what, bytes) in bad_hosts {
        put_raw(&cell, "update/host/h", bytes);
        let Err(RowError::Corrupt { key, .. }) = rows.hosts() else {
            panic!("{what}: the row was read");
        };
        assert_eq!(key, "update/host/h", "{what}");
    }
    cell.store()
        .txn(Txn::new().delete("update/host/h", Cond::Any))
        .expect("a write")
        .expect("no condition");

    // One host in two places, and a holder with no name.
    let twice = [
        0x0a, 0x04, 0x0a, 0x02, 0x73, 0x31, 0x12, 0x04, 0x0a, 0x02, 0x73, 0x31,
    ];
    put_raw(&cell, "update/slot", &twice);
    assert!(matches!(rows.slot(), Err(RowError::Corrupt { .. })));
    put_raw(&cell, "update/slot", &[0x0a, 0x02, 0x10, 0x01]);
    assert!(matches!(rows.slot(), Err(RowError::Corrupt { .. })));

    // A run with no commit, and one with no generation (generation 0 is what a host no run
    // has touched carries, so a run of it would read every host as having finished it).
    put_raw(&cell, "update/desired", &[0x08, 0x01]);
    assert!(matches!(rows.desired(), Err(RowError::Corrupt { .. })));
    put_raw(&cell, "update/desired", &[0x12, 0x02, 0x76, 0x32]);
    assert!(matches!(rows.desired(), Err(RowError::Corrupt { .. })));
    put_raw(&cell, "update/policy", &[0xff]);
    assert!(matches!(rows.policy(), Err(RowError::Corrupt { .. })));
}

/// Catches: a reader that cannot take a row a later release wrote with one more field
/// (state-store rows are frozen across one step, section 3.2): the field is skipped and the
/// rest read as before.
#[test]
fn a_field_added_later_is_skipped() {
    let cell = Cell::new(Backing::Fake, "newer");
    let rows = Rows::new(cell.store());
    write_host(&rows, "h", &row(HostPhase::Cordoned));
    let mut bytes = cell.store().get("update/host/h").unwrap().unwrap().value;
    // Field 15, a varint, and field 16, a string.
    bytes.extend_from_slice(&[0x78, 0x2a, 0x82, 0x01, 0x02, 0x68, 0x69]);
    put_raw(&cell, "update/host/h", &bytes);
    assert_eq!(
        rows.hosts().expect("a read")[&id("h")].value,
        row(HostPhase::Cordoned)
    );
}

// The rows.

/// Catches: an offer that reuses a generation or forgets the pause; a rollback that is not
/// an offer; an offer with no release. A generation only grows (the first is 1), the pause
/// and the clock are kept as asked, and the same on both backends.
#[test]
fn offers_grow_the_generation_and_keep_the_pause() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "offer");
        let rows = Rows::new(cell.store());
        assert_eq!(rows.desired().unwrap().value, None);
        assert!(!rows.set_paused(true).unwrap(), "no run to pause");
        let at = FarmTime::from_millis(42);
        let first = rows.offer(&Release::new("v2"), at).unwrap();
        assert_eq!(
            first,
            Desired {
                generation: Generation(1),
                commit: Release::new("v2"),
                set_at: at,
                paused: false,
            }
        );
        assert!(rows.set_paused(true).unwrap());
        let back = rows
            .offer(&Release::new("v1"), FarmTime::from_millis(50))
            .unwrap();
        assert_eq!(back.generation, Generation(2));
        assert!(back.paused, "an offer does not resume a paused run");
        assert!(rows.set_paused(false).unwrap());
        assert!(!rows.desired().unwrap().value.unwrap().paused);
        assert_eq!(
            rows.offer(&Release::new(""), at),
            Err(RowError::NoRelease),
            "an offer names a release"
        );
        assert_eq!(
            rows.desired().unwrap().value.unwrap().generation,
            Generation(2)
        );
    }
}

/// Catches: a repeated click that starts a new run (`pull-updates.md` section 5: setting the
/// commit that is already desired changes nothing and keeps the generation, and the time it
/// was set), and a generation that is not above the current run's when the counter it is
/// drawn from is behind the row (a row written without the counter, or a counter restored
/// from an older backup than the row). Setting the older commit again afterwards is a new
/// run: that is the rollback.
#[test]
fn the_commit_already_desired_keeps_its_generation_and_a_generation_is_always_above_the_row() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "same");
        let rows = Rows::new(cell.store());
        let first = rows
            .offer(&Release::new("v2"), FarmTime::from_millis(10))
            .unwrap();
        let again = rows
            .offer(&Release::new("v2"), FarmTime::from_millis(99))
            .unwrap();
        assert_eq!(again, first, "the same commit changes nothing");
        assert_eq!(rows.desired().unwrap().value.unwrap(), first);
        let back = rows
            .offer(&Release::new("v1"), FarmTime::from_millis(20))
            .unwrap();
        assert_eq!(
            back.generation,
            Generation(2),
            "the repeat did not spend the counter"
        );
        let forward = rows
            .offer(&Release::new("v2"), FarmTime::from_millis(30))
            .unwrap();
        assert_eq!(forward.generation, Generation(3), "a rollback is a new run");

        // A row from a counter that is ahead of this store's: generation 90.
        let ahead = Desired {
            generation: Generation(90),
            commit: Release::new("x"),
            set_at: FarmTime::from_millis(1),
            paused: false,
        };
        put_raw(
            &cell,
            "update/desired",
            &[0x08, 0x5a, 0x12, 0x01, 0x78, 0x18, 0x01],
        );
        assert_eq!(rows.desired().unwrap().value.unwrap(), ahead);
        let next = rows
            .offer(&Release::new("y"), FarmTime::from_millis(40))
            .unwrap();
        assert_eq!(
            next.generation,
            Generation(91),
            "above the row, whatever the counter says"
        );
    }
}

/// Catches: a policy row written that the controller could not keep; a missing row read as
/// anything but the design's defaults; a field lost in the round trip.
#[test]
fn the_policy_is_the_defaults_until_written_and_is_checked_when_written() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "policy");
        let rows = Rows::new(cell.store());
        assert_eq!(rows.policy().unwrap(), Policy::default());
        let bad = Policy {
            max_hosts_out: 0,
            ..Policy::default()
        };
        assert_eq!(
            rows.set_policy(&bad),
            Err(RowError::Policy(PolicyError::NoHostsOut))
        );
        assert_eq!(
            rows.policy().unwrap(),
            Policy::default(),
            "nothing was written"
        );
        let every = Policy {
            ttl: Duration::from_millis(15_500),
            min_serving: 2,
            max_hosts_out: 4,
            rebuild_limit: Some(Duration::from_secs(900)),
            mac_force_after: Some(Duration::from_secs(60)),
            ..Policy::default()
        };
        rows.set_policy(&every).unwrap();
        assert_eq!(rows.policy().unwrap(), every);
    }
}

/// Catches: the storage place and the places for other hosts mixed; a claim granted when
/// the kind is full; a claim that is not repeatable; a release of a place not held that
/// changes the slot; a slot that loses its holders' claims.
#[test]
fn places_are_claimed_and_released_by_kind() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "slot");
        let rows = Rows::new(cell.store());
        let claim = |class, name: &str, seq, max| {
            rows.claim(class, &id(name), OrderId::new(1, seq), max)
                .unwrap()
        };
        let s1 = claim(SlotClass::Storage, "s1", 1, 2).expect("the storage place is free");
        assert_eq!(s1.storage.as_ref().map(|h| h.host.clone()), Some(id("s1")));
        assert!(
            claim(SlotClass::Storage, "s2", 2, 2).is_none(),
            "one storage place"
        );
        assert!(claim(SlotClass::Host, "d1", 3, 2).is_some());
        assert!(claim(SlotClass::Host, "d2", 4, 2).is_some());
        assert!(
            claim(SlotClass::Host, "d3", 5, 2).is_none(),
            "two places for hosts"
        );
        // Repeatable: the host keeps its place and its first claim.
        let again = claim(SlotClass::Host, "d1", 9, 2).expect("held");
        assert_eq!(again.hosts[0].order, OrderId::new(1, 3));
        assert_eq!(rows.slot().unwrap().value, again);

        let freed = rows.release(&id("s1")).unwrap();
        assert!(freed.storage.is_none() && freed.hosts.len() == 2);
        assert_eq!(rows.release(&id("nobody")).unwrap(), freed);
        let freed = rows.release(&id("d1")).unwrap();
        assert_eq!(freed.hosts.len(), 1);
        assert!(claim(SlotClass::Host, "d3", 6, 2).is_some());
        assert!(claim(SlotClass::Storage, "s2", 7, 2).is_some());
    }
}

/// Catches: a release decided from an older look that frees a place claimed since. `d1` held
/// a place by claim 3, finished, and claimed again by claim 8; a controller that read the
/// first claim and releases it by that claim leaves the second alone, and one that names the
/// current claim frees it. A release by the host alone (the controller's own, after a row it
/// wrote) frees whichever claim it holds.
#[test]
fn a_release_made_from_an_older_look_does_not_free_a_newer_claim() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "stale-release");
        let rows = Rows::new(cell.store());
        let claim = |seq| {
            rows.claim(SlotClass::Host, &id("d1"), OrderId::new(1, seq), 2)
                .unwrap()
                .expect("a place")
        };
        claim(3);
        rows.release(&id("d1")).unwrap();
        let again = claim(8);
        assert_eq!(again.hosts[0].order, OrderId::new(1, 8));

        let stale = rows.release_claim(&id("d1"), OrderId::new(1, 3)).unwrap();
        assert!(stale.holds(&id("d1")), "a newer claim was freed: {stale:?}");
        assert_eq!(rows.slot().unwrap().value, again);
        let fresh = rows.release_claim(&id("d1"), OrderId::new(1, 8)).unwrap();
        assert!(!fresh.holds(&id("d1")));
        assert!(rows.slot().unwrap().value.is_empty());

        claim(9);
        assert!(rows.release(&id("d1")).unwrap().is_empty());
    }
}

/// Catches: a row write that overwrites a row that moved: a write from a stale read is
/// refused with a conflict and changes nothing, a second creation of a row is refused, and
/// the slot's claim loop re-reads instead of writing over a competitor.
#[test]
fn a_write_from_a_stale_read_is_refused_and_changes_nothing() {
    for backing in BACKINGS {
        let cell = Cell::new(backing, "cas");
        let rows = Rows::new(cell.store());
        let absent = Stored {
            value: HostRow::unstarted(),
            version: None,
        };
        let mine = row(HostPhase::Selected);
        rows.put_host(&id("h"), &absent, &mine).unwrap();
        // Created again from the same read: another controller got there first.
        let theirs = row(HostPhase::Cordoned);
        let err = rows.put_host(&id("h"), &absent, &theirs).unwrap_err();
        assert!(matches!(err, RowError::Conflict(_)), "{err:?}");
        let read = rows.hosts().unwrap()[&id("h")].clone();
        assert_eq!(read.value, mine);
        // Moved by someone, then written from the old read.
        rows.put_host(&id("h"), &read, &theirs).unwrap();
        let err = rows
            .put_host(&id("h"), &read, &row(HostPhase::Done))
            .unwrap_err();
        assert!(matches!(err, RowError::Conflict(_)), "{err:?}");
        assert_eq!(rows.hosts().unwrap()[&id("h")].value, theirs);

        let slot_read = rows.slot().unwrap();
        let held = Slot {
            storage: Some(Holder {
                host: id("s1"),
                order: OrderId::new(1, 1),
            }),
            hosts: Vec::new(),
        };
        rows.put_slot(&slot_read, &held).unwrap();
        let err = rows.put_slot(&slot_read, &Slot::default()).unwrap_err();
        assert!(matches!(err, RowError::Conflict(_)), "{err:?}");
        assert_eq!(rows.slot().unwrap().value, held);
    }
}

/// Catches: an order id that repeats. Ids ascend within an epoch, a new epoch starts its own
/// count and is greater than any id of an earlier epoch whatever the count, and on a SQLite
/// file the count goes on where it was after the store is closed and opened again.
#[test]
fn order_ids_ascend_within_an_epoch_and_the_epoch_comes_first() {
    for backing in BACKINGS {
        let mut cell = Cell::new(backing, "orders");
        let mint = |cell: &Cell, epoch| Rows::new(cell.store()).mint_order(epoch).unwrap();
        let a: Vec<OrderId> = (0..5).map(|_| mint(&cell, 3)).collect();
        assert!(a.windows(2).all(|w| w[0] < w[1]), "{a:?}");
        assert!(a.iter().all(|o| o.epoch == 3));
        let b = mint(&cell, 4);
        assert_eq!(b, OrderId::new(4, 1), "an epoch of its own counts from 1");
        assert!(
            b > *a.last().unwrap(),
            "a later epoch is above every id of an earlier one"
        );
        if backing == Backing::Sqlite {
            cell.reopen();
            let c = mint(&cell, 3);
            assert!(c > *a.last().unwrap(), "{c:?} after a reopen, {a:?} before");
            assert_eq!(c.seq, a.last().unwrap().seq + 1, "the count goes on");
        }
    }
}

/// Catches: rows that do not survive the store being closed and opened again, which is
/// what a frontend restart is: every row of the design comes back as written.
#[test]
fn every_row_survives_the_store_being_reopened() {
    let mut cell = Cell::new(Backing::Sqlite, "reopen");
    {
        let rows = Rows::new(cell.store());
        rows.offer(&Release::new("v2"), FarmTime::from_millis(7))
            .unwrap();
        write_host(&rows, "h", &row(HostPhase::Gated));
        rows.claim(SlotClass::Host, &id("h"), OrderId::new(1, 1), 1)
            .unwrap();
        rows.set_policy(&Policy {
            max_hosts_out: 2,
            ..Policy::default()
        })
        .unwrap();
    }
    cell.reopen();
    let rows = Rows::new(cell.store());
    assert_eq!(
        rows.desired().unwrap().value.unwrap().commit,
        Release::new("v2")
    );
    assert_eq!(rows.hosts().unwrap()[&id("h")].value, row(HostPhase::Gated));
    assert!(rows.slot().unwrap().value.holds(&id("h")));
    assert_eq!(rows.policy().unwrap().max_hosts_out, 2);
}
