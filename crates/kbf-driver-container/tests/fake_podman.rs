//! The driver's steps against a stand-in `podman` (`fixtures/fake-podman.sh`) that runs
//! each action as a plain process, and a plain directory standing in for the cgroup
//! mount. These run on any Linux machine; `podman.rs` runs the same promises against
//! real rootless Podman.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::{Runtime, RuntimeError};
use kbf_driver_container::{MemoryCas, PodmanConfig, PodmanRuntime};
use kbf_types::Resources;
use support::{Spec, blob, exists, store_action, tree, work};

/// The per-architecture manifest every fake image store holds.
const MANIFEST: &[u8] =
    br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
/// An image index over it.
const INDEX: &[u8] =
    br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#;

fn sha256(bytes: &[u8]) -> String {
    format!(
        "sha256:{}",
        kbf_driver_container::cas::digest_of(bytes).hash
    )
}

fn image_by(digest: &str) -> String {
    format!("docker://registry.test/tools/busybox@{digest}")
}

fn image() -> String {
    image_by(&sha256(MANIFEST))
}

/// One test's fake Podman, fake cgroup mount, scratch directory and runtime.
struct Fake {
    dir: PathBuf,
    state: PathBuf,
    cgroup: PathBuf,
    scratch: PathBuf,
    cas: Arc<MemoryCas>,
    runtime: Arc<PodmanRuntime<MemoryCas>>,
}

impl Fake {
    fn new(name: &str) -> Self {
        let dir = support::scratch(&format!("fake-{name}"));
        let state = dir.join("state");
        let cgroup = dir.join("cgroup");
        let scratch = dir.join("scratch");
        for d in [&state, &cgroup.join("actions"), &scratch] {
            std::fs::create_dir_all(d).expect("mkdir");
        }
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-podman.sh");
        // A symlink, not a written script: exec'ing a file this process just wrote can
        // fail with ETXTBSY while another test thread forks.
        let program = dir.join("podman");
        std::os::unix::fs::symlink(&fixture, &program).expect("link podman");
        std::fs::write(state.join("image-id"), "img1\n").expect("image id");
        let mut config = PodmanConfig::new(scratch.clone(), "/actions".to_owned());
        config.podman = program;
        config.cgroup_root = cgroup.clone();
        config.default_timeout = Duration::from_secs(60);
        config.kill_grace = Duration::from_millis(300);
        let cas = Arc::new(MemoryCas::new());
        let runtime = Arc::new(PodmanRuntime::new(config, Arc::clone(&cas)).expect("runtime"));
        let fake = Self {
            dir,
            state,
            cgroup,
            scratch,
            cas,
            runtime,
        };
        fake.store_manifest(&sha256(MANIFEST), MANIFEST);
        fake
    }

    /// Puts `bytes` in the fake image store as image img1's manifest under `digest`.
    fn store_manifest(&self, digest: &str, bytes: &[u8]) {
        let dir = self.state.join("store/fake-images/img1");
        std::fs::create_dir_all(&dir).expect("mkdir store");
        let file = kbf_driver_container::image::manifest_file(digest);
        std::fs::write(dir.join(file), bytes).expect("write manifest");
    }

