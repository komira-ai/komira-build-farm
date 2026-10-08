//! The verbs end to end over a fake applier: S3.1's checks through `stage` and `apply`,
//! the floor, the state file across a restart, and staging's digest checks.

use std::os::unix::fs::symlink;

use super::*;
use crate::apply::FakeApplier;
use crate::set::MAX_SERIAL_STEP;
use crate::signed::public_key;
use crate::testkit::{self, COMPONENT_SEED, PLATFORM_SEED, ROOT_SEED, seal_set, sha};

const NOW: u64 = 1_000;

fn config(dir: &Path) -> Config {
    Config {
        root_key: public_key(&ROOT_SEED),
        pin: testkit::pin(),
        state_dir: dir.join("state"),
        artifacts_dir: dir.join("artifacts"),
    }
}

/// An updater over a fresh state directory, with the artifacts `d`, `u` and `d2` in
/// its artifacts directory.
fn updater(name: &str) -> (Updater<FakeApplier>, PathBuf) {
    let dir = testkit::scratch(name);
    let cfg = config(&dir);
    fs::create_dir_all(&cfg.state_dir).unwrap();
    fs::create_dir_all(&cfg.artifacts_dir).unwrap();
    for content in ["d", "u", "d2"] {
        fs::write(cfg.artifacts_dir.join(sha(content)), content).unwrap();
    }
    (Updater::open(cfg, FakeApplier::default()).unwrap(), dir)
}

fn reopen(u: Updater<FakeApplier>) -> Updater<FakeApplier> {
    let applier = u.applier().clone();
    Updater::open(u.cfg, applier).unwrap()
}

fn statement(serial: u64, component: &[[u8; 32]]) -> Envelope {
    testkit::seal_statement(&testkit::statement(serial, component))
}

fn set_with(serial: u64, f: impl FnOnce(&mut crate::set::SoftwareSet)) -> crate::set::SoftwareSet {
    let mut s = testkit::set(serial);
    f(&mut s);
    s
}

fn daemon_only(serial: u64) -> crate::set::SoftwareSet {
    set_with(serial, |s| {
        s.artifacts.get_mut("kbf-daemon").unwrap().sha256 = sha("d2");
    })
}

/// Stages and applies `set` under statement 1, which names the component key.
fn install(u: &mut Updater<FakeApplier>, set: &Envelope) -> Outcome {
    let st = statement(1, &[COMPONENT_SEED]);
    assert_eq!(u.stage(set, Some(&st), NOW), Ok(Outcome::Staged));
    u.apply(set, Some(&st), NOW).unwrap()
}

/// Catches: `stage` changing what is installed, `apply` not installing the staged
/// artifacts, the installed set not recorded, the staging directory left behind, or the
/// installed set reinstalled instead of a no-op.
#[test]
fn stage_then_apply_installs_and_repeating_is_a_no_op() {
    let (mut u, _) = updater("flow");
    let set = seal_set(&testkit::set(5), &PLATFORM_SEED);
    let st = statement(1, &[]);
    assert_eq!(u.stage(&set, Some(&st), NOW), Ok(Outcome::Staged));
    assert!(u.applier().installs.is_empty(), "stage installed something");
    let status = u.status();
    assert_eq!(status.installed, None);
    assert_eq!(status.staged.as_ref().map(|s| s.serial), Some(5));
    assert_eq!(status.pool, "linux-x86");
    assert_eq!(
        u.apply(&set, None, NOW),
        Ok(Outcome::Applied { reboot: false })
    );
    assert_eq!(
        u.applier().installs,
        [(5, vec!["kbf-daemon".to_owned(), "kbf-updater".to_owned()])]
    );
    let status = u.status();
    assert_eq!(status.installed.map(|s| s.serial), Some(5));
    assert_eq!(status.staged, None);
    assert_eq!(status.in_progress, None);
    assert!(!u.staging().exists());
    assert_eq!(u.apply(&set, None, NOW), Ok(Outcome::AlreadyInstalled));
    assert_eq!(u.stage(&set, None, NOW), Ok(Outcome::AlreadyInstalled));
    assert_eq!(u.applier().installs.len(), 1);
    assert_eq!(u.applier().reboots, 0);
}

