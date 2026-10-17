//! The cross-build reads: the invocation list, the instances, the catalog and the tests
//! by status, and what the writer's index rows must be for them.

use std::sync::atomic::Ordering;

use futures::executor::block_on as run;
use kbf_history::records::{InvState, InvocationHeader, RunOutcome};
use kbf_history::{
    InvRef, Limits, ListQuery, MAX_PAGE, PageCursor, PendingLimits, Reader, StatusKind,
};
use kbf_state::{Cond, Txn};
use prost::Message;

use crate::rows_support::{CFG, key, plain_run, test_run};
use crate::support::{Index, Life, Objects, T, all};

fn page(
    reader: &Reader<Objects>,
    q: &ListQuery<'_>,
    allow: &dyn Fn(&str) -> bool,
) -> (Vec<kbf_history::InvKey>, Option<PageCursor>) {
    let page = futures::executor::block_on(reader.list_invocations(q, allow)).unwrap();
    (page.items.into_iter().map(|i| i.inv).collect(), page.next)
}

/// Offers one run of `instance` for each of `names`, a second apart from `T`, closed.
fn builds(life: &mut Life, instance: &str, names: &[&str], from: u64) {
    for (i, name) in names.iter().enumerate() {
        let inv = InvRef::named(name);
        life.run(&inv, T + from + i as u64 * 1000, instance, RunOutcome::Ok);
        life.close(&inv);
    }
}

fn keys(names: &[&str]) -> Vec<kbf_history::InvKey> {
    names.iter().map(|n| key(n)).collect()
}

fn only(instance: &'static str) -> impl Fn(&str) -> bool {
    move |i| i == instance
}

#[test]
fn list_is_newest_first_and_pages_stay_stable_while_rows_arrive() {
    // Catches: an oldest-first list, a page that repeats or skips a row when new
    // invocations arrive between pages (a position by offset rather than by key), and
    // a time range that drops its end points.
    let mut life = Life::fresh(Limits::default());
    builds(&mut life, "main", &["b0", "b1", "b2", "b3", "b4"], 0);
    life.drain(T + 10_000, 50);
    let reader = life.reader();
    let q = ListQuery::new("main", 2);
    let (first, next) = page(&reader, &q, &all);
    assert_eq!(first, keys(&["b4", "b3"]));
    let next = next.expect("more rows follow");
    assert_eq!(next.to_string().parse::<PageCursor>(), Ok(next.clone()));

    builds(&mut life, "main", &["b5"], 6_000);
    life.drain(T + 10_000, 50);
    let q2 = ListQuery {
        after: Some(&next),
        ..ListQuery::new("main", 2)
    };
    let (second, next2) = page(&reader, &q2, &all);
    assert_eq!(
        second,
        keys(&["b2", "b1"]),
        "b5 arrived newer than the cursor"
    );
    let next2 = next2.unwrap();
    let q3 = ListQuery {
        after: Some(&next2),
        ..ListQuery::new("main", 2)
    };
    assert_eq!(page(&reader, &q3, &all), (keys(&["b0"]), None));
    assert_eq!(page(&reader, &q, &all).0, keys(&["b5", "b4"]));

    let in_range = ListQuery {
        since_ms: T + 1_000,
        until_ms: T + 3_000,
        ..ListQuery::new("main", 10)
    };
    assert_eq!(
        page(&reader, &in_range, &all),
        (keys(&["b3", "b2", "b1"]), None),
        "both ends are included"
    );
    // A page holds at most MAX_PAGE, and at least one.
    assert_eq!(page(&reader, &ListQuery::new("main", 0), &all).0.len(), 1);
    assert_eq!(
        page(&reader, &ListQuery::new("main", MAX_PAGE * 2), &all)
            .0
            .len(),
        6
    );
    let at_the_end = ListQuery {
        until_ms: u64::MAX - 1,
        ..ListQuery::new("main", 1)
    };
    assert_eq!(page(&reader, &at_the_end, &all).0, keys(&["b5"]));
}