    fn knob(&self, name: &str, contents: &str) {
        std::fs::write(self.state.join(name), contents).expect("write knob");
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.state.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// What the fake recorded, in order (see `fixtures/fake-podman.sh`).
    fn events(&self) -> Vec<String> {
        std::fs::read_to_string(self.state.join("events"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn lease_dir(&self, seq: u64) -> PathBuf {
        self.scratch.join(format!("kbf-lease-1-{seq}"))
    }

    fn lease_cgroup(&self, seq: u64) -> PathBuf {
        self.cgroup.join(format!("actions/kbf-lease-1-{seq}"))
    }

    /// Asserts the lease left nothing behind.
    fn assert_clean(&self, seq: u64) {
        assert!(!exists(&self.lease_dir(seq)), "scratch directory left");
        assert!(!exists(&self.lease_cgroup(seq)), "lease cgroup left");
    }

    async fn run(
        &self,
        seq: u64,
        spec: &Spec,
        script: &str,
    ) -> Result<kbf_proto::reapi::ActionResult, RuntimeError> {
        self.knob("action.sh", script);
        let action = store_action(&self.cas, spec);
        self.runtime
            .run(work(seq, action, Resources::new(2000, 1 << 30)))
            .await
    }

    async fn wait_for_start(&self) {
        for _ in 0..500 {
            if self.state.join("pid").exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the action never started");
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            support::force_remove(&self.dir);
        }
    }
}

/// Catches each part of a run going missing: inputs not written (or written without
/// the executable bit), the working directory or output parents not made, outputs,
/// stdout, stderr or the exit code not collected, the lease's limits not written, and
/// any of it left behind afterwards.
#[tokio::test]
async fn a_run_collects_everything_and_leaves_nothing() {
    let fake = Fake::new("collects");
    let mut spec = Spec::new(&image(), "unused by the fake");
    spec.working_directory = "pkg".to_owned();
    spec.inputs = vec![
        ("pkg/src/in.txt", b"input bytes", false),
        ("tool.sh", b"#!/bin/sh\n", true),
    ];
    spec.symlinks = vec![("pkg/link-in", "src/in.txt")];
    spec.outputs = vec![
        "out/copy.txt".to_owned(),
        "out/dir".to_owned(),
        "out/link".to_owned(),
        "out/never-written".to_owned(),
    ];
    let script = r#"
        set -e
        out="$UPPER/pkg/out"
        [ -d "$out" ]
        [ -x "$ROOT/tool.sh" ] && [ ! -x "$ROOT/pkg/src/in.txt" ]
        [ "$(readlink "$ROOT/pkg/link-in")" = src/in.txt ]
        cp "$ROOT/pkg/src/in.txt" "$out/copy.txt"
        mkdir -p "$out/dir/sub"
        echo nested > "$out/dir/sub/n.txt"
        printf '#!/bin/sh\n' > "$out/dir/run.sh"; chmod +x "$out/dir/run.sh"
        ln -s sub/n.txt "$out/dir/to-n"
        mkfifo "$out/dir/fifo"
        ln -s copy.txt "$out/link"
        cat "$CG/memory.high" "$CG/cpu.weight" "$CG/cgroup.subtree_control" > "$out/dir/limits"
        echo to-stdout; echo to-stderr >&2
        exit 3
    "#;
    let result = fake.run(1, &spec, script).await.expect("ran");
    assert_eq!(result.exit_code, 3);
    assert_eq!(
        blob(&fake.cas, result.stdout_digest.as_ref()),
        b"to-stdout\n"
    );
    assert_eq!(
        blob(&fake.cas, result.stderr_digest.as_ref()),
        b"to-stderr\n"
    );

    assert_eq!(result.output_files.len(), 1);
    let copy = &result.output_files[0];
    assert_eq!(copy.path, "out/copy.txt");
    assert_eq!(blob(&fake.cas, copy.digest.as_ref()), b"input bytes");
    assert!(!copy.is_executable);

    assert_eq!(result.output_symlinks.len(), 1);
    assert_eq!(result.output_symlinks[0].path, "out/link");
    assert_eq!(result.output_symlinks[0].target, "copy.txt");

    assert_eq!(result.output_directories.len(), 1);
    let dir = &result.output_directories[0];
    assert_eq!(dir.path, "out/dir");
    let tree = tree(&fake.cas, dir.tree_digest.as_ref());
    let root = tree.root.expect("root");
    let names: Vec<_> = root.files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["limits", "run.sh"], "sorted, fifo left out");
    assert!(root.files[1].is_executable);
    // memory.high = 1 GiB x 1.5 + 512 MiB; cpu.weight = 2000 millicpus / 10.
    assert_eq!(
        blob(&fake.cas, root.files[0].digest.as_ref()),
        format!("{}200+cpu +memory +pids", (3u64 << 29) + (512 << 20)).as_bytes()
    );
    assert_eq!(root.symlinks[0].name, "to-n");
    assert_eq!(root.symlinks[0].target, "sub/n.txt");
    assert_eq!(root.directories[0].name, "sub");
    assert_eq!(tree.children.len(), 1);
    assert_eq!(tree.children[0].files[0].name, "n.txt");
    assert_eq!(
        dir.root_directory_digest.as_ref(),
        Some(&kbf_driver_container::cas::digest_of(
            &prost::Message::encode_to_vec(&root)
        ))
    );

    fake.assert_clean(1);
    assert!(fake.calls().contains(&"rm".to_owned()));
}

/// The marker test: the action writes outside its output directory (beside the input
/// root, into its lease cgroup). Catches a skipped clean step: after the lease ends,
/// nothing of the marker may remain.
#[tokio::test]
async fn the_marker_test_nothing_outside_the_outputs_survives() {
    let fake = Fake::new("leftovers");
    let spec = Spec::new(&image(), "unused");
    let script = r#"
        echo marker > "$UPPER/../marker"
        echo marker > "$UPPER/marker"
        mkdir "$CG/marker-cgroup"
    "#;
    let result = fake.run(1, &spec, script).await.expect("ran");
    assert_eq!(result.exit_code, 0);
    fake.assert_clean(1);
    let left: Vec<_> = walk(&fake.dir)
        .into_iter()
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().contains("marker"))
        })
        .collect();
    assert!(left.is_empty(), "marker left behind: {left:?}");
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(d) = pending.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                pending.push(path.clone());
            }
            found.push(path);
        }
    }
    found
}

