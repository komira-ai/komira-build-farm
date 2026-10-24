//! The assertion that a pool user holds nothing a lease left, for real: the baseline is
//! recorded when the slots start, a clean user passes every probe, a state planted as
//! the user (a process, a launchd job, a System V semaphore, an attached disk image, a
//! file in the home, a file in `/private/tmp`, a cron job; and for the finer probes a
//! changed mode, flag, ACL or attribute of the home, a stray file and a changed folder in
//! the `/var/folders` entry, a changed directory record, a mount under the home, a crash
//! report in a shared folder, and a process with only its effective uid) turns the probe that looks
//! for it red, and the clean that follows removes it and the assertion is green again.
//! A slot that fails the assertion at slot-ready is set aside and comes back once a
//! clean passes the assertion.
//!
//! Every plant is made by the user's own processes, outside any sandbox, as a job that
//! left the sandbox would; no test runs `simctl` or another CoreSimulator tool as the
//! user, and neither does the assertion.
//!
//! Every test is `#[ignore]`: each needs root and real users. The macOS CI job runs them
//! with `cargo xtask sudo-test -p kbf-driver-native --test slot_assert`, with the users
//! of `tests/support/pool.rs`.

#![cfg(target_os = "macos")]

mod support;

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use kbf_driver_native::NativeRuntime;
use kbf_driver_native::slots::macos::Slots;
use kbf_driver_native::slots::{AS_USER_ARG, SlotUser};
use support::pool::{Scratch, Users, config, home_of, wait_until};
use support::{MemoryCas, Spec};

fn assert_root() {
    support::pool::assert_root("slot_assert");
}

/// A pool of one user, started the way the daemon starts it: the clean, the warm-up and
/// the baseline.
fn start(name: &str) -> (Users, Scratch, Slots) {
    start_prepared(name, None)
}

/// [`start`] after `prepare` ran for the user.
fn start_prepared(name: &str, prepare: Option<fn(&SlotUser)>) -> (Users, Scratch, Slots) {
    assert_root();
    let users = Users::new(name, 1);
    if let Some(prepare) = prepare {
        prepare(&users.0[0]);
    }
    let scratch = Scratch::new(name);
    let pool = users.pool();
    let slots = Slots::start(&pool, Duration::from_secs(10)).expect("slots");
    (users, scratch, slots)
}