#[test]
fn list_hides_what_the_caller_may_not_read() {
    // Catches: a list that ignores the instance, one that shows another instance's
    // builds, and an invocation spanning instances shown to a caller allowed on only one
    // of them (all-must-allow).
    let mut life = Life::fresh(Limits::default());
    builds(&mut life, "main", &["solo-main"], 0);
    builds(&mut life, "other", &["solo-other"], 100);
    builds(&mut life, "third", &["only-third"], 200);
    let span = InvRef::named("span");
    life.run(&span, T + 1_000, "main", RunOutcome::Ok);
    life.run(&span, T + 1_001, "other", RunOutcome::Ok);
    life.close(&span);
    life.drain(T + 10_000, 50);
    let reader = life.reader();
    let q = ListQuery::new("main", 10);
    assert_eq!(page(&reader, &q, &all).0, keys(&["span", "solo-main"]));
    assert_eq!(page(&reader, &q, &only("main")).0, keys(&["solo-main"]));
    let q = ListQuery::new("other", 10);
    assert_eq!(page(&reader, &q, &all).0, keys(&["span", "solo-other"]));
    let scans = life.index.scans.load(Ordering::SeqCst);
    assert_eq!(
        page(&reader, &q, &only("main")).0,
        Vec::<kbf_history::InvKey>::new()
    );
    assert_eq!(
        life.index.scans.load(Ordering::SeqCst),
        scans,
        "an instance the caller may not read is not even scanned"
    );
    assert_eq!(page(&reader, &q, &only("other")).0, keys(&["solo-other"]));

    let names = |allow: &dyn Fn(&str) -> bool| {
        futures::executor::block_on(reader.instances(allow)).unwrap()
    };
    let got = names(&all);
    assert_eq!(got.names, ["main", "other", "third"]);
    assert!(!got.truncated);
    assert_eq!(names(&only("main")).names, ["main"]);
}

#[test]
fn a_dropped_inv_shape_or_status_row_comes_back_when_the_backlog_drains() {
    // Catches: a list entry, catalog row or status row lost with a failed index
    // transaction and never written again (the invocation would never be listed, the
    // test would have no status).
    let mut life = Life::fresh(Limits::default());
    life.step(T); // reserves the stream block
    let inv = InvRef::named("b1");
    test_run(&life, &inv, T + 1, RunOutcome::Failed, 3, 1);
    life.close(&inv);
    life.index.fail_txns.store(1, Ordering::SeqCst);
    life.step(T + 2); // shape and status rows, and the chunk's rows, one transaction
    assert!(life.writer.drops().index >= 3, "{:?}", life.writer.drops());
    assert!(life.index.keys("ui/inv/").is_empty());
    life.drain(T + 3, 50);
    life.step(T + 4);
    assert_eq!(life.index.keys("ui/inv/").len(), 1);
    assert_eq!(life.index.keys("ui/shape/").len(), 1);
    assert_eq!(life.index.keys("ui/status/").len(), 1);
    assert_eq!(life.index.keys("ui/statusx/").len(), 1);
    let listed = page(&life.reader(), &ListQuery::new("main", 5), &all).0;
    assert_eq!(listed, keys(&["b1"]));
    assert_eq!(life.writer.open_invocations(), 0, "forgotten once listed");
}

#[test]
fn rows_a_full_backlog_refuses_come_back_too() {
    // Catches: the same loss when the pending bound refuses the row rather than the
    // transaction failing: the refusal must leave the owed row held, not forgotten.
    let limits = Limits {
        pending: PendingLimits {
            keys: 4,
            bytes: 1 << 20,
        },
        txn_rows: 1,
        ..Limits::default()
    };
    let mut life = Life::fresh(limits);
    let inv = InvRef::named("b1");
    for shape in 1..=3 {
        test_run(&life, &inv, T + shape, RunOutcome::Failed, shape, 1);
    }
    life.close(&inv);
    life.drain(T + 10, 200);
    for _ in 0..30 {
        life.step(T + 11);
    }
    assert_eq!(life.index.keys("ui/shape/").len(), 3);
    assert_eq!(life.index.keys("ui/status/").len(), 3);
    assert_eq!(life.index.keys("ui/inv/").len(), 1);
    assert!(life.writer.drops().index > 0, "refusals were counted");
}

