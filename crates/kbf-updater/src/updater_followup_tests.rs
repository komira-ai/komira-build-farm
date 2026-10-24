//! The verifier's follow-up on the first roll tests: a failed save poisons the updater, the
//! directory lock, the real fsyncs, the plan check of a record, the install cap, abort at the
//! last host and the `Unreachable` kind of a port error.

use std::fs::{self, File};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::api::{ErrorCode, Hosts, Phase, RollState};
use crate::fakes::Event;
use crate::ports::{Installed, PortError};
use crate::record::{Durable, Fsync, RollStore, StoreError};
use crate::updater_restart_tests::{OLD, TARGET, record, restart};
use crate::updater_tests::{Rig, all, request};
use crate::{Ports, Timing, Updater};

use Phase::{Done, Installing, Pending};

/// Fails the directory sync of the `at`th save (1-based) and no other: the record is renamed
/// into place and then the save reports an error.
#[derive(Debug)]
struct DirFailsAt {
    at: usize,
    seen: Arc<AtomicUsize>,
}

impl Durable for DirFailsAt {
    fn file(&self, _file: &File) -> std::io::Result<()> {
        Ok(())
    }

    fn dir(&self, _dir: &Path) -> std::io::Result<()> {
        if self.seen.fetch_add(1, Ordering::SeqCst) + 1 == self.at {
            return Err(std::io::Error::other("dir fsync: EIO"));
        }
        Ok(())
    }
}

fn dir_fails_at(rig: &Rig, at: usize) -> Updater {
    let sync = DirFailsAt {
        at,
        seen: Arc::default(),
    };
    rig.open_with(RollStore::with_sync(&rig.dir, Box::new(sync)))
}

/// Catches memory falling behind the disk: the write that records `failed` is renamed into
/// place and then its directory fsync fails, so the disk says Failed and memory says
/// installing. A second `drive` used to reinstall the host the disk calls failed. Now every
/// operation refuses (`state_lost`), nothing more is installed, a reload from disk knows the
/// roll is failed, and the roll stays over.
#[test]
fn a_failed_save_poisons_the_updater_until_it_is_reloaded() {
    let rig = Rig::new("poison");
    rig.inst
        .script_install("fe", vec![], Err(PortError("dpkg: lock held".into())));
    // Writes: start, fe installing, fe failed.
    let u = dir_fails_at(&rig, 3);
    let id = u.start(&all(TARGET, "k")).expect("start").roll.id;
    assert_eq!(u.drive(&id).expect_err("lost").code, ErrorCode::StateLost);
    assert_eq!(rig.log.installs(), ["fe"]);
    let on_disk = rig.store().load().expect("load").expect("record");
    assert_eq!(on_disk.roll.state, RollState::Failed, "the rename happened");
    assert_eq!(u.memory().expect("mem").state, RollState::Running, "behind");

    let before = rig.log.events();
    let lost = |e: crate::api::ApiError| e.code;
    assert_eq!(
        lost(u.drive(&id).expect_err("poisoned")),
        ErrorCode::StateLost
    );
    assert_eq!(
        lost(u.get(&id).expect_err("poisoned")),
        ErrorCode::StateLost
    );
    assert_eq!(
        lost(u.current().expect_err("poisoned")),
        ErrorCode::StateLost
    );
    assert_eq!(
        lost(u.abort(&id).expect_err("poisoned")),
        ErrorCode::StateLost
    );
    assert_eq!(
        lost(u.start(&all(TARGET, "k2")).expect_err("poisoned")),
        ErrorCode::StateLost
    );
    assert_eq!(rig.log.events(), before, "no port was called");
    assert_eq!(rig.store().load().expect("load").expect("record"), on_disk);

    let reloaded = u.reload().expect("reload").expect("a roll");
    assert_eq!(reloaded.state, RollState::Failed);
    assert_eq!(u.drive(&id).expect("as is").state, RollState::Failed);
    assert_eq!(rig.log.events(), before, "a failed roll is not run again");
    assert_eq!(u.current().expect("live"), Some(reloaded));
}

