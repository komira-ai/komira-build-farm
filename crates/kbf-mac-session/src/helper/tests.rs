use std::io::Read as _;
use std::os::fd::OwnedFd;

use super::*;
use crate::grant::testing::{key_line, payload, sign};
use crate::testing::{FakeHost, NOW, scratch, trace};

fn me() -> u32 {
    rustix::process::getuid().as_raw()
}

fn my_gid() -> u32 {
    rustix::process::getgid().as_raw()
}

struct Rig {
    host: FakeHost,
    helper: Helper,
    dir: PathBuf,
}

/// A helper whose lease range starts at the test's own uid (so a lease created first
/// gets it, and `run` can start a process as it unprivileged), over a fake host.
fn rig(name: &str, len: u32) -> Rig {
    rig_with(name, UidRange::new(me(), me() + len - 1).unwrap(), None)
}

fn rig_with(name: &str, range: UidRange, dir: Option<PathBuf>) -> Rig {
    trace();
    let dir = dir.unwrap_or_else(|| scratch(&format!("helper-{name}")));
    let homes = dir.join("Users");
    let _ = std::fs::create_dir(&homes);
    let host = FakeHost::default();
    let settings = Settings {
        range,
        gid: my_gid(),
        homes,
        sweep: SweepPlan {
            named: vec![format!("{}/tabs/{{user}}", dir.display())],
            owned: vec![dir.join("Shared")],
        },
        grant_keys: Some(GrantKeys::parse(&key_line(7)).unwrap()),
        serial: "SERIAL1".to_owned(),
    };
    let ledger = Ledger::open(&dir.join("ledger")).unwrap();
    let helper = Helper::new(Box::new(host.clone()), settings, ledger);
    Rig { host, helper, dir }
}

fn log(rig: &Rig) -> Vec<String> {
    std::mem::take(&mut rig.host.state().log)
}

/// Catches: a uid reused while its lease is live, a uid another user record or a
/// running process has, or allocation that restarts at the bottom of the range.
#[test]
fn each_lease_gets_a_free_uid_after_the_last_one() {
    let rig = rig("alloc", 4);
    let base = me();
    assert_eq!(rig.helper.user_create("1.1", None), Ok(base));
    assert_eq!(rig.helper.user_create("1.2", None), Ok(base + 1));
    let user = rig.host.state().users["kbf-lease-1-2"].clone();
    // Deleting 1.1 frees its uid, but allocation goes on after the uid handed out
    // last, so the freed one is reused as late as possible.
    rig.helper.user_delete("1.1").unwrap();
    assert_eq!(rig.helper.user_create("1.3", None), Ok(base + 2));
    // Skipped: a uid another user record has (base+3), one a process runs as (base),
    // and ones live leases hold (base+1, base+2) even when their records are gone.
    rig.host.state().foreign.insert(base + 3);
    rig.host.state().users.clear();
    rig.host.state().procs.insert(base, 1);
    let error = rig.helper.user_create("1.4", None).unwrap_err();
    assert!(error.contains("no free uid"), "{error}");
    rig.host.state().procs.clear();
    // Wrapped round to the bottom of the range.
    assert_eq!(rig.helper.user_create("1.5", None), Ok(base));
    assert_eq!(
        user,
        NewUser {
            name: "kbf-lease-1-2".to_owned(),
            uid: base + 1,
            gid: my_gid(),
            home: rig.dir.join("Users/kbf-lease-1-2"),
            admin: false,
        }
    );
    let made = log(&rig)
        .into_iter()
        .filter(|line| line.starts_with("make_home"))
        .count();
    assert_eq!(made, 4);
}