/// Catches an image named by tag reaching Podman (the "tag instead of a digest"
/// mutant): refused as the client's error before anything is created.
#[tokio::test]
async fn a_tag_is_refused_before_anything_runs() {
    let fake = Fake::new("tag");
    let spec = Spec::new("docker://registry.test/tools/busybox:latest", "unused");
    let outcome = fake.run(1, &spec, "exit 0").await;
    assert!(
        matches!(outcome, Err(RuntimeError::Invalid(ref why)) if why.contains("digest")),
        "{outcome:?}"
    );
    assert!(
        fake.calls().is_empty(),
        "podman was called: {:?}",
        fake.calls()
    );
    fake.assert_clean(1);
}

/// Catches an image index digest being run (the store holds the image, pulled through
/// the index): one action digest would mean a different image on each architecture.
#[tokio::test]
async fn an_index_digest_is_refused() {
    let fake = Fake::new("index");
    let index = sha256(INDEX);
    fake.store_manifest(&index, INDEX);
    let outcome = fake
        .run(1, &Spec::new(&image_by(&index), "unused"), "exit 0")
        .await;
    assert!(
        matches!(outcome, Err(RuntimeError::Invalid(ref why)) if why.contains("image index")),
        "{outcome:?}"
    );
    assert_eq!(fake.calls(), ["image", "info"]);
    fake.assert_clean(1);
}

/// Catches a missing image being pulled or run anyway (nodes never pull at action
/// time; a node without the image is the farm's failure, not the client's), and an
/// image store that cannot vouch for the manifest being trusted.
#[tokio::test]
async fn an_image_the_store_cannot_vouch_for_is_an_infrastructure_failure() {
    let fake = Fake::new("no-image");
    let spec = Spec::new(&image(), "unused");
    // (what the failure says, how to break the store, how to mend it)
    type Case<'a> = (&'a str, &'a dyn Fn(), &'a dyn Fn());
    let cases: [Case; 4] = [
        (
            "not in this node's image store",
            &|| std::fs::remove_file(fake.state.join("image-id")).expect("rm"),
            &|| fake.knob("image-id", "img1\n"),
        ),
        ("info refused", &|| fake.knob("info-fails", ""), &|| {
            std::fs::remove_file(fake.state.join("info-fails")).expect("rm")
        }),
        (
            "holds no manifest",
            &|| fake.knob("image-id", "img2\n"),
            &|| fake.knob("image-id", "img1\n"),
        ),
        (
            "hashes to",
            &|| fake.store_manifest(&sha256(MANIFEST), INDEX),
            &|| fake.store_manifest(&sha256(MANIFEST), MANIFEST),
        ),
    ];
    for (seq, (want, break_it, mend_it)) in cases.iter().enumerate() {
        let seq = seq as u64 + 1;
        break_it();
        let outcome = fake.run(seq, &spec, "exit 0").await;
        assert!(
            matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains(want)),
            "{want}: {outcome:?}"
        );
        fake.assert_clean(seq);
        mend_it();
    }
    let result = fake.run(9, &spec, "exit 0").await.expect("mended");
    assert_eq!(result.exit_code, 0);
}

/// Catches a timeout that is not enforced, or that leaves the container behind.
#[tokio::test]
async fn a_timeout_stops_the_action_and_cleans_up() {
    let fake = Fake::new("timeout");
    let mut spec = Spec::new(&image(), "unused");
    spec.timeout = Some(Duration::from_millis(300));
    let outcome = fake.run(1, &spec, "sleep 30").await;
    assert!(
        matches!(outcome, Err(RuntimeError::TimedOut)),
        "{outcome:?}"
    );
    assert!(fake.calls().contains(&"kill".to_owned()));
    fake.assert_clean(1);
}

