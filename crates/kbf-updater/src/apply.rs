//! Who installs a verified set: the [`Applier`] interface, a [`FakeApplier`] for tests,
//! and [`AptSnapshot`], the Linux pinned-package applier of section 8.1 of
//! `docs/design/fleet-updates.md`.
//!
//! The updater calls an applier only with a set every check of S3.1 passed and whose
//! changed artifacts are already staged and hashed. An applier must be idempotent: after
//! a crash mid-apply the updater calls it again with the same set.
//!
//! [`AptSnapshot`] runs its commands through a [`Runner`]. The binary gives it
//! [`SystemRunner`]; the tests give it a runner that only records the commands, so no
//! test changes a package on any machine. What it does on a node:
//!
//! 1. with a snapshot pinned: `apt-get --snapshot <id> update`, then
//!    `apt-get --snapshot <id> --yes full-upgrade`, then
//!    `apt-get --snapshot <id> --yes install <kernel>` (apt checks every package against
//!    the archive's signature; the set chooses only which snapshot);
//! 2. every staged artifact is copied beside its installed path in the bin directory,
//!    synced, made executable and renamed over it, so a crash leaves the old binary or
//!    the new;
//! 3. the step needs a reboot exactly when `/run/reboot-required` exists afterwards, and
//!    the reboot is `systemctl reboot`.
//!
//! **Not done yet: restarting what it replaced.** Nothing restarts `kbf-daemon` (or the
//! updater) after its binary is renamed over. Until a reboot, the old daemon keeps
//! running, and its `/proc/<pid>/exe` is the old file, which no longer hashes to the
//! file at `--daemon-path`; the caller check (`caller`, on Linux) then refuses every
//! request from it. So after a set that changes `kbf-daemon` and does not reboot, the
//! daemon is locked out of the updater until it is restarted. The restart (with the
//! daemon's drain) is left for the change that adds the systemd units.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::set::SoftwareSet;

/// What an install did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Installed {
    /// The node must reboot to finish the step.
    pub reboot: bool,
}

/// Installs a verified, staged set.
pub trait Applier {
    /// Installs `set`. Its changed artifacts are in `staged`, one file per artifact name,
    /// already checked against their digests. Must be idempotent.
    ///
    /// # Errors
    /// The install failed; the updater keeps the apply in progress and reports it.
    fn install(&mut self, set: &SoftwareSet, staged: &Path) -> Result<Installed, String>;

    /// Reboots the node, after an install that asked for it.
    ///
    /// # Errors
    /// The reboot could not be started.
    fn reboot(&mut self) -> Result<(), String>;
}

/// An applier that records what it is asked to do and installs nothing.
#[derive(Clone, Debug, Default)]
pub struct FakeApplier {
    /// Each install: the set's serial and the staged file names, sorted.
    pub installs: Vec<(u64, Vec<String>)>,
    /// How many reboots were asked for.
    pub reboots: u32,
    /// Installs fail while this is set.
    pub fail_install: bool,
    /// Installs ask for a reboot while this is set.
    pub needs_reboot: bool,
}

