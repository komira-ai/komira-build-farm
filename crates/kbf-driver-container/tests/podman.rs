//! The driver against real rootless Podman and a delegated cgroup.
//!
//! These need what a hosted runner has once `tools/ci/podman-tests.sh` has set it up
//! (the T4 spike, `docs/spikes/hosted-runners.md`): rootless Podman, a system unit with
//! `Delegate=yes` whose `actions` cgroup enables cpu, memory and pids, and a busybox
//! image pulled by its index digest. So each test is `#[ignore]` with that reason, and
//! the script runs them with `--include-ignored`. Run that way without the setup, a
//! test fails (it never skips silently). The marker walk's own test needs only GNU
//! find, which those runners have, and runs the same way. The variables the script
//! sets:
//!
//! - `KBF_TEST_IMAGE`: `docker://<repo>@sha256:<per-architecture manifest digest>`;
//! - `KBF_TEST_INDEX_IMAGE`: the same image by its image index digest (pulled by it,
//!   so the store holds the index too);
//! - `KBF_TEST_CGROUP`: the delegated cgroup, relative to `/sys/fs/cgroup`.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kbf_daemon::{Runtime, RuntimeError, Work};
use kbf_driver_container::{MemoryCas, PodmanConfig, PodmanRuntime};
use kbf_proto::reapi::{ActionResult, Digest};
use kbf_types::{LeaseId, Resources};
use support::{Spec, blob, exists, store_action, work};

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!("{name} is not set: run these tests through tools/ci/podman-tests.sh")
    })
}

/// One test's cgroup parent (under the delegated cgroup), scratch and runtime.
struct Cell {
    /// The lease term this test's leases use. Container names are unique per Podman
    /// store, and the tests share one, so each test gets its own term.
    term: u64,
    cgroup: PathBuf,
    scratch: PathBuf,
    config: PodmanConfig,
    cas: Arc<MemoryCas>,
    runtime: Arc<PodmanRuntime<MemoryCas>>,
}

static TERMS: AtomicU64 = AtomicU64::new(1);

/// Each cgroup under `dir` with the processes in it, for a failure message.
fn describe(dir: &Path) -> String {
    let mut out = String::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(d) = pending.pop() {
        let procs = std::fs::read_to_string(d.join("cgroup.procs")).unwrap_or_default();
        let commands: Vec<String> = procs
            .split_whitespace()
            .map(|pid| {
                std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
                    .unwrap_or_default()
                    .replace('\0', " ")
            })
            .collect();
        out.push_str(&format!("\n  {}: {commands:?}", d.display()));
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                pending.push(entry.path());
            }
        }
    }
    out
}

/// Shows the driver's logs (its clean errors among them) in a failing test's output.
fn trace() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
}

impl Cell {
    fn new(name: &str) -> Self {
        trace();
        let term = TERMS.fetch_add(1, Ordering::Relaxed);
        let parent = format!("{}/{name}", var("KBF_TEST_CGROUP"));
        let cgroup = Path::new("/sys/fs/cgroup").join(parent.trim_start_matches('/'));
        if exists(&cgroup) {
            std::fs::remove_dir(&cgroup).expect("remove a stale test cgroup");
        }
        std::fs::create_dir(&cgroup).expect("create the test cgroup");
        std::fs::write(cgroup.join("cgroup.subtree_control"), "+cpu +memory +pids")
            .expect("enable controllers");
        let scratch = support::scratch(&format!("podman-{name}"));
        // Each test its own owner: they share one Podman store, and a runtime removes
        // its owner's containers when it starts.
        let mut config = PodmanConfig::new(scratch.clone(), parent, format!("kbf-test-{name}"));
        config.default_timeout = Duration::from_secs(120);
        config.kill_grace = Duration::from_secs(2);
        let cas = Arc::new(MemoryCas::new());
        let runtime =
            Arc::new(PodmanRuntime::new(config.clone(), Arc::clone(&cas)).expect("runtime"));
        Self {
            term,
            cgroup,
            scratch,
            config,
            cas,
            runtime,
        }
    }

    /// Lease (`term`, `seq`) running `action`.
    fn work(&self, seq: u64, action: Digest, resources: Resources) -> Work {
        let mut work = work(seq, action, resources);
        work.lease_id = LeaseId::new(self.term, seq);
        work
    }

    /// The container and lease cgroup name of lease `seq`.
    fn name(&self, seq: u64) -> String {
        format!("kbf-lease-{}-{seq}", self.term)
    }

