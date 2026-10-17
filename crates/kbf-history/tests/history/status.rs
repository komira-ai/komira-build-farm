//! The status rows the writer keeps for test shapes: the outcome rules, the flaky proof,
//! the run ring, and a status that survives restarts and races.

use std::sync::atomic::Ordering;

use kbf_history::records::{MemoryKill, RunOutcome, TestStatus};
use kbf_history::status::classify;
use kbf_history::{DerivedLimits, InvRef, Limits, ListQuery, StatusKind};
use kbf_state::{Cond, Store, Txn};
use prost::Message;

use crate::rows_support::{
    CFG, all_statusx, id, mark_run, offer_with, plain_run, restart, run_ids, shape_row, status_key,
    status_row, statusx, test_run,
};
use crate::support::{Life, T, all, digest, run_record};

const S: u64 = 0x5eed;

/// A life with a test shape's runs offered in order, written and drained.
fn life_with(runs: &[(RunOutcome, u8)]) -> (Life, Vec<kbf_history::records::RunRecord>) {
    let mut life = Life::fresh(Limits::default());
    let inv = InvRef::named("b1");
    let made = runs
        .iter()
        .enumerate()
        .map(|(i, (outcome, d))| test_run(&life, &inv, T + i as u64, *outcome, S, *d))
        .collect();
    life.drain(T + 10, 50);
    (life, made)
}

#[test]
fn flaky_is_proven_by_one_digest_seen_both_ways_and_not_by_two_digests() {
    // Catches: flaky claimed for a test that failed on one digest and passed on another
    // (a code change fixed it, which is not flakiness), and a flaky proof missed when
    // the same digest passes and fails.
    let (life, _) = life_with(&[(RunOutcome::Ok, 1), (RunOutcome::Failed, 2)]);
    let row = status_row(&life, S).unwrap();
    assert!(!row.proven, "two digests prove nothing: {row:?}");
    assert_eq!(row.kind(), Some(StatusKind::Failing));
    assert_eq!(row.verdicts.len(), 2);

    let (life, runs) = life_with(&[
        (RunOutcome::Ok, 1),
        (RunOutcome::Failed, 1),
        (RunOutcome::Ok, 1),
    ]);
    let row = status_row(&life, S).unwrap();
    assert!(row.proven);
    assert_eq!(row.proof_digest, digest(1).prefix16());
    assert_eq!(
        mark_run(&row.proven_since),
        Some(id(&runs[1])),
        "the run that completed the proof"
    );
    assert_eq!(
        row.kind(),
        Some(StatusKind::Flaky),
        "passing now, proven flaky"
    );
    assert_eq!(statusx(&life, StatusKind::Flaky).len(), 1);
    assert!(statusx(&life, StatusKind::Passing).is_empty());

    // The proof stays set when the digest is long gone from the last 16.
    let (mut life, _) = life_with(&[(RunOutcome::Ok, 1), (RunOutcome::Failed, 1)]);
    let inv = InvRef::named("b2");
    for d in 10..30 {
        test_run(&life, &inv, T + 100 + u64::from(d), RunOutcome::Ok, S, d);
    }
    life.drain(T + 200, 50);
    let row = status_row(&life, S).unwrap();
    assert!(row.proven && row.proof_digest == digest(1).prefix16());
    assert_eq!(row.verdicts.len(), 16);
    assert!(
        row.verdicts
            .iter()
            .all(|v| v.digest16 != digest(1).prefix16())
    );
}

