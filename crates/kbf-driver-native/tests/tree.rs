//! The whole process tree ends with the lease: after a normal exit, a timeout, a kill
//! and a dropped run, no process the action started is left alive, including one that
//! left the process group and was orphaned.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::{Runtime, RuntimeError};
use support::{MemoryCas, Spec, alive, config, no_leases, pid_in, run, runtime, scratch, work};

/// A Perl program that forks a child which leaves the process group (`setsid`),
/// writes its pid to `$PIDFILE` and sleeps; the leader waits until a few polls have
/// seen the child, then exits, so the child is orphaned and reparented.
const ESCAPER: &str = r#"
use POSIX ();
my $pid = fork();
if ($pid == 0) { POSIX::setsid(); sleep 300; exit 0; }
open(my $f, '>', $ENV{PIDFILE}) or die; print $f "$pid\n"; close($f);
select(undef, undef, undef, 0.3);
exit 0;
"#;

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
    let pidfile = dir.join("pid");
    let spec = Spec::sh("sleep 300 & echo $! > \"$PIDFILE\"; exit 0")
        .env("PIDFILE", pidfile.to_str().expect("utf8"));
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 0);
    let pid = pid_in(&pidfile).await;
    assert!(!alive(pid), "the background child {pid} outlived the lease");

    let pidfile = dir.join("escaped");
    let spec = Spec::argv(&["/usr/bin/perl", "-e", ESCAPER])
        .env("PIDFILE", pidfile.to_str().expect("utf8"));
    let result = run(&rt, &cas, 2, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 0, "{}", support::stderr(&cas, &result));
    let pid = pid_in(&pidfile).await;
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
    let pidfile = dir.join("pid");
    let spec = Spec::sh("sh -c 'sleep 300 & echo $! > \"$PIDFILE\"; wait' & wait")
        .env("PIDFILE", pidfile.to_str().expect("utf8"))
        .timeout(Duration::from_millis(500));
    let outcome = run(&rt, &cas, 1, &spec).await;
    assert!(
        matches!(outcome, Err(RuntimeError::TimedOut)),
        "{outcome:?}"
    );
    let pid = pid_in(&pidfile).await;
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
    let pidfile = dir.join("pid");
    let action = Spec::sh("sleep 300 & echo $! > \"$PIDFILE\"; wait")
        .env("PIDFILE", pidfile.to_str().expect("utf8"))
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
    let pidfile = dir.join("pid");
    let action = Spec::sh("sleep 300 & echo $! > \"$PIDFILE\"; wait")
        .env("PIDFILE", pidfile.to_str().expect("utf8"))
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
