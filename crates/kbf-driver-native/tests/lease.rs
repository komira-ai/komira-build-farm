//! One lease end to end through the native driver: inputs in, the action run with its
//! own environment, outputs and captured output back, the lease directory gone.

mod support;
// In this binary, not one of its own: the driver's generic code is counted by its best single
// instantiation, one per test binary, so the simulator paths must run where the rest does.
#[path = "lease/cache.rs"]
mod cache;
#[path = "lease/simulator.rs"]
mod simulator;

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
    let steps = "run ../tool from-tool ; echo --stderr err ; mkdir out/sub ; \
                 append out/sub/f f ; append file x ; exit 3";
    let mut spec = Spec::act(steps).outputs(&["out", "file", "missing"]);
    spec.working_directory = "w".to_owned();
    // An input that runs only if it is written executable: its interpreter is the act
    // steps' link, and it echoes its path and arguments.
    let tool = format!("#!{} echo\n", support::act().display());
    spec.inputs = vec![("tool".to_owned(), tool.into_bytes())];
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 3);
    assert_eq!(stdout(&cas, &result), "../tool from-tool\n");
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
        ("CFFIXED_USER_HOME", "home"),
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
    // The action prints the five variables, which must name its lease's own
    // directories, then makes `left` in each with `symlink`, which fails (exit 1,
    // naming it) where `left` is there already or the directory is missing.
    let vars = [
        "HOME",
        "CFFIXED_USER_HOME",
        "TMPDIR",
        "XDG_CACHE_HOME",
        "CLANG_MODULE_CACHE_PATH",
    ];
    for seq in [1, 2] {
        let lease = config.scratch.join(format!("lease-1-{seq}"));
        let dirs: Vec<String> = ["home", "tmp", "cache", "cache/clang/ModuleCache"]
            .iter()
            .map(|sub| lease.join(sub).display().to_string())
            .collect();
        // The directory each variable names, in the order of `vars`.
        let named: Vec<String> = [0, 0, 1, 2, 3].iter().map(|&i| dirs[i].clone()).collect();
        let steps: Vec<String> = vars
            .iter()
            .map(|var| format!("env {var}"))
            .chain(dirs.iter().map(|dir| format!("symlink x {dir}/left")))
            .collect();
        let spec = Spec::act(&steps.join(" ; "));
        let result = run(&rt, &cas, seq, &spec).await.expect("ran");
        assert_eq!(result.exit_code, 0, "{}", stderr(&cas, &result));
        let printed: Vec<String> = stdout(&cas, &result).lines().map(str::to_owned).collect();
        assert_eq!(printed, named, "lease {seq}");
    }
    assert!(no_leases(&config), "the lease directories are removed");

    let own = Spec::act("env HOME ; env TMPDIR")
        .env("HOME", "/the/commands/home")
        .env("TMPDIR", "/the/commands/tmp");
    let result = run(&rt, &cas, 3, &own).await.expect("ran");
    assert_eq!(
        stdout(&cas, &result),
        "/the/commands/home\n/the/commands/tmp\n"
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
    let echo = Spec::act("env DEVELOPER_DIR unset").env("DEVELOPER_DIR", "/own");
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
        (
            Spec::act("exit 0").outputs(&["../escape"]),
            "must be relative",
        ),
        (Spec::act("exit 0").property("network", "maybe"), "network"),
        (
            {
                let mut s = Spec::act("exit 0");
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
    let negative = Spec::act("exit 0");
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
    let outcome = run(&rt, &cas, 14, &Spec::act("exit 0")).await;
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

/// Catches a program missing from, or not executable in, the input root answered as
/// the farm's fault (`RuntimeError::Failed`, the client's `INTERNAL`): a relative path
/// and a bare name on a `PATH` of relative entries name the input root only, so they
/// are the action's result, exit 127 (not there) or 126 (there, not an executable
/// file), with kbf's message naming the program and the `PATH`. A bare name missing
/// from an absolute `PATH`, or the default one, may be this node's drift and stays
/// the farm's, its message naming the `PATH` searched.
#[tokio::test]
async fn a_program_not_in_the_input_root_is_the_actions_failure() {
    let dir = scratch("program");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let in_w = |argv: &[&str]| {
        let mut spec = Spec::argv(argv);
        spec.working_directory = "w".to_owned();
        spec
    };
    let cases = [
        (in_w(&["./missing"]), 127, "`./missing`"),
        (
            in_w(&["tool"]).env("PATH", "bin:tools"),
            127,
            "PATH \"bin:tools\"",
        ),
        (in_w(&["../w"]), 126, "not an executable file"),
    ];
    for (seq, (spec, code, says)) in (1..).zip(cases) {
        let result = run(&rt, &cas, seq, &spec).await;
        let result = result.unwrap_or_else(|e| panic!("{:?}: {e:?}", spec.argv));
        let stderr = stderr(&cas, &result);
        assert_eq!(result.exit_code, code, "{:?}: {stderr}", spec.argv);
        assert!(stderr.starts_with("kbf: "), "{stderr}");
        assert!(stderr.contains(says), "{stderr}");
        assert_eq!(stdout(&cas, &result), "");
    }
    for (seq, spec, says) in [
        (
            8,
            Spec::argv(&["no-such-tool-kbf"]),
            "the default PATH \"/usr/bin:/bin\"",
        ),
        (
            9,
            Spec::argv(&["no-such-tool-kbf"]).env("PATH", "bin:/nonexistent"),
            "PATH \"bin:/nonexistent\"",
        ),
    ] {
        let outcome = run(&rt, &cas, seq, &spec).await;
        assert!(
            matches!(&outcome, Err(RuntimeError::Failed(why))
                if why.contains("`no-such-tool-kbf`") && why.contains(says)),
            "{outcome:?}"
        );
    }
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
    let deep = vec!["d"; 300].join("/");
    let steps = format!(
        "mkdir a/b/c ; append a/b/c/f x ; chmod 000 a/b/c/f ; chmod 000 a/b/c ; chmod 500 a/b ; \
         chmod 100 a ; mkdir {deep}"
    );
    let result = run(&rt, &cas, 1, &Spec::act(&steps)).await.expect("ran");
    assert_eq!(result.exit_code, 0, "{}", stderr(&cas, &result));
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
    let action = Spec::act("exit 0").store(&cas);
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
async fn a_sandboxed_action_cannot_lock_the_scratch_root() {
    let dir = scratch("lock-root");
    let config = config(&dir);
    assert!(matches!(config.isolation, network::Isolation::Sandbox(_)));
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let spec = Spec::act("chmod 555 ../..");
    let result = run(&rt, &cas, 1, &spec).await.expect("ran, and cleaned");
    // The act step fails (exit 1) when the chmod is refused.
    assert_eq!(result.exit_code, 1, "the chmod went through");
    assert!(
        stderr(&cas, &result).contains("chmod ../.."),
        "refused elsewhere"
    );
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
    let outcome = run(&rt, &cas, 1, &Spec::act("chmod 555 ../..")).await;
    reopen();
    assert!(
        matches!(&outcome, Err(RuntimeError::Failed(why)) if why.starts_with("clean: ")),
        "{outcome:?}"
    );
    let spec = Spec::act("chmod 555 ../.. ; sleep 300").timeout(Duration::from_millis(300));
    let outcome = run(&rt, &cas, 2, &spec).await;
    reopen();
    assert!(
        matches!(outcome, Err(RuntimeError::TimedOut { .. })),
        "{outcome:?}"
    );
}

/// Catches (issue #45) a timeout that loses what the action wrote to stdout and
/// stderr before it was stopped, or swaps the two; a partial log read with no limit;
/// and one log past the stdio limit failing the lease or taking the other with it
/// (the timeout is still the answer; each stream in turn, so a limit applied to one
/// only shows).
#[tokio::test]
async fn a_timeout_keeps_the_logs_written_before_it() {
    const MAX: u64 = 16;
    let dir = scratch("timeout-logs");
    let mut config = config(&dir);
    config.outputs.max_stdio_bytes = MAX;
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    // With its newline, one byte past the limit.
    let past_bytes = "x".repeat(usize::try_from(MAX).expect("small"));
    let cases = [
        (
            "echo started ; echo --stderr warming".to_owned(),
            Some("started\n"),
            Some("warming\n"),
        ),
        (
            format!("echo {past_bytes} ; echo --stderr kept"),
            None,
            Some("kept\n"),
        ),
        (
            format!("echo kept ; echo --stderr {past_bytes}"),
            Some("kept\n"),
            None,
        ),
    ];
    for (seq, (script, want_out, want_err)) in (1..).zip(cases) {
        // Room for the writes to land before the timeout on a loaded runner.
        let spec = Spec::act(&format!("{script} ; sleep 300")).timeout(Duration::from_secs(1));
        let outcome = run(&rt, &cas, seq, &spec).await;
        let Err(RuntimeError::TimedOut { stdout, stderr }) = outcome else {
            panic!("{script}: {outcome:?}");
        };
        let text = |d: Option<kbf_proto::reapi::Digest>| {
            d.map(|d| String::from_utf8(cas.blob(&d)).expect("UTF-8"))
        };
        assert_eq!(text(stdout).as_deref(), want_out, "{script}");
        assert_eq!(text(stderr).as_deref(), want_err, "{script}");
    }
    assert!(no_leases(&config), "the lease directories are removed");
}
