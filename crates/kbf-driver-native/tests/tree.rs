//! The whole process tree ends with the lease: after a normal exit, a timeout, a kill,
//! a dropped run and the memory watch, no process the action started is left alive,
//! including one that left the process group and was orphaned. The memory watch kills
//! an action whose processes together hold more than the lease's limit and reports it
//! out of memory; one within it runs to the end.
//!
//! Every way a run ends is in this one test binary: the async body that runs an action
//! is compiled into each test binary that calls it, and llvm-cov's summary counts the
//! best-covered copy, not the union of the copies.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::{Runtime, RuntimeError};
use kbf_driver_native::MemoryPolicy;
use support::{
    MemoryCas, PROMPT, Spec, alive, config, no_leases, pid_in, run, runtime, scratch, work,
};

/// A Perl program that forks a child which leaves the process group (`setsid`),
/// prints its pid and sleeps; the leader waits until a few polls have
/// seen the child, then exits, so the child is orphaned and reparented.
const ESCAPER: &str = r#"
use POSIX ();
my $pid = fork();
if ($pid == 0) { POSIX::setsid(); sleep 300; exit 0; }
print "$pid\n";
select(undef, undef, undef, 0.3);
exit 0;
"#;

/// Where a test action writes its pid: in its working directory, since the sandbox
/// lets an action write nowhere outside its lease.
const PIDFILE: &str = "pid";

/// The pid file of lease `1.seq` while it runs, as the test sees it.
fn in_lease(config: &kbf_driver_native::NativeConfig, seq: u64) -> std::path::PathBuf {
    config
        .scratch
        .join(format!("lease-1-{seq}"))
        .join("root")
        .join(PIDFILE)
}

/// The pid an action printed on its stdout.
fn pid_of(cas: &MemoryCas, result: &kbf_proto::reapi::ActionResult) -> i32 {
    let out = support::stdout(cas, result);
    out.trim()
        .parse()
        .unwrap_or_else(|_| panic!("no pid in {out:?}"))
}

/// Catches: background processes an action leaves behind surviving its lease (they
/// would run on, and could change outputs while they are read). Covers a plain
/// background child in the group, and one that left the group with `setsid` and was
/// orphaned when its parent exited.
#[tokio::test]
async fn processes_left_behind_after_exit_are_killed() {
    let dir = scratch("left-behind");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let spec = Spec::sh("sleep 300 & echo $!; exit 0");
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 0);
    let pid = pid_of(&cas, &result);
    assert!(!alive(pid), "the background child {pid} outlived the lease");

    let spec = Spec::argv(&["/usr/bin/perl", "-e", ESCAPER]);
    let result = run(&rt, &cas, 2, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 0, "{}", support::stderr(&cas, &result));
    let pid = pid_of(&cas, &result);
    assert!(!alive(pid), "the setsid child {pid} outlived the lease");
    assert!(no_leases(&config));
}

/// Catches: a timeout that ends only the leader (a grandchild runs on), or that is
/// not reported as one.
#[tokio::test]
async fn a_timeout_ends_the_whole_tree() {
    let dir = scratch("timeout");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let spec = Spec::sh("sh -c 'sleep 300 & echo $! > \"$PIDFILE\"; wait' & wait")
        .env("PIDFILE", PIDFILE)
        .timeout(Duration::from_millis(500));
    let pidfile = in_lease(&config, 1);
    let (outcome, pid) = tokio::join!(run(&rt, &cas, 1, &spec), pid_in(&pidfile));
    assert!(
        matches!(outcome, Err(RuntimeError::TimedOut)),
        "{outcome:?}"
    );
    assert!(!alive(pid), "the grandchild {pid} outlived the timeout");
    assert!(no_leases(&config));
}

/// Catches: the daemon's kill ending only the leader, or returning before the tree
/// is gone and the lease directory removed.
#[tokio::test]
async fn a_kill_ends_the_whole_tree_before_it_returns() {
    let dir = scratch("kill");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = Arc::new(runtime(config.clone(), &cas));
    let pidfile = in_lease(&config, 1);
    let action = Spec::sh("sleep 300 & echo $! > \"$PIDFILE\"; wait")
        .env("PIDFILE", PIDFILE)
        .store(&cas);
    let running = tokio::spawn({
        let rt = Arc::clone(&rt);
        async move { rt.run(work(1, action, 0)).await }
    });
    let pid = pid_in(&pidfile).await;
    rt.kill(kbf_types::LeaseId::new(1, 1)).await;
    assert!(!alive(pid), "the child {pid} outlived the kill");
    assert!(no_leases(&config), "the kill returned before the clean");
    let outcome = running.await.expect("run task");
    assert!(matches!(outcome, Err(RuntimeError::Killed)), "{outcome:?}");
}

