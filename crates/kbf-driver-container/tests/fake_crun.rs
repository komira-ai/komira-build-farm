//! The driver's steps against a stand-in `crun` (`support/fake.rs`) that runs each
//! action as a plain process, and a plain directory standing in for the cgroup mount.
//! These run on any Linux machine; `crun.rs` runs the same promises against real
//! rootless `crun`.

mod conformance;
mod fake_cache;
mod fake_clock;
mod fake_directory_blobs;
mod fake_memory;
mod fake_missing_blobs;
mod fake_oci;
mod fake_usage;
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::{Runtime, RuntimeError};
use kbf_types::Resources;
use support::fake::{Fake, image};
use support::{Spec, blob, exists, store_action, tree, work};
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
        ("tool.sh", b"a tool\n", true),
    ];
    spec.symlinks = vec![("pkg/link-in", "src/in.txt")];
    spec.outputs = vec![
        "out/copy.txt".to_owned(),
        "out/dir".to_owned(),
        "out/link".to_owned(),
        "out/never-written".to_owned(),
    ];
    let steps = [
        "test",
        "-d",
        "$UPPER/pkg/out",
        ";",
        "test",
        "-x",
        "$ROOT/tool.sh",
        ";",
        "test",
        "!",
        "-x",
        "$ROOT/pkg/src/in.txt",
        ";",
        "points-to",
        "$ROOT/pkg/link-in",
        "src/in.txt",
        ";",
        "copy",
        "$UPPER/pkg/out/copy.txt",
        "$ROOT/pkg/src/in.txt",
        ";",
        "mkdir",
        "$UPPER/pkg/out/dir/sub",
        ";",
        "write-lines",
        "$UPPER/pkg/out/dir/sub/n.txt",
        "nested",
        ";",
        "write-lines",
        "$UPPER/pkg/out/dir/run.sh",
        "a program",
        ";",
        "chmod",
        "755",
        "$UPPER/pkg/out/dir/run.sh",
        ";",
        "symlink",
        "sub/n.txt",
        "$UPPER/pkg/out/dir/to-n",
        ";",
        "mkfifo",
        "$UPPER/pkg/out/dir/fifo",
        ";",
        "symlink",
        "copy.txt",
        "$UPPER/pkg/out/link",
        ";",
        "copy",
        "$UPPER/pkg/out/dir/limits",
        "$CG/memory.max",
        "$CG/memory.oom.group",
        "$CG/cpu.weight",
        "$CG/cgroup.subtree_control",
        ";",
        "echo",
        "to-stdout",
        ";",
        "echo",
        "--stderr",
        "to-stderr",
        ";",
        "exit",
        "3",
    ];
    let result = fake.run(1, &spec, &steps).await.expect("ran");
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
    // memory.max = 1 GiB x 1.5 + 512 MiB; memory.oom.group = 1; cpu.weight = 2000
    // millicpus / 10.
    assert_eq!(
        blob(&fake.cas, root.files[0].digest.as_ref()),
        format!("{}1200+cpu +memory +pids", (3u64 << 29) + (512 << 20)).as_bytes()
    );
    assert_eq!(root.symlinks[0].name, "to-n");
    assert_eq!(root.symlinks[0].target, "sub/n.txt");
    assert_eq!(root.directories[0].name, "sub");
    assert_eq!(tree.children.len(), 1);
    assert_eq!(tree.children[0].files[0].name, "n.txt");
    assert_eq!(
        dir.root_directory_digest.as_ref(),
        Some(&kbf_daemon::cas::digest_of(&prost::Message::encode_to_vec(
            &root
        )))
    );

    fake.assert_clean(1);
    assert!(fake.calls().contains(&"delete".to_owned()));
}

/// Catches `--lease-placement=numa-node` not reaching the lease cgroup: the lease must
/// get its node's CPUs and memory node and hand `cpuset` to its container's cgroup, and
/// the first lease of an idle host goes to the lowest node id.
#[tokio::test]
async fn a_placed_lease_gets_its_nodes_cpus_and_memory() {
    use kbf_driver_container::{CpuSet, Placer};
    let nodes = vec![
        (0, "0-1,4".parse::<CpuSet>().expect("cpus")),
        (1, "2-3,5".parse::<CpuSet>().expect("cpus")),
    ];
    let fake = Fake::with("placed", |config| config.placer = Placer::new(nodes));
    let mut spec = Spec::new(&image(), "unused by the fake");
    spec.outputs = vec!["cpus".to_owned(), "mems".to_owned(), "subtree".to_owned()];
    let steps = [
        "copy",
        "$UPPER/cpus",
        "$CG/cpuset.cpus",
        ";",
        "copy",
        "$UPPER/mems",
        "$CG/cpuset.mems",
        ";",
        "copy",
        "$UPPER/subtree",
        "$CG/cgroup.subtree_control",
    ];
    let result = fake.run(1, &spec, &steps).await.expect("ran");
    assert_eq!(result.exit_code, 0);
    let output = |name: &str| {
        let file = result.output_files.iter().find(|f| f.path == name);
        blob(&fake.cas, file.expect(name).digest.as_ref())
    };
    assert_eq!(output("cpus"), b"0-1,4");
    assert_eq!(output("mems"), b"0");
    assert_eq!(output("subtree"), b"+cpu +memory +pids +cpuset");
    fake.assert_clean(1);
}