#[test]
fn infra_never_moves_first_fail_or_flaky() {
    // Catches: an infrastructure failure counted as a test failure: it would set
    // first_fail on a passing test, prove a digest flaky (pass and "fail" on one
    // digest) or count in the transition mask.
    let (life, runs) = life_with(&[
        (RunOutcome::Ok, 1),
        (RunOutcome::Infra, 1),
        (RunOutcome::Lost, 1),
    ]);
    let row = status_row(&life, S).unwrap();
    assert_eq!(row.first_fail, None);
    assert!(!row.proven);
    assert_eq!(row.kind(), Some(StatusKind::Passing));
    assert_eq!(row.masked, 1, "only the pass is in the mask");
    assert_eq!(row.mask, 0);
    assert_eq!(row.ring.len(), 3, "but the runs are in the ring");
    assert_eq!(mark_run(&row.last_pass), Some(id(&runs[0])));

    // After a failure, infra neither moves first_fail nor clears it.
    let (life, runs) = life_with(&[
        (RunOutcome::Ok, 1),
        (RunOutcome::Failed, 2),
        (RunOutcome::Infra, 2),
        (RunOutcome::Failed, 2),
    ]);
    let row = status_row(&life, S).unwrap();
    assert_eq!(mark_run(&row.first_fail), Some(id(&runs[1])));
    assert_eq!(row.kind(), Some(StatusKind::Failing));
    assert_eq!(row.masked, 3);
    assert_eq!(row.mask, 0b011);

    // A pass ends the failure: first_fail is the first failure of the current streak.
    let (life, runs) = life_with(&[
        (RunOutcome::Failed, 1),
        (RunOutcome::Ok, 2),
        (RunOutcome::Failed, 3),
    ]);
    let row = status_row(&life, S).unwrap();
    assert_eq!(mark_run(&row.first_fail), Some(id(&runs[2])));
    let (life, _) = life_with(&[(RunOutcome::Failed, 1), (RunOutcome::Ok, 2)]);
    let row = status_row(&life, S).unwrap();
    assert_eq!(row.first_fail, None, "a passing test has no first failure");

    // A key that has only had infrastructure failures has a row and no status.
    let (life, _) = life_with(&[(RunOutcome::Infra, 1)]);
    let row = status_row(&life, S).unwrap();
    assert_eq!(row.kind(), None);
    assert!(all_statusx(&life).is_empty());
}

#[test]
fn oom_killed_counts_as_a_failure_for_first_fail() {
    // Catches: an own-limit kill that ended the operation read as infra (the test
    // would stay "passing" while its action cannot run on the farm), and a kill
    // counted in the flaky proof (a resource kill on a digest that passed would make
    // the test flaky).
    let mut life = Life::fresh(Limits::default());
    let inv = InvRef::named("b1");
    test_run(&life, &inv, T, RunOutcome::Ok, S, 1);
    let kill = offer_with(&life, &inv, T + 1, "main", RunOutcome::MemoryKilled, |r| {
        r.shape = S;
        r.cfg = CFG.into();
        r.test = true;
        r.action = Some(digest(1));
        r.memory_kill = MemoryKill::OwnLimit.into();
    });
    life.drain(T + 10, 50);
    let row = status_row(&life, S).unwrap();
    assert_eq!(row.kind(), Some(StatusKind::OomKilled));
    assert_eq!(mark_run(&row.first_fail), Some(id(&kill)));
    assert!(
        !row.proven,
        "a kill on a digest that passed is not a flaky proof"
    );
    assert_eq!(statusx(&life, StatusKind::OomKilled).len(), 1);
    assert!(statusx(&life, StatusKind::Passing).is_empty());

    // A later failure of another kind keeps the first failure.
    test_run(&life, &inv, T + 2, RunOutcome::Failed, S, 2);
    life.drain(T + 10, 50);
    let row = status_row(&life, S).unwrap();
    assert_eq!(mark_run(&row.first_fail), Some(id(&kill)));
    assert_eq!(row.kind(), Some(StatusKind::Failing));
    assert_eq!(row.mask, 0b11);
}

