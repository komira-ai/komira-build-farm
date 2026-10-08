//! On macOS, no action can hand work to launchd, which would run it outside the
//! sandbox and outside the action's process tree, whether or not it has the network;
//! and the sandbox still compiles and runs a C program.
//!
//! Each refusal is checked against two controls on the same host: the same action run
//! by the same driver without a sandbox (it must reach launchd, or the test proves
//! nothing), and the same commands under a bare `(allow default)` sandbox (what
//! launchd and Launch Services do with any sandboxed caller, before any rule of ours).

#![cfg(target_os = "macos")]

mod support;

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use kbf_driver_native::NativeRuntime;
use kbf_driver_native::network::{Isolation, SANDBOX_EXEC};
use support::{MemoryCas, Spec, config, run, runtime, scratch, stderr, stdout};

/// The actions' `PATH`: where `launchctl`, `open` and `cc` are.
const PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// A sandbox with no rule of ours in it.
const BARE_PROFILE: &str = "(version 1)\n(allow default)\n";

/// The ways to give launchd a job, as shell commands: `LABEL` is the job's label and
/// `PLIST` a property list that defines it.
const WAYS: [(&str, &str); 3] = [
    ("submit", "launchctl submit -l LABEL -- /bin/sleep 120"),
    ("load", "launchctl load PLIST"),
    ("bootstrap", "launchctl bootstrap gui/$(id -u) PLIST"),
];

/// `sh -c script` from the test itself, quietly; whether it succeeded.
fn sh(script: &str) -> bool {
    Command::new("/bin/sh")
        .args(["-c", script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("sh")
        .success()
}

/// `script` under the bare sandbox, from the test itself.
fn bare(script: &str) {
    let _ = Command::new(SANDBOX_EXEC)
        .args(["-p", BARE_PROFILE, "/bin/sh", "-c", script])
        .env("PATH", PATH)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("sandbox-exec");
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

/// Whether launchd has a job `label`, in the caller's domain or the GUI one; the job
/// is removed either way.
fn escaped(label: &str) -> bool {
    let loaded = sh(&format!(
        "launchctl list {label} || launchctl print gui/$(id -u)/{label}"
    ));
    sh(&format!(
        "launchctl remove {label}; launchctl bootout gui/$(id -u)/{label}"
    ));
    loaded
}

/// The driver over `cas` with its sandbox (as the node runs it), and the same driver
/// with none: the control.
fn runtimes(
    dir: &Path,
    cas: &Arc<MemoryCas>,
) -> (NativeRuntime<MemoryCas>, NativeRuntime<MemoryCas>) {
    let sandboxed = config(dir);
    assert!(matches!(sandboxed.isolation, Isolation::Sandbox(_)));
    let mut open = config(&dir.join("unsandboxed"));
    open.isolation = Isolation::None;
    (runtime(sandboxed, cas), runtime(open, cas))
}

/// An action running `script` with the network off or on.
fn action(script: &str, network: &str) -> Spec {
    Spec::sh(&format!("{script}; echo $?"))
        .env("PATH", PATH)
        .property("network", network)
}

/// Catches an action, with or without the network, that can give launchd a job
/// (`launchctl submit`, `load` or `bootstrap`): launchd would keep it running after
/// the lease, outside the sandbox and with the network. Controls: each way reaches
/// launchd from the same action run without a sandbox; and each is refused under a
/// bare sandbox, so that refusal is launchd's own, and `(deny job-creation)` in the
/// profile is defence in depth (its mutants stayed green).
#[tokio::test]
async fn no_action_can_give_launchd_a_job() {
    let dir = scratch("launchd");
    let cas = Arc::new(MemoryCas::default());
    let (sandboxed, unsandboxed) = runtimes(&dir, &cas);
    let pid = std::process::id();
    let mut seq = 0;
    for (way, command) in WAYS {
        let fill = |label: &str| {
            command
                .replace("LABEL", label)
                .replace("PLIST", &plist(&dir, label))
        };
        seq += 1;
        let label = format!("dev.kbf.test.{way}.control.{pid}");
        let result = run(&unsandboxed, &cas, seq, &action(&fill(&label), "on"))
            .await
            .expect("ran");
        assert!(
            escaped(&label),
            "control: {way} does not reach launchd even unsandboxed: {} / {}",
            stdout(&cas, &result),
            stderr(&cas, &result)
        );

        let label = format!("dev.kbf.test.{way}.bare.{pid}");
        bare(&fill(&label));
        assert!(
            !escaped(&label),
            "control: launchd took a job ({way}) from a bare sandbox, so the docs that \
             call (deny job-creation) defence in depth are wrong"
        );

        for network in ["off", "on"] {
            seq += 1;
            let label = format!("dev.kbf.test.{way}.{network}.{pid}");
            let result = run(&sandboxed, &cas, seq, &action(&fill(&label), network))
                .await
                .expect("ran");
            assert!(
                !escaped(&label),
                "an action with network={network} gave launchd a job with {way}: {} / {}",
                stdout(&cas, &result),
                stderr(&cas, &result)
            );
        }
    }
}

/// Whether a process named `name` is running.
fn running(name: &str) -> bool {
    sh(&format!("pgrep -x {name}"))
}

/// Whether `app` starts within a few seconds; it is ended either way.
async fn started(app: &str) -> bool {
    let up = wait_for(|| running(app)).await;
    sh(&format!("pkill -x {app}"));
    assert!(wait_for(|| !running(app)).await, "{app} ended");
    up
}

/// Catches an action, with or without the network, that can `open` an application,
/// which Launch Services starts outside the sandbox and the action's tree. Controls:
/// the same action opens it without a sandbox, and so does a bare sandbox, so it is
/// `(deny lsopen)` in the profile that refuses it.
#[tokio::test]
async fn no_action_can_open_an_application() {
    let dir = scratch("lsopen");
    let cas = Arc::new(MemoryCas::default());
    let (sandboxed, unsandboxed) = runtimes(&dir, &cas);
    let app = "Calculator";
    let open = format!("open -g -j -n -a {app}");

    let result = run(&unsandboxed, &cas, 1, &action(&open, "on"))
        .await
        .expect("ran");
    assert!(
        started(app).await,
        "control: no app opened unsandboxed: {}",
        stderr(&cas, &result)
    );
    bare(&open);
    assert!(started(app).await, "control: a bare sandbox opens {app}");

    for (seq, network) in [(2, "off"), (3, "on")] {
        let result = run(&sandboxed, &cas, seq, &action(&open, network))
            .await
            .expect("ran");
        assert!(
            !started(app).await,
            "an action with network={network} opened {app}: {} / {}",
            stdout(&cas, &result),
            stderr(&cas, &result)
        );
        assert_ne!(stdout(&cas, &result).trim(), "0", "open reported success");
    }
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
    for (seq, network) in [(1, "off"), (2, "on")] {
        let spec = Spec::sh(script)
            .env("PATH", PATH)
            .property("network", network);
        let result = run(&rt, &cas, seq, &spec).await.expect("ran");
        assert_eq!(
            stdout(&cas, &result).trim(),
            "42",
            "{}",
            stderr(&cas, &result)
        );
    }
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
