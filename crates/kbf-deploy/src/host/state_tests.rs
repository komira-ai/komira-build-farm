use std::fs::TryLockError;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::sync::{Arc, Barrier};
use std::thread;

use super::*;

pub(crate) const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub(crate) const B: &str = "0123456789abcdef0123456789abcdef01234567";
pub(crate) const C: &str = "cccccccccccccccccccccccccccccccccccccccc";

pub(crate) fn c(id: &str) -> CommitId {
    CommitId::new(id).expect("a test commit")
}

/// A scratch root with the directories a host has: `/run`, the state directory and
/// `/usr/local/kbf`.
pub(crate) fn host_root(name: &str) -> (Root, PathBuf) {
    let dir = kbf_host::testkit::scratch(name);
    for sub in ["run", "var/lib/kbf-deploy", "usr/local/kbf"] {
        fs::create_dir_all(dir.join(sub)).expect("seed");
    }
    (Root::new(&dir).expect("absolute"), dir)
}

pub(crate) fn full_state() -> State {
    State {
        current: Some(c(A)),
        previous: Some(c(B)),
        staged: Some(c(C)),
        pending_confirm: Some(PendingConfirm {
            commit: c(A),
            previous: Some(c(B)),
            deadline: 1_800_000_000,
        }),
        order_floor: 7,
    }
}

/// Catches a state that does not read back as written (a field dropped or renamed), a
/// file without its schema number first, and a missing file read as anything but "no
/// state yet". The bytes are pinned: the next build reads what this one wrote.
#[test]
fn the_state_round_trips_with_its_schema() {
    let (root, dir) = host_root("deploy-host-state-round-trip");
    let lock = Locked::acquire(&root).expect("lock");
    assert_eq!(lock.read().expect("read"), None);
    lock.write(&full_state()).expect("write");
    assert_eq!(lock.read().expect("read"), Some(full_state()));
    let text = fs::read_to_string(dir.join("var/lib/kbf-deploy/state.json")).expect("file");
    assert_eq!(
        text,
        format!(
            "{{\n  \"schema\": 1,\n  \"current\": \"{A}\",\n  \"previous\": \"{B}\",\n  \
             \"staged\": \"{C}\",\n  \"pending_confirm\": {{\n    \"commit\": \"{A}\",\n    \
             \"previous\": \"{B}\",\n    \"deadline\": 1800000000\n  }},\n  \
             \"order_floor\": 7\n}}\n"
        )
    );
    lock.write(&State::default()).expect("write");
    assert_eq!(lock.read().expect("read"), Some(State::default()));
}

/// Catches a state read and written without the lock held: while one run holds it, no
/// other may take it, and once it is dropped another may.
///
/// Mutant seen red: `acquire` without `lock()` (the second lock is taken at once).
#[test]
fn the_lock_is_held_while_a_locked_lives() {
    let (root, dir) = host_root("deploy-host-state-lock");
    let held = Locked::acquire(&root).expect("lock");
    let other = File::open(dir.join("run/kbf-deploy.lock")).expect("the lock file exists");
    assert!(
        matches!(other.try_lock(), Err(TryLockError::WouldBlock)),
        "a second run took the lock while the first held it"
    );
    held.write(&full_state()).expect("write");
    drop(held);
    other.try_lock().expect("free once dropped");
}

/// Catches a lock file or state file another user can read or write: the state says
/// which release a root service runs, and anyone who could hold the lock could stall
/// every deploy. Both are made 0600, and a rewrite keeps the state at 0600.
///
/// Mutant seen red: the state file made 0644 (as before this test).
#[test]
fn the_lock_and_the_state_are_root_s_alone() {
    let (root, dir) = host_root("deploy-host-state-modes");
    let lock = Locked::acquire(&root).expect("lock");
    lock.write(&full_state()).expect("write");
    lock.write(&State::default()).expect("rewrite");
    for file in ["run/kbf-deploy.lock", "var/lib/kbf-deploy/state.json"] {
        let mode = fs::metadata(dir.join(file)).expect(file).mode() & 0o7777;
        assert_eq!(mode, 0o600, "{file}");
    }
}

