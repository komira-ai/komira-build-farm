//! A program the container cannot start, against the stand-in `crun`
//! (`support/fake.rs`): the action's own failure, never the farm's.
//! `crun_env.rs` runs the same cases against real rootless `crun`.
//!
//! The fake refuses a start with its `crun-error` knob: it logs the text as an error
//! record, as `crun --log-format json` does, and exits before the action runs.

mod support;

use kbf_daemon::RuntimeError;
use support::fake::{Fake, image};
use support::{Spec, blob};

/// What `crun` 1.14 logs when it cannot find the program `name`.
fn not_found(name: &str) -> String {
    format!("executable file `{name}` not found in $PATH: No such file or directory")
}

/// The fake's next start is refused with `said`.
fn refuse(fake: &Fake, said: &str) {
    fake.knob("crun-error", said);
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
/// looked up on, and `crun`'s own words after it. Also catches the message naming
/// the wrong `PATH` and a lease left behind.
#[tokio::test]
async fn a_program_that_is_not_there_is_the_actions_failure() {
    let fake = Fake::new("program-missing");

    refuse(&fake, &not_found("env"));
    let result = fake
        .run(1, &spec(&["env"], &[]), &["exit", "0"])
        .await
        .expect("the action's own result");
    assert_eq!(result.exit_code, 127);
    let stderr = String::from_utf8(blob(&fake.cas, result.stderr_digest.as_ref())).expect("utf-8");
    assert!(stderr.starts_with("kbf: "), "{stderr}");
    assert!(stderr.contains("`env`"), "{stderr}");
    assert!(stderr.contains("the Command sets no PATH"), "{stderr}");
    assert!(
        stderr.contains(&format!("crun: {}", not_found("env"))),
        "crun's words kept: {stderr}"
    );
    assert!(blob(&fake.cas, result.stdout_digest.as_ref()).is_empty());
    fake.assert_clean(1);

    let env = [("PATH", "/opt/x:/bin")];
    refuse(&fake, &not_found("tool"));
    let result = fake
        .run(2, &spec(&["tool", "-v"], &env), &["exit", "0"])
        .await
        .expect("the action's own result");
    assert_eq!(result.exit_code, 127);
    let stderr = String::from_utf8(blob(&fake.cas, result.stderr_digest.as_ref())).expect("utf-8");
    assert!(stderr.contains("`tool`"), "{stderr}");
    assert!(stderr.contains("PATH \"/opt/x:/bin\""), "{stderr}");
    fake.assert_clean(2);

    refuse(&fake, &not_found("/no/such/tool"));
    let result = fake
        .run(3, &spec(&["/no/such/tool"], &[]), &["exit", "0"])
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
    for (seq, why) in [(1, "Permission denied"), (2, "Operation not permitted")] {
        refuse(&fake, &format!("open executable: {why}"));
        let result = fake
            .run(seq, &spec(&["./data"], &[]), &["exit", "0"])
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
/// lookup, which would make a farm fault the client's. Any other error of `crun` (a
/// missing mount source, a cgroup it may not write) stays the farm's; so does crun
/// naming another program than `argv[0]`. And an action that ran and printed crun's
/// words itself keeps its own exit code.
#[tokio::test]
async fn only_crun_naming_the_program_makes_it_the_actions() {
    let fake = Fake::new("program-farm");
    let cases = [
        "mount `/scratch/root` to `/kbf/root`: No such file or directory".to_owned(),
        "write to `/sys/fs/cgroup/kbf/pids.max`: Operation not permitted".to_owned(),
        not_found("other"),
    ];
    for (seq, said) in (1..).zip(&cases) {
        refuse(&fake, said);
        let outcome = fake.run(seq, &spec(&["env"], &[]), &["exit", "0"]).await;
        assert!(
            matches!(&outcome, Err(RuntimeError::Failed(why))
                if why.contains("did not run") && why.contains(said.as_str())),
            "{said}: {outcome:?}"
        );
        fake.assert_clean(seq);
    }

    std::fs::remove_file(fake.state.join("crun-error")).expect("rm knob");
    let said = format!(
        "Error: unable to start container: crun: {}",
        not_found("env")
    );
    let steps = ["echo", "--stderr", said.as_str(), ";", "exit", "3"];
    let result = fake
        .run(9, &spec(&["env"], &[]), &steps)
        .await
        .expect("ran");
    assert_eq!(result.exit_code, 3);
    let stderr = String::from_utf8(blob(&fake.cas, result.stderr_digest.as_ref())).expect("utf-8");
    assert!(!stderr.contains("kbf: "), "{stderr}");
    fake.assert_clean(9);
}
