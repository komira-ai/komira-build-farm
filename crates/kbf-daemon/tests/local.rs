//! The test-only local runtime: one action, run as a child process from a fresh lease
//! directory, measured, its outputs read back, and the directory removed.

#![cfg(target_os = "linux")]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::cas::digest_of;
use kbf_daemon::usage::usage_of;
use kbf_daemon::{LOCAL_DRIVER, LocalRuntime, Runtime, RuntimeError, Work};
use kbf_proto::reapi::{Action, ActionResult, Command, Digest};
use kbf_types::LeaseId;
use support::memory::{MemoryCas, Spec};
use support::scratch;

struct Local {
    cas: Arc<MemoryCas>,
    scratch: PathBuf,
    runtime: Arc<LocalRuntime<MemoryCas>>,
}

impl Local {
    fn new(name: &str) -> Self {
        let cas = Arc::new(MemoryCas::default());
        let scratch = scratch(name).join("leases");
        let runtime = Arc::new(LocalRuntime::new(Arc::clone(&cas), scratch.clone()));
        Self {
            cas,
            scratch,
            runtime,
        }
    }

    async fn run(&self, seq: u64, action: Digest) -> Result<ActionResult, RuntimeError> {
        self.runtime.run(work(seq, action)).await
    }

    async fn run_spec(&self, seq: u64, spec: &Spec) -> Result<ActionResult, RuntimeError> {
        self.run(seq, spec.store(&self.cas)).await
    }

    fn text(&self, digest: Option<&Digest>) -> String {
        let bytes = self.cas.blob(digest.expect("a digest")).expect("stored");
        String::from_utf8(bytes).expect("UTF-8")
    }

    /// Whether every lease directory is gone.
    fn clean(&self) -> bool {
        std::fs::read_dir(&self.scratch).map_or(true, |mut d| d.next().is_none())
    }
}

fn work(seq: u64, action: Digest) -> Work {
    Work {
        lease_id: LeaseId::new(1, seq),
        kind: "action".to_owned(),
        action_digest: action,
    }
}

/// Catches a runtime that does not run the action's argv in the working directory
/// under the input root, with exactly the Command's environment; that loses the exit
/// code, stdout, stderr or an output file; that does not make an output's parent
/// directory; or that leaves the lease directory behind.
#[tokio::test]
async fn an_action_runs_and_its_outputs_are_stored() {
    let local = Local::new("local-run");
    let mut spec = Spec::sh(
        "cat ../in/data > out/copy; printf '%s' \"$GREETING:$HOME\" > greeting; \
         echo to-stdout; echo to-stderr >&2; exit 3",
    )
    .outputs(&["out/copy", "greeting", "absent"])
    .inputs(&[("in/data", b"input bytes", false)]);
    spec.working_directory = "work".to_owned();
    spec.env = vec![("GREETING".to_owned(), "hello".to_owned())];
    let result = local.run_spec(1, &spec).await.expect("ran");

    assert_eq!(result.exit_code, 3);
    assert_eq!(local.text(result.stdout_digest.as_ref()), "to-stdout\n");
    assert_eq!(local.text(result.stderr_digest.as_ref()), "to-stderr\n");
    let files: Vec<(&str, String)> = result
        .output_files
        .iter()
        .map(|f| (f.path.as_str(), local.text(f.digest.as_ref())))
        .collect();
    // HOME is unset: the daemon's own environment does not leak into the action.
    assert_eq!(
        files,
        [
            ("out/copy", "input bytes".to_owned()),
            ("greeting", "hello:".to_owned())
        ]
    );
    assert!(local.clean(), "the lease directory is removed");
}