/// Catches a reload that clears the poison on a file it cannot trust, or that swaps the
/// record under a thread that is driving: a corrupt `roll.json` leaves the updater poisoned,
/// and a reload from inside an install is refused with 409.
#[test]
fn a_reload_needs_a_readable_record_and_no_driver() {
    let rig = Rig::new("reload-refused");
    let u = Arc::new(dir_fails_at(&rig, 1));
    assert_eq!(
        u.start(&all(TARGET, "k")).expect_err("lost").code,
        ErrorCode::StateLost
    );
    fs::write(rig.dir.join("roll.json"), "{ not json").expect("corrupt");
    assert_eq!(u.reload().expect_err("corrupt").code, ErrorCode::StateLost);
    assert_eq!(u.current().expect_err("still").code, ErrorCode::StateLost);
    fs::remove_file(rig.dir.join("roll.json")).expect("rm");
    assert_eq!(u.reload().expect("empty"), None);

    let u2 = Arc::clone(&u);
    let refused = Arc::new(std::sync::Mutex::new(None));
    let sink = Arc::clone(&refused);
    rig.inst.on_install(move |_| {
        *sink.lock().expect("sink") = Some(u2.reload().expect_err("driving").code);
    });
    let id = u.start(&all(TARGET, "k")).expect("start").roll.id;
    assert_eq!(u.drive(&id).expect("drive").state, RollState::Done);
    assert_eq!(
        refused.lock().expect("sink").take(),
        Some(ErrorCode::RollActive)
    );
}

/// Catches fsyncs that are not made: `Fsync` on a device, which cannot be synced, reports the
/// kernel's EINVAL (so `sync_all` was called on the file and on the directory), and on a real
/// file and directory it succeeds. A body that returned `Ok(())` fails the first half.
#[test]
fn the_real_fsync_syncs_the_file_and_the_directory() {
    let dev = File::open("/dev/null").expect("dev null");
    assert!(Fsync.file(&dev).is_err(), "file: no sync_all call");
    assert!(
        Fsync.dir(Path::new("/dev/null")).is_err(),
        "dir: no sync_all call"
    );

    let rig = Rig::new("fsync-real");
    let file = File::create(rig.dir.join("probe")).expect("probe");
    Fsync.file(&file).expect("sync a file");
    Fsync.dir(&rig.dir).expect("sync a directory");
    assert!(Fsync.dir(&rig.dir.join("missing")).is_err());
}

/// Catches `RollStore::open` wired to anything but the real fsyncs (a no-op `Durable` would
/// pass every other test): the store it makes holds `Fsync` and the directory lock.
#[test]
fn an_opened_store_uses_the_real_fsync_and_holds_the_lock() {
    let rig = Rig::new("open-wiring");
    let store = RollStore::open(&rig.dir).expect("open");
    let shown = format!("{store:?}");
    assert!(shown.contains("sync: Fsync"), "{shown}");
    assert!(shown.contains("_lock: Some"), "{shown}");
}

/// Catches two updaters driving one `roll.json`: while a store holds the directory, a second
/// open is refused (`Busy`) and so is an updater over it; when the first is dropped the
/// directory is free again. The lock file is private (0600).
#[test]
fn a_second_store_over_one_directory_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new("dir-lock");
    let first = RollStore::open(&rig.dir).expect("first");
    assert!(matches!(RollStore::open(&rig.dir), Err(StoreError::Busy)));
    let mode = fs::metadata(rig.dir.join("lock"))
        .expect("lock")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    drop(first);
    let again = RollStore::open(&rig.dir).expect("free again");
    let u = Updater::open(
        crate::updater_tests::inventory(),
        again,
        rig.ports(),
        Timing::default(),
    )
    .expect("updater");
    assert!(matches!(RollStore::open(&rig.dir), Err(StoreError::Busy)));
    drop(u);
    assert!(RollStore::open(&rig.dir).is_ok());
    assert!(matches!(
        RollStore::open(&rig.dir.join("nowhere")),
        Err(StoreError::Io(_))
    ));
}

/// Fail fast, not hang, when a defect reinstalls without end: panic on the `max`+1st install.
fn at_most_installs(rig: &Rig, max: usize) {
    let seen = Arc::new(AtomicUsize::new(0));
    rig.inst.on_install(move |h| {
        let n = seen.fetch_add(1, Ordering::SeqCst) + 1;
        assert!(n <= max, "install {n} of {} is over the cap", h.alias);
    });
}

fn open_over(rig: &Rig, rec: &crate::record::Record) -> Result<Updater, StoreError> {
    rig.store().save(rec).expect("save");
    Updater::open(
        crate::updater_tests::inventory(),
        rig.store(),
        Ports {
            safe: Box::new(rig.stop.clone()),
            installer: Box::new(rig.inst.clone()),
            drain: Box::new(rig.drain.clone()),
            health: Box::new(rig.health.clone()),
            clock: Box::new(rig.clock.clone()),
        },
        Timing::default(),
    )
}