/// Waits up to a minute for `done`: the recovery thread waits 5 s before its first pass.
fn wait_a_minute(what: &str, mut done: impl FnMut() -> bool) {
    let give_up = std::time::Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(
            std::time::Instant::now() < give_up,
            "{what}: not within a minute"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `program` with `args` as `user`, outside any sandbox, with a plain environment; its
/// stdout, and whether it exited 0.
fn as_user(user: &SlotUser, program: &str, args: &[&str]) -> (bool, String) {
    let out = Command::new(program)
        .args(args)
        .uid(user.uid)
        .gid(user.gid)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("HOME", home_of(user))
        .env("USER", &user.name)
        .env("LOGNAME", &user.name)
        .current_dir("/")
        .stdin(Stdio::null())
        .output()
        .expect(program);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// `program` with `args` as root; its output, and whether it exited 0.
fn root(program: &str, args: &[&str]) -> (bool, String) {
    let out = Command::new(program).args(args).output().expect(program);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// What a test plants as the user, and the line the assertion must then print.
struct Control {
    /// Names the user and the scratch directory.
    name: &'static str,
    /// The start of the line that must be among the red ones (`<probe>: + ...`).
    red: &'static str,
    /// Whether the cheap probes, run at slot-ready, must see it too.
    cheap: bool,
    /// Run for the user before the slots start, so that what the plant needs exists in the
    /// baseline.
    prepare: Option<fn(&SlotUser)>,
}

/// The positive control of one probe. With a pool of one user: the assertion is green;
/// `plant` makes its instance as the user; the assertion is red with a line that starts
/// with `control.red` (and so is the slot-ready check, if `cheap`); the slot is given
/// back, which cleans it and asserts again, and it is offered; taken again, the
/// assertion is green. `plant` returns what to say when it fails to plant, and a
/// substring the red line must carry.
async fn red_then_green(control: Control, plant: impl FnOnce(&SlotUser, &Scratch) -> String) {
    let (users, scratch, slots) = start_prepared(control.name, control.prepare);
    let user = users.0[0].clone();
    assert_eq!(
        slots.count(),
        1,
        "the start cleaned and recorded the baseline"
    );
    let slot = slots.take().await.expect("a slot");
    let before = slot.assert_everything().await.expect("ran");
    assert!(
        before.is_empty(),
        "red before anything was planted: {before:?}"
    );
    let ready = slot.assert_ready(&scratch.0).await.expect("ran");
    assert!(
        ready.is_empty(),
        "red at slot-ready before anything was planted: {ready:?}"
    );

    let carries = plant(&user, &scratch);
    let red = slot.assert_everything().await.expect("ran");
    assert!(
        red.iter()
            .any(|l| l.starts_with(control.red) && l.contains(&carries)),
        "the plant ({carries:?}) was not seen as {:?}: {red:?}",
        control.red
    );
    let ready = slot.assert_ready(&scratch.0).await.expect("ran");
    assert_eq!(
        ready.iter().any(|l| l.starts_with(control.red)),
        control.cheap,
        "the slot-ready check (cheap probes) on {:?}: {ready:?}",
        control.red
    );

    // The clean removes it, and the assertion that follows it is green: the slot is free.
    slot.release().await;
    wait_until("the slot is clean and offered again", || slots.count() == 1);
    let slot = slots.take().await.expect("the slot again");
    let after = slot.assert_everything().await.expect("ran");
    assert!(after.is_empty(), "red after the clean: {after:?}");
    slot.release().await;
    drop(users);
}

/// Catches an assertion that never runs against the baseline, or whose baseline is not
/// of a clean user: a used-once user that has been cleaned, taken for a lease, checked at
/// slot-ready, given back and taken again is green at every look. A baseline that held
/// something a later clean does not leave, a probe that reads its own tool's output as a
/// finding, or a slot-ready check that disagrees with the clean's would turn this red.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_clean_user_is_green_at_every_look() {
    let (_users, scratch, slots) = start("assert-green");
    assert_eq!(slots.count(), 1);
    for round in 0..3 {
        let slot = slots.take().await.expect("a slot");
        let all = slot.assert_everything().await.expect("ran");
        assert!(all.is_empty(), "round {round}: {all:?}");
        let ready = slot.assert_ready(&scratch.0).await.expect("ran");
        assert!(ready.is_empty(), "round {round}: {ready:?}");
        slot.release().await;
        wait_until("offered again", || slots.count() == 1);
    }
}

/// Catches a process probe that cannot see a lingering process of the user (the clean
/// kills what it is shown; one it is never shown would reach the next lease).
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_lingering_process_is_seen_then_cleaned() {
    let control = Control {
        name: "assert-proc",
        red: "processes: + ",
        cheap: true,
        prepare: None,
    };
    red_then_green(control, |user, _| {
        let sleeper = support::pool::sleeper(Some(user));
        let pid = sleeper.id().to_string();
        // Reaped by the clean's kill; the child handle is dropped without waiting.
        std::mem::forget(sleeper);
        format!("{pid} /bin/sleep")
    })
    .await;
}

/// A KeepAlive agent labelled `label` that runs `sleep 300`, for the Background session a
/// pool user's domain is.
fn agent(label: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key><array><string>/bin/sleep</string><string>300</string></array>
  <key>KeepAlive</key><true/>
  <key>RunAtLoad</key><true/>
  <key>LimitLoadToSessionType</key><string>Background</string>
</dict>
</plist>
"#
    )
}

/// Bootstraps the agent `label` into `user/<uid>` from the user's home, as the user
/// through `launchctl asuser` and the `as-user` drop of `drop_to`, and waits until
/// `launchctl print` lists it.
fn bootstrap(user: &SlotUser, drop_to: &str, label: &str) {
    let agents = home_of(user).join("Library").join("LaunchAgents");
    let (made, said) = as_user(user, "/bin/mkdir", &["-p", &agents.to_string_lossy()]);
    assert!(made, "mkdir LaunchAgents: {said}");
    let plist = agents.join(format!("{label}.plist"));
    std::fs::write(&plist, agent(label)).expect("agent plist");
    std::os::unix::fs::chown(&plist, Some(user.uid), Some(user.gid)).expect("chown");
    std::fs::set_permissions(&plist, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let domain = format!("user/{}", user.uid);
    let (uid, gid) = (user.uid.to_string(), user.gid.to_string());
    let path = plist.to_string_lossy().into_owned();
    let service = format!("{domain}/{label}");
    let (booted, said) = root(
        "/bin/launchctl",
        &[
            "asuser",
            &uid,
            drop_to,
            AS_USER_ARG,
            &uid,
            &gid,
            "--",
            "/bin/launchctl",
            "bootstrap",
            &domain,
            &path,
        ],
    );
    wait_until(&format!("{label} is listed ({booted}: {said})"), || {
        root("/bin/launchctl", &["print", &service]).0
    });
}

/// Catches a launchd probe that cannot see a job a lease bootstrapped into the user's
/// domain (with `LimitLoadToSessionType=Background`, which a pool user's domain accepts),
/// or that judges the job by a label chosen to look like launchd's own: this one is named
/// `com.apple.` and lives in the user's home, so it is not launchd's.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_launch_agent_is_seen_then_cleaned() {
    let control = Control {
        name: "assert-agent",
        red: "launchd: + job ",
        cheap: true,
        prepare: None,
    };
    red_then_green(control, |user, scratch| {
        bootstrap(
            user,
            &scratch.as_user_binary(),
            "com.apple.kbf-assert-agent",
        );
        "com.apple.kbf-assert-agent".to_owned()
    })
    .await;
}

/// Catches an IPC probe that cannot see a System V semaphore the user made (`ipcs`
/// lists it by owner and creator).
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_semaphore_is_seen_then_cleaned() {
    let control = Control {
        name: "assert-sem",
        red: "ipc: + s ",
        cheap: true,
        prepare: None,
    };
    red_then_green(control, |user, _| {
        let id = semaphore_of(user);
        format!("s {id}")
    })
    .await;
}

/// Makes a System V semaphore set as `user`, from a forked child that becomes the user
/// (a process of the test cannot change its own uid), and returns its id.
fn semaphore_of(user: &SlotUser) -> i32 {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a writable array of two descriptors.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
    // SAFETY: the child calls only async-signal-safe functions (setgid, setuid, semget,
    // write, _exit) before it exits, as a fork of a threaded process must.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork");
    if pid == 0 {
        // SAFETY: async-signal-safe calls only; `_exit` never returns.
        unsafe {
            if libc::setgid(user.gid) != 0 || libc::setuid(user.uid) != 0 {
                libc::_exit(1);
            }
            let id = libc::semget(libc::IPC_PRIVATE, 1, 0o600 | libc::IPC_CREAT);
            if id < 0 {
                libc::_exit(2);
            }
            let bytes = id.to_ne_bytes();
            libc::write(fds[1], bytes.as_ptr().cast(), bytes.len());
            libc::_exit(0);
        }
    }
    let mut bytes = [0_u8; 4];
    let mut status = 0;
    // SAFETY: `fds[0]` is the read end of the pipe, `bytes` a writable buffer of its
    // length, and `pid` this process's child.
    let (read, waited) = unsafe {
        libc::close(fds[1]);
        let read = libc::read(fds[0], bytes.as_mut_ptr().cast(), bytes.len());
        libc::close(fds[0]);
        (read, libc::waitpid(pid, &raw mut status, 0))
    };
    assert_eq!(waited, pid, "waitpid");
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "semget as the user: status {status:#x}"
    );
    assert_eq!(read, 4, "the child wrote the id");
    i32::from_ne_bytes(bytes)
}

/// Catches a mount probe that cannot see an image the user attached with no mount point
/// (it is in no `mount` line, only in `hdiutil info`).
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn an_attached_image_is_seen_then_cleaned() {
    let control = Control {
        name: "assert-image",
        red: "mounts: + image ",
        cheap: true,
        prepare: None,
    };
    red_then_green(control, |user, scratch| {
        let image: PathBuf = scratch.0.join("assert.dmg");
        let path = image.to_string_lossy().into_owned();
        let (made, said) = root(
            "/usr/bin/hdiutil",
            &[
                "create",
                "-quiet",
                "-size",
                "1m",
                "-fs",
                "HFS+",
                "-volname",
                "kbfassert",
                &path,
            ],
        );
        assert!(made, "hdiutil create: {said}");
        std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let (attached, said) = as_user(
            user,
            "/usr/bin/hdiutil",
            &["attach", "-quiet", "-nomount", &path],
        );
        assert!(attached, "hdiutil attach as the user: {said}");
        "attached by the uid".to_owned()
    })
    .await;
}

/// Catches a home probe that cannot see a file left in the home.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_file_in_the_home_is_seen_then_cleaned() {
    let control = Control {
        name: "assert-home",
        red: "home: + entry ",
        cheap: true,
        prepare: None,
    };
    red_then_green(control, |user, _| {
        let file = home_of(user).join("kbf-assert-file");
        std::fs::write(&file, b"left").expect("file");
        std::os::unix::fs::chown(&file, Some(user.uid), Some(user.gid)).expect("chown");
        "kbf-assert-file".to_owned()
    })
    .await;
}

