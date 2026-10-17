//! Unit tests of `runtime`: the configuration, the image and timeout of an action,
//! the clean step's waits, and whose an error is.

use kbf_proto::reapi::platform::Property;

use super::*;

fn platform(value: &str) -> Option<Platform> {
    Some(Platform {
        properties: vec![Property {
            name: PROPERTY.to_owned(),
            value: value.to_owned(),
        }],
    })
}

fn image(hex: char) -> String {
    format!("docker://busybox@sha256:{}", hex.to_string().repeat(64))
}

/// Catches the deprecated Command platform overriding the Action's, and an action
/// with no image running in whatever image a node picks.
#[test]
#[allow(deprecated)]
fn the_image_comes_from_the_action_then_the_command() {
    let command = Command {
        platform: platform(&image('c')),
        ..Command::default()
    };
    let action = Action {
        platform: platform(&image('a')),
        ..Action::default()
    };
    let chosen = image_of(&action, &command).expect("image");
    assert_eq!(chosen.digest(), format!("sha256:{}", "a".repeat(64)));
    let chosen = image_of(&Action::default(), &command).expect("image");
    assert_eq!(chosen.digest(), format!("sha256:{}", "c".repeat(64)));
    let none = image_of(&Action::default(), &Command::default());
    assert!(matches!(none, Err(RuntimeError::Invalid(_))));
    let tagged = Action {
        platform: platform("docker://busybox:latest"),
        ..Action::default()
    };
    assert!(matches!(
        image_of(&tagged, &command),
        Err(RuntimeError::Invalid(_))
    ));
}

/// Catches an action's timeout being ignored, a zero timeout meaning "no time at
/// all", and a negative one wrapping to centuries.
#[test]
fn the_timeout_is_the_actions_or_the_default() {
    let default = Duration::from_secs(60);
    let with = |seconds, nanos| Action {
        timeout: Some(prost_types::Duration { seconds, nanos }),
        ..Action::default()
    };
    assert_eq!(timeout_of(&Action::default(), default).ok(), Some(default));
    assert_eq!(timeout_of(&with(0, 0), default).ok(), Some(default));
    assert_eq!(
        timeout_of(&with(2, 500), default).ok(),
        Some(Duration::new(2, 500))
    );
    assert!(matches!(
        timeout_of(&with(-1, 0), default),
        Err(RuntimeError::Invalid(_))
    ));
    assert!(matches!(
        timeout_of(&with(1, -1), default),
        Err(RuntimeError::Invalid(_))
    ));
}

/// Catches a scratch path Podman's `--volume` syntax would split, a cgroup parent
/// Podman would read relative to its own default, and an empty owner (whose sweep
/// would look for containers labelled `kbf.owner=`), and a container limit of 0
/// (to Podman, an unlimited pids count). A usable one starts, its sweep
/// asking a `podman` that knows no container (`true`).
#[test]
fn unusable_configurations_are_refused() {
    let cas = Arc::new(crate::cas::MemoryCas::new());
    let mut ok = PodmanConfig::new(
        PathBuf::from("/nonexistent/scratch"),
        "/actions".to_owned(),
        "node-1".to_owned(),
    );
    ok.podman = PathBuf::from("/bin/true");
    // The Podman engine is what this test starts: only the fake-Podman tests do.
    ok.container_runtime = ContainerRuntime::Podman;
    assert!(PodmanRuntime::new(ok.clone(), Arc::clone(&cas)).is_ok());
    let refused = |config: PodmanConfig| PodmanRuntime::new(config, Arc::clone(&cas)).err();
    for scratch in ["relative", "/a:b", "/a,b"] {
        let mut config = ok.clone();
        config.scratch = PathBuf::from(scratch);
        assert_eq!(
            refused(config),
            Some(ConfigError::Scratch(PathBuf::from(scratch)).into())
        );
    }
    let mut config = ok.clone();
    config.cgroup_parent = "actions".to_owned();
    assert_eq!(
        refused(config),
        Some(ConfigError::CgroupParent("actions".to_owned()).into())
    );
    let mut config = ok.clone();
    config.owner.clear();
    assert_eq!(refused(config), Some(ConfigError::Owner.into()));
    for name in ["pids", "shm", "nofile", "nproc"] {
        let mut config = ok.clone();
        let limits = &mut config.limits;
        match name {
            "pids" => limits.pids = 0,
            "shm" => limits.shm_mib = 0,
            "nofile" => limits.nofile = 0,
            _ => limits.nproc = 0,
        }
        assert_eq!(refused(config), Some(ConfigError::Limit(name).into()));
    }
}

/// Catches a clean that removes the lease while `podman start` still runs (kbf
/// #157): the process is killed and reaped before `reap` returns, and one already
/// waited for is not an error.
#[tokio::test]
async fn reap_kills_and_reaps_a_running_child() {
    let mut child = tokio::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep");
    let pid = child.id().expect("pid");
    reap(&mut child, Duration::from_secs(5), Duration::from_millis(1)).expect("reaped");
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "sleep {pid} still there after reap"
    );
    let mut done = tokio::process::Command::new("true")
        .spawn()
        .expect("spawn true");
    done.wait().await.expect("wait");
    reap(&mut done, Duration::ZERO, Duration::ZERO).expect("an exited child");
}

/// Catches an unbounded wait for a process that does not exit (one stuck in the
/// kernel exits only once it leaves the call), and a failed wait taken as an exit.
#[test]
fn the_wait_for_podman_start_is_bounded() {
    let mut looks = 0;
    let mut never = || {
        looks += 1;
        Ok(None)
    };
    let limit = Duration::from_millis(20);
    let why = wait_exited(&mut never, limit, Duration::from_millis(1)).expect_err("never");
    assert!(why.contains("still runs"), "{why}");
    assert!(looks > 1, "gave up after {looks} looks");
    let mut failing = || Err(std::io::Error::other("no child"));
    let why = wait_exited(&mut failing, Duration::from_secs(5), Duration::ZERO)
        .expect_err("the wait failed");
    assert!(why.starts_with("wait for podman start: "), "{why}");
}