impl Applier for FakeApplier {
    fn install(&mut self, set: &SoftwareSet, staged: &Path) -> Result<Installed, String> {
        if self.fail_install {
            return Err("planned failure".into());
        }
        let mut files: Vec<String> = fs::read_dir(staged)
            .map_err(|e| e.to_string())?
            .map(|e| {
                e.expect("a staged entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        files.sort();
        self.installs.push((set.serial, files));
        Ok(Installed {
            reboot: self.needs_reboot,
        })
    }

    fn reboot(&mut self) -> Result<(), String> {
        self.reboots += 1;
        Ok(())
    }
}

/// Runs one command to completion.
pub trait Runner {
    /// Runs `argv` and waits for it.
    ///
    /// # Errors
    /// It could not start, or it exited unsuccessfully.
    fn run(&mut self, argv: &[String]) -> Result<(), String>;
}

/// Runs commands on this machine, with `DEBIAN_FRONTEND=noninteractive` so apt never
/// waits for a terminal.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&mut self, argv: &[String]) -> Result<(), String> {
        let (program, args) = argv.split_first().ok_or("empty command")?;
        let status = Command::new(program)
            .args(args)
            .env("DEBIAN_FRONTEND", "noninteractive")
            .status()
            .map_err(|e| format!("{program}: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("{}: {status}", argv.join(" ")))
        }
    }
}

/// The pinned-package applier for Linux (section 8.1).
#[derive(Clone, Debug)]
pub struct AptSnapshot<R> {
    /// Runs apt and the reboot.
    pub runner: R,
    /// Where kbf's binaries are installed, one file per artifact name.
    pub bin_dir: PathBuf,
    /// The file whose presence means the node needs a reboot (`/run/reboot-required`).
    pub reboot_required: PathBuf,
}

impl<R> AptSnapshot<R> {
    /// The applier a node uses: kbf binaries in `bin_dir`, reboots signalled by
    /// `/run/reboot-required`.
    pub fn new(runner: R, bin_dir: PathBuf) -> Self {
        AptSnapshot {
            runner,
            bin_dir,
            reboot_required: PathBuf::from("/run/reboot-required"),
        }
    }
}

/// Copies `from` to `to` through a synced temporary file beside `to`, mode 0755.
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    let tmp = to.with_extension("kbf-new");
    let mut out = fs::File::create(&tmp)?;
    out.write_all(&fs::read(from)?)?;
    out.set_permissions(fs::Permissions::from_mode(0o755))?;
    out.sync_all()?;
    fs::rename(&tmp, to)
}

impl<R: Runner> Applier for AptSnapshot<R> {
    fn install(&mut self, set: &SoftwareSet, staged: &Path) -> Result<Installed, String> {
        if let Some(pin) = &set.snapshot {
            let apt = |args: &[&str]| -> Vec<String> {
                ["apt-get", "--snapshot", pin.snapshot.as_str()]
                    .iter()
                    .chain(args)
                    .map(|s| (*s).to_owned())
                    .collect()
            };
            self.runner.run(&apt(&["update"]))?;
            self.runner.run(&apt(&["--yes", "full-upgrade"]))?;
            self.runner
                .run(&apt(&["--yes", "install", pin.kernel.as_str()]))?;
        }
        for name in set.artifacts.keys() {
            let from = staged.join(name);
            if from.exists() {
                replace_file(&from, &self.bin_dir.join(name))
                    .map_err(|e| format!("install {name}: {e}"))?;
            }
        }
        Ok(Installed {
            reboot: self.reboot_required.exists(),
        })
    }

