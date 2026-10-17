//! A killed daemon's container action, against real rootless `crun` and a delegated
//! cgroup (issue #155).
//!
//! Run by `cargo xtask podman-tests`, like `podman.rs` (the test is `#[ignore]` with
//! that reason). The daemon here is a second run of this test binary, which runs the
//! test's other half (the variable [`ROLE`] set): it starts a lease and, once its
//! container's program runs, prints where the lease is and waits. This half SIGKILLs
//! that process, as the kernel's OOM killer or a panic that aborts ends a daemon.

mod support;

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kbf_daemon::Runtime as _;
use kbf_driver_container::{MemoryCas, PodmanConfig, PodmanRuntime};
use kbf_types::Resources;
use support::real::{Cell, crun, delegation, describe, sh};
use support::store_action;

/// Set in the daemon's environment: the test runs its daemon half.
const ROLE: &str = "KBF_TEST_KILLED_DAEMON";
/// What the daemon half prints before the lease cgroup, scratch directory and cgroup
/// parent, once its container's program runs.
const READY: &str = "kbf-killed-daemon-ready";
/// This test's own lease term: the daemon is another process, whose terms would
/// otherwise start at 1 and name the same containers as this binary's other tests.
const TERM: u64 = 155_155;
const OWNER: &str = "kbf-test-killed";
/// How long the container's processes may outlive the SIGKILL.
const PROMPT: Duration = Duration::from_secs(30);

/// Catches (issue #155) a killed daemon whose container action runs on until a daemon
/// is started again on the node, for as long as the work takes (or for ever): the
/// scheduler requeues its lease after the handover grace, so the work runs twice. The
/// lease cgroup must hold no process within [`PROMPT`] of the daemon's SIGKILL, with no
/// daemon started again. A restarted runtime then removes what is left (container,
/// cgroup, scratch directory), as before.
#[tokio::test]
#[ignore = "needs rootless crun and a delegated cgroup: run by cargo xtask podman-tests"]
async fn a_killed_daemons_container_action_dies_with_it() {
    if std::env::var_os(ROLE).is_some() {
        return be_the_daemon().await;
    }
    // Before the daemon is spawned, so it starts in `supervisor/` with this process.
    delegation();
    let mut daemon = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "a_killed_daemons_container_action_dies_with_it",
            "--include-ignored",
            "--nocapture",
        ])
        .env(ROLE, "1")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the daemon");
    let stdout = BufReader::new(daemon.stdout.take().expect("stdout"));
    let ready = stdout
        .lines()
        .map_while(Result::ok)
        .find_map(|line| line.strip_prefix(READY).map(str::to_owned));
    let ready = ready.unwrap_or_else(|| {
        let _ = daemon.kill();
        panic!("the daemon ended before its lease ran: {:?}", daemon.wait())
    });
    let [lease, scratch, parent] = ready.split_whitespace().collect::<Vec<_>>()[..] else {
        panic!("not a ready line: {ready:?}")
    };
    let (lease, scratch) = (PathBuf::from(lease), PathBuf::from(scratch));
    assert!(populated(&lease), "the action runs: {}", describe(&lease));

    daemon.kill().expect("SIGKILL the daemon");
    daemon.wait().expect("reap the daemon");
    let deadline = Instant::now() + PROMPT;
    while populated(&lease) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let left = populated(&lease).then(|| describe(&lease));

    // What a restarted daemon does, which also ends a red run's container.
    let mut config = PodmanConfig::new(scratch.clone(), parent.to_owned(), OWNER.to_owned());
    config.oci.runtime = crun();
    PodmanRuntime::new(config, Arc::new(MemoryCas::new())).expect("restart");
    let name = format!("kbf-lease-{TERM}-1");
    let directory_left = scratch.join(&name).exists();
    let cgroup = lease.parent().expect("the test's cgroup");
    let _ = std::fs::remove_dir(cgroup);
    support::force_remove(&scratch);
    assert!(
        left.is_none(),
        "the container's processes outlived the killed daemon by {PROMPT:?}:{}",
        left.unwrap_or_default()
    );
    assert!(!directory_left, "lease directory left: {name}");
    assert!(!lease.exists(), "lease cgroup left");
}

/// The daemon's half: a lease running `sleep 300`, reported ready once the sleep runs
/// in the container's cgroup; then it waits to be killed.
async fn be_the_daemon() {
    let mut cell = Cell::new("killed");
    cell.term = TERM;
    let action = store_action(&cell.cas, &sh("sleep 300"));
    let work = cell.work(1, action, Resources::default());
    let runtime = Arc::clone(&cell.runtime);
    let mut run = Box::pin(async move { runtime.run(work).await });
    let lease = cell.cgroup.join(cell.name(1));
    tokio::select! {
        ended = &mut run => panic!("the run ended: {ended:?}"),
        () = sleeping_in(&lease) => {}
    }
    println!(
        "{READY} {} {} {}",
        lease.display(),
        cell.scratch.display(),
        cell.config.cgroup_parent
    );
    let ended = run.await;
    panic!("the run ended before the daemon was killed: {ended:?}");
}

/// Waits until a `sleep` runs in a cgroup under `lease`.
async fn sleeping_in(lease: &Path) {
    let deadline = Instant::now() + PROMPT;
    loop {
        let mut pending = vec![lease.to_owned()];
        while let Some(dir) = pending.pop() {
            let procs = std::fs::read_to_string(dir.join("cgroup.procs")).unwrap_or_default();
            let sleeping = procs.split_whitespace().any(|pid| {
                std::fs::read_to_string(format!("/proc/{pid}/comm"))
                    .is_ok_and(|c| c.trim() == "sleep")
            });
            if sleeping {
                return;
            }
            let children = std::fs::read_dir(&dir).into_iter().flatten().flatten();
            pending.extend(
                children
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| e.path()),
            );
        }
        assert!(Instant::now() < deadline, "no sleep in {}", describe(lease));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Whether a process is in the cgroup `dir` or below it (`cgroup.events`).
fn populated(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("cgroup.events"))
        .is_ok_and(|events| events.lines().any(|line| line == "populated 1"))
}