    async fn run(&self, seq: u64, spec: &Spec) -> Result<ActionResult, RuntimeError> {
        let action = store_action(&self.cas, spec);
        self.runtime
            .run(self.work(seq, action, Resources::new(1000, 256 << 20)))
            .await
    }

    /// Asserts lease `seq` left no container, cgroup or scratch directory.
    fn assert_clean(&self, seq: u64) {
        let name = self.name(seq);
        assert!(!exists(&self.scratch.join(&name)), "{name}: scratch left");
        let lease = self.cgroup.join(&name);
        assert!(
            !exists(&lease),
            "{name}: lease cgroup left: {}",
            describe(&lease)
        );
        let names = podman(&["ps", "--all", "--format={{.Names}}"]);
        assert!(!names.lines().any(|n| n == name), "container left: {names}");
    }

    fn stdout(&self, result: &ActionResult) -> String {
        String::from_utf8(blob(&self.cas, result.stdout_digest.as_ref())).expect("utf-8")
    }
}

impl Drop for Cell {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir(&self.cgroup);
            support::force_remove(&self.scratch);
        }
    }
}

/// Runs podman (the test's own checks) and returns its stdout.
fn podman(args: &[&str]) -> String {
    let output = Command::new("podman")
        .args(args)
        .output()
        .expect("run podman");
    assert!(
        output.status.success(),
        "podman {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn sh(script: &str) -> Spec {
    Spec::new(&var("KBF_TEST_IMAGE"), script)
}

/// Catches the action not seeing its inputs, environment, working directory or the
/// constant hostname, and its exit code, stdout, stderr or outputs going uncollected.
#[tokio::test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn an_action_runs_and_its_results_are_collected() {
    let cell = Cell::new("collects");
    let mut spec =
        sh(r#"echo "$FOO $(pwd) $(hostname)"; cat in.txt > out/copy.txt; echo err >&2; exit 7"#);
    spec.working_directory = "pkg".to_owned();
    spec.env = vec![("FOO".to_owned(), "bar".to_owned())];
    spec.inputs = vec![("pkg/in.txt", b"input bytes", false)];
    spec.outputs = vec!["out/copy.txt".to_owned()];
    let result = cell.run(1, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 7);
    assert_eq!(cell.stdout(&result), "bar /kbf/root/pkg localhost\n");
    assert_eq!(blob(&cell.cas, result.stderr_digest.as_ref()), b"err\n");
    assert_eq!(result.output_files.len(), 1);
    assert_eq!(
        blob(&cell.cas, result.output_files[0].digest.as_ref()),
        b"input bytes"
    );
    cell.assert_clean(1);
}

/// Outputs are read from the overlay's upper layer. Catches an output that is already
/// an input being run (it would come back partial), and an input moved into an output
/// coming back partial: if the overlay recorded the move as a redirect, or a chmod as
/// a metadata-only copy-up, the upper layer would hold the directory without its
/// files, the file without its bytes, or a deleted file as a whiteout.
#[tokio::test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn outputs_are_whole_and_never_inputs() {
    let cell = Cell::new("overlap");
    let mut spec = sh("echo x > in/new");
    spec.inputs = vec![("in/a.txt", b"a", false)];
    spec.outputs = vec!["in".to_owned()];
    let outcome = cell.run(1, &spec).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Invalid(_))),
        "{outcome:?}"
    );
    cell.assert_clean(1);

    let mut spec = sh(
        "set -e; mv src/a.txt out/moved.txt; chmod +x out/moved.txt; \
         mv lib out/dir; rm out/dir/b.txt",
    );
    spec.inputs = vec![
        ("src/a.txt", b"a bytes", false),
        ("lib/b.txt", b"b", false),
        ("lib/c/d.txt", b"d bytes", false),
    ];
    spec.outputs = vec!["out/moved.txt".to_owned(), "out/dir".to_owned()];
    let result = cell.run(2, &spec).await.expect("ran");
    assert_eq!(
        result.exit_code,
        0,
        "stderr: {}",
        String::from_utf8_lossy(&blob(&cell.cas, result.stderr_digest.as_ref()))
    );
    assert_eq!(result.output_files.len(), 1, "{result:?}");
    let moved = &result.output_files[0];
    assert_eq!(blob(&cell.cas, moved.digest.as_ref()), b"a bytes");
    assert!(moved.is_executable);
    assert_eq!(result.output_directories.len(), 1, "{result:?}");
    let tree = support::tree(&cell.cas, result.output_directories[0].tree_digest.as_ref());
    let root = tree.root.expect("root");
    assert!(
        root.files.is_empty() && root.symlinks.is_empty(),
        "{root:?}"
    );
    assert_eq!(root.directories.len(), 1, "{root:?}");
    assert_eq!(root.directories[0].name, "c");
    assert_eq!(tree.children.len(), 1);
    let d = &tree.children[0].files;
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0].name, "d.txt");
    assert_eq!(blob(&cell.cas, d[0].digest.as_ref()), b"d bytes");
    cell.assert_clean(2);
}

