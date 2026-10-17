//! The lease-user pool for real (docs/design/macos-end-state.md, step 3): a root
//! daemon's actions run as throwaway pool users this file creates and deletes, through
//! `launchctl asuser` and the `as-user` drop; every process of the slot's uid ends with
//! the lease, and with the daemon's death through the keeper; two leases at once never
//! share a user; and a root daemon without a pool runs nothing.
//!
//! Every test is `#[ignore]`: each needs root and real users. The macOS CI job runs them
//! with `cargo xtask sudo-test -p kbf-driver-native --test slots`, which builds this
//! file as the runner's user and runs the binary through `sudo -n` with `--ignored`. They make users at free uids from
//! 690 (`kbf-ci-slot-<test>-<i>`, hidden, shell `/usr/bin/false`, homes in the folder
//! tests/support/pool.rs names) and delete them, their homes and every process of their
//! uids when they end, passed or not.

#![cfg(target_os = "macos")]

mod support;

use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kbf_daemon::Runtime as _;
use kbf_daemon::uid;
use kbf_driver_native::slots::{Pool, SlotUser};
use kbf_driver_native::{NativeConfig, NativeRuntime};
use support::pool::{GID, Scratch, Users, config, ids_of, sleeper, slot_report};
use support::{MemoryCas, PROMPT, Spec, stdout};

/// The variable that turns [`keeper_daemon`] into the daemon [`the_keeper_ends_the_slot_when_the_daemon_dies`]
/// kills: the scratch directory, the user's name and uid.
const KEEPER_CHILD: &str = "KBF_SLOTS_KEEPER_CHILD";

fn assert_root() {
    support::pool::assert_root("slots");
}

/// Runs `spec` as lease `1.seq`.
async fn lease(
    rt: &NativeRuntime<MemoryCas>,
    cas: &MemoryCas,
    seq: u64,
    spec: &Spec,
) -> kbf_proto::reapi::ActionResult {
    support::run(rt, cas, seq, spec).await.expect("ran")
}

/// Whether `pid` is alive and not a zombie.
fn alive(pid: i32) -> bool {
    support::alive(pid)
}

/// The act steps that leave a `sleep 300` behind in a session of its own whose parent
/// has exited (a double fork after `setsid`), then print its pid.
fn leave_a_sleeper(act: &str) -> Vec<String> {
    [
        act,
        "spawn-child",
        "setsid",
        ";;",
        "run",
        "/bin/sh",
        "-c",
        "/bin/sleep 300 </dev/null >/dev/null 2>&1 & echo $! > sleeper",
        ";",
        "wait-for",
        "sleeper",
        "10",
        ";",
        "cat",
        "sleeper",
    ]
    .map(str::to_owned)
    .to_vec()
}