/// Catches `kill` returning before the work has stopped and the lease is clean, and a
/// killed lease reported as anything but Killed.
#[tokio::test]
async fn kill_stops_the_action_and_returns_once_clean() {
    let fake = Fake::new("kill");
    let spec = Spec::new(&image(), "unused");
    fake.knob("action.sh", "sleep 30");
    let action = store_action(&fake.cas, &spec);
    let runtime = Arc::clone(&fake.runtime);
    let run = tokio::spawn(async move { runtime.run(work(1, action, Resources::default())).await });
    fake.wait_for_start().await;
    fake.runtime.kill(kbf_types::LeaseId::new(1, 1)).await;
    fake.assert_clean(1);
    let outcome = run.await.expect("join");
    assert!(matches!(outcome, Err(RuntimeError::Killed)), "{outcome:?}");
    // Killing a lease that is not running does nothing and returns.
    fake.runtime.kill(kbf_types::LeaseId::new(1, 1)).await;
}

/// Catches the kill path skipping `cgroup.kill` after the SIGTERM grace period: an
/// action that ignores SIGTERM must die from the lease cgroup's `cgroup.kill`, and
/// `podman start` must then end on its own within the second grace period. Without
/// that step `podman start` is killed after the second grace period (so it never
/// records its end) and only the clean step's `cgroup.kill` ends the action.
#[tokio::test]
async fn sigterm_ignored_falls_back_to_cgroup_kill() {
    let fake = Fake::new("cgroup-kill");
    let mut spec = Spec::new(&image(), "unused");
    spec.timeout = Some(Duration::from_millis(200));
    let script = "trap '' TERM; while :; do sleep 0.05; done";
    let outcome = fake.run(1, &spec, script).await;
    assert!(
        matches!(outcome, Err(RuntimeError::TimedOut)),
        "{outcome:?}"
    );
    let events = fake.events();
    let expected = ["cgroup.kill ended the action", "start ended", "rm"];
    assert_eq!(events, expected, "{events:?}");
    fake.assert_clean(1);
}

/// Catches a cancelled run (the daemon dropped its task) leaving its container, cgroup
/// or scratch directory behind.
#[tokio::test]
async fn a_dropped_run_still_cleans_up() {
    let fake = Fake::new("dropped");
    let spec = Spec::new(&image(), "unused");
    fake.knob("action.sh", "sleep 30");
    let action = store_action(&fake.cas, &spec);
    let runtime = Arc::clone(&fake.runtime);
    let run = tokio::spawn(async move { runtime.run(work(1, action, Resources::default())).await });
    fake.wait_for_start().await;
    run.abort();
    assert!(run.await.expect_err("cancelled").is_cancelled());
    fake.assert_clean(1);
    assert!(fake.calls().contains(&"rm".to_owned()));
    // On real Podman a run dropped mid-start leaves `crun create` in the lease, and
    // `podman rm --force` does not wait for it; the clean kills the cgroup first.
    assert!(
        fake.state.join("killed-before-rm").exists(),
        "podman rm ran before cgroup.kill"
    );

    // A dropped run whose clean fails still removes what it can (and logs the rest).
    fake.knob("rm-fails", "");
    let action = store_action(&fake.cas, &spec);
    let runtime = Arc::clone(&fake.runtime);
    let run = tokio::spawn(async move { runtime.run(work(2, action, Resources::default())).await });
    std::fs::remove_file(fake.state.join("pid")).expect("rm pid");
    fake.wait_for_start().await;
    run.abort();
    assert!(run.await.expect_err("cancelled").is_cancelled());
    assert!(!exists(&fake.lease_dir(2)), "scratch left");
}

/// Catches a kernel OOM kill reported as the action's own exit 137 (it would be cached
/// as a failing action), and an action's own exit 137 reported as an OOM.
#[tokio::test]
async fn exit_137_is_an_oom_only_with_a_kernel_oom_event() {
    let fake = Fake::new("oom");
    let spec = Spec::new(&image(), "unused");
    let oom =
        r#"printf 'low 0\nhigh 4\nmax 1\noom 1\noom_kill 1\n' > "$CG/memory.events"; exit 137"#;
    let outcome = fake.run(1, &spec, oom).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("OOM")),
        "{outcome:?}"
    );
    fake.assert_clean(1);

    let own = r#"printf 'oom 0\noom_kill 0\n' > "$CG/memory.events"; exit 137"#;
    let result = fake.run(2, &spec, own).await.expect("ran");
    assert_eq!(result.exit_code, 137);
    fake.assert_clean(2);

    // No memory.events to read: the driver cannot tell, so it does not guess.
    let outcome = fake.run(3, &spec, "exit 137").await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("memory.events")),
        "{outcome:?}"
    );
    fake.assert_clean(3);

    // Nor when memory.events carries no count.
    let garbled = r#"printf 'oom_kill many\n' > "$CG/memory.events"; exit 137"#;
    let outcome = fake.run(4, &spec, garbled).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("no oom_kill count")),
        "{outcome:?}"
    );
    fake.assert_clean(4);
}