#[test]
fn a_restart_keeps_one_list_entry_at_the_first_seen_time() {
    // Catches: a second list entry for an invocation continued after a restart (keyed
    // by the new life's first record), which lists the build twice, and an instance
    // joining later getting an entry at another time.
    let mut life = Life::fresh(Limits::default());
    let inv = InvRef::named("b1");
    life.run(&inv, T + 5_000, "main", RunOutcome::Ok);
    life.close(&inv);
    life.drain(T + 130_000, 50);
    let mut again = crate::rows_support::restart(&life, 2);
    again.run(&inv, T + 4_000, "main", RunOutcome::Ok);
    again.run(&inv, T + 6_000, "other", RunOutcome::Ok);
    again.close(&inv);
    again.drain(T + 300_000, 50);
    let rows = life.index.keys("ui/inv/");
    assert_eq!(
        rows,
        [
            format!("ui/inv/main/{:012x}/{}", T + 5_000, inv.key),
            format!("ui/inv/other/{:012x}/{}", T + 5_000, inv.key),
        ]
    );
    let listed = page(&again.reader(), &ListQuery::new("other", 5), &all).0;
    assert_eq!(listed, [inv.key]);
}

#[test]
fn a_list_entry_the_header_does_not_name_is_hidden() {
    // Catches: two frontends that listed one invocation at different times showing it
    // twice, and an entry whose header is gone showing a build that cannot be read.
    let mut life = Life::fresh(Limits::default());
    builds(&mut life, "main", &["b1"], 0);
    life.drain(T + 10_000, 50);
    let inv = key("b1");
    let stale = format!("ui/inv/main/{:012x}/{inv}", T + 50);
    life.index.put(&stale, Vec::new());
    let orphan = key("never-written");
    life.index
        .put(&format!("ui/inv/main/{:012x}/{orphan}", T + 60), Vec::new());
    life.index.put("ui/inv/main/not-a-time", Vec::new());
    let reader = life.reader();
    assert_eq!(
        page(&reader, &ListQuery::new("main", 10), &all),
        (vec![inv], None)
    );
}

#[test]
fn the_state_filter_reads_headers_and_the_scan_stops_at_its_bound() {
    // Catches: a filter that matches little scanning the whole instance in one
    // request, and a page cut at the bound that loses its place (no cursor) or repeats.
    let mut life = Life::fresh(Limits::default());
    builds(&mut life, "main", &["c0", "c1", "c2"], 0);
    let open = InvRef::named("still-open");
    life.run(&open, T + 500, "main", RunOutcome::Ok);
    life.drain(T + 10_000, 50);
    life.step(T + 200_000); // the open build's chunk is cut by age
    life.drain(T + 200_001, 50);
    let reader = life.reader().scanning_at_most(2);
    let open_only = ListQuery {
        state: Some(InvState::Open),
        ..ListQuery::new("main", 5)
    };
    // Newest first: c2, c1 (closed, skipped), then c0 and the open one.
    let (items, next) = page(&reader, &open_only, &all);
    assert!(items.is_empty());
    let first = next.expect("the scan stopped at its bound, not at the end");
    let q = ListQuery {
        after: Some(&first),
        ..open_only.clone()
    };
    let (items, next) = page(&reader, &q, &all);
    assert_eq!(items, std::slice::from_ref(&open.key));
    assert!(next.is_none(), "{next:?}");
    // The same filter with room finds it in one page.
    let wide = life.reader();
    assert_eq!(
        page(&wide, &open_only, &all),
        (vec![open.key.clone()], None)
    );
    let closed = ListQuery {
        state: Some(InvState::Closed),
        ..ListQuery::new("main", 5)
    };
    assert_eq!(page(&wide, &closed, &all).0, keys(&["c2", "c1", "c0"]));
}

/// A list of `n` invocations of `instance` written straight to the index, as a farm
/// that has run for months would have.
fn fixture(index: &Index, instance: &str, n: u64) {
    let rows = kbf_history::IndexRoot::default();
    let mut txn = Txn::new();
    for i in 0..n {
        let inv = kbf_history::InvKey::from_record_id(&format!("old-{i}"));
        let header = InvocationHeader {
            record_id: format!("old-{i}"),
            instances: vec![instance.into()],
            first_seen_ms: T + i,
            last_seen_ms: T + i,
            list_ms: T + i,
            state: InvState::Closed.into(),
            ..InvocationHeader::default()
        };
        txn = txn
            .put(rows.invid(&inv), header.encode_to_vec(), Cond::Any)
            .put(rows.inv_list(instance, T + i, &inv), Vec::new(), Cond::Any);
        if txn.ops().len() >= 2_000 {
            index.inner.txn(std::mem::take(&mut txn)).unwrap().unwrap();
        }
    }
    index.inner.txn(txn).unwrap().unwrap();
}

