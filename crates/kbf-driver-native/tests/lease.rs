//! One lease end to end through the native driver: inputs in, the action run with its
//! own environment, outputs and captured output back, the lease directory gone.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::{Runtime, RuntimeError};
use kbf_driver_native::{DRIVER, KIND, NativeRuntime, network};
use support::{MemoryCas, Spec, config, no_leases, run, runtime, scratch, stderr, stdout, work};

/// Catches: the exit code, stdout or stderr lost or swapped; an input not written or
/// not executable; outputs not collected relative to the working directory; and the
/// lease directory left behind.
#[tokio::test]
async fn an_action_runs_and_its_results_come_back() {
    let dir = scratch("runs");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    assert_eq!(rt.driver(), DRIVER);
    assert!(rt.serves(KIND) && !rt.serves("whole_machine"));
    let script =
        "../tool; echo err >&2; mkdir -p out/sub; echo f > out/sub/f; echo x > file; exit 3";
    let mut spec = Spec::sh(script).outputs(&["out", "file", "missing"]);
    spec.working_directory = "w".to_owned();
    spec.inputs = vec![("tool".to_owned(), b"#!/bin/sh\necho from-tool\n".to_vec())];
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 3);
    assert_eq!(stdout(&cas, &result), "from-tool\n");
    assert_eq!(stderr(&cas, &result), "err\n");
    assert_eq!(result.output_files.len(), 1);
    assert_eq!(result.output_files[0].path, "file");
    assert_eq!(result.output_directories.len(), 1);
    let digest = result.output_directories[0]
        .tree_digest
        .as_ref()
        .expect("tree");
    let tree = support::tree(&cas, digest);
    assert_eq!(tree.root.expect("root").directories[0].name, "sub");
    let metadata = result.execution_metadata.expect("metadata");
    let at = |t: Option<prost_types::Timestamp>| t.map(|t| (t.seconds, t.nanos)).expect("stamped");
    assert!(at(metadata.execution_start_timestamp) <= at(metadata.execution_completed_timestamp));
    assert!(metadata.worker_completed_timestamp.is_some());
    assert!(no_leases(&config), "the lease directory is removed");
}

/// Catches: the daemon's environment leaking into an action (its PATH, HOME, anything
/// set on the node), the Command's variables not set, or the lease's own variables
/// missing or pointing outside the lease directory.
#[tokio::test]
async fn the_environment_is_the_commands_and_the_leases_own() {
    let dir = scratch("env");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let spec = Spec::argv(&["/usr/bin/env"]).env("ONLY", "this");
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    let lease = config.scratch.join("lease-1-1");
    let mut want: Vec<String> = [
        ("CLANG_MODULE_CACHE_PATH", "cache/clang/ModuleCache"),
        ("HOME", "home"),
        ("TMPDIR", "tmp"),
        ("XDG_CACHE_HOME", "cache"),
    ]
    .iter()
    .map(|(name, sub)| format!("{name}={}", lease.join(sub).display()))
    .collect();
    want.push("ONLY=this".to_owned());
    let mut got: Vec<String> = stdout(&cas, &result).lines().map(str::to_owned).collect();
    got.sort();
    want.sort();
    assert_eq!(got, want);
}

/// Catches: a lease's home, temporary or cache directory missing (a tool writing there
/// fails), shared between leases (what one action leaves is read by the next), or a
/// Command's own `HOME` or `TMPDIR` overridden by the lease's.
#[tokio::test]
async fn each_lease_has_its_own_home_tmp_and_cache() {
    let dir = scratch("home");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let script = "for d in \"$HOME\" \"$TMPDIR\" \"$XDG_CACHE_HOME\" \"$CLANG_MODULE_CACHE_PATH\"; do \
                  if [ -e \"$d/left\" ]; then echo seen \"$d\"; fi; echo x > \"$d/left\" || exit 1; done; \
                  echo ok";
    for seq in [1, 2] {
        let result = run(&rt, &cas, seq, &Spec::sh(script)).await.expect("ran");
        assert_eq!(result.exit_code, 0, "{}", stderr(&cas, &result));
        assert_eq!(stdout(&cas, &result), "ok\n", "lease {seq}");
    }
    assert!(no_leases(&config), "the lease directories are removed");

    let own = Spec::sh("echo \"$HOME $TMPDIR\"")
        .env("HOME", "/the/commands/home")
        .env("TMPDIR", "/the/commands/tmp");
    let result = run(&rt, &cas, 3, &own).await.expect("ran");
    assert_eq!(
        stdout(&cas, &result),
        "/the/commands/home /the/commands/tmp\n"
    );
}