/// Removes a path when dropped, so a failed test leaves nothing in a shared place.
struct Remove(PathBuf);

impl Drop for Remove {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Catches a shared-places probe that cannot see a file the user owns in
/// `/private/tmp` (the sweep of the clean covers it; the probe is what shows it did).
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_file_in_private_tmp_is_seen_then_cleaned() {
    let name = format!("kbf-assert-tmp-{}", std::process::id());
    let _guard = Remove(Path::new("/private/tmp").join(&name));
    let control = Control {
        name: "assert-tmp",
        red: "shared: + ",
        cheap: false,
        prepare: None,
    };
    red_then_green(control, |user, _| {
        let path = format!("/private/tmp/{name}");
        std::fs::write(&path, b"left").expect("file");
        std::os::unix::fs::chown(&path, Some(user.uid), Some(user.gid)).expect("chown");
        path
    })
    .await;
}

/// Catches a schedules probe that cannot see a cron job of the user.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_cron_job_is_seen_then_cleaned() {
    let control = Control {
        name: "assert-cron",
        red: "schedules: + ",
        cheap: false,
        prepare: None,
    };
    red_then_green(control, |user, _| {
        let mut crontab = Command::new("/usr/bin/crontab")
            .arg("-")
            .uid(user.uid)
            .gid(user.gid)
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", home_of(user))
            .env("USER", &user.name)
            .env("LOGNAME", &user.name)
            .current_dir("/")
            .stdin(Stdio::piped())
            .spawn()
            .expect("crontab");
        crontab
            .stdin
            .take()
            .expect("stdin")
            .write_all(b"0 0 1 1 * /usr/bin/true\n")
            .expect("the job");
        assert!(crontab.wait().expect("crontab").success(), "crontab");
        format!("/private/var/at/tabs/{}", user.name)
    })
    .await;
}

