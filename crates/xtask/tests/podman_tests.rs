//! `cargo xtask podman-tests` as CI runs it: the real binary, with `xtask-fake` as
//! cargo (printing the JSON messages each test writes for it), as sudo (recording the
//! `loginctl` and `systemd-run` it was asked to run) and as podman (recording its
//! commands and printing the image index each test writes for it).

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::podman_tests::{BUSYBOX_INDEX, TARGETS, arch_for};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
const FAKE: &str = env!("CARGO_BIN_EXE_xtask-fake");
const INDEX: &str = "docker.io/library/busybox@sha256:\
                     bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e";
const D_ARM64: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000004";
const D_AMD64: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000005";
const PATH: &str = "/usr/bin:/bin";
const HOME: &str = "/srv/runner";

/// This machine's busybox manifest in the index the fake prints.
fn this_manifest() -> &'static str {
    let machine = rustix::system::uname()
        .machine()
        .to_string_lossy()
        .into_owned();
    match arch_for(&machine).expect("a machine these tests run on") {
        "amd64" => D_AMD64,
        _ => D_ARM64,
    }
}

/// One run's directories: the repository root cargo runs in, the fakes' directory,
/// and the runtime directory, with the user manager's bus in it.
struct Run {
    root: PathBuf,
    bin: PathBuf,
    runtime: PathBuf,
}

impl Run {
    /// An empty root for one test (keep `name` short: the bus's path must fit in a
    /// socket address, under `target/llvm-cov-target` too), fakes for sudo and podman,
    /// the bus, the image index the fake podman prints (linux/arm64, then
    /// linux/amd64), and the messages the fake cargo prints: each target's test binary
    /// (`<root>/target/debug/deps/<target>-0f1e`, from the package at
    /// `<root>/crates/kbf-driver-container`), after a decoy.
    fn new(name: &str) -> Run {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("xpt")
            .join(name);
        let _ = std::fs::remove_dir_all(&root);
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("scratch");
        for program in ["sudo", "podman"] {
            std::fs::hard_link(FAKE, bin.join(program)).expect("link the fake");
        }
        let runtime = root.join("r");
        std::fs::create_dir_all(&runtime).expect("runtime directory");
        drop(UnixListener::bind(runtime.join("bus")).expect("the bus"));
        let entry = |digest: &str, arch: &str| {
            format!(
                r#"{{"digest":"{digest}","platform":{{"architecture":"{arch}","os":"linux"}}}}"#
            )
        };
        let index = format!(
            r#"{{"manifests":[{},{}]}}"#,
            entry(D_ARM64, "arm64"),
            entry(D_AMD64, "amd64")
        );
        std::fs::write(bin.join("podman-index.json"), index).expect("index");
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
        Run { root, bin, runtime }
    }

    /// `podman-tests` with the fakes, the cargo `cargo`, and the sudo and podman
    /// `programs` (`None`: the fakes), planting `defect` (or none): exit code, stdout
    /// and stderr.
    fn xtask(
        &self,
        cargo: &Path,
        programs: Option<(&Path, &Path)>,
        defect: Option<&str>,
    ) -> (Option<i32>, String, String) {
        let (sudo, podman) = (self.bin.join("sudo"), self.bin.join("podman"));
        let (sudo, podman) = programs.unwrap_or((sudo.as_path(), podman.as_path()));
        let mut cmd = Command::new(XTASK);
        cmd.arg("--root").arg(&self.root).arg("podman-tests");
        cmd.arg("--sudo").arg(sudo).arg("--podman").arg(podman);
        cmd.arg("--runtime-dir").arg(&self.runtime);
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

    /// What the fake podman was asked to do, one `XDG_RUNTIME_DIR=<dir>: <args>` line
    /// per run.
    fn podman_args(&self) -> String {
        self.log(self.bin.join("podman-args"))
    }

    /// The `sudo-args` line of the `loginctl` call that turns lingering on.
    fn linger(&self) -> String {
        let uid = rustix::process::getuid().as_raw();
        format!(
            "{}: -n loginctl enable-linger {uid}\n",
            std::env::current_dir().expect("cwd").display()
        )
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
             -E HOME={HOME} -E PATH={PATH} -E XDG_RUNTIME_DIR={runtime} \
             -E KBF_TEST_IMAGE=docker://docker.io/library/busybox@{manifest} \
             -E KBF_TEST_INDEX_IMAGE=docker://{INDEX}{extra_env} {exe} --include-ignored{extra_args}\n",
            cwd = std::env::current_dir().expect("cwd").display(),
            package = package.display(),
            runtime = self.runtime.display(),
            manifest = this_manifest(),
            exe = self
                .root
                .join(format!("target/debug/deps/{target}-0f1e"))
                .display(),
        )
    }