/// Catches: `apply` of a set that was never staged, or of a set other than the staged
/// one (its artifacts were never hashed).
#[test]
fn apply_installs_only_the_staged_set() {
    let (mut u, _) = updater("not-staged");
    let st = statement(1, &[]);
    let five = seal_set(&testkit::set(5), &PLATFORM_SEED);
    let six = seal_set(&testkit::set(6), &PLATFORM_SEED);
    assert!(matches!(
        u.apply(&five, Some(&st), NOW),
        Err(Refusal::NotStaged(_))
    ));
    assert_eq!(u.stage(&five, None, NOW), Ok(Outcome::Staged));
    assert!(matches!(
        u.apply(&six, None, NOW),
        Err(Refusal::NotStaged(_))
    ));
    assert!(u.applier().installs.is_empty());
}

/// Catches: skipping the serial check (a replayed older set or an equal serial with
/// other contents reinstalls) in either verb.
#[test]
fn a_replayed_older_set_is_refused() {
    let (mut u, _) = updater("replay");
    install(&mut u, &seal_set(&testkit::set(6), &PLATFORM_SEED));
    let older = seal_set(&testkit::set(5), &PLATFORM_SEED);
    let refused = Err(Refusal::NotNewer {
        serial: 5,
        installed: 6,
    });
    assert_eq!(u.stage(&older, None, NOW), refused);
    assert_eq!(u.apply(&older, None, NOW), refused);
    let twin = seal_set(&set_with(6, |s| s.expires += 1), &PLATFORM_SEED);
    assert_eq!(
        u.stage(&twin, None, NOW),
        Err(Refusal::NotNewer {
            serial: 6,
            installed: 6
        })
    );
}

/// Catches: the floor not raised by a staged set, not kept across a restart, or not
/// checked (a node that saw a rollback set can be moved to the bad set it retired).
#[test]
fn a_staged_set_raises_the_floor_for_good() {
    let (mut u, _) = updater("floor");
    let st = statement(1, &[]);
    let bad = seal_set(&testkit::set(7), &PLATFORM_SEED);
    let rollback = seal_set(&set_with(8, |s| s.min_serial = 8), &PLATFORM_SEED);
    assert_eq!(u.stage(&rollback, Some(&st), NOW), Ok(Outcome::Staged));
    assert_eq!(u.status().floor, 8);
    let mut u = reopen(u);
    assert_eq!(u.status().floor, 8);
    assert_eq!(
        u.stage(&bad, None, NOW),
        Err(Refusal::BelowFloor {
            serial: 7,
            floor: 8
        })
    );
    // A later set with a lower min_serial never lowers the floor.
    let later = seal_set(&set_with(9, |s| s.min_serial = 2), &PLATFORM_SEED);
    assert_eq!(u.stage(&later, None, NOW), Ok(Outcome::Staged));
    assert_eq!(u.status().floor, 8);
}

/// Catches: the expiry, pool or signature check skipped on the way through the verbs.
#[test]
fn expired_foreign_and_unsigned_sets_are_refused() {
    let (mut u, _) = updater("refusals");
    let st = statement(1, &[]);
    let set = set_with(5, |s| s.expires = 1_500);
    let sealed = seal_set(&set, &PLATFORM_SEED);
    assert_eq!(u.stage(&sealed, Some(&st), 1_499), Ok(Outcome::Staged));
    assert_eq!(u.stage(&sealed, None, 1_500), Err(Refusal::Expired(5)));
    assert_eq!(u.apply(&sealed, None, 1_500), Err(Refusal::Expired(5)));
    let mac = seal_set(
        &set_with(5, |s| s.pool = "macos-arm64".into()),
        &PLATFORM_SEED,
    );
    assert_eq!(
        u.stage(&mac, None, NOW),
        Err(Refusal::WrongPool("macos-arm64".into()))
    );
    let mut unsigned = sealed.clone();
    unsigned.signature = seal_set(&testkit::set(4), &PLATFORM_SEED).signature;
    assert_eq!(u.stage(&unsigned, None, NOW), Err(Refusal::BadSignature));
    assert_eq!(u.apply(&unsigned, None, NOW), Err(Refusal::BadSignature));
    let stranger = seal_set(&set, &[7; 32]);
    assert_eq!(u.stage(&stranger, None, NOW), Err(Refusal::UnknownKey));
    assert!(u.applier().installs.is_empty());
}

/// Catches: a node without any key statement accepting a set, or a set signed by the
/// root key itself (the root signs statements only).
#[test]
fn a_set_needs_a_statement_and_a_named_key() {
    let (mut u, _) = updater("no-statement");
    let set = seal_set(&testkit::set(5), &PLATFORM_SEED);
    assert_eq!(u.stage(&set, None, NOW), Err(Refusal::NoStatement));
    let by_root = seal_set(&testkit::set(5), &ROOT_SEED);
    assert_eq!(
        u.stage(&by_root, Some(&statement(1, &[])), NOW),
        Err(Refusal::UnknownKey)
    );
}