#[test]
fn list_reads_a_bounded_range_not_the_whole_instance() {
    // Catches: a list that reads every row of the instance and cuts the page after
    // (scan_prefix then truncate): with 20,000 listed it would return 20,000 rows for
    // a page of 20.
    let life = Life::fresh(Limits::default());
    fixture(&life.index, "main", 20_000);
    let reader = life.reader();
    let (items, next) = page(&reader, &ListQuery::new("main", 20), &all);
    assert_eq!(items.len(), 20);
    assert!(next.is_some());
    let read = life.index.rows_read.load(Ordering::SeqCst);
    assert!(read <= 300, "a page of 20 read {read} index rows");
    // The default bound: a filter that matches nothing scans 10,000 rows and stops.
    let nothing = ListQuery {
        state: Some(InvState::Open),
        ..ListQuery::new("main", 20)
    };
    life.index.rows_read.store(0, Ordering::SeqCst);
    let (items, next) = page(&reader, &nothing, &all);
    assert!(items.is_empty() && next.is_some());
    let scanned = life.index.rows_read.load(Ordering::SeqCst);
    assert!(
        (10_000..11_000).contains(&scanned),
        "scanned {scanned} rows"
    );
}

#[test]
fn an_instance_with_slashes_cannot_reach_another_instances_rows() {
    // Catches: an instance name used raw in a key: `a/b` would list under `a`, an
    // instance named like a time-and-key tail would plant entries in another's range,
    // and names would not come back from `instances()` as they went in.
    let mut life = Life::fresh(Limits::default());
    builds(&mut life, "a", &["in-a"], 0);
    builds(&mut life, "a/b", &["in-a-b"], 100);
    builds(&mut life, "", &["in-empty"], 200);
    builds(&mut life, "50%/~", &["in-odd"], 300);
    life.drain(T + 10_000, 50);
    let reader = life.reader();
    assert_eq!(
        page(&reader, &ListQuery::new("a", 10), &all).0,
        keys(&["in-a"])
    );
    assert_eq!(
        page(&reader, &ListQuery::new("a/b", 10), &all).0,
        keys(&["in-a-b"])
    );
    assert_eq!(
        page(&reader, &ListQuery::new("", 10), &all).0,
        keys(&["in-empty"])
    );
    assert_eq!(
        page(&reader, &ListQuery::new("50%/~", 10), &all).0,
        keys(&["in-odd"])
    );
    let got = futures::executor::block_on(reader.instances(&all)).unwrap();
    let mut want = vec!["", "50%/~", "a", "a/b"];
    // Key order: `a%2Fb/` sorts before `a/`, and the empty name (`~`) last.
    want.sort_by_key(|n| format!("{}/", kbf_history::IndexRoot::segment(n)));
    assert_eq!(got.names, want);
}

#[test]
fn instances_are_bounded_and_broken_keys_are_skipped() {
    // Catches: an instance list that reads without end when a client names thousands
    // of instances, and a malformed segment aborting the whole list.
    let life = Life::fresh(Limits::default());
    let inv = key("x");
    let mut txn = Txn::new();
    for i in 0..300 {
        let name = format!("i{i:03}");
        txn = txn.put(
            format!("ui/inv/{name}/{:012x}/{inv}", T),
            Vec::new(),
            Cond::Any,
        );
    }
    txn = txn.put(
        format!("ui/inv/%ZZ/{:012x}/{inv}", T),
        Vec::new(),
        Cond::Any,
    );
    life.index.inner.txn(txn).unwrap().unwrap();
    let got = futures::executor::block_on(life.reader().instances(&all)).unwrap();
    assert!(got.truncated);
    assert_eq!(got.names.len(), 256 - 1, "256 read, one of them broken");
    assert_eq!(got.names[0], "i000");
    let empty = Life::fresh(Limits::default());
    let got = futures::executor::block_on(empty.reader().instances(&all)).unwrap();
    assert_eq!((got.names.len(), got.truncated), (0, false));
}