/// The marker test. The action writes a uniquely named marker outside its output
/// directory: in the container's root, its /tmp, and beside its inputs. Catches a
/// skipped or partial clean step: after the lease ends no file of that name may exist
/// in Podman's storage or the scratch directory, and no container may remain.
#[tokio::test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn the_marker_test_nothing_outside_the_outputs_survives() {
    let cell = Cell::new("marker");
    let marker = format!("kbf-marker-{}", std::process::id());
    let spec = sh(&format!(
        "echo m > /{marker}; echo m > /tmp/{marker}; echo m > /kbf/root/{marker}; ls /{marker}"
    ));
    let result = cell.run(1, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 0, "the marker was written");
    let graph_root = podman(&["info", "--format={{.Store.GraphRoot}}"]);
    let graph_root = graph_root.trim();
    let scratch = cell.scratch.to_string_lossy();
    // The whole store, not this lease's paths: its layer is gone by now, and a marker
    // anywhere else in the store (a committed layer, a volume) is a leak too. Other
    // tests remove their containers while this walks, so a directory of the store may
    // vanish under it; one in this test's scratch may not.
    let walk = find(
        &["podman", "unshare"],
        &[graph_root, &scratch],
        &[graph_root],
        &["-name", &marker],
    );
    let found = walk.unwrap_or_else(|why| panic!("{why}"));
    assert!(found.trim().is_empty(), "marker left behind:\n{found}");
    cell.assert_clean(1);
}

/// Runs GNU find (through `prefix`) over `roots` with the expression `expr`, and
/// returns its stdout. The walk passes when find exits 0, or when every error it
/// reported is a directory below one of `racing` that vanished mid-walk: a directory
/// that is gone holds no file, and find walks the rest of the tree past that error.
/// Any other error, a vanished root among them, is an `Err` with find's stderr.
///
/// `-ignore_readdir_race` is not enough: findutils applies it only to its own stat
/// of an entry. A directory removed after it was listed is reported by fts (as an
/// unreadable directory, or an entry that could not be stat'd) whatever that option
/// says, and find then exits 1.
fn find(prefix: &[&str], roots: &[&str], racing: &[&str], expr: &[&str]) -> Result<String, String> {
    let (program, prefix) = prefix.split_first().expect("a program");
    let output = Command::new(program)
        .args(prefix)
        // The C locale fixes the message text and the quotes find puts around a path.
        .args(["env", "LC_ALL=C", "find"])
        .args(roots)
        .arg("-ignore_readdir_race")
        .args(expr)
        .output()
        .expect("run find");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let vanished = |line: &str| {
        line.strip_prefix("find: '")
            .and_then(|l| l.strip_suffix("': No such file or directory"))
            .is_some_and(|path| {
                racing.iter().any(|root| {
                    path.strip_prefix(root)
                        .is_some_and(|rest| rest.starts_with('/'))
                })
            })
    };
    let benign = false && !stderr.trim().is_empty() && stderr.lines().all(vanished);
    if output.status.success() || (output.status.code() == Some(1) && benign) {
        Ok(stdout)
    } else {
        Err(format!(
            "find {roots:?} {expr:?}: {}:\n{stderr}",
            output.status
        ))
    }
}