/// A record of an explicit list: `ask` is what was asked, `have` the roll's hosts.
fn listed(ask: &[&str], have: &[&str]) -> crate::record::Record {
    let mut rec = record(
        [(Pending, 0), (Pending, 0), (Pending, 0), (Pending, 0)],
        RollState::Running,
    );
    let aliases = ask.iter().map(|s| (*s).to_owned()).collect();
    rec.request = request(TARGET, Hosts::Aliases(aliases), "run-1");
    rec.roll.hosts.truncate(have.len());
    for (h, alias) in rec.roll.hosts.iter_mut().zip(have) {
        h.alias = (*alias).to_owned();
    }
    rec
}

/// Asserts that a record whose hosts are not the request's plan is refused, so that no host
/// nobody asked for is rolled, and no right host in the wrong order: a swapped host, a missing
/// one and a reordering that is no move of hosts to the front are refused by the record's own
/// check; a reordered pair, hosts moved to the front that the inventory does not make
/// frontends, and a frontend not first, are refused when the inventory (which knows the
/// frontends) makes the plan. The right plans load, including one with the frontend moved to
/// the front.
#[test]
fn a_record_whose_hosts_are_not_the_requests_plan_is_refused() {
    let rig = Rig::new("plan-check");
    let inconsistent = |rec: &crate::record::Record| {
        let err = open_over(&rig, rec).err().expect("refused");
        assert!(matches!(err, StoreError::Inconsistent(_)), "{err}");
        err.to_string()
    };
    assert!(inconsistent(&listed(&["st1", "dm"], &["st1", "st2"])).contains("not the request's"));
    assert!(inconsistent(&listed(&["st1", "dm"], &["st1"])).contains("not the request's"));
    assert!(inconsistent(&listed(&["st1", "dm"], &["dm", "st1"])).contains("not the plan"));
    assert!(inconsistent(&listed(&["st1", "fe"], &["st1", "fe"])).contains("not the plan"));
    let moved = inconsistent(&listed(&["st1", "dm", "st2"], &["dm", "st2", "st1"]));
    assert!(moved.contains("not the plan"), "{moved}");
    assert!(
        inconsistent(&listed(&["st1", "dm", "st2"], &["st2", "dm", "st1"])).contains("request's")
    );

    for (ask, have) in [
        (&["st1", "dm"][..], &["st1", "dm"][..]),
        (&["st1", "fe"], &["fe", "st1"]),
        (&["st1", "dm", "fe"], &["fe", "st1", "dm"]),
        (&["st1"], &["st1"]),
    ] {
        open_over(&rig, &listed(ask, have)).unwrap_or_else(|e| panic!("{ask:?}: {e}"));
    }
    // A host that left the inventory is the roll's business at that host.
    open_over(&rig, &listed(&["st1", "gone"], &["st1", "gone"])).expect("open");
}

/// Catches a restart loop that reinstalls for ever: a host found `installing` that still
/// reports an old version after two install attempts is not installed a third time; the roll
/// is interrupted, naming the host and the version, and nothing is installed.
#[test]
fn a_third_install_is_not_made_the_roll_is_interrupted() {
    let rig = Rig::new("cap-installs");
    rig.inst
        .set_installed("st1", Installed::Version(OLD.into()));
    at_most_installs(&rig, 0);
    let u = restart(
        &rig,
        &record(
            [(Done, 1), (Installing, 2), (Pending, 0), (Pending, 0)],
            RollState::Running,
        ),
    );
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Interrupted);
    let detail = roll.hosts[1].detail.clone().unwrap_or_default();
    assert!(
        detail.contains("1.0") && detail.contains("2 install attempts"),
        "{detail}"
    );
    assert!(rig.log.installs().is_empty());
    assert_eq!(roll.hosts[1].attempts, 2);
}

/// Catches an abort asked during the last host being lost: the host finishes, then the roll
/// ends `aborted` (not `done`), the flag is consumed on disk, and the reply to the abort
/// said the roll was still running.
#[test]
fn an_abort_during_the_last_host_ends_the_roll_aborted_and_is_consumed() {
    let rig = Rig::new("abort-last-host");
    let u = Arc::new(rig.open());
    let id = u.start(&all(TARGET, "k")).expect("start").roll.id;
    let (u2, id2) = (Arc::clone(&u), id.clone());
    let reply = Arc::new(std::sync::Mutex::new(None));
    let sink = Arc::clone(&reply);
    rig.inst.on_install(move |h| {
        if h.alias == "st2" {
            *sink.lock().expect("sink") = Some(u2.abort(&id2).expect("abort").state);
        }
    });
    let roll = u.drive(&id).expect("drive");
    assert_eq!(roll.state, RollState::Aborted);
    assert!(
        roll.hosts.iter().all(|h| h.phase == Done),
        "the host finished"
    );
    assert_eq!(reply.lock().expect("sink").take(), Some(RollState::Running));
    let on_disk = rig.store().load().expect("load").expect("record");
    assert!(!on_disk.abort_requested, "consumed");
    assert_eq!(on_disk.roll, roll);
}