/// Catches: a lease id used twice (S4.2: a name never reused), including after its
/// user was deleted, and including across a restart of the helper.
#[test]
fn a_lease_id_is_never_used_twice() {
    let rig = rig("reuse", 4);
    rig.helper.user_create("2.1", None).unwrap();
    let again = rig.helper.user_create("2.1", None).unwrap_err();
    assert!(again.contains("never reused"), "{again}");
    rig.helper.user_delete("2.1").unwrap();
    assert!(
        rig.helper
            .user_create("2.1", None)
            .unwrap_err()
            .contains("never reused")
    );
    // A new helper over the same ledger (a reboot) remembers.
    let range = UidRange::new(me(), me() + 3).unwrap();
    let rebooted = rig_with("reuse", range, Some(rig.dir.clone()));
    let after = rebooted.helper.user_create("2.1", None).unwrap_err();
    assert!(after.contains("never reused"), "{after}");
    assert!(
        rig.helper
            .user_create("2.01", None)
            .unwrap_err()
            .contains("not <term>.<seq>")
    );
}

/// Catches: an administrator made on the daemon's word (S10: "trust an admin flag
/// from the daemon"), a grant used twice, or a grant for another lease or Mac.
#[test]
fn an_administrator_needs_a_valid_unused_grant() {
    let rig = rig("admin", 4);
    let grant = sign(7, &payload("SERIAL1", "3.1", NOW + 600));
    assert_eq!(rig.helper.user_create("3.1", Some(&grant)), Ok(me()));
    assert!(rig.host.state().users["kbf-lease-3-1"].admin);
    // Single use: the lease id is spent.
    assert!(
        rig.helper
            .user_create("3.1", Some(&grant))
            .unwrap_err()
            .contains("never reused")
    );
    // A grant names one lease and one Mac, and the gate's key.
    let error = rig.helper.user_create("3.2", Some(&grant)).unwrap_err();
    assert!(error.contains("names lease 3.1"), "{error}");
    let other_mac = sign(7, &payload("SERIAL2", "3.2", NOW + 600));
    assert!(
        rig.helper
            .user_create("3.2", Some(&other_mac))
            .unwrap_err()
            .contains("serial")
    );
    let forged = sign(8, &payload("SERIAL1", "3.2", NOW + 600));
    assert!(
        rig.helper
            .user_create("3.2", Some(&forged))
            .unwrap_err()
            .contains("does not verify")
    );
    // A refused grant made no user and spent nothing: the plain lease still works.
    assert_eq!(rig.helper.user_create("3.2", None), Ok(me() + 1));
    assert!(!rig.host.state().users["kbf-lease-3-2"].admin);

    let mut keyless = rig_with("admin-nokeys", UidRange::new(me(), me()).unwrap(), None);
    keyless.helper.settings.grant_keys = None;
    let grant = sign(7, &payload("SERIAL1", "3.3", NOW + 600));
    let error = keyless.helper.user_create("3.3", Some(&grant)).unwrap_err();
    assert!(error.contains("no gate key"), "{error}");
    assert!(keyless.host.state().users.is_empty());
}

/// Catches: a lease recorded as created when the host failed part-way silently
/// reported as success.
#[test]
fn host_failures_during_create_are_reported() {
    for fail in ["uid_taken", "live_processes", "make_home", "create_user"] {
        let rig = rig(&format!("create-{fail}"), 2);
        rig.host.state().fail.insert(fail);
        let error = rig.helper.user_create("4.1", None).unwrap_err();
        assert!(error.contains("failed"), "{fail}: {error}");
    }
}

/// Catches: a user created, or a deletion reported done, when the ledger could not
/// record it (after a reboot the lease id or the uid would be reused).
#[test]
fn a_ledger_that_cannot_record_stops_the_verb() {
    let rig = rig("ledger-fail", 2);
    rig.helper.user_create("4.2", None).unwrap();
    rig.helper.lock().fail_writes(&rig.dir.join("ledger"));
    let error = rig.helper.user_create("4.3", None).unwrap_err();
    assert!(error.starts_with("recording lease 4.3"), "{error}");
    assert!(!rig.host.state().users.contains_key("kbf-lease-4-3"));
    let error = rig.helper.user_delete("4.2").unwrap_err();
    assert!(
        error.starts_with("recording the deletion of lease 4.2"),
        "{error}"
    );
}

