//! The driver's promises, tested once and run against every container runtime it has:
//! each body below is generic over a rig (`support/rig.rs`) and runs twice, against
//! the stand-in `podman` (`podman_…`) and against the stand-in `crun` of the OCI
//! backend (`oci_…`). These run on any Linux machine; `oci_crun.rs` and `podman.rs`
//! run the real runtimes.
//!
//! A defect the OCI backend could have that Podman's path cannot (or the other way
//! round) is tested in `fake_oci.rs` or `fake_podman.rs`; what is here is what both
//! must keep: exit codes and logs, what a timeout keeps, a kill, a dropped run, the
//! clean, runtime failures, the second start after `ETXTBSY`, the sweep at start, and
//! the order in which the lease's directories are handed over.

use std::time::{Duration, Instant};

use crate::support::fake::{Fake, image};
use crate::support::fake_oci::FakeOci;
use crate::support::rig::Rig;
use crate::support::{Spec, blob, ended, exists, store_action, work};
use kbf_daemon::{Runtime, RuntimeError};
use kbf_types::Resources;

/// Runs `$body::<R>()` as two tests, `podman_$body` and `oci_$body`.
macro_rules! conformance {
    ($($body:ident),+ $(,)?) => {
        mod podman {
            use super::*;
            $(
                #[tokio::test]
                async fn $body() {
                    super::$body::<Fake>().await;
                }
            )+
        }
        mod oci {
            use super::*;
            $(
                #[tokio::test]
                async fn $body() {
                    super::$body::<FakeOci>().await;
                }
            )+
        }
    };
}

conformance!(
    the_exit_code_and_both_logs_are_collected,
    outputs_are_collected_from_the_upper_layer,
    a_timeout_keeps_the_partial_logs_and_cleans_up,
    kill_stops_the_action_and_returns_once_clean,
    sigterm_ignored_falls_back_to_cgroup_kill,
    a_dropped_run_still_cleans_up,
    a_runtime_failure_is_an_infrastructure_failure,
    a_busy_program_is_started_once_more,
    a_restart_removes_what_its_predecessor_left,
    the_lease_is_handed_to_the_container_and_back,
);

fn name<R: Rig>(test: &str) -> String {
    format!("conf-{}-{test}", R::NAME)
}

/// Catches the exit code, stdout or stderr of the action not reaching the result, and
/// anything left behind: the lease's scratch directory, its cgroup, its processes.
async fn the_exit_code_and_both_logs_are_collected<R: Rig>() {
    let rig = R::new(&name::<R>("exit"));
    let spec = Spec::new(&image(), "unused");
    let steps = [
        "echo", "out", ";", "echo", "--stderr", "err", ";", "exit", "3",
    ];
    let result = rig.run(1, &spec, &steps).await.expect("ran");
    assert_eq!(result.exit_code, 3);
    assert_eq!(blob(&rig.cas(), result.stdout_digest.as_ref()), b"out\n");
    assert_eq!(blob(&rig.cas(), result.stderr_digest.as_ref()), b"err\n");
    rig.assert_clean(1);
}

/// Catches an output the action wrote in the upper layer not being collected, or one it
/// never wrote being invented.
async fn outputs_are_collected_from_the_upper_layer<R: Rig>() {
    let rig = R::new(&name::<R>("outputs"));
    let mut spec = Spec::new(&image(), "unused");
    spec.outputs = vec!["out/a.txt".to_owned(), "out/never".to_owned()];
    let steps = ["write-lines", "$UPPER/out/a.txt", "hello"];
    // The parent is made by the driver (`prepare_outputs`), as the action expects.
    let result = rig.run(1, &spec, &steps).await.expect("ran");
    assert_eq!(result.exit_code, 0);
    let files: Vec<_> = result
        .output_files
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    assert_eq!(files, ["out/a.txt"], "{result:?}");
    let digest = result.output_files[0].digest.as_ref();
    assert_eq!(blob(&rig.cas(), digest), b"hello\n");
    rig.assert_clean(1);
}

