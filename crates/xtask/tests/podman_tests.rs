//! `cargo xtask podman-tests` as CI runs it: the real binary, with `xtask-fake` as
//! cargo (printing the JSON messages each test writes for it) and as sudo (recording
//! the `systemd-run` it was asked to run).

use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::podman_tests::{BUSYBOX_AMD64, BUSYBOX_ARM64, TARGETS, arch_for};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const INDEX: &str = "docker.io/library/busybox@sha256:\
                     bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e";
const PATH: &str = "/usr/bin:/bin";
const HOME: &str = "/srv/runner";

/// This machine's pinned busybox manifest.
fn this_manifest() -> &'static str {
    let machine = rustix::system::uname()
        .machine()
        .to_string_lossy()
        .into_owned();
    match arch_for(&machine).expect("a machine these tests run on") {
        "amd64" => BUSYBOX_AMD64,
        _ => BUSYBOX_ARM64,
    }
}

/// One run's directories: the repository root cargo runs in, and the fakes' directory.
struct Run {
    root: PathBuf,
    bin: PathBuf,
}

impl Run {
    /// An empty root for one test, a fake sudo, and the messages the fake cargo prints:
    /// each target's test binary (`<root>/target/debug/deps/<target>-0f1e`, from the
    /// package at `<root>/crates/kbf-driver-container`), after a decoy.
    fn new(name: &str) -> Run {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("xpt")
            .join(name);
        let _ = std::fs::remove_dir_all(&root);
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("scratch");
        std::fs::hard_link(FAKE, bin.join("sudo")).expect("link the fake");
        let package = root.join("crates/kbf-driver-container");
        std::fs::create_dir_all(&package).expect("package directory");
        let mut messages = String::from(
            r#"{"reason":"compiler-artifact","target":{"kind":["lib"],"name":"kbf_driver_container"},"profile":{"test":false},"executable":null,"manifest_path":"/w/Cargo.toml"}"#,
        );
        messages.push('\n');
        for target in TARGETS {
            messages.push_str(&format!(
                r#"{{"reason":"compiler-artifact","target":{{"kind":["test"],"name":"{}"}},"profile":{{"test":true}},"executable":"{}","manifest_path":"{}"}}"#,
                target.name,
                root.join(format!("target/debug/deps/{}-0f1e", target.name))
                    .display(),
                package.join("Cargo.toml").display()
            ));
            messages.push('\n');
        }
        std::fs::write(root.join("cargo-messages"), messages).expect("messages");
        Run { root, bin }
    }