/// Catches: skipping key coverage through the verbs (a component-key set changes the
/// updater), or a component-key set that changes only the daemon refused, or its
/// unchanged artifacts copied.
#[test]
fn a_component_key_set_installs_only_daemon_changes() {
    let (mut u, _) = updater("coverage");
    install(&mut u, &seal_set(&testkit::set(5), &PLATFORM_SEED));
    let daemon = seal_set(&daemon_only(6), &COMPONENT_SEED);
    assert_eq!(install(&mut u, &daemon), Outcome::Applied { reboot: false });
    assert_eq!(u.applier().installs[1], (6, vec!["kbf-daemon".to_owned()]));
    let updater_change = set_with(7, |s| {
        s.artifacts.get_mut("kbf-daemon").unwrap().sha256 = sha("d2");
        s.artifacts.get_mut("kbf-updater").unwrap().sha256 = sha("d");
    });
    let sealed = seal_set(&updater_change, &COMPONENT_SEED);
    assert_eq!(
        u.stage(&sealed, None, NOW),
        Err(Refusal::NotCovered("kbf-updater".into()))
    );
    assert_eq!(
        u.apply(&sealed, None, NOW),
        Err(Refusal::NotCovered("kbf-updater".into()))
    );
}

/// Catches: a newer statement forgotten when its set is refused (a revocation that does
/// not stick), or an older statement used again after a newer one was seen.
#[test]
fn a_revocation_sticks_even_when_its_set_is_refused() {
    let (mut u, _) = updater("revocation");
    install(&mut u, &seal_set(&testkit::set(5), &PLATFORM_SEED));
    let revoking = statement(2, &[]);
    let stranger = seal_set(&testkit::set(6), &[7; 32]);
    assert_eq!(
        u.stage(&stranger, Some(&revoking), NOW),
        Err(Refusal::UnknownKey)
    );
    let mut u = reopen(u);
    let daemon = seal_set(&daemon_only(6), &COMPONENT_SEED);
    let old = statement(1, &[COMPONENT_SEED]);
    assert_eq!(u.stage(&daemon, Some(&old), NOW), Err(Refusal::UnknownKey));
    // Under a newer statement naming it again, the same set stages.
    let renamed = statement(3, &[COMPONENT_SEED]);
    assert_eq!(u.stage(&daemon, Some(&renamed), NOW), Ok(Outcome::Staged));
}

/// Catches: a crash mid-apply forgotten on restart, another set staged or applied over
/// it, or the same set not resumable.
#[test]
fn an_apply_in_progress_survives_a_restart_and_only_it_may_continue() {
    let (mut u, _) = updater("in-progress");
    let st = statement(1, &[]);
    let five = seal_set(&testkit::set(5), &PLATFORM_SEED);
    assert_eq!(u.stage(&five, Some(&st), NOW), Ok(Outcome::Staged));
    u.applier_mut().fail_install = true;
    assert_eq!(
        u.apply(&five, None, NOW),
        Err(Refusal::Apply("planned failure".into()))
    );
    let digest = u
        .status()
        .in_progress
        .expect("in progress after a failed install");
    let mut u = reopen(u);
    assert_eq!(u.status().in_progress.as_ref(), Some(&digest));
    let six = seal_set(&testkit::set(6), &PLATFORM_SEED);
    assert_eq!(
        u.stage(&six, None, NOW),
        Err(Refusal::InProgress(digest.clone()))
    );
    assert_eq!(u.apply(&six, None, NOW), Err(Refusal::InProgress(digest)));
    u.applier_mut().fail_install = false;
    assert_eq!(
        u.apply(&five, None, NOW),
        Ok(Outcome::Applied { reboot: false })
    );
    assert_eq!(u.status().in_progress, None);
}

