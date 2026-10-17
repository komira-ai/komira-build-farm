//! The driver's promises about a lease, against the stand-in `crun` (`support/fake.rs`):
//! exit codes and logs, what a timeout keeps, a kill, a dropped run, the clean, runtime
//! failures, the second start after `ETXTBSY`, the sweep at start, and the order in
//! which the lease's directories are handed over. These run on any Linux machine;
//! `oci_crun.rs` and `crun.rs` run the same promises against the real `crun`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::support::fake::{Fake, image};
use crate::support::{Spec, blob, ended, exists, store_action, work};
use kbf_daemon::{Runtime, RuntimeError};
use kbf_types::Resources;

/// Catches the exit code, stdout or stderr of the action not reaching the result, and
/// anything left behind: the lease's scratch directory, its cgroup, its processes.
#[tokio::test]
async fn the_exit_code_and_both_logs_are_collected() {
    let fake = Fake::new("conf-exit");
    let spec = Spec::new(&image(), "unused");
    let steps = [
        "echo", "out", ";", "echo", "--stderr", "err", ";", "exit", "3",
    ];
    let result = fake.run(1, &spec, &steps).await.expect("ran");
    assert_eq!(result.exit_code, 3);
    assert_eq!(blob(&fake.cas, result.stdout_digest.as_ref()), b"out\n");
    assert_eq!(blob(&fake.cas, result.stderr_digest.as_ref()), b"err\n");
    fake.assert_clean(1);
}

/// Catches an output the action wrote in the upper layer not being collected, or one it
/// never wrote being invented.
#[tokio::test]
async fn outputs_are_collected_from_the_upper_layer() {
    let fake = Fake::new("conf-outputs");
    let mut spec = Spec::new(&image(), "unused");
    spec.outputs = vec!["out/a.txt".to_owned(), "out/never".to_owned()];
    let steps = ["write-lines", "$UPPER/out/a.txt", "hello"];
    // The parent is made by the driver (`prepare_outputs`), as the action expects.
    let result = fake.run(1, &spec, &steps).await.expect("ran");
    assert_eq!(result.exit_code, 0);
    let files: Vec<_> = result
        .output_files
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    assert_eq!(files, ["out/a.txt"], "{result:?}");
    let digest = result.output_files[0].digest.as_ref();
    assert_eq!(blob(&fake.cas, digest), b"hello\n");
    fake.assert_clean(1);
}

/// Catches a timed-out action's partial stdout and stderr being lost, the action
/// not being signalled, and anything left behind.
#[tokio::test]
async fn a_timeout_keeps_the_partial_logs_and_cleans_up() {
    let fake = Fake::new("conf-timeout");
    let mut spec = Spec::new(&image(), "unused");
    // Room for the two echoes to land before the timeout on a loaded runner.
    spec.timeout = Some(Duration::from_secs(1));
    let steps = [
        "echo", "started", ";", "echo", "--stderr", "warming", ";", "sleep", "30",
    ];
    let outcome = fake.run(1, &spec, &steps).await;
    let Err(RuntimeError::TimedOut { stdout, stderr }) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(blob(&fake.cas, stdout.as_ref()), b"started\n");
    assert_eq!(blob(&fake.cas, stderr.as_ref()), b"warming\n");
    assert!(
        fake.calls().contains(&"kill".to_owned()),
        "{:?}",
        fake.calls()
    );
    fake.assert_clean(1);
}

/// Catches `Runtime::kill` returning before the lease is clean, or not stopping the
/// action; and a kill of a lease that is not running doing anything.
#[tokio::test]
async fn kill_stops_the_action_and_returns_once_clean() {
    let fake = Fake::new("conf-kill");
    let spec = Spec::new(&image(), "unused");
    fake.action(&["sleep", "30"]);
    let action = store_action(&fake.cas, &spec);
    let runtime = Arc::clone(&fake.runtime);
    let run = tokio::spawn(async move { runtime.run(work(1, action, Resources::default())).await });
    fake.wait_for_start().await;
    Arc::clone(&fake.runtime)
        .kill(kbf_types::LeaseId::new(1, 1))
        .await;
    fake.assert_clean(1);
    let outcome = run.await.expect("join");
    assert!(matches!(outcome, Err(RuntimeError::Killed)), "{outcome:?}");
    Arc::clone(&fake.runtime)
        .kill(kbf_types::LeaseId::new(1, 1))
        .await;
}