/// Catches a slot-ready check that does not set the slot aside (a lease would run on a
/// user that holds what the last one left), and a recovery that frees the slot without
/// asserting: a process planted after the clean is seen by the cheap probes, the slot is
/// set aside and the count is 0, and once the recovery thread has cleaned and asserted
/// it the slot is offered again with the process gone.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_slot_that_fails_the_check_at_slot_ready_is_set_aside_and_comes_back() {
    let (users, scratch, slots) = start("assert-aside");
    let user = users.0[0].clone();
    let slot = slots.take().await.expect("a slot");
    let sleeper = support::pool::sleeper(Some(&user));
    let pid = i32::try_from(sleeper.id()).expect("pid");
    std::mem::forget(sleeper);
    let red = slot.assert_ready(&scratch.0).await.expect("ran");
    assert!(
        red.iter().any(|l| l.starts_with("processes: + ")),
        "{red:?}"
    );
    slot.set_aside(&format!("assertion: {}", red.join("; ")));
    assert_eq!(
        slots.count(),
        0,
        "a slot that fails the check is not counted"
    );
    wait_a_minute("the recovery cleans and asserts the slot", || {
        slots.count() == 1
    });
    assert!(
        !kbf_daemon::uid::processes(user.uid)
            .unwrap_or_default()
            .contains(&pid),
        "the recovery's clean left the process"
    );
    let slot = slots.take().await.expect("the slot again");
    let after = slot.assert_everything().await.expect("ran");
    assert!(after.is_empty(), "{after:?}");
    slot.release().await;
}