/// Catches a timed-out action's partial stdout and stderr being lost, the action
/// not being signalled, and anything left behind.
async fn a_timeout_keeps_the_partial_logs_and_cleans_up<R: Rig>() {
    let rig = R::new(&name::<R>("timeout"));
    let mut spec = Spec::new(&image(), "unused");
    // Room for the two echoes to land before the timeout on a loaded runner.
    spec.timeout = Some(Duration::from_secs(1));
    let steps = [
        "echo", "started", ";", "echo", "--stderr", "warming", ";", "sleep", "30",
    ];
    let outcome = rig.run(1, &spec, &steps).await;
    let Err(RuntimeError::TimedOut { stdout, stderr }) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(blob(&rig.cas(), stdout.as_ref()), b"started\n");
    assert_eq!(blob(&rig.cas(), stderr.as_ref()), b"warming\n");
    assert!(
        rig.calls().contains(&"kill".to_owned()),
        "{:?}",
        rig.calls()
    );
    rig.assert_clean(1);
}

/// Catches `Runtime::kill` returning before the lease is clean, or not stopping the
/// action; and a kill of a lease that is not running doing anything.
async fn kill_stops_the_action_and_returns_once_clean<R: Rig>() {
    let rig = R::new(&name::<R>("kill"));
    let spec = Spec::new(&image(), "unused");
    rig.action(&["sleep", "30"]);
    let action = store_action(&rig.cas(), &spec);
    let runtime = rig.runtime();
    let run = tokio::spawn(async move { runtime.run(work(1, action, Resources::default())).await });
    rig.wait_for_start().await;
    rig.runtime().kill(kbf_types::LeaseId::new(1, 1)).await;
    rig.assert_clean(1);
    let outcome = run.await.expect("join");
    assert!(matches!(outcome, Err(RuntimeError::Killed)), "{outcome:?}");
    rig.runtime().kill(kbf_types::LeaseId::new(1, 1)).await;
}

/// Catches the kill path stopping at SIGTERM: an action that ignores it must still be
/// ended, by the lease cgroup's kill, and nothing left behind.
async fn sigterm_ignored_falls_back_to_cgroup_kill<R: Rig>() {
    // Slack for a loaded machine, without changing what is checked: the stand-ins
    // notice cgroup.kill by polling.
    let rig = R::with(&name::<R>("cgroup-kill"), |c| {
        c.kill_grace = Duration::from_secs(3);
    });
    let mut spec = Spec::new(&image(), "unused");
    spec.timeout = Some(Duration::from_secs(2));
    // SIGTERM is 15.
    let steps = ["ignore-signal", "15", ";", "sleep", "30"];
    let outcome = rig.run(1, &spec, &steps).await;
    assert!(
        matches!(outcome, Err(RuntimeError::TimedOut { .. })),
        "{outcome:?}"
    );
    let events: Vec<_> = rig
        .events()
        .into_iter()
        .filter(|e| !e.starts_with("chown"))
        .collect();
    assert_eq!(
        events.first().map(String::as_str),
        Some("cgroup.kill ended the action"),
        "{events:?}"
    );
    rig.assert_clean(1);
}

/// Catches a cancelled run (the daemon dropped its task) leaving its container, cgroup
/// or scratch directory behind, or cleaning while the runtime's process may still run
/// (kbf #157).
async fn a_dropped_run_still_cleans_up<R: Rig>() {
    let rig = R::new(&name::<R>("dropped"));
    let spec = Spec::new(&image(), "unused");
    rig.action(&["sleep", "30"]);
    let action = store_action(&rig.cas(), &spec);
    let runtime = rig.runtime();
    let run = tokio::spawn(async move { runtime.run(work(1, action, Resources::default())).await });
    rig.wait_for_start().await;
    // Positive control: what `assert_clean` must find gone is there to find.
    rig.assert_action_running(1);
    let started = Instant::now();
    run.abort();
    assert!(run.await.expect_err("cancelled").is_cancelled());
    // The clean ends the action at once (the cgroup's kill, or the start's), it does not
    // wait out the five seconds it allows a start that will not end.
    let took = started.elapsed();
    assert!(took < Duration::from_secs(3), "the clean took {took:?}");
    rig.assert_clean(1);
}