/// Runs lease 1 of a fake over two nodes (`memory` says what the placer knows of the
/// node's memory), booking `memory_bytes`. The action copies out `cgroup.subtree_control`
/// and, with `pinned`, `cpuset.cpus` and `cpuset.mems`; without it, it fails unless neither
/// file exists. Returns the copied files in that order.
async fn lease_cgroup(
    name: &str,
    memory: kbf_driver_container::NodeMemory,
    memory_bytes: u64,
    pinned: bool,
) -> Vec<Vec<u8>> {
    use kbf_driver_container::{CpuSet, Placer};
    let nodes = vec![
        (0, "0-1,4".parse::<CpuSet>().expect("cpus")),
        (1, "2-3,5".parse::<CpuSet>().expect("cpus")),
    ];
    let fake = Fake::with(name, |config| {
        config.placer = Placer::with_memory(nodes, memory);
    });
    let mut spec = Spec::new(&image(), "unused by the fake");
    let mut steps: Vec<String> = Vec::new();
    let mut outputs = Vec::new();
    for (file, out) in [("cpuset.cpus", "cpus"), ("cpuset.mems", "mems")] {
        let cgroup_file = format!("$CG/{file}");
        if pinned {
            outputs.push(out);
            steps.extend(["copy".to_owned(), format!("$UPPER/{out}"), cgroup_file]);
        } else {
            steps.extend(["test", "!", "-e"].map(str::to_owned));
            steps.push(cgroup_file);
        }
        steps.push(";".to_owned());
    }
    outputs.push("subtree");
    for step in ["copy", "$UPPER/subtree", "$CG/cgroup.subtree_control"] {
        steps.push(step.to_owned());
    }
    spec.outputs = outputs.iter().map(|o| (*o).to_owned()).collect();
    let result = fake
        .run_with(1, &spec, &steps, Resources::new(2000, memory_bytes))
        .await
        .expect("ran");
    assert_eq!(result.exit_code, 0, "{name}");
    let got = outputs
        .iter()
        .map(|name| {
            let file = result.output_files.iter().find(|f| f.path == *name);
            blob(&fake.cas, file.expect(name).digest.as_ref())
        })
        .collect();
    fake.assert_clean(1);
    got
}

/// Catches a lease under `--lease-placement=numa-node` that books the whole host's memory
/// (the last rung of the memory ladder) or more than one node holds, pinned to one node: its
/// cgroup must get no `cpuset.cpus` or `cpuset.mems` (so every CPU and memory node the
/// daemon may use) and `cpuset` must not be handed to its container's cgroup; a share of a
/// node is pinned as before. The host is 4 GiB of two 2 GiB nodes.
#[tokio::test]
async fn whole_host_and_over_node_leases_get_no_cpuset_and_a_share_does() {
    use kbf_driver_container::NodeMemory;
    const GIB: u64 = 1 << 30;
    let memory = NodeMemory {
        whole_host: Some(4 * GIB),
        largest_node: Some(2 * GIB),
    };
    let whole = lease_cgroup("whole-host", memory, 4 * GIB, false).await;
    assert_eq!(whole, [b"+cpu +memory +pids".to_vec()]);
    let over = lease_cgroup("over-node", memory, 3 * GIB, false).await;
    assert_eq!(over, [b"+cpu +memory +pids".to_vec()]);
    let share = lease_cgroup("share", memory, GIB, true).await;
    assert_eq!(
        share,
        [
            b"0-1,4".to_vec(),
            b"0".to_vec(),
            b"+cpu +memory +pids +cpuset".to_vec()
        ]
    );
}