/// Catches Podman's own failures being reported as the action's result.
#[tokio::test]
async fn podman_failures_are_infrastructure_failures() {
    let fake = Fake::new("podman-fails");
    let spec = Spec::new(&image(), "unused");
    fake.knob("create-fails", "");
    let outcome = fake.run(1, &spec, "exit 0").await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("create refused")),
        "{outcome:?}"
    );
    fake.assert_clean(1);
    assert!(
        fake.calls().contains(&"rm".to_owned()),
        "a half-made container is removed"
    );

    std::fs::remove_file(fake.state.join("create-fails")).expect("rm knob");
    fake.knob("status-override", "created 0\n");
    let outcome = fake.run(2, &spec, "exit 0").await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("did not run")),
        "{outcome:?}"
    );
    fake.assert_clean(2);

    fake.knob("status-override", "exited many\n");
    let outcome = fake.run(3, &spec, "exit 0").await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("exit code")),
        "{outcome:?}"
    );
    fake.assert_clean(3);

    std::fs::remove_file(fake.state.join("status-override")).expect("rm knob");
    fake.knob("inspect-fails", "");
    let outcome = fake.run(4, &spec, "exit 0").await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("inspect refused")),
        "{outcome:?}"
    );
    fake.assert_clean(4);
}

/// Catches the driver's own file steps failing silently or as the action's result: an
/// output parent it cannot make (a name over the kernel's 255-byte limit), a log file
/// it cannot create, and a log file it cannot read back are each the farm's failure,
/// and the lease is still cleaned.
#[tokio::test]
async fn the_drivers_own_file_failures_are_infrastructure_failures() {
    let fake = Fake::new("driver-files");
    let mut spec = Spec::new(&image(), "unused");
    spec.outputs = vec![format!("out/{}/f", "n".repeat(300))];
    let outcome = fake.run(1, &spec, "exit 0").await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("upper/out/nnn")),
        "{outcome:?}"
    );
    fake.assert_clean(1);

    let spec = Spec::new(&image(), "unused");
    for (seq, log) in [(2, "stdout"), (3, "stderr")] {
        fake.knob("block-log", log);
        let outcome = fake.run(seq, &spec, "exit 0").await;
        let want = format!("kbf-lease-1-{seq}/{log}: ");
        assert!(
            matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains(&want)),
            "{log}: {outcome:?}"
        );
        fake.assert_clean(seq);
    }
    std::fs::remove_file(fake.state.join("block-log")).expect("rm knob");

    // The fake runs the action beside the lease directory, so it can unlink a log.
    let outcome = fake
        .run(4, &spec, r#"rm "$(dirname "$ROOT")/stderr""#)
        .await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("kbf-lease-1-4/stderr: ")),
        "{outcome:?}"
    );
    fake.assert_clean(4);
}

/// Catches a failing clean step hiding the lease's own failure: the first error is
/// the one reported (the clean error is logged).
#[tokio::test]
async fn a_failed_run_with_a_failed_clean_reports_the_run() {
    let fake = Fake::new("both-fail");
    fake.knob("create-fails", "");
    fake.knob("rm-fails", "");
    let outcome = fake.run(1, &Spec::new(&image(), "unused"), "exit 0").await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("create refused")),
        "{outcome:?}"
    );
}

/// Catches a missing Podman program being reported as anything but the farm's
/// failure.
#[tokio::test]
async fn a_missing_podman_is_an_infrastructure_failure() {
    let fake = Fake::new("no-podman");
    std::fs::remove_file(fake.dir.join("podman")).expect("unlink podman");
    let outcome = fake.run(1, &Spec::new(&image(), "unused"), "exit 0").await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.starts_with("run ")),
        "{outcome:?}"
    );
    fake.assert_clean(1);
}

