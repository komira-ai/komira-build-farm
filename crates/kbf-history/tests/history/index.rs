//! The index seam: every blocking call through the offload, a call that never runs,
//! and rows under a root of the deployment's choosing.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use kbf_history::records::RunOutcome;
use kbf_history::{IndexRoot, InvRef, Limits, ReadError, Reader};

use crate::support::{Index, Life, Never, Objects, T, Threaded, all, root, whole};

#[test]
fn every_index_call_runs_through_the_offload_off_the_polling_thread() {
    // Catches: a blocking state-store call made inline in the writer's or reader's
    // `async fn` (an HTTP handler awaiting it would stall its runtime worker on SQLite
    // or Postgres I/O). The index panics on a call from the thread that polls.
    let offload = Arc::new(Threaded::default());
    let index = Index::fake();
    let mut life = Life::start_with(
        &Objects::new(),
        &index,
        1,
        Limits::default(),
        IndexRoot::default(),
        offload.clone(),
    );
    *index.forbid.lock().unwrap() = Some(std::thread::current().id());
    life.step(T);
    let inv = InvRef::named("b1");
    let r = life.run(&inv, T, "main", RunOutcome::Failed);
    life.close(&inv);
    life.step(T);
    let reader = life.reader();
    let got = futures::executor::block_on(reader.run(r.run_id(), &all))
        .unwrap()
        .unwrap();
    assert!(got.failed.is_some());
    let view = whole(&reader, &inv.key, &all).unwrap().unwrap();
    assert!(view.complete);
    let made = index.txns.load(Ordering::SeqCst)
        + index.gets.load(Ordering::SeqCst)
        + index.scans.load(Ordering::SeqCst);
    assert!(
        made >= 5,
        "a refill, a header read, rows, two lookups: {made}"
    );
    assert_eq!(
        offload.jobs.load(Ordering::SeqCst),
        made,
        "every call offloaded"
    );
}

#[test]
fn an_offloaded_call_that_never_runs_is_an_index_error() {
    // Catches: a cancelled blocking task read as an empty answer (a run that exists
    // would read as absent) or as a panic.
    let mut life = Life::start_with(
        &Objects::new(),
        &Index::fake(),
        1,
        Limits::default(),
        IndexRoot::default(),
        Arc::new(Never),
    );
    let inv = InvRef::named("b1");
    let r = life.run(&inv, T, "main", RunOutcome::Ok);
    life.close(&inv);
    life.step(T); // the stream block refill fails, and is the step's transaction
    life.step(T);
    assert_eq!(
        life.writer.kept_objects(),
        1,
        "the rows failed; the object is kept"
    );
    let err = futures::executor::block_on(life.reader().run(r.run_id(), &all)).unwrap_err();
    assert!(matches!(err, ReadError::Index(_)), "{err}");
    assert!(err.to_string().contains("never ran"), "{err}");
}

#[test]
fn an_index_root_keeps_every_row_under_its_prefix() {
    // Catches: rows written under a hard-coded `ui/` whatever root the deployment
    // gave, so two farms or tenants sharing a state store would share rows.
    let objects = Objects::new();
    let index = Index::fake();
    let tenant = IndexRoot::under("tenant-a/").unwrap();
    let mut life = Life::start_with(
        &objects,
        &index,
        1,
        Limits::default(),
        tenant,
        Arc::new(kbf_history::Inline),
    );
    life.step(T);
    let inv = InvRef::named("b1");
    let r = life.run(&inv, T, "main", RunOutcome::Ok);
    life.close(&inv);
    life.step(T);
    assert!(index.keys("ui/").is_empty());
    let rows = index.keys("tenant-a/ui/");
    for part in ["invid/", "ichunk/", "stream/"] {
        let want = format!("tenant-a/ui/{part}");
        assert!(
            rows.iter().any(|k| k.starts_with(&want)),
            "{want} in {rows:?}"
        );
    }
    // The stream block came from the tenant's counter, not the default one.
    let next = |counter: &str| {
        let commit = index.inner.txn(kbf_state::Txn::new().next(counter));
        commit.unwrap().unwrap().counters[0]
    };
    assert_eq!((next("ui/streams"), next("tenant-a/ui/streams")), (1, 2));
    let found = futures::executor::block_on(life.reader().run(r.run_id(), &all)).unwrap();
    assert_eq!(found.map(|v| v.run), Some(r.clone()));
    let as_store: Arc<dyn kbf_state::Store> = index.clone();
    let elsewhere = Reader::new(
        objects.clone(),
        root(),
        kbf_history::HistoryIndex::new(
            as_store,
            IndexRoot::default(),
            Arc::new(kbf_history::Inline),
        ),
    );
    let none = futures::executor::block_on(elsewhere.run(r.run_id(), &all)).unwrap();
    assert_eq!(none, None, "another root sees nothing");
}