#[test]
fn shapes_and_statuses_read_back_by_key_and_by_status_for_the_instances_allowed() {
    // Catches: a catalog or status read that answers an instance the caller may not
    // read, a by-status list that shows another instance's tests, a stale by-status
    // entry (a move in flight) listing a test under the wrong status, and a page that
    // loses its place.
    let mut life = Life::fresh(Limits::default());
    let inv = InvRef::named("b1");
    for shape in 1..=3 {
        test_run(&life, &inv, T + shape, RunOutcome::Failed, shape, 1);
    }
    test_run(&life, &inv, T + 10, RunOutcome::Ok, 4, 1);
    plain_run(&life, &inv, T + 11, 5);
    crate::rows_support::offer_with(&life, &inv, T + 12, "other", RunOutcome::Failed, |r| {
        r.shape = 9;
        r.cfg = CFG.into();
        r.test = true;
    });
    life.close(&inv);
    life.drain(T + 100, 50);
    let reader = life.reader();

    let shapes = run(reader.shapes("main", None, 3, &all)).unwrap();
    let got: Vec<_> = shapes
        .items
        .iter()
        .map(|e| (e.shape, e.cfg.as_str()))
        .collect();
    assert_eq!(got, [(1, CFG), (2, CFG), (3, CFG)]);
    let next = shapes.next.unwrap();
    let rest = run(reader.shapes("main", Some(&next), 3, &all)).unwrap();
    let got: Vec<_> = rest.items.iter().map(|e| e.shape).collect();
    assert_eq!((got, rest.next), (vec![4, 5], None));
    assert!(
        run(reader.shapes("main", None, 3, &only("other")))
            .unwrap()
            .items
            .is_empty()
    );
    let one = run(reader.shape("main", 4, CFG, &all)).unwrap().unwrap();
    assert_eq!(
        (one.instance.as_str(), one.shape, one.row.kind),
        ("main", 4, 1)
    );
    assert!(
        run(reader.shape("main", 4, CFG, &only("other")))
            .unwrap()
            .is_none()
    );
    assert!(
        run(reader.shape("main", 4, "nope", &all))
            .unwrap()
            .is_none()
    );

    let status = run(reader.status("main", 1, CFG, &all)).unwrap().unwrap();
    assert_eq!(status.row.kind(), Some(StatusKind::Failing));
    assert!(
        run(reader.status("main", 1, CFG, &only("other")))
            .unwrap()
            .is_none()
    );
    assert!(run(reader.status("main", 5, CFG, &all)).unwrap().is_none());

    let failing = run(reader.by_status(StatusKind::Failing, None, 2, &all)).unwrap();
    let got: Vec<_> = failing
        .items
        .iter()
        .map(|e| (e.instance.as_str(), e.shape))
        .collect();
    assert_eq!(got, [("main", 1), ("main", 2)]);
    let more = run(reader.by_status(StatusKind::Failing, failing.next.as_ref(), 2, &all)).unwrap();
    let got: Vec<_> = more
        .items
        .iter()
        .map(|e| (e.instance.as_str(), e.shape))
        .collect();
    assert_eq!((got, more.next), (vec![("main", 3), ("other", 9)], None));
    let mine = run(reader.by_status(StatusKind::Failing, None, 10, &only("main"))).unwrap();
    assert_eq!(mine.items.len(), 3);
    let theirs = run(reader.by_status(StatusKind::Failing, None, 10, &only("other"))).unwrap();
    assert_eq!(theirs.items.len(), 1);
    assert_eq!(theirs.items[0].instance, "other");
    let passing = run(reader.by_status(StatusKind::Passing, None, 10, &all)).unwrap();
    assert_eq!(passing.items.len(), 1);
    assert!(
        run(reader.by_status(StatusKind::Flaky, None, 10, &all))
            .unwrap()
            .items
            .is_empty()
    );

    // An entry whose status row says another status, or is gone, or whose key is not
    // one of a test, is left out.
    let rows = &life.index_root;
    life.index
        .put(&rows.statusx("flaky", "main", 1, CFG), Vec::new());
    life.index
        .put(&rows.statusx("flaky", "main", 77, CFG), Vec::new());
    life.index
        .put(&format!("{}junk", rows.statusx_prefix("flaky")), Vec::new());
    life.index.put(
        &format!("{}main/zz/x", rows.statusx_prefix("flaky")),
        Vec::new(),
    );
    assert!(
        run(reader.by_status(StatusKind::Flaky, None, 10, &all))
            .unwrap()
            .items
            .is_empty()
    );
    life.index
        .put(&format!("{}main/zz", rows.shape_prefix("main")), vec![1]);
    assert_eq!(
        run(reader.shapes("main", None, 50, &all))
            .unwrap()
            .items
            .len(),
        5
    );
}

