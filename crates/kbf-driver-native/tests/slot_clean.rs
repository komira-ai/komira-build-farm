//! The clean of a slot between two leases, for real (docs/design/macos-end-state.md,
//! step 4): after every lease the daemon cleans its slot's user in one pass, checks the
//! clean, and leaves the slot dirty (not offered, cleaned again before it is) when a step
//! fails or the check finds something.
//!
//! The gate is [`a_lease_finds_nothing_the_lease_before_it_left_on_its_slot`]: with
//! one slot, lease A leaves a marker in each place a lease can leave state, and lease B,
//! on the same user, looks for each through the tool that would use it and finds none.
//! The other tests: a process the uid cannot kill fails the clean within its deadline
//! and leaves the slot dirty; the start cleans what a killed daemon left; a lease's job
//! labelled like launchd's own is still counted; and the single-pass tests
//! (`tests/slot_clean/single_pass.rs`: a home locked by its user, a file in the entry
//! root, `/Library/Caches`, a crash report under another user's folder, a failed clean).
//!
//! The tests of necessity (`tests/slot_clean/steps_needed.rs`) run the same scenario with
//! one step of the clean left out and the clean's own check blind, and see which rows
//! lease B then finds; the table is `tests/support/slot_rows.rs`, its model
//! `tests/slot_steps.rs` (every platform).
//!
//! Every test is `#[ignore]`: each needs root and real users. The macOS CI job runs them
//! with `cargo xtask sudo-test -p kbf-driver-native --test slot_clean`, after
//! `tests/slots.rs`, with the users of `tests/support/pool.rs`.

#![cfg(target_os = "macos")]

mod support;

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kbf_daemon::Runtime as _;
use kbf_daemon::uid;
use kbf_driver_native::NativeRuntime;
use kbf_driver_native::slots::SlotUser;
use kbf_driver_native::slots::clean::Skipped;
use support::pool::{Scratch, Users, config, home_of, ids_of};
use support::slot_rows::ROWS;
use support::{MemoryCas, PROMPT, Spec, stdout};

fn assert_root() {
    support::pool::assert_root("slot_clean");
}

/// Lease A's part inside the sandbox, as the slot user: `$1` the marker. Each place
/// written prints `planted <row>`; then it waits for the test's `go`.
const PLANT_INSIDE: &str = r#"m=$1
eval "h=~$(id -un)"
t=$(getconf DARWIN_USER_TEMP_DIR)
c=$(getconf DARWIN_USER_CACHE_DIR)
ok() { echo "planted $*"; }
printf %s "$m" > kbf-fresh && ok lease-dir
printf %s "$m" > "$h/kbf-fresh" && ok home
defaults write kbf.ci.fresh v "$m" && [ "$(defaults read kbf.ci.fresh v)" = "$m" ] && ok defaults
kc="$h/Library/Keychains/kbf-fresh.keychain-db"
mkdir -p "$h/Library/Keychains" && security create-keychain -p "" "$kc" &&
  security add-generic-password -a kbf-ci -s kbf-ci-fresh -w "$m" "$kc" &&
  [ "$(security find-generic-password -s kbf-ci-fresh -w "$kc")" = "$m" ] && ok keychain
printf %s "$m" > "${t}kbf-fresh" && ok T
printf %s "$m" > "${c}kbf-fresh" && ok C
hdiutil create -quiet -size 1m -fs HFS+ -volname kbffresh "$m.dmg" &&
  hdiutil attach -quiet -nomount "$m.dmg" && ok image
sem=$(/usr/bin/perl -e 'my $id = semget(0, 1, 0600 | 01000); defined $id or die "semget: $!\n"; print $id') &&
  ipcs -s | awk -v id="$sem" '$1 == "s" && $2 == id' | grep -q . && ok sem "$sem"
: > planted
i=0
while [ ! -e go ]; do i=$((i + 1)); [ "$i" -lt 600 ] || exit 9; sleep 0.1; done
exit 0
"#;

/// Lease A's part outside the sandbox, which denies an action these writes and the
/// setuid tools: run by the test as the slot user (as a job that left the sandbox
/// would be), `$1` the marker.
const PLANT_OUTSIDE: &str = r#"m=$1
ok() { echo "planted $*"; }
printf %s "$m" > "/private/tmp/$m" && ok tmp
printf %s "$m" > "/private/var/tmp/$m" && ok var-tmp
printf %s "$m" > "/Users"/Shared/"$m" && ok shared
echo "0 0 1 1 * /usr/bin/true # $m" | crontab - && crontab -l | grep -q "$m" && ok crontab
echo /usr/bin/true | at now + 2 hours && [ -n "$(atq)" ] && ok at
exit 0
"#;