/// Catches the marker walk failing when another test removes its container mid-walk
/// (issue #105), and the tolerance hiding anything else. The first trigger file find
/// reaches deletes every other directory of the tree, which find has already listed
/// and not yet entered, so one directory vanishes mid-walk whichever order find takes.
/// That walk must pass and still print the trigger it matched (a marker found during
/// a race is still found); a vanished root, a vanished directory outside `racing`,
/// and an unreadable directory must each still fail.
#[test]
#[ignore = "needs GNU find: run by tools/ci/podman-tests.sh"]
fn a_directory_vanishing_mid_walk_fails_nothing_else() {
    let scratch = support::scratch("podman-walk-race");
    let tree = |name: &str| {
        let root = scratch.join(name);
        for d in ["p", "q"] {
            std::fs::create_dir_all(root.join(d)).expect("create");
            std::fs::write(root.join(d).join("trigger"), b"t").expect("write");
        }
        root.to_string_lossy().into_owned()
    };
    // Deletes every directory of the root ($0) except the one holding this trigger ($1).
    let remove_others =
        r#"for d in "$0"/*/; do case "$1" in "$d"*) ;; *) rm -r "$d" ;; esac; done"#;
    let race = |root: &str| -> Vec<String> {
        vec![
            "-name".into(),
            "trigger".into(),
            "-exec".into(),
            "sh".into(),
            "-c".into(),
            remove_others.into(),
            root.into(),
            "{}".into(),
            ";".into(),
            "-print".into(),
        ]
    };
    fn strs(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }

    let root = tree("raced");
    let found = find(&["env"], &[&root], &[&root], &strs(&race(&root)))
        .unwrap_or_else(|why| panic!("a directory vanishing mid-walk failed the walk: {why}"));
    let found: Vec<&str> = found.lines().collect();
    assert_eq!(
        found.len(),
        1,
        "one trigger runs, the other vanishes: {found:?}"
    );
    assert!(
        found[0].starts_with(&root) && found[0].ends_with("/trigger"),
        "{found:?}"
    );
    let left: Vec<_> = std::fs::read_dir(&root).expect("read").flatten().collect();
    assert_eq!(left.len(), 1, "the other directory was removed mid-walk");

    // The same race in a tree where nothing may vanish.
    let root = tree("not-racing");
    let outcome = find(&["env"], &[&root], &[], &strs(&race(&root)));
    assert!(outcome.is_err(), "{outcome:?}");

    // A root that does not exist.
    let missing = scratch.join("missing").to_string_lossy().into_owned();
    let outcome = find(&["env"], &[&missing], &[&missing], &["-name", "x"]);
    assert!(outcome.is_err(), "{outcome:?}");

    // A directory find may not read.
    let root = tree("unreadable");
    std::fs::set_permissions(
        Path::new(&root).join("p"),
        std::os::unix::fs::PermissionsExt::from_mode(0o000),
    )
    .expect("chmod");
    let outcome = find(&["env"], &[&root], &[&root], &["-name", "x"]);
    assert!(
        matches!(outcome, Err(ref why) if why.contains("Permission denied")),
        "{outcome:?}"
    );
    support::force_remove(&scratch);
}

/// Catches the network being on by default (the mutant drops `--network=none`, and
/// rootless Podman's default slirp4netns adds an interface and DNS): the action must
/// see loopback only, and a name lookup must fail.
#[tokio::test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn there_is_no_network_but_loopback() {
    let cell = Cell::new("network");
    let spec = sh("sed -n '3,$p' /proc/net/dev | cut -d: -f1 | tr -d ' '; \
         if nslookup example.com >/dev/null 2>&1; then echo dns-resolved; fi");
    let result = cell.run(1, &spec).await.expect("ran");
    assert_eq!(cell.stdout(&result), "lo\n");
    cell.assert_clean(1);
}

/// Catches an image named by tag, or by index digest, being run: the action digest
/// would not pin the bytes (the "tag instead of a digest" mutant).
#[tokio::test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn tags_and_index_digests_are_refused() {
    let cell = Cell::new("refused");
    let image = var("KBF_TEST_IMAGE");
    let repo = image.split('@').next().expect("repo");
    let tagged = Spec::new(&format!("{repo}:latest"), "true");
    let outcome = cell.run(1, &tagged).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Invalid(_))),
        "{outcome:?}"
    );
    let index = Spec::new(&var("KBF_TEST_INDEX_IMAGE"), "true");
    let outcome = cell.run(2, &index).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Invalid(ref why)) if why.contains("image index")),
        "{outcome:?}"
    );
    cell.assert_clean(1);
    cell.assert_clean(2);
}