/// Catches a lease cgroup that gets cpuset files without `--lease-placement=numa-node`:
/// the default must leave a lease on any CPU, as before (the limits test above pins
/// `cgroup.subtree_control` to `+cpu +memory +pids`).
#[tokio::test]
async fn an_unplaced_lease_gets_no_cpuset() {
    let fake = Fake::new("unplaced");
    let spec = Spec::new(&image(), "unused by the fake");
    let steps = [
        "test",
        "!",
        "-e",
        "$CG/cpuset.cpus",
        ";",
        "test",
        "!",
        "-e",
        "$CG/cpuset.mems",
    ];
    let result = fake.run(1, &spec, &steps).await.expect("ran");
    assert_eq!(result.exit_code, 0);
    fake.assert_clean(1);
}

/// The marker test: the action writes outside its output directory (beside the input
/// root, into its lease cgroup). Catches a skipped clean step: after the lease ends,
/// nothing of the marker may remain.
#[tokio::test]
async fn the_marker_test_nothing_outside_the_outputs_survives() {
    let fake = Fake::new("leftovers");
    let spec = Spec::new(&image(), "unused");
    let steps = [
        "write-lines",
        "$UPPER/../marker",
        "marker",
        ";",
        "write-lines",
        "$UPPER/marker",
        "marker",
        ";",
        "mkdir",
        "$CG/marker-cgroup",
    ];
    let result = fake.run(1, &spec, &steps).await.expect("ran");
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

/// Catches an image named by tag reaching `crun` (the "tag instead of a digest"
/// mutant): refused as the client's error before anything is created.
#[tokio::test]
async fn a_tag_is_refused_before_anything_runs() {
    let fake = Fake::new("tag");
    let spec = Spec::new("docker://registry.test/tools/busybox:latest", "unused");
    let outcome = fake.run(1, &spec, &["exit", "0"]).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Invalid(ref why)) if why.contains("digest")),
        "{outcome:?}"
    );
    assert!(fake.calls().is_empty(), "crun was run: {:?}", fake.calls());
    fake.assert_clean(1);
}

/// Catches (issue #45) a timed-out action's partial logs read with no limit, and one
/// log past the stdio limit failing the lease or taking the other log with it: the
/// timeout is still the answer, the log within the limit is stored, the one past it is
/// left out. Each stream in turn, so a limit applied to one only shows.
#[tokio::test]
async fn a_timed_out_log_past_its_limit_is_left_out() {
    const MAX: u64 = 16;
    let fake = Fake::with("timeout-stdio", |config| {
        config.outputs.max_stdio_bytes = MAX;
    });
    let mut spec = Spec::new(&image(), "unused");
    spec.timeout = Some(Duration::from_secs(1));
    let past = (MAX + 1).to_string();
    let past = past.as_str();
    let cases = [
        (
            1,
            [
                "zeros", past, ";", "echo", "--stderr", "kept", ";", "sleep", "30",
            ],
        ),
        (
            2,
            [
                "echo", "kept", ";", "zeros", "--stderr", past, ";", "sleep", "30",
            ],
        ),
    ];
    for (seq, steps) in cases {
        let outcome = fake.run(seq, &spec, &steps).await;
        let Err(RuntimeError::TimedOut { stdout, stderr }) = outcome else {
            panic!("{steps:?}: {outcome:?}");
        };
        let (kept, left_out) = if seq == 1 {
            (stderr, stdout)
        } else {
            (stdout, stderr)
        };
        assert_eq!(left_out, None, "{steps:?}");
        assert_eq!(blob(&fake.cas, kept.as_ref()), b"kept\n", "{steps:?}");
        fake.assert_clean(seq);
    }
}

/// Catches the fake running an action through a shell, or through any program but
/// the act program it is given (`kbf-act` beside `crun`, a link to this test
/// binary). The process the run starts, whose pid the fake publishes, must execute this
/// binary: a shell that forks the act program shows its own executable there. Its
/// environment must be the fake `crun`'s own plus the four the fake sets: a shell that
/// `exec`s the act program in its place (as `bash -c` does with a single command)
/// leaves the same executable, but adds what it exports, such as bash's `_`. (Seen:
/// with the fake starting `bash -c '"$0" "$@"' kbf-act ...`, the executable check
/// alone passed.)
#[tokio::test]
async fn the_action_is_the_act_program_and_no_shell() {
    let fake = Fake::new("act-exe");
    fake.action(&["sleep", "2"]);
    let action = store_action(&fake.cas, &Spec::new(&image(), "unused"));
    let runtime = Arc::clone(&fake.runtime);
    let run = tokio::spawn(async move { runtime.run(work(1, action, Resources::default())).await });
    fake.wait_for_start().await;
    let read = |name: &str| {
        let text = std::fs::read_to_string(fake.state.join(name)).expect(name);
        text.trim().to_owned()
    };
    let (pid, start) = (read("pid"), read("start-pid"));
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).expect("its executable");
    let this = std::env::current_exe().expect("this test binary");
    assert_eq!(exe, this, "the action runs another program");
    // What a process was started with, less what the fake gives the action.
    let environment = |pid: &str| -> Vec<String> {
        let given = ["CG=", "ROOT=", "UPPER=", "FAKE_LEASE="];
        let bytes = std::fs::read(format!("/proc/{pid}/environ")).expect("environ");
        let mut entries: Vec<String> = bytes
            .split(|b| *b == 0)
            .filter(|e| !e.is_empty())
            .map(|e| String::from_utf8_lossy(e).into_owned())
            .filter(|e| !given.iter().any(|g| e.starts_with(g)))
            .collect();
        entries.sort();
        entries
    };
    assert_eq!(
        environment(&pid),
        environment(&start),
        "something between `crun run` and the action changed its environment"
    );
    let result = run.await.expect("join").expect("ran");
    assert_eq!(result.exit_code, 0);
    fake.assert_clean(1);
}