/// Catches: a node wedged for good by an apply in progress whose set has expired: the
/// set itself can no longer be applied, so unless a newer set that passes every check
/// may replace it, every later `stage` and `apply` is refused until someone edits the
/// state file by hand. Also: the expired set itself let through.
#[test]
fn an_expired_apply_in_progress_gives_way_to_a_newer_valid_set() {
    let (mut u, _) = updater("in-progress-expired");
    let st = statement(1, &[]);
    let five = seal_set(&set_with(5, |s| s.expires = 1_500), &PLATFORM_SEED);
    assert_eq!(u.stage(&five, Some(&st), NOW), Ok(Outcome::Staged));
    u.applier_mut().fail_install = true;
    assert!(u.apply(&five, None, NOW).is_err());
    let stuck = u.status().in_progress.expect("in progress");
    u.applier_mut().fail_install = false;
    let later = 1_500;
    assert_eq!(u.apply(&five, None, later), Err(Refusal::Expired(5)));
    // While the in-progress set still passes, it alone may continue (the restart test
    // above); once it cannot, a newer set that passes every check replaces it.
    let six = seal_set(&testkit::set(6), &PLATFORM_SEED);
    assert_eq!(u.stage(&six, None, later), Ok(Outcome::Staged));
    // The unfinished apply is still reported until the newer set's apply starts.
    assert_eq!(u.status().in_progress, Some(stuck));
    // With the in-progress set no longer staged, it blocks nothing either.
    let seven = seal_set(&testkit::set(7), &PLATFORM_SEED);
    assert_eq!(u.stage(&seven, None, later), Ok(Outcome::Staged));
    assert_eq!(
        u.apply(&seven, None, later),
        Ok(Outcome::Applied { reboot: false })
    );
    let status = u.status();
    assert_eq!(status.installed.map(|s| s.serial), Some(7));
    assert_eq!(status.in_progress, None);
}

/// Catches: a node wedged for good by an apply in progress whose signing key a newer
/// key statement revoked (the set fails as `UnknownKey`, every other set as
/// `InProgress`).
#[test]
fn an_apply_in_progress_under_a_revoked_key_gives_way_to_a_newer_valid_set() {
    let (mut u, _) = updater("in-progress-revoked");
    install(&mut u, &seal_set(&testkit::set(5), &PLATFORM_SEED));
    let daemon = seal_set(&daemon_only(6), &COMPONENT_SEED);
    assert_eq!(u.stage(&daemon, None, NOW), Ok(Outcome::Staged));
    u.applier_mut().fail_install = true;
    assert!(u.apply(&daemon, None, NOW).is_err());
    u.applier_mut().fail_install = false;
    let revoking = statement(2, &[]);
    assert_eq!(
        u.apply(&daemon, Some(&revoking), NOW),
        Err(Refusal::UnknownKey)
    );
    let seven = seal_set(&testkit::set(7), &PLATFORM_SEED);
    assert_eq!(u.stage(&seven, None, NOW), Ok(Outcome::Staged));
    assert_eq!(
        u.apply(&seven, None, NOW),
        Ok(Outcome::Applied { reboot: false })
    );
    assert_eq!(u.status().installed.map(|s| s.serial), Some(7));
}

/// Catches: one set stopping updates on a node for good by jumping its serial (and
/// `min_serial`, so the floor) to `u64::MAX`: no later set could ever be newer or at
/// the floor. A compromised component key could do it with a set that changes only
/// `kbf-daemon`, and the revocation of S2.3 assumes a newer set can always follow.
#[test]
fn a_serial_jump_past_the_bound_is_refused() {
    let (mut u, _) = updater("serial-jump");
    install(&mut u, &seal_set(&testkit::set(5), &PLATFORM_SEED));
    let mut jump = daemon_only(u64::MAX);
    jump.min_serial = u64::MAX;
    let jump = seal_set(&jump, &COMPONENT_SEED);
    let refused = Err(Refusal::SerialJump {
        serial: u64::MAX,
        installed: 5,
    });
    assert_eq!(u.stage(&jump, None, NOW), refused);
    assert_eq!(u.apply(&jump, None, NOW), refused);
    assert_eq!(u.status().floor, 0);
    // The largest step allowed still installs, and a newer set can follow it.
    let top = 5 + MAX_SERIAL_STEP;
    let step = seal_set(&set_with(top, |s| s.min_serial = top), &PLATFORM_SEED);
    assert_eq!(install(&mut u, &step), Outcome::Applied { reboot: false });
    let next = seal_set(&testkit::set(top + 1), &PLATFORM_SEED);
    assert_eq!(install(&mut u, &next), Outcome::Applied { reboot: false });
}

/// Catches: a reboot skipped when the install asks for one.
#[test]
fn apply_reboots_when_the_install_asks() {
    let (mut u, _) = updater("reboot");
    u.applier_mut().needs_reboot = true;
    let set = seal_set(&testkit::set(5), &PLATFORM_SEED);
    assert_eq!(install(&mut u, &set), Outcome::Applied { reboot: true });
    assert_eq!(u.applier().reboots, 1);
}

