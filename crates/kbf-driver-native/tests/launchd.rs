//! On macOS, an action without the network cannot hand work to launchd, which would
//! run it outside the sandbox and outside the action's process tree; and the same
//! sandbox still compiles and runs a C program.

#![cfg(target_os = "macos")]

mod support;

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use support::{MemoryCas, Spec, config, run, runtime, scratch, stderr, stdout};

/// The actions' `PATH`: where `launchctl`, `open` and `cc` are.
const PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// The ways to give launchd a job, as shell commands: `LABEL` is the job's label and
/// `PLIST` a property list that defines it.
const WAYS: [(&str, &str); 3] = [
    ("submit", "launchctl submit -l LABEL -- /bin/sleep 120"),
    ("load", "launchctl load PLIST"),
    ("bootstrap", "launchctl bootstrap gui/$(id -u) PLIST"),
];

/// `sh -c script`, quietly; whether it succeeded.
fn sh(script: &str) -> bool {
    Command::new("/bin/sh")
        .args(["-c", script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("sh")
        .success()
}

/// Writes a job definition for `label` (a long sleep) into `dir`; its path.
fn plist(dir: &Path, label: &str) -> String {
    let path = dir.join(format!("{label}.plist"));
    let text = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>\
         <key>Label</key><string>{label}</string>\
         <key>ProgramArguments</key><array><string>/bin/sleep</string><string>120</string></array>\
         <key>RunAtLoad</key><true/>\
         </dict></plist>\n"
    );
    std::fs::write(&path, text).expect("plist");
    path.to_string_lossy().into_owned()
}

/// Whether launchd has a job `label`, in the caller's domain or the GUI one.
fn loaded(label: &str) -> bool {
    sh(&format!(
        "launchctl list {label} || launchctl print gui/$(id -u)/{label}"
    ))
}

/// Removes the job `label` wherever it was loaded.
fn unload(label: &str) {
    sh(&format!(
        "launchctl remove {label}; launchctl bootout gui/$(id -u)/{label}"
    ));
}

/// Catches the profile not stopping an action from giving launchd a job (`launchctl
/// submit`, `load` or `bootstrap`): launchd would keep it running after the lease,
/// outside the sandbox and with the network. Each way is first shown to work from
/// outside the sandbox on this host, so a refusal inside it is the sandbox's doing.
#[tokio::test]
async fn an_action_cannot_give_launchd_a_job() {
    let dir = scratch("launchd");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let pid = std::process::id();
    for (seq, (way, command)) in (1..).zip(WAYS) {
        let outside = format!("dev.kbf.test.{way}.outside.{pid}");
        let script = command
            .replace("LABEL", &outside)
            .replace("PLIST", &plist(&dir, &outside));
        let ran = sh(&script);
        let escaped = loaded(&outside);
        unload(&outside);
        assert!(ran && escaped, "{way} works outside the sandbox");

        let inside = format!("dev.kbf.test.{way}.inside.{pid}");
        let script = command
            .replace("LABEL", &inside)
            .replace("PLIST", &plist(&dir, &inside));
        let spec = Spec::sh(&format!("{script}; echo $?")).env("PATH", PATH);
        let result = run(&rt, &cas, seq, &spec).await.expect("ran");
        let escaped = loaded(&inside);
        unload(&inside);
        assert!(
            !escaped,
            "the action gave launchd a job with {way}: {} / {}",
            stdout(&cas, &result),
            stderr(&cas, &result)
        );
    }
}

/// Whether a process named `name` is running.
fn running(name: &str) -> bool {
    sh(&format!("pgrep -x {name}"))
}

/// Catches the profile not stopping an action from `open`ing an application, which
/// Launch Services starts outside the sandbox and the action's tree. `open` is first
/// shown to work from outside the sandbox on this host.
#[tokio::test]
async fn an_action_cannot_open_an_application() {
    let dir = scratch("lsopen");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let app = "Calculator";
    let open = format!("open -g -j -n -a {app}");
    let opened = sh(&open);
    let started = wait_for(|| running(app)).await;
    sh(&format!("pkill -x {app}"));
    assert!(opened && started, "open works outside the sandbox");
    assert!(wait_for(|| !running(app)).await, "{app} ended");

    let spec = Spec::sh(&format!("{open}; echo $?")).env("PATH", PATH);
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    let started = wait_for(|| running(app)).await;
    sh(&format!("pkill -x {app}"));
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