/// Catches the soft-limit policy not reaching the kernel: `memory.high` from the
/// booking, `cpu.weight` from the CPU, no hard cap and swap allowed on the lease, and
/// one OOM group for the container.
#[tokio::test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn the_lease_cgroup_carries_the_soft_limits() {
    let cell = Cell::new("limits");
    let action = store_action(&cell.cas, &sh("sleep 5"));
    let work = cell.work(1, action, Resources::new(2000, 1 << 30));
    let runtime = Arc::clone(&cell.runtime);
    let run = tokio::spawn(async move { runtime.run(work).await });
    let lease = cell.cgroup.join(cell.name(1));
    let container = set_up_container_cgroup(&lease, "sleep").await;
    let read = |dir: &Path, file: &str| {
        std::fs::read_to_string(dir.join(file))
            .expect(file)
            .trim()
            .to_owned()
    };
    assert_eq!(
        read(&lease, "memory.high"),
        ((3u64 << 29) + (512 << 20)).to_string()
    );
    assert_eq!(read(&lease, "cpu.weight"), "200");
    assert_eq!(read(&lease, "memory.max"), "max");
    assert_eq!(read(&lease, "memory.swap.max"), "max");
    assert_eq!(read(&container, "memory.oom.group"), "1");
    let result = run.await.expect("join").expect("ran");
    assert_eq!(result.exit_code, 0);
    cell.assert_clean(1);
}

/// The container's cgroup under the lease cgroup `lease`, once the OCI runtime has set
/// it up. The directory alone is not enough: the runtime makes it first and writes the
/// container's cgroup files (memory.oom.group among them) later in `create`. The
/// action's own program (`comm`) running in it is the state the driver relies on (#88).
async fn set_up_container_cgroup(lease: &Path, comm: &str) -> PathBuf {
    let container = wait_for_container_cgroup(lease).await;
    let _ = (comm, wait_for_program_in); // MUTANT: no wait for the program
    container
}

/// The container's cgroup under `lease`, as soon as its directory exists. Polled every
/// 5 ms: a read right after it appears is what `set_up_container_cgroup` must not do,
/// and the stress test only shows that if this one is quick.
async fn wait_for_container_cgroup(lease: &Path) -> PathBuf {
    for _ in 0..6000 {
        let found = std::fs::read_dir(lease)
            .into_iter()
            .flatten()
            .flatten()
            .find(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.starts_with("libpod-") && !name.contains("conmon")
            });
        if let Some(entry) = found {
            return entry.path();
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("no container cgroup under {}", lease.display());
}

/// Waits until a process whose command name is `comm` is in `container`'s cgroup.
///
/// The OCI runtime (crun, runc) creates the container's cgroup directory, then writes
/// the spec's resources into it, all during `create`, while the container's init is
/// held on a sync socket; the action's argv is only exec'd at `start`. So once the
/// action's own program runs in the cgroup, its setup is complete, which the directory
/// existing does not imply.
async fn wait_for_program_in(container: &Path, comm: &str) {
    for _ in 0..600 {
        let procs = std::fs::read_to_string(container.join("cgroup.procs")).unwrap_or_default();
        let running = procs.split_whitespace().any(|pid| {
            std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim() == comm)
        });
        if running {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "no {comm} process in {}:{}",
        container.display(),
        describe(container)
    );
}

/// Catches (issue #88) a container's cgroup files read before the OCI runtime wrote
/// them: 100 leases, five at a time, each container's `memory.oom.group` read once
/// `set_up_container_cgroup` returns, which must be 1 every time. The mutant drops
/// that function's wait for the action's program: the read then follows the
/// directory's creation by at most one 5 ms poll, inside the window #88 hit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn every_container_is_one_oom_group_once_its_action_runs() {
    let cell = Cell::new("oom-group-stress");
    for batch in 0..20 {
        let seqs: Vec<u64> = (1..=5).map(|i| batch * 5 + i).collect();
        let runs: Vec<_> = seqs
            .iter()
            .map(|&seq| {
                let action = store_action(&cell.cas, &sh("sleep 3"));
                let work = cell.work(seq, action, Resources::default());
                let runtime = Arc::clone(&cell.runtime);
                tokio::spawn(async move { runtime.run(work).await })
            })
            .collect();
        for &seq in &seqs {
            let container =
                set_up_container_cgroup(&cell.cgroup.join(cell.name(seq)), "sleep").await;
            let group = std::fs::read_to_string(container.join("memory.oom.group"))
                .expect("memory.oom.group");
            assert_eq!(group.trim(), "1", "lease {seq}");
        }
        for (run, seq) in runs.into_iter().zip(seqs) {
            let result = run.await.expect("join").expect("ran");
            assert_eq!(result.exit_code, 0, "lease {seq}");
            cell.assert_clean(seq);
        }
    }
}

