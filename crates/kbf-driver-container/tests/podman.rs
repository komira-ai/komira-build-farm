//! The driver against real rootless Podman and a delegated cgroup.
//!
//! These need what a hosted runner has once `tools/ci/podman-tests.sh` has set it up
//! (the T4 spike, `docs/spikes/hosted-runners.md`): rootless Podman, a system unit with
//! `Delegate=yes` whose `actions` cgroup enables cpu, memory and pids, and a busybox
//! image pulled by its index digest. So each test is `#[ignore]` with that reason, and
//! the script runs them with `--include-ignored`. Run that way without the setup, a
//! test fails (it never skips silently):
//!
//! - `KBF_TEST_IMAGE`: `docker://<repo>@sha256:<per-architecture manifest digest>`;
//! - `KBF_TEST_INDEX_IMAGE`: the same image by its image index digest;
//! - `KBF_TEST_CGROUP`: the delegated cgroup, relative to `/sys/fs/cgroup`.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::{Runtime, RuntimeError};
use kbf_driver_container::{MemoryCas, PodmanConfig, PodmanRuntime};
use kbf_proto::reapi::ActionResult;
use kbf_types::{LeaseId, Resources};
use support::{Spec, blob, exists, store_action, work};

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!("{name} is not set: run these tests through tools/ci/podman-tests.sh")
    })
}

/// One test's cgroup parent (under the delegated cgroup), scratch and runtime.
struct Cell {
    cgroup: PathBuf,
    scratch: PathBuf,
    cas: Arc<MemoryCas>,
    runtime: Arc<PodmanRuntime<MemoryCas>>,
}

impl Cell {
    fn new(name: &str) -> Self {
        let parent = format!("{}/{name}", var("KBF_TEST_CGROUP"));
        let cgroup = Path::new("/sys/fs/cgroup").join(parent.trim_start_matches('/'));
        if exists(&cgroup) {
            std::fs::remove_dir(&cgroup).expect("remove a stale test cgroup");
        }
        std::fs::create_dir(&cgroup).expect("create the test cgroup");
        std::fs::write(cgroup.join("cgroup.subtree_control"), "+cpu +memory +pids")
            .expect("enable controllers");
        let scratch = support::scratch(&format!("podman-{name}"));
        let mut config = PodmanConfig::new(scratch.clone(), parent);
        config.default_timeout = Duration::from_secs(120);
        config.kill_grace = Duration::from_secs(2);
        let cas = Arc::new(MemoryCas::new());
        let runtime = Arc::new(PodmanRuntime::new(config, Arc::clone(&cas)).expect("runtime"));
        Self {
            cgroup,
            scratch,
            cas,
            runtime,
        }
    }

    async fn run(&self, seq: u64, spec: &Spec) -> Result<ActionResult, RuntimeError> {
        let action = store_action(&self.cas, spec);
        self.runtime
            .run(work(seq, action, Resources::new(1000, 256 << 20)))
            .await
    }

    /// Asserts lease (1, `seq`) left no container, cgroup or scratch directory.
    fn assert_clean(&self, seq: u64) {
        let name = format!("kbf-lease-1-{seq}");
        assert!(!exists(&self.scratch.join(&name)), "scratch left");
        assert!(!exists(&self.cgroup.join(&name)), "lease cgroup left");
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
    cell.assert_clean(1);
    let graph_root = podman(&["info", "--format={{.Store.GraphRoot}}"]);
    let found = podman(&[
        "unshare",
        "find",
        graph_root.trim(),
        &cell.scratch.to_string_lossy(),
        "-name",
        &marker,
    ]);
    assert!(found.trim().is_empty(), "marker left behind:\n{found}");
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
        matches!(outcome, Err(RuntimeError::Invalid(ref why)) if why.contains("per-architecture")),
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
    let runtime = Arc::clone(&cell.runtime);
    let run = tokio::spawn(async move {
        runtime
            .run(work(1, action, Resources::new(2000, 1 << 30)))
            .await
    });
    let lease = cell.cgroup.join("kbf-lease-1-1");
    let container = wait_for_container_cgroup(&lease).await;
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

async fn wait_for_container_cgroup(lease: &Path) -> PathBuf {
    for _ in 0..600 {
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
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no container cgroup under {}", lease.display());
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
    for seq in [1, 2] {
        let action = store_action(&cell.cas, &sh("sleep 60"));
        let runtime = Arc::clone(&cell.runtime);
        let run =
            tokio::spawn(async move { runtime.run(work(seq, action, Resources::default())).await });
        wait_for_container_cgroup(&cell.cgroup.join(format!("kbf-lease-1-{seq}"))).await;
        if seq == 1 {
            cell.runtime.kill(LeaseId::new(1, seq)).await;
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