    fn reboot(&mut self) -> Result<(), String> {
        self.runner
            .run(&["systemctl".to_owned(), "reboot".to_owned()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::set::AptPin;
    use crate::testkit;

    /// Records commands; fails the one whose first argument after the snapshot matches.
    #[derive(Default)]
    struct Recorder {
        ran: Vec<String>,
        fail_on: Option<&'static str>,
    }

    impl Runner for Recorder {
        fn run(&mut self, argv: &[String]) -> Result<(), String> {
            let line = argv.join(" ");
            self.ran.push(line.clone());
            match self.fail_on {
                Some(word) if line.contains(word) => Err(format!("{line}: failed")),
                _ => Ok(()),
            }
        }
    }

    fn apt(name: &str) -> (AptSnapshot<Recorder>, PathBuf, PathBuf) {
        let dir = testkit::scratch(name);
        let (bin, staged) = (dir.join("bin"), dir.join("staged"));
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&staged).unwrap();
        let mut a = AptSnapshot::new(Recorder::default(), bin.clone());
        assert_eq!(a.reboot_required, Path::new("/run/reboot-required"));
        a.reboot_required = dir.join("reboot-required");
        (a, bin, staged)
    }

    fn snapshot_set() -> SoftwareSet {
        let mut s = testkit::set(5);
        s.snapshot = Some(AptPin {
            snapshot: "20261001T000000Z".into(),
            kernel: "linux-image-6.8".into(),
        });
        s
    }

    /// Catches: apt run without the pinned snapshot (the node drifts to whatever the
    /// archive serves today), the kernel package not installed, or the commands out of
    /// order. Nothing here runs apt: the runner only records.
    #[test]
    fn a_snapshot_set_runs_apt_pinned_to_its_snapshot() {
        let (mut a, _, staged) = apt("apt-plan");
        assert_eq!(
            a.install(&snapshot_set(), &staged),
            Ok(Installed { reboot: false })
        );
        assert_eq!(
            a.runner.ran,
            [
                "apt-get --snapshot 20261001T000000Z update",
                "apt-get --snapshot 20261001T000000Z --yes full-upgrade",
                "apt-get --snapshot 20261001T000000Z --yes install linux-image-6.8",
            ]
        );
        a.runner.ran.clear();
        assert_eq!(
            a.install(&testkit::set(5), &staged),
            Ok(Installed { reboot: false })
        );
        assert!(
            a.runner.ran.is_empty(),
            "no snapshot, no apt: {:?}",
            a.runner.ran
        );
    }

    /// Catches: a failed apt step ignored and the next one run (or the set recorded as
    /// installed by the updater).
    #[test]
    fn a_failed_apt_step_stops_the_install() {
        for (word, ran) in [("update", 1), ("full-upgrade", 2), ("install", 3)] {
            let (mut a, _, staged) = apt(&format!("apt-fail-{word}"));
            a.runner.fail_on = Some(word);
            assert!(a.install(&snapshot_set(), &staged).is_err(), "{word}");
            assert_eq!(a.runner.ran.len(), ran, "{word}: {:?}", a.runner.ran);
        }
    }

    /// Catches: a staged binary not installed, installed without the executable bit, an
    /// unstaged (unchanged) artifact touched, or a reboot asked for when none is needed
    /// (and none asked for when one is).
    #[test]
    fn staged_artifacts_replace_the_installed_binaries() {
        let (mut a, bin, staged) = apt("apt-bins");
        fs::write(staged.join("kbf-daemon"), b"new daemon").unwrap();
        fs::write(bin.join("kbf-updater"), b"old updater").unwrap();
        assert_eq!(
            a.install(&testkit::set(5), &staged),
            Ok(Installed { reboot: false })
        );
        assert_eq!(fs::read(bin.join("kbf-daemon")).unwrap(), b"new daemon");
        let mode = fs::metadata(bin.join("kbf-daemon"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(fs::read(bin.join("kbf-updater")).unwrap(), b"old updater");
        fs::write(&a.reboot_required, b"").unwrap();
        assert_eq!(
            a.install(&testkit::set(5), &staged),
            Ok(Installed { reboot: true })
        );
        assert_eq!(a.reboot(), Ok(()));
        assert_eq!(a.runner.ran, ["systemctl reboot"]);
        fs::remove_dir_all(&bin).unwrap();
        let err = a.install(&testkit::set(5), &staged).unwrap_err();
        assert!(err.starts_with("install kbf-daemon:"), "{err}");
    }

    /// Catches: the system runner reporting a failed or missing command as success.
    /// It runs only `true` and `false`, never apt.
    #[test]
    fn the_system_runner_reports_exit_status() {
        let argv = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(SystemRunner.run(&argv(&["true"])), Ok(()));
        assert!(
            SystemRunner
                .run(&argv(&["false"]))
                .unwrap_err()
                .starts_with("false: ")
        );
        assert!(
            SystemRunner
                .run(&argv(&["/nonexistent/kbf-no-such-command"]))
                .is_err()
        );
        assert_eq!(SystemRunner.run(&[]), Err("empty command".into()));
    }

    /// Catches: the fake installing while told to fail, or losing the staged names.
    #[test]
    fn the_fake_records_installs_and_reboots() {
        let dir = testkit::scratch("fake");
        fs::write(dir.join("b"), b"").unwrap();
        fs::write(dir.join("a"), b"").unwrap();
        let mut f = FakeApplier {
            needs_reboot: true,
            ..FakeApplier::default()
        };
        assert_eq!(
            f.install(&testkit::set(5), &dir),
            Ok(Installed { reboot: true })
        );
        assert_eq!(f.installs, [(5, vec!["a".to_owned(), "b".to_owned()])]);
        assert_eq!(f.reboot(), Ok(()));
        assert_eq!(f.reboots, 1);
        assert!(f.install(&testkit::set(5), &dir.join("missing")).is_err());
        f.fail_install = true;
        assert_eq!(
            f.install(&testkit::set(5), &dir),
            Err("planned failure".into())
        );
    }
}
