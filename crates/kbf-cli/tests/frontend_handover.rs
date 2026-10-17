//! `kbf frontend handover`, the real binary, against a fake `systemctl`
//! (`tests/bin/fake_systemctl.rs`, linked as `<dir>/systemctl`): it keeps each unit's
//! `ActiveState` in a file, logs every call, and starts a unit by setting it `active`, as a
//! `Type=notify` start that was told `READY=1` does. It refuses any verb but `show` and
//! `start`, so a stop, restart or kill by the command fails the test.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const KBF: &str = env!("CARGO_BIN_EXE_kbf");
const FAKE: &str = env!("CARGO_BIN_EXE_kbf-cli-fake-systemctl");

struct Fake {
    dir: PathBuf,
}

impl Fake {
    fn new(name: &str) -> Fake {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("frontend_handover")
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch");
        std::os::unix::fs::symlink(FAKE, dir.join("systemctl")).expect("link");
        Fake { dir }
    }

    fn state(&self, instance: &str, state: &str) {
        fs::write(self.dir.join(format!("state.{}", unit(instance))), state).expect("state");
    }

    fn file(&self, name: &str, text: &str) {
        fs::write(self.dir.join(name), text).expect("file");
    }

    fn states(&self) -> (String, String) {
        let read = |i: &str| {
            fs::read_to_string(self.dir.join(format!("state.{}", unit(i))))
                .map_or_else(|_| "inactive".to_owned(), |s| s.trim().to_owned())
        };
        (read("a"), read("b"))
    }