/// Catches a lease that runs on a user the slot-ready check would refuse (the runtime
/// never calls the check, or ignores its answer): with one slot, a process of the user
/// started after the start-up clean makes the first slot fail the check, the lease waits
/// for the recovery thread (its first pass is [`FIRST_RECOVERY_PASS`] after the slot
/// goes dirty), and it
/// runs only on the cleaned user: the process is already dead, killed by that clean and
/// not by the clean after the lease, and the lease took longer than a lease that is not
/// held up.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_lease_waits_for_a_slot_that_fails_the_check_to_be_cleaned() {
    assert_root();
    let users = Users::new("assert-lease", 1);
    let scratch = Scratch::new("assert-lease");
    let cas = Arc::new(MemoryCas::default());
    let rt = NativeRuntime::new(config(&scratch, users.pool()), Arc::clone(&cas)).expect("runtime");
    let mut sleeper = support::pool::sleeper(Some(&users.0[0]));
    let started = std::time::Instant::now();
    // The lease waits for the slot to be set aside, for the recovery thread's first pass,
    // and for the clean and the assertion of that pass: its bound allows for them.
    let first_pass = kbf_driver_native::slots::macos::FIRST_RECOVERY_PASS;
    let bound =
        support::PROMPT * 3 + support::GRACE_ALLOWANCE + support::CLEAN_ALLOWANCE + first_pass;
    let ran = support::run_within(&rt, &cas, 1, &Spec::argv(&["/usr/bin/true"]), bound)
        .await
        .expect("the lease ran");
    let took = started.elapsed();
    assert_eq!(ran.exit_code, 0);
    let ended = sleeper.try_wait().expect("wait");
    assert!(
        ended.is_some_and(|status| !status.success()),
        "the lease ran before the user was cleaned: {ended:?}"
    );
    // The recovery thread's first pass over a slot is that long after the slot went dirty;
    // a lease that was not held up for it took a fraction of a second.
    assert!(
        took >= first_pass.saturating_sub(Duration::from_secs(1)),
        "the lease was not held up for the recovery ({first_pass:?}): {took:?}"
    );
}

/// `getconf <name>` as `user`: a path, which the system makes when it is asked.
fn getconf(user: &SlotUser, name: &str) -> PathBuf {
    let (ok, said) = as_user(user, "/usr/bin/getconf", &[name]);
    assert!(ok, "getconf {name}: {said}");
    PathBuf::from(said.trim())
}

/// The user's `/var/folders` entry: the parent of its `DARWIN_USER_DIR`.
fn entry_of(user: &SlotUser) -> PathBuf {
    let dir = getconf(user, "DARWIN_USER_DIR");
    let parent = dir.parent().expect("an entry").to_owned();
    std::fs::canonicalize(parent).expect("the entry exists")
}

/// Makes the folders of the entry before the slots start, so that the baseline has them.
fn make_entry(user: &SlotUser) {
    getconf(user, "DARWIN_USER_DIR");
    getconf(user, "DARWIN_USER_TEMP_DIR");
}