#[test]
fn broken_rows_are_errors_and_cursors_are_checked() {
    // Catches: a header or catalog row that does not decode answered as empty (a hole
    // in the list that nobody sees), and a cursor that could carry control characters
    // or any length into a key.
    let mut life = Life::fresh(Limits::default());
    builds(&mut life, "main", &["b1"], 0);
    life.drain(T + 10_000, 50);
    let inv = key("b1");
    life.index.put(&life.index_root.invid(&inv), vec![0xff; 4]);
    let reader = life.reader();
    let q = ListQuery::new("main", 5);
    let err = futures::executor::block_on(reader.list_invocations(&q, &all)).unwrap_err();
    assert!(err.to_string().contains("does not decode"), "{err}");
    life.index
        .put(&life.index_root.shape("main", 1, CFG), vec![0xff; 4]);
    let err = futures::executor::block_on(reader.shapes("main", None, 5, &all)).unwrap_err();
    assert!(err.to_string().contains("does not decode"), "{err}");
    for bad in ["", "a\nb", &"x".repeat(1025)] {
        assert!(bad.parse::<PageCursor>().is_err(), "{bad:?}");
    }
    assert!(
        "00000000000a/0123456789abcdef"
            .parse::<PageCursor>()
            .is_ok()
    );
    let text = "not a cursor".parse::<PageCursor>().unwrap();
    assert_eq!(text.to_string(), "not a cursor");
    let e = "".parse::<PageCursor>().unwrap_err();
    assert!(e.to_string().contains("not a page cursor"));
}

#[test]
fn an_id_less_header_is_not_listed_even_with_a_list_entry() {
    // Catches: an id-less pseudo-invocation shown in a list because something wrote an
    // entry for it (it has no client id to name it by).
    let life = Life::fresh(Limits::default());
    let inv = key("pseudo");
    let header = InvocationHeader {
        idless: true,
        instances: vec!["main".into()],
        list_ms: T,
        ..InvocationHeader::default()
    };
    life.index
        .put(&life.index_root.invid(&inv), header.encode_to_vec());
    life.index
        .put(&life.index_root.inv_list("main", T, &inv), Vec::new());
    assert!(
        page(&life.reader(), &ListQuery::new("main", 5), &all)
            .0
            .is_empty()
    );
}

#[test]
fn a_forward_scan_goes_on_past_a_chunk_of_rows_the_caller_may_not_read() {
    // Catches: a by-status page that stops at the first chunk of hidden rows and
    // answers empty with no cursor, and one that never reaches the rows after them.
    let mut life = Life::fresh(Limits::default());
    let inv = InvRef::named("b1");
    test_run(&life, &inv, T, RunOutcome::Failed, 1, 1);
    life.close(&inv);
    life.drain(T + 10, 50);
    // 300 tests of an instance sorting before `main`, then a bad segment before those.
    let rows = &life.index_root;
    let mut txn = Txn::new();
    for shape in 0..300 {
        txn = txn.put(
            rows.statusx("failing", "a-hidden", shape, CFG),
            Vec::new(),
            Cond::Any,
        );
    }
    txn = txn.put(
        format!("{}%ZZ/{:016x}/x", rows.statusx_prefix("failing"), 1),
        Vec::new(),
        Cond::Any,
    );
    life.index.inner.txn(txn).unwrap().unwrap();
    let got = run(life
        .reader()
        .by_status(StatusKind::Failing, None, 5, &only("main")))
    .unwrap();
    assert_eq!(got.items.len(), 1);
    assert_eq!(got.items[0].instance, "main");
    assert!(got.next.is_none());
}

