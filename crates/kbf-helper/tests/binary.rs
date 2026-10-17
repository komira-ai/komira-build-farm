//! `kbf-helper` as the unit runs it, on a scratch root. Nothing here restarts a service or
//! runs a staged binary: the passes see an empty inbox, an inbox entry the stage refuses
//! (no `gh` is pinned under the scratch root), and a release on trial.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use kbf_installer::layout::Layout;
use kbf_installer::state::{Locked, PendingConfirm, State};
use kbf_installer::testkit::{A, B, c};

const BIN: &str = env!("CARGO_BIN_EXE_kbf-helper");

fn root(name: &str) -> PathBuf {
    let (_, dir) = kbf_installer::testkit::host_root(&format!("helper-bin-{name}"));
    fs::create_dir_all(dir.join("var/lib/kbf-updater/inbox/orders")).expect("inbox");
    dir
}

fn layout(root: &Path) -> Layout {
    Layout::linux(kbf_host::Root::new(root).expect("absolute"))
}

fn helper(root: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("runs")
}

fn status(root: &Path) -> serde_json::Value {
    let bytes = fs::read(root.join("var/lib/kbf-updater/status.json")).expect("status.json");
    serde_json::from_slice(&bytes).expect("json")
}

/// Catches a helper that cannot do one pass of its own: it writes a world-readable
/// status.json and exits 0 (here as a non-root user, so with the installer's refusal); with an inbox entry it
/// cannot stage (nothing is pinned under this root), the pass ends and the refusal is the
/// node's to read, not a crash.
#[test]
fn one_pass_reports_the_host_and_a_refused_entry() {
    let root = root("pass");
    let host = layout(&root);
    Locked::acquire(&host)
        .expect("lock")
        .write(&State {
            current: Some(c(A)),
            ..State::default()
        })
        .expect("state");
    fs::create_dir_all(root.join("usr/local/kbf/releases").join(A)).expect("release");
    fs::write(
        root.join("usr/local/kbf/releases").join(A).join("kbf"),
        b"x",
    )
    .expect("binary");

    let run = [
        "run",
        "--passes",
        "1",
        "--interval-secs",
        "0",
        "--unit",
        "kbf.service",
    ];
    let out = helper(&root, &run);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Not root: even a pass that moves no link refuses (the installer's rule), and the
    // node is told why in status.json rather than the helper dying silently.
    assert_eq!(status(&root)["state"], "refused");
    assert!(
        status(&root)["refusal"]
            .as_str()
            .expect("text")
            .contains("as root")
    );
    assert_eq!(status(&root)["installed_commit"], A);
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(root.join("var/lib/kbf-updater/status.json"))
        .expect("meta")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o644, "the node reads it as the service user");

    fs::create_dir(root.join(format!("var/lib/kbf-updater/inbox/{:020}-{B}", 2))).expect("entry");
    let out = helper(&root, &run);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(status(&root)["state"], "refused");
    let why = status(&root)["refusal"].as_str().expect("text").to_owned();
    assert!(
        !why.contains("as root"),
        "the stage's own refusal now: {why}"
    );
    assert!(why.contains(':'), "{why}");
}

/// Catches the start check that does not count: with a release on trial, the unit's
/// ExecStartPre uses one of its starts and starts the window. The revert with none left is
/// the installer's (tested on a host that plays root); here the helper must only refuse to
/// change the links when it is not root.
///
/// A state file that is not readable stops the start (exit 1) instead of guessing.
#[test]
fn the_boot_check_counts_the_starts_and_reverts_the_last() {
    let root = root("boot");
    let host = layout(&root);
    for commit in [A, B] {
        let dir = root.join("usr/local/kbf/releases").join(commit);
        fs::create_dir_all(&dir).expect("release");
        fs::write(dir.join("kbf"), b"x").expect("binary");
    }
    let lock = Locked::acquire(&host).expect("lock");
    lock.write(&State {
        current: Some(c(B)),
        previous: Some(c(A)),
        pending_confirm: Some(PendingConfirm {
            commit: c(B),
            previous: Some(c(A)),
            deadline: None,
            order_id: Some("5".to_owned()),
            starts_left: Some(1),
        }),
        ..State::default()
    })
    .expect("state");
    drop(lock);
    let out = helper(&root, &["boot-check", "--window-secs", "600"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let left = |root: &Path| {
        kbf_installer::state::read_unlocked(&layout(root))
            .expect("reads")
            .expect("there")
    };
    assert_eq!(
        left(&root).pending_confirm.expect("on trial").starts_left,
        Some(0)
    );
    // The last start's turn: reverting changes the links, which only root may do. Run as
    // the test's own user it refuses, and leaves the swap as it was.
    let out = helper(&root, &["boot-check"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("as root"));
    // The state is written first and the links follow (reconcile, as root): the swap is
    // already back on A in the state.
    assert_eq!(left(&root).current, Some(c(A)));

    fs::write(root.join("var/lib/kbf-deploy/state.json"), b"not json").expect("ruin");
    let out = helper(&root, &["boot-check"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("kbf-helper:"));
}

/// Catches a helper that starts without being told what to restart: neither flag is a
/// usage error (exit 2), and both together are too; a relative `--root` is refused.
#[test]
fn the_service_must_be_named_exactly_once() {
    let root = root("flags");
    assert_eq!(
        helper(&root, &["run", "--passes", "1"]).status.code(),
        Some(2)
    );
    let both = [
        "run",
        "--passes",
        "1",
        "--unit",
        "a",
        "--launchd-label",
        "b",
    ];
    assert_eq!(helper(&root, &both).status.code(), Some(2));
    let out = Command::new(BIN)
        .args(["--root", "relative", "boot-check"])
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(1));
}

/// Catches a loop that does not loop or does not stop: two passes with a launchd label
/// (the label is taken, and no unit is needed) end on their own; a helper.json the helper
/// cannot read ends the run with an exit of 1 and a message, not a guess.
///
/// Mutant seen red: `passes` ignored (the run never ends; the test would hang), the
/// decrement dropped.
#[test]
fn passes_end_the_run_and_an_unreadable_memory_stops_it() {
    let root = root("passes");
    Locked::acquire(&layout(&root))
        .expect("lock")
        .write(&State {
            current: Some(c(A)),
            ..State::default()
        })
        .expect("state");
    let two = [
        "run",
        "--passes",
        "2",
        "--interval-secs",
        "0",
        "--launchd-label",
        "com.example.kbf",
    ];
    let out = helper(&root, &two);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::write(root.join("var/lib/kbf-deploy/helper.json"), b"not json").expect("ruin");
    let out = helper(&root, &two);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("helper.json"));
    // A directory the helper cannot write status.json into ends the pass with an error.
    fs::remove_file(root.join("var/lib/kbf-deploy/helper.json")).expect("remove");
    let mut elsewhere = two.to_vec();
    elsewhere.extend(["--update-dir", "/no/such/dir"]);
    let out = helper(&root, &elsewhere);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("kbf-helper:"));
}