/// Catches an entry probe that cannot see a name in the entry root that is not one of its
/// folders (the clean removes it whole; the probe is what shows it did).
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_stray_file_in_the_entry_is_seen_then_cleaned() {
    let control = Control {
        name: "assert-entry-stray",
        red: "entry: + ",
        cheap: true,
        prepare: Some(make_entry),
    };
    red_then_green(control, |user, _| {
        let file = entry_of(user).join("kbf-assert-stray");
        std::fs::write(&file, b"left").expect("file");
        std::os::unix::fs::chown(&file, Some(user.uid), Some(user.gid)).expect("chown");
        "kbf-assert-stray: not part of the entry".to_owned()
    })
    .await;
}

/// Catches an entry probe that looks at the entry root only: a file in one of its folders
/// (`T`, the user's temporary folder) is a line of its own.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_file_in_an_entry_folder_is_seen_then_cleaned() {
    let control = Control {
        name: "assert-entry-folder",
        red: "entry: + ",
        cheap: true,
        prepare: Some(make_entry),
    };
    red_then_green(control, |user, _| {
        let file = getconf(user, "DARWIN_USER_TEMP_DIR").join("kbf-assert-leak");
        std::fs::write(&file, b"left").expect("file");
        std::os::unix::fs::chown(&file, Some(user.uid), Some(user.gid)).expect("chown");
        "T: [\"kbf-assert-leak\"]".to_owned()
    })
    .await;
}

/// Catches an entry probe that does not record the mode of a folder: `0` (the system
/// makes it 755) is made 775, wider than the system's, and the clean narrows it back.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_changed_mode_of_an_entry_folder_is_seen_then_cleaned() {
    let control = Control {
        name: "assert-entry-mode",
        red: "entry: + ",
        cheap: true,
        prepare: Some(make_entry),
    };
    red_then_green(control, |user, _| {
        let folder = getconf(user, "DARWIN_USER_DIR");
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o775)).expect("chmod");
        ". 0 mode 775".to_owned()
    })
    .await;
}

/// Catches an entry probe that does not read the extended attributes of a folder (the
/// descriptor-based `flistxattr` of the reader), and a clean that leaves one.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn an_attribute_on_an_entry_folder_is_seen_then_cleaned() {
    let control = Control {
        name: "assert-entry-xattr",
        red: "entry: + ",
        cheap: true,
        prepare: Some(make_entry),
    };
    red_then_green(control, |user, _| {
        let folder = getconf(user, "DARWIN_USER_DIR");
        let (ok, said) = root(
            "/usr/bin/xattr",
            &["-w", "kbf.assert", "x", &folder.to_string_lossy()],
        );
        assert!(ok, "xattr: {said}");
        ". 0 xattr kbf.assert".to_owned()
    })
    .await;
}

/// Catches a home probe that does not compare the mode (755 instead of 700).
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_changed_mode_of_the_home_is_seen_then_cleaned() {
    let control = Control {
        name: "assert-home-mode",
        red: "home: + ",
        cheap: true,
        prepare: None,
    };
    red_then_green(control, |user, _| {
        std::fs::set_permissions(home_of(user), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        "mode 755, not 700".to_owned()
    })
    .await;
}

/// Catches a home probe that cannot read the extended attributes of the home (a wrong
/// `flistxattr` call returns none), and a clean that leaves one.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn an_attribute_on_the_home_is_seen_then_cleaned() {
    let control = Control {
        name: "assert-home-xattr",
        red: "home: + ",
        cheap: true,
        prepare: None,
    };
    red_then_green(control, |user, _| {
        let (ok, said) = root(
            "/usr/bin/xattr",
            &["-w", "kbf.assert", "x", &home_of(user).to_string_lossy()],
        );
        assert!(ok, "xattr: {said}");
        "xattr kbf.assert".to_owned()
    })
    .await;
}

/// Catches a home probe that cannot read an ACL from `ls -led` (a changed format or a
/// wrong skip of the first line), and a clean that leaves one.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn an_acl_on_the_home_is_seen_then_cleaned() {
    let control = Control {
        name: "assert-home-acl",
        red: "home: + acl ",
        cheap: true,
        prepare: None,
    };
    red_then_green(control, |user, _| {
        let (ok, said) = root(
            "/bin/chmod",
            &[
                "+a",
                "everyone deny delete",
                &home_of(user).to_string_lossy(),
            ],
        );
        assert!(ok, "chmod +a: {said}");
        "deny delete".to_owned()
    })
    .await;
}