/// Catches a cancelled run (the daemon dropped its task) leaving its container, cgroup
/// or scratch directory behind, or cleaning while the runtime's process may still run
/// (kbf #157).
#[tokio::test]
async fn a_dropped_run_still_cleans_up() {
    let fake = Fake::new("conf-dropped");
    let spec = Spec::new(&image(), "unused");
    fake.action(&["sleep", "30"]);
    let action = store_action(&fake.cas, &spec);
    let runtime = Arc::clone(&fake.runtime);
    let run = tokio::spawn(async move { runtime.run(work(1, action, Resources::default())).await });
    fake.wait_for_start().await;
    // Positive control: what `assert_clean` must find gone is there to find.
    fake.assert_action_running(1);
    let started = Instant::now();
    run.abort();
    assert!(run.await.expect_err("cancelled").is_cancelled());
    // The clean ends the action at once (the cgroup's kill, or the start's), it does not
    // wait out the five seconds it allows a start that will not end.
    let took = started.elapsed();
    assert!(took < Duration::from_secs(3), "the clean took {took:?}");
    fake.assert_clean(1);
}

/// Catches the runtime's own failure being reported as the action's result: it is the
/// farm's, so an infrastructure failure, and nothing is left behind.
#[tokio::test]
async fn a_runtime_failure_is_an_infrastructure_failure() {
    let fake = Fake::new("conf-runtime-fails");
    fake.runtime_breaks();
    let outcome = fake
        .run(1, &Spec::new(&image(), "unused"), &["exit", "0"])
        .await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(_))),
        "{outcome:?}"
    );
    fake.assert_clean(1);
}

/// Catches an exec that found its program busy (ETXTBSY) failing the lease instead of
/// being tried once more, and being tried more than once.
#[tokio::test]
async fn a_busy_program_is_started_once_more() {
    let fake = Fake::new("conf-busy");
    fake.busy_once();
    let result = fake
        .run(1, &Spec::new(&image(), "unused"), &["echo", "ran"])
        .await
        .expect("ran");
    assert_eq!(result.exit_code, 0);
    assert_eq!(blob(&fake.cas, result.stdout_digest.as_ref()), b"ran\n");
    assert_eq!(fake.starts(), 2, "{:?}", fake.calls());
    fake.assert_clean(1);
}

/// Catches a restarted daemon that leaves its killed predecessor's action running, or its
/// lease cgroup or scratch directory behind. The killed daemon is modelled by forgetting
/// its run: nothing of it is dropped, so nothing of it is cleaned.
#[tokio::test]
async fn a_restart_removes_what_its_predecessor_left() {
    let fake = Fake::new("conf-restart");
    fake.action(&["sleep", "300"]);
    let action = store_action(&fake.cas, &Spec::new(&image(), "unused"));
    let runtime = Arc::clone(&fake.runtime);
    let mut run = Box::pin(async move { runtime.run(work(1, action, Resources::default())).await });
    tokio::select! {
        _ = &mut run => panic!("the run ended"),
        () = fake.wait_for_start() => {}
    }
    // Never polled or dropped again, as a daemon that was killed.
    std::mem::forget(run);
    // The stand-in creates the pid file, then writes it.
    let mut pid = String::new();
    for _ in 0..500 {
        pid = fake.action_pid();
        if !pid.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!ended(&pid), "the action runs");
    fake.restart().expect("the restarted runtime starts");
    assert!(ended(&pid), "the action still runs");
    assert!(!exists(&fake.lease_dir(1)), "scratch directory left");
    assert!(!exists(&fake.lease_cgroup(1)), "lease cgroup left");
}

/// Catches the lease's directories not being given to the container's root before the
/// action runs, or not being given back after it and before the outputs are read: the
/// order is `chown 1:1`, the run, `chown 0:0`.
#[tokio::test]
async fn the_lease_is_handed_to_the_container_and_back() {
    let fake = Fake::new("conf-hand-over");
    let result = fake
        .run(1, &Spec::new(&image(), "unused"), &["exit", "0"])
        .await
        .expect("ran");
    assert_eq!(result.exit_code, 0);
    let events = fake.events();
    let at = |prefix: &str| {
        events
            .iter()
            .position(|e| e.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix:?} in {events:?}"))
    };
    let ended = events
        .iter()
        .position(|e| e == "run ended")
        .expect("the run ended");
    assert!(at("chown 1:1") < ended, "{events:?}");
    assert!(ended < at("chown 0:0"), "{events:?}");
    fake.assert_clean(1);
}