/// Catches usage that is not measured or is measured wrongly: CPU time must cover a
/// busy loop and stay within the wall time, wall time must cover a sleep, and peak
/// memory must count the action's own allocation; and REAPI's timestamps out of order.
#[tokio::test]
async fn usage_and_timestamps_are_reported() {
    let local = Local::new("local-usage");
    // About 100 ms of CPU, 64 MiB written into one shell variable, then 300 ms asleep.
    let spec = Spec::sh(
        "i=0; while [ $i -lt 200000 ]; do i=$((i+1)); done; \
         big=$(head -c 67108864 /dev/zero | tr '\\0' x); sleep 0.3",
    );
    let result = local.run_spec(1, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 0);

    let usage = usage_of(&result).expect("usage reported");
    let cpu = usage.cpu_user_micros + usage.cpu_system_micros;
    assert!(usage.cpu_user_micros >= 20_000, "{usage:?}");
    assert!(cpu <= usage.wall_micros, "{usage:?}");
    assert!(usage.wall_micros >= 300_000, "{usage:?}");
    assert!(usage.wall_micros < 60_000_000, "{usage:?}");
    assert!(usage.peak_memory_bytes >= 64 << 20, "{usage:?}");
    assert!(usage.peak_memory_bytes < 8 << 30, "{usage:?}");

    let m = result.execution_metadata.expect("metadata");
    let order = [
        &m.worker_start_timestamp,
        &m.input_fetch_start_timestamp,
        &m.input_fetch_completed_timestamp,
        &m.execution_start_timestamp,
        &m.execution_completed_timestamp,
        &m.output_upload_start_timestamp,
        &m.output_upload_completed_timestamp,
        &m.worker_completed_timestamp,
    ]
    .map(|t| {
        let t = t.as_ref().expect("timestamp set");
        (t.seconds, t.nanos)
    });
    assert!(order.is_sorted(), "{order:?}");
    let ran = (order[4].0 - order[3].0) * 1_000_000_000 + i64::from(order[4].1 - order[3].1);
    assert!(ran >= 300_000_000, "execution took {ran} ns");
}

/// Catches a process a signal ended reported as a success (or with a status the shell
/// would not give it).
#[tokio::test]
async fn a_signal_is_reported_as_128_plus_its_number() {
    let local = Local::new("local-signal");
    let result = local
        .run_spec(1, &Spec::sh("kill -9 $$"))
        .await
        .expect("ran");
    assert_eq!(result.exit_code, 137);
}

/// Catches a program path read relative to the working directory rather than the
/// input root, an executable bit lost on the way in, and a bare program name not
/// looked up in the Command's PATH.
#[tokio::test]
async fn programs_are_found_as_reapi_says() {
    let local = Local::new("local-program");
    let mut relative = Spec::sh("").inputs(&[("tools/hi", b"#!/bin/sh\necho hi\n", true)]);
    relative.argv = vec!["tools/hi".to_owned()];
    relative.working_directory = "deep/er".to_owned();
    let result = local.run_spec(1, &relative).await.expect("ran");
    assert_eq!(result.exit_code, 0);
    assert_eq!(local.text(result.stdout_digest.as_ref()), "hi\n");

    let mut bare = Spec::sh("");
    bare.argv = vec!["sh".to_owned(), "-c".to_owned(), "echo bare".to_owned()];
    bare.env = vec![("PATH".to_owned(), "/usr/bin:/bin".to_owned())];
    let result = local.run_spec(2, &bare).await.expect("ran");
    assert_eq!(local.text(result.stdout_digest.as_ref()), "bare\n");
}

/// Catches an action that breaks a rule being run anyway, or reported as the farm's
/// failure (it would be retried elsewhere) instead of the client's.
#[tokio::test]
async fn invalid_actions_are_the_clients_error() {
    let local = Local::new("local-invalid");
    let cas = &local.cas;
    let command = cas.message(&Spec::sh("true").command());
    let root = cas.message(&kbf_proto::reapi::Directory::default());
    let no_command = cas.message(&Action {
        input_root_digest: Some(root.clone()),
        ..Action::default()
    });
    let no_root = cas.message(&Action {
        command_digest: Some(command.clone()),
        ..Action::default()
    });
    let no_args = cas.message(&Action {
        command_digest: Some(cas.message(&Command::default())),
        input_root_digest: Some(root),
        ..Action::default()
    });
    let garbage = cas.insert(vec![0xff, 0xff]);
    let mut up = Spec::sh("true");
    up.working_directory = "../up".to_owned();
    let mut escape = Spec::sh("true");
    escape.outputs = vec!["/etc/passwd".to_owned()];
    let mut through_link = Spec::sh("true").outputs(&["link/out"]);
    through_link.symlinks = vec![("link", "/tmp")];
    let cases = [
        (no_command, "names no Command"),
        (no_root, "names no input root"),
        (no_args, "no arguments"),
        (garbage, "does not decode"),
        (up.store(cas), "working directory"),
        (escape.store(cas), "output path"),
        (through_link.store(cas), "other than a directory"),
    ];
    for (seq, (action, says)) in (1..).zip(cases) {
        let outcome = local.run(seq, action).await;
        assert!(
            matches!(&outcome, Err(RuntimeError::Invalid(why)) if why.contains(says)),
            "{says}: {outcome:?}"
        );
    }
    assert!(local.clean());
}