/// Catches writers that do not exclude each other: each of several threads adds one to
/// the order floor many times, through read, change and write under the lock; an update
/// lost to a concurrent writer leaves the total short.
///
/// Mutant seen red: `acquire` without `lock()`.
#[test]
fn concurrent_writers_lose_no_update() {
    let (root, _dir) = host_root("deploy-host-state-writers");
    const THREADS: u64 = 4;
    const EACH: u64 = 25;
    let start = Arc::new(Barrier::new(THREADS as usize));
    let writers: Vec<_> = (0..THREADS)
        .map(|_| {
            let (root, start) = (root.clone(), Arc::clone(&start));
            thread::spawn(move || {
                start.wait();
                for _ in 0..EACH {
                    let lock = Locked::acquire(&root).expect("lock");
                    let mut state = lock.read().expect("read").unwrap_or_default();
                    state.order_floor += 1;
                    lock.write(&state).expect("write");
                }
            })
        })
        .collect();
    for w in writers {
        w.join().expect("writer");
    }
    let lock = Locked::acquire(&root).expect("lock");
    let floor = lock.read().expect("read").expect("written").order_floor;
    assert_eq!(floor, THREADS * EACH);
}

/// Catches a write into the state file itself (a crash or power cut part-way would
/// leave half a state, which no later run could read): the old file, held open, must
/// keep the old state whole, the path must name a new file, and no copy may be left.
///
/// Mutant seen red: the state written in place (`fs::write` of the path).
#[test]
fn a_write_replaces_the_file_and_never_changes_the_old_one() {
    let (root, dir) = host_root("deploy-host-state-replace");
    let path = dir.join("var/lib/kbf-deploy/state.json");
    let lock = Locked::acquire(&root).expect("lock");
    lock.write(&State::default()).expect("write");
    let before = fs::read(&path).expect("file");
    let mut old = File::open(&path).expect("open the old file");
    let old_inode = old.metadata().expect("stat").ino();
    lock.write(&full_state()).expect("write");
    let mut kept = Vec::new();
    old.read_to_end(&mut kept).expect("read the old file");
    assert_eq!(kept, before, "the old file was changed in place");
    assert_ne!(fs::metadata(&path).expect("stat").ino(), old_inode);
    let names: Vec<_> = fs::read_dir(dir.join("var/lib/kbf-deploy"))
        .expect("list")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(names, ["state.json"], "a temporary copy was left");
}

/// Catches a reader that takes a state it does not understand: another schema (a newer
/// kbf-deploy wrote it), no schema, a field it does not know, a commit id that is not 40
/// lowercase hex characters, or something that is not a JSON object.
#[test]
fn a_state_this_build_does_not_know_is_refused() {
    let path = Path::new("/var/lib/kbf-deploy/state.json");
    let full = format!(
        "\"current\":\"{A}\",\"previous\":null,\"staged\":null,\
         \"pending_confirm\":null,\"order_floor\":0"
    );
    let ok = format!("{{\"schema\":1,{full}}}");
    assert_eq!(
        parse(path, ok.as_bytes()).expect("parses").current,
        Some(c(A))
    );
    let schemas = [
        (format!("{{\"schema\":2,{full}}}"), "2"),
        (format!("{{{full}}}"), "null"),
        (format!("{{\"schema\":\"1\",{full}}}"), "\"1\""),
    ];
    for (text, found) in schemas {
        let err = parse(path, text.as_bytes()).expect_err(&text);
        assert_eq!(
            err.to_string(),
            format!(
                "/var/lib/kbf-deploy/state.json has schema {found}; this kbf-deploy reads \
                 only schema 1"
            )
        );
    }
    let upper = A.to_uppercase();
    let refused = [
        (format!("{{\"schema\":1,{full},\"extra\":1}}"), "unknown field"),
        (
            format!("{{\"schema\":1,{}}}", full.replace(A, &upper)),
            "is not a commit id",
        ),
        (
            format!(
                "{{\"schema\":1,{}}}",
                full.replace(
                    "\"pending_confirm\":null",
                    &format!("\"pending_confirm\":{{\"commit\":\"{A}\",\"previous\":null,\"deadline\":1,\"x\":0}}")
                )
            ),
            "unknown field",
        ),
        ("[1]".to_owned(), "is not a deploy state"),
        ("{".to_owned(), "is not a deploy state"),
    ];
    for (text, why) in refused {
        let err = parse(path, text.as_bytes()).expect_err(&text);
        assert!(matches!(err, StateError::Parse { .. }), "{text}: {err:?}");
        assert!(err.to_string().contains(why), "{text}: {err}");
    }
}

