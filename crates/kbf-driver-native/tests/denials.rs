//! A failed action under the sandbox gets the sandbox's refusals of its processes at
//! the end of its stderr, capped, and no other process's. The log is a stand-in here
//! (a reader the test gives the config), the sandbox is the testkit's `sandbox-exec`,
//! which runs the command unsandboxed; `tests/sandbox.rs` runs the real ones on macOS.

mod support;

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use kbf_driver_native::network::Isolation;
use kbf_driver_native::{NativeConfig, NativeRuntime};
use support::{MemoryCas, Spec, config, run, runtime, scratch, stderr};

static READS: AtomicUsize = AtomicUsize::new(0);

/// The sandbox's log as a test sets it: a denial by the action (whose pid its `pgid`
/// step printed to stdout) and one by a process of another lease. `file` is in the
/// lease directory, next to `stdout`.
fn log_with_two(file: &Path, _: Duration) -> io::Result<String> {
    READS.fetch_add(1, Ordering::SeqCst);
    let out = std::fs::read_to_string(file.with_file_name("stdout"))?;
    let pid = out.trim();
    Ok(format!(
        "x Sandbox: touch({pid}) deny(1) file-write-create /outside/mine\n\
         x Sandbox: touch(1) deny(1) file-write-create /elsewhere/theirs\n\
         x Sandbox: touch({pid}) allow(1) file-write-create /inside\n"
    ))
}

/// A reader that panics.
fn log_panics(_: &Path, _: Duration) -> io::Result<String> {
    panic!("the reader panics");
}

/// A reader that makes stderr read-only and then reports a refusal of the action.
fn log_locks_stderr(file: &Path, last: Duration) -> io::Result<String> {
    use std::os::unix::fs::PermissionsExt as _;
    let stderr = file.with_file_name("stderr");
    std::fs::set_permissions(&stderr, std::fs::Permissions::from_mode(0o444))?;
    log_with_two(file, last)
}

/// Only a process of another lease was refused.
fn log_others_only(_: &Path, _: Duration) -> io::Result<String> {
    Ok("x Sandbox: touch(1) deny(1) file-write-create /elsewhere/theirs\n".to_owned())
}

fn log_down(_: &Path, _: Duration) -> io::Result<String> {
    READS.fetch_add(1, Ordering::SeqCst);
    Err(io::Error::other("log is down"))
}

fn sandboxed(name: &str) -> (NativeConfig, Arc<MemoryCas>) {
    let dir = scratch(name);
    let mut config = config(&dir);
    let link = dir.join("sandbox-exec");
    kbf_testkit::act::link_self(&link).expect("link");
    std::fs::write(format!("{}.rules", link.display()), "").expect("rules");
    config.isolation = Isolation::Sandbox(link);
    (config, Arc::new(MemoryCas::default()))
}

/// Catches: refusals never reaching the result (the operator sees `EPERM` only), another
/// process's paths in this lease's stderr, an `allow` line listed, a note on an action
/// that passed (and a log read for it), the action's own stderr or exit code lost, a
/// log that cannot be read failing the lease, and a read without a sandbox.
#[tokio::test]
async fn a_failed_action_gets_its_own_denials_in_its_stderr() {
    let (mut config, cas) = sandboxed("denials-note");
    config.denial_log = log_with_two;
    let rt = runtime(config.clone(), &cas);

    let before = READS.load(Ordering::SeqCst);
    let spec = Spec::act("pgid ; echo --stderr own failure ; exit 3");
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 3);
    let text = stderr(&cas, &result);
    assert!(text.starts_with("own failure\n"), "{text}");
    assert!(
        text.contains("kbf: the macOS sandbox denied this action 1 operation(s)"),
        "{text}"
    );
    assert!(text.contains("kbf:   touch("), "{text}");
    assert!(text.contains("file-write-create /outside/mine\n"), "{text}");
    assert!(!text.contains("/elsewhere/theirs"), "{text}");
    assert!(!text.contains("/inside"), "{text}");
    assert!(
        text.contains("1 more denial(s) by processes this lease did not watch"),
        "{text}"
    );
    assert_eq!(READS.load(Ordering::SeqCst), before + 1);

    // A pass is not read.
    let result = run(&rt, &cas, 2, &Spec::act("pgid")).await.expect("ran");
    assert_eq!(result.exit_code, 0);
    assert_eq!(stderr(&cas, &result), "");
    assert_eq!(READS.load(Ordering::SeqCst), before + 1);

    // A log that cannot be read leaves the result as the action made it.
    let (mut config, cas) = sandboxed("denials-down");
    config.denial_log = log_down;
    let rt = runtime(config, &cas);
    let result = run(&rt, &cas, 1, &Spec::act("echo --stderr own ; exit 4"))
        .await
        .expect("ran");
    assert_eq!(result.exit_code, 4);
    assert_eq!(stderr(&cas, &result), "own\n");

    // A failure with no refusal of its own keeps its stderr as it was: nothing says a
    // sandbox failed it, and another lease's paths are not this lease's to show.
    let (mut config, cas) = sandboxed("denials-others");
    config.denial_log = log_others_only;
    let rt = runtime(config, &cas);
    let spec = Spec::act("echo --stderr own failure ; exit 3");
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    assert_eq!(stderr(&cas, &result), "own failure\n");

    // A note that would push stderr past the stdio limit is left out: the limit
    // would drop the action's own stderr with it.
    let (mut config, cas) = sandboxed("denials-limit");
    config.denial_log = log_with_two;
    config.outputs.max_stdio_bytes = 100;
    let rt = runtime(config, &cas);
    let spec = Spec::act("pgid ; echo --stderr own failure ; exit 3");
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 3);
    assert_eq!(stderr(&cas, &result), "own failure\n");

    // A reader that panics, and a stderr that cannot be appended to, leave the result
    // as the action made it.
    for (name, reader) in [
        (
            "denials-panic",
            log_panics as kbf_driver_native::denials::LogReader,
        ),
        ("denials-locked", log_locks_stderr),
    ] {
        let (mut config, cas) = sandboxed(name);
        config.denial_log = reader;
        let rt = runtime(config, &cas);
        let spec = Spec::act("pgid ; echo --stderr own failure ; exit 3");
        let result = run(&rt, &cas, 1, &spec).await.expect("ran");
        assert_eq!(result.exit_code, 3, "{name}");
        assert_eq!(stderr(&cas, &result), "own failure\n", "{name}");
    }

    // Without a sandbox there is nothing to read.
    let dir = scratch("denials-none");
    let mut config = config_without_sandbox(&dir);
    config.denial_log = log_down;
    let cas = Arc::new(MemoryCas::default());
    let rt: NativeRuntime<MemoryCas> = runtime(config, &cas);
    let reads = READS.load(Ordering::SeqCst);
    let result = run(&rt, &cas, 1, &Spec::act("exit 5")).await.expect("ran");
    assert_eq!(result.exit_code, 5);
    assert_eq!(READS.load(Ordering::SeqCst), reads);
}

fn config_without_sandbox(dir: &Path) -> NativeConfig {
    let mut config = config(dir);
    config.isolation = Isolation::None;
    config
}