/// Catches a home probe that does not read the flags (`uchg` locks the home).
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_flag_on_the_home_is_seen_then_cleaned() {
    let control = Control {
        name: "assert-home-flag",
        red: "home: + ",
        cheap: true,
        prepare: None,
    };
    red_then_green(control, |user, _| {
        let (ok, said) = root(
            "/usr/bin/chflags",
            &["uchg", &home_of(user).to_string_lossy()],
        );
        assert!(ok, "chflags: {said}");
        "flags 0x2".to_owned()
    })
    .await;
}

/// The home as a system tool makes it can carry an ACL and an extended attribute. Catches
/// a baseline that demands a home with none (the slot would start dirty, and stay so):
/// with both on the home before the start, the slot is offered, and green at every look.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_home_made_with_an_attribute_and_an_acl_starts_clean() {
    fn mark(user: &SlotUser) {
        let home = home_of(user).to_string_lossy().into_owned();
        let (ok, said) = root(
            "/usr/bin/xattr",
            &["-w", "com.apple.provenance", "x", &home],
        );
        assert!(ok, "xattr: {said}");
        let (ok, said) = root("/bin/chmod", &["+a", "everyone deny delete", &home]);
        assert!(ok, "chmod +a: {said}");
    }
    let (_users, scratch, slots) = start_prepared("assert-home-made", Some(mark));
    assert_eq!(slots.count(), 1, "the slot started dirty");
    let slot = slots.take().await.expect("a slot");
    let all = slot.assert_everything().await.expect("ran");
    assert!(all.is_empty(), "{all:?}");
    let ready = slot.assert_ready(&scratch.0).await.expect("ran");
    assert!(ready.is_empty(), "{ready:?}");
    slot.release().await;
}

/// Sets `UserShell` of a user's directory record back when dropped (the clean does not
/// repair the record), so a failed test leaves the user as it was.
struct ShellBack {
    record: String,
    was: Option<String>,
}

impl Drop for ShellBack {
    fn drop(&mut self) {
        let _ = match &self.was {
            Some(shell) => root(
                "/usr/bin/dscl",
                &[".", "-create", &self.record, "UserShell", shell],
            ),
            None => root(
                "/usr/bin/dscl",
                &[".", "-delete", &self.record, "UserShell"],
            ),
        };
    }
}

/// Catches a record probe that does not read the directory record (a constant, or an
/// `id` only): a changed `UserShell` is a line that is new and one that is gone, and the
/// record is green again once it is put back.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_changed_directory_record_is_seen_and_green_again_when_restored() {
    let (users, _scratch, slots) = start("assert-record");
    let user = users.0[0].clone();
    let slot = slots.take().await.expect("a slot");
    let record = format!("/Users/{}", user.name);
    let (_, read) = root("/usr/bin/dscl", &[".", "-read", &record, "UserShell"]);
    let was = read
        .strip_prefix("UserShell:")
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    let _back = ShellBack {
        record: record.clone(),
        was: was.clone(),
    };
    let (ok, said) = root(
        "/usr/bin/dscl",
        &[
            ".",
            "-create",
            &record,
            "UserShell",
            "/bin/kbf-assert-shell",
        ],
    );
    assert!(ok, "dscl -create: {said}");
    let red = slot.assert_everything().await.expect("ran");
    assert!(
        red.iter()
            .any(|l| l == "record: + UserShell=/bin/kbf-assert-shell"),
        "{red:?}"
    );
    assert!(
        red.iter()
            .any(|l| l.starts_with("record: - UserShell=") && !l.contains("kbf-assert-shell")),
        "{red:?}"
    );
    drop(_back);
    let green = slot.assert_everything().await.expect("ran");
    assert!(green.is_empty(), "{green:?}");
    slot.release().await;
}

/// Detaches a disk image when dropped.
struct Detach(Mutex<Option<String>>);

