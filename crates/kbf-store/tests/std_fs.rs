//! The store on the real filesystem: what the fault-injecting tests prove over
//! `FaultFs` holds over `StdFs` too.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use kbf_raft::{Entry, HardState, LogId, LogIndex, Payload, ServerId, Term};
use kbf_store::{Options, StdFs, Store, StoreError};

fn entry(index: u64, term: u64) -> Entry {
    Entry {
        id: LogId::new(Term(term), LogIndex(index)),
        payload: Payload::Command(index.to_le_bytes().to_vec()),
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("kbf-store-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Catches: a `StdFs` that does not create its directory, list, read, create, append,
/// rename or remove as the store needs (a reopen would not read back the log, the
/// truncation segment or the hard state), and a torn tail on a real file not cut off.
#[test]
fn persists_reopens_and_cuts_a_torn_tail() {
    let dir = scratch("roundtrip");
    let opts = Options { segment_bytes: 64 };
    let hard = HardState {
        term: Term(4),
        voted_for: Some(ServerId(3)),
    };
    {
        let fs = StdFs::new(&dir).unwrap();
        assert_eq!(fs.dir(), dir);
        let (mut store, rec) = Store::open(fs, opts).unwrap();
        assert!(rec.entries.is_empty());
        store.persist_hard_state(HardState::default()).unwrap();
        store.persist_hard_state(hard).unwrap();
        let first: Vec<Entry> = (1..=5).map(|i| entry(i, 1)).collect();
        store.persist_entries(&first).unwrap();
        store.persist_entries(&[entry(6, 1)]).unwrap();
        store.persist_entries(&[entry(4, 2)]).unwrap();
    }
    let want: Vec<Entry> = (1..=3).map(|i| entry(i, 1)).chain([entry(4, 2)]).collect();
    let segs = |d: &PathBuf| {
        let mut v: Vec<String> = std::fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".log"))
            .collect();
        v.sort();
        v
    };
    let names = segs(&dir);
    assert_eq!(names.len(), 2, "{names:?}");

    let last = dir.join(names.last().unwrap());
    let len = std::fs::metadata(&last).unwrap().len();
    OpenOptions::new()
        .append(true)
        .open(&last)
        .unwrap()
        .write_all(&[0x2A, 0, 0, 0, 1, 2])
        .unwrap();
    let (mut store, rec) = Store::open(StdFs::new(&dir).unwrap(), opts).unwrap();
    assert_eq!(rec.hard, hard);
    assert_eq!(rec.entries, want);
    assert_eq!(
        std::fs::metadata(&last).unwrap().len(),
        len,
        "torn tail cut"
    );
    store.persist_entries(&[entry(5, 2)]).unwrap();
    drop(store);

    let (_, rec) = Store::open(StdFs::new(&dir).unwrap(), opts).unwrap();
    assert_eq!(rec.entries.len(), 5);

    std::fs::write(dir.join(names.first().unwrap()), b"\x01\0\0\0\0\0\0\0x").unwrap();
    assert!(matches!(
        Store::open(StdFs::new(&dir).unwrap(), opts),
        Err(StoreError::Corrupt { .. })
    ));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Catches: `StdFs` errors that are swallowed: a create over an existing file, an open
/// of a missing one, and a directory that cannot be created.
#[test]
fn std_fs_reports_errors() {
    use kbf_store::{Fs, FsFile};
    let dir = scratch("errors");
    let fs = StdFs::new(&dir).unwrap();
    let mut f = fs.create("a").unwrap();
    f.append(b"abc").unwrap();
    f.truncate(1).unwrap();
    f.sync().unwrap();
    assert_eq!(fs.read("a").unwrap(), b"a");
    assert!(fs.create("a").is_err());
    assert!(fs.open_append("b").is_err());
    assert!(fs.rename("b", "c").is_err());
    assert!(fs.remove("b").is_err());
    assert!(StdFs::new(dir.join("a").join("sub")).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}