    /// Every unit's line, in the order the targets run.
    fn units(&self) -> String {
        let conf = format!(
            " -E CONTAINERS_CONF_OVERRIDE={}",
            self.root
                .join("crates/kbf-driver-container/tests/fixtures/containers-override.conf")
                .display()
        );
        [
            self.unit("podman", "", ""),
            self.unit("podman_memory", "", ""),
            self.unit("podman_killed", "", ""),
            self.unit("podman_cache", "", " --nocapture"),
            self.unit("podman_env", &conf, ""),
            self.unit("podman_credential", "", ""),
            self.unit("oci_crun", "", " --nocapture"),
        ]
        .concat()
    }

    /// The fake podman's log of a run that got as far as reading the index.
    fn podman_ran(&self) -> String {
        let at = format!("XDG_RUNTIME_DIR={}:", self.runtime.display());
        format!("{at} --version\n{at} pull -q {INDEX}\n{at} manifest inspect {INDEX}\n")
    }
}

/// The build the verb asks cargo for: every real-Podman target, as JSON, as this user.
const BUILD: &str = "test --locked --no-run --message-format=json -p kbf-driver-container \
                     --test podman --test podman_memory --test podman_killed --test podman_cache \
                     --test podman_env --test podman_credential --test oci_crun\n";

/// Catches any step of the script dropped or reordered: lingering not turned on, podman
/// run without the runner's XDG_RUNTIME_DIR, busybox not pulled by its index, the
/// manifest not read from the index or taken for the wrong architecture, the targets
/// not built (or built without --locked, or not all of them) before any unit starts,
/// and a target's unit missing, run with the wrong binary, or with the wrong settings
/// (see the unit tests of `systemd_run_args` for each one).
#[test]
fn every_target_runs_in_its_own_delegated_unit() {
    let run = Run::new("ok");
    let (code, out, err) = run.xtask(Path::new(FAKE), None, None);
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(run.podman_args(), run.podman_ran());
    assert_eq!(run.cargo_args(), BUILD);
    assert_eq!(run.sudo_args(), format!("{}{}", run.linger(), run.units()));
    assert!(
        out.starts_with("podman version 0.0.0-xtask-fake\nkernel "),
        "{out}"
    );
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
            "ok: the real-Podman tests of -p kbf-driver-container passed, each target in its \
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
    assert_eq!(run.sudo_args(), format!("{}{}", run.linger(), run.units()));
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

/// Catches the wait for the user manager's bus skipped or cut short: the bus appears
/// a second after the verb starts, and the fake podman fails when there is no bus, so
/// the run passes only if the verb waited for it before the first podman command.
#[test]
fn the_verb_waits_for_a_bus_that_appears_late() {
    let run = Run::new("late");
    let bus = run.runtime.join("bus");
    std::fs::remove_file(&bus).expect("no bus yet");
    let binder = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(1));
        drop(UnixListener::bind(bus).expect("the bus"));
    });
    let (code, _, err) = run.xtask(Path::new(FAKE), None, None);
    binder.join().expect("the bus was bound");
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(run.podman_args(), run.podman_ran());
    assert_eq!(run.sudo_args(), format!("{}{}", run.linger(), run.units()));
}

/// Catches a bus that never appears passed over: after 100 looks 100 ms apart the
/// verb fails naming the bus, and podman, cargo and the units never run.
#[test]
fn a_bus_that_never_appears_runs_nothing_after_linger() {
    let run = Run::new("nobus");
    let bus = run.runtime.join("bus");
    std::fs::remove_file(&bus).expect("no bus");
    let (code, _, err) = run.xtask(Path::new(FAKE), None, None);
    assert_eq!(code, Some(1));
    assert_eq!(run.sudo_args(), run.linger());
    assert_eq!(run.podman_args(), "");
    assert_eq!(run.cargo_args(), "");
    assert!(
        err.ends_with(&format!(
            "xtask: no socket at {} after 100 looks 100ms apart\n",
            bus.display()
        )),
        "{err}"
    );
}