/// Catches a clean that reports success when it could not wait for `podman start`
/// (here another reaper took its exit).
#[tokio::test]
async fn a_clean_that_cannot_wait_for_podman_start_fails() {
    let child = tokio::process::Command::new("true")
        .spawn()
        .expect("spawn true");
    let raw = i32::try_from(child.id().expect("pid")).expect("pid fits");
    let pid = rustix::process::Pid::from_raw(raw);
    rustix::process::waitpid(pid, rustix::process::WaitOptions::empty()).expect("reaped elsewhere");
    let mut lease = lease_left_only(Running::Process(child));
    let why = lease.clean_blocking().expect_err("the wait failed");
    assert!(why.starts_with("wait for podman start: "), "{why}");
}

/// A lease that never got a container, cgroup or scratch directory (all its paths
/// are absent, so removing them succeeds), holding `start` as its `podman start`.
fn lease_left_only(start: Running) -> Lease {
    let mut config = PodmanConfig::new(
        PathBuf::from("/nonexistent-kbf-157/scratch"),
        "/actions".to_owned(),
        "node-1".to_owned(),
    );
    config.cgroup_root = PathBuf::from("/nonexistent-kbf-157/cgroup");
    let id = LeaseId { term: 1, seq: 1 };
    let engine = Engine::Podman(Arc::new(PodmanBackend::new(Podman::new(
        config.podman.clone(),
    ))));
    let mut lease = Lease::new(&engine, &config, id);
    lease.start = Some(start);
    lease
}

/// Catches a clean that does not give a running `podman start` time to exit and be
/// reaped (`REAP_LIMIT` too short), or that only signals or drops it: either leaves
/// it running or a zombie while the lease is removed (kbf #157). This pins the
/// timing the fake Podman's `start-not-reaped-at-rm` check sees only when its test
/// runs alone (another test's runtime may reap a dropped child first).
#[tokio::test]
async fn the_clean_waits_until_podman_start_is_reaped() {
    let start = tokio::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep");
    let pid = start.id().expect("pid");
    let mut lease = lease_left_only(Running::Process(start));
    lease.clean_blocking().expect("cleaned");
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "sleep {pid} still there (running or unreaped) after the clean"
    );
}

/// Catches a clean step whose blocking task panicked being reported without saying
/// it was the clean that failed (the lease then fails INTERNAL with this text).
#[tokio::test]
async fn a_clean_task_that_did_not_finish_names_the_clean() {
    let panicked = tokio::task::spawn_blocking(|| panic!("clean panicked"))
        .await
        .expect_err("the task panicked");
    let why = clean_task_failed(panicked);
    assert!(why.starts_with("clean task: "), "{why}");
}

/// Catches an error reported as the wrong party's: a missing input as anything but
/// `MissingBlob` with its digest (#130: the client never uploads it), a missing
/// blob while storing outputs as the client's to upload, a corrupt input as
/// missing, and a broken rule as the farm's. (Compared as text: a pattern guard
/// would be a branch the coverage ratchet counts and no run takes.)
#[test]
fn errors_are_reported_as_whose_they_are() {
    let shown = |e: RuntimeError| format!("{e:?}");
    let missing = || TreeError::Cas(CasError::Missing("ab/1".to_owned()));
    assert_eq!(shown(tree_error(missing())), "MissingBlob(\"ab/1\")");
    assert_eq!(
        shown(outputs_error(missing())),
        "Failed(\"blob ab/1 is not in the CAS\")"
    );
    let corrupt = TreeError::Cas(CasError::Corrupt("a".into(), "b".into()));
    assert_eq!(
        shown(tree_error(corrupt)),
        "Failed(\"blob a failed verification: its bytes hash to b\")"
    );
    for map in [tree_error, outputs_error] {
        assert_eq!(shown(map(TreeError::Invalid("x".into()))), "Invalid(\"x\")");
    }
}

/// Catches an output path left out of the private copies, or not seen from the input
/// root (the working directory before it, a trailing slash kept as an empty step).
#[test]
fn the_private_paths_are_the_outputs_from_the_input_root() {
    let outputs = ["out/a".to_owned(), "b".to_owned()];
    assert_eq!(private_paths("", &outputs), ["out/a", "b"]);
    assert_eq!(private_paths("pkg/", &outputs), ["pkg/out/a", "pkg/b"]);
}

/// Catches a tree store opened where a volume cannot name it (a path with ':' or ',' ends
/// a `--volume` field early: the "no path check" mutant mounts the wrong directory), one
/// whose directory cannot be made (the node then links every file, with a warning, and
/// does not stop), and a store that is not opened when it can be.
#[test]
fn a_tree_store_opens_where_a_volume_can_name_it() {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/kbf-driver-unit")
        .join(format!("trees-{}", std::process::id()));
    std::fs::create_dir_all(&base).expect("scratch");
    let config = |dir: PathBuf| TreeConfig::new(dir, 1 << 20);
    assert!(open_trees(config(base.join("fine"))).is_some());
    assert!(base.join("fine").is_dir());
    assert!(open_trees(config(base.join("a:b"))).is_none());
    assert!(open_trees(config(base.join("a,b"))).is_none());
    assert!(!base.join("a:b").exists());
    // A directory under a file cannot be made.
    std::fs::write(base.join("file"), b"x").expect("file");
    assert!(open_trees(config(base.join("file/trees"))).is_none());
}