/// Catches (issue #105) the marker test's walk of Podman's store failing because a
/// container was removed while it walked. 40 leases run, two at a time, while the walk
/// goes over the whole store again and again; every walk must pass and find nothing.
/// The mutant makes `find` treat no error as benign: a layer directory that vanishes
/// mid-walk then fails a walk.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn the_store_walk_passes_while_containers_come_and_go() {
    let cell = Cell::new("walk-churn");
    let pairs: Vec<Vec<Work>> = (0..20)
        .map(|pair| {
            (1..=2)
                .map(|i| {
                    let action = store_action(&cell.cas, &sh("echo churn > /tmp/churn"));
                    cell.work(pair * 2 + i, action, Resources::new(1000, 256 << 20))
                })
                .collect()
        })
        .collect();
    let runtime = Arc::clone(&cell.runtime);
    let churn = tokio::spawn(async move {
        let mut outcomes = Vec::new();
        for pair in pairs {
            let runs: Vec<_> = pair
                .into_iter()
                .map(|work| {
                    let runtime = Arc::clone(&runtime);
                    tokio::spawn(async move { runtime.run(work).await })
                })
                .collect();
            for run in runs {
                outcomes.push(run.await.expect("join"));
            }
        }
        outcomes
    });
    let graph_root = podman(&["info", "--format={{.Store.GraphRoot}}"]);
    let graph_root = graph_root.trim().to_owned();
    let name = format!("kbf-never-written-{}", std::process::id());
    let mut walks = 0;
    while !churn.is_finished() {
        let (root, name) = (graph_root.clone(), name.clone());
        let walk = tokio::task::spawn_blocking(move || {
            find(
                &["podman", "unshare"],
                &[&root],
                &[&root],
                &["-name", &name],
            )
        })
        .await
        .expect("join");
        let found = walk.unwrap_or_else(|why| panic!("walk {walks}: {why}"));
        assert!(found.trim().is_empty(), "{found}");
        walks += 1;
    }
    for (i, outcome) in churn.await.expect("join").into_iter().enumerate() {
        let result = outcome.unwrap_or_else(|e| panic!("lease {}: {e:?}", i + 1));
        assert_eq!(result.exit_code, 0, "lease {}", i + 1);
        cell.assert_clean(i as u64 + 1);
    }
    assert!(
        walks > 1,
        "the store was walked {walks} times while leases ran"
    );
    println!("{walks} walks of the store while 40 leases came and went");
}

/// Catches a kernel OOM kill being reported as the action's own result (exit 137 would
/// be cached as a failing action): detected from the lease cgroup's memory.events,
/// never from Podman's OOMKilled flag.
#[tokio::test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn a_kernel_oom_kill_is_an_infrastructure_failure() {
    let cell = Cell::new("oom");
    // The test cgroup stands in for `actions/` and its host cap.
    std::fs::write(cell.cgroup.join("memory.max"), "64M").expect("memory.max");
    std::fs::write(cell.cgroup.join("memory.swap.max"), "0").expect("memory.swap.max");
    let spec = sh("dd if=/dev/zero of=/dev/null bs=200M count=1");
    let outcome = cell.run(1, &spec).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("OOM")),
        "{outcome:?}"
    );
    cell.assert_clean(1);
}

/// Catches a timeout that is not enforced on a real container, or that leaves it.
#[tokio::test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn a_timeout_stops_the_container() {
    let cell = Cell::new("timeout");
    let mut spec = sh("sleep 60");
    spec.timeout = Some(Duration::from_secs(2));
    let outcome = cell.run(1, &spec).await;
    assert!(
        matches!(outcome, Err(RuntimeError::TimedOut)),
        "{outcome:?}"
    );
    cell.assert_clean(1);
}