/// Catches the kill path skipping `cgroup.kill` after the SIGTERM grace period: an
/// action that ignores SIGTERM must die from the lease cgroup's `cgroup.kill`, and
/// `crun run` must then end on its own within the second grace period. Without that
/// step the run is aborted after the second grace period (so it never records its end)
/// and only the clean step's `cgroup.kill` ends the action.
#[tokio::test]
async fn sigterm_ignored_falls_back_to_cgroup_kill() {
    // Slack for a loaded machine, without changing what is checked. The fake notices
    // cgroup.kill by polling, and the default 300 ms grace can pass before it looks;
    // a skipped cgroup.kill still shows, since the run then ends only by the abort
    // after the second grace. The timeout gives the action time to ignore SIGTERM: a
    // SIGTERM before that ends it in the first grace, with no cgroup.kill.
    let fake = Fake::with("cgroup-kill", |c| c.kill_grace = Duration::from_secs(3));
    let mut spec = Spec::new(&image(), "unused");
    spec.timeout = Some(Duration::from_secs(2));
    // SIGTERM is 15.
    let steps = ["ignore-signal", "15", ";", "sleep", "30"];
    let outcome = fake.run(1, &spec, &steps).await;
    assert!(
        matches!(outcome, Err(RuntimeError::TimedOut { .. })),
        "{outcome:?}"
    );
    // The overlay's owners are `userns.rs`'s to check.
    let events: Vec<_> = fake
        .events()
        .into_iter()
        .filter(|e| !e.starts_with("chown"))
        .collect();
    let expected = ["cgroup.kill ended the action", "run ended", "delete"];
    assert_eq!(events, expected, "{events:?}");
    fake.assert_clean(1);
}

/// Catches a dropped run whose clean does not kill the lease cgroup before `crun delete`
/// (a run dropped mid-start leaves `crun create` in the lease, and `delete --force` does
/// not wait for it), and a dropped run whose clean fails leaving what it could remove:
/// the scratch directory is gone, the action ended, and `crun run` was reaped before the
/// delete.
#[tokio::test]
async fn a_dropped_run_kills_the_cgroup_before_the_delete_and_survives_a_failed_one() {
    let fake = Fake::new("dropped-delete");
    let spec = Spec::new(&image(), "unused");
    fake.action(&["sleep", "30"]);
    let action = store_action(&fake.cas, &spec);
    let runtime = Arc::clone(&fake.runtime);
    let run = tokio::spawn(async move { runtime.run(work(1, action, Resources::default())).await });
    fake.wait_for_start().await;
    run.abort();
    assert!(run.await.expect_err("cancelled").is_cancelled());
    fake.assert_clean(1);
    assert!(fake.calls().contains(&"delete".to_owned()));
    assert!(
        fake.state.join("killed-before-delete").exists(),
        "crun delete ran before cgroup.kill"
    );

    // A dropped run whose clean fails still removes what it can (and logs the rest).
    fake.knob("delete-fails", "");
    let action = store_action(&fake.cas, &spec);
    let runtime = Arc::clone(&fake.runtime);
    let run = tokio::spawn(async move { runtime.run(work(2, action, Resources::default())).await });
    std::fs::remove_file(fake.state.join("pid")).expect("rm pid");
    fake.wait_for_start().await;
    fake.assert_action_running(2);
    run.abort();
    assert!(run.await.expect_err("cancelled").is_cancelled());
    assert!(!exists(&fake.lease_dir(2)), "scratch left");
    assert!(
        !fake.state.join("run-not-reaped-at-delete").exists(),
        "crun delete ran before crun run was reaped"
    );
    // The clean's cgroup.kill ended the action although `crun delete` then failed.
    fake.assert_action_gone(2);
}