/// Catches: an action that names an Xcode run without its `DEVELOPER_DIR`, or with the
/// Command's own instead (the scheduler matched the build the property names), a
/// build the node lacks run anyway with no Xcode or another one, and the node's Xcodes
/// missing from what the driver reports.
#[tokio::test]
async fn an_action_that_names_an_xcode_runs_with_it() {
    let dir = scratch("xcode");
    let mut config = config(&dir);
    config.xcodes = [
        ("16C5032a", "/Apps/Xcode_16.2.app/Contents/Developer"),
        ("16E140", "/Apps/Xcode_16.3.app/Contents/Developer"),
    ]
    .into_iter()
    .map(|(build, path)| (build.to_owned(), std::path::PathBuf::from(path)))
    .collect();
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let reported = rt.capabilities();
    for build in ["16C5032a", "16E140"] {
        assert!(
            reported.contains(&("xcode".to_owned(), build.to_owned())),
            "{reported:?}"
        );
    }
    let echo = Spec::sh("echo \"${DEVELOPER_DIR-unset}\"").env("DEVELOPER_DIR", "/own");
    let named = echo.clone().property("XCode", "16E140");
    let result = run(&rt, &cas, 1, &named).await.expect("ran");
    assert_eq!(
        stdout(&cas, &result),
        "/Apps/Xcode_16.3.app/Contents/Developer\n"
    );
    let result = run(&rt, &cas, 2, &echo).await.expect("ran");
    assert_eq!(
        stdout(&cas, &result),
        "/own\n",
        "no xcode: the Command's own"
    );
    let lacking = echo.property("xcode", "15F31d");
    let error = run(&rt, &cas, 3, &lacking)
        .await
        .expect_err("a build it lacks");
    assert!(
        matches!(&error, RuntimeError::Failed(why) if why.contains("15F31d")),
        "{error:?}"
    );
    assert!(no_leases(&config), "the lease directories are removed");
}

/// Catches: a malformed action run anyway or reported as the farm's failure, and a
/// missing input reported as anything but a missing blob.
#[tokio::test]
async fn bad_actions_are_the_clients_errors() {
    let dir = scratch("bad");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let cases = [
        (Spec::argv(&[]), "no arguments"),
        (Spec::sh("true").outputs(&["../escape"]), "must be relative"),
        (Spec::sh("true").property("network", "maybe"), "network"),
        (
            {
                let mut s = Spec::sh("true");
                s.working_directory = "/abs".to_owned();
                s
            },
            "working directory",
        ),
    ];
    for (i, (spec, says)) in cases.into_iter().enumerate() {
        let outcome = run(&rt, &cas, i as u64 + 1, &spec).await;
        assert!(
            matches!(&outcome, Err(RuntimeError::Invalid(why)) if why.contains(says)),
            "{says}: {outcome:?}"
        );
    }
    let missing = kbf_daemon::cas::digest_of(b"never stored");
    let outcome = rt.run(work(9, missing.clone(), 0)).await;
    assert!(
        matches!(outcome, Err(RuntimeError::MissingBlob(_))),
        "{outcome:?}"
    );
    for (field, says) in [
        ("command", "names no Command"),
        ("root", "names no input root"),
    ] {
        let mut action = kbf_proto::reapi::Action {
            command_digest: Some(missing.clone()),
            input_root_digest: Some(missing.clone()),
            ..kbf_proto::reapi::Action::default()
        };
        if field == "command" {
            action.command_digest = None;
        } else {
            action.input_root_digest = None;
        }
        let digest = cas.insert(prost::Message::encode_to_vec(&action));
        let outcome = rt.run(work(11, digest, 0)).await;
        assert!(
            matches!(&outcome, Err(RuntimeError::Invalid(why)) if why.contains(says)),
            "{outcome:?}"
        );
    }
    let negative = Spec::sh("true");
    let mut action: kbf_proto::reapi::Action =
        prost::Message::decode(cas.blob(&negative.store(&cas)).as_slice()).expect("action");
    action.timeout = Some(prost_types::Duration {
        seconds: -1,
        nanos: 0,
    });
    let digest = cas.insert(prost::Message::encode_to_vec(&action));
    let outcome = rt.run(work(12, digest, 0)).await;
    assert!(
        matches!(&outcome, Err(RuntimeError::Invalid(why)) if why.contains("negative")),
        "{outcome:?}"
    );
    // A lease directory that is already there is not this run's to use.
    std::fs::create_dir_all(config.scratch.join("lease-1-14")).expect("mkdir");
    let outcome = run(&rt, &cas, 14, &Spec::sh("true")).await;
    assert!(
        matches!(&outcome, Err(RuntimeError::Failed(why)) if why.contains("lease-1-14")),
        "{outcome:?}"
    );
    let outcome = run(&rt, &cas, 13, &Spec::argv(&["/nonexistent/program"])).await;
    assert!(
        matches!(&outcome, Err(RuntimeError::Failed(why)) if why.contains("/nonexistent/program")),
        "{outcome:?}"
    );
    assert!(no_leases(&config));
}

