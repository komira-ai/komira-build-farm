//! The writer and reader on the SQLite state store, across a restart of the store.

use std::path::Path;
use std::sync::Arc;

use kbf_history::records::RunOutcome;
use kbf_history::{InvRef, Limits};
use kbf_state::sqlite::{Options, SqliteStore};
use kbf_state::{Clock, ManualClock, Store};

use crate::support::{Index, Life, Objects, T, all, whole};

fn open(path: &Path) -> Arc<Index> {
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::default());
    let store: Arc<dyn Store> =
        Arc::new(SqliteStore::open(path, clock, Options::default()).expect("open the state file"));
    Index::over(store)
}

#[test]
fn history_written_through_sqlite_reads_back_after_the_store_restarts() {
    // Catches: index rows that only the in-memory fake keeps (a key or value the
    // SQLite backend stores differently), and history that does not survive a
    // frontend restart, which reopens the state file.
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("kbf-history-sqlite-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.db");
    let _ = std::fs::remove_file(&path);
    let objects = Objects::new();
    let inv = InvRef::named("b1");
    let (ok, failed) = {
        let mut life = Life::start(&objects, &open(&path), 1, Limits::default());
        life.step(T);
        let ok = life.run(&inv, T, "main", RunOutcome::Ok);
        let failed = life.run(&inv, T + 1, "main", RunOutcome::Failed);
        life.close(&inv);
        life.step(T + 1);
        assert!(
            !ok.run_id().stream().is_fallback(),
            "a block came from the counter"
        );
        (ok, failed)
    };
    let after = Life::start(&objects, &open(&path), 2, Limits::default());
    let reader = after.reader();
    let got = whole(&reader, &inv.key, &all).unwrap().unwrap();
    assert!(got.complete);
    assert_eq!(got.runs, [ok.clone(), failed.clone()]);
    let run = futures::executor::block_on(reader.run(failed.run_id(), &all))
        .unwrap()
        .unwrap();
    assert!(run.failed.is_some());
}