/// Catches (issue #157) a dropped run whose clean removes the lease while the `crun run`
/// it started may still be there: the run would write into the removed directories
/// after the clean, as the flake did. `conformance`'s dropped run, 200 times over with
/// the CPUs busy (`support::CpuHog`), so a timing one run passes by luck shows up. The
/// mutant skips `reap` in the lease's clean: `delete` then finds the run still there.
#[tokio::test]
async fn no_round_of_a_dropped_run_races_the_clean() {
    let _hog = support::CpuHog::start();
    for round in 0..200 {
        let fake = Fake::new(&format!("dropped-stress-{round}"));
        fake.action(&["sleep", "30"]);
        let action = store_action(&fake.cas, &Spec::new(&image(), "unused"));
        let runtime = Arc::clone(&fake.runtime);
        let run =
            tokio::spawn(async move { runtime.run(work(1, action, Resources::default())).await });
        fake.wait_for_start().await;
        run.abort();
        assert!(run.await.expect_err("cancelled").is_cancelled());
        fake.assert_clean(1);
    }
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
    let outcome = fake.run(1, &spec, &["exit", "0"]).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("upper/out/nnn")),
        "{outcome:?}"
    );
    fake.assert_clean(1);

    let spec = Spec::new(&image(), "unused");
    for (seq, log) in [(2, "stdout"), (3, "stderr")] {
        fake.knob("block-log", log);
        let outcome = fake.run(seq, &spec, &["exit", "0"]).await;
        let want = format!("kbf-lease-1-{seq}/{log}: ");
        assert!(
            matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains(&want)),
            "{log}: {outcome:?}"
        );
        fake.assert_clean(seq);
    }
    std::fs::remove_file(fake.state.join("block-log")).expect("rm knob");

    // The fake runs the action beside the lease directory, so it can unlink a log.
    let outcome = fake.run(4, &spec, &["remove", "$ROOT/../stderr"]).await;
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
    fake.runtime_breaks();
    fake.knob("delete-fails", "");
    let outcome = fake
        .run(1, &Spec::new(&image(), "unused"), &["exit", "0"])
        .await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why))
            if why.contains("cannot create the container") && !why.contains("delete refused")),
        "{outcome:?}"
    );
}

/// Catches actions that cannot be run safely reaching `crun`: no arguments, an
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
        let outcome = fake.run(seq as u64 + 1, spec, &["exit", "0"]).await;
        assert!(
            matches!(outcome, Err(RuntimeError::Invalid(_))),
            "{outcome:?}"
        );
        fake.assert_clean(seq as u64 + 1);
    }
    assert!(fake.calls().is_empty(), "crun was run: {:?}", fake.calls());
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

/// Catches an Action without a Command or an input root being run with defaults. A
/// missing blob is `fake_missing_blobs`'s.
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
        ..action
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
    for seq in 1..=2 {
        fake.assert_clean(seq);
    }
    assert_eq!(fake.starts(), 0);
}

/// Catches the action's environment not reaching the bundle, and an output path that is
/// neither file, directory nor symlink being reported as an output.
#[tokio::test]
async fn the_environment_reaches_the_bundle_and_odd_outputs_are_left_out() {
    let fake = Fake::new("env");
    let mut spec = Spec::new(&image(), "unused");
    spec.env = vec![("FOO".to_owned(), "a b=c".to_owned())];
    spec.outputs = vec!["pipe".to_owned()];
    let result = fake
        .run(1, &spec, &["mkfifo", "$UPPER/pipe"])
        .await
        .expect("ran");
    let env = fake.config_json()["process"]["env"].clone();
    assert!(
        env.as_array()
            .is_some_and(|env| env.iter().any(|e| e == "FOO=a b=c")),
        "{env}"
    );
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
    let outcome = fake.run(1, &spec, &["exit", "0"]).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("kbf-lease-1-1")),
        "{outcome:?}"
    );
    fake.assert_clean(1);

    // A file where the lease's scratch directory goes.
    std::fs::create_dir(fake.cgroup.join("actions")).expect("mkdir actions");
    std::fs::write(fake.lease_dir(2), b"in the way").expect("a file in the way");
    let outcome = fake.run(2, &spec, &["exit", "0"]).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(_))),
        "{outcome:?}"
    );
    assert_eq!(fake.starts(), 0);
}