/// Catches the runtime's own failure being reported as the action's result: it is the
/// farm's, so an infrastructure failure, and nothing is left behind.
async fn a_runtime_failure_is_an_infrastructure_failure<R: Rig>() {
    let rig = R::new(&name::<R>("runtime-fails"));
    rig.runtime_breaks();
    let outcome = rig
        .run(1, &Spec::new(&image(), "unused"), &["exit", "0"])
        .await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(_))),
        "{outcome:?}"
    );
    rig.assert_clean(1);
}

/// Catches an exec that found its program busy (ETXTBSY) failing the lease instead of
/// being tried once more, and being tried more than once.
async fn a_busy_program_is_started_once_more<R: Rig>() {
    let rig = R::new(&name::<R>("busy"));
    rig.busy_once();
    let result = rig
        .run(1, &Spec::new(&image(), "unused"), &["echo", "ran"])
        .await
        .expect("ran");
    assert_eq!(result.exit_code, 0);
    assert_eq!(blob(&rig.cas(), result.stdout_digest.as_ref()), b"ran\n");
    assert_eq!(rig.starts(), 2, "{:?}", rig.calls());
    rig.assert_clean(1);
}

/// Catches a restarted daemon that leaves its killed predecessor's action running, or its
/// lease cgroup or scratch directory behind. The killed daemon is modelled by forgetting
/// its run: nothing of it is dropped, so nothing of it is cleaned.
async fn a_restart_removes_what_its_predecessor_left<R: Rig>() {
    let rig = R::new(&name::<R>("restart"));
    rig.action(&["sleep", "300"]);
    let action = store_action(&rig.cas(), &Spec::new(&image(), "unused"));
    let runtime = rig.runtime();
    let mut run = Box::pin(async move { runtime.run(work(1, action, Resources::default())).await });
    tokio::select! {
        _ = &mut run => panic!("the run ended"),
        () = rig.wait_for_start() => {}
    }
    // Never polled or dropped again, as a daemon that was killed.
    std::mem::forget(run);
    // The stand-in creates the pid file, then writes it.
    let mut pid = String::new();
    for _ in 0..500 {
        pid = rig.action_pid();
        if !pid.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!ended(&pid), "the action runs");
    rig.restart().expect("the restarted runtime starts");
    assert!(ended(&pid), "the action still runs");
    assert!(!exists(&rig.lease_dir(1)), "scratch directory left");
    assert!(!exists(&rig.lease_cgroup(1)), "lease cgroup left");
}

/// Catches the lease's directories not being given to the container's root before the
/// action runs, or not being given back after it and before the outputs are read: the
/// order is `chown 1:1`, the run, `chown 0:0`.
async fn the_lease_is_handed_to_the_container_and_back<R: Rig>() {
    let rig = R::new(&name::<R>("hand-over"));
    let result = rig
        .run(1, &Spec::new(&image(), "unused"), &["exit", "0"])
        .await
        .expect("ran");
    assert_eq!(result.exit_code, 0);
    let events = rig.events();
    let at = |prefix: &str| {
        events
            .iter()
            .position(|e| e.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix:?} in {events:?}"))
    };
    let ended = events
        .iter()
        .position(|e| e == "start ended" || e == "run ended")
        .expect("the run ended");
    assert!(at("chown 1:1") < ended, "{events:?}");
    assert!(ended < at("chown 0:0"), "{events:?}");
    rig.assert_clean(1);
}