#[test]
fn an_entry_keeps_its_time_while_later_records_move_the_header() {
    // Catches: the entry's time following the header's last record time (the header
    // would then name a time no entry has, and the build would drop out of the list as
    // soon as a second record arrived).
    let mut life = Life::fresh(Limits::default());
    let inv = InvRef::named("b1");
    life.run(&inv, T + 5_000, "main", RunOutcome::Ok);
    life.step(T + 5_000);
    life.step(T + 200_000); // cut by age: the header is written
    life.run(&inv, T + 9_000, "main", RunOutcome::Ok);
    life.run(&inv, T + 7_000, "main", RunOutcome::Ok);
    life.step(T + 210_000);
    life.step(T + 400_000);
    life.close(&inv);
    life.drain(T + 400_001, 50);
    assert_eq!(
        life.index.keys("ui/inv/"),
        [format!("ui/inv/main/{:012x}/{}", T + 5_000, inv.key)]
    );
    let got = run(life
        .reader()
        .list_invocations(&ListQuery::new("main", 5), &all))
    .unwrap();
    assert_eq!(got.items.len(), 1);
    let header = &got.items[0].header;
    assert_eq!((header.runs, header.list_ms), (3, T + 5_000));
    assert_eq!(
        (header.first_seen_ms, header.last_seen_ms),
        (T + 5_000, T + 9_000)
    );
}

#[test]
fn an_entry_is_put_once_not_with_every_chunk_s_header() {
    // Catches: every header rewrite carrying the list entry again (one more row write
    // per chunk of every build, for a row that is there).
    let mut life = Life::fresh(Limits::default());
    let inv = InvRef::named("b1");
    life.run(&inv, T, "main", RunOutcome::Ok);
    life.step(T);
    life.step(T + 120_000);
    life.drain(T + 120_001, 50);
    let entry = life.index_root.inv_list("main", T, &inv.key);
    assert!(life.index.inner.get(&entry).unwrap().is_some());
    life.index
        .inner
        .txn(Txn::new().delete(entry.clone(), Cond::Any))
        .unwrap()
        .unwrap();
    life.run(&inv, T + 130_000, "main", RunOutcome::Ok);
    life.step(T + 130_000);
    life.step(T + 260_000);
    life.drain(T + 260_001, 50);
    assert_eq!(life.index.keys("ui/invid/").len(), 1);
    assert!(
        life.index.inner.get(&entry).unwrap().is_none(),
        "the second chunk's header did not put it again"
    );
}

#[test]
fn a_stored_header_with_no_list_time_takes_the_one_the_writer_chose() {
    // Catches: a header written before invocations were listed (no list time) merged
    // with the writer's into a header that still has none, so its entry would be
    // keyed at time 0 and the build would never appear.
    let mut life = Life::fresh(Limits::default());
    let inv = InvRef::named("b1");
    let stored = InvocationHeader {
        record_id: "b1".into(),
        instances: vec!["main".into()],
        first_seen_ms: T,
        last_seen_ms: T,
        ..InvocationHeader::default()
    };
    life.index
        .put(&life.index_root.invid(&inv.key), stored.encode_to_vec());
    life.run(&inv, T + 7_000, "main", RunOutcome::Ok);
    life.close(&inv);
    life.drain(T + 8_000, 50);
    assert_eq!(
        life.index.keys("ui/inv/"),
        [format!("ui/inv/main/{:012x}/{}", T + 7_000, inv.key)]
    );
    assert_eq!(
        page(&life.reader(), &ListQuery::new("main", 5), &all).0,
        [inv.key]
    );
}

#[test]
fn a_page_cut_at_the_end_of_a_scan_chunk_has_a_cursor_for_what_follows() {
    // Catches: a full page that ends exactly where one scan chunk ends answering no
    // cursor, which hides every row after it.
    let mut life = Life::fresh(Limits::default());
    builds(&mut life, "main", &["d0", "d1", "d2", "d3", "d4"], 0);
    life.drain(T + 10_000, 50);
    let reader = life.reader().scanning_at_most(2);
    let (items, next) = page(&reader, &ListQuery::new("main", 2), &all);
    assert_eq!(items, keys(&["d4", "d3"]));
    let next = next.expect("three more rows follow");
    let q = ListQuery {
        after: Some(&next),
        ..ListQuery::new("main", 2)
    };
    assert_eq!(page(&reader, &q, &all).0, keys(&["d2", "d1"]));
}