/// Catches lingering that could not be turned on passed over: nothing else runs.
#[test]
fn a_failed_linger_runs_nothing() {
    let run = Run::new("linger");
    let (code, _, err) = run.xtask(Path::new(FAKE), None, Some("tests-fail"));
    assert_eq!(code, Some(1));
    assert_eq!(run.sudo_args(), run.linger());
    assert_eq!(run.podman_args(), "");
    let uid = rustix::process::getuid().as_raw();
    assert!(
        err.ends_with(&format!(
            "xtask: sudo -n loginctl enable-linger {uid} failed: exit status: 101\n"
        )),
        "{err}"
    );
}

/// Catches a podman that fails passed over: the run stops at the first podman command,
/// and nothing is built or run in a unit.
#[test]
fn a_failed_podman_command_stops_the_run() {
    let run = Run::new("pfail");
    let (code, _, err) = run.xtask(Path::new(FAKE), None, Some("podman-fails"));
    assert_eq!(code, Some(1));
    let at = format!("XDG_RUNTIME_DIR={}:", run.runtime.display());
    assert_eq!(run.podman_args(), format!("{at} --version\n"));
    assert_eq!(run.cargo_args(), "");
    assert_eq!(run.sudo_args(), run.linger());
    assert!(
        err.ends_with("xtask: podman --version failed: exit status: 125\n"),
        "{err}"
    );
}

/// Catches an index whose manifest for this machine is the index itself run anyway:
/// the refusal comes before the build, so nothing is built or run in a unit.
#[test]
fn a_manifest_that_is_the_index_builds_nothing() {
    let run = Run::new("idx");
    let index = format!(
        r#"{{"manifests":[{{"digest":"{BUSYBOX_INDEX}","platform":{{"architecture":"amd64","os":"linux"}}}},{{"digest":"{BUSYBOX_INDEX}","platform":{{"architecture":"arm64","os":"linux"}}}}]}}"#
    );
    std::fs::write(run.bin.join("podman-index.json"), index).expect("index");
    let (code, _, err) = run.xtask(Path::new(FAKE), None, None);
    assert_eq!(code, Some(1));
    assert_eq!(run.podman_args(), run.podman_ran());
    assert_eq!(run.cargo_args(), "");
    assert_eq!(run.sudo_args(), run.linger());
    assert!(err.contains("the index test would mean nothing"), "{err}");
}

/// Catches a failed build run anyway: no unit starts when cargo failed.
#[test]
fn a_failed_build_starts_no_unit() {
    let run = Run::new("build");
    let (code, _, err) = run.xtask(Path::new(FAKE), None, Some("build-fails"));
    assert_eq!(code, Some(1));
    assert_eq!(run.cargo_args(), BUILD);
    assert_eq!(run.sudo_args(), run.linger());
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
    assert_eq!(run.sudo_args(), run.linger());
    assert!(
        err.ends_with("xtask: cargo built 0 test binaries for the target `podman_killed`, not 1\n"),
        "{err}"
    );
}

/// Catches a sudo or podman that cannot be started reported as anything but that
/// program not starting.
#[test]
fn a_sudo_or_podman_that_cannot_start_fails_the_verb() {
    let run = Run::new("noprog");
    let missing = run.bin.join("missing");
    let (sudo, podman) = (run.bin.join("sudo"), run.bin.join("podman"));
    let (code, _, err) = run.xtask(Path::new(FAKE), Some((&missing, &podman)), None);
    assert_eq!(code, Some(1));
    assert!(
        err.starts_with(&format!("xtask: {}: ", missing.display())),
        "{err}"
    );
    let (code, _, err) = run.xtask(Path::new(FAKE), Some((&sudo, &missing)), None);
    assert_eq!(code, Some(1));
    assert!(
        err.starts_with(&format!("xtask: {}: ", missing.display())),
        "{err}"
    );
    assert_eq!(run.cargo_args(), "");
}

/// Catches a fake podman that runs without the user manager's bus, which would let
/// `the_verb_waits_for_a_bus_that_appears_late` pass with the wait skipped: with no
/// socket at `$XDG_RUNTIME_DIR/bus` it exits 125, after logging the command.
#[test]
fn the_fake_podman_refuses_without_a_bus() {
    let run = Run::new("fakebus");
    std::fs::remove_file(run.runtime.join("bus")).expect("no bus");
    let out = Command::new(run.bin.join("podman"))
        .arg("--version")
        .env("XDG_RUNTIME_DIR", &run.runtime)
        .env_remove("XTASK_FAKE_DEFECT")
        .output()
        .expect("run the fake podman");
    assert_eq!(out.status.code(), Some(125));
    assert_eq!(out.stdout, b"");
    assert_eq!(
        run.podman_args(),
        format!("XDG_RUNTIME_DIR={}: --version\n", run.runtime.display())
    );
}