/// Catches an output whose parent the action replaced with a file failing the lease
/// (it is simply absent), and an output the driver cannot read being reported as
/// absent (the result would be cached without it).
#[tokio::test]
async fn unreadable_outputs_fail_and_shadowed_ones_are_absent() {
    let fake = Fake::new("shadowed");
    let mut spec = Spec::new(&image(), "unused");
    spec.outputs = vec!["gone/x".to_owned()];
    let steps = ["remove", "$UPPER/gone", ";", "write-lines", "$UPPER/gone"];
    let result = fake.run(1, &spec, &steps).await.expect("ran");
    assert!(result.output_files.is_empty(), "{result:?}");
    fake.assert_clean(1);

    spec.outputs = vec!["locked/x".to_owned()];
    let steps = [
        "write-lines",
        "$UPPER/locked/x",
        ";",
        "chmod",
        "000",
        "$UPPER/locked",
    ];
    let outcome = fake.run(2, &spec, &steps).await;
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
    let steps = [
        "remove",
        "$UPPER/pkg/d",
        ";",
        "symlink",
        host.as_str(),
        "$UPPER/pkg/d",
    ];
    let result = fake.run(1, &spec, &steps).await.expect("ran");
    assert!(result.output_files.is_empty(), "{result:?}");
    assert_eq!(result.output_symlinks.len(), 1, "{result:?}");
    assert_eq!(result.output_symlinks[0].path, "d");
    assert_eq!(result.output_symlinks[0].target, host);
    fake.assert_clean(1);

    spec.outputs = vec!["key.pem".to_owned()];
    let steps = [
        "remove",
        "$UPPER/pkg",
        ";",
        "symlink",
        host.as_str(),
        "$UPPER/pkg",
    ];
    let result = fake.run(2, &spec, &steps).await.expect("ran");
    assert!(result.output_files.is_empty(), "{result:?}");
    fake.assert_clean(2);
    let digest = kbf_daemon::cas::digest_of(secret);
    assert_eq!(fake.cas.blob(&digest), None);
}

/// Catches an action taking the daemon down through the depth of its outputs: an
/// output directory 3000 levels deep overflowed the recursive walk's stack (SIGABRT,
/// every lease on the node lost; seen red: this test binary aborts). It fails the
/// action alone, naming the limit's flag, and the next action on the same runtime
/// runs.
#[tokio::test]
async fn an_output_too_deep_fails_the_action_not_the_daemon() {
    let fake = Fake::new("deep");
    let tree = support::scratch("fake-deep-tree");
    support::deep(&tree, 3000, b"bottom");
    let mut spec = Spec::new(&image(), "unused");
    spec.outputs = vec!["out".to_owned()];
    let tree = tree.display().to_string();
    let steps = ["rename", tree.as_str(), "$UPPER/out"];
    let outcome = fake.run(1, &spec, &steps).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("--output-max-depth")),
        "{outcome:?}"
    );
    fake.assert_clean(1);
    let steps = [
        "mkdir",
        "$UPPER/out",
        ";",
        "write-lines",
        "$UPPER/out/f",
        "x",
    ];
    let result = fake
        .run(2, &spec, &steps)
        .await
        .expect("the next action runs");
    assert_eq!(result.output_directories.len(), 1, "{result:?}");
    fake.assert_clean(2);
}

