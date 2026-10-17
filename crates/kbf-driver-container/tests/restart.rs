//! What a restarted daemon's runtime removes before the daemon says `Hello` (issue
//! #155), against the stand-in `crun` (`support/fake.rs`). `crun.rs`
//! runs the same promise against real rootless `crun`.

mod support;

use std::sync::Arc;

use kbf_daemon::Runtime;
use kbf_driver_container::StartError;
use kbf_types::Resources;
use support::fake::{Fake, image};
use support::{Spec, ended, exists, store_action, work};

/// Catches (issue #155) a restarted daemon that leaves its killed predecessor's
/// action running (the "sweep skipped" mutant: the scheduler would run the lease again
/// beside it), its container, lease cgroup or scratch directory behind. The killed
/// daemon is modelled by forgetting its run: nothing of it is dropped, so nothing of it
/// is cleaned.
#[tokio::test]
async fn a_restarted_runtime_removes_what_its_predecessor_left() {
    let (fake, pid) = kill_the_daemon("restart").await;
    let pid = pid.as_str();
    fake.restart().expect("the restarted runtime starts");
    assert!(ended(pid), "the container's action still runs");
    assert!(
        exists(&fake.state.join("removed")),
        "the container was not removed"
    );
    // Not `assert_clean`, which also requires `crun run` reaped before the delete: that
    // `crun run` was the killed daemon's child, which no later daemon can wait for. It
    // ends by itself once its container is gone.
    assert!(!exists(&fake.lease_dir(1)), "scratch directory left");
    assert!(!exists(&fake.lease_cgroup(1)), "lease cgroup left");
}

/// Catches (issue #194) the test removing a Fake's directory while the killed
/// daemon's `crun run` still runs: nobody waits for that process, and once the
/// restarted runtime's sweep has ended its action, it writes its status and events
/// into the state directory, which made the removal fail with "Directory not empty".
/// 200 rounds with the CPUs busy ([`support::CpuHog`]); each restarts and drops its
/// Fake at once (the sweep's checks are the test above's). Ballast in the state
/// directory (2000 empty files in a subdirectory) makes its removal take long enough
/// to overlap the orphaned run's last writes, which land in the state directory
/// after the delete listed it. The removal must not fail, and nothing of that fake may run
/// once it is done (it waited). The mutant drops the wait in the Fake's `Drop`.
#[tokio::test]
async fn no_round_of_kill_and_restart_races_the_removal() {
    let _hog = support::CpuHog::start();
    for round in 0..200 {
        let (fake, _) = kill_the_daemon(&format!("restart-stress-{round}")).await;
        let ballast = fake.state.join("ballast");
        std::fs::create_dir(&ballast).expect("mkdir ballast");
        for i in 0..2000 {
            std::fs::write(ballast.join(i.to_string()), b"").expect("write ballast");
        }
        // What the fake's checks need from it, taken before it is gone.
        let probe = support::fake::Probe::of(&fake);
        let dir = fake.dir.clone();
        fake.restart().expect("the restarted runtime starts");
        drop(fake);
        let left = probe.processes();
        assert!(
            left.is_empty(),
            "round {round}: pids {left:?} of {} run after its removal",
            dir.display()
        );
    }
}

/// Starts a lease and kills the daemon mid-run: the run is forgotten, never polled or
/// dropped again. Returns the fake and the action's pid, which runs.
async fn kill_the_daemon(name: &str) -> (Fake, String) {
    let fake = Fake::new(name);
    // The pid the fake records is the sleeping act program itself.
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
    // The fake creates the pid file, then writes it.
    let mut pid = String::new();
    for _ in 0..500 {
        pid = std::fs::read_to_string(fake.state.join("pid")).expect("pid");
        if pid.ends_with('\n') {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let pid = pid.trim().to_owned();
    assert!(!ended(&pid), "the action runs");
    (fake, pid)
}

/// Catches a runtime that starts although what its predecessor left could not be
/// removed (the leftover could run beside its retry), one that removes what is not a
/// lease's, and one that refuses to start on a node that never ran a lease.
#[test]
fn leftovers_that_cannot_be_removed_stop_the_start() {
    let fake = Fake::new("sweep-fails");
    std::fs::create_dir(fake.scratch.join("kbf-lease-7-7")).expect("mkdir");
    std::fs::create_dir(fake.scratch.join("not-a-lease")).expect("mkdir");
    fake.knob("delete-fails", "");
    let refused = fake.restart().err();
    assert!(
        matches!(&refused, Some(StartError::Sweep(why)) if why.contains("delete refused")),
        "{refused:?}"
    );
    std::fs::remove_file(fake.state.join("delete-fails")).expect("rm");
    fake.restart().expect("removed once it can be");
    assert!(!exists(&fake.scratch.join("kbf-lease-7-7")));
    assert!(
        exists(&fake.scratch.join("not-a-lease")),
        "not a lease directory"
    );

    // A scratch directory that cannot be listed cannot be swept.
    use std::os::unix::fs::PermissionsExt as _;
    let mode = |bits| {
        std::fs::set_permissions(&fake.scratch, std::fs::Permissions::from_mode(bits))
            .expect("chmod");
    };
    mode(0o300);
    let refused = fake.restart().err();
    mode(0o700);
    let shown = fake.scratch.display().to_string();
    assert!(
        matches!(&refused, Some(StartError::Sweep(why)) if why.starts_with(&shown)),
        "{refused:?}"
    );
}
