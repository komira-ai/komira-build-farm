//! On macOS, an action without the network cannot hand work to launchd, which would
//! run it outside the sandbox and outside the action's process tree; and the same
//! sandbox still compiles and runs a C program.

#![cfg(target_os = "macos")]

mod support;

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use support::{MemoryCas, Spec, config, run, runtime, scratch, stderr, stdout};

/// The actions' `PATH`: where `launchctl`, `open` and `cc` are.
const PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// `launchctl` with `args`, quietly; whether it succeeded.
fn launchctl(args: &[&str]) -> bool {
    Command::new("/bin/launchctl")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("launchctl")
        .success()
}

/// Whether a process named `name` is running.
fn running(name: &str) -> bool {
    Command::new("/usr/bin/pgrep")
        .args(["-x", name])
        .stdout(Stdio::null())
        .status()
        .expect("pgrep")
        .success()
}

/// Catches the profile not denying `launchctl submit` (a job launchd keeps running
/// after the lease, with the network) or `open` (an application Launch Services
/// starts outside the sandbox). Each is first shown to work from outside the sandbox
/// on this host, so a refusal inside it is the profile's doing.
#[tokio::test]
async fn an_action_cannot_hand_work_to_launchd() {
    let dir = scratch("launchd");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let pid = std::process::id();

    let outside = format!("dev.kbf.test.outside.{pid}");
    let submitted = launchctl(&["submit", "-l", &outside, "--", "/bin/sleep", "120"]);
    let listed = launchctl(&["list", &outside]);
    launchctl(&["remove", &outside]);
    assert!(
        submitted && listed,
        "launchctl submit works outside the sandbox"
    );

    let inside = format!("dev.kbf.test.inside.{pid}");
    let script = format!("launchctl submit -l {inside} -- /bin/sleep 120; echo $?");
    let result = run(&rt, &cas, 1, &Spec::sh(&script).env("PATH", PATH))
        .await
        .expect("ran");
    let escaped = launchctl(&["list", &inside]);
    launchctl(&["remove", &inside]);
    assert!(
        !escaped,
        "the action submitted a launchd job: {} / {}",
        stdout(&cas, &result),
        stderr(&cas, &result)
    );
    assert_ne!(stdout(&cas, &result).trim(), "0", "submit reported success");

    let app = "Calculator";
    let opened = Command::new("/usr/bin/open")
        .args(["-g", "-j", "-n", "-a", app])
        .status()
        .expect("open")
        .success();
    let started = wait_for(|| running(app)).await;
    let _ = Command::new("/usr/bin/pkill").args(["-x", app]).status();
    assert!(opened && started, "open works outside the sandbox");
    assert!(wait_for(|| !running(app)).await, "{app} ended");

    let script = format!("open -g -j -n -a {app}; echo $?");
    let result = run(&rt, &cas, 2, &Spec::sh(&script).env("PATH", PATH))
        .await
        .expect("ran");
    let started = wait_for(|| running(app)).await;
    let _ = Command::new("/usr/bin/pkill").args(["-x", app]).status();
    assert!(
        !started,
        "the action opened {app}: {} / {}",
        stdout(&cas, &result),
        stderr(&cas, &result)
    );
    assert_ne!(stdout(&cas, &result).trim(), "0", "open reported success");
}

/// Catches a profile that breaks an ordinary compile: `cc` (through `xcrun`) compiles
/// and links a C program inside the sandbox, and the program runs.
#[tokio::test]
async fn a_compile_still_works_in_the_sandbox() {
    let dir = scratch("compile");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let script = "printf 'int main(void) { return 42; }\\n' > hello.c && cc -o hello hello.c; ./hello; echo $?";
    let result = run(&rt, &cas, 1, &Spec::sh(script).env("PATH", PATH))
        .await
        .expect("ran");
    assert_eq!(
        stdout(&cas, &result).trim(),
        "42",
        "{}",
        stderr(&cas, &result)
    );
}

/// Waits up to five seconds for `check` to hold.
async fn wait_for(check: impl Fn() -> bool) -> bool {
    for _ in 0..50 {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}