/// Catches the kill path giving up early: when SIGTERM cannot be sent and the
/// container survives `cgroup.kill`, the driver still stops `podman start` and
/// removes the container by force.
#[tokio::test]
async fn the_kill_path_ends_even_when_every_signal_fails() {
    let fake = Fake::new("stubborn");
    fake.knob("kill-fails", "");
    let mut spec = Spec::new(&image(), "unused");
    spec.timeout = Some(Duration::from_millis(200));
    // With its lease cgroup gone, cgroup.kill cannot be written either.
    let outcome = fake.run(1, &spec, r#"find "$CG" -delete; sleep 30"#).await;
    assert!(
        matches!(outcome, Err(RuntimeError::TimedOut)),
        "{outcome:?}"
    );
    fake.assert_clean(1);
}

/// Catches a failed clean step being ignored: a dirty node must fail the lease.
#[tokio::test]
async fn a_failed_clean_fails_the_lease() {
    let fake = Fake::new("rm-fails");
    fake.knob("rm-fails", "");
    let outcome = fake.run(1, &Spec::new(&image(), "unused"), "exit 0").await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.starts_with("clean:")),
        "{outcome:?}"
    );
}

/// Catches scratch the daemon's user cannot delete (no permissions left, or another
/// container user's files) being left behind: removal falls back to `podman unshare`.
#[tokio::test]
async fn unremovable_scratch_falls_back_to_podman_unshare() {
    let fake = Fake::new("unshare");
    let spec = Spec::new(&image(), "unused");
    let script = r#"mkdir "$UPPER/locked"; touch "$UPPER/locked/f"; chmod 000 "$UPPER/locked""#;
    let result = fake.run(1, &spec, script).await.expect("ran");
    assert_eq!(result.exit_code, 0);
    assert!(fake.calls().contains(&"unshare".to_owned()));
    fake.assert_clean(1);

    // verify-clean: a removal that claims success but leaves the directory is caught.
    fake.knob("unshare-noop", "");
    let outcome = fake.run(2, &spec, script).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("still exists")),
        "{outcome:?}"
    );

    std::fs::remove_file(fake.state.join("unshare-noop")).expect("rm knob");
    fake.knob("unshare-fails", "");
    let outcome = fake.run(3, &spec, script).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("unshare refused")),
        "{outcome:?}"
    );
}

/// Catches actions that cannot be run safely reaching Podman: no arguments, an
/// output path or working directory that climbs out of the exec root, a negative
/// timeout. Each is the client's error.
#[tokio::test]
async fn invalid_actions_are_refused_before_anything_runs() {
    let fake = Fake::new("invalid");
    let mut no_args = Spec::new(&image(), "unused");
    no_args.argv.clear();
    let mut escaping_output = Spec::new(&image(), "unused");
    escaping_output.outputs = vec!["../escape".to_owned()];
    let mut escaping_workdir = Spec::new(&image(), "unused");
    escaping_workdir.working_directory = "/abs".to_owned();
    for (seq, spec) in [no_args, escaping_output, escaping_workdir]
        .iter()
        .enumerate()
    {
        let outcome = fake.run(seq as u64 + 1, spec, "exit 0").await;
        assert!(
            matches!(outcome, Err(RuntimeError::Invalid(_))),
            "{outcome:?}"
        );
        fake.assert_clean(seq as u64 + 1);
    }
    assert!(
        fake.calls().is_empty(),
        "podman was called: {:?}",
        fake.calls()
    );
}

/// Catches the driver advertising itself under another name, or taking leases of a
/// kind it cannot run.
#[test]
fn the_driver_serves_action_leases_only() {
    let fake = Fake::new("serves");
    assert_eq!(fake.runtime.driver(), "container");
    assert!(fake.runtime.serves("action"));
    assert!(!fake.runtime.serves("whole_machine"));
}

/// Catches an Action without a Command or an input root being run with defaults, and
/// a missing input blob being reported as the client's error rather than the farm's.
#[tokio::test]
async fn incomplete_actions_are_refused() {
    use kbf_proto::reapi::Action;
    use prost::Message;

    let fake = Fake::new("incomplete");
    let full = store_action(&fake.cas, &Spec::new(&image(), "unused"));
    let action = Action::decode(fake.cas.blob(&full).expect("action").as_slice()).expect("Action");
    let no_command = Action {
        command_digest: None,
        ..action.clone()
    };
    let no_root = Action {
        input_root_digest: None,
        ..action.clone()
    };
    for (seq, broken) in [no_command, no_root].into_iter().enumerate() {
        let digest = fake.cas.insert(broken.encode_to_vec());
        let outcome = fake
            .runtime
            .run(work(seq as u64 + 1, digest, Resources::default()))
            .await;
        assert!(
            matches!(outcome, Err(RuntimeError::Invalid(_))),
            "{outcome:?}"
        );
    }
    let missing_root = Action {
        input_root_digest: Some(kbf_driver_container::cas::digest_of(b"not stored")),
        ..action
    };
    let digest = fake.cas.insert(missing_root.encode_to_vec());
    let outcome = fake
        .runtime
        .run(work(3, digest, Resources::default()))
        .await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("not in the CAS")),
        "{outcome:?}"
    );
    for seq in 1..=3 {
        fake.assert_clean(seq);
    }
    assert!(!fake.calls().contains(&"create".to_owned()));
}