/// Inputs named as outputs come back as the action left them (REAPI collects outputs
/// after the action, from its whole tree; the native driver reads them from its input
/// root). Catches each such output read from the upper layer without its inputs: an
/// unchanged input file missing or an output directory without its unchanged files
/// (the inputs never moved up), an input left in the lower layer as well (on a real
/// overlay a delete would then be a whiteout and an edit a copy-up), the inputs moved
/// to the upper layer's root instead of the working directory, outputs taken in the
/// client's order (the file `dir/keep.txt` moved up first, then `dir` cannot move onto
/// the parent made for it), and an input beside the outputs moved too.
#[tokio::test]
async fn inputs_named_as_outputs_come_back_as_the_action_left_them() {
    let fake = Fake::new("overlap");
    let mut spec = Spec::new(&image(), "unused");
    spec.working_directory = "pkg".to_owned();
    spec.inputs = vec![
        ("pkg/same.txt", b"same bytes", false),
        ("pkg/edit.txt", b"old", true),
        ("pkg/other.txt", b"other", false),
        ("pkg/dir/keep.txt", b"keep", false),
        ("pkg/dir/gone.txt", b"gone", false),
        ("pkg/dir/changed.txt", b"before", false),
        ("pkg/dir/sub/deep.txt", b"deep", false),
    ];
    spec.outputs = ["dir/keep.txt", "same.txt", "edit.txt", "dir"]
        .map(str::to_owned)
        .to_vec();
    // The fake runs the action on the two layers apart: what it sees in `$UPPER` is
    // what a real overlay would hold in its upper directory.
    let mut steps = Vec::new();
    for p in ["same.txt", "edit.txt", "dir"] {
        steps.extend(["test", "!", "-e"].map(str::to_owned));
        steps.extend([format!("$ROOT/pkg/{p}"), ";".to_owned()]);
    }
    steps.extend(
        [
            "holds",
            "$ROOT/pkg/other.txt",
            "other",
            ";",
            "test",
            "!",
            "-e",
            "$UPPER/pkg/other.txt",
            ";",
            "holds",
            "$UPPER/pkg/same.txt",
            "same bytes",
            ";",
            "test",
            "-x",
            "$UPPER/pkg/edit.txt",
            ";",
            "append",
            "$UPPER/pkg/edit.txt",
            " new",
            ";",
            "remove",
            "$UPPER/pkg/dir/gone.txt",
            ";",
            "write-lines",
            "$UPPER/pkg/dir/changed.txt",
            "after",
        ]
        .map(str::to_owned),
    );
    let result = fake.run(1, &spec, &steps).await.expect("ran");
    assert_eq!(
        result.exit_code,
        0,
        "stderr: {}",
        String::from_utf8_lossy(&blob(&fake.cas, result.stderr_digest.as_ref()))
    );
    let file = |path: &str| {
        let found = result.output_files.iter().find(|f| f.path == path);
        let found = found.unwrap_or_else(|| panic!("{path} not collected: {result:?}"));
        (blob(&fake.cas, found.digest.as_ref()), found.is_executable)
    };
    assert_eq!(file("same.txt"), (b"same bytes".to_vec(), false));
    assert_eq!(file("edit.txt"), (b"old new\n".to_vec(), true));
    assert_eq!(file("dir/keep.txt"), (b"keep".to_vec(), false));
    assert_eq!(result.output_files.len(), 3, "{result:?}");
    assert_eq!(result.output_directories.len(), 1, "{result:?}");
    let dir = tree(&fake.cas, result.output_directories[0].tree_digest.as_ref());
    let root = dir.root.expect("root");
    let names: Vec<_> = root.files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["changed.txt", "keep.txt"], "gone.txt deleted");
    assert_eq!(blob(&fake.cas, root.files[0].digest.as_ref()), b"after\n");
    assert_eq!(blob(&fake.cas, root.files[1].digest.as_ref()), b"keep");
    assert_eq!(root.directories.len(), 1, "{root:?}");
    assert_eq!(dir.children[0].files[0].name, "deep.txt");
    assert_eq!(
        blob(&fake.cas, dir.children[0].files[0].digest.as_ref()),
        b"deep"
    );
    fake.assert_clean(1);
}

/// Catches the parents of the outputs made through an input symlink that another
/// output moved into the upper layer: from there `lnk` points at the scratch directory
/// that holds the leases, so following it would make `escaped/y` there, outside the
/// lease. The action is the client's error, as on the native driver, and nothing is
/// created.
#[tokio::test]
async fn an_output_below_an_input_symlink_named_as_an_output_is_refused() {
    let fake = Fake::new("overlap-link");
    let mut spec = Spec::new(&image(), "unused");
    spec.working_directory = "pkg".to_owned();
    spec.inputs = vec![("pkg/in.txt", b"input bytes", false)];
    spec.symlinks = vec![("pkg/lnk", "../../..")];
    spec.outputs = vec!["lnk".to_owned(), "lnk/escaped/y".to_owned()];
    let outcome = fake.run(1, &spec, &["exit", "0"]).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Invalid(ref why)) if why.contains("lnk")),
        "{outcome:?}"
    );
    fake.assert_clean(1);
    let scratch = fake.lease_dir(1);
    let scratch = scratch.parent().expect("the scratch directory");
    assert!(!exists(&scratch.join("escaped")));
    assert_eq!(fake.starts(), 0);
}

/// Catches a working directory hidden by an input symlink being run: the directory the
/// driver makes for it in the upper layer would hide the link, so the action would
/// start in an empty directory. Also catches an output beside the inputs, under an
/// input directory, not running.
#[tokio::test]
async fn a_hidden_working_directory_is_refused_before_anything_runs() {
    let fake = Fake::new("hidden-wd");
    let mut spec = Spec::new(&image(), "unused");
    spec.inputs = vec![("pkg/in.txt", b"input bytes", false)];
    let steps = ["write-lines", "$UPPER/pkg/new", "x"];
    spec.symlinks = vec![("lnk", "pkg")];
    spec.working_directory = "lnk".to_owned();
    spec.outputs = vec!["out".to_owned()];
    let outcome = fake.run(1, &spec, &steps).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Invalid(ref why)) if why.contains("working directory")),
        "{outcome:?}"
    );
    fake.assert_clean(1);
    assert_eq!(fake.starts(), 0);
    spec.symlinks = Vec::new();

    // An output beside the inputs, under an input directory, still runs.
    spec.working_directory = String::new();
    spec.outputs = vec!["pkg/new".to_owned()];
    let result = fake.run(2, &spec, &steps).await.expect("ran");
    assert_eq!(result.output_files.len(), 1, "{result:?}");
    assert_eq!(
        blob(&fake.cas, result.output_files[0].digest.as_ref()),
        b"x\n"
    );
    fake.assert_clean(2);
}