#[test]
fn only_results_are_outcomes() {
    // Catches: a kill followed by a ladder rung, a node-pressure kill, a cancel or a
    // run with no outcome counted as a test result.
    let base = |outcome| run_record("main", outcome, T);
    let kill = |kind, raised| {
        let mut r = base(RunOutcome::MemoryKilled);
        r.memory_kill = kind;
        r.raised_memory_bytes = raised;
        r
    };
    let own = i32::from(MemoryKill::OwnLimit);
    let pressure = i32::from(MemoryKill::NodePressure);
    let got = [
        classify(&base(RunOutcome::Ok)),
        classify(&base(RunOutcome::Failed)),
        classify(&base(RunOutcome::TimedOut)),
        classify(&base(RunOutcome::Infra)),
        classify(&base(RunOutcome::Lost)),
        classify(&kill(own, 0)),
        classify(&kill(own, 64)),
        classify(&kill(pressure, 0)),
        classify(&kill(0, 0)),
        classify(&base(RunOutcome::Cancelled)),
        classify(&base(RunOutcome::Unspecified)),
    ];
    assert_eq!(
        got,
        [
            Some(TestStatus::Pass),
            Some(TestStatus::Fail),
            Some(TestStatus::Timeout),
            Some(TestStatus::Infra),
            Some(TestStatus::Infra),
            Some(TestStatus::OomKilled),
            None,
            Some(TestStatus::Infra),
            Some(TestStatus::Infra),
            None,
            None,
        ]
    );
}

#[test]
fn a_restart_continues_the_status_row_instead_of_resetting_it() {
    // Catches: a status row written from the runs a restarted writer has seen, which
    // forgets the failure the test was in and its first_fail, and drops the digest
    // verdicts the flaky proof needs.
    let (life, first) = life_with(&[(RunOutcome::Ok, 1), (RunOutcome::Failed, 2)]);
    let mut again = restart(&life, 2);
    let inv = InvRef::named("b2");
    let third = test_run(&again, &inv, T + 50, RunOutcome::Failed, S, 3);
    again.drain(T + 60, 50);
    let row = status_row(&again, S).unwrap();
    assert_eq!(
        run_ids(&row),
        [id(&first[0]), id(&first[1]), id(&third)],
        "the new life's run joins the old life's"
    );
    assert_eq!(
        mark_run(&row.first_fail),
        Some(id(&first[1])),
        "not the new run"
    );
    assert_eq!(mark_run(&row.last_pass), Some(id(&first[0])));
    assert_eq!(row.mask, 0b011);
    assert_eq!(row.verdicts.len(), 3);
    assert_eq!(statusx(&again, StatusKind::Failing).len(), 1);
}

#[test]
fn statusx_has_one_row_per_key_after_a_status_change() {
    // Catches: a by-status entry added without the old one deleted, which lists the
    // test under both statuses for ever.
    let mut life = Life::fresh(Limits::default());
    let inv = InvRef::named("b1");
    let steps = [
        (RunOutcome::Ok, StatusKind::Passing),
        (RunOutcome::Failed, StatusKind::Failing),
        (RunOutcome::Ok, StatusKind::Passing),
        (RunOutcome::TimedOut, StatusKind::Failing),
    ];
    for (i, (outcome, kind)) in steps.into_iter().enumerate() {
        test_run(&life, &inv, T + i as u64, outcome, S, 1 + i as u8);
        life.drain(T + 10, 50);
        let rows = all_statusx(&life);
        assert_eq!(rows.len(), 1, "after step {i}: {rows:?}");
        assert!(rows[0].starts_with(&life.index_root.statusx_prefix(kind.name())));
    }
    // A run that changes nothing about the status rewrites no by-status row.
    let before = life.index.txns.load(Ordering::SeqCst);
    test_run(&life, &inv, T + 20, RunOutcome::Failed, S, 9);
    life.drain(T + 30, 50);
    assert_eq!(all_statusx(&life).len(), 1);
    assert!(life.index.txns.load(Ordering::SeqCst) > before);
}

#[test]
fn a_failed_status_read_blocks_the_write_instead_of_overwriting() {
    // Catches: a status row written when its stored row could not be read, which
    // replaces the stored status with one built from the runs this life saw.
    let (life, first) = life_with(&[(RunOutcome::Ok, 1), (RunOutcome::Failed, 2)]);
    let key = status_key(&life, "main", S);
    let stored = life.index.inner.get(&key).unwrap().unwrap();
    let mut again = restart(&life, 2);
    let inv = InvRef::named("b2");
    let third = test_run(&again, &inv, T + 50, RunOutcome::Ok, S, 3);
    again.index.fail_gets.store(true, Ordering::SeqCst);
    again.step(T + 60);
    again.step(T + 61);
    assert_eq!(
        life.index.inner.get(&key).unwrap().unwrap(),
        stored,
        "nothing written while the read fails"
    );
    assert_eq!(again.writer.index_owed(), 1, "the run waits");
    again.index.fail_gets.store(false, Ordering::SeqCst);
    again.drain(T + 70, 50);
    let row = status_row(&again, S).unwrap();
    assert_eq!(run_ids(&row), [id(&first[0]), id(&first[1]), id(&third)]);
}