/// The exact kill. Catches a lease whose processes are killed by process group (the
/// sleeper left the group) or from root by a list of the pids the tracker followed
/// (the sleeper's parent exited before any poll saw it): no process of the slot's uid
/// may be left once the lease has ended. Also catches a kill that reaches past the
/// uid (a root `sleep` and another slot's `sleep` live on), an action that does not
/// run as its slot's user or loses its environment on the way, one started without
/// `launchctl asuser` (with a plain setuid it stays in the daemon's bootstrap
/// namespace, whose manager is not the slot's uid), and a slot that cannot be used
/// again after its kill (the second lease, on the same slot, runs and exits with its
/// own code).
///
/// A setuid-root process the slot user started (effective uid 0, real uid the
/// slot's) is listed among the uid's processes by its real uid, left out of the
/// memory watch's effective-uid list, and ended with the lease: catches the kill's
/// list without `PROC_RUID_ONLY` and the watch's list by real uid.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slots"]
#[tokio::test]
async fn an_action_ends_with_every_process_of_its_uid_and_nothing_else() {
    assert_root();
    let scratch = Scratch::new("kill");
    let users = Users::new("kill", 2);
    let (mine, other) = (&users.0[0], &users.0[1]);
    let cas = Arc::new(MemoryCas::default());
    let rt = NativeRuntime::new(config(&scratch, users.pool()), Arc::clone(&cas)).expect("runtime");
    // After the start, which ends every process of every slot.
    let mut root_sleep = sleeper(None);
    let mut other_sleep = sleeper(Some(other));
    let mut setuid = Command::new(scratch.setuid_sleep())
        .arg("300")
        .stdin(Stdio::null())
        .uid(mine.uid)
        .gid(mine.gid)
        .spawn()
        .expect("the setuid sleep");
    let setuid_pid = i32::try_from(setuid.id()).expect("pid");
    let setuid_ids = ids_of(setuid_pid);
    let listed = uid::processes(mine.uid).expect("list");
    let watched = uid::effective(mine.uid).expect("list");
    let mut steps = leave_a_sleeper(&scratch.act());
    steps.extend(
        [
            ";",
            "id",
            ";",
            "env",
            "TMPDIR",
            ";",
            "run",
            uid::LAUNCHCTL,
            "manageruid",
        ]
        .map(str::to_owned),
    );
    let argv: Vec<&str> = steps.iter().map(String::as_str).collect();
    let first = lease(&rt, &cas, 1, &Spec::argv(&argv)).await;
    let out = stdout(&cas, &first);
    let lines: Vec<&str> = out.lines().collect();
    // As the slot's clean counts them: launchd starts its own agents of the domain
    // (distnoted) again as the uid once the clean has looked at the domain.
    let left = kbf_driver_native::slots::macos::host::leftover_processes(mine.uid).expect("list");
    let root_lives = alive(i32::try_from(root_sleep.id()).expect("pid"));
    let other_lives = alive(i32::try_from(other_sleep.id()).expect("pid"));
    let setuid_lives = alive(setuid_pid);
    for child in [&mut root_sleep, &mut other_sleep, &mut setuid] {
        let _ = child.kill();
        let _ = child.wait();
    }
    assert_eq!(
        setuid_ids,
        Some((0, mine.uid)),
        "the setuid sleep {setuid_pid} is not root's by its effective uid and the slot's by its real one"
    );
    assert!(
        listed.contains(&setuid_pid),
        "the setuid sleep {setuid_pid} is not among uid {}'s processes: {listed:?}",
        mine.uid
    );
    assert!(
        !watched.contains(&setuid_pid),
        "the memory watch's list holds the setuid sleep {setuid_pid}: {watched:?}"
    );
    assert!(
        !setuid_lives,
        "the setuid sleep {setuid_pid} outlived its lease"
    );
    assert_eq!(
        first.exit_code,
        0,
        "{out}{}\n{}",
        support::stderr(&cas, &first),
        slot_report(std::slice::from_ref(mine))
    );
    let [sleeper, id, tmpdir, manager] = lines[..] else {
        panic!("the action printed {out:?}");
    };
    assert_eq!(id, mine.uid.to_string(), "the action ran as uid {id}");
    assert_eq!(
        manager.split_whitespace().last(),
        Some(mine.uid.to_string().as_str()),
        "the action's bootstrap namespace is not the slot's: {manager:?}"
    );
    let lease_tmp = scratch.leases().join("lease-1-1").join("tmp");
    assert_eq!(
        Path::new(tmpdir),
        lease_tmp,
        "the environment did not reach the action"
    );
    let sleeper: i32 = sleeper.parse().expect("the sleeper's pid");
    assert!(
        !alive(sleeper),
        "the setsid sleeper {sleeper} outlived its lease"
    );
    assert!(
        left.is_empty(),
        "processes of uid {} outlived the lease: {left:?}",
        mine.uid
    );
    assert!(root_lives, "the kill reached root's sleep");
    assert!(other_lives, "the kill reached another slot's sleep");
    let act = scratch.act();
    let second = lease(&rt, &cas, 2, &Spec::argv(&[&act, "id", ";", "exit", "7"])).await;
    assert_eq!(second.exit_code, 7, "{}", stdout(&cas, &second));
    assert_eq!(stdout(&cas, &second), format!("{}\n", mine.uid));
}