/// Catches the action's environment not reaching Podman, and an output path that is
/// neither file, directory nor symlink being reported as an output.
#[tokio::test]
async fn the_environment_reaches_podman_and_odd_outputs_are_left_out() {
    let fake = Fake::new("env");
    let mut spec = Spec::new(&image(), "unused");
    spec.env = vec![("FOO".to_owned(), "a b=c".to_owned())];
    spec.outputs = vec!["pipe".to_owned()];
    let result = fake
        .run(1, &spec, r#"mkfifo "$UPPER/pipe""#)
        .await
        .expect("ran");
    let args = std::fs::read_to_string(fake.state.join("create.args")).expect("args");
    assert!(args.lines().any(|a| a == "--env=FOO=a b=c"), "{args}");
    assert!(result.output_files.is_empty() && result.output_directories.is_empty());
    assert!(result.output_symlinks.is_empty());
    fake.assert_clean(1);
}

/// Catches a node that cannot make the lease's scratch directory or cgroup running the
/// action anyway, or reporting it as the client's error.
#[tokio::test]
async fn a_node_that_cannot_prepare_fails_the_lease() {
    let fake = Fake::new("unprepared");
    let spec = Spec::new(&image(), "unused");
    std::fs::remove_dir(fake.cgroup.join("actions")).expect("rm actions");
    let outcome = fake.run(1, &spec, "exit 0").await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("kbf-lease-1-1")),
        "{outcome:?}"
    );
    fake.assert_clean(1);

    std::fs::create_dir(fake.cgroup.join("actions")).expect("mkdir actions");
    std::fs::remove_dir(&fake.scratch).expect("rm scratch");
    let outcome = fake.run(2, &spec, "exit 0").await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(_))),
        "{outcome:?}"
    );
    assert!(!fake.calls().contains(&"create".to_owned()));
}

/// Catches a Podman that disappears mid-lease (a package upgrade) being reported as
/// anything but the farm's failure, or stopping the scratch directory's removal.
#[tokio::test]
async fn a_podman_gone_after_create_fails_the_lease() {
    let fake = Fake::new("vanish");
    fake.knob("vanish-after-create", "");
    let outcome = fake.run(1, &Spec::new(&image(), "unused"), "exit 0").await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.starts_with("run ")),
        "{outcome:?}"
    );
    assert!(!exists(&fake.lease_dir(1)), "scratch left");
}

/// Catches an output whose parent the action replaced with a file failing the lease
/// (it is simply absent), and an output the driver cannot read being reported as
/// absent (the result would be cached without it).
#[tokio::test]
async fn unreadable_outputs_fail_and_shadowed_ones_are_absent() {
    let fake = Fake::new("shadowed");
    let mut spec = Spec::new(&image(), "unused");
    spec.outputs = vec!["gone/x".to_owned()];
    let script = r#"rmdir "$UPPER/gone"; touch "$UPPER/gone""#;
    let result = fake.run(1, &spec, script).await.expect("ran");
    assert!(result.output_files.is_empty(), "{result:?}");
    fake.assert_clean(1);

    spec.outputs = vec!["locked/x".to_owned()];
    let script = r#"touch "$UPPER/locked/x"; chmod 000 "$UPPER/locked""#;
    let outcome = fake.run(2, &spec, script).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("locked/x")),
        "{outcome:?}"
    );
    fake.assert_clean(2);
}