#[test]
fn the_ring_keeps_the_last_16_runs_in_order() {
    // Catches: a ring that keeps the oldest 16 runs, or the newest in the wrong order.
    let mut life = Life::fresh(Limits::default());
    let inv = InvRef::named("b1");
    let made: Vec<_> = (0..20)
        .map(|i| test_run(&life, &inv, T + i, RunOutcome::Ok, S, 1))
        .collect();
    life.drain(T + 100, 50);
    let row = status_row(&life, S).unwrap();
    let want: Vec<_> = made[4..].iter().map(id).collect();
    assert_eq!(run_ids(&row), want);
    assert_eq!(row.masked, 20);
    let entry = &row.ring[15];
    assert_eq!(
        (entry.status, entry.digest16, entry.execution_ms),
        (TestStatus::Pass.into(), digest(1).prefix16(), 0)
    );
    assert_eq!(row.last_seen_ms, T + 19);
}

#[test]
fn a_non_test_shape_has_a_catalog_row_and_no_status_row() {
    // Catches: status rows (and their by-status entries) for every compile and link,
    // which would fill the 100,000-key status bound with shapes that have no status.
    let mut life = Life::fresh(Limits::default());
    let inv = InvRef::named("b1");
    plain_run(&life, &inv, T, 7);
    let named = |t, shape| {
        offer_with(&life, &inv, t, "main", RunOutcome::Ok, |r| {
            r.shape = shape;
            r.cfg = CFG.into();
            r.test = true;
            r.target = "//pkg:t".into();
        })
    };
    named(T + 1, 8);
    // A run whose shape was not parsed names no key.
    offer_with(&life, &inv, T + 2, "main", RunOutcome::Ok, |_| {});
    life.drain(T + 10, 50);
    let other = shape_row(&life, 7).unwrap();
    assert_eq!(other.kind, 0);
    assert_eq!((other.first_seen_ms, other.last_seen_ms), (T, T));
    assert!(status_row(&life, 7).is_none());
    let test = shape_row(&life, 8).unwrap();
    assert_eq!((test.kind, test.target.as_str()), (1, "//pkg:t"));
    assert!(status_row(&life, 8).is_some());
    assert_eq!(life.index.keys("ui/shape/").len(), 2);
    assert_eq!(life.index.keys("ui/status/").len(), 1);
}

#[test]
fn a_catalog_row_is_written_once_and_a_stored_one_is_kept() {
    // Catches: a shape row rewritten by each run or each life, which moves first_seen
    // to the latest sight.
    let (life, _) = life_with(&[(RunOutcome::Ok, 1)]);
    let key = life.index_root.shape("main", S, CFG);
    let stored = life.index.inner.get(&key).unwrap().unwrap();
    let mut again = restart(&life, 2);
    let inv = InvRef::named("b2");
    test_run(&again, &inv, T + 5_000, RunOutcome::Ok, S, 1);
    again.drain(T + 6_000, 50);
    assert_eq!(life.index.inner.get(&key).unwrap().unwrap(), stored);
    assert_eq!(
        again.writer.index_keys(),
        (1, 0),
        "the second life holds it too until forgotten"
    );
}

#[test]
fn a_write_that_landed_unacknowledged_is_not_counted_twice() {
    // Catches: a run applied again when its write is made again after a lost
    // acknowledgement, which doubles the mask and the ring.
    let mut life = Life::fresh(Limits::default());
    life.step(T);
    let inv = InvRef::named("b1");
    let r = test_run(&life, &inv, T + 1, RunOutcome::Failed, S, 1);
    life.index.lose_acks.store(1, Ordering::SeqCst);
    life.step(T + 2);
    assert_eq!(
        life.writer.drops().index,
        2,
        "the shape and status rows, counted"
    );
    life.drain(T + 3, 50);
    let row = status_row(&life, S).unwrap();
    assert_eq!((run_ids(&row), row.masked), (vec![id(&r)], 1));
    assert_eq!(all_statusx(&life).len(), 1);
}