/// Catches a kill that waits for a slow prepare step (an input fetch, an image store
/// query) to finish before it stops the lease: the fence of a node that lost contact
/// would wait as long. Here the helper container that gives the lease's directories to
/// the container hangs for 30 s; the kill must return, with the lease Killed and clean,
/// well before that.
#[tokio::test]
async fn a_kill_during_prepare_stops_it_at_once() {
    let fake = Fake::new("kill-prepare");
    fake.knob("give-hangs", "");
    let action = store_action(&fake.cas, &Spec::new(&image(), "unused"));
    let runtime = Arc::clone(&fake.runtime);
    let run = tokio::spawn(async move { runtime.run(work(1, action, Resources::default())).await });
    for _ in 0..500 {
        if fake.calls().contains(&"helper give".to_owned()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        fake.calls(),
        ["helper give"],
        "prepare never reached the hand-over"
    );
    let kill = fake.runtime.kill(kbf_types::LeaseId::new(1, 1));
    tokio::time::timeout(Duration::from_secs(10), kill)
        .await
        .expect("the kill waited for the prepare step");
    let outcome = run.await.expect("join");
    assert!(matches!(outcome, Err(RuntimeError::Killed)), "{outcome:?}");
    fake.assert_clean(1);
}

/// Catches the clean step recursing into what the action left behind: a junk tree
/// 20,000 levels deep that is not an output overflowed `std::fs::remove_dir_all`'s
/// recursion on a tokio blocking thread (SIGABRT, every lease on the node lost; seen
/// red: this test binary aborts). The lease succeeds and leaves nothing, and the next
/// lease on the same runtime runs.
#[tokio::test]
async fn a_deep_junk_tree_is_cleaned_without_taking_the_daemon_down() {
    let fake = Fake::new("deep-junk");
    let tree = support::scratch("fake-deep-junk-tree");
    support::deep(&tree, 20_000, b"bottom");
    let spec = Spec::new(&image(), "unused");
    let from = tree.display().to_string();
    let steps = ["rename", from.as_str(), "$UPPER/junk"];
    let result = fake.run(1, &spec, &steps).await.expect("ran");
    assert_eq!(result.exit_code, 0);
    assert!(!exists(&tree), "the junk tree never reached the lease");
    assert!(!fake.calls().contains(&"helper rm".to_owned()));
    fake.assert_clean(1);
    let result = fake
        .run(2, &spec, &["exit", "0"])
        .await
        .expect("the next lease");
    assert_eq!(result.exit_code, 0);
    fake.assert_clean(2);
}

/// Catches stdout and stderr read with no limit, the limit off by one, or applied to
/// one stream only: a stdout of exactly `--output-max-stdio-bytes` (three chunks) is
/// stored whole, and one byte more of stdout, or of stderr, fails the action naming
/// the flag and the log, with the lease cleaned.
#[tokio::test]
async fn stdout_and_stderr_are_stored_within_their_limit() {
    const MAX: u64 = 3 << 20;
    let fake = Fake::with("stdio", |config| config.outputs.max_stdio_bytes = MAX);
    let spec = Spec::new(&image(), "unused");
    let result = fake
        .run(1, &spec, &["zeros", MAX.to_string().as_str()])
        .await
        .expect("ran");
    let stdout = blob(&fake.cas, result.stdout_digest.as_ref());
    assert!(stdout.len() as u64 == MAX && stdout.iter().all(|&b| b == 0));
    fake.assert_clean(1);
    let past = (MAX + 1).to_string();
    let past = past.as_str();
    for (seq, log, steps) in [
        (2, "stdout", vec!["zeros", past]),
        (3, "stderr", vec!["zeros", "--stderr", past]),
    ] {
        let outcome = fake.run(seq, &spec, &steps).await;
        let want = format!("kbf-lease-1-{seq}/{log}: ");
        assert!(
            matches!(outcome, Err(RuntimeError::Failed(ref why))
                if why.contains(&want) && why.contains("--output-max-stdio-bytes")),
            "{log}: {outcome:?}"
        );
        fake.assert_clean(seq);
    }
}