/// Catches: a run the daemon drops (its task cancelled) leaving the action's
/// processes or its lease directory behind.
#[tokio::test]
async fn a_dropped_run_ends_the_tree_and_cleans() {
    let dir = scratch("dropped");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let pidfile = in_lease(&config, 1);
    let action = Spec::sh("sleep 300 & echo $! > \"$PIDFILE\"; wait")
        .env("PIDFILE", PIDFILE)
        .store(&cas);
    let run = rt.run(work(1, action, 0));
    let watched = async {
        let pid = pid_in(&pidfile).await;
        // Long enough for the watch to have seen the tree.
        tokio::time::sleep(Duration::from_millis(100)).await;
        pid
    };
    let pid = tokio::select! {
        outcome = run => panic!("the run ended: {outcome:?}"),
        pid = watched => pid,
    };
    assert!(!alive(pid), "the child {pid} outlived the dropped run");
    assert!(no_leases(&config));
}

/// Catches: processes that outlive the kill wait reported as a clean end. With no
/// wait at all, the first round of SIGKILL is never confirmed, so the lease fails.
#[tokio::test]
async fn processes_that_outlive_the_kill_wait_fail_the_lease() {
    let dir = scratch("survivors");
    let mut config = config(&dir);
    config.kill_wait = Duration::ZERO;
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let outcome = run(&rt, &cas, 1, &Spec::sh("sleep 300 & exit 0")).await;
    assert!(
        matches!(&outcome, Err(RuntimeError::Failed(why)) if why.contains("survived SIGKILL")),
        "{outcome:?}"
    );
    assert!(no_leases(&config));
}

/// Perl that writes its pid to `$PIDFILE`, then fills and holds `$MIB` MiB and
/// sleeps.
const HOG: &str = r#"
open(my $f, '>', $ENV{PIDFILE}) or die; print $f "$$\n"; close($f);
my $x = "a" x ($ENV{MIB} * 1024 * 1024);
sleep 300;
"#;

/// Catches: the memory watch skipped (the hog runs to its timeout), memory counted
/// for the leader only (the hog is a grandchild), the hog left alive after the kill,
/// and an out-of-memory end reported as anything but OutOfMemory with the limit.
#[tokio::test]
async fn an_action_past_its_limit_is_killed_whole() {
    let dir = scratch("oom");
    let mut config = config(&dir);
    config.memory = MemoryPolicy {
        percent: 100,
        headroom_bytes: 0,
    };
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let action = Spec::sh("/usr/bin/perl -e \"$HOG\" & wait")
        .env("PIDFILE", PIDFILE)
        .env("HOG", HOG)
        .env("MIB", "256")
        .timeout(Duration::from_secs(60))
        .store(&cas);
    let limit = 64 << 20;
    let pidfile = in_lease(&config, 1);
    let (outcome, pid) = tokio::join!(
        tokio::time::timeout(PROMPT * 3, rt.run(work(1, action, limit))),
        pid_in(&pidfile)
    );
    let outcome = outcome.expect("the watch ends the run long before its timeout");
    match outcome {
        Err(RuntimeError::OutOfMemory { used, limit: l }) => {
            assert_eq!(l, limit);
            assert!(used > limit, "{used} <= {limit}");
        }
        other => panic!("not out of memory: {other:?}"),
    }
    assert!(!alive(pid), "the hog {pid} outlived the kill");
    assert!(no_leases(&config));
}

/// Catches: a limit applied to an action within it (killed for memory it does not
/// hold), or a limit applied when nothing was booked.
#[tokio::test]
async fn an_action_within_its_limit_runs_to_the_end() {
    let dir = scratch("within");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config, &cas);
    let script = "/usr/bin/perl -e 'open(my $f, \">\", $ENV{PIDFILE}); print $f \"$$\\n\"; close($f); my $x = \"a\" x (32*1024*1024); select(undef,undef,undef,0.3)'";
    for (seq, booked) in [(1, 64_u64 << 20), (2, 0)] {
        let action = Spec::sh(script).env("PIDFILE", PIDFILE).store(&cas);
        let result = rt.run(work(seq, action, booked)).await.expect("ran");
        assert_eq!(result.exit_code, 0, "booked {booked}");
    }
}