/// Lease B, in its sandbox, as the same slot user: `$1` the marker, `$2` lease A's
/// semaphore id, `$3` lease A's directory. Prints `found <row> ...` for each marker it
/// finds, then `checked`.
const CHECK: &str = r#"m=$1
uid=$(id -u)
eval "h=~$(id -un)"
t=$(getconf DARWIN_USER_TEMP_DIR)
c=$(getconf DARWIN_USER_CACHE_DIR)
found() { echo "found $*"; }
kc="$h/Library/Keychains/kbf-fresh.keychain-db"
[ -e "$3/kbf-fresh" ] && found lease-dir
[ -e "$h/kbf-fresh" ] && found home
v=$(defaults read kbf.ci.fresh v 2>/dev/null) && found defaults "$v"
v=$(security find-generic-password -s kbf-ci-fresh -w "$kc" 2>/dev/null) && found keychain "$v"
[ -e "$kc" ] && found keychain file
[ -e "${t}kbf-fresh" ] && found T
[ -e "${c}kbf-fresh" ] && found C
[ -e "/private/tmp/$m" ] && found tmp
[ -e "/private/var/tmp/$m" ] && found var-tmp
[ -e "/Users"/Shared/"$m" ] && found shared
launchctl print "user/$uid/kbf.ci.fresh" >/dev/null 2>&1 && found agent in user/$uid
[ -e "$h/Library/LaunchAgents/kbf.ci.fresh.plist" ] && found agent plist
launchctl print "user/$uid/com.apple.kbf-ci-fresh" >/dev/null 2>&1 && found apple-agent in user/$uid
hdiutil info 2>/dev/null | grep -q "$m" && found image
[ -n "$2" ] && ipcs -s | awk -v id="$2" '$1 == "s" && $2 == id' | grep -q . && found sem
echo checked
"#;

/// What the slot user, outside the sandbox, finds of lease A's schedules: `$1` the
/// marker.
const CHECK_OUTSIDE: &str = r#"m=$1
crontab -l 2>/dev/null | grep -q "$m" && echo "found crontab"
q=$(atq 2>/dev/null) && [ -n "$q" ] && echo "found at $q"
exit 0
"#;

/// A KeepAlive LaunchAgent labelled `label` that runs `sleep <secs>`, for the Background
/// session a pool user's domain is (an agent without the key is for Aqua, and its
/// bootstrap fails with EIO there).
fn agent(label: &str, secs: u32) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key><array><string>/bin/sleep</string><string>{secs}</string></array>
  <key>KeepAlive</key><true/>
  <key>RunAtLoad</key><true/>
  <key>LimitLoadToSessionType</key><string>Background</string>
</dict>
</plist>
"#
    )
}

/// The agents [`plant_outside`] bootstraps: their row, label and sleep. The second is
/// labelled like launchd's own agents, which the clean leaves loaded: its plist (the
/// lease's, in the home) says it is not.
const AGENTS: [(&str, &str, u32); 2] = [
    ("agent", "kbf.ci.fresh", 3003),
    ("apple-agent", "com.apple.kbf-ci-fresh", 3006),
];

/// The rows `text` names after `word` (`planted` or `found`), and what each line said.
fn rows(text: &str, word: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| line.strip_prefix(word)?.strip_prefix(' '))
        .map(|rest| {
            let (row, what) = rest.split_once(' ').unwrap_or((rest, ""));
            (row.to_owned(), what.to_owned())
        })
        .collect()
}

/// `program` with `args` as `user`, outside any sandbox, with a plain environment.
fn as_user(user: &SlotUser, program: &str, args: &[&str]) -> std::process::Output {
    Command::new(program)
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
        .expect(program)
}