/// Catches the daemon uploading a host file: the action replaces an output's parent
/// directory (`rm -rf d; ln -s /host/dir d`), or its working directory, with a symlink
/// to a host directory and declares `d/key.pem`. Both outputs are left out, the
/// symlink declared as an output itself is recorded as one, and the host file's bytes
/// never reach the CAS.
#[tokio::test]
async fn an_action_cannot_upload_a_host_file_through_a_symlink() {
    let fake = Fake::new("host-link");
    let host = support::scratch("fake-host-link-host");
    let secret = b"host secret, never an output";
    std::fs::write(host.join("key.pem"), secret).expect("plant the host file");
    let host = host.to_string_lossy().into_owned();
    let mut spec = Spec::new(&image(), "unused");
    spec.working_directory = "pkg".to_owned();
    spec.outputs = vec!["d/key.pem".to_owned(), "d".to_owned()];
    let script = format!(r#"rm -rf "$UPPER/pkg/d"; ln -s '{host}' "$UPPER/pkg/d""#);
    let result = fake.run(1, &spec, &script).await.expect("ran");
    assert!(result.output_files.is_empty(), "{result:?}");
    assert_eq!(result.output_symlinks.len(), 1, "{result:?}");
    assert_eq!(result.output_symlinks[0].path, "d");
    assert_eq!(result.output_symlinks[0].target, host);
    fake.assert_clean(1);

    spec.outputs = vec!["key.pem".to_owned()];
    let script = format!(r#"rm -rf "$UPPER/pkg"; ln -s '{host}' "$UPPER/pkg""#);
    let result = fake.run(2, &spec, &script).await.expect("ran");
    assert!(result.output_files.is_empty(), "{result:?}");
    fake.assert_clean(2);
    let digest = kbf_driver_container::cas::digest_of(secret);
    assert_eq!(fake.cas.blob(&digest), None);
}

/// Catches an output that is already an input being run and cached incomplete: the
/// driver reads outputs from the overlay's upper layer, so an output directory `pkg`
/// over the input `pkg/in.txt` would come back holding only what the action wrote, and
/// an unchanged input named as an output file would come back missing. Each is the
/// client's error, refused before Podman creates anything.
#[tokio::test]
async fn outputs_that_are_inputs_are_refused_before_anything_runs() {
    let fake = Fake::new("overlap");
    let mut spec = Spec::new(&image(), "unused");
    spec.inputs = vec![("pkg/in.txt", b"input bytes", false)];
    let script = r#"echo x > "$UPPER/pkg/new""#;
    for (seq, (wd, output)) in [("", "pkg"), ("", "pkg/in.txt"), ("pkg", "in.txt")]
        .into_iter()
        .enumerate()
    {
        let seq = seq as u64 + 1;
        spec.working_directory = wd.to_owned();
        spec.outputs = vec![output.to_owned()];
        let outcome = fake.run(seq, &spec, script).await;
        assert!(
            matches!(outcome, Err(RuntimeError::Invalid(ref why)) if why.contains(output)),
            "{wd:?} {output:?}: {outcome:?}"
        );
        fake.assert_clean(seq);
    }
    assert!(
        !fake.calls().contains(&"create".to_owned()),
        "{:?}",
        fake.calls()
    );

    // A working directory that is an input symlink is refused the same way: the
    // directory the driver makes for it would hide the link.
    spec.symlinks = vec![("lnk", "pkg")];
    spec.working_directory = "lnk".to_owned();
    spec.outputs = vec!["out".to_owned()];
    let outcome = fake.run(5, &spec, script).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Invalid(ref why)) if why.contains("working directory")),
        "{outcome:?}"
    );
    fake.assert_clean(5);
    assert!(!fake.calls().contains(&"create".to_owned()));
    spec.symlinks = Vec::new();

    // An output beside the inputs, under an input directory, still runs.
    spec.working_directory = String::new();
    spec.outputs = vec!["pkg/new".to_owned()];
    let result = fake.run(4, &spec, script).await.expect("ran");
    assert_eq!(result.output_files.len(), 1, "{result:?}");
    assert_eq!(
        blob(&fake.cas, result.output_files[0].digest.as_ref()),
        b"x\n"
    );
    fake.assert_clean(4);
}

/// Catches a kill that waits for a slow prepare step (an input fetch, an image store
/// query) to finish before it stops the lease: the fence of a node that lost contact
/// would wait as long. Here `podman image inspect` hangs for 30 s; the kill must
/// return, with the lease Killed and clean, well before that.
#[tokio::test]
async fn a_kill_during_prepare_stops_it_at_once() {
    let fake = Fake::new("kill-prepare");
    fake.knob("image-hangs", "");
    let action = store_action(&fake.cas, &Spec::new(&image(), "unused"));
    let runtime = Arc::clone(&fake.runtime);
    let run = tokio::spawn(async move { runtime.run(work(1, action, Resources::default())).await });
    for _ in 0..500 {
        if fake.calls().contains(&"image".to_owned()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        fake.calls(),
        ["image"],
        "prepare never reached the image check"
    );
    let kill = fake.runtime.kill(kbf_types::LeaseId::new(1, 1));
    tokio::time::timeout(Duration::from_secs(10), kill)
        .await
        .expect("the kill waited for the prepare step");
    let outcome = run.await.expect("join");
    assert!(matches!(outcome, Err(RuntimeError::Killed)), "{outcome:?}");
    fake.assert_clean(1);
}