/// Catches: killing before the launchd domains are booted out (launchd would restart
/// the user's agents), giving up after one round, or never giving up.
#[test]
fn kill_uid_boots_out_then_kills_until_none_is_left() {
    let rig = rig("kill", 2);
    rig.helper.user_create("5.1", None).unwrap();
    let uid = me();
    log(&rig);
    rig.host.state().procs.insert(uid, 3);
    rig.host.state().stubborn.insert(uid, 2);
    rig.host.state().fail.insert("bootout gui");
    assert_eq!(rig.helper.kill_uid("5.1"), Ok(()));
    let calls = log(&rig);
    assert_eq!(
        calls,
        [
            format!("bootout gui/{uid}"),
            format!("bootout user/{uid}"),
            format!("kill_all {uid}"),
            format!("live_processes {uid}"),
            "pause".to_owned(),
            format!("kill_all {uid}"),
            format!("live_processes {uid}"),
            "pause".to_owned(),
            format!("kill_all {uid}"),
            format!("live_processes {uid}"),
        ]
    );

    rig.host.state().procs.insert(uid, 1);
    rig.host.state().stubborn.insert(uid, usize::MAX);
    let error = rig.helper.kill_uid("5.1").unwrap_err();
    assert!(
        error.contains(&format!("1 processes of uid {uid} remain after 10 rounds")),
        "{error}"
    );
    let rounds = log(&rig)
        .iter()
        .filter(|c| c.starts_with("kill_all"))
        .count();
    assert_eq!(rounds, KILL_ROUNDS);

    rig.host.state().fail.insert("kill_all");
    assert!(
        rig.helper
            .kill_uid("5.1")
            .unwrap_err()
            .contains("kill_all failed")
    );
}

/// Catches: `kill-uid` or `run` acting on a lease the helper never created, or on a
/// deleted one whose uid may now be another lease's, or on a uid outside the range.
#[test]
fn only_live_leases_in_range_are_acted_on() {
    let rig = rig("live", 2);
    assert!(
        rig.helper
            .kill_uid("6.1")
            .unwrap_err()
            .contains("has no user")
    );
    assert!(
        rig.helper
            .user_delete("6.1")
            .unwrap_err()
            .contains("has no user")
    );
    rig.helper.user_create("6.1", None).unwrap();
    rig.helper.user_delete("6.1").unwrap();
    assert!(
        rig.helper
            .kill_uid("6.1")
            .unwrap_err()
            .contains("was deleted")
    );
    assert!(
        rig.helper
            .kill_uid("x")
            .unwrap_err()
            .contains("not <term>.<seq>")
    );
    let empty = Vec::new;
    let error = rig
        .helper
        .run(
            "6.1",
            vec!["/usr/bin/true".to_owned()],
            empty(),
            fds(&rig.dir),
        )
        .unwrap_err();
    assert!(error.contains("was deleted"), "{error}");

    // The same ledger under a range that no longer holds the lease's uid.
    rig.helper.user_create("6.2", None).unwrap();
    let narrow = UidRange::new(me() + 5, me() + 6).unwrap();
    let moved = rig_with("live", narrow, Some(rig.dir.clone()));
    let error = moved.helper.kill_uid("6.2").unwrap_err();
    assert!(error.contains("outside"), "{error}");
    assert!(
        moved
            .helper
            .user_delete("6.2")
            .unwrap_err()
            .contains("outside")
    );
    assert!(log(&moved).iter().all(|call| !call.starts_with("kill_all")));
}

/// stdin, stdout (a pipe the test reads), stderr, and the lease directory.
fn fds(dir: &Path) -> Vec<OwnedFd> {
    let null = || -> OwnedFd { std::fs::File::open("/dev/null").unwrap().into() };
    vec![
        null(),
        null(),
        null(),
        std::fs::File::open(dir).unwrap().into(),
    ]
}