#[test]
fn a_status_write_that_lost_a_race_reads_again_and_keeps_both() {
    // Catches: a status row overwritten when another frontend wrote it between this
    // writer's read and its write (a blind write loses that frontend's runs), or a
    // lost race dropped with its run.
    let (life, first) = life_with(&[(RunOutcome::Ok, 1)]);
    let mut again = restart(&life, 2);
    again.step(T + 45); // reserves its stream block, so the hook meets the status write
    let key = status_key(&life, "main", S);
    let other = {
        let mut row = status_row(&life, S).unwrap();
        row.apply(&kbf_history::status::Observed {
            status: TestStatus::Fail,
            run_id: vec![9; 12],
            time_ms: T + 40,
            execution_ms: 1,
            digest16: 77,
            inv_key: String::new(),
        });
        row.encode_to_vec()
    };
    let planted = key.clone();
    *again.index.before_txn.lock().unwrap() = Some(Box::new(move |store: &dyn Store| {
        store
            .txn(Txn::new().put(planted, other, Cond::Any))
            .unwrap()
            .unwrap();
    }));
    let inv = InvRef::named("b2");
    let mine = test_run(&again, &inv, T + 50, RunOutcome::Failed, S, 2);
    again.drain(T + 60, 50);
    let row = status_row(&again, S).unwrap();
    assert_eq!(run_ids(&row), [id(&first[0]), vec![9; 12], id(&mine)]);
    assert_eq!(row.mask, 0b011);
    assert_eq!(mark_run(&row.first_fail), Some(vec![9; 12]));
}

#[test]
fn a_status_row_that_does_not_decode_is_replaced() {
    // Catches: a broken stored row that wedges its key for good (every write would
    // conflict or be refused) instead of being replaced, as an unreadable header is.
    let mut life = Life::fresh(Limits::default());
    life.index.put(&status_key(&life, "main", S), vec![0xff; 5]);
    let inv = InvRef::named("b1");
    let r = test_run(&life, &inv, T, RunOutcome::Ok, S, 1);
    life.drain(T + 10, 50);
    assert_eq!(run_ids(&status_row(&life, S).unwrap()), [id(&r)]);
}

#[test]
fn state_is_bounded_and_past_it_runs_are_counted() {
    // Catches: unbounded writer memory for keys (a client names the shapes), a key
    // past the bound accepted without a count, and a bound that never frees: keys whose
    // rows exist are forgotten to make room.
    let limits = Limits {
        derived: DerivedLimits {
            keys: 2,
            reads_per_step: 256,
            waiting: 2,
        },
        ..Limits::default()
    };
    let mut life = Life::fresh(limits);
    let inv = InvRef::named("b1");
    for shape in 1..=3 {
        test_run(&life, &inv, T + shape, RunOutcome::Ok, shape, 1);
    }
    life.step(T + 10);
    // Three runs of three shapes offered, two shapes and two tests held: the third run
    // is refused and counted once.
    assert_eq!(life.writer.index_keys(), (2, 2));
    assert_eq!(life.writer.drops().index, 1);
    life.drain(T + 20, 50);
    assert!(status_row(&life, 3).is_none() && shape_row(&life, 3).is_none());
    // The rows exist now, so a new key makes room by forgetting them.
    test_run(&life, &inv, T + 30, RunOutcome::Ok, 4, 1);
    life.drain(T + 40, 50);
    assert!(shape_row(&life, 4).is_some() && status_row(&life, 4).is_some());
    // Runs of one key past the waiting bound are counted too.
    let before = life.writer.drops().index;
    for i in 0..3 {
        test_run(&life, &inv, T + 50 + i, RunOutcome::Ok, 4, 1);
    }
    life.step(T + 60);
    assert_eq!(life.writer.drops().index, before + 1);
}