/// Catches an abort that says `aborted` for a roll that is over: when every host is already
/// done and only the driver's last write is left, the reply says `done`, the flag is not set
/// and the roll ends `done`.
#[test]
fn an_abort_after_the_last_host_finished_says_done() {
    let rig = Rig::new("abort-after-last");
    let u = rig.open();
    let id = u.start(&all(TARGET, "k")).expect("start").roll.id;
    {
        let mut st = u.lock();
        st.driving = true;
        for h in &mut st.rec.as_mut().expect("rec").roll.hosts {
            h.phase = Phase::Done;
        }
    }
    assert_eq!(u.abort(&id).expect("reply").state, RollState::Done);
    assert!(!u.lock().rec.as_ref().expect("rec").abort_requested);
    u.lock().driving = false;
    assert_eq!(u.drive(&id).expect("drive").state, RollState::Done);
}

/// Catches an unreachable host being failed: an install that gets no answer is not
/// `install_failed`. The roll asks the host what it has: the old version means the install
/// never ran and is made once more; the target means it ran and is not repeated.
#[test]
fn an_unreachable_install_is_asked_about_not_failed() {
    let rig = Rig::new("unreachable-reinstall");
    rig.inst.script_install(
        "fe",
        vec![Err(PortError::unreachable("ssh: connection reset"))],
        Ok(()),
    );
    let u = rig.open();
    let roll = rig.roll(&u);
    assert_eq!(roll.state, RollState::Done);
    assert_eq!(roll.hosts[0].attempts, 2);
    assert_eq!(
        &rig.log.touching("fe")[..3],
        &[
            Event::Install("fe".into(), TARGET.into()),
            Event::Query("fe".into()),
            Event::Install("fe".into(), TARGET.into())
        ]
    );

    // The install ran and only the answer was lost: the host reports the target.
    let rig = Rig::new("unreachable-ran");
    rig.inst.script_install(
        "fe",
        vec![Err(PortError::unreachable("ssh: connection reset"))],
        Ok(()),
    );
    rig.inst
        .script_query("fe", vec![Ok(Installed::Version(TARGET.into()))]);
    let u = rig.open();
    assert_eq!(rig.roll(&u).state, RollState::Done);
    assert_eq!(rig.log.installs(), ["fe", "st1", "dm", "st2"]);
}

/// Catches silence ending in `failed`: an install whose host never answers, nor to the
/// queries after it, ends `interrupted` after the 120 s of the query bound, with nothing
/// after that host touched. An "other" error on the same install is still `install_failed`.
#[test]
fn a_host_that_never_answers_interrupts_the_roll_it_does_not_fail_it() {
    let rig = Rig::new("unreachable-forever");
    rig.inst
        .script_install("fe", vec![], Err(PortError::unreachable("no route")));
    rig.inst.script_query(
        "fe",
        (0..100)
            .map(|_| Err(PortError::unreachable("no route")))
            .collect(),
    );
    let u = rig.open();
    let roll = rig.roll(&u);
    assert_eq!(roll.state, RollState::Interrupted);
    assert!(roll.failure.is_none());
    assert_eq!(rig.elapsed(), Duration::from_secs(120));
    assert_eq!(rig.log.installs(), ["fe"]);
    assert!(rig.log.touching("st1").is_empty());
    assert!(
        roll.hosts[0]
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("did not answer"))
    );

    let rig = Rig::new("other-still-fails");
    rig.inst
        .script_install("fe", vec![], Err(PortError::other("refused: older")));
    let u = rig.open();
    assert_eq!(rig.roll(&u).state, RollState::Failed);
}

/// Catches the cap being missed when the rerun itself gets no answer: the install after a
/// restart is unreachable, the query then still says old, and a third install is not made.
#[test]
fn an_unreachable_rerun_does_not_buy_a_third_install() {
    let rig = Rig::new("unreachable-rerun");
    rig.inst
        .set_installed("st1", Installed::Version(OLD.into()));
    rig.inst
        .script_install("st1", vec![], Err(PortError::unreachable("no route")));
    at_most_installs(&rig, 1);
    let u = restart(
        &rig,
        &record(
            [(Done, 1), (Installing, 1), (Pending, 0), (Pending, 0)],
            RollState::Running,
        ),
    );
    let roll = u.drive("roll-1").expect("drive");
    assert_eq!(roll.state, RollState::Interrupted);
    assert_eq!(rig.log.installs(), ["st1"]);
    assert_eq!(roll.hosts[1].attempts, 2);
}