/// Catches: a lease directory the action locked (no permissions left on its
/// directories, files without any) or made deep surviving the clean; and a clean
/// failure (the scratch root made unwritable) reported as success.
#[tokio::test]
async fn the_lease_directory_goes_whatever_the_action_left() {
    let dir = scratch("clean");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let script = "mkdir -p a/b/c && echo x > a/b/c/f && chmod 000 a/b/c/f a/b/c && chmod 500 a/b && chmod 100 a; \
        i=0; while [ $i -lt 300 ]; do mkdir d && cd d || exit 1; i=$((i+1)); done";
    let result = run(&rt, &cas, 1, &Spec::sh(script)).await.expect("ran");
    assert_eq!(result.exit_code, 0);
    assert!(no_leases(&config));
}

/// Catches the reviewer's brick on a Mac: an action that marks its output immutable
/// (`chflags uchg`, which needs no privilege) leaving a lease directory that will not
/// go, and the same directory, left by a crashed daemon, stopping the next start.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn what_an_action_made_immutable_does_not_stay() {
    let dir = scratch("uchg");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let script = "mkdir -p out/d && touch out/x out/d/y && chflags uchg out/x out/d/y out/d out";
    let result = run(&rt, &cas, 1, &Spec::sh(script)).await.expect("ran");
    assert_eq!(result.exit_code, 0, "{}", stderr(&cas, &result));
    assert!(no_leases(&config));

    let left = config.scratch.join("lease-9-9");
    std::fs::create_dir_all(left.join("root/out")).expect("mkdir");
    std::fs::write(left.join("root/out/x"), b"x").expect("write");
    let status = std::process::Command::new("/usr/bin/chflags")
        .args(["uchg"])
        .arg(left.join("root/out/x"))
        .arg(left.join("root/out"))
        .arg(&left)
        .status()
        .expect("chflags");
    assert!(status.success());
    let _restarted = runtime(config.clone(), &cas);
    assert!(no_leases(&config), "removed at start, not moved aside");
}

/// Catches the ACL route to the same brick on a Mac: an action that denies everyone,
/// itself included, deletion of an output and of its directory's entries (which no
/// permission bit undoes) leaving a lease directory that will not go.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn what_an_action_locked_with_an_acl_does_not_stay() {
    let dir = scratch("acl");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let script = "mkdir out && touch out/x && chmod +a 'everyone deny delete' out/x \
        && chmod +a 'everyone deny delete_child' out && ! rm -f out/x";
    let result = run(&rt, &cas, 1, &Spec::sh(script)).await.expect("ran");
    assert_eq!(result.exit_code, 0, "{}", stderr(&cas, &result));
    assert!(no_leases(&config));
}