/// The keeper. Catches a keeper that ends only its own process group (the `group`
/// mode, which the sleeper left), and one that does not run once the daemon is dead:
/// with the daemon killed by SIGKILL mid-lease and not started again, no process of
/// the slot's uid is left within 5 s.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slots"]
#[test]
fn the_keeper_ends_the_slot_when_the_daemon_dies() {
    assert_root();
    let scratch = Scratch::new("keeper");
    let users = Users::new("keeper", 1);
    let user = &users.0[0];
    let mut daemon = Command::new(std::env::current_exe().expect("this binary"))
        .args(["--ignored", "--exact", "keeper_daemon", "--nocapture"])
        .env(
            KEEPER_CHILD,
            format!("{}:{}:{}", scratch.0.display(), user.name, user.uid),
        )
        .spawn()
        .expect("the daemon");
    let started = scratch
        .leases()
        .join("lease-1-1")
        .join("root")
        .join("sleeper");
    let give_up = Instant::now() + PROMPT * 3;
    let sleeper = loop {
        let text = std::fs::read_to_string(&started).unwrap_or_default();
        if let Ok(pid) = text.trim().parse::<i32>() {
            break pid;
        }
        if Instant::now() > give_up || daemon.try_wait().expect("wait").is_some() {
            let _ = daemon.kill();
            panic!(
                "the daemon's lease never started its sleeper\n{}",
                slot_report(std::slice::from_ref(user))
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let before = uid::processes(user.uid).expect("list");
    if !before.contains(&sleeper) {
        let _ = daemon.kill();
        panic!(
            "the sleeper {sleeper} is not the slot's: uid {} runs {before:?}",
            user.uid
        );
    }
    daemon.kill().expect("SIGKILL the daemon");
    daemon.wait().expect("reap the daemon");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut left = uid::processes(user.uid).expect("list");
    while !left.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        left = uid::processes(user.uid).expect("list");
    }
    assert!(
        left.is_empty(),
        "5 s after the daemon died, processes of uid {} remain: {left:?} (the sleeper is {sleeper})",
        user.uid
    );
}

/// The daemon [`the_keeper_ends_the_slot_when_the_daemon_dies`] kills: run by that test
/// with [`KEEPER_CHILD`] set, it starts one lease that leaves a sleeper behind and then
/// waits, until it is killed. Without the variable it does nothing.
#[ignore = "run by the_keeper_ends_the_slot_when_the_daemon_dies"]
#[tokio::test]
async fn keeper_daemon() {
    let Ok(child) = std::env::var(KEEPER_CHILD) else {
        return;
    };
    let mut parts = child.split(':');
    let (Some(dir), Some(name), Some(uid)) = (parts.next(), parts.next(), parts.next()) else {
        panic!("{KEEPER_CHILD}={child}");
    };
    let scratch = Scratch(PathBuf::from(dir));
    let user = SlotUser {
        name: name.to_owned(),
        uid: uid.parse().expect("uid"),
        gid: GID,
    };
    let cas = Arc::new(MemoryCas::default());
    let pool = Pool::new(vec![user]).expect("pool");
    let rt = NativeRuntime::new(config(&scratch, pool), Arc::clone(&cas)).expect("runtime");
    let mut steps = leave_a_sleeper(&scratch.act());
    steps.extend([";", "sleep", "300"].map(str::to_owned));
    let argv: Vec<&str> = steps.iter().map(String::as_str).collect();
    let action = Spec::argv(&argv).store(&cas);
    let _ = rt.run(support::work(1, action, 0)).await;
    // Never reached: the test kills this process mid-lease. Keep the scratch.
    std::mem::forget(scratch);
}

/// Parallel leases. Catches two leases handed the same slot: with two slots, two leases
/// at once each write the same names in their temporary folder (`T/`), their home, the
/// preferences (`defaults write`) and a keychain, wait until both have written, and
/// read them back: each sees only its own values.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slots"]
#[tokio::test]
async fn two_leases_at_once_never_share_a_user() {
    assert_root();
    let scratch = Scratch::new("parallel");
    let users = Users::new("parallel", 2);
    let cas = Arc::new(MemoryCas::default());
    let rt = NativeRuntime::new(config(&scratch, users.pool()), Arc::clone(&cas)).expect("runtime");
    let script = r#"set -e
v=$1
t=$(getconf DARWIN_USER_TEMP_DIR)
eval "h=~$(id -un)"
kc="$h/Library/Keychains/kbf-ci.keychain-db"
printf %s "$v" > "${t}kbf-ci-same"
printf %s "$v" > "$h/kbf-ci-same"
defaults write kbf.ci.same v "$v"
mkdir -p "$h/Library/Keychains"
security create-keychain -p "" "$kc"
security add-generic-password -a kbf-ci -s kbf-ci-same -w "$v" "$kc"
: > wrote
i=0
while [ ! -e go ]; do i=$((i + 1)); [ "$i" -lt 300 ] || exit 9; sleep 0.1; done
echo "T=$(cat "${t}kbf-ci-same") H=$(cat "$h/kbf-ci-same") D=$(defaults read kbf.ci.same v) K=$(security find-generic-password -a kbf-ci -s kbf-ci-same -w "$kc")"
"#;
    let spec = |v: &str| Spec::argv(&["/bin/sh", "-c", script, "sh", v]);
    let leases = scratch.leases();
    let barrier = async {
        let wrote = |seq: u64| {
            leases
                .join(format!("lease-1-{seq}"))
                .join("root")
                .join("wrote")
        };
        let give_up = Instant::now() + PROMPT * 3;
        while !(wrote(1).exists() && wrote(2).exists()) && Instant::now() < give_up {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        for seq in [1, 2] {
            let go = leases
                .join(format!("lease-1-{seq}"))
                .join("root")
                .join("go");
            let _ = std::fs::write(go, b"");
        }
    };
    let (first, second) = (spec("v1"), spec("v2"));
    let (one, two, ()) = tokio::join!(
        support::run(&rt, &cas, 1, &first),
        support::run(&rt, &cas, 2, &second),
        barrier
    );
    for (result, v) in [(one, "v1"), (two, "v2")] {
        let result = result.expect("ran");
        let out = stdout(&cas, &result);
        let err = support::stderr(&cas, &result);
        assert_eq!(
            result.exit_code,
            0,
            "lease {v}: {out}{err}\n{}",
            slot_report(&users.0)
        );
        assert_eq!(
            out,
            format!("T={v} H={v} D={v} K={v}\n"),
            "lease {v}: {err}"
        );
    }
}

/// Never root. Catches a root daemon that starts without a pool: it would run every
/// action as root.
#[ignore = "needs root: cargo xtask sudo-test -p kbf-driver-native --test slots"]
#[test]
fn a_root_daemon_without_a_pool_runs_nothing() {
    assert_root();
    let scratch = Scratch::new("root");
    let mut config = NativeConfig::new(scratch.leases());
    config.pool = None;
    let refused = NativeRuntime::new(config, Arc::new(MemoryCas::default()));
    let why = refused
        .expect_err("a root daemon without a pool started")
        .to_string();
    assert!(why.contains("no action as root"), "{why}");
    assert!(
        !scratch.leases().exists(),
        "it made its scratch root before refusing"
    );
}

/// The wait for a free slot. With one slot held by a lease that waits for a file:
/// a second lease waits for the slot and is killed while it waits (it ends `Killed`,
/// promptly, never having run), and a third waits until the first has ended, then
/// runs on the same slot. Catches a lease past the last slot that fails ("none free",
/// as with one permit too many) instead of waiting, a kill that does not reach a lease
/// waiting for a slot, and a slot a killed waiter keeps.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slots"]
#[tokio::test]
async fn with_every_slot_held_a_lease_waits_and_a_kill_ends_the_wait() {
    assert_root();
    let scratch = Scratch::new("wait");
    let users = Users::new("wait", 1);
    let user = &users.0[0];
    let cas = Arc::new(MemoryCas::default());
    let rt = NativeRuntime::new(config(&scratch, users.pool()), Arc::clone(&cas)).expect("runtime");
    let act = scratch.act();
    let hold = Spec::argv(&[
        &act, "append", "held", "x", ";", "wait-for", "go", "60", ";", "id",
    ]);
    let id = Spec::argv(&[&act, "id"]);
    let root = |seq: u64| scratch.leases().join(format!("lease-1-{seq}")).join("root");
    let rest = async {
        let give_up = Instant::now() + PROMPT * 3;
        while !root(1).join("held").exists() {
            assert!(
                Instant::now() < give_up,
                "the first lease never held its slot\n{}",
                slot_report(std::slice::from_ref(user))
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let kill = async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            rt.kill(kbf_types::LeaseId::new(1, 2)).await;
        };
        let waited = Instant::now();
        let (killed, ()) = tokio::time::timeout(PROMPT, async {
            tokio::join!(rt.run(support::work(2, id.store(&cas), 0)), kill)
        })
        .await
        .expect("the waiting lease was not stopped by its kill");
        let took = waited.elapsed();
        let mut third = std::pin::pin!(support::run(&rt, &cas, 3, &id));
        // Ended within 2 s (it did not wait), its result is that one: a finished
        // future is not polled again.
        let early = tokio::time::timeout(Duration::from_secs(2), &mut third).await;
        std::fs::write(root(1).join("go"), b"").expect("go");
        let waited = early.is_err();
        let third = match early {
            Ok(ended) => ended,
            Err(_) => third.await,
        };
        (killed, took, waited, third)
    };
    let (first, (killed, took, waited, third)) =
        tokio::join!(support::run(&rt, &cas, 1, &hold), rest);
    let first = first.expect("the first lease ran");
    assert_eq!(
        first.exit_code,
        0,
        "{}{}\n{}",
        stdout(&cas, &first),
        support::stderr(&cas, &first),
        slot_report(std::slice::from_ref(user))
    );
    assert!(
        matches!(killed, Err(kbf_daemon::RuntimeError::Killed)),
        "the lease killed while it waited for a slot ended {killed:?}"
    );
    assert!(took < PROMPT, "the kill took {took:?}");
    assert!(waited, "the third lease did not wait for the held slot");
    let third = third.expect("the third lease ran");
    assert_eq!(third.exit_code, 0, "{}", support::stderr(&cas, &third));
    assert_eq!(stdout(&cas, &third), format!("{}\n", user.uid));
}

/// The start. Catches a daemon that starts without ending what a slot user still runs
/// (a killed daemon's survivors would run beside the next lease): a slot user's
/// `sleep`, started before the runtime, is gone once the runtime has started.
#[ignore = "needs root and real users: cargo xtask sudo-test -p kbf-driver-native --test slots"]
#[test]
fn the_start_ends_every_process_of_every_slot() {
    assert_root();
    let scratch = Scratch::new("start");
    let users = Users::new("start", 1);
    let mut left = sleeper(Some(&users.0[0]));
    let pid = i32::try_from(left.id()).expect("pid");
    let before = alive(pid);
    let rt = NativeRuntime::new(
        config(&scratch, users.pool()),
        Arc::new(MemoryCas::default()),
    )
    .expect("runtime");
    let after = alive(pid);
    let _ = left.kill();
    let _ = left.wait();
    drop(rt);
    assert!(before, "the slot user's sleep {pid} did not start");
    assert!(!after, "the slot user's sleep {pid} outlived the start");
}