/// Catches: a process started as someone other than the lease user, outside the lease
/// directory, or with the descriptors in the wrong places.
#[test]
fn run_starts_the_process_as_the_lease_user_in_the_lease_directory() {
    let rig = rig("run", 2);
    rig.helper.user_create("7.1", None).unwrap();
    let (mut out, write) = std::io::pipe().unwrap();
    let mut fds = fds(&rig.dir);
    fds[1] = write.into();
    let argv = ["/bin/sh", "-c", "id -u; pwd; echo $HOME"]
        .map(str::to_owned)
        .to_vec();
    let mut child = rig.helper.run("7.1", argv, Vec::new(), fds).unwrap();
    assert!(child.wait().unwrap().success());
    let mut text = String::new();
    out.read_to_string(&mut text).unwrap();
    let want = format!(
        "{}\n{}\n{}\n",
        me(),
        std::fs::canonicalize(&rig.dir).unwrap().display(),
        rig.dir.join("Users/kbf-lease-7-1").display()
    );
    assert_eq!(text, want);
}

#[test]
fn run_refuses_a_bad_request() {
    let rig = rig("run-bad", 2);
    rig.helper.user_create("8.1", None).unwrap();
    let true_ = || vec!["/usr/bin/true".to_owned()];
    let three = fds(&rig.dir).into_iter().take(3).collect();
    let error = rig
        .helper
        .run("8.1", true_(), Vec::new(), three)
        .unwrap_err();
    assert!(error.contains("got 3 descriptors"), "{error}");
    let error = rig
        .helper
        .run("8.1", Vec::new(), Vec::new(), fds(&rig.dir))
        .unwrap_err();
    assert!(error.contains("empty"), "{error}");
    let error = rig
        .helper
        .run("8.x", true_(), Vec::new(), fds(&rig.dir))
        .unwrap_err();
    assert!(error.contains("not <term>.<seq>"), "{error}");
    let missing = vec!["/no/such/program".to_owned()];
    let error = rig
        .helper
        .run("8.1", missing, Vec::new(), fds(&rig.dir))
        .unwrap_err();
    assert!(error.starts_with("starting /no/such/program"), "{error}");
}