    /// `podman-tests` with the fakes, the cargo `cargo`, and the sudo `program` (`None`:
    /// the fake), planting `defect` (or none): exit code, stdout and stderr.
    fn xtask(
        &self,
        cargo: &Path,
        program: Option<&Path>,
        defect: Option<&str>,
    ) -> (Option<i32>, String, String) {
        let fake = self.bin.join("sudo");
        let mut cmd = Command::new(XTASK);
        cmd.arg("--root").arg(&self.root).arg("podman-tests");
        cmd.arg("--sudo").arg(program.unwrap_or(fake.as_path()));
        cmd.env("CARGO", cargo).env("HOME", HOME).env("PATH", PATH);
        match defect {
            Some(d) => cmd.env("XTASK_FAKE_DEFECT", d),
            None => cmd.env_remove("XTASK_FAKE_DEFECT"),
        };
        let out = cmd.output().expect("run xtask");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn log(&self, path: PathBuf) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    /// What the fake cargo was asked to do, one line per run; empty when it never ran.
    fn cargo_args(&self) -> String {
        self.log(self.root.join("target/debug/cargo-args"))
    }

    /// What the fake sudo was asked to run, one `<dir>: <args>` line per run.
    fn sudo_args(&self) -> String {
        self.log(self.bin.join("sudo-args"))
    }

    /// The `sudo-args` line of the unit that runs `target`'s binary.
    fn unit(&self, target: &str, extra_env: &str, extra_args: &str) -> String {
        let uid = rustix::process::getuid().as_raw();
        let gid = rustix::process::getgid().as_raw();
        let package = self.root.join("crates/kbf-driver-container");
        format!(
            "{cwd}: -n systemd-run --quiet --wait --collect --pipe \
             --unit=kbf-podman-tests-{target} --slice=kbf-daemon.slice -p Delegate=yes \
             -p OOMScoreAdjust=-900 --uid={uid} --gid={gid} --working-directory={package} \
             -E HOME={HOME} -E PATH={PATH} \
             -E KBF_TEST_IMAGE=docker://docker.io/library/busybox@{manifest} \
             -E KBF_TEST_INDEX_IMAGE=docker://{INDEX}{extra_env} {exe} --include-ignored{extra_args}\n",
            cwd = std::env::current_dir().expect("cwd").display(),
            package = package.display(),
            manifest = this_manifest(),
            exe = self
                .root
                .join(format!("target/debug/deps/{target}-0f1e"))
                .display(),
        )
    }

    /// Every unit's line, in the order the targets run.
    fn units(&self) -> String {
        [
            self.unit("podman", "", ""),
            self.unit("podman_memory", "", ""),
            self.unit("podman_killed", "", ""),
            self.unit("podman_cache", "", " --nocapture"),
            self.unit("podman_env", "", ""),
            self.unit("podman_credential", "", ""),
            self.unit("oci_crun", "", " --nocapture"),
        ]
        .concat()
    }
}

/// The build the verb asks cargo for: every real-Podman target, as JSON, as this user.
const BUILD: &str = "test --locked --no-run --message-format=json -p kbf-driver-container \
                     --test podman --test podman_memory --test podman_killed --test podman_cache \
                     --test podman_env --test podman_credential --test oci_crun\n";

/// Catches any step of the script dropped or reordered: the targets not built (or built
/// without --locked, or not all of them) before any unit starts, the machine's pinned
/// busybox manifest not named, anything run with Podman or in a user session, and a
/// target's unit missing, run with the wrong binary, or with the wrong settings (see the
/// unit tests of `systemd_run_args` for each one).
#[test]
fn every_target_runs_in_its_own_delegated_unit() {
    let run = Run::new("ok");
    let (code, out, err) = run.xtask(Path::new(FAKE), None, None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(run.cargo_args(), BUILD);
    assert_eq!(run.sudo_args(), run.units());
    assert!(out.starts_with("kernel "), "{out}");
    let machine = rustix::system::uname()
        .machine()
        .to_string_lossy()
        .into_owned();
    let arch = arch_for(&machine).expect("arch");
    assert!(
        out.contains(&format!(
            "\nbusybox manifest for {arch}: {}\n",
            this_manifest()
        )),
        "{out}"
    );
    assert!(
        out.ends_with(
            "ok: the real-crun tests of -p kbf-driver-container passed, each target in its \
             own Delegate=yes unit\n"
        ),
        "{out}"
    );
}

/// Catches a failed target that stops the run (the later targets never run), one
/// passed over because a later target passed (the status of the last unit taken for
/// the run's), and a verb that records only the first failure or only the last: the
/// first and fourth targets fail, every target still runs, and the verb fails naming
/// both, in the order they ran.
#[test]
fn a_failed_target_fails_the_verb_after_every_target_ran() {
    let run = Run::new("fail2");
    let (code, _, err) = run.xtask(
        Path::new(FAKE),
        None,
        Some("fails:--unit=kbf-podman-tests-podman,--unit=kbf-podman-tests-podman_cache"),
    );
    assert_eq!(code, Some(1), "{err}");
    assert_eq!(run.sudo_args(), run.units());
    let failed = |target: &str| {
        format!(
            "the tests of -p kbf-driver-container --test {target} failed in their unit \
             ({}): exit status: 101\n",
            run.root
                .join(format!("target/debug/deps/{target}-0f1e"))
                .display()
        )
    };
    assert!(
        err.ends_with(&format!(
            "xtask: {}{}",
            failed("podman"),
            failed("podman_cache")
        )),
        "{err}"
    );
}

/// Catches a failed build run anyway: no unit starts when cargo failed.
#[test]
fn a_failed_build_starts_no_unit() {
    let run = Run::new("build");
    let (code, _, err) = run.xtask(Path::new(FAKE), None, Some("build-fails"));
    assert_eq!(code, Some(1));
    assert_eq!(run.cargo_args(), BUILD);
    assert_eq!(run.sudo_args(), "");
    assert!(
        err.ends_with(
            "xtask: cargo test --no-run -p kbf-driver-container --test podman --test \
             podman_memory --test podman_killed --test podman_cache --test podman_env --test \
             podman_credential --test oci_crun \
             failed: exit status: 101\n"
        ),
        "{err}"
    );
}

/// Catches a build whose messages lack one target (a target renamed in the package
/// but not here) run without it: no unit starts.
#[test]
fn a_target_missing_from_the_build_starts_no_unit() {
    let run = Run::new("notgt");
    let messages = std::fs::read_to_string(run.root.join("cargo-messages")).expect("messages");
    let without: String = messages
        .lines()
        .filter(|l| !l.contains(r#""name":"podman_killed""#))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(run.root.join("cargo-messages"), without).expect("messages");
    let (code, _, err) = run.xtask(Path::new(FAKE), None, None);
    assert_eq!(code, Some(1));
    assert_eq!(run.sudo_args(), "");
    assert!(
        err.ends_with("xtask: cargo built 0 test binaries for the target `podman_killed`, not 1\n"),
        "{err}"
    );
}

/// Catches a sudo that cannot be started reported as anything but that program not
/// starting.
#[test]
fn a_sudo_that_cannot_start_fails_the_verb() {
    let run = Run::new("noprog");
    let missing = run.bin.join("missing");
    let (code, _, err) = run.xtask(Path::new(FAKE), Some(&missing), None);
    assert_eq!(code, Some(1));
    assert!(
        err.starts_with(&format!("xtask: {}: ", missing.display())),
        "{err}"
    );
}