impl Drop for Detach {
    fn drop(&mut self) {
        let device = self.0.lock().ok().and_then(|mut d| d.take());
        if let Some(device) = device {
            let _ = root("/usr/bin/hdiutil", &["detach", "-force", &device]);
        }
    }
}

/// Catches a mount probe that cannot see a filesystem mounted under the home (the clean
/// unmounts it): an image attached by root with its mount point in the home.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_mount_under_the_home_is_seen_then_cleaned() {
    let detach = Arc::new(Detach(Mutex::new(None)));
    let control = Control {
        name: "assert-mount",
        red: "mounts: + mounted ",
        cheap: true,
        prepare: None,
    };
    let device = Arc::clone(&detach);
    red_then_green(control, move |user, scratch| {
        let image: PathBuf = scratch.0.join("mount.dmg");
        let path = image.to_string_lossy().into_owned();
        let (made, said) = root(
            "/usr/bin/hdiutil",
            &[
                "create", "-quiet", "-size", "1m", "-fs", "HFS+", "-volname", "kbfmount", &path,
            ],
        );
        assert!(made, "hdiutil create: {said}");
        let point = home_of(user).join("kbf-mnt");
        std::fs::create_dir(&point).expect("mount point");
        std::os::unix::fs::chown(&point, Some(user.uid), Some(user.gid)).expect("chown");
        let (attached, said) = root(
            "/usr/bin/hdiutil",
            &[
                "attach",
                "-nobrowse",
                "-mountpoint",
                &point.to_string_lossy(),
                &path,
            ],
        );
        assert!(attached, "hdiutil attach: {said}");
        let whole = said
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().next())
            .map(str::to_owned);
        *device.0.lock().expect("lock") = whole;
        "kbf-mnt".to_owned()
    })
    .await;
    drop(detach);
}

/// Catches a shared-places probe that does not look in the crash folders: a report that
/// names the user's uid, written by root as the crash reporter does, in the system's
/// `DiagnosticReports`.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_crash_report_naming_the_user_is_seen_then_cleaned() {
    let folder = Path::new("/Library/Logs/DiagnosticReports");
    std::fs::create_dir_all(folder).expect("reports");
    let report = folder.join(format!("kbf-assert-{}.ips", std::process::id()));
    let _guard = Remove(report.clone());
    let control = Control {
        name: "assert-crash",
        red: "shared: + ",
        cheap: false,
        prepare: None,
    };
    red_then_green(control, |user, _| {
        std::fs::write(
            &report,
            format!(
                "{{\"app_name\":\"canary\",\"bug_type\":\"309\"}}\n{{\"pid\":912,\"userID\":{},\"procName\":\"canary\"}}\n",
                user.uid
            ),
        )
        .expect("report");
        report.to_string_lossy().into_owned()
    })
    .await;
}

/// Catches a process probe that matches the real uid only: a process whose real uid is
/// root and whose effective uid is the user's (a setuid program that dropped half of its
/// rights) is a finding, and the assertion is green once it is ended.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_assert"]
#[tokio::test(flavor = "multi_thread")]
async fn a_process_with_only_the_effective_uid_of_the_user_is_seen() {
    let (users, _scratch, slots) = start("assert-euid");
    let user = users.0[0].clone();
    let slot = slots.take().await.expect("a slot");
    let uid = user.uid;
    let mut command = Command::new("/bin/sleep");
    command.arg("300").stdin(Stdio::null());
    // SAFETY: the closure calls only `seteuid`, which is async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            if libc::seteuid(uid) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let mut child = command.spawn().expect("sleep");
    let pid = child.id();
    let red = slot.assert_everything().await.expect("ran");
    let wanted = format!("processes: + {pid} /bin/sleep");
    let seen = red.contains(&wanted);
    child.kill().expect("kill");
    child.wait().expect("wait");
    assert!(seen, "{wanted:?} not in {red:?}");
    let green = slot.assert_everything().await.expect("ran");
    assert!(green.is_empty(), "{green:?}");
    slot.release().await;
}