/// Catches a missing input reported as anything but the missing blob (the client
/// could not tell what to upload), and a corrupt one trusted.
#[tokio::test]
async fn missing_and_corrupt_inputs_fail_cleanly() {
    let local = Local::new("local-missing");
    let spec = Spec::sh("cat data").inputs(&[("data", b"needed", false)]);
    let action = spec.store(&local.cas);
    let data = digest_of(b"needed");
    local.cas.remove(&data);
    let outcome = local.run(1, action.clone()).await;
    let want = format!("{}/6", data.hash);
    assert!(
        matches!(&outcome, Err(RuntimeError::MissingBlob(blob)) if *blob == want),
        "{outcome:?}"
    );
    assert_eq!(
        outcome.expect_err("missing").to_string(),
        format!("blob {want} is not in the CAS")
    );

    let absent_action = digest_of(b"no such action");
    assert!(matches!(
        local.run(2, absent_action).await,
        Err(RuntimeError::MissingBlob(_))
    ));

    local.cas.corrupt(&data, b"tampered".to_vec());
    let outcome = local.run(3, action).await;
    assert!(
        matches!(&outcome, Err(RuntimeError::Failed(why)) if why.contains("failed verification")),
        "{outcome:?}"
    );
    assert!(local.clean());
}

/// Catches a program that cannot be started reported as a run, and a lease whose
/// directory cannot be made (the scratch path is a file, or the lease's directory is
/// already there) run anyway, or that directory removed although it was not this
/// run's.
#[tokio::test]
async fn setup_failures_are_the_farms() {
    let local = Local::new("local-setup");
    let mut nothing = Spec::sh("");
    nothing.argv = vec!["/nonexistent/program".to_owned()];
    let outcome = local.run_spec(1, &nothing).await;
    assert!(
        matches!(&outcome, Err(RuntimeError::Failed(why)) if why.contains("/nonexistent/program")),
        "{outcome:?}"
    );

    let taken = local.scratch.join("lease-1-2");
    std::fs::create_dir_all(&taken).expect("plant");
    let outcome = local.run_spec(2, &Spec::sh("true")).await;
    assert!(
        matches!(&outcome, Err(RuntimeError::Failed(why)) if why.contains("lease-1-2")),
        "{outcome:?}"
    );
    assert!(taken.is_dir(), "a directory that was not the run's is kept");

    let file = scratch("local-setup-file").join("file");
    std::fs::write(&file, b"").expect("file");
    let blocked = LocalRuntime::new(Arc::clone(&local.cas), file.join("leases"));
    let outcome = blocked
        .run(work(3, Spec::sh("true").store(&local.cas)))
        .await;
    assert!(
        matches!(&outcome, Err(RuntimeError::Failed(why)) if why.contains("leases")),
        "{outcome:?}"
    );
}

/// Catches a lease directory that cannot be removed being ignored: the node would
/// fill up, and the next lease could meet the last one's files.
#[tokio::test]
async fn a_lease_directory_left_behind_fails_the_lease() {
    let local = Local::new("local-dirty");
    let spec = Spec::sh("mkdir stuck && touch stuck/file && chmod 555 stuck");
    let outcome = local.run_spec(1, &spec).await;
    assert!(
        matches!(&outcome, Err(RuntimeError::Failed(why)) if why.contains("could not be removed")),
        "{outcome:?}"
    );
    // So the scratch directory can be removed.
    let stuck = local.scratch.join("lease-1-1/root/stuck");
    std::fs::set_permissions(stuck, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

/// Catches a kill that does not stop the action (its process group), that is not
/// reported as Killed, or that leaves the lease directory; and a kill of a lease that
/// is not running doing anything.
#[tokio::test]
async fn a_kill_stops_the_action() {
    let local = Local::new("local-kill");
    local.runtime.kill(LeaseId::new(9, 9)).await;
    let marker = scratch("local-kill-marker").join("survived");
    let script = format!("sleep 1; touch {}", marker.display());
    let action = Spec::sh(&script).store(&local.cas);
    let runtime = Arc::clone(&local.runtime);
    let run = tokio::spawn(async move { runtime.run(work(1, action)).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    local.runtime.kill(LeaseId::new(1, 1)).await;
    let outcome = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("the run ends promptly")
        .expect("no panic");
    assert!(matches!(outcome, Err(RuntimeError::Killed)), "{outcome:?}");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!marker.exists(), "the action ran on after the kill");
    assert!(local.clean());
}

/// Catches the runtime claiming a driver name or lease kinds it does not have.
#[test]
fn it_serves_actions_as_the_local_driver() {
    let runtime = LocalRuntime::new(Arc::new(MemoryCas::default()), PathBuf::from("/unused"));
    assert_eq!(runtime.driver(), LOCAL_DRIVER);
    assert!(runtime.serves("action"));
    assert!(!runtime.serves("whole_machine"));
}
