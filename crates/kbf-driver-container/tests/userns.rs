//! Who owns a lease's files, against the stand-in `crun` (`support/fake.rs`).
//!
//! Every container's root is a subordinate id, not the daemon's user. The driver gives
//! the overlay's directories to that id before the container runs and takes them back
//! once it has exited, before reading the outputs. The fake records each hand-over (a
//! helper container) and changes no owner (a test runs as one user); while the overlay
//! is the container's once the run has ended, it makes the upper directory unreadable,
//! as a stranger's private files would be, until the hand-back. `tests/crun.rs` checks
//! the ids on real `crun`.

mod support;

use kbf_daemon::RuntimeError;
use support::fake::{Fake, image};
use support::swap_kill::Logs;
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
        .run(1, &spec, &["write-lines", "$UPPER/out/f", "f"])
        .await
        .expect("ran");
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.output_files.len(), 1, "{result:?}");
    let lease = fake.lease_dir(1);
    let dirs = ["root", "upper", "work", "rw"]
        .map(|d| lease.join(d).display().to_string())
        .join(" ");
    let events: Vec<String> = fake
        .events()
        .into_iter()
        .filter(|e| e.starts_with("chown ") || e == "run ended")
        .collect();
    assert_eq!(
        events,
        [
            format!("chown 1:1 {dirs}"),
            "run ended".to_owned(),
            format!("chown 0:0 {dirs}"),
        ]
    );
    let calls = fake.calls();
    let first_give = calls.iter().position(|c| c == "helper give");
    let run = calls.iter().position(|c| c == "run");
    assert!(first_give < run, "{calls:?}");
    fake.assert_clean(1);
}

/// The `inputs phases:` line's `chown_ms` is the hand-over's time: with the fake's
/// helper giving to the container's owner sleeping 200 ms, it is at least 200
/// (a lower bound only: a loaded machine adds time, never removes it). Catches the
/// timer read before `give_overlay` runs (0), the hand-over timed into another field,
/// and no line or more than one per lease.
#[tokio::test]
async fn the_inputs_phases_line_times_the_hand_over() {
    let fake = Fake::new("chown-sleeps");
    fake.knob("chown-sleeps", "1:1");
    let logs = Logs::capture();
    let result = fake
        .run(1, &Spec::new(&image(), "unused"), &["exit", "0"])
        .await
        .expect("ran");
    assert_eq!(result.exit_code, 0);
    let text = logs.text();
    let lines: Vec<&str> = text
        .lines()
        .filter_map(|l| l.split_once("inputs phases: ").map(|(_, fields)| fields))
        .collect();
    let [fields] = lines[..] else {
        panic!("not one inputs phases line:\n{text}");
    };
    let chown_ms: u64 = fields
        .split(' ')
        .find_map(|f| f.strip_prefix("chown_ms="))
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("no chown_ms in {fields:?}"));
    assert!(chown_ms >= 200, "{fields}");
    fake.assert_clean(1);
}

/// Catches a failed hand-over being ignored. Before the container: the action would
/// run unable to write, so nothing is run. After it: the outputs would be read
/// as a stranger, so the lease fails rather than report what it could see. Either
/// way the lease is an infrastructure failure and leaves nothing.
#[tokio::test]
async fn a_failed_hand_over_fails_the_lease_and_leaves_nothing() {
    let fake = Fake::new("chown-fails");
    let spec = Spec::new(&image(), "unused");

    fake.knob("chown-fails", "1:1");
    let outcome = fake.run(1, &spec, &["exit", "0"]).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why))
            if why.contains("chown refused")),
        "{outcome:?}"
    );
    assert_eq!(fake.starts(), 0);
    fake.assert_clean(1);

    fake.knob("chown-fails", "0:0");
    let outcome = fake.run(2, &spec, &["exit", "0"]).await;
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
/// container may still be stopping, and its files are removed by a helper container
/// instead (the clean step's fallback).
#[tokio::test]
async fn a_timed_out_lease_is_not_handed_back() {
    let fake = Fake::new("chown-timeout");
    let mut spec = Spec::new(&image(), "unused");
    spec.timeout = Some(std::time::Duration::from_millis(200));
    let outcome = fake.run(1, &spec, &["sleep", "30"]).await;
    assert!(
        matches!(outcome, Err(RuntimeError::TimedOut { .. })),
        "{outcome:?}"
    );
    let chowns = chowns(&fake);
    assert_eq!(chowns.len(), 1, "{chowns:?}");
    assert!(chowns[0].starts_with("chown 1:1 "), "{chowns:?}");
    fake.assert_clean(1);
}