/// Catches errors read as "no state" or passed over: a lock whose directory is missing,
/// a state directory that is missing (or holds a stale copy the write cannot clear), and
/// a state path that is a directory.
#[test]
fn errors_name_the_path_and_change_nothing() {
    let dir = kbf_host::testkit::scratch("deploy-host-state-errors");
    let root = Root::new(&dir).expect("absolute");
    let err = Locked::acquire(&root).expect_err("no /run");
    assert!(matches!(err, StateError::Lock { .. }), "{err:?}");
    assert!(err.to_string().contains("run/kbf-deploy.lock"), "{err}");

    fs::create_dir(dir.join("run")).expect("seed");
    let lock = Locked::acquire(&root).expect("lock");
    let err = lock.write(&full_state()).expect_err("no state directory");
    assert!(matches!(err, StateError::Write { .. }), "{err:?}");
    assert!(err.to_string().contains("state.json"), "{err}");

    fs::create_dir_all(dir.join("var/lib/kbf-deploy/.state.json.tmp/x")).expect("seed");
    let err = lock
        .write(&full_state())
        .expect_err("a stale copy that is a directory");
    assert!(matches!(err, StateError::Write { .. }), "{err:?}");
    assert!(!dir.join("var/lib/kbf-deploy/state.json").exists());

    fs::create_dir(dir.join("var/lib/kbf-deploy/state.json")).expect("seed");
    let err = lock.read().expect_err("a directory");
    assert!(matches!(err, StateError::Read { .. }), "{err:?}");
    assert!(matches!(status(&root), Err(StateError::Read { .. })));
}

/// Catches a status that reads a link it cannot read as missing, or a file or directory
/// at `current` as a link, and a match that takes anything but exactly
/// `releases/<commit>` (an absolute target, another commit, a link with no state).
#[test]
fn status_reads_the_current_link_and_matches_it_to_the_state() {
    let (root, dir) = host_root("deploy-host-state-status");
    let link = dir.join("usr/local/kbf/current");
    let none = status(&root).expect("status");
    assert_eq!(
        none,
        Status {
            exists: false,
            state: State::default(),
            link: CurrentLink::Missing
        }
    );
    assert!(none.link_matches());

    Locked::acquire(&root)
        .expect("lock")
        .write(&full_state())
        .expect("write");
    let cases = [
        (None, false),
        (Some(format!("releases/{A}")), true),
        (Some(format!("releases/{B}")), false),
        (Some(format!("/usr/local/kbf/releases/{A}")), false),
    ];
    for (target, matches) in cases {
        let _ = fs::remove_file(&link);
        if let Some(t) = &target {
            std::os::unix::fs::symlink(t, &link).expect("link");
        }
        let got = status(&root).expect("status");
        assert!(got.exists);
        assert_eq!(got.state, full_state());
        assert_eq!(got.link_matches(), matches, "{target:?}");
    }
    let no_current = Status {
        exists: true,
        state: State::default(),
        link: CurrentLink::To(PathBuf::from(format!("releases/{A}"))),
    };
    assert!(!no_current.link_matches());

    fs::remove_file(&link).expect("remove");
    fs::create_dir(&link).expect("a directory");
    assert_eq!(status(&root).expect("status").link, CurrentLink::NotALink);

    fs::remove_dir(&link).expect("remove");
    fs::remove_dir(dir.join("usr/local/kbf")).expect("remove");
    fs::write(dir.join("usr/local/kbf"), "").expect("a file where the directory is");
    let err = status(&root).expect_err("ENOTDIR");
    assert!(err.to_string().contains("usr/local/kbf/current"), "{err}");
}