/// Catches `kill` returning while the container still exists, and a cancelled run
/// (the daemon dropped its task) leaving its container behind.
#[tokio::test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn kill_and_cancel_remove_the_container() {
    let cell = Cell::new("kill");
    // Several rounds: a lease cgroup left after kill or cancel showed up once in hosted CI.
    for seq in 1..=6 {
        let action = store_action(&cell.cas, &sh("sleep 60"));
        let work = cell.work(seq, action, Resources::default());
        let runtime = Arc::clone(&cell.runtime);
        let run = tokio::spawn(async move { runtime.run(work).await });
        wait_for_container_cgroup(&cell.cgroup.join(cell.name(seq))).await;
        if seq % 2 == 1 {
            cell.runtime.kill(LeaseId::new(cell.term, seq)).await;
            cell.assert_clean(seq);
            let outcome = run.await.expect("join");
            assert!(matches!(outcome, Err(RuntimeError::Killed)), "{outcome:?}");
        } else {
            run.abort();
            assert!(run.await.expect_err("cancelled").is_cancelled());
            cell.assert_clean(seq);
        }
    }
}

/// Catches (issue #155) a restarted daemon that leaves its killed predecessor's
/// container running (the scheduler would run the lease again beside it), or its lease
/// cgroup or scratch directory behind, and one that removes a container another owner
/// labelled. The killed daemon is modelled by forgetting its run: nothing of it is
/// dropped, so nothing of it is cleaned.
#[tokio::test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn a_restarted_runtime_removes_what_its_predecessor_left() {
    let cell = Cell::new("restart");
    let other = Cell::new("restart-other");
    let run = |c: &Cell| {
        let action = store_action(&c.cas, &sh("sleep 300"));
        let work = c.work(1, action, Resources::default());
        let runtime = Arc::clone(&c.runtime);
        Box::pin(async move { runtime.run(work).await })
    };
    let (mut mine, mut theirs) = (run(&cell), run(&other));
    tokio::select! {
        _ = &mut mine => panic!("the run ended"),
        _ = &mut theirs => panic!("the other run ended"),
        () = async {
            wait_for_container_cgroup(&cell.cgroup.join(cell.name(1))).await;
            wait_for_container_cgroup(&other.cgroup.join(other.name(1))).await;
        } => {}
    }
    // Never polled or dropped again, as a daemon that was killed.
    std::mem::forget(mine);
    std::mem::forget(theirs);
    let label = podman(&[
        "inspect",
        "--format={{index .Config.Labels \"kbf.owner\"}}",
        &cell.name(1),
    ]);
    assert_eq!(label.trim(), "kbf-test-restart");

    PodmanRuntime::new(cell.config.clone(), Arc::clone(&cell.cas)).expect("restart");
    cell.assert_clean(1);
    let names = podman(&["ps", "--all", "--format={{.Names}}"]);
    assert!(
        names.lines().any(|n| n == other.name(1)),
        "another owner's container was removed: {names}"
    );
    PodmanRuntime::new(other.config.clone(), Arc::clone(&other.cas)).expect("restart");
    other.assert_clean(1);
}

/// The ids on the `Uid:` and `Gid:` lines of process `pid`'s status (real, effective,
/// saved, filesystem), each with its line's name.
fn ids(pid: &str) -> Vec<(&'static str, u32)> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let mut ids = Vec::new();
    for line in status.lines() {
        for key in ["Uid", "Gid"] {
            if let Some(values) = line.strip_prefix(key).and_then(|v| v.strip_prefix(':')) {
                ids.extend(
                    values
                        .split_whitespace()
                        .map(|v| (key, v.parse().expect("an id"))),
                );
            }
        }
    }
    ids
}

