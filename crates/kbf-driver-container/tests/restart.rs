//! What a restarted daemon's runtime removes before the daemon says `Hello` (issue
//! #155), against the stand-in `podman` (`fixtures/fake-podman.sh`). `podman.rs`
//! runs the same promise against real rootless Podman.

mod support;

use std::sync::Arc;

use kbf_daemon::Runtime;
use kbf_driver_container::StartError;
use kbf_types::Resources;
use support::fake::{Fake, image};
use support::{Spec, exists, store_action, work};

/// Whether process `pid` has ended: gone, or a zombie not yet reaped.
fn ended(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| Some(stat.rsplit_once(") ")?.1.starts_with('Z')))
        .unwrap_or(true)
}

/// Catches (issue #155) a restarted daemon that leaves its killed predecessor's
/// container running (the "container sweep skipped" mutant: the scheduler would run
/// the lease again beside it), its lease cgroup or scratch directory behind, a sweep
/// that looks for another owner's label, and a container created without the label
/// the sweep finds it by. The killed daemon is modelled by forgetting its run: nothing
/// of it is dropped, so nothing of it is cleaned.
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
    // Not `assert_clean`, which also requires `podman start` reaped before `rm`: that
    // `podman start` was the killed daemon's child, which no later daemon can wait for.
    // It ends by itself once its container is gone.
    assert!(!exists(&fake.lease_dir(1)), "scratch directory left");
    assert!(!exists(&fake.lease_cgroup(1)), "lease cgroup left");
    let ps = std::fs::read_to_string(fake.state.join("ps.args")).expect("ps ran");
    assert!(
        ps.lines().any(|a| a == "--filter=label=kbf.owner=node-1"),
        "{ps}"
    );
}

/// Catches (issue #194) the test removing a Fake's directory while the killed
/// daemon's `podman start` still runs: nobody waits for that process, and once the
/// restarted runtime's sweep has ended its action, it writes its status and events
/// into the state directory, which made the removal fail with "Directory not empty".
/// 200 rounds with the CPUs busy ([`support::CpuHog`]); each restarts and drops its
/// Fake at once (the sweep's checks are the test above's), so the removal overlaps
/// the orphaned `start`'s last writes. The removal must not fail, and nothing of that
/// fake may run once it is done (it waited). The mutant drops the wait in the Fake's
/// `Drop`.
#[tokio::test]
async fn no_round_of_kill_and_restart_races_the_removal() {
    let _hog = support::CpuHog::start();
    for round in 0..200 {
        let (fake, _) = kill_the_daemon(&format!("restart-stress-{round}")).await;
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
    // `exec`: the pid the fake records is the sleep itself, not a shell around it.
    fake.knob("action.sh", "exec sleep 300");
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
    let created = std::fs::read_to_string(fake.state.join("create.args")).expect("args");
    assert!(
        created.lines().any(|a| a == "--label=kbf.owner=node-1"),
        "{created}"
    );
    (fake, pid)
}

/// Catches a runtime that starts although what its predecessor left could not be
/// listed or removed (the leftover could run beside its retry), one that removes
/// containers of another owner, or that are not lease containers, and one that
/// refuses to start on a node that never ran a lease.
#[test]
fn leftovers_that_cannot_be_listed_or_removed_stop_the_start() {
    let fake = Fake::new("sweep-fails");
    fake.knob("ps-fails", "");
    let refused = fake.restart().err();
    assert!(
        matches!(&refused, Some(StartError::Sweep(why)) if why.contains("podman ps")),
        "{refused:?}"
    );
    std::fs::remove_file(fake.state.join("ps-fails")).expect("rm");

    // Lines for another owner, and a container of the owner that is no lease's.
    fake.knob(
        "ps-extra",
        "node-2 kbf-lease-9-9\nnode-1 something-else\nnode-1x kbf-lease-8-8\n",
    );
    let calls = fake.calls().len();
    fake.restart().expect("nothing of node-1's to remove");
    let new: Vec<String> = fake.calls().split_off(calls);
    assert_eq!(new, ["ps"], "only node-1's lease containers are removed");

    std::fs::create_dir(fake.scratch.join("kbf-lease-7-7")).expect("mkdir");
    std::fs::create_dir(fake.scratch.join("not-a-lease")).expect("mkdir");
    fake.knob("rm-fails", "");
    let refused = fake.restart().err();
    assert!(
        matches!(&refused, Some(StartError::Sweep(why)) if why.contains("rm refused")),
        "{refused:?}"
    );
    std::fs::remove_file(fake.state.join("rm-fails")).expect("rm");
    fake.restart().expect("removed once it can be");
    assert!(!exists(&fake.scratch.join("kbf-lease-7-7")));
    assert!(
        exists(&fake.scratch.join("not-a-lease")),
        "not a lease directory"
    );

    // A scratch root that is not a directory cannot be swept.
    std::fs::remove_dir(fake.scratch.join("not-a-lease")).expect("rmdir");
    std::fs::remove_dir(&fake.scratch).expect("rmdir");
    std::fs::write(&fake.scratch, b"in the way").expect("write");
    assert!(matches!(fake.restart(), Err(StartError::Sweep(_))));
    // One that does not exist yet holds nothing.
    std::fs::remove_file(&fake.scratch).expect("rm");
    fake.restart().expect("no scratch root yet");
}