/// Catches: an artifact whose bytes do not match its digest staged, a symbolic link in
/// the daemon's artifacts directory followed (root would copy any file it points at),
/// a directory or missing file taken, or a failed stage left looking staged.
#[test]
fn staging_takes_only_regular_files_that_match_their_digest() {
    let (mut u, dir) = updater("digests");
    let st = statement(1, &[]);
    let artifacts = dir.join("artifacts");
    let set = seal_set(&testkit::set(5), &PLATFORM_SEED);
    fs::write(artifacts.join(sha("u")), "not u").unwrap();
    assert_eq!(
        u.stage(&set, Some(&st), NOW),
        Err(Refusal::DigestMismatch("kbf-updater".into()))
    );
    assert!(!u.staging().join("kbf-updater").exists());
    assert_eq!(u.status().staged, None);
    let secret = dir.join("secret");
    fs::write(&secret, "u").unwrap();
    fs::remove_file(artifacts.join(sha("u"))).unwrap();
    symlink(&secret, artifacts.join(sha("u"))).unwrap();
    assert!(matches!(
        u.stage(&set, None, NOW),
        Err(Refusal::Artifact(_))
    ));
    fs::remove_file(artifacts.join(sha("u"))).unwrap();
    fs::create_dir(artifacts.join(sha("u"))).unwrap();
    assert_eq!(
        u.stage(&set, None, NOW),
        Err(Refusal::Artifact("kbf-updater: not a regular file".into()))
    );
    fs::remove_dir(artifacts.join(sha("u"))).unwrap();
    assert!(matches!(
        u.stage(&set, None, NOW),
        Err(Refusal::Artifact(_))
    ));
    fs::write(artifacts.join(sha("u")), "u").unwrap();
    assert_eq!(u.stage(&set, None, NOW), Ok(Outcome::Staged));
}

/// Catches: a large artifact truncated or hashed over its first buffer only.
#[test]
fn a_large_artifact_is_copied_and_hashed_whole() {
    let (mut u, dir) = updater("large");
    let big = "x".repeat(200_000);
    fs::write(dir.join("artifacts").join(sha(&big)), &big).unwrap();
    let set = set_with(5, |s| {
        s.artifacts.get_mut("kbf-daemon").unwrap().sha256 = sha(&big)
    });
    let sealed = seal_set(&set, &PLATFORM_SEED);
    assert_eq!(
        u.stage(&sealed, Some(&statement(1, &[])), NOW),
        Ok(Outcome::Staged)
    );
    assert_eq!(
        fs::read_to_string(u.staging().join("kbf-daemon")).unwrap(),
        big
    );
}

/// Catches: a state directory the updater cannot write treated as success.
#[test]
fn an_unwritable_state_is_an_error() {
    let (u, dir) = updater("unwritable");
    let mut cfg = u.cfg.clone();
    cfg.state_dir = dir.join("missing");
    let mut u = Updater::open(cfg, FakeApplier::default()).unwrap();
    let set = seal_set(&testkit::set(5), &PLATFORM_SEED);
    assert!(matches!(
        u.stage(&set, Some(&statement(1, &[])), NOW),
        Err(Refusal::State(_))
    ));
    // A file where the staging directory belongs.
    let (mut u, _) = updater("staging-file");
    fs::write(u.staging(), b"").unwrap();
    assert!(matches!(
        u.stage(&set, Some(&statement(1, &[])), NOW),
        Err(Refusal::State(_))
    ));
    let state_file_is_dir = dir.join("state").join("state.json");
    fs::create_dir_all(&state_file_is_dir).unwrap();
    let mut cfg = config(&dir);
    cfg.state_dir = dir.join("state");
    assert!(matches!(
        Updater::open(cfg, FakeApplier::default()),
        Err(Refusal::State(_))
    ));
}

/// Catches: a change with no artifact to fetch (a dropped artifact, a new package
/// snapshot) failing the stage, or an unchanged artifact copied again.
#[test]
fn changes_without_an_artifact_stage_nothing_extra() {
    let (mut u, _) = updater("no-fetch");
    install(&mut u, &seal_set(&testkit::set(5), &PLATFORM_SEED));
    let next = set_with(6, |s| {
        s.artifacts.remove("kbf-updater");
        s.snapshot = Some(crate::set::AptPin {
            snapshot: "20261001T000000Z".into(),
            kernel: "linux-image-6.8".into(),
        });
    });
    assert_eq!(
        install(&mut u, &seal_set(&next, &PLATFORM_SEED)),
        Outcome::Applied { reboot: false }
    );
    assert_eq!(u.applier().installs[1], (6, vec![]));
}