/// Catches: lease directories a crashed daemon left not removed at start (the node
/// would fill up), or files beside them removed.
#[tokio::test]
async fn leftovers_are_removed_at_start() {
    let dir = scratch("leftovers");
    let config = config(&dir);
    let left = config.scratch.join("lease-7-7");
    std::fs::create_dir_all(left.join("root/locked")).expect("mkdir");
    std::fs::write(config.scratch.join("keep"), b"not a lease").expect("write");
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    assert!(!left.exists());
    assert!(config.scratch.join("keep").exists());
    assert_eq!(
        rt.capabilities(),
        [(
            network::CAPABILITY.to_owned(),
            config.isolation.name().to_owned()
        )]
    );
    std::fs::write(dir.join("file"), b"x").expect("write");
    let mut bad = config.clone();
    bad.scratch = dir.join("file").join("leases");
    assert!(NativeRuntime::new(bad, Arc::clone(&cas)).is_err());
}

/// Catches: a kill during the input fetch not stopping the lease, or returning before
/// the lease directory is gone; and a kill of a lease that is not running blocking.
#[tokio::test]
async fn a_kill_during_the_fetch_stops_the_lease() {
    let dir = scratch("kill-fetch");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas {
        delay: Some(Duration::from_millis(200)),
        ..MemoryCas::default()
    });
    let rt = Arc::new(runtime(config.clone(), &cas));
    rt.kill(kbf_types::LeaseId::new(5, 5)).await;
    let action = Spec::sh("true").store(&cas);
    let running = tokio::spawn({
        let rt = Arc::clone(&rt);
        async move { rt.run(work(1, action, 0)).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    rt.kill(kbf_types::LeaseId::new(1, 1)).await;
    let outcome = running.await.expect("run task");
    assert!(matches!(outcome, Err(RuntimeError::Killed)), "{outcome:?}");
    assert!(no_leases(&config));
}

/// Catches, on macOS, an action that can take write permission from the scratch root
/// (every later lease on the node would then fail its clean): the sandbox refuses the
/// chmod, and the lease ends clean.
#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "mutant run: let cargo reach tests/sandbox.rs"]
async fn a_sandboxed_action_cannot_lock_the_scratch_root() {
    let dir = scratch("lock-root");
    let config = config(&dir);
    assert!(matches!(config.isolation, network::Isolation::Sandbox(_)));
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let spec = Spec::sh("chmod 555 ../.. 2>/dev/null; echo $?");
    let result = run(&rt, &cas, 1, &spec).await.expect("ran, and cleaned");
    assert_ne!(stdout(&cas, &result).trim(), "0", "the chmod went through");
    assert!(no_leases(&config), "the lease directory is removed");
}

/// Catches a lease directory that could not be removed reported as success (a node
/// filling up unseen), and a failed clean hiding the action's own outcome. The action
/// takes write permission from the scratch root, so its lease directory cannot be
/// unlinked from it. It runs unsandboxed: on macOS the sandbox refuses that chmod
/// (`a_sandboxed_action_cannot_lock_the_scratch_root`).
#[tokio::test]
async fn a_lease_directory_that_stays_fails_the_lease() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("clean-fails");
    let mut config = config(&dir);
    config.isolation = network::Isolation::None;
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let reopen = || {
        std::fs::set_permissions(&config.scratch, std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
    };
    let outcome = run(&rt, &cas, 1, &Spec::sh("chmod 555 ../..")).await;
    reopen();
    assert!(
        matches!(&outcome, Err(RuntimeError::Failed(why)) if why.starts_with("clean: ")),
        "{outcome:?}"
    );
    let spec = Spec::sh("chmod 555 ../..; sleep 300").timeout(Duration::from_millis(300));
    let outcome = run(&rt, &cas, 2, &spec).await;
    reopen();
    assert!(
        matches!(outcome, Err(RuntimeError::TimedOut)),
        "{outcome:?}"
    );
}
