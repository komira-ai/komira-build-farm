//! Who owns a lease's files, against the stand-in `podman` (`fixtures/fake-podman.sh`).
//!
//! Every container runs with `--userns=nomap`, so its root is a subordinate id, not
//! the daemon's user. The driver gives the overlay's directories to that id before the
//! container is created and takes them back once it has exited, before reading the
//! outputs. The fake records each `podman unshare chown` and changes no owner (a test
//! runs as one user); while the overlay is the container's once `start` has ended, it
//! makes the upper directory unreadable, as a stranger's private files would be, until
//! the hand-back. `tests/podman.rs` checks the ids on real Podman.

mod support;

use kbf_daemon::RuntimeError;
use support::fake::{Fake, image};
use support::{Spec, exists};

/// The `chown` events the fake recorded.
fn chowns(fake: &Fake) -> Vec<String> {
    fake.events()
        .into_iter()
        .filter(|e| e.starts_with("chown "))
        .collect()
}

/// Catches the overlay not being handed to the container's root before the container
/// starts (the action could write nothing: the "drop the pre-chown" mutant), not being
/// handed back after it exits (an output the action made `0600` could not be read:
/// the "drop the chown-back" mutant), handed back after the outputs are read (the
/// fake's upper directory is still unreadable then: the "hand back after collect"
/// mutant), either one naming the wrong directories, and the hand-back running before
/// the container has exited.
#[tokio::test]
async fn the_overlay_is_the_containers_while_it_runs_and_the_daemons_after() {
    let fake = Fake::new("owners");
    let mut spec = Spec::new(&image(), "unused");
    spec.outputs = vec!["out/f".to_owned()];
    let result = fake
        .run(1, &spec, r#"echo f > "$UPPER/out/f""#)
        .await
        .expect("ran");
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.output_files.len(), 1, "{result:?}");
    let lease = fake.lease_dir(1);
    let dirs = ["root", "upper", "work"]
        .map(|d| lease.join(d).display().to_string())
        .join(" ");
    let events: Vec<String> = fake
        .events()
        .into_iter()
        .filter(|e| e.starts_with("chown ") || e == "start ended")
        .collect();
    assert_eq!(
        events,
        [
            format!("chown 1:1 {dirs}"),
            "start ended".to_owned(),
            format!("chown 0:0 {dirs}"),
        ]
    );
    let calls = fake.calls();
    let first_chown = calls.iter().position(|c| c == "unshare chown");
    let create = calls.iter().position(|c| c == "create");
    assert!(first_chown < create, "{calls:?}");
    fake.assert_clean(1);
}

/// Catches a failed hand-over being ignored. Before the container: the action would
/// run unable to write, so nothing is created. After it: the outputs would be read
/// as a stranger, so the lease fails rather than report what it could see. Either
/// way the lease is an infrastructure failure and leaves nothing.
#[tokio::test]
async fn a_failed_hand_over_fails_the_lease_and_leaves_nothing() {
    let fake = Fake::new("chown-fails");
    let spec = Spec::new(&image(), "unused");

    fake.knob("chown-fails", "1:1");
    let outcome = fake.run(1, &spec, "exit 0").await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why))
            if why.starts_with("podman unshare chown: ") && why.contains("chown refused")),
        "{outcome:?}"
    );
    assert!(!fake.calls().contains(&"create".to_owned()));
    fake.assert_clean(1);

    fake.knob("chown-fails", "0:0");
    let outcome = fake.run(2, &spec, "exit 0").await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("chown refused")),
        "{outcome:?}"
    );
    assert!(
        exists(&fake.state.join("removed")),
        "the container was removed"
    );
    fake.assert_clean(2);
}

/// Catches the hand-back running on a lease that never reached collect: a timed-out
/// container may still be stopping, and its files are removed inside Podman's user
/// namespace instead (the clean step's fallback).
#[tokio::test]
async fn a_timed_out_lease_is_not_handed_back() {
    let fake = Fake::new("chown-timeout");
    let mut spec = Spec::new(&image(), "unused");
    spec.timeout = Some(std::time::Duration::from_millis(200));
    let outcome = fake.run(1, &spec, "sleep 30").await;
    assert!(
        matches!(outcome, Err(RuntimeError::TimedOut)),
        "{outcome:?}"
    );
    let chowns = chowns(&fake);
    assert_eq!(chowns.len(), 1, "{chowns:?}");
    assert!(chowns[0].starts_with("chown 1:1 "), "{chowns:?}");
    fake.assert_clean(1);
}