#[test]
fn an_id_less_run_is_a_test_result_but_is_listed_nowhere() {
    // Catches: an id-less pseudo-invocation listed in `/v1/invocations`, and its test
    // runs lost from the status row.
    let mut life = Life::fresh(Limits::default());
    let day = kbf_history::Day::from_ms(T);
    let inv = InvRef::idless(life.writer_id, day);
    let r = test_run(&life, &inv, T, RunOutcome::Failed, S, 1);
    life.close(&inv);
    life.drain(T + 10, 50);
    assert!(
        life.index.keys("ui/invid/").len() == 1,
        "its header is written"
    );
    let row = status_row(&life, S).unwrap();
    assert_eq!(run_ids(&row), [id(&r)]);
    assert_eq!(row.first_fail.unwrap().inv_key, "", "no invocation to name");
    assert!(life.index.keys("ui/inv/").is_empty());
    let page = futures::executor::block_on(
        life.reader()
            .list_invocations(&ListQuery::new("main", 10), &all),
    )
    .unwrap();
    assert!(page.items.is_empty());
}

#[test]
fn a_run_with_no_digest_is_no_evidence_for_the_flaky_proof() {
    // Catches: runs with no action digest all sharing digest 0, so any pass and any
    // failure of such a test would prove it flaky.
    let mut life = Life::fresh(Limits::default());
    let inv = InvRef::named("b1");
    for (i, outcome) in [RunOutcome::Ok, RunOutcome::Failed].into_iter().enumerate() {
        offer_with(&life, &inv, T + i as u64, "main", outcome, |r| {
            r.shape = S;
            r.cfg = CFG.into();
            r.test = true;
            r.action = None;
        });
    }
    life.drain(T + 10, 50);
    let row = status_row(&life, S).unwrap();
    assert!(!row.proven && row.verdicts.is_empty());
    assert_eq!((row.masked, row.mask), (2, 0b01));
    assert_eq!(row.kind(), Some(StatusKind::Failing));
}

#[test]
fn a_kill_followed_by_a_rung_makes_no_status() {
    // Catches: the intermediate kills of the memory ladder counted as test failures,
    // which would show a test failing while the farm is still finding it room.
    let mut life = Life::fresh(Limits::default());
    let inv = InvRef::named("b1");
    offer_with(&life, &inv, T, "main", RunOutcome::MemoryKilled, |r| {
        r.shape = S;
        r.cfg = CFG.into();
        r.test = true;
        r.memory_kill = MemoryKill::OwnLimit.into();
        r.raised_memory_bytes = 1 << 30;
    });
    life.drain(T + 10, 50);
    assert!(status_row(&life, S).is_none(), "not a result");
    assert!(shape_row(&life, S).is_some(), "but the shape was seen");
}

#[test]
fn status_names_round_trip() {
    // Catches: a by-status name that does not read back, or an unknown one accepted.
    for kind in StatusKind::ALL {
        assert_eq!(StatusKind::parse(kind.name()), Some(kind));
    }
    assert_eq!(StatusKind::parse("slower"), None);
}

#[test]
fn runs_that_arrive_while_a_write_waits_are_written_after_it() {
    // Catches: a status write that carries runs which arrived after it was made taken
    // for written (dropped from the slot at its commit), and a slot that forgets the
    // runs waiting behind a write in flight.
    let mut life = Life::fresh(Limits::default());
    let inv = InvRef::named("b1");
    let first = test_run(&life, &inv, T, RunOutcome::Ok, S, 1);
    life.step(T); // the stream block takes the step's transaction: the row waits
    assert!(status_row(&life, S).is_none());
    let second = test_run(&life, &inv, T + 1, RunOutcome::Failed, S, 2);
    life.step(T + 1);
    let row = status_row(&life, S).unwrap();
    assert_eq!(
        run_ids(&row),
        [id(&first)],
        "the write made before the second run"
    );
    assert_eq!(
        life.writer.index_owed(),
        1,
        "the second run waits behind it"
    );
    life.drain(T + 2, 50);
    let row = status_row(&life, S).unwrap();
    assert_eq!(run_ids(&row), [id(&first), id(&second)]);
    assert_eq!(mark_run(&row.first_fail), Some(id(&second)));
}
