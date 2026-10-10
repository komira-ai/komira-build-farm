//! A program the container cannot start, against the stand-in `podman`
//! (`fixtures/fake-podman.sh`): the action's own failure, never the farm's.
//! `podman_env.rs` runs the same cases against real rootless Podman and crun.
//!
//! The fake's `start` stands in for a refused start with `status-override` set to
//! `created 0` (the container never ran) and an action script that prints what Podman
//! prints when crun refuses the program, then exits 125 as `podman start` does.

mod support;

use kbf_daemon::RuntimeError;
use support::fake::{Fake, image};
use support::{Spec, blob};

/// What Podman 4.9 prints when crun 1.14 cannot find the program `name`.
fn not_found(name: &str) -> String {
    format!(
        "Error: unable to start container \"0123456789ab\": crun: executable file `{name}` \
         not found in $PATH: No such file or directory: OCI runtime attempted to invoke a \
         command that was not found"
    )
}

/// A start the fake refuses, printing `said` as `podman start`'s stderr.
fn refused(said: &str) -> String {
    format!("cat >&2 <<'EOF'\n{said}\nEOF\nexit 125\n")
}

fn spec(argv: &[&str], env: &[(&str, &str)]) -> Spec {
    let mut spec = Spec::new(&image(), "unused");
    spec.argv = argv.iter().map(|a| (*a).to_owned()).collect();
    spec.env = env
        .iter()
        .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
        .collect();
    spec
}

/// Catches a Command whose bare `argv[0]` cannot be found (no `PATH` in the Command's
/// environment, which is all the container gets) answered as the farm's fault
/// (`RuntimeError::Failed`, which the client sees as `INTERNAL`): it is the action's
/// result, exit 127 with kbf's message naming the program and the `PATH` it was
/// looked up on, and Podman's own words after it. Also catches the message naming
/// the wrong `PATH` and a lease left behind.
#[tokio::test]
async fn a_program_that_is_not_there_is_the_actions_failure() {
    let fake = Fake::new("program-missing");
    fake.knob("status-override", "created 0\n");

    let result = fake
        .run(1, &spec(&["env"], &[]), &refused(&not_found("env")))
        .await
        .expect("the action's own result");
    assert_eq!(result.exit_code, 127);
    let stderr = String::from_utf8(blob(&fake.cas, result.stderr_digest.as_ref())).expect("utf-8");
    assert!(stderr.starts_with("kbf: "), "{stderr}");
    assert!(stderr.contains("`env`"), "{stderr}");
    assert!(stderr.contains("the Command sets no PATH"), "{stderr}");
    assert!(
        stderr.contains(&not_found("env")),
        "Podman's words kept: {stderr}"
    );
    assert!(blob(&fake.cas, result.stdout_digest.as_ref()).is_empty());
    fake.assert_clean(1);

    let env = [("PATH", "/opt/x:/bin")];
    let result = fake
        .run(
            2,
            &spec(&["tool", "-v"], &env),
            &refused(&not_found("tool")),
        )
        .await
        .expect("the action's own result");
    assert_eq!(result.exit_code, 127);
    let stderr = String::from_utf8(blob(&fake.cas, result.stderr_digest.as_ref())).expect("utf-8");
    assert!(stderr.contains("`tool`"), "{stderr}");
    assert!(stderr.contains("PATH \"/opt/x:/bin\""), "{stderr}");
    fake.assert_clean(2);

    let result = fake
        .run(
            3,
            &spec(&["/no/such/tool"], &[]),
            &refused(&not_found("/no/such/tool")),
        )
        .await
        .expect("the action's own result");
    assert_eq!(result.exit_code, 127);
    let stderr = String::from_utf8(blob(&fake.cas, result.stderr_digest.as_ref())).expect("utf-8");
    assert!(stderr.contains("`/no/such/tool`"), "{stderr}");
    fake.assert_clean(3);
}

/// Catches a program that is there but cannot be executed (a file without the
/// executable bit, a directory) answered as the farm's fault, or as not found: it is
/// the action's result, exit 126.
#[tokio::test]
async fn a_program_that_cannot_be_executed_is_the_actions_failure() {
    let fake = Fake::new("program-not-executable");
    fake.knob("status-override", "created 0\n");
    for (seq, why) in [(1, "Permission denied"), (2, "Operation not permitted")] {
        let said = format!(
            "Error: unable to start container \"0123456789ab\": crun: open executable: \
             {why}: OCI permission denied"
        );
        let result = fake
            .run(seq, &spec(&["./data"], &[]), &refused(&said))
            .await
            .expect("the action's own result");
        assert_eq!(result.exit_code, 126, "{why}");
        let stderr =
            String::from_utf8(blob(&fake.cas, result.stderr_digest.as_ref())).expect("utf-8");
        assert!(stderr.contains("`./data`"), "{stderr}");
        assert!(stderr.contains("not an executable file"), "{stderr}");
        fake.assert_clean(seq);
    }
}

/// Catches the classification trusting anything but crun's report of the program
/// lookup, which would make a farm fault the client's. Podman wraps every runtime
/// "No such file or directory" as "a command that was not found" (a missing mount
/// source among them) and every "Operation not permitted" as "OCI permission denied"
/// (a cgroup it may not write), so its wrapping alone stays the farm's; so does crun
/// naming another program than `argv[0]`. And an action that ran and printed crun's
/// words itself keeps its own exit code.
#[tokio::test]
async fn only_crun_naming_the_program_makes_it_the_actions() {
    let fake = Fake::new("program-farm");
    fake.knob("status-override", "created 0\n");
    let cases = [
        "Error: unable to start container \"0123456789ab\": crun: mount `/scratch/root` to \
         `/kbf/root`: No such file or directory: OCI runtime attempted to invoke a command \
         that was not found",
        "Error: unable to start container \"0123456789ab\": crun: write to \
         `/sys/fs/cgroup/kbf/pids.max`: Operation not permitted: OCI permission denied",
        &not_found("other"),
    ];
    for (seq, said) in (1..).zip(cases) {
        let outcome = fake.run(seq, &spec(&["env"], &[]), &refused(said)).await;
        assert!(
            matches!(&outcome, Err(RuntimeError::Failed(why))
                if why.contains("did not run") && why.contains(said)),
            "{said}: {outcome:?}"
        );
        fake.assert_clean(seq);
    }

    std::fs::remove_file(fake.state.join("status-override")).expect("rm knob");
    let script = format!(
        "{}exit 3\n",
        refused(&not_found("env")).replace("exit 125\n", "")
    );
    let result = fake
        .run(9, &spec(&["env"], &[]), &script)
        .await
        .expect("ran");
    assert_eq!(result.exit_code, 3);
    let stderr = String::from_utf8(blob(&fake.cas, result.stderr_digest.as_ref())).expect("utf-8");
    assert!(!stderr.contains("kbf: "), "{stderr}");
    fake.assert_clean(9);
}