/// fleet-updates-security S10, "a container action's host uid is not the daemon's".
/// Catches the "drop `--userns`" mutant: rootless Podman's default runs the
/// container's root as the daemon's own uid on the host. Two views of the running
/// action: the ids of each of its processes as the host sees them, and the owner, on
/// the host, of a file it created. Then a kill, whose clean step removes files the
/// container's ids still own (inside Podman's user namespace).
#[tokio::test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn no_container_id_is_the_daemons_on_the_host() {
    use std::os::unix::fs::MetadataExt;

    let cell = Cell::new("userns");
    let daemon = std::fs::metadata("/proc/self").expect("/proc/self");
    let (daemon_uid, daemon_gid) = (daemon.uid(), daemon.gid());
    let mut spec = sh("echo made > out/made; exec sleep 60");
    spec.outputs = vec!["out/made".to_owned()];
    let action = store_action(&cell.cas, &spec);
    let work = cell.work(1, action, Resources::default());
    let runtime = Arc::clone(&cell.runtime);
    let run = tokio::spawn(async move { runtime.run(work).await });
    let container = set_up_container_cgroup(&cell.cgroup.join(cell.name(1)), "sleep").await;

    let procs = std::fs::read_to_string(container.join("cgroup.procs")).expect("cgroup.procs");
    let seen: Vec<_> = procs.split_whitespace().flat_map(ids).collect();
    assert!(
        !seen.is_empty(),
        "no process read in {}",
        container.display()
    );
    let daemons: Vec<_> = seen
        .iter()
        .filter(|&&(key, id)| id == if key == "Uid" { daemon_uid } else { daemon_gid })
        .collect();
    assert!(
        daemons.is_empty(),
        "a container process runs as the daemon's user ({daemon_uid}:{daemon_gid}): {seen:?}"
    );
    let made = cell.scratch.join(cell.name(1)).join("upper/out/made");
    let meta = std::fs::symlink_metadata(&made).expect("the action's file");
    assert_ne!(meta.uid(), daemon_uid, "{} is the daemon's", made.display());
    assert_ne!(meta.gid(), daemon_gid, "{} is the daemon's", made.display());
    // The container's root made the file and runs `sleep`: one host id for both.
    assert_eq!(seen.first(), Some(&("Uid", meta.uid())), "{seen:?}");

    cell.runtime.kill(LeaseId::new(cell.term, 1)).await;
    let outcome = run.await.expect("join");
    assert!(matches!(outcome, Err(RuntimeError::Killed)), "{outcome:?}");
    cell.assert_clean(1);
}

/// Why `--userns=nomap` and not `auto`: rootless `auto` hands the first container
/// all but one of the user's 65,536 subordinate ids, so a second container on the
/// node fails in `podman create` ("not enough unused IDs in user namespace") until the
/// first ends. Catches a switch to `auto`: lease 2 runs to completion while lease 1's
/// container is still running.
#[tokio::test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn two_containers_run_at_once() {
    let cell = Cell::new("together");
    let action = store_action(&cell.cas, &sh("exec sleep 60"));
    let work = cell.work(1, action, Resources::default());
    let runtime = Arc::clone(&cell.runtime);
    let first = tokio::spawn(async move { runtime.run(work).await });
    set_up_container_cgroup(&cell.cgroup.join(cell.name(1)), "sleep").await;

    let second = cell.run(2, &sh("echo second")).await;
    let still = podman(&["ps", "--format={{.Names}}"]);
    assert!(
        still.lines().any(|n| n == cell.name(1)),
        "lease 1 ended before lease 2 ran: {still}"
    );
    let second = second.expect("lease 2 runs beside lease 1");
    assert_eq!(second.exit_code, 0, "{second:?}");
    assert_eq!(cell.stdout(&second), "second\n");
    cell.assert_clean(2);

    cell.runtime.kill(LeaseId::new(cell.term, 1)).await;
    let outcome = first.await.expect("join");
    assert!(matches!(outcome, Err(RuntimeError::Killed)), "{outcome:?}");
    cell.assert_clean(1);
}

/// Catches the overlay not being handed to the container's root before it runs (the
/// action can write nothing under the exec root: `EROFS`), and not being handed back
/// before the outputs are read (a `0700` output directory holding a `0600` file is
/// unreadable to the daemon's user). The action also appends to an input, which the
/// overlay copies up: input files are the container's root's too, as without a user
/// namespace.
#[tokio::test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn the_action_owns_its_files_and_its_private_outputs_are_collected() {
    let cell = Cell::new("owners");
    let mut spec = sh(
        "set -e; echo more >> in/a.txt; echo new > in/new; mkdir out/p; \
         echo secret > out/p/s; chmod 600 out/p/s; chmod 700 out/p",
    );
    spec.inputs = vec![("in/a.txt", b"a", false)];
    spec.outputs = vec!["out/p".to_owned()];
    let result = cell.run(1, &spec).await.expect("ran");
    assert_eq!(
        result.exit_code,
        0,
        "stderr: {}",
        String::from_utf8_lossy(&blob(&cell.cas, result.stderr_digest.as_ref()))
    );
    assert_eq!(result.output_directories.len(), 1, "{result:?}");
    let tree = support::tree(&cell.cas, result.output_directories[0].tree_digest.as_ref());
    let files = tree.root.expect("root").files;
    assert_eq!(files.len(), 1, "{files:?}");
    assert_eq!(files[0].name, "s");
    assert_eq!(blob(&cell.cas, files[0].digest.as_ref()), b"secret\n");
    cell.assert_clean(1);
}