    /// The calls made, one per line.
    fn log(&self) -> Vec<String> {
        fs::read_to_string(self.dir.join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// `kbf frontend handover [--wait]`: the exit code, stdout and stderr.
    fn handover(&self, wait: bool) -> (i32, String, String) {
        let mut cmd = Command::new(KBF);
        cmd.args(["frontend", "handover", "--systemctl"])
            .arg(self.dir.join("systemctl"));
        if wait {
            cmd.args(["--wait", "--poll-ms=1"]);
        }
        let out = cmd.output().expect("run kbf");
        (
            out.status.code().expect("an exit code"),
            String::from_utf8(out.stdout).expect("utf-8"),
            String::from_utf8(out.stderr).expect("utf-8"),
        )
    }
}

fn unit(instance: &str) -> String {
    format!("kbf-frontend@{instance}.service")
}

const SHOW_A: &str = "show -p ActiveState --value kbf-frontend@a.service";
const SHOW_B: &str = "show -p ActiveState --value kbf-frontend@b.service";

/// Catches a handover that starts the instance that serves (a restart of the serving
/// frontend under another name), starts both, calls any verb of systemctl other than
/// `show` and `start` (a stop, restart or kill would fail the builds in flight), or says
/// the old instance has exited when it has not.
#[test]
fn the_idle_instance_is_started_and_the_serving_one_is_left_alone() {
    for (serving, idle) in [("a", "b"), ("b", "a")] {
        let fake = Fake::new(&format!("idle-{idle}"));
        fake.state(serving, "active");
        let (code, out, err) = fake.handover(false);
        assert_eq!((code, err.as_str()), (0, ""), "{out}");
        let start = format!("start kbf-frontend@{idle}.service");
        assert_eq!(fake.log(), [SHOW_A, SHOW_B, start.as_str()]);
        assert_eq!(
            out,
            format!(
                "kbf frontend handover: kbf-frontend@{serving}.service serves; starting kbf-frontend@{idle}.service to take over from it\n\
                 kbf frontend handover: kbf-frontend@{idle}.service is ready and serves; kbf-frontend@{serving}.service drains its open builds and exits when they are done\n"
            )
        );
        let (a, b) = fake.states();
        assert_eq!((a.as_str(), b.as_str()), ("active", "active"));
    }
}

/// Catches a start that failed reported as a success, a failed start that is followed by a
/// stop or restart of the old instance, and a message that does not say whether the old
/// one still serves (the operator's next step depends on it).
#[test]
fn a_start_that_fails_leaves_the_serving_instance_serving_and_says_so() {
    let fake = Fake::new("start-fails");
    fake.state("a", "active");
    fake.file("start-fails", "");
    let (code, out, err) = fake.handover(false);
    assert_eq!(code, 1, "{out}");
    assert_eq!(
        err,
        "kbf frontend handover: kbf-frontend@b.service did not become ready: systemctl start failed (exit status: 1): Job for kbf-frontend@b.service failed.\n\
         kbf frontend handover: kbf-frontend@a.service is still active; it stands down only for a new instance that is ready (see `journalctl -u kbf-frontend@b.service`)\n"
    );
    assert_eq!(
        fake.log(),
        [SHOW_A, SHOW_B, "start kbf-frontend@b.service", SHOW_A]
    );

    // The old instance is gone when the start fails: the message says no frontend serves.
    let fake = Fake::new("start-fails-old-gone");
    fake.state("a", "active");
    fake.file("start-fails", "");
    fake.file("looks.kbf-frontend@a.service", "1");
    let (code, _, err) = fake.handover(false);
    assert_eq!(code, 1);
    assert!(
        err.contains("kbf-frontend@a.service is no longer active either, and no frontend serves"),
        "{err}"
    );
}

/// Catches a start that goes ahead when the instances are not in a state that can hand
/// over: both active (the old one is still draining, and starting an instance would stop
/// nothing but would take the listeners from a frontend that cannot give them), one
/// activating or deactivating (a handover or a stop is under way), and a systemctl that
/// cannot be run or cannot show a unit.
#[test]
fn nothing_is_started_while_a_handover_is_draining_or_an_instance_is_changing() {
    let rows = [
        (
            "both-active",
            ("active", "active"),
            "kbf-frontend@a.service and kbf-frontend@b.service are both active: the one an earlier handover replaced is still draining its open builds and exits when they are done; run this again then\n",
        ),
        (
            "activating",
            ("active", "activating"),
            "kbf-frontend@b.service is activating: run this again when it is active or stopped\n",
        ),
        (
            "deactivating",
            ("deactivating", "inactive"),
            "kbf-frontend@a.service is deactivating: run this again when it is active or stopped\n",
        ),
    ];
    for (name, (a, b), why) in rows {
        let fake = Fake::new(name);
        fake.state("a", a);
        fake.state("b", b);
        let (code, out, err) = fake.handover(false);
        assert_eq!((code, out.as_str()), (1, ""), "{name}");
        assert_eq!(err, format!("kbf frontend handover: {why}"), "{name}");
        assert_eq!(fake.log(), [SHOW_A, SHOW_B], "{name}: nothing was started");
    }
    let fake = Fake::new("show-fails");
    fake.file("show-fails", "");
    let (code, _, err) = fake.handover(false);
    assert_eq!(code, 1);
    assert!(
        err.contains("show kbf-frontend@a.service: no such unit"),
        "{err}"
    );
}

/// Catches a host with no frontend up that is left without one (the command is how the
/// first start and a start after a crash are done too), and a first start that picks the
/// second instance (the one that is enabled at boot is the first).
#[test]
fn with_no_instance_up_the_first_is_started() {
    for (name, (a, b)) in [
        ("none", ("inactive", "inactive")),
        ("failed", ("failed", "inactive")),
    ] {
        let fake = Fake::new(name);
        fake.state("a", a);
        fake.state("b", b);
        let (code, out, err) = fake.handover(false);
        assert_eq!((code, err.as_str()), (0, ""), "{name}");
        assert_eq!(
            out,
            "kbf frontend handover: no frontend serves; starting kbf-frontend@a.service\n\
             kbf frontend handover: kbf-frontend@a.service is ready and serves\n"
        );
        assert_eq!(fake.log(), [SHOW_A, SHOW_B, "start kbf-frontend@a.service"]);
    }
}

/// Catches `--wait` that returns before the old instance has exited, one that hangs on a
/// stopped instance, and a result other than `success` (the old frontend crashed while it
/// drained: its builds failed) reported as a clean end.
#[test]
fn wait_follows_the_old_instance_to_its_exit() {
    let fake = Fake::new("wait");
    fake.state("a", "active");
    // It is active for three looks (the first of them the command's own), then it has exited.
    fake.file("looks.kbf-frontend@a.service", "3");
    let (code, out, err) = fake.handover(true);
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    assert!(
        out.ends_with("kbf-frontend@a.service has exited\n"),
        "{out}"
    );
    let log = fake.log();
    let looks = log.iter().filter(|l| l.as_str() == SHOW_A).count();
    assert_eq!(
        looks, 4,
        "three looks while it is active, one inactive: {log:?}"
    );
    assert_eq!(fake.states().0, "inactive");

    let fake = Fake::new("wait-failed");
    fake.state("a", "active");
    fake.file("looks.kbf-frontend@a.service", "1");
    fake.file("result.kbf-frontend@a.service", "exit-code\n");
    let (code, _, err) = fake.handover(true);
    assert_eq!(code, 1);
    assert!(
        err.contains("kbf-frontend@a.service ended with result exit-code"),
        "{err}"
    );
}

/// Catches a start that fails with no instance serving reported as a handover that left
/// the old one up (there is none: the message must say to read the new instance's log), a
/// failed start whose follow-up look at the old instance cannot be made (the failure must
/// still be reported, with what systemctl said), and a `--wait` that cannot look at the
/// old instance or cannot read its result (neither is a clean exit).
#[test]
fn a_systemctl_that_fails_to_answer_fails_the_command_with_what_it_said() {
    let fake = Fake::new("no-old-start-fails");
    fake.file("start-fails", "");
    let (code, out, err) = fake.handover(false);
    assert_eq!(
        (code, out.as_str()),
        (
            1,
            "kbf frontend handover: no frontend serves; starting kbf-frontend@a.service\n"
        )
    );
    assert_eq!(
        err,
        "kbf frontend handover: kbf-frontend@a.service did not become ready: systemctl start failed (exit status: 1): Job for kbf-frontend@a.service failed.\n\
         kbf frontend handover: see `journalctl -u kbf-frontend@a.service`\n"
    );

    let fake = Fake::new("start-fails-then-show-fails");
    fake.state("a", "active");
    fake.file("start-fails", "");
    fake.file("show-fails-after-start", "");
    let (code, _, err) = fake.handover(false);
    assert_eq!(code, 1);
    assert!(
        err.contains("show kbf-frontend@a.service: no such unit"),
        "{err}"
    );

    let fake = Fake::new("wait-show-fails");
    fake.state("a", "active");
    fake.file("show-fails-after-start", "");
    let (code, out, err) = fake.handover(true);
    assert_eq!(code, 1, "{out}");
    assert!(
        err.contains("show kbf-frontend@a.service: no such unit"),
        "{err}"
    );

    let fake = Fake::new("wait-result-fails");
    fake.state("a", "active");
    fake.file("looks.kbf-frontend@a.service", "1");
    fake.file("result-fails", "");
    let (code, out, err) = fake.handover(true);
    assert_eq!(code, 1, "{out}");
    assert!(
        err.contains("show kbf-frontend@a.service: no such unit"),
        "{err}"
    );
}