/// `program` with `args` as root; its stdout and stderr, and whether it exited 0.
fn root(program: &str, args: &[&str]) -> (bool, String) {
    let out = Command::new(program).args(args).output().expect(program);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// Background Task Management's entries for `uid`, from `sfltool dumpbtm`.
fn btm(uid: u32) -> Vec<String> {
    let (_, text) = root("/usr/bin/sfltool", &["dumpbtm"]);
    let want = format!("uid={uid} ");
    kbf_mac_session::scan::parse_dumpbtm(&text)
        .unwrap_or_else(|why| vec![format!("unreadable: {why}")])
        .into_iter()
        .filter(|item| item.contains(&want))
        .collect()
}

/// What the Mac says of `user` where a marker could hide, for a failure message.
fn diagnosis(user: &SlotUser) -> String {
    let domain = format!("user/{}", user.uid);
    let mut out = String::new();
    for (what, program, args) in [
        ("hdiutil info", "/usr/bin/hdiutil", vec!["info", "-plist"]),
        ("ipcs", "/usr/bin/ipcs", vec!["-a"]),
        (
            "launchctl print",
            "/bin/launchctl",
            vec!["print", domain.as_str()],
        ),
    ] {
        let (_, text) = root(program, &args);
        let head: Vec<&str> = text.lines().take(60).collect();
        out.push_str(&format!("--- {what}\n{}\n", head.join("\n")));
    }
    out.push_str(&format!(
        "--- btm of uid {}\n{:?}\n",
        user.uid,
        btm(user.uid)
    ));
    let home = home_of(user);
    let (_, listing) = root("/bin/ls", &["-laR", &home.to_string_lossy()]);
    let head: Vec<&str> = listing.lines().take(60).collect();
    out.push_str(&format!("--- {}\n{}\n", home.display(), head.join("\n")));
    out
}

/// The slot count the runtime reports.
fn slots_reported(rt: &NativeRuntime<MemoryCas>) -> Option<String> {
    rt.capabilities()
        .into_iter()
        .find(|(key, _)| key == kbf_driver_native::slots::CAPABILITY)
        .map(|(_, value)| value)
}

/// What one run of the freshness scenario ([`freshness`]) saw.
struct Fresh {
    /// The rows of [`ROWS`] it did not (so those rows prove nothing).
    not_planted: Vec<String>,
    /// Each marker found after the clean, by row, and what the finder said.
    found: Vec<(String, String)>,
    /// The rows of `found`.
    red: BTreeSet<String>,
    /// Lease A's exit code, and what it printed to stdout and stderr.
    a_exit: i32,
    a_text: String,
    /// Lease B's exit code, its stdout and its stderr.
    b_exit: i32,
    b_out: String,
    b_err: String,
    /// What the part of lease A outside the sandbox said.
    outside: String,
    diagnosis: String,
}

impl Fresh {
    /// The scenario ran: both leases exited 0, lease A planted every row, and lease B
    /// reached its end. Without it a green or red row means nothing.
    fn assert_ran(&self) {
        assert_eq!(self.a_exit, 0, "lease A: {}\n{}", self.a_text, self.outside);
        assert!(
            self.not_planted.is_empty(),
            "lease A did not plant {:?} (so those rows prove nothing):\n{}\n{}\n{}",
            self.not_planted,
            self.a_text,
            self.outside,
            self.diagnosis
        );
        assert_eq!(self.b_exit, 0, "lease B: {}{}", self.b_out, self.b_err);
        assert!(
            self.b_out.ends_with("checked\n"),
            "lease B: {}{}",
            self.b_out,
            self.b_err
        );
    }
}

/// The freshness scenario. With one slot, lease A leaves a marker in each place of
/// [`ROWS`]: from inside its sandbox, its own directory, the real home, a preference,
/// a keychain, `T/` and `C/` of its `/var/folders` entry, a setsid sleeper, an attached
/// disk image and a System V semaphore; and as the slot user outside the sandbox
/// (which denies an action these writes and the setuid tools; a job that left it is
/// what the clean must also catch), files in `/private/tmp`, `/private/var/tmp` and the
/// `Shared` folder under `/Users`, a crontab line, an `at` job, a setuid-root sleep and
/// two KeepAlive LaunchAgents bootstrapped into `user/<uid>` from the home, one of them
/// labelled `com.apple.kbf-ci-fresh` like launchd's own agents. After the
/// clean that follows lease A, lease B, on the same user, looks for each
/// through the tool that would use it, the slot user runs `crontab -l` and `atq`, and
/// root reads the process table and `hdiutil info`. Background Task Management entries
/// of the uid, if the agent made any, are among the findings too.
///
/// `skipped` leaves one step out of the clean of the slot ([`Skipped`]); `None` is the
/// clean as the daemon runs it. `name` names the scratch directory and the user.
async fn freshness(name: &str, skipped: Option<Skipped>) -> Fresh {
    assert_root();
    let scratch = Scratch::new(name);
    let users = Users::new(name, 1);
    let user = users.0[0].clone();
    let cas = Arc::new(MemoryCas::default());
    let config = config(&scratch, users.pool());
    let rt = match skipped {
        None => NativeRuntime::new(config, Arc::clone(&cas)),
        Some(skipped) => NativeRuntime::with_skipped_step(config, Arc::clone(&cas), skipped),
    };
    let rt = rt.expect("runtime");
    // The test's name is in the marker: every run in this process has the same pid, and
    // what a run without a step leaves on the runner (an attached image, files in the shared
    // places) would be found by the runs after it if their markers were the same.
    let mark = format!("kbf-fresh-{name}-{}", std::process::id());
    // Declared after `users`, so it is dropped first: the user is still there to remove
    // its crontab and `at` jobs.
    let mut residue = Residue::new(&user, &mark);
    let setuid = scratch.setuid_sleep();
    let act = scratch.act();
    let a = Spec::argv(&[
        &act,
        "spawn-child",
        "setsid",
        ";;",
        "run",
        "/bin/sh",
        "-c",
        "/bin/sleep 3004 </dev/null >/dev/null 2>&1 & echo $! > sleeper",
        ";",
        "wait-for",
        "sleeper",
        "10",
        ";",
        "run",
        "/bin/sh",
        "-c",
        PLANT_INSIDE,
        "sh",
        &mark,
    ]);
    let a_root = scratch.lease_root(1);
    let outside = {
        let (user, mark, a_root) = (user.clone(), mark.clone(), a_root.clone());
        let drop_to = scratch.as_user_binary();
        async move {
            let give_up = Instant::now() + PROMPT * 3;
            while !a_root.join("planted").exists() {
                if Instant::now() > give_up {
                    return Outside::default();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let planted = tokio::task::spawn_blocking(move || {
                plant_outside(&user, &mark, &setuid, &drop_to, &a_root)
            });
            let planted = planted.await.expect("planted");
            let _ = std::fs::write(scratch_go(&planted.a_root), b"");
            planted
        }
    };
    let (first, mut outside) = tokio::join!(support::run(&rt, &cas, 1, &a), outside);
    let first = first.expect("lease A ran");
    let a_out = stdout(&cas, &first);
    let a_err = support::stderr(&cas, &first);
    // Between the leases: the clean ran before lease A's run returned.
    let mut found: Vec<(String, String)> = Vec::new();
    let left = kbf_driver_native::slots::macos::host::leftover_processes(user.uid).expect("list");
    if !left.is_empty() {
        found.push(("processes".to_owned(), format!("{left:?}")));
    }
    for (row, pid) in [("sleeper", outside.sleeper), ("setuid", outside.setuid_pid)] {
        if pid.is_some_and(support::alive) {
            found.push((row.to_owned(), format!("pid {pid:?}")));
        }
    }
    let attached = images(user.uid, &mark);
    if !attached.is_empty() {
        found.push(("image".to_owned(), format!("{attached:?}")));
    }
    let btm_after = btm(user.uid);
    if !btm_after.is_empty() {
        found.push(("btm".to_owned(), format!("{btm_after:?}")));
    }
    let schedules = as_user(&user, "/bin/sh", &["-c", CHECK_OUTSIDE, "sh", &mark]);
    found.extend(rows(&String::from_utf8_lossy(&schedules.stdout), "found"));
    let sem = rows(&a_out, "planted")
        .into_iter()
        .find(|(row, _)| row == "sem")
        .map(|(_, id)| id)
        .unwrap_or_default();
    residue.sem.clone_from(&sem);
    let a_dir = a_root.to_string_lossy().into_owned();
    let b = Spec::argv(&["/bin/sh", "-c", CHECK, "sh", &mark, &sem, &a_dir]);
    let second = support::run(&rt, &cas, 2, &b).await.expect("lease B ran");
    let b_out = stdout(&cas, &second);
    let b_err = support::stderr(&cas, &second);
    found.extend(rows(&b_out, "found"));
    if let Some(child) = outside.setuid.as_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let mut planted: BTreeSet<String> = rows(&a_out, "planted")
        .into_iter()
        .map(|(r, _)| r)
        .collect();
    planted.extend(rows(&outside.text, "planted").into_iter().map(|(r, _)| r));
    planted.insert("processes".to_owned());
    let not_planted: Vec<&str> = ROWS
        .iter()
        .map(|(row, _)| *row)
        .filter(|row| !planted.contains(*row))
        .collect();
    let red: BTreeSet<String> = found.iter().map(|(row, _)| row.clone()).collect();
    let diagnosis = diagnosis(&user);
    eprintln!(
        "freshness {name}: planted {planted:?}; red rows {red:?}; found {found:?}\n{}",
        outside.text
    );
    Fresh {
        not_planted: not_planted.into_iter().map(str::to_owned).collect(),
        found,
        red,
        a_exit: first.exit_code,
        a_text: format!("{a_out}{a_err}"),
        b_exit: second.exit_code,
        b_out,
        b_err,
        outside: outside.text,
        diagnosis,
    }
}

/// The freshness test. Lease A leaves a marker in each place of [`ROWS`], and after the
/// clean that follows it lease B, on the same user, finds none ([`freshness`]).
///
/// The test of the clean as the daemon runs it; the tests of necessity run the same
/// scenario with one step of the clean left out and show which rows lease B then finds.
/// This one fails if a row was not planted, so a row cannot pass by never having been left.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn a_lease_finds_nothing_the_lease_before_it_left_on_its_slot() {
    let fresh = freshness("fresh", None).await;
    fresh.assert_ran();
    assert!(
        fresh.found.is_empty(),
        "what lease A left was found, rows {:?}: {:?}\n{}",
        fresh.red,
        fresh.found,
        fresh.diagnosis
    );
}

/// A lease, as lease `1.seq`, whose action says `started` in its directory and then waits
/// for `go`; `plant` runs in between and its result is returned with the lease's.
///
/// Whatever is in a free slot when a lease is handed it is refused at slot-ready (the
/// assertion, `tests/slot_assert.rs`), so a state that the clean after the lease is to
/// remove, or to fail on, is made while the lease runs, as a lease would make it.
async fn lease_planting<T>(
    rt: &NativeRuntime<MemoryCas>,
    cas: &Arc<MemoryCas>,
    scratch: &Scratch,
    seq: u64,
    plant: impl FnOnce() -> T,
) -> (kbf_proto::reapi::ActionResult, T) {
    let act = scratch.act();
    let spec = Spec::argv(&[&act, "write-lines", "started", ";", "wait-for", "go", "60"]);
    let root = scratch.lease_root(seq);
    let planting = async {
        let give_up = Instant::now() + PROMPT * 3;
        while !root.join("started").exists() {
            assert!(Instant::now() < give_up, "the lease never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let planted = plant();
        std::fs::write(root.join("go"), b"").expect("go");
        planted
    };
    let (ran, planted) = tokio::join!(support::run(rt, cas, seq, &spec), planting);
    (ran.expect("the lease ran"), planted)
}

#[path = "slot_clean/keychain.rs"]
mod keychain;
#[path = "slot_clean/single_pass.rs"]
mod single_pass;
#[path = "slot_clean/steps_needed.rs"]
mod steps_needed;

/// `go` in lease A's directory: it ends lease A's wait.
fn scratch_go(a_root: &Path) -> PathBuf {
    a_root.join("go")
}

/// What [`plant_outside`] planted.
#[derive(Default)]
struct Outside {
    /// Its `planted <row>` lines, and what failed.
    text: String,
    /// Lease A's directory.
    a_root: PathBuf,
    /// Lease A's setsid sleeper, from its `sleeper` file, if it is the uid's.
    sleeper: Option<i32>,
    /// The setuid-root sleep, the slot user's by its real uid.
    setuid: Option<std::process::Child>,
    setuid_pid: Option<i32>,
}

/// The part of lease A made as the slot user outside the sandbox while lease A waits:
/// [`PLANT_OUTSIDE`], a setuid-root sleep (`setuid`), and the LaunchAgents
/// ([`AGENTS`]), which the user bootstraps into `user/<uid>` from its home through
/// `launchctl asuser` and the `as-user` drop of `drop_to` (this test binary). Also looks up lease A's setsid
/// sleeper, and notes what `hdiutil info` lists of the uid's while lease A runs.
fn plant_outside(
    user: &SlotUser,
    mark: &str,
    setuid: &Path,
    drop_to: &str,
    a_root: &Path,
) -> Outside {
    let out = as_user(user, "/bin/sh", &["-c", PLANT_OUTSIDE, "sh", mark]);
    let mut text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let listed = uid::processes(user.uid).unwrap_or_default();
    let sleeper = std::fs::read_to_string(a_root.join("sleeper"))
        .ok()
        .and_then(|pid| pid.trim().parse::<i32>().ok())
        .filter(|pid| listed.contains(pid));
    if sleeper.is_some() {
        text.push_str("planted sleeper\n");
    }
    let child = Command::new(setuid)
        .arg("3005")
        .stdin(Stdio::null())
        .uid(user.uid)
        .gid(user.gid)
        .spawn()
        .expect("the setuid sleep");
    let setuid_pid = i32::try_from(child.id()).expect("pid");
    let give_up = Instant::now() + PROMPT;
    while ids_of(setuid_pid) != Some((0, user.uid)) && Instant::now() < give_up {
        std::thread::sleep(Duration::from_millis(20));
    }
    if ids_of(setuid_pid) == Some((0, user.uid)) {
        text.push_str("planted setuid\n");
    }
    let agents = home_of(user).join("Library").join("LaunchAgents");
    let made = as_user(user, "/bin/mkdir", &["-p", &agents.to_string_lossy()]);
    if made.status.success() {
        for (row, label, secs) in AGENTS {
            bootstrap(user, drop_to, &agents, row, label, secs, &mut text);
        }
    } else {
        text.push_str("mkdir LaunchAgents failed\n");
    }
    text.push_str(&format!("btm after the agent: {:?}\n", btm(user.uid)));
    text.push_str(&format!(
        "images during lease A: {:?}\n",
        images(user.uid, mark)
    ));
    Outside {
        text,
        a_root: a_root.to_owned(),
        sleeper,
        setuid: Some(child),
        setuid_pid: Some(setuid_pid),
    }
}

/// Writes the agent `label` (row `row`, `sleep <secs>`) into the user's `agents`
/// folder and bootstraps it into `user/<uid>` as the user, noting in `text` what
/// launchctl said and `planted <row>` once `launchctl print` lists it.
fn bootstrap(
    user: &SlotUser,
    drop_to: &str,
    agents: &Path,
    row: &str,
    label: &str,
    secs: u32,
    text: &mut String,
) {
    let plist = agents.join(format!("{label}.plist"));
    std::fs::write(&plist, agent(label, secs)).expect("agent plist");
    std::os::unix::fs::chown(&plist, Some(user.uid), Some(user.gid)).expect("chown");
    std::fs::set_permissions(&plist, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let domain = format!("user/{}", user.uid);
    let path = plist.to_string_lossy().into_owned();
    let (uid, gid) = (user.uid.to_string(), user.gid.to_string());
    let service = format!("{domain}/{label}");
    let tries: [&[&str]; 2] = [
        &[
            "asuser",
            &uid,
            drop_to,
            kbf_driver_native::slots::AS_USER_ARG,
            &uid,
            &gid,
            "--",
            "/bin/launchctl",
            "bootstrap",
            &domain,
            &path,
        ],
        &[
            "asuser",
            &uid,
            "/bin/launchctl",
            "bootstrap",
            &domain,
            &path,
        ],
    ];
    for args in tries {
        let (booted, said) = root("/bin/launchctl", args);
        text.push_str(&format!("launchctl {}: {booted} {said}\n", args.join(" ")));
        if root("/bin/launchctl", &["print", &service]).0 {
            text.push_str(&format!("planted {row}\n"));
            break;
        }
    }
}

/// The images `hdiutil info -plist` lists as `uid`'s or by a path naming `mark`, each
/// as its dictionary; or what was wrong with the listing.
fn listed_images(uid: u32, mark: &str) -> Result<Vec<plist::Dictionary>, String> {
    let out = Command::new("/usr/bin/hdiutil")
        .args(["info", "-plist"])
        .output()
        .expect("hdiutil");
    let Ok(info) = plist::Value::from_reader_xml(out.stdout.as_slice()) else {
        return Err(format!(
            "unreadable: {}",
            String::from_utf8_lossy(&out.stdout)
        ));
    };
    let images = info
        .as_dictionary()
        .and_then(|d| d.get("images"))
        .and_then(plist::Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(images
        .iter()
        .filter_map(plist::Value::as_dictionary)
        .filter(|image| {
            let path = image.get("image-path").and_then(plist::Value::as_string);
            let owner = image
                .get("owner-uid")
                .and_then(plist::Value::as_unsigned_integer);
            owner == Some(u64::from(uid)) || path.is_some_and(|path| path.contains(mark))
        })
        .cloned()
        .collect())
}

/// The paths of the images [`listed_images`] finds.
fn images(uid: u32, mark: &str) -> Vec<String> {
    match listed_images(uid, mark) {
        Err(why) => vec![why],
        Ok(images) => images
            .iter()
            .filter_map(|image| image.get("image-path").and_then(plist::Value::as_string))
            .map(str::to_owned)
            .collect(),
    }
}

/// What a run of [`freshness`] can leave on the runner when a step of the clean is
/// left out, removed when it is dropped (so also when the test panics): the clean that
/// should have removed it did not run, and the next test, or the next run on the same
/// runner, must start clean. The user's own state (its home, preferences, keychain, and
/// its processes and jobs) goes with the user ([`Users`]); what is here is what
/// outlives it.
struct Residue {
    user: SlotUser,
    /// Lease A's marker.
    mark: String,
    /// Lease A's semaphore id, once known; empty before.
    sem: String,
}

impl Residue {
    fn new(user: &SlotUser, mark: &str) -> Self {
        Self {
            user: user.clone(),
            mark: mark.to_owned(),
            sem: String::new(),
        }
    }
}

impl Drop for Residue {
    fn drop(&mut self) {
        // The attached image: found by its path (it names the marker) or its owner, and
        // detached by its device.
        for image in listed_images(self.user.uid, &self.mark).unwrap_or_default() {
            let devices = image
                .get("system-entities")
                .and_then(plist::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(plist::Value::as_dictionary)
                .filter_map(|entity| entity.get("dev-entry").and_then(plist::Value::as_string));
            for dev in devices {
                let (_, said) = root("/usr/bin/hdiutil", &["detach", "-force", dev]);
                eprintln!("cleanup: hdiutil detach {dev}: {said}");
            }
        }
        if !self.sem.is_empty() {
            let (removed, said) = root("/usr/bin/ipcrm", &["-s", &self.sem]);
            eprintln!("cleanup: ipcrm -s {}: {removed} {said}", self.sem);
        }
        // The user's crontab and `at` jobs live outside its home, and outlive the user.
        let removed = as_user(&self.user, "/usr/bin/crontab", &["-r"]);
        eprintln!("cleanup: crontab -r: {}", removed.status);
        let queue = as_user(&self.user, "/usr/bin/atq", &[]);
        for job in String::from_utf8_lossy(&queue.stdout)
            .lines()
            .filter_map(|line| line.split_whitespace().next())
        {
            let removed = as_user(&self.user, "/usr/bin/atrm", &[job]);
            eprintln!("cleanup: atrm {job}: {}", removed.status);
        }
        // The marker files of the shared places, and of this uid's `T/` and `C/`, which a
        // user made later at the same uid would find.
        let mut files = vec![
            PathBuf::from("/private/tmp").join(&self.mark),
            PathBuf::from("/private/var/tmp").join(&self.mark),
            PathBuf::from("/Users").join("Shared").join(&self.mark),
        ];
        for key in ["DARWIN_USER_TEMP_DIR", "DARWIN_USER_CACHE_DIR"] {
            let dir = as_user(&self.user, "/usr/bin/getconf", &[key]);
            let dir = String::from_utf8_lossy(&dir.stdout).trim().to_owned();
            if !dir.is_empty() {
                files.push(Path::new(&dir).join("kbf-fresh"));
            }
        }
        for file in files {
            let _ = std::fs::remove_file(file);
        }
    }
}

/// A process with the real uid 0 and the effective uid of a slot: listed among the
/// uid's processes, and out of reach of a kill from the uid (macOS lets a process
/// signal one whose real or saved uid is the sender's, and this one's are root's).
/// Killed and reaped when dropped.
struct Unkillable(libc::pid_t);

impl Unkillable {
    fn new(uid: u32) -> Self {
        // SAFETY: the child calls only async-signal-safe functions (seteuid, pause,
        // _exit), as a fork of a threaded process must.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork: {}", std::io::Error::last_os_error());
        if pid == 0 {
            // SAFETY: async-signal-safe calls only; neither returns to Rust code.
            unsafe {
                if libc::seteuid(uid) != 0 {
                    libc::_exit(1);
                }
                loop {
                    libc::pause();
                }
            }
        }
        Self(pid)
    }
}

impl Unkillable {
    /// Moves this process to a thread that kills and reaps it once told to (through the
    /// sender), or after `within`, whichever comes first.
    fn guard(self, within: Duration) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let (release, told) = std::sync::mpsc::channel::<()>();
        let guard = std::thread::spawn(move || {
            let _ = told.recv_timeout(within);
            drop(self);
        });
        (release, guard)
    }
}

impl Drop for Unkillable {
    fn drop(&mut self) {
        // SAFETY: plain integers; `self.0` is this process's child, reaped here once.
        unsafe {
            libc::kill(self.0, libc::SIGKILL);
            libc::waitpid(self.0, std::ptr::null_mut(), 0);
        }
    }
}

/// The deadline. Catches a clean whose kill loops with no deadline (the lease never
/// ends: a process stuck in the kernel would hang the slot), and a slot whose
/// processes survived every kill given back to the pool (the next lease would run
/// beside them): with a process of the uid that the uid's kill cannot reach, the lease
/// ends within its deadlines, failed, and the node reports no slot.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[tokio::test]
async fn a_process_the_kill_cannot_end_fails_the_clean_within_its_deadline() {
    assert_root();
    let scratch = Scratch::new("deadline");
    let users = Users::new("deadline", 1);
    let user = &users.0[0];
    let cas = Arc::new(MemoryCas::default());
    let mut config = config(&scratch, users.pool());
    config.kill_wait = Duration::from_secs(2);
    let rt = NativeRuntime::new(config, Arc::clone(&cas)).expect("runtime");
    // The start-up clean can fail on a slow host (the lease would then wait for a slot
    // forever, as the stuck process below keeps every later clean from passing): the
    // test starts from a slot that is offered.
    let offered = Instant::now() + PROMPT;
    while slots_reported(&rt).as_deref() != Some("1") && Instant::now() < offered {
        std::thread::sleep(Duration::from_millis(20));
    }
    let before = slots_reported(&rt);
    assert_eq!(
        before.as_deref(),
        Some("1"),
        "the slot was dirty at start-up"
    );
    // After the slot-ready check, which would refuse the slot on it: planted while the
    // lease runs, as a process the action left behind would be.
    let act = scratch.act();
    let waiting = Spec::argv(&[&act, "write-lines", "started", ";", "wait-for", "go", "60"]);
    let action = waiting.store(&cas);
    let root = scratch.lease_root(1);
    let started = Instant::now();
    let lease = tokio::time::timeout(Duration::from_secs(90), rt.run(support::work(1, action, 0)));
    let planting = async {
        let give_up = Instant::now() + PROMPT * 3;
        while !root.join("started").exists() {
            assert!(Instant::now() < give_up, "the lease never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let stuck = Unkillable::new(user.uid);
        let mut ids = None;
        let give_up = Instant::now() + PROMPT;
        while ids != Some((user.uid, 0)) && Instant::now() < give_up {
            std::thread::sleep(Duration::from_millis(20));
            ids = ids_of(stuck.0);
        }
        let listed = uid::processes(user.uid).expect("list").contains(&stuck.0);
        // Killed by its guard after 120 s at the latest: a kill with no deadline, which
        // waits for it forever (on the runtime's thread too, as the dropped run ends its
        // uid), then ends, and the test fails instead of hanging the job.
        let (release, guard) = stuck.guard(Duration::from_secs(120));
        std::fs::write(root.join("go"), b"").expect("go");
        (ids, listed, release, guard)
    };
    let (ended, (ids, listed, release, guard)) = tokio::join!(lease, planting);
    let took = started.elapsed();
    let after = slots_reported(&rt);
    let _ = release.send(());
    guard.join().expect("the guard");
    assert_eq!(
        ids,
        Some((user.uid, 0)),
        "the stuck process is not root's by its real uid and the slot's by its effective one"
    );
    assert!(
        listed,
        "the stuck process is not among uid {}'s processes",
        user.uid
    );
    let ended = ended.expect("the lease never ended: the kill has no deadline");
    let why = ended.expect_err("a lease whose processes survived succeeded");
    assert!(why.to_string().contains("survived"), "{why}");
    assert!(took < Duration::from_secs(60), "the lease took {took:?}");
    assert_eq!(before.as_deref(), Some("1"));
    assert_eq!(
        after.as_deref(),
        Some("0"),
        "a slot whose processes survived is still reported"
    );
}

/// Launchd's own agents are told by their plist, not their label. Catches a check that
/// leaves a process out by its job's label, which the lease chooses (a KeepAlive agent
/// labelled `com.apple.*` that a bootout left loaded would run beside every later lease,
/// and the check would pass): the running agent `com.apple.kbf-ci-label`, from a plist
/// in the home, is among the uid's leftover processes.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[test]
fn a_job_labelled_like_launchds_own_is_still_the_leases() {
    assert_root();
    let scratch = Scratch::new("label");
    let users = Users::new("label", 1);
    let user = &users.0[0];
    let label = "com.apple.kbf-ci-label";
    let agents = home_of(user).join("Library").join("LaunchAgents");
    let made = as_user(user, "/bin/mkdir", &["-p", &agents.to_string_lossy()]);
    assert!(made.status.success(), "mkdir: {made:?}");
    let mut text = String::new();
    let drop_to = scratch.as_user_binary();
    bootstrap(user, &drop_to, &agents, "label", label, 3007, &mut text);
    let service = format!("user/{}/{label}", user.uid);
    let mut pid = None;
    let give_up = Instant::now() + PROMPT;
    while pid.is_none() && Instant::now() < give_up {
        let (_, said) = root("/bin/launchctl", &["print", &service]);
        pid = said
            .lines()
            .find_map(|line| line.strip_prefix("\tpid = "))
            .and_then(|pid| pid.trim().parse::<i32>().ok());
        if pid.is_none() {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let left = kbf_driver_native::slots::macos::host::leftover_processes(user.uid).expect("list");
    let (_, booted) = root("/bin/launchctl", &["bootout", &service]);
    assert!(text.contains("planted label"), "{text}");
    let pid = pid.unwrap_or_else(|| panic!("the agent never ran: {text}"));
    assert!(
        left.contains(&pid),
        "the lease's agent {label} (pid {pid}) is left out of uid {}'s processes {left:?} \
         (bootout: {booted})",
        user.uid
    );
}

/// The entries of `dir`, by name.
fn entries(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir).map_or_else(
        |_| vec!["<unreadable>".to_owned()],
        |list| {
            list.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        },
    )
}

/// The start. Catches a start that ends a killed daemon's processes but leaves its
/// leases' state (the next lease would find it): what a lease left as the slot user (a
/// file in the home, in `T/` and in `/private/tmp`, and a crontab line) is gone once the
/// runtime has started, and the slot is reported.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slot_clean"]
#[test]
fn the_start_cleans_what_a_killed_daemon_left_in_every_slot() {
    assert_root();
    let scratch = Scratch::new("start");
    let users = Users::new("start", 1);
    let user = &users.0[0];
    let mark = format!("kbf-left-{}", std::process::id());
    let plant = r#"m=$1
printf %s "$m" > "$HOME/kbf-left" &&
printf %s "$m" > "$(getconf DARWIN_USER_TEMP_DIR)kbf-left" &&
printf %s "$m" > "/private/tmp/$m" &&
echo "0 0 1 1 * /usr/bin/true # $m" | crontab -
"#;
    let check = r#"m=$1
[ -e "$HOME/kbf-left" ] && echo home
[ -e "$(getconf DARWIN_USER_TEMP_DIR)kbf-left" ] && echo T
[ -e "/private/tmp/$m" ] && echo tmp
crontab -l 2>/dev/null | grep -q "$m" && echo crontab
exit 0
"#;
    let planted = as_user(user, "/bin/sh", &["-c", plant, "sh", &mark]);
    let rt = NativeRuntime::new(
        config(&scratch, users.pool()),
        Arc::new(MemoryCas::default()),
    )
    .expect("runtime");
    let reported = slots_reported(&rt);
    drop(rt);
    let left = as_user(user, "/bin/sh", &["-c", check, "sh", &mark]);
    let _ = std::fs::remove_file(format!("/private/tmp/{mark}"));
    assert!(
        planted.status.success(),
        "planting failed: {}",
        String::from_utf8_lossy(&planted.stderr)
    );
    let left = String::from_utf8_lossy(&left.stdout).into_owned();
    assert_eq!(
        left, "",
        "the start left what the killed daemon's lease left"
    );
    assert_eq!(reported.as_deref(), Some("1"));
}