/// Catches: deleting a user whose processes still run (S4.2: refused while kill-uid
/// still finds a process), a deletion recorded before the sweep finished, or a
/// failed sweep or deletion reported as done.
#[test]
fn user_delete_refuses_live_processes_and_an_incomplete_sweep() {
    let rig = rig("delete", 2);
    let uid = rig.helper.user_create("9.1", None).unwrap();
    rig.host.state().procs.insert(uid, 2);
    let error = rig.helper.user_delete("9.1").unwrap_err();
    assert!(error.contains("2 processes"), "{error}");
    assert!(rig.host.state().users.contains_key("kbf-lease-9-1"));
    rig.host.state().procs.clear();

    // A home folder and a crontab of the user, and a shared folder whose files go.
    std::fs::create_dir_all(rig.dir.join("Users/kbf-lease-9-1/Library")).unwrap();
    std::fs::create_dir_all(rig.dir.join("tabs")).unwrap();
    std::fs::write(rig.dir.join("tabs/kbf-lease-9-1"), "x").unwrap();
    std::fs::create_dir_all(rig.dir.join("Shared/stuck")).unwrap();
    std::fs::write(rig.dir.join("Shared/stuck/f"), "x").unwrap();
    std::fs::write(rig.dir.join("Shared/loose"), "x").unwrap();

    rig.host.state().fail.insert("delete_user");
    let error = rig.helper.user_delete("9.1").unwrap_err();
    assert!(error.contains("delete_user failed"), "{error}");
    rig.host.state().fail.clear();

    if me() != 0 {
        // An entry the sweep cannot remove stops the deletion; the lease stays live.
        std::fs::create_dir_all(rig.dir.join("Shared/stuck")).unwrap();
        std::fs::write(rig.dir.join("Shared/stuck/f"), "x").unwrap();
        let stuck = rig.dir.join("Shared/stuck");
        std::fs::set_permissions(&stuck, std::os::unix::fs::PermissionsExt::from_mode(0o000))
            .unwrap();
        let error = rig.helper.user_delete("9.1").unwrap_err();
        assert!(
            error.contains("sweep for kbf-lease-9-1 is incomplete"),
            "{error}"
        );
        assert!(rig.host.state().users.contains_key("kbf-lease-9-1"));
        std::fs::set_permissions(&stuck, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
    }

    log(&rig);
    assert_eq!(rig.helper.user_delete("9.1"), Ok(true));
    assert!(!rig.dir.join("Users/kbf-lease-9-1").exists());
    assert!(!rig.dir.join("tabs/kbf-lease-9-1").exists());
    assert!(
        std::fs::read_dir(rig.dir.join("Shared"))
            .unwrap()
            .next()
            .is_none()
    );
    assert!(rig.host.state().users.is_empty());
    // Done once; a repeat touches nothing.
    log(&rig);
    assert_eq!(rig.helper.user_delete("9.1"), Ok(false));
    assert!(log(&rig).is_empty());

    // A record already gone (a delete that crashed after it) still completes.
    rig.helper.user_create("9.2", None).unwrap();
    rig.host.state().users.clear();
    assert_eq!(rig.helper.user_delete("9.2"), Ok(false));
    assert!(
        rig.helper
            .kill_uid("9.2")
            .unwrap_err()
            .contains("was deleted")
    );

    rig.helper.user_create("9.3", None).unwrap();
    rig.host.state().fail.insert("live_processes");
    assert!(
        rig.helper
            .user_delete("9.3")
            .unwrap_err()
            .contains("live_processes failed")
    );
}

/// Catches: requests routed to the wrong verb, or descriptors accepted (and leaked)
/// with a verb that takes none.
#[test]
fn requests_are_routed_and_stray_descriptors_refused() {
    let rig = rig("handle", 2);
    let lease = || "10.1".to_owned();
    let reply = |outcome: Outcome| match outcome {
        Outcome::Reply(reply) => reply,
        Outcome::Running(child) => panic!("a process started: {child:?}"),
    };
    let create = Request::UserCreate {
        lease: lease(),
        grant: None,
    };
    let stray = reply(rig.helper.handle(create.clone(), fds(&rig.dir)));
    assert_eq!(
        stray,
        Reply::Refused {
            reason: "only run takes descriptors".to_owned()
        }
    );
    assert_eq!(
        reply(rig.helper.handle(create, Vec::new())),
        Reply::Created { uid: me() }
    );
    let run = Request::Run {
        lease: lease(),
        argv: vec!["/usr/bin/true".to_owned()],
        env: Vec::new(),
    };
    let Outcome::Running(mut child) = rig.helper.handle(run.clone(), fds(&rig.dir)) else {
        panic!("run did not start");
    };
    assert!(child.wait().unwrap().success());
    let refused = reply(rig.helper.handle(run, Vec::new()));
    assert!(matches!(refused, Reply::Refused { .. }), "{refused:?}");
    let kill = Request::KillUid { lease: lease() };
    assert_eq!(reply(rig.helper.handle(kill, Vec::new())), Reply::Killed);
    let delete = Request::UserDelete { lease: lease() };
    let deleted = reply(rig.helper.handle(delete, Vec::new()));
    assert_eq!(deleted, Reply::Deleted { existed: true });
}

/// The portable home-folder maker, on the test's own uid.
#[test]
fn a_home_folder_is_new_private_and_the_users() {
    let dir = scratch("make-home");
    make_home(&dir, "kbf-lease-1-1", me(), my_gid()).unwrap();
    let meta = std::fs::metadata(dir.join("kbf-lease-1-1")).unwrap();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o777,
        0o700
    );
    assert_eq!(std::os::unix::fs::MetadataExt::uid(&meta), me());
    let again = make_home(&dir, "kbf-lease-1-1", me(), my_gid()).unwrap_err();
    assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
    std::os::unix::fs::symlink(&dir, dir.join("linked")).unwrap();
    assert!(make_home(&dir.join("linked"), "x", me(), my_gid()).is_err());
}
